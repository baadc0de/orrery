//! Command-line interface for the P0 NAT test tool.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use iroh::PublicKey;

/// The one checked-in relay host default. `gates/p0-nat-lab/deploy-gw.sh` reads this
/// same file before resolving and pinning the host in each peer namespace.
const DEFAULT_RELAY_HOST: &str = include_str!("../relay-host");

/// Build the relay URL from its host. The NAT lab legitimately needs the host
/// separately: it resolves it on the gateway and pins the resulting IP because
/// the peer network namespaces cannot rely on DNS.
fn relay_url(host: &str) -> String {
    format!("https://{host}")
}

/// The relay URL used unless `--relay` is supplied. Set `ORRERY_RELAY_HOST` to
/// override the shared host default for both this CLI and the NAT-lab deployer.
fn default_relay() -> String {
    let host =
        std::env::var("ORRERY_RELAY_HOST").unwrap_or_else(|_| DEFAULT_RELAY_HOST.trim().to_owned());
    relay_url(&host)
}

/// Default test window: the P0 demo criterion runs 30 minutes; the default
/// here is a quick smoke test that still exercises punch + a few seconds of
/// 60 Hz datagrams.
const DEFAULT_DURATION_SECS: u64 = 30;

/// P0 NAT test — Orrery transport spike.
///
/// Exercises iroh 1.0.x QUIC with NAT hole punching and relay fallback
/// (docs/02-networking.md §1, docs/11-roadmap.md §P0). Run `host` on one
/// machine, pass its NodeId to friends as `--peer`, and compare the path and
/// datagram telemetry each side prints.
#[derive(Debug, Parser)]
#[command(name = "p0-nat-test", version, about)]
pub struct Cli {
    /// The iroh relay URL used as the punch rendezvous and fallback path.
    #[arg(long, global = true, default_value_t = default_relay(), env = "ORRERY_RELAY")]
    pub relay: String,

    /// The remote peer's NodeId to dial. Omit to act as the host (rendezvous).
    #[arg(long, global = true, env = "ORRERY_PEER")]
    pub peer: Option<PublicKey>,

    /// Host mode: accept this many simultaneous connections (local mesh test).
    /// Defaults to 1; only meaningful when `--peer` is absent.
    #[arg(long, global = true, default_value_t = 1, env = "ORRERY_P0_NAT_PEERS")]
    pub peers: u32,

    /// State datagram send rate, in ticks per second (P0 stress = 60).
    #[arg(long, global = true, default_value_t = 60, env = "ORRERY_TICK_HZ")]
    pub tick_hz: u32,

    /// Payload size of each state datagram, in bytes.
    #[arg(
        long,
        global = true,
        default_value_t = 64,
        env = "ORRERY_PAYLOAD_BYTES"
    )]
    pub payload_bytes: usize,

    /// Roundtrip ping rate, in pings per second (for P50/P95 latency).
    #[arg(long, global = true, default_value_t = 1, env = "ORRERY_PING_HZ")]
    pub ping_hz: u32,

    /// Total test window, in seconds.
    #[arg(long, global = true, default_value_t = DEFAULT_DURATION_SECS, env = "ORRERY_P0_NAT_DURATION_SECS")]
    pub duration_secs: u64,

    /// Print the NodeId and exit without dialing or sending (host helper).
    #[arg(long, global = true)]
    pub print_id: bool,

    /// Full-mesh mode: path to a roster file (one NodeId per line, this node
    /// included). Dials every node after us in the list, accepts every node
    /// before us, so each pair connects exactly once. Combine with
    /// `--mesh-index` to identify this node's position.
    #[arg(long, global = true, env = "ORRERY_MESH")]
    pub mesh: Option<PathBuf>,

    /// This node's index in the `--mesh` roster (0-based). Required with
    /// `--mesh` if the roster cannot be matched to the local NodeId.
    #[arg(long, global = true, env = "ORRERY_MESH_INDEX")]
    pub mesh_index: Option<usize>,

    /// A stable secret key (hex) so this node keeps the same NodeId across
    /// runs. Without it, every invocation generates a fresh NodeId, which
    /// breaks `--mesh` rosters (NodeIds are ephemeral).
    #[arg(long, global = true)]
    pub secret_key: Option<String>,

    /// Emit telemetry as one JSON object per line on stdout (machine-parseable
    /// for the punch-rate dashboard). Tracing logs go to stderr.
    #[arg(long, global = true, env = "ORRERY_JSON")]
    pub json: bool,
}

impl Cli {
    /// The test window as a `Duration`.
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.duration_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn cli_env_fallback_and_flag_precedence() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        const NAME: &str = "ORRERY_MESH_INDEX";

        let _lock = ENV_LOCK.lock().expect("environment lock");
        let previous = std::env::var_os(NAME);
        std::env::set_var(NAME, "7");
        let from_env = Cli::try_parse_from(["p0-nat-test"]);
        let from_flag = Cli::try_parse_from(["p0-nat-test", "--mesh-index", "3"]);
        match previous {
            Some(value) => std::env::set_var(NAME, value),
            None => std::env::remove_var(NAME),
        }

        assert_eq!(
            from_env.expect("environment fallback parses").mesh_index,
            Some(7)
        );
        assert_eq!(
            from_flag.expect("explicit flag parses").mesh_index,
            Some(3),
            "an explicit flag must beat the environment fallback"
        );
    }

    #[test]
    fn nat_lab_and_cli_derive_the_same_default_relay() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../p0-nat-lab/deploy-gw.sh");
        let output = Command::new("bash")
            .arg(script)
            .arg("--print-relay-host")
            .output()
            .expect("run NAT-lab relay-host query");
        assert!(
            output.status.success(),
            "NAT-lab relay-host query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let nat_lab_host = String::from_utf8(output.stdout)
            .expect("NAT-lab relay host is UTF-8")
            .trim()
            .to_owned();

        let cli = Cli::try_parse_from(["p0-nat-test"]).expect("CLI accepts its default relay");
        // Spell the expected URL out rather than calling `relay_url` again. An
        // assertion that re-derives through the function under test puts the
        // mutation on both sides of the equality, where it cancels: changing
        // the scheme to `http` would leave both halves equal and the test
        // green. The relay is reached over HTTPS, so the scheme is part of what
        // this guards, not incidental formatting.
        assert_eq!(cli.relay, format!("https://{nat_lab_host}"));
    }
}
