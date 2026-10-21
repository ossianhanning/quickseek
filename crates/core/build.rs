//! Bakes the build's identity into `RAPIDFIND_BUILD` ("0.1.0+4388850",
//! ".dirty" for uncommitted changes), so the service and its clients can say
//! exactly which build they are.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let build = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(hash) => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            format!("{version}+{hash}{}", if dirty { ".dirty" } else { "" })
        }
        None => version,
    };
    println!("cargo:rustc-env=RAPIDFIND_BUILD={build}");
    // Rebuild when HEAD moves or the index changes (commits, staging).
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &r])
    {
        println!("cargo:rerun-if-changed={path}");
    }
}
