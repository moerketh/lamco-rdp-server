//! Build script for lamco-rdp-server
//!
//! Sets compile-time environment variables for build identification.

use std::process::Command;

fn run_command(program: &str, args: &[&str], fallback: &str) -> String {
    match Command::new(program).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => fallback.to_string(),
    }
}

fn main() {
    if std::env::var_os("CARGO_FEATURE_X264").is_some() {
        cc::Build::new()
            .file("src/egfx/x264_shim.c")
            .warnings(true)
            .compile("lamco_x264_shim");
        println!("cargo:rerun-if-changed=src/egfx/x264_shim.c");
    }

    let date_output = run_command("date", &["+%Y-%m-%d"], "unknown");
    println!("cargo:rustc-env=BUILD_DATE={date_output}");

    let time_output = run_command("date", &["+%H:%M:%S"], "");
    println!("cargo:rustc-env=BUILD_TIME={time_output}");

    let git_hash = run_command("git", &["rev-parse", "--short", "HEAD"], "unknown");
    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    // Re-run when HEAD or the resolved ref target changes.
    //
    // .git/HEAD is a symbolic ref on a branch (e.g. "ref: refs/heads/main") and
    // its file contents do not change between commits — only the resolved target
    // (.git/refs/heads/<branch> for loose refs, .git/packed-refs after gc) does.
    // Watching only .git/HEAD leaves GIT_HASH stale across same-branch commits.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(ref_path) = head.trim().strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{ref_path}");
    }
}
