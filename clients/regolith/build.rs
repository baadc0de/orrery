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
    // `proton_debug` is set by `scripts/proton-debug-build.sh` through
    // RUSTFLAGS, never from `Cargo.toml`, and it is what
    // `orrery_regolith_client::BANKABLE` reads (#1060). Declaring it here is
    // what keeps `unexpected_cfgs` quiet without touching `Cargo.toml`.
    //
    // A cargo *feature* was the obvious spelling and is the wrong one. A
    // feature is a manifest fact: it shows up in `Cargo.toml`, in `cargo
    // tree`, in cargo's `-C metadata`, and in every consumer's feature
    // unification -- measured, adding an unused `[features]` table changed the
    // release binary's hash with no source change at all. A bare `--cfg` is
    // invisible to cargo, so the manifest and the lockfile that
    // `package-client.yml` builds `--locked` against stay exactly as they are.
    //
    // What that does *not* buy is a byte-identical binary, and it is worth
    // being exact about why. This binary is a deterministic function of its
    // source and nothing else -- two builds of the same tree hash the same --
    // but *any* edit moves the hash, including appending a comment
    // (e4cccd1... -> b8baee0... -> e4cccd1..., measured). So the guarantee
    // here is the stronger and checkable one: with the cfg unset, every
    // refusal is stripped before lowering, so the shipped build runs the code
    // it ran before this change.
    println!("cargo::rustc-check-cfg=cfg(proton_debug)");
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

    // The banking row's `platform_triple` must equal the harness report's
    // `identity.target` — a Rust target triple — or `p4-ledger.sh`'s
    // `validate_session_record` refuses the row. Cargo hands every build
    // script the triple in TARGET; `std::env::consts` cannot reconstruct it
    // at runtime ("linux-x86_64" is not a triple, and that spelling is what
    // this replaced).
    let target = env::var("TARGET").expect("cargo always sets TARGET for build scripts");
    println!("cargo:rustc-env=ORRERY_PLATFORM_TRIPLE={target}");
}
