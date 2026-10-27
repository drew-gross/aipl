//! The staged-IR override, compiled once and shared by every process.
//!
//! `AIPL_DOGFOOD_IR` names a *candidate* dogfood artifact — CLIF text — so the
//! whole corpus can be run against it before it is promoted. The object linked
//! into the binary was built from the live artifact and cannot speak for a
//! candidate, so the candidate has to be compiled and loaded some other way.
//!
//! Loading it by JIT meant every process compiled all of it: ~3.7s and ~750 MB
//! each, in each of the hundreds of processes a corpus run starts (the test
//! harness runs every test in its own, and spawns the compiler besides) and
//! again in every thread of them that parses. Twelve at once is ~9 GB.
//!
//! Instead, the first process to need a given file compiles it **once**, the way
//! promotion does ([`aipl_artifact::emit_object`]), links the object into a
//! shared library beside the CLIF, and every process — that one included —
//! `dlopen`s the library and calls its entries through the same name→address
//! table the prebuilt object uses ([`Compilation::from_prebuilt`]). It is also
//! closer to what promotion will ship than a JIT is: the same lowering, the same
//! position-independent object.
//!
//! The library leaves its runtime calls (`aipl_str_alloc`, …) undefined and
//! binds them when it is loaded, to the same functions in the host binary the
//! JIT registers by address. That needs the host to export them, which the
//! binaries do because the prebuilt object references them; the tests under
//! `dogfood_ir` load a library into the test binary to keep that true.

use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use crate::PrebuiltFn;

/// The extension of a shared library on this platform.
const LIBRARY_EXTENSION: &str = if cfg!(target_os = "macos") {
    "dylib"
} else {
    "so"
};

/// An artifact's entry points as loaded into this process: the manifest text and
/// the name→address table [`crate::Compilation::from_prebuilt`] takes. Leaked,
/// because that is what `'static` is and the library is never unloaded.
#[derive(Clone, Copy)]
pub(crate) struct Loaded {
    pub manifest: &'static str,
    pub entries: &'static [(&'static str, PrebuiltFn)],
}

/// The two files that make up a staged artifact's compiled form, named by the
/// CLIF they were built from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Files {
    library: PathBuf,
    manifest: PathBuf,
}

impl Files {
    /// Both exist. The manifest is moved into place last, so a manifest that is
    /// there means the library is too.
    fn complete(&self) -> bool {
        self.library.exists() && self.manifest.exists()
    }

    /// The same files under names no other builder will use, for a build to
    /// write into before moving them to their own.
    fn scratch(&self) -> Files {
        let pid = std::process::id();
        let rename = |p: &Path| {
            let mut name = p.file_name().expect("a named file").to_os_string();
            name.push(format!(".building{pid}"));
            p.with_file_name(name)
        };
        Files {
            library: rename(&self.library),
            manifest: rename(&self.manifest),
        }
    }
}

