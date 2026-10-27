//! A sampling CPU profiler for JIT-compiled AIPL, behind `aipl run --flamegraph`.
//!
//! It is the profiler Brendan Gregg's flame graphs are drawn from: a tick every
//! 1/997 s of CPU time (an odd rate, so sampling does not fall into lockstep
//! with a program's own periodic work), and on each tick a walk of the
//! interrupted thread's frame-pointer chain. A stack that was on CPU for longer
//! is caught in more samples, so its width in the graph is its share of the
//! time. Time the thread spends blocked — reading stdin, waiting on a child —
//! is not CPU time and is not sampled.
//!
//! The ticks come from a thread of the profiler's own rather than from
//! `ITIMER_PROF`, which macOS fires at little more than 100 Hz whatever it is
//! asked for. That thread reads the profiled thread's CPU clock about twice per
//! tick and signals it each time the clock passes one; a signal that arrives
//! after the next tick was due counts for each tick it covers, so the total is
//! the CPU time whatever the sampler thread's own scheduling.
//!
//! The walk needs every frame to keep a frame record, which is what
//! [`Compilation::new_profiled`](crate::codegen::Compilation::new_profiled)
//! compiles for; the runtime, written in Rust, keeps them already on the one
//! target this supports (frame pointers are part of the arm64 macOS ABI).
//!
//! The signal handler may not allocate or take a lock, so it only copies
//! addresses into a buffer mapped before the timer starts. Everything else —
//! naming the addresses, cutting the stack down to the program's own frames —
//! happens after the run, in [`sample`].

use std::collections::{BTreeMap, HashMap};

/// One frame of a sampled stack, as the flame graph labels and colors it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame {
    pub name: String,
    pub kind: FrameKind,
}

/// Where a frame's code came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameKind {
    /// Compiled from the program's AIPL source.
    Aipl,
    /// The AIPL runtime, linked into the `aipl` binary.
    Runtime,
    /// A system library the runtime called (the allocator, `memcpy`, ...).
    System,
    /// Stands for no code: the graph's root, or the frames below a stack too
    /// deep to walk to the bottom.
    Marker,
}

/// What a profiled run spent its CPU time on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// How many ticks each stack was caught on, root first. Every stack starts at the
    /// program's entry function: frames outside the program (the driver that
    /// called it) are cut away.
    pub stacks: BTreeMap<Vec<Frame>, u64>,
    /// The sampling rate, per second of CPU time.
    pub hz: u32,
    /// Samples dropped because the buffer filled, or taken while no frame of
    /// the program was on the stack (before its entry function was called).
    pub lost: u64,
}

impl Profile {
    /// The number of samples the profile holds.
    pub fn samples(&self) -> u64 {
        self.stacks.values().sum()
    }

    /// The stacks in Brendan Gregg's folded format, one `root;..;leaf count`
    /// line each, sorted — the input `flamegraph.pl` and its ports read.
    pub fn folded(&self) -> String {
        self.stacks
            .iter()
            .map(|(stack, count)| {
                let names: Vec<&str> = stack.iter().map(|f| f.name.as_str()).collect();
                format!("{} {count}\n", names.join(";"))
            })
            .collect()
    }
}

/// The sampling rate, per second of CPU time.
const HZ: u32 = 997;

/// The deepest stack a sample records. A deeper one keeps its innermost frames
/// and is rooted at a [`FrameKind::Marker`] named `[truncated]`.
const MAX_DEPTH: usize = 4096;

/// The sample buffer's size in words: 512 MiB of address space, which the
/// kernel backs only as it is written — around 30 minutes of samples at a
/// typical depth.
const BUFFER_WORDS: usize = 64 << 20;

/// Set in a sample's length word when the walk stopped at [`MAX_DEPTH`].
const TRUNCATED: u64 = 1 << 63;

/// Return addresses saved by arm64e system code carry a pointer
/// authentication code in their high bits; user addresses fit below them.
const ADDRESS_MASK: u64 = (1 << 47) - 1;

/// Run `run` while sampling the calling thread's stack, and name each sampled
/// frame: an address in JIT-compiled code by `code` (start address and name of
/// every function the JIT defined, sorted by address), any other by the symbol
/// table of the image it falls in.
pub fn sample<R>(
    code: &[(usize, String)],
    run: impl FnOnce() -> R,
) -> Result<(R, Profile), String> {
    let raw = sampler::record(run)?;
    let mut namer = Namer::new(code);
    let mut stacks: BTreeMap<Vec<Frame>, u64> = BTreeMap::new();
    let mut lost = raw.lost;
    for sample in &raw.samples {
        match namer.stack(sample) {
            Some(stack) => *stacks.entry(stack).or_default() += sample.weight,
            None => lost += sample.weight,
        }
    }
    Ok((
        raw.result,
        Profile {
            stacks,
            hz: HZ,
            lost,
        },
    ))
}

