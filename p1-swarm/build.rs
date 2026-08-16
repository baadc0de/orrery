//! Stamps the harness with the two facts a report cannot recover at runtime:
//! the target triple it was compiled for and the commit it was built from.
//!
//! `std` exposes the architecture and the OS but never the triple, and a binary
//! knows nothing about its own provenance. Both are baked in here so that a
//! report found in a nightly artifact names the code that produced it — which
//! is what turns a run into evidence rather than a number.
//!
//! A checkout with no `git` — a source tarball, a vendored build — stamps
//! `unknown` rather than failing the build.

use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=P1_SWARM_TARGET={target}");

    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map_or_else(|| "unknown".to_owned(), |sha| sha.trim().to_owned());
    println!("cargo:rustc-env=P1_SWARM_COMMIT={commit}");

    // The stamp would otherwise be whatever the first build of this `target/`
    // happened to see. `.git/HEAD` moves on every commit and every branch
    // switch, which is the granularity the stamp is about; when it does not
    // exist the script simply re-runs every build, which is the safe direction.
    println!("cargo:rerun-if-changed=../.git/HEAD");
}
