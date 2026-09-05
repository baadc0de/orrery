//! The shipped sidecar binary: canonical rules in Lightyear's tick, poses in
//! the authority's ring, verdicts on the wire — and, when asked, presentation
//! frames on a link an engine can render from.
//!
//! `orrery-sidecar` is the repository's first shipped binary that builds a
//! Bevy `App` over the client facade. Before it, every consumer of
//! `OrreryClientPlugins` — and so of the pose ring hit claims are validated
//! against — was a test or an example, which is the precise failure #871
//! names: a fully designed mechanism with no production caller.
//!
//! # Usage
//!
//! ```text
//! orrery-sidecar [--serve ADDR] [--seed N] [--entity ID] [--entities N]
//!                [--stand-in-remote ID]
//! ```
//!
//! | Flag | Effect |
//! |---|---|
//! | `--serve ADDR` | Bind an IPC listener (`127.0.0.1:0` for an OS-chosen port) and stream extracted batches to one observer. Off by default: an unobserved sidecar opens no socket. |
//! | `--seed N` | The node seed, so a scenario's node ids are reproducible. |
//! | `--entity ID` | The stable id of the first entity this sidecar simulates. |
//! | `--entities N` | How many predicted entities to simulate, ids `ID..ID+N-1`. Defaults to 1. A population flag rather than a repeated `--entity` because that is the shape `examples/extract_cost.rs` measures (`--entities 24`), and a crossing measured at some other N is not comparable to it. |
//! | `--stand-in-remote ID` | Also present an *interpolated* entity under that id. See the warning on [`orrery_sidecar::spawn_stand_in_remote`]: the presentation path is real, the peer is not. |
//!
//! With `--serve`, the bound address is printed on stdout as
//! `orrery-sidecar: serving ipc on ADDR` before the app runs, so a launcher
//! or a test can read the port off the first line rather than guessing it.

use std::process::ExitCode;

use bevy::prelude::AppExit;
use orrery_protocol::PersistId;
use orrery_sidecar::{
    secret, sidecar, sidecar_serving, spawn_predicted, spawn_stand_in_remote, IpcServer,
};

/// The node seed. Deterministic so a scenario's node ids are reproducible;
/// a real deployment supplies its own key.
const NODE_SEED: u8 = 9;

/// The first entity this sidecar simulates and holds.
const ENTITY: PersistId = PersistId::new(1);

/// How many predicted entities a sidecar carries unless asked for more.
const ENTITIES: u64 = 1;

/// What the command line asked for.
struct Options {
    seed: u8,
    entity: PersistId,
    entities: u64,
    serve: Option<String>,
    stand_in_remote: Option<PersistId>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            seed: NODE_SEED,
            entity: ENTITY,
            entities: ENTITIES,
            serve: None,
            stand_in_remote: None,
        }
    }
}

/// Parse the flags this binary understands, refusing anything else.
///
/// Hand-rolled rather than `clap`: five flags, and the workspace does not
/// carry an argument parser as a shipped dependency. An unrecognised flag is
/// an error rather than a silently ignored word — the failure mode of a
/// launcher whose typo'd `--serv` left the sidecar unobserved and silent is
/// worth four lines here.
fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--serve" => options.serve = Some(value),
            "--seed" => {
                options.seed = value.parse().map_err(|_| format!("bad --seed: {value}"))?;
            }
            "--entity" => {
                options.entity = PersistId::new(
                    value
                        .parse()
                        .map_err(|_| format!("bad --entity: {value}"))?,
                );
            }
            "--entities" => {
                options.entities = value
                    .parse()
                    .map_err(|_| format!("bad --entities: {value}"))?;
                if options.entities == 0 {
                    return Err("--entities must be at least 1".to_owned());
                }
            }
            "--stand-in-remote" => {
                options.stand_in_remote = Some(PersistId::new(
                    value
                        .parse()
                        .map_err(|_| format!("bad --stand-in-remote: {value}"))?,
                ));
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(problem) => {
            eprintln!("orrery-sidecar: {problem}");
            eprintln!(
                "usage: orrery-sidecar [--serve ADDR] [--seed N] [--entity ID] [--entities N] [--stand-in-remote ID]"
            );
            return ExitCode::from(2);
        }
    };

    let key = secret(options.seed);
    let authority = key.public();
    let mut app = match options.serve.as_deref() {
        None => sidecar(key, true),
        Some(addr) => match IpcServer::bind(addr) {
            Ok(server) => {
                // Printed before the app runs, and flushed, because a launcher
                // reading the port must not be waiting on a buffer that only
                // empties when the process exits.
                println!("orrery-sidecar: serving ipc on {}", server.bound());
                sidecar_serving(key, true, server)
            }
            Err(problem) => {
                eprintln!("orrery-sidecar: cannot serve on {addr}: {problem}");
                return ExitCode::from(1);
            }
        },
    };
    // Consecutive ids from `--entity`, which is what `examples/extract_cost.rs`
    // spawns (`PersistId::new(n + 1)` for `n` in `0..entities`) so a population
    // here is the same population there.
    for offset in 0..options.entities {
        spawn_predicted(
            &mut app,
            authority,
            PersistId::new(options.entity.0 + offset),
        );
    }
    if let Some(remote) = options.stand_in_remote {
        spawn_stand_in_remote(&mut app, remote);
    }
    match app.run() {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_of(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn the_default_sidecar_opens_no_socket() {
        let options = parse_of(&[]).expect("no flags parses");
        assert!(
            options.serve.is_none(),
            "an unobserved sidecar must not bind a listener it was not asked for"
        );
        assert_eq!(options.seed, NODE_SEED);
        assert_eq!(options.entity, ENTITY);
        assert_eq!(
            options.entities, 1,
            "a sidecar carries one entity by default"
        );
        assert!(options.stand_in_remote.is_none());
    }

    #[test]
    fn every_flag_is_read() {
        let options = parse_of(&[
            "--serve",
            "127.0.0.1:0",
            "--seed",
            "11",
            "--entity",
            "898",
            "--entities",
            "24",
            "--stand-in-remote",
            "42",
        ])
        .expect("all five flags parse");
        assert_eq!(options.serve.as_deref(), Some("127.0.0.1:0"));
        assert_eq!(options.seed, 11);
        assert_eq!(options.entity, PersistId::new(898));
        assert_eq!(options.entities, 24);
        assert_eq!(options.stand_in_remote, Some(PersistId::new(42)));
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        assert!(parse_of(&["--serv", "127.0.0.1:0"]).is_err());
        assert!(parse_of(&["--serve"]).is_err());
        assert!(parse_of(&["--seed", "not-a-number"]).is_err());
        assert!(
            parse_of(&["--entities", "0"]).is_err(),
            "a population of zero would serve an empty presentation set silently"
        );
    }
}