/// One sample as the handler recorded it.
struct RawSample {
    /// The interrupted instruction.
    pc: u64,
    /// The link register: the return address of a function that has not saved
    /// it yet.
    lr: u64,
    /// The return addresses on the frame-pointer chain, innermost first.
    returns: Vec<u64>,
    truncated: bool,
    /// The ticks it stands for: more than one when the signal arrived after
    /// the next tick was due.
    weight: u64,
}

/// A function's start address and its frame, which is what tells two
/// addresses apart as belonging to the same function or not.
type Symbol = (u64, Frame);

/// Turns sampled addresses into frames, caching each address's answer.
struct Namer<'a> {
    code: &'a [(usize, String)],
    cache: HashMap<u64, Option<Symbol>>,
    /// The image the runtime is in: this binary.
    own_image: usize,
}

impl<'a> Namer<'a> {
    fn new(code: &'a [(usize, String)]) -> Self {
        let own_image = image_of(demangle as *const () as u64).map_or(0, |i| i.base);
        Self {
            code,
            cache: HashMap::new(),
            own_image,
        }
    }

    /// `sample`'s frames, root first, cut down to the program's own: from its
    /// outermost AIPL frame inward. `None` when no frame is the program's.
    fn stack(&mut self, sample: &RawSample) -> Option<Vec<Frame>> {
        // A return address is the instruction *after* the call, which can be
        // the first instruction of the next function; one byte back is inside
        // the call.
        let pc = self.symbol(sample.pc);
        let callers: Vec<Option<Symbol>> =
            sample.returns.iter().map(|&r| self.symbol(r - 1)).collect();

        // The chain starts at the innermost function that has saved a frame
        // record. One that has not — a leaf, or any function in its prologue —
        // is missing its caller from the chain, and that caller is in the link
        // register. When the function has saved its record, the link register
        // either still names that same caller (so the chain has it) or names a
        // call this function has made since (so it is the function itself).
        let lr = self.symbol(sample.lr.wrapping_sub(1));
        let start = |s: &Option<Symbol>| s.as_ref().map(|(start, _)| *start);
        let caller_missing = lr.is_some()
            && start(&lr) != start(&pc)
            && callers.first().is_none_or(|c| start(c) != start(&lr));

        let mut leaf_first: Vec<Option<Symbol>> = vec![pc];
        if caller_missing {
            leaf_first.push(lr);
        }
        leaf_first.extend(callers);

        let frames: Vec<Frame> = leaf_first
            .into_iter()
            .map(|s| match s {
                Some((_, frame)) => frame,
                None => Frame {
                    name: "[unknown]".to_string(),
                    kind: FrameKind::System,
                },
            })
            .collect();
        let mut root_first: Vec<Frame> = if sample.truncated {
            let mut frames = frames;
            frames.push(Frame {
                name: "[truncated]".to_string(),
                kind: FrameKind::Marker,
            });
            frames
        } else {
            let outermost = frames.iter().rposition(|f| f.kind == FrameKind::Aipl)?;
            frames.into_iter().take(outermost + 1).collect()
        };
        root_first.reverse();
        Some(root_first)
    }

    fn symbol(&mut self, address: u64) -> Option<Symbol> {
        if let Some(known) = self.cache.get(&address) {
            return known.clone();
        }
        let found = self.lookup(address);
        self.cache.insert(address, found.clone());
        found
    }

    fn lookup(&self, address: u64) -> Option<Symbol> {
        match image_of(address) {
            Some(image) => {
                let kind = if image.base == self.own_image {
                    FrameKind::Runtime
                } else {
                    FrameKind::System
                };
                let (start, name) = match image.symbol {
                    Some((start, name)) => (start, demangle(&name)),
                    None => (image.base, format!("[{}]", image.file)),
                };
                Some((start as u64, Frame { name, kind }))
            }
            // Not in any loaded image, so in memory the JIT mapped, or not code
            // at all: a link register a function reused as scratch.
            None => {
                let index = self
                    .code
                    .partition_point(|(start, _)| *start as u64 <= address);
                let (start, name) = self.code.get(index.checked_sub(1)?)?;
                // The last function has no next start to bound it; nothing the
                // JIT compiles is anywhere near this large.
                if index == self.code.len() && address - *start as u64 > 1 << 20 {
                    return None;
                }
                Some((
                    *start as u64,
                    Frame {
                        name: name.clone(),
                        kind: FrameKind::Aipl,
                    },
                ))
            }
        }
    }
}

