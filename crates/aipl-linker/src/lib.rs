//! AOT binary linking. Takes the cranelift-produced object bytes and the
//! runtime staticlib that `build.rs` produced, then drives `clang` as the
//! linker to emit a native executable.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use aipl_syntax::Error;

/// Bytes of the runtime staticlib (libaipl_runtime.a / aipl_runtime.lib)
/// emitted by `build.rs`. Embedded so consumers don't need a separate file.
const RUNTIME_LIB_BYTES: &[u8] = include_bytes!(env!("AIPL_RUNTIME_LIB_PATH"));
const RUNTIME_LIB_NAME: &str = env!("AIPL_RUNTIME_LIB_NAME");

/// The instrumented runtime variant, which counts heap allocations/frees and
/// reports them at exit. Linked via [`link_instrumented`] for the test
/// harness's `--- performance ---` checks; behaves identically otherwise.
const RUNTIME_INSTR_LIB_BYTES: &[u8] = include_bytes!(env!("AIPL_RUNTIME_INSTR_LIB_PATH"));
const RUNTIME_INSTR_LIB_NAME: &str = env!("AIPL_RUNTIME_INSTR_LIB_NAME");

/// Link `obj_bytes` (a cranelift-emitted relocatable object) into a
/// native executable at `output`. Stages files in a per-call temp dir and
/// invokes `clang` as the linker driver — clang figures out libc / startup
/// paths for us, which is why we don't talk to `link.exe` / `ld` directly.
pub fn link(obj_bytes: &[u8], output: &Path) -> Result<(), Error> {
    link_with(obj_bytes, output, RUNTIME_LIB_BYTES, RUNTIME_LIB_NAME)
}

/// Like [`link`], but links the allocation-instrumented runtime. The resulting
/// binary writes its alloc/dealloc tallies to the path named by the
/// `AIPL_ALLOC_STATS` environment variable when run.
pub fn link_instrumented(obj_bytes: &[u8], output: &Path) -> Result<(), Error> {
    link_with(
        obj_bytes,
        output,
        RUNTIME_INSTR_LIB_BYTES,
        RUNTIME_INSTR_LIB_NAME,
    )
}

fn link_with(obj_bytes: &[u8], output: &Path, rt_bytes: &[u8], rt_name: &str) -> Result<(), Error> {
    let clang = which("clang").ok_or_else(|| {
        Error::msg("could not find \"clang\" on PATH (required to link aipl binaries)".to_string())
    })?;

    let staging = staging_dir()?;
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("aipl_out");
    let obj_path = staging.join(format!("{stem}.{}", object_ext()));
    let rt_path = staging.join(rt_name);
    fs::write(&obj_path, obj_bytes)
        .map_err(|e| Error::msg(format!("write {}: {e}", obj_path.display())))?;
    fs::write(&rt_path, rt_bytes)
        .map_err(|e| Error::msg(format!("write {}: {e}", rt_path.display())))?;

    let mut cmd = Command::new(&clang);
    cmd.arg(&obj_path).arg(&rt_path).arg("-o").arg(output);
    // Cranelift-emitted objects lack an LC_BUILD_VERSION load command, which
    // triggers a harmless ld warning on macOS. Suppress all ld warnings so
    // the test output stays clean; real errors are still errors (exit ≠ 0).
    #[cfg(target_os = "macos")]
    cmd.arg("-Wl,-w");
    let status = cmd
        .status()
        .map_err(|e| Error::msg(format!("spawn {}: {e}", clang.display())))?;
    if !status.success() {
        return Err(Error::msg(format!("clang exited with status {status}")));
    }

    let _ = fs::remove_dir_all(&staging);
    Ok(())
}

/// Default executable name for a given source stem.
pub fn default_exe_name(stem: &str) -> String {
    stem.to_string()
}

fn object_ext() -> &'static str {
    "o"
}

fn staging_dir() -> Result<PathBuf, Error> {
    // PID + monotonic counter keeps concurrent in-process callers (e.g.
    // parallel cargo-test threads) from colliding on the same temp dir.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = env::temp_dir().join(format!("aipl-build-{pid}-{n}"));
    fs::create_dir_all(&dir).map_err(|e| Error::msg(format!("create {}: {e}", dir.display())))?;
    Ok(dir)
}

fn which(prog: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