/// The files for `clif` as it is now. Named by its size and modification time,
/// so a regenerated candidate gets new files without reading 70 MB to find out
/// that it is new.
fn files_for(clif: &Path) -> Result<Files, String> {
    let meta = fs::metadata(clif).map_err(|e| format!("could not read IR at {clif:?}: {e}"))?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let name = file_name(clif);
    let stem = format!("{name}.{:x}-{modified:x}", meta.len());
    Ok(Files {
        library: clif.with_file_name(format!("{stem}.{LIBRARY_EXTENSION}")),
        manifest: clif.with_file_name(format!("{stem}.manifest")),
    })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// The file whose exclusive lock serializes builders of `clif`'s library.
fn lock_path(clif: &Path) -> PathBuf {
    clif.with_file_name(format!("{}.lock", file_name(clif)))
}

/// Delete everything built for `clif` — its libraries, manifests and any build
/// that was interrupted — except the files named by `keep`. The lock stays.
fn prune(clif: &Path, keep: Option<&Files>) {
    let dir = match clif.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let prefix = format!("{}.", file_name(clif));
    let lock = file_name(&lock_path(clif));
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = file_name(&path);
        let ours = name.starts_with(&prefix) && name != lock;
        let kept = keep.is_some_and(|k| {
            path.file_name() == k.library.file_name() || path.file_name() == k.manifest.file_name()
        });
        if ours && !kept {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Delete everything built for `clif`. For the commands that replace or retire a
/// staged file, which would otherwise leave libraries nothing refers to.
pub fn remove_libraries(clif: &Path) {
    prune(clif, None);
    let _ = fs::remove_file(lock_path(clif));
}

/// The compiled form of `clif`, built by `build` if no process has built it yet.
///
/// Many processes ask at once — a corpus run starts a dozen together — and the
/// first to take the lock builds while the rest wait for it, then find the
/// result. `build` writes the files it is given, which are scratch names, and
/// they are moved into place only once it has finished, so a process that dies
/// mid-build leaves nothing that looks complete.
fn ensure_library(
    clif: &Path,
    build: impl FnOnce(&Files) -> Result<(), String>,
) -> Result<Files, String> {
    let files = files_for(clif)?;
    if files.complete() {
        return Ok(files);
    }
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(clif))
        .map_err(|e| format!("could not open the lock beside {clif:?}: {e}"))?;
    // Released when `lock` closes, including if this process dies.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "could not lock {:?}: {}",
            lock_path(clif),
            std::io::Error::last_os_error()
        ));
    }
    // Whoever held the lock may have built it.
    if files.complete() {
        return Ok(files);
    }
    // A candidate that was regenerated leaves its old libraries behind.
    prune(clif, Some(&files));
    let scratch = files.scratch();
    let built = build(&scratch);
    let moved = built.and_then(|()| {
        fs::rename(&scratch.library, &files.library)
            .and_then(|()| fs::rename(&scratch.manifest, &files.manifest))
            .map_err(|e| format!("could not move the built library into place: {e}"))
    });
    if moved.is_err() {
        let _ = fs::remove_file(&scratch.library);
        let _ = fs::remove_file(&scratch.manifest);
        let _ = fs::remove_file(&files.library);
    }
    moved.map(|()| files)
}

