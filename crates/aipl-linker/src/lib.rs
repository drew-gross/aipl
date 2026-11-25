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
/// reports them at exit. Linked via [`link_instrumented`] for `aipl report`,
/// `measure` and the cases harness; behaves identically otherwise.
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
    // The runtime's `http_get` is libcurl's, which macOS ships.
    cmd.arg("-lcurl");
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

/// A fresh empty directory under the system temp dir, named `aipl-<prefix>-…`.
///
/// PID + monotonic counter keeps concurrent callers — parallel cargo-test
/// threads, or several `aipl` processes at once — from colliding on the same
/// path. The caller owns cleanup.
pub fn scratch_dir(prefix: &str) -> Result<PathBuf, Error> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = env::temp_dir().join(format!("aipl-{prefix}-{pid}-{n}"));
    fs::create_dir_all(&dir).map_err(|e| Error::msg(format!("create {}: {e}", dir.display())))?;
    Ok(dir)
}

/// [`scratch_dir`] named for what it is for: `aipl-<prefix>-<name>-…`, with `name` (a test's path,
/// say) cut down to characters a directory name can hold and to its last 80, so the directory says
/// whose it is when it is found lying around.
pub fn scratch_dir_for(prefix: &str, name: &str) -> Result<PathBuf, Error> {
    let kept: Vec<char> = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let tail: String = kept[kept.len().saturating_sub(80)..].iter().collect();
    scratch_dir(&format!("{prefix}-{tail}"))
}

fn staging_dir() -> Result<PathBuf, Error> {
    scratch_dir("build")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_directory_is_named_for_what_it_is_for() {
        let dir = scratch_dir_for("check", "tests/cases/files/read file").unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("aipl-check-tests_cases_files_read_file-"),
            "{name}"
        );
        assert!(dir.is_dir());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_long_name_keeps_its_end() {
        let long = format!("{}/the_test", "x".repeat(200));
        let dir = scratch_dir_for("check", &long).unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.contains("_the_test-"), "{name}");
        assert!(name.len() < 120, "{name}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