/// What `dladdr` knows about an address in a loaded image.
struct Image {
    base: usize,
    /// The image's file name, without its directory.
    file: String,
    /// The closest symbol at or below the address, and its start.
    symbol: Option<(usize, String)>,
}

fn image_of(address: u64) -> Option<Image> {
    use std::ffi::CStr;
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    if unsafe { libc::dladdr(address as *const libc::c_void, &mut info) } == 0 {
        return None;
    }
    let text = |p: *const libc::c_char| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    let file = if info.dli_fname.is_null() {
        String::new()
    } else {
        let path = text(info.dli_fname);
        path.rsplit('/').next().unwrap_or(&path).to_string()
    };
    let symbol =
        (!info.dli_sname.is_null()).then(|| (info.dli_saddr as usize, text(info.dli_sname)));
    Some(Image {
        base: info.dli_fbase as usize,
        file,
        symbol,
    })
}

/// A Rust symbol's path (`alloc::raw_vec::finish_grow`); any other symbol as
/// it is. The trailing hash is dropped: it tells apart instances a reader of a
/// flame graph wants merged.
fn demangle(symbol: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(symbol))
}

/// What a sampled run recorded, before any address is named.
struct Recording<R> {
    result: R,
    samples: Vec<RawSample>,
    lost: u64,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod sampler {
    use super::{RawSample, Recording, ADDRESS_MASK, BUFFER_WORDS, HZ, MAX_DEPTH, TRUNCATED};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
    use std::time::Duration;

    // What the handler reads, set before the first tick. Statics because a
    // signal handler has no other way to reach state.
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static BUFFER: AtomicUsize = AtomicUsize::new(0);
    /// The next free word of [`BUFFER`]. Only the handler writes it, and it
    /// does not interrupt itself: the kernel blocks a signal while its handler
    /// runs.
    static CURSOR: AtomicUsize = AtomicUsize::new(0);
    static LOST: AtomicU64 = AtomicU64::new(0);
    /// The thread being profiled; 0 once the run is over.
    static THREAD: AtomicUsize = AtomicUsize::new(0);
    static STACK_LOW: AtomicU64 = AtomicU64::new(0);
    static STACK_HIGH: AtomicU64 = AtomicU64::new(0);
    /// Ticks the sampler thread has signalled that no sample has claimed yet.
    static OWED: AtomicU64 = AtomicU64::new(0);

    /// Each sample's words: its length (with [`TRUNCATED`]), its weight, the
    /// pc, the link register, then the return addresses.
    const HEADER_WORDS: usize = 4;

    pub(super) fn record<R>(run: impl FnOnce() -> R) -> Result<Recording<R>, String> {
        if ACTIVE.swap(true, SeqCst) {
            return Err("a profiled run is already in progress".to_string());
        }
        let bytes = BUFFER_WORDS * 8;
        let buffer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if buffer == libc::MAP_FAILED {
            ACTIVE.store(false, SeqCst);
            return Err("could not map the profiler's sample buffer".to_string());
        }
        let thread = unsafe { libc::pthread_self() };
        // The stack grows down from its address, the highest one.
        let high = unsafe { libc::pthread_get_stackaddr_np(thread) } as u64;
        let size = unsafe { libc::pthread_get_stacksize_np(thread) } as u64;
        BUFFER.store(buffer as usize, SeqCst);
        CURSOR.store(0, SeqCst);
        LOST.store(0, SeqCst);
        OWED.store(0, SeqCst);
        THREAD.store(thread, SeqCst);
        STACK_LOW.store(high - size, SeqCst);
        STACK_HIGH.store(high, SeqCst);

        // The handler stays installed after the run, ignoring what arrives: a
        // tick still pending would otherwise meet `SIGPROF`'s default action,
        // which ends the process.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_tick as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGPROF, &action, std::ptr::null_mut());
        }

        let port = unsafe { libc::pthread_mach_thread_np(thread) };
        let stop = AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            let ticker = std::thread::Builder::new()
                .name("aipl-profiler".to_string())
                .spawn_scoped(scope, || tick(port, thread, &stop))
                .map_err(|e| format!("could not start the profiler's thread: {e}"))?;
            let result = run();
            stop.store(true, SeqCst);
            let _ = ticker.join();
            Ok::<R, String>(result)
        });
        THREAD.store(0, SeqCst);

        let words =
            unsafe { std::slice::from_raw_parts(buffer as *const u64, CURSOR.load(SeqCst)) };
        let mut samples = Vec::new();
        let mut at = 0;
        while at < words.len() {
            let len = (words[at] & !TRUNCATED) as usize;
            let returns = &words[at + HEADER_WORDS..at + HEADER_WORDS + len];
            samples.push(RawSample {
                pc: words[at + 2],
                lr: words[at + 3],
                returns: returns.to_vec(),
                truncated: words[at] & TRUNCATED != 0,
                weight: words[at + 1],
            });
            at += HEADER_WORDS + len;
        }
        let lost = LOST.load(SeqCst);
        unsafe { libc::munmap(buffer, bytes) };
        ACTIVE.store(false, SeqCst);
        Ok(Recording {
            result: result?,
            samples,
            lost,
        })
    }

    /// Signal `thread` each time its CPU clock passes a tick, until `stop`.
    fn tick(port: libc::mach_port_t, thread: libc::pthread_t, stop: &AtomicBool) {
        let interval = 1_000_000 / HZ as u64;
        let Some(start) = cpu_micros(port) else {
            return;
        };
        let mut next = start + interval;
        while !stop.load(Relaxed) {
            std::thread::sleep(Duration::from_micros(interval / 2));
            let Some(now) = cpu_micros(port) else {
                return;
            };
            if now >= next {
                let ticks = (now - next) / interval + 1;
                next += ticks * interval;
                OWED.fetch_add(ticks, SeqCst);
                unsafe { libc::pthread_kill(thread, libc::SIGPROF) };
            }
        }
    }

    /// The CPU time `port`'s thread has used, user and system, in microseconds.
    fn cpu_micros(port: libc::mach_port_t) -> Option<u64> {
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        let status = unsafe {
            libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                &mut info as *mut libc::thread_basic_info as libc::thread_info_t,
                &mut count,
            )
        };
        let micros = |t: libc::time_value_t| t.seconds as u64 * 1_000_000 + t.microseconds as u64;
        (status == libc::KERN_SUCCESS).then(|| micros(info.user_time) + micros(info.system_time))
    }

    /// The registers the kernel saved for the interrupted thread. The layouts
    /// are `<sys/_types/_ucontext.h>` and `<mach/arm/_structs.h>`, which the
    /// `libc` crate does not carry.
    #[repr(C)]
    struct UContext {
        onstack: i32,
        sigmask: u32,
        stack: [u64; 3],
        link: *mut UContext,
        mcsize: usize,
        mcontext: *const MContext,
    }

    #[repr(C)]
    struct MContext {
        /// `__darwin_arm_exception_state64`.
        exception: [u64; 2],
        x: [u64; 29],
        fp: u64,
        lr: u64,
        sp: u64,
        pc: u64,
    }

    extern "C" fn on_tick(_: libc::c_int, _: *mut libc::siginfo_t, context: *mut libc::c_void) {
        if unsafe { libc::pthread_self() } != THREAD.load(Relaxed) {
            return;
        }
        // Signals coalesce: two ticks signalled before the first was handled
        // arrive as one, which then claims both.
        let weight = OWED.swap(0, SeqCst);
        if weight == 0 {
            return;
        }
        let at = CURSOR.load(Relaxed);
        if at + HEADER_WORDS + MAX_DEPTH > BUFFER_WORDS {
            LOST.fetch_add(weight, Relaxed);
            return;
        }
        let registers = unsafe { &*(*(context as *const UContext)).mcontext };
        let buffer = BUFFER.load(Relaxed) as *mut u64;
        let (low, high) = (STACK_LOW.load(Relaxed), STACK_HIGH.load(Relaxed));
        let mut len = 0;
        let mut fp = registers.fp;
        let mut truncated = false;
        // Each frame record is the caller's frame pointer and then the return
        // address, and lies higher on the stack than the record of the frame
        // it calls. A pointer that breaks any of that ends the walk.
        while fp >= low && fp + 16 <= high && fp % 8 == 0 {
            if len == MAX_DEPTH {
                truncated = true;
                break;
            }
            let (next, ret) = unsafe { (*(fp as *const u64), *((fp + 8) as *const u64)) };
            let ret = ret & ADDRESS_MASK;
            if ret == 0 {
                break;
            }
            unsafe { *buffer.add(at + HEADER_WORDS + len) = ret };
            len += 1;
            if next <= fp {
                break;
            }
            fp = next;
        }
        unsafe {
            *buffer.add(at) = len as u64 | if truncated { TRUNCATED } else { 0 };
            *buffer.add(at + 1) = weight;
            *buffer.add(at + 2) = registers.pc & ADDRESS_MASK;
            *buffer.add(at + 3) = registers.lr & ADDRESS_MASK;
        }
        CURSOR.store(at + HEADER_WORDS + len, Relaxed);
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod sampler {
    use super::Recording;

    pub(super) fn record<R>(_: impl FnOnce() -> R) -> Result<Recording<R>, String> {
        Err("profiling is supported only on arm64 macOS".to_string())
    }
}
