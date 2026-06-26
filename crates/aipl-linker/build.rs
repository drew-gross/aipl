//! Builds the AIPL standalone runtime as a static library and stashes the
//! resulting `.a`/`.lib` path in an env var that source code reads via
//! `env!`/`include_bytes!`.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=runtime/aipl_runtime.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let target = env::var("TARGET").expect("TARGET");
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());

    // The default runtime, plus an instrumented variant that counts heap
    // allocations/frees (built with `--cfg aipl_instrument`). The harness links
    // the instrumented one for `--- performance ---` checks; everything else
    // uses the default. They differ only in the crate name (so the output
    // filenames don't collide) and the cfg flag.
    let (lib_path, lib_name) = build_runtime(&rustc, &target, &out_dir, "aipl_runtime", false);
    let (instr_path, instr_name) =
        build_runtime(&rustc, &target, &out_dir, "aipl_runtime_instr", true);

    println!(
        "cargo:rustc-env=AIPL_RUNTIME_LIB_PATH={}",
        lib_path.display()
    );
    println!("cargo:rustc-env=AIPL_RUNTIME_LIB_NAME={lib_name}");
    println!(
        "cargo:rustc-env=AIPL_RUNTIME_INSTR_LIB_PATH={}",
        instr_path.display()
    );
    println!("cargo:rustc-env=AIPL_RUNTIME_INSTR_LIB_NAME={instr_name}");
}

/// Build `runtime/aipl_runtime.rs` as a staticlib named `crate_name`, returning
/// `(lib_path, lib_file_name)`. When `instrument` is set, `--cfg aipl_instrument`
/// turns on the allocation counters.
fn build_runtime(
    rustc: &str,
    target: &str,
    out_dir: &PathBuf,
    crate_name: &str,
    instrument: bool,
) -> (PathBuf, String) {
    let mut cmd = Command::new(rustc);
    cmd.args([
        "--edition=2021",
        "--crate-type=staticlib",
        "--crate-name",
        crate_name,
        "-C",
        "opt-level=3",
        "-C",
        "panic=abort",
        "--target",
    ])
    .arg(target)
    .arg("--out-dir")
    .arg(out_dir);
    if instrument {
        cmd.args(["--cfg", "aipl_instrument"]);
    }
    let status = cmd
        .arg("runtime/aipl_runtime.rs")
        .status()
        .unwrap_or_else(|e| panic!("invoke {rustc}: {e}"));
    if !status.success() {
        panic!("rustc failed to build runtime staticlib {crate_name} (exit {status})");
    }

    let lib_name = format!("lib{crate_name}.a");
    let lib_path = out_dir.join(&lib_name);
    if !lib_path.exists() {
        panic!(
            "expected runtime staticlib at {} (target={target}); found neither",
            lib_path.display()
        );
    }
    (lib_path, lib_name)
}
