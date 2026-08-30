//! Exposes the build's target triple and profile to the environment manifest.
//!
//! The target triple is one of the fields the baseline-refusal rule keys on:
//! a differential comparison across triples is a comparison across instruction
//! sets, and the harness must refuse it as environment-mismatched rather than
//! report a ratio about nothing. `TARGET` and `PROFILE` are cargo build-script
//! inputs, so they describe the binary that is actually running, not whatever
//! happens to be on PATH at run time.

fn main() {
    println!(
        "cargo:rustc-env=MIGRATION_BENCH_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string())
    );
    println!(
        "cargo:rustc-env=MIGRATION_BENCH_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string())
    );
    println!("cargo:rerun-if-changed=build.rs");
}
