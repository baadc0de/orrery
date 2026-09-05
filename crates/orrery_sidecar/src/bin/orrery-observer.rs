//! The reference observer: a headless renderer with the renderer taken out.
//!
//! It dials one or two serving sidecars, applies their frames to an
//! [`ObserverView`](orrery_ipc_transport::observer::ObserverView) each, and
//! prints the presentation set. Everything an engine binding does *except*
//! draw, which is exactly the part `-NullRHI` would not have drawn either.
//!
//! # Why a Rust observer exists beside the Unreal one
//!
//! Two reasons, and neither is convenience:
//!
//! 1. **A9 P-4 needs a process to kill, in a test that runs on every commit.**
//!    `tests/observer_kill.rs` spawns this binary, `SIGKILL`s it mid-run, and
//!    checks the sidecar's rules-produced state is untouched. An Unreal
//!    editor cannot be a `cargo test` dependency; this can.
//! 2. **It is the control for the engine binding.** When the Unreal observer
//!    disagrees with the sidecar, the question is always whether the C++ or
//!    the crossing is wrong. Running this against the same sidecar answers it
//!    in one command.
//!
//! ```text
//! orrery-observer --addr ADDR [--addr ADDR] [--frames N] [--quiet] [--print-every N]
//! ```
//!
//! With `--frames N` it exits successfully after applying `N` frames batches
//! across all links; without it, it runs until the sidecar closes the stream
//! or the process is killed.

use std::process::ExitCode;

use orrery_ipc_transport::observer::{ObserverLink, Polled, Timeline};

/// How often the presentation set is printed, in frames batches.
const DEFAULT_PRINT_EVERY: u64 = 60;

struct Options {
    addrs: Vec<String>,
    frames: Option<u64>,
    print_every: u64,
    quiet: bool,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        addrs: Vec::new(),
        frames: None,
        print_every: DEFAULT_PRINT_EVERY,
        quiet: false,
    };
    let mut args = args;
    while let Some(flag) = args.next() {
        if flag == "--quiet" {
            options.quiet = true;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--addr" => options.addrs.push(value),
            "--frames" => {
                options.frames = Some(
                    value
                        .parse()
                        .map_err(|_| format!("bad --frames: {value}"))?,
                );
            }
            "--print-every" => {
                options.print_every = value
                    .parse()
                    .map_err(|_| format!("bad --print-every: {value}"))?;
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if options.addrs.is_empty() {
        return Err("at least one --addr is required".to_owned());
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(problem) => {
            eprintln!("orrery-observer: {problem}");
            eprintln!(
                "usage: orrery-observer --addr ADDR [--addr ADDR] [--frames N] [--print-every N] [--quiet]"
            );
            return ExitCode::from(2);
        }
    };

    let mut links = Vec::new();
    for addr in &options.addrs {
        match ObserverLink::connect(addr.as_str()) {
            Ok(link) => links.push(link),
            Err(problem) => {
                eprintln!("orrery-observer: cannot dial {addr}: {problem}");
                return ExitCode::from(1);
            }
        }
    }
    println!("orrery-observer: watching {} sidecar(s)", links.len());

    // Round-robin rather than one thread per link: `poll` blocks until a
    // complete message arrives, and a renderer's frame is bounded by its
    // slowest source anyway. Two links at 60 Hz is not a scheduling problem.
    let mut applied = 0_u64;
    loop {
        for (index, link) in links.iter_mut().enumerate() {
            match link.poll() {
                Ok(Polled::Applied) => {}
                Ok(Polled::Closed) => {
                    println!("orrery-observer: sidecar {index} closed the stream");
                    return ExitCode::SUCCESS;
                }
                Err(problem) => {
                    eprintln!("orrery-observer: sidecar {index} link failed: {problem}");
                    return ExitCode::from(1);
                }
            }
        }
        applied += 1;
        if !options.quiet && options.print_every > 0 && applied.is_multiple_of(options.print_every)
        {
            report(&links);
        }
        if options.frames.is_some_and(|limit| applied >= limit) {
            if !options.quiet {
                report(&links);
            }
            return ExitCode::SUCCESS;
        }
    }
}

/// One line per presented entity: the capsule an engine would move.
fn report<R: std::io::Read>(links: &[ObserverLink<R>]) {
    for (index, link) in links.iter().enumerate() {
        let view = link.view();
        for (id, entity) in view.entities() {
            let class = match entity.timeline {
                Timeline::Predicted => "predicted",
                Timeline::Interpolated => "interpolated",
            };
            println!(
                "observer: sidecar={index} tick={} id={} class={class} x={} basis={}..{}@{}",
                entity.presented_at.0,
                id.0,
                entity.transform.translation.x,
                entity.basis.from.0,
                entity.basis.to.0,
                entity.basis.alpha.0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_of(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn two_sidecars_are_two_addresses() {
        let options = parse_of(&[
            "--addr",
            "127.0.0.1:1",
            "--addr",
            "127.0.0.1:2",
            "--frames",
            "12",
            "--quiet",
        ])
        .expect("parses");
        assert_eq!(options.addrs, vec!["127.0.0.1:1", "127.0.0.1:2"]);
        assert_eq!(options.frames, Some(12));
        assert!(options.quiet);
    }

    #[test]
    fn an_observer_with_nothing_to_watch_is_an_error() {
        assert!(parse_of(&["--quiet"]).is_err());
        assert!(parse_of(&["--addr"]).is_err());
    }
}
