use std::env;
use std::fs;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .ok()
            .map(|revision| revision.trim().to_owned())
    } else {
        None
    }
}

fn main() {
    // The target triple the banking row names as its platform. The ledger
    // folds hours per platform by comparing this against the host's own
    // stamp (`P1_SWARM_TARGET`), so both are captured from cargo's TARGET.
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=ORRERY_BUILD_TARGET={target}");

    println!("cargo:rerun-if-env-changed=ORRERY_BUILD_REV");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
        if let Ok(reference) = fs::read_to_string(&head) {
            if let Some(reference) = reference.trim().strip_prefix("ref: ") {
                if let Some(path) = git(&["rev-parse", "--git-path", reference]) {
                    println!("cargo:rerun-if-changed={path}");
                }
            }
        }
    }

    // The packaging workflow supplies ORRERY_BUILD_REV. Falling back to git
    // keeps local builds identifiable without a build-info dependency.
    let revision = env::var("ORRERY_BUILD_REV")
        .ok()
        .or_else(|| env::var("GITHUB_SHA").ok())
        .or_else(|| git(&["rev-parse", "--verify", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=ORRERY_BUILD_REV={revision}");
}
