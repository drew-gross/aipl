//! Measuring a program: the deterministic counts behind `aipl report` and the
//! cases harness's `--- performance ---` section.
//!
//! One implementation serves both, so what the command prints and what the
//! corpus records cannot drift apart. A measurement is two builds of the same
//! program: the production object, whose *size* is reported (the code the
//! program actually ships), and an instrumented one, linked against the
//! instrumented runtime and run once, whose executed-instruction and
//! allocation tallies are reported. The production object is the caller's to
//! build, because the caller usually has a use for it too (the harness runs
//! it to check behavior).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::ast::{Item, Program};
use crate::binary;
use crate::codegen::ObjectCompilation;
use crate::{DebugOptions, Error};

/// What a `functions:` entry's `bytes:` line reads when the function has no code
/// in the measured object — see [`FnStat::bytes`]. Not a number, so it can never
/// be mistaken for a measured zero.
pub const EXTERNAL_BYTES: &str = "(external)";

/// The program whose binary is built and measured: the program's own when it has
/// a `main`; otherwise the synthesized `.test` driver, with its `__test_main`
/// renamed to `main` so it is the AOT entry. So a library (no `main`, only
/// `.test` blocks) is measured as the same test run `aipl check` verifies, not as
/// an absent `main`.
pub fn measured_program(program: &Program) -> Program {
    let has_main = program
        .items
        .iter()
        .any(|it| matches!(it, Item::Fn(f) if f.name == "main"));
    if has_main {
        program.clone()
    } else {
        let mut tp = crate::codegen::build_test_program(program);
        for it in &mut tp.items {
            if let Item::Fn(f) = it {
                if f.name == "__test_main" {
                    f.name.text = "main".to_string();
                }
            }
        }
        tp
    }
}

/// A program's `.performance` driver (see
/// [`crate::codegen::build_performance_program`]) as a program that can be built
/// and run: its `__test_main` becomes the entry, and any `main` of the user's own
/// steps aside for it.
pub fn driver_as_main(driver: &Program) -> Program {
    let mut program = driver.clone();
    program
        .items
        .retain(|it| !matches!(it, Item::Fn(f) if f.name == "main"));
    for it in &mut program.items {
        if let Item::Fn(f) = it {
            if f.name == "__test_main" {
                f.name.text = "main".to_string();
            }
        }
    }
    program
}

/// What running a file's `.performance` blocks produced.
pub struct TestRun {
    /// Blocks run, and how many of them failed.
    pub total: i64,
    pub failed: i64,
    /// The failure report, exactly as the in-process runner prints one (`test
    /// <name> ... FAIL`, then a line per failed assertion). Empty when all passed.
    pub report: String,
}

/// Run a `.performance` driver: build it instrumented, link it against the
/// instrumented runtime, and run it once in the current directory (the caller has
/// staged the file's companions there).
///
/// The blocks run apart from the file's `.test` blocks because their counts exist
/// only in the instrumented build, which the in-process JIT is not. The program
/// reports its own failures — `file_label` is the batch label it qualifies each
/// header with, empty for a single file — and its tallies come back through a file
/// it is told to write, so the caller can fold them into its aggregate.
pub fn run_performance_tests(
    driver: &Program,
    stem: &str,
    file_label: &str,
    dbg: DebugOptions,
) -> Result<TestRun, MeasureError> {
    let object = ObjectCompilation::new(&driver_as_main(driver), stem, dbg, true)
        .and_then(|c| c.emit().map_err(Vec::from))
        .map_err(MeasureError::Compile)?;
    let dir = binary::scratch_dir("check-perf").map_err(MeasureError::Link)?;
    let result = (|| {
        let exe = dir.join(binary::default_exe_name(&format!("{stem}_performance")));
        binary::link_instrumented(&object, &exe).map_err(MeasureError::Link)?;
        let tally_path = dir.join("tally.txt");
        let output = Command::new(&exe)
            .env("AIPL_TEST_REPORT", file_label)
            .env("AIPL_TEST_TALLY", &tally_path)
            .output()
            .map_err(|e| MeasureError::Run(format!("`.performance` run failed to start: {e}")))?;
        let tally = fs::read_to_string(&tally_path).map_err(|e| {
            MeasureError::Run(format!(
                "`.performance` run exited with {:?} before reporting its results: {e}",
                output.status.code()
            ))
        })?;
        let mut counts = tally.split_whitespace().map(|n| n.parse::<i64>());
        let (Some(Ok(total)), Some(Ok(failed))) = (counts.next(), counts.next()) else {
            return Err(MeasureError::Run(format!(
                "`.performance` run reported an unreadable tally: {tally:?}"
            )));
        };
        Ok(TestRun {
            total,
            failed,
            report: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        })
    })();
    let _ = fs::remove_dir_all(&dir);
    result
}