/// Compile the candidate artifact `clif` to a shared library at `out.library` and
/// the manifest header beside it at `out.manifest`.
///
/// `name` and `prefix` are the artifact's ([`aipl_artifact::ArtifactFiles`]); the
/// prefix is what makes its entries exported symbols.
fn build_library(clif: &Path, name: &str, prefix: &str, out: &Files) -> Result<(), String> {
    let text = fs::read_to_string(clif).map_err(|e| format!("could not read {clif:?}: {e}"))?;
    let (object, header) = aipl_artifact::emit_object(&text, name, prefix)?;
    // The IR is tens of megabytes of text and the linker is about to run.
    drop(text);
    let object_path = out
        .library
        .with_file_name(format!("{}.o", file_name(&out.library)));
    fs::write(&object_path, object).map_err(|e| format!("write {object_path:?}: {e}"))?;
    fs::write(&out.manifest, header).map_err(|e| format!("write {:?}: {e}", out.manifest))?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let mut link = Command::new(&cc);
    if cfg!(target_os = "macos") {
        // The runtime calls are left to bind when the library is loaded.
        link.args(["-dynamiclib", "-undefined", "dynamic_lookup"]);
    } else {
        link.arg("-shared");
    }
    let output = link
        .arg("-o")
        .arg(&out.library)
        .arg(&object_path)
        .output()
        .map_err(|e| format!("could not run {cc} to link {object_path:?}: {e}"))?;
    let _ = fs::remove_file(&object_path);
    if !output.status.success() {
        return Err(format!(
            "{cc} failed to link the staged library:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Load the library in `files` into this process and resolve every entry the
/// manifest lists, each exported as `{prefix}{entry}`.
fn open(files: &Files, prefix: &str) -> Result<Loaded, String> {
    // The library calls the runtime by name, and a host has only the runtime
    // functions its linker kept. A binary that calls some of the artifact's
    // entries — a crate's own tests, say — has the linker strip the others, and
    // with them the runtime functions only they reach; but the library holds
    // every entry. `new_jit_module` takes the address of every runtime function,
    // so a binary that can load a library keeps them all, and exports them.
    std::hint::black_box(crate::new_jit_module as fn(bool) -> _);
    let manifest_text = fs::read_to_string(&files.manifest)
        .map_err(|e| format!("could not read {:?}: {e}", files.manifest))?;
    let manifest = aipl_artifact::parse_manifest(&manifest_text)?;
    let path = CString::new(files.library.as_os_str().as_bytes())
        .map_err(|_| format!("{:?} holds a NUL", files.library))?;
    // NOW, so an unbound runtime call is an error here and not a crash in the
    // middle of some later parse. LOCAL, so nothing else binds into it.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("dlopen {:?}: {}", files.library, dl_error()));
    }
    let mut entries = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let symbol = CString::new(format!("{prefix}{}", entry.name))
            .map_err(|_| format!("entry {:?} holds a NUL", entry.name))?;
        let address = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
        if address.is_null() {
            return Err(format!(
                "{:?} does not export {symbol:?}: {}",
                files.library,
                dl_error()
            ));
        }
        let function = unsafe { std::mem::transmute::<*mut c_void, PrebuiltFn>(address) };
        let name: &'static str = Box::leak(entry.name.clone().into_boxed_str());
        entries.push((name, function));
    }
    Ok(Loaded {
        manifest: Box::leak(manifest_text.into_boxed_str()),
        entries: Box::leak(entries.into_boxed_slice()),
    })
}

/// The last `dl*` error, as text.
fn dl_error() -> String {
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "no error reported".into();
    }
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

/// A shared library's entries, loaded from `library` with the manifest header
/// `manifest` beside it. For a library built some other way than [`load`] does.
pub(crate) fn open_library(
    library: &Path,
    manifest: &Path,
    prefix: &str,
) -> Result<Loaded, String> {
    open(
        &Files {
            library: library.to_path_buf(),
            manifest: manifest.to_path_buf(),
        },
        prefix,
    )
}

/// The candidate artifact `clif` loaded into this process, compiled first if no
/// process has compiled this version of it.
///
/// Once per file per process, however many threads ask: each thread builds its
/// own engine over the same loaded entries.
pub(crate) fn load(clif: &Path, name: &str, prefix: &str) -> Result<Loaded, String> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Loaded>>> = OnceLock::new();
    // Held across the build, so threads of one process wait for it rather than
    // each queueing on the file lock.
    let mut loaded = LOADED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "the staged-library table was poisoned".to_string())?;
    if let Some(entries) = loaded.get(clif) {
        return Ok(*entries);
    }
    let files = ensure_library(clif, |out| build_library(clif, name, prefix, out))?;
    let entries = open(&files, prefix)?;
    loaded.insert(clif.to_path_buf(), entries);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory of its own, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(label: &str) -> Dir {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "aipl-staged-library-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|e| file_name(&e.unwrap().path()))
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A builder that writes two files and counts how often it ran.
    fn counting(count: &AtomicUsize) -> impl FnOnce(&Files) -> Result<(), String> + '_ {
        move |out| {
            count.fetch_add(1, Ordering::SeqCst);
            fs::write(&out.library, "library").unwrap();
            fs::write(&out.manifest, "; manifest").unwrap();
            Ok(())
        }
    }

    #[test]
    fn the_files_are_named_by_the_clif_and_its_version() {
        let dir = Dir::new("named");
        let clif = dir.file("dogfood.clif.staged", "abc");
        let files = files_for(&clif).unwrap();
        let name = file_name(&files.library);
        // `<clif>.<size in hex>-<mtime in hex>.<library extension>`
        assert!(
            name.starts_with("dogfood.clif.staged.3-") && name.ends_with(LIBRARY_EXTENSION),
            "{name}"
        );
        assert_eq!(
            file_name(&files.manifest),
            name.replace(&format!(".{LIBRARY_EXTENSION}"), ".manifest")
        );
        // Changed contents are a new version, hence new files.
        fs::write(&clif, "abcdef").unwrap();
        assert_ne!(files_for(&clif).unwrap(), files);
    }

    #[test]
    fn a_library_is_built_once_and_found_afterwards() {
        let dir = Dir::new("once");
        let clif = dir.file("dogfood.clif.staged", "ir");
        let built = AtomicUsize::new(0);
        let first = ensure_library(&clif, counting(&built)).unwrap();
        let second = ensure_library(&clif, counting(&built)).unwrap();
        assert_eq!(first, second);
        assert_eq!(built.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read_to_string(&first.library).unwrap(), "library");
        assert_eq!(fs::read_to_string(&first.manifest).unwrap(), "; manifest");
    }

    #[test]
    fn processes_asking_together_build_it_once() {
        let dir = Dir::new("together");
        let clif = dir.file("dogfood.clif.staged", "ir");
        let built = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    ensure_library(&clif, |out| {
                        built.fetch_add(1, Ordering::SeqCst);
                        // Long enough that the others are waiting on the lock.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        fs::write(&out.library, "library").unwrap();
                        fs::write(&out.manifest, "; manifest").unwrap();
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(built.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_regenerated_candidate_replaces_the_old_library() {
        let dir = Dir::new("regenerated");
        let clif = dir.file("dogfood.clif.staged", "first");
        let built = AtomicUsize::new(0);
        let old = ensure_library(&clif, counting(&built)).unwrap();
        fs::write(&clif, "a second, longer candidate").unwrap();
        let new = ensure_library(&clif, counting(&built)).unwrap();
        assert_ne!(old, new);
        assert_eq!(built.load(Ordering::SeqCst), 2);
        assert!(!old.library.exists() && !old.manifest.exists());
        assert!(new.complete());
    }

    #[test]
    fn a_build_that_fails_leaves_nothing_that_looks_complete() {
        let dir = Dir::new("failed");
        let clif = dir.file("dogfood.clif.staged", "ir");
        let failed = ensure_library(&clif, |out| {
            // Written, and then the build gives up.
            fs::write(&out.library, "half").unwrap();
            Err("the linker failed".to_string())
        });
        assert_eq!(failed.unwrap_err(), "the linker failed");
        assert!(!files_for(&clif).unwrap().complete());
        // Nothing but the file and its lock is left, and a retry builds.
        assert_eq!(
            dir.names(),
            ["dogfood.clif.staged", "dogfood.clif.staged.lock"]
        );
        let built = AtomicUsize::new(0);
        ensure_library(&clif, counting(&built)).unwrap();
        assert_eq!(built.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn removing_the_libraries_leaves_the_clif_and_other_files_alone() {
        let dir = Dir::new("removed");
        let clif = dir.file("dogfood.clif.staged", "ir");
        // Neither is named for this CLIF: one shares only a prefix with it.
        dir.file("dogfood.clif.staged-other", "not ours");
        dir.file("dogfood.manifest", "; live");
        ensure_library(&clif, counting(&AtomicUsize::new(0))).unwrap();
        assert_eq!(dir.names().len(), 6, "{:?}", dir.names());
        remove_libraries(&clif);
        assert_eq!(
            dir.names(),
            [
                "dogfood.clif.staged",
                "dogfood.clif.staged-other",
                "dogfood.manifest"
            ]
        );
    }

    #[test]
    fn a_clif_that_is_not_there_is_an_error_naming_it() {
        let dir = Dir::new("missing");
        let missing = dir.0.join("dogfood.clif.staged");
        let error = ensure_library(&missing, |_| Ok(())).unwrap_err();
        assert!(error.starts_with("could not read IR at"), "{error}");
    }
}
