//! The #920 sidecar IPC measurement harness, one binary, two roles.
//!
//! ```text
//! # terminal 1 — the engine-side observer (binds an ephemeral port)
//! orrery-ipc-bench observer --entities 24 --ticks 36000 --report run24.json
//!
//! # terminal 2 — the sidecar (address taken from the observer's output)
//! orrery-ipc-bench sidecar --addr 127.0.0.1:PORT --entities 24
//! ```
//!
//! Both roles exit 0 only on a completed run. The observer writes the report
//! `scripts/ipc-report.py` reads; the sidecar's report is supporting
//! evidence. On Windows pass `--time-period` to raise the timer resolution;
//! the issue asks for the measurement with and without it.

use std::process::ExitCode;

use orrery_ipc_transport::bench::{run_observer, run_sidecar, ObserverConfig, SidecarConfig};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("observer") => observer(&args[1..]),
        Some("sidecar") => sidecar(&args[1..]),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

const USAGE: &str = "\
orrery-ipc-bench — the #920 sidecar IPC measurement

USAGE:
    orrery-ipc-bench observer [--bind 127.0.0.1] [--port 0]
                              [--entities 24] [--hz 60]
                              [--ticks 36000] [--warmup 600]
                              [--report PATH] [--time-period]
    orrery-ipc-bench sidecar  --addr HOST:PORT
                              [--entities 24] [--hz 60]
                              [--report PATH] [--time-period]

The observer prints the address it bound; pass it to the sidecar. The
observer's report is the artifact scripts/ipc-report.py reads.
";

struct Flags {
    map: std::collections::HashMap<String, String>,
    present: std::collections::HashSet<String>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut map = std::collections::HashMap::new();
        let mut present = std::collections::HashSet::new();
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            let Some(name) = flag.strip_prefix("--") else {
                return Err(format!("unexpected argument {flag}"));
            };
            if name == "time-period" {
                present.insert(name.to_owned());
                continue;
            }
            let Some(value) = iter.next() else {
                return Err(format!("--{name} needs a value"));
            };
            map.insert(name.to_owned(), value.clone());
        }
        Ok(Self { map, present })
    }

    fn value(&self, name: &str, default: &str) -> String {
        self.map
            .get(name)
            .cloned()
            .unwrap_or_else(|| default.to_owned())
    }

    fn flagged(&self, name: &str) -> bool {
        self.present.contains(name)
    }
}

fn parse_u64(flags: &Flags, name: &str, default: u64) -> u64 {
    flags
        .value(name, &default.to_string())
        .parse()
        .unwrap_or_else(|_| panic!("--{name} must be an integer"))
}

fn parse_u32(flags: &Flags, name: &str, default: u32) -> u32 {
    flags
        .value(name, &default.to_string())
        .parse()
        .unwrap_or_else(|_| panic!("--{name} must be an integer"))
}

fn observer(args: &[String]) -> ExitCode {
    let Ok(flags) = Flags::parse(args) else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let config = ObserverConfig {
        bind: flags.value("bind", "127.0.0.1"),
        port: u16::try_from(parse_u64(&flags, "port", 0)).unwrap_or(0),
        entities: parse_u32(&flags, "entities", 24),
        tick_hz: parse_u32(&flags, "hz", 60),
        ticks: parse_u64(&flags, "ticks", 36_000),
        warmup: parse_u64(&flags, "warmup", 600),
        time_begin_period: flags.flagged("time-period"),
    };
    match run_observer(&config) {
        Ok(report) => {
            let text = serde_json::to_string_pretty(&report).expect("report serializes");
            match flags.value("report", "") {
                path if path.is_empty() => println!("{text}"),
                path => {
                    if let Err(error) = std::fs::write(&path, text) {
                        eprintln!("orrery-ipc-bench: could not write {path}: {error}");
                        return ExitCode::FAILURE;
                    }
                    println!("orrery-ipc-bench: report written to {path}");
                }
            }
            print_summary(report.drops.tick_overruns, report.samples, &report.drops);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("orrery-ipc-bench: observer failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn sidecar(args: &[String]) -> ExitCode {
    let Ok(flags) = Flags::parse(args) else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let Some(addr) = flags.map.get("addr").cloned() else {
        eprintln!("orrery-ipc-bench: sidecar needs --addr HOST:PORT");
        return ExitCode::from(2);
    };
    let config = SidecarConfig {
        addr,
        entities: parse_u32(&flags, "entities", 24),
        tick_hz: parse_u32(&flags, "hz", 60),
        time_begin_period: flags.flagged("time-period"),
    };
    match run_sidecar(&config) {
        Ok(report) => {
            let text = serde_json::to_string_pretty(&report).expect("report serializes");
            match flags.value("report", "") {
                path if path.is_empty() => println!("{text}"),
                path => {
                    if let Err(error) = std::fs::write(&path, text) {
                        eprintln!("orrery-ipc-bench: could not write {path}: {error}");
                        return ExitCode::FAILURE;
                    }
                    println!("orrery-ipc-bench: report written to {path}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("orrery-ipc-bench: sidecar failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_summary(overruns: u64, samples: usize, drops: &orrery_ipc_transport::bench::DropsReport) {
    println!(
        "orrery-ipc-bench: run complete: {samples} samples, {} tick overruns, \
{} forward gaps, {} return gaps, {} frames discarded sidecar, {} overwritten observer, \
{} spawn missing, {} despawn missing",
        overruns,
        drops.forward_seq_gaps,
        drops.return_seq_gaps,
        drops.frame_discarded_sidecar,
        drops.frame_overwritten_observer,
        drops.spawn_missing,
        drops.despawn_missing,
    );
}