/// Why a measurement could not be produced. The variants keep a compile or link
/// diagnostic as the diagnostic it is, so the caller renders it against the
/// source it holds; [`MeasureError::render`] does that.
pub enum MeasureError {
    /// The instrumented object failed to compile or emit.
    Compile(Vec<Error>),
    /// The instrumented object failed to link.
    Link(Error),
    /// The instrumented program could not be run, or reported something
    /// unusable. Already a complete message.
    Run(String),
}

impl MeasureError {
    /// The failure as text, with diagnostics rendered against `source` and named
    /// `path`.
    pub fn render(&self, source: &str, path: &str) -> String {
        match self {
            MeasureError::Compile(e) => format!(
                "instrumented compile failed:\n{}",
                Error::render_all(e, source, path)
            ),
            MeasureError::Link(e) => {
                format!("instrumented link failed:\n{}", e.render(source, path))
            }
            MeasureError::Run(msg) => msg.clone(),
        }
    }
}

/// Measure `program` (already passed through [`measured_program`]).
///
/// `production` is its non-instrumented object and `symbol_names` that object's
/// [`code_symbol_names`](ObjectCompilation::code_symbol_names): together they give
/// the `binary size` split and each function's code size. The instrumented build
/// is staged and run in `dir` with `cli` as its arguments, so a program's
/// relative reads find whatever the caller staged there.
pub fn measure(
    program: &Program,
    stem: &str,
    production: &[u8],
    symbol_names: &HashMap<String, String>,
    cli: &[String],
    dir: &Path,
    dbg: DebugOptions,
) -> Result<PerfStats, MeasureError> {
    let (prod_sizes, fn_sizes) = BinSizes::of(production, symbol_names);
    let instrumented = ObjectCompilation::new(program, stem, dbg, true)
        .and_then(|c| c.emit().map_err(Vec::from))
        .map_err(MeasureError::Compile)?;

    let exe = dir.join(binary::default_exe_name(&format!("{stem}_instr")));
    binary::link_instrumented(&instrumented, &exe).map_err(MeasureError::Link)?;

    let stats_path = dir.join("alloc_stats.txt");
    let _ = fs::remove_file(&stats_path);
    let output = Command::new(&exe)
        .args(cli)
        .current_dir(dir)
        .env("AIPL_ALLOC_STATS", &stats_path)
        .output()
        .map_err(|e| MeasureError::Run(format!("instrumented spawn failed: {e}")))?;

    // A program may exit nonzero by design, so the status alone is not a failure;
    // a run that never reported is, and its status is the best clue why.
    let contents = fs::read_to_string(&stats_path).map_err(|e| {
        MeasureError::Run(format!(
            "could not read alloc stats at {} (the program exited with {:?} before \
             reporting): {e}",
            stats_path.display(),
            output.status.code()
        ))
    })?;
    // The runtime's call table is fixed-size; an overflow would quietly under-
    // count, so it says so instead and that is a failure, not a metric.
    if contents.contains("uncounted calls:") {
        return Err(MeasureError::Run(format!(
            "the runtime's per-function call table overflowed, so the counts are\n\
             incomplete. Raise `fn_calls::CAP` in crates/aipl-linker/runtime/aipl_runtime.rs.\n\
             {contents}"
        )));
    }
    // The runtime reports the execution counters and per-function call counts;
    // append the (measured here) binary size split so the combined text parses
    // into a full `PerfStats`, then fold the per-function byte sizes into the
    // `functions:` block the counts opened.
    let contents = format!("{contents}\n{}", prod_sizes.render());
    let mut stats = PerfStats::parse(&contents).ok_or_else(|| {
        MeasureError::Run(format!("malformed perf stats from runtime:\n{contents}"))
    })?;
    stats.merge_fn_sizes(symbol_names, fn_sizes);
    Ok(stats)
}

