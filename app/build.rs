//! Bakes the build's git commit into the binary as `STAMPEDE_BUILD`, for the
//! version label ("v0.1.0 · ff5ed5f"). `STAMPEDE_GIT_HASH` (set by
//! tools/build-apk.sh) wins; else `git rev-parse --short HEAD`, with "+dirty"
//! when tracked files have uncommitted changes; "dev" without git.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=STAMPEDE_GIT_HASH");
    let build = match std::env::var("STAMPEDE_GIT_HASH") {
        Ok(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => match git(&["rev-parse", "--short", "HEAD"]) {
            Some(hash) if !hash.is_empty() => {
                let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
                if dirty {
                    format!("{hash}+dirty")
                } else {
                    hash
                }
            }
            _ => "dev".to_string(),
        },
    };
    println!("cargo:rustc-env=STAMPEDE_BUILD={build}");
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/refs");
    }
    // Only the manifest and Android files: a dirty tree changes the label on
    // the next real build; rebuilding on every source edit is cargo's job.
    println!("cargo:rerun-if-changed=Cargo.toml");
}