/// Measured performance statistics for a case run. All six fields are required
/// in a `--- performance ---` body. `instructions` is the CLIF instructions
/// executed; `binary_size` is the byte length of the compiler-emitted (non-
/// instrumented) object — the machine code produced for the program, excluding
/// the separately-linked runtime. All are deterministic for a fixed toolchain
/// (codegen emits from ordered work-lists, not HashMap iteration), though
/// `binary_size` is target-specific (machine code + object format), unlike the
/// others.
#[derive(Clone, PartialEq, Eq)]
pub struct PerfStats {
    pub allocations: u64,
    pub deallocations: u64,
    pub reallocations: u64,
    pub bytes_allocated: u64,
    pub instructions: u64,
    pub binary_size: BinSizes,
    /// Every function the program ran or emitted code for, sorted by name — the
    /// `functions:` block. Compiled AIPL functions and runtime builtins share
    /// one list because the two questions a reader has (how often did this run,
    /// how big is it) are the same questions either way; which side of the
    /// runtime boundary a function sits on shows up as a `bytes: (external)`,
    /// since only this object's code is measured.
    pub functions: Vec<FnStat>,
}

/// One `functions:` entry. `calls` is how many times the function was *entered*
/// during the run (from the instrumented build's counters), `bytes` how much
/// code it occupies in the emitted object.
///
/// The two are worth reading together, and neither alone tells the story. Call
/// counts are what the `instructions executed` metric cannot see: a call into
/// the runtime is one CLIF instruction however much work it does, so a builtin's
/// count is the only sign of work crossing that boundary. Byte sizes are what
/// monomorphization did: every emitted function appears, so a change in what
/// gets specialized shows up here as an added or removed entry.
#[derive(Clone, PartialEq, Eq)]
pub struct FnStat {
    pub name: String,
    /// Times entered. `0` for a function that was emitted but never ran.
    pub calls: u64,
    /// Bytes of code in the object, or `None` — rendered `(external)` — for a
    /// function with no code *here*: a runtime builtin, resolved at link time
    /// from the separately-compiled runtime staticlib. Distinguished from a
    /// number so the two zeros can't be confused: `calls: 0` is a real count (an
    /// emitted specialization nothing reached), while a builtin's size is not
    /// zero but unmeasured, and deliberately so — the runtime is identical for
    /// every program, so folding it in would add the same large constant to
    /// every case. The numbers that *are* present sum to `code:` exactly.
    pub bytes: Option<u64>,
}

impl PerfStats {
    /// The `--- performance ---` body these stats represent.
    pub fn render(&self) -> String {
        let mut out = format!(
            "allocations: {}\ndeallocations: {}\nreallocations: {}\nbytes allocated: {}\n\
             instructions executed: {}\n{}",
            self.allocations,
            self.deallocations,
            self.reallocations,
            self.bytes_allocated,
            self.instructions,
            self.binary_size.render(),
        );
        out.push_str("\nfunctions:");
        for f in &self.functions {
            let bytes = match f.bytes {
                Some(n) => n.to_string(),
                None => EXTERNAL_BYTES.to_string(),
            };
            out.push_str(&format!(
                "\n  {}:\n    calls: {}\n    bytes: {bytes}",
                f.name, f.calls
            ));
        }
        out
    }

    /// Fold the object's per-function byte sizes into the call counts read back
    /// from the run, producing the single sorted `functions:` list.
    ///
    /// The two arrive under different names: the runtime reports the *symbol*
    /// it was handed, while `fn_sizes` has already been mapped to AIPL-level
    /// names — so the counts are mapped through the same table first (see
    /// [`crate::codegen::ObjectCompilation::code_symbol_names`]). A builtin is in neither table
    /// and keeps its own name.
    ///
    /// Neither side is a superset, and that is exactly what identifies an
    /// external: a function that never ran contributes only bytes, while an
    /// entry `fn_sizes` never matches has no code in this object at all and
    /// keeps its `None`.
    pub fn merge_fn_sizes(
        &mut self,
        names: &HashMap<String, String>,
        fn_sizes: Vec<(String, u64)>,
    ) {
        let mut by_name: HashMap<String, FnStat> = HashMap::new();
        for f in self.functions.drain(..) {
            let name = names.get(&f.name).cloned().unwrap_or(f.name);
            by_name.insert(name.clone(), FnStat { name, ..f });
        }
        for (name, bytes) in fn_sizes {
            by_name
                .entry(name.clone())
                .or_insert(FnStat {
                    name,
                    calls: 0,
                    bytes: None,
                })
                .bytes = Some(bytes);
        }
        self.functions = by_name.into_values().collect();
        self.functions.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

/// The object's `binary size` and where it goes. `code` is the executable
/// `__text`, `data` the read-only literal sections (string literals, …), and
/// `metadata` everything else in the object file — headers, the symbol + string
/// tables, relocations, and padding (i.e. `total - code - data`). The split is
/// deterministic for a fixed toolchain (like `total` itself) and is often
/// dominated by `metadata`: AIPL's long mangled symbol names and per-function
/// relocations cost more than the machine code for small programs.
#[derive(Clone, PartialEq, Eq)]
pub struct BinSizes {
    pub total: u64,
    pub code: u64,
    pub data: u64,
    pub metadata: u64,
}

impl BinSizes {
    /// Split an object's bytes into code/data/metadata (see the struct docs),
    /// returning that alongside `code`'s per-function parts — which the caller
    /// folds into the `functions:` block rather than reporting under `code:`,
    /// since a function's size only makes sense next to how often it ran.
    /// `names` maps object symbols back to the AIPL names they were compiled
    /// from (see [`crate::codegen::ObjectCompilation::code_symbol_names`]); a symbol that isn't
    /// in it — codegen's synthesized helpers — keeps its own name.
    pub fn of(obj_bytes: &[u8], names: &HashMap<String, String>) -> (BinSizes, Vec<(String, u64)>) {
        use object::{Object, ObjectSection, ObjectSymbol, SectionIndex, SectionKind, SymbolKind};
        let total = obj_bytes.len() as u64;
        let (mut code, mut data) = (0u64, 0u64);
        let mut code_fns: Vec<(String, u64)> = Vec::new();
        if let Ok(obj) = object::File::parse(obj_bytes) {
            // Mach-O prefixes every symbol with `_`; the name in `names` (and
            // the one worth reading) is the unprefixed one.
            let prefix = match obj.format() {
                object::BinaryFormat::MachO => "_",
                _ => "",
            };
            let mut text: Vec<(SectionIndex, u64, u64)> = Vec::new();
            for sec in obj.sections() {
                // On-disk byte span (BSS-style sections have none → 0), so
                // `code + data` never exceeds the file and `metadata` is the rest.
                let bytes = sec.file_range().map_or(0, |(_, len)| len);
                match sec.kind() {
                    SectionKind::Text => {
                        code += bytes;
                        text.push((sec.index(), sec.address(), bytes));
                    }
                    SectionKind::Data | SectionKind::ReadOnlyData | SectionKind::ReadOnlyString => {
                        data += bytes
                    }
                    _ => {}
                }
            }
            // Where each function starts. The object records no symbol *sizes*
            // (Mach-O has nowhere to put them), so a function's size is the
            // distance to the next one — and the last runs to the end of its
            // section. Padding between functions therefore lands on the one it
            // follows, and the parts sum to `code` exactly.
            let mut starts: HashMap<SectionIndex, Vec<(u64, String)>> = HashMap::new();
            for sym in obj.symbols() {
                let Some(ix) = sym.section_index() else {
                    continue;
                };
                // Only function starts: a text section can also carry section
                // symbols and embedded data, whose bytes belong to the function
                // they sit in rather than to a line of their own.
                if sym.kind() != SymbolKind::Text || !text.iter().any(|(t, _, _)| *t == ix) {
                    continue;
                }
                let raw = sym.name().unwrap_or("<unnamed>");
                let unprefixed = raw.strip_prefix(prefix).unwrap_or(raw);
                let name = names.get(unprefixed).map_or(unprefixed, String::as_str);
                starts
                    .entry(ix)
                    .or_default()
                    .push((sym.address(), name.to_string()));
            }
            for (ix, addr, size) in text {
                let mut syms = starts.remove(&ix).unwrap_or_default();
                syms.sort();
                for (i, (at, name)) in syms.iter().enumerate() {
                    // The first symbol absorbs any section-leading padding, so
                    // no bytes go unattributed.
                    let start = if i == 0 { addr } else { *at };
                    let end = syms.get(i + 1).map_or(addr + size, |(next, _)| *next);
                    code_fns.push((name.clone(), end.saturating_sub(start)));
                }
            }
        }
        code_fns.sort();
        (
            BinSizes {
                total,
                code,
                data,
                metadata: total.saturating_sub(code + data),
            },
            code_fns,
        )
    }

    /// The `binary size:` line plus its indented `code`/`data`/`metadata` split.
    pub fn render(&self) -> String {
        format!(
            "binary size: {}\n  code: {}\n  data: {}\n  metadata: {}",
            self.total, self.code, self.data, self.metadata
        )
    }
}

impl PerfStats {
    /// Parse the `allocations:` / `deallocations:` / `reallocations:` / `bytes
    /// allocated:` / `instructions executed:` / `binary size:` lines (order-
    /// independent, surrounding whitespace ignored) plus the `functions:` block
    /// (which is positional — see below). All six scalars are required; a missing
    /// line yields `None`.
    pub fn parse(s: &str) -> Option<PerfStats> {
        let mut allocations = None;
        let mut deallocations = None;
        let mut reallocations = None;
        let mut bytes_allocated = None;
        let mut instructions = None;
        let mut binary_size = None;
        let mut code = None;
        let mut data = None;
        let mut metadata = None;
        // The `functions:` block: a `  <name>:` heading per function, then its
        // `    calls:`/`    bytes:` lines. Read by *indent*, not by name — a
        // function may legitimately be called `calls`, `bytes`, or `metadata`, so
        // matching on the text would mistake a heading for a field (or for the
        // sibling line the block sits next to). A missing field defaults to 0,
        // which is what lets the runtime write calls-only entries the harness then
        // fills sizes into.
        let mut functions: Vec<FnStat> = Vec::new();
        let mut in_functions = false;
        for raw in s.lines() {
            let line = raw.trim();
            if line == "functions:" {
                in_functions = true;
                continue;
            }
            if in_functions {
                // A field of the entry above it (deeper indent), …
                if raw.starts_with("    ") {
                    if let (Some(f), Some((key, v))) = (functions.last_mut(), line.split_once(": "))
                    {
                        let v = v.trim();
                        match (key.trim(), v.parse::<u64>()) {
                            ("calls", Ok(n)) => {
                                f.calls = n;
                                continue;
                            }
                            ("bytes", Ok(n)) => {
                                f.bytes = Some(n);
                                continue;
                            }
                            // A builtin's size isn't 0, it's unmeasured — see
                            // [`FnStat::bytes`].
                            ("bytes", Err(_)) if v == EXTERNAL_BYTES => {
                                f.bytes = None;
                                continue;
                            }
                            _ => {}
                        }
                    }
                // … or a new entry's heading (`  <name>:`, no value).
                } else if raw.starts_with("  ") && line.ends_with(':') {
                    functions.push(FnStat {
                        name: line[..line.len() - 1].to_string(),
                        calls: 0,
                        bytes: None,
                    });
                    continue;
                }
                in_functions = false;
            }
            if let Some(v) = line.strip_prefix("bytes allocated:") {
                bytes_allocated = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("instructions executed:") {
                instructions = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("binary size:") {
                binary_size = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("code:") {
                code = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("data:") {
                data = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("metadata:") {
                metadata = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("allocations:") {
                allocations = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("deallocations:") {
                deallocations = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("reallocations:") {
                reallocations = v.trim().parse().ok();
            }
        }
        Some(PerfStats {
            allocations: allocations?,
            deallocations: deallocations?,
            reallocations: reallocations?,
            bytes_allocated: bytes_allocated?,
            instructions: instructions?,
            binary_size: BinSizes {
                total: binary_size?,
                code: code?,
                data: data?,
                metadata: metadata?,
            },
            functions,
        })
    }
}
