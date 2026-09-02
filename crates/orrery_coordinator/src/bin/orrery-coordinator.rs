//! The `orrery-coordinator` service binary (docs/10-crates.md §12).
//!
//! Peers connect here, authenticate with an identity session token, and report
//! what their interest covers. In return they get island manifests and signed
//! interest grants — the grants being what a gateway requires before it will
//! believe a weak claim or offer a peer a lost lease (D7 §5).
//!
//! Deployment shape mirrors `persistd`: a single-line machine-readable
//! readiness contract on stdout, everything else on stderr via tracing, so a
//! harness can parse the address without scraping logs.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use clap::Parser;
use orrery_coordinator::server::{
    ServerConfig, StrikesMode, StrikesPosture, SystemPresenceClock, SystemUnixClock,
};
use orrery_coordinator::{CoordinatorServer, InterestIssuer, WitnessEpochIssuer};
use orrery_protocol::{GridId, IssuerKey, IssuerKeyId, NodeId};

#[cfg(feature = "fdb-state")]
struct FdbStrikesPostureReader(orrery_persistd::intent::FdbRampPostureStore);

#[cfg(feature = "fdb-state")]
#[async_trait::async_trait]
impl orrery_coordinator::StrikesPostureReader for FdbStrikesPostureReader {
    async fn read_strikes(&self) -> Result<Option<StrikesMode>, String> {
        self.0
            .read(orrery_persistd::gateway::STRIKES_CONTROL)
            .await
            .map(|row| {
                row.map(|row| match row.mode {
                    orrery_persistd::intent::RampMode::Off => StrikesMode::Off,
                    orrery_persistd::intent::RampMode::Shadow => StrikesMode::Shadow,
                    orrery_persistd::intent::RampMode::Live => StrikesMode::Live,
                })
            })
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "orrery-coordinator",
    about = "Orrery coordinator: presence in, island manifests and interest grants out"
)]
struct Cli {
    /// Local address to bind. Port `0` asks the OS for an ephemeral port.
    #[arg(long, default_value = "127.0.0.1:0", env = "ORRERY_COORDINATOR_BIND")]
    bind: String,

    /// Trusted identity issuer key in `<key-id>@<public-key>` form.
    ///
    /// Presence decides island membership *and* what a peer may claim, so an
    /// unauthenticated peer reporting presence would be granting itself
    /// authority by another route. At least one key is required.
    #[arg(long, value_name = "KEY_ID@PUBLIC_KEY", env = "ORRERY_ISSUER_KEY")]
    issuer_key: Vec<IssuerKeySpec>,

    /// Hex-encoded ed25519 secret used to sign interest grants.
    ///
    /// Gateways must be configured with the matching public half via
    /// `persistd --coordinator-key`. Without `--secret-key` this is also the
    /// only stable identity the coordinator has, so pin it in production.
    #[arg(long, value_name = "HEX")]
    interest_secret: String,

    /// Key id stamped into issued grants, for rotation.
    #[arg(long, default_value_t = 1, env = "ORRERY_INTEREST_KEY_ID")]
    interest_key_id: u32,

    /// Hex-encoded iroh secret key, pinning the coordinator's NodeId across
    /// runs. A fresh identity is generated per boot when absent.
    #[arg(long, value_name = "HEX")]
    secret_key: Option<String>,

    /// The grid whose cell space this coordinator serves (P-7: cell ids are
    /// grid-relative).
    #[arg(long, default_value_t = 0, env = "ORRERY_GRID")]
    grid: u32,

    /// Hex-encoded 32-byte master secret for witness-epoch seed keys (D28).
    ///
    /// Supplying it is what turns witness-set seeding on: without it the
    /// coordinator announces nothing and `orrery_witness` keeps its
    /// self-chosen fallback, which is only safe while nothing is filed
    /// against a report. Announcements are signed with `--interest-secret`
    /// under `--interest-key-id`, so a gateway that already trusts this
    /// coordinator's grants needs no second key.
    ///
    /// **Provision it, do not generate it per boot.** Every epoch key is
    /// derived from this secret, so a coordinator that loses it can no longer
    /// reveal — and therefore no longer usefully reseed — any cell it had
    /// already announced.
    #[arg(long, value_name = "HEX")]
    witness_master_secret: Option<String>,

    /// The leader-lease generation stamped into the high bits of every epoch
    /// handle this process mints (D28 clause (b)).
    ///
    /// It must increase across failovers: two coordinators sharing an
    /// incarnation can mint colliding handles, and a handle is what an intent
    /// names when it says which witness set it was collected under.
    #[arg(long, default_value_t = 1, env = "ORRERY_WITNESS_INCARNATION")]
    witness_incarnation: u64,

    /// D32 control C5: strike enforcement posture for this coordinator.
    ///
    /// The default is `off`, preserving the absence of strike actions until
    /// an operator deliberately selects shadow or live.
    // C5 is a posture mode selector, so it takes no environment fallback —
    // the rule #869 applied to C1 and C4 and #868 to persistd's own C5. An
    // inherited variable would arm every coordinator a supervisor spawns.
    #[arg(long, value_name = "off|shadow|live", default_value = "off")]
    strikes: StrikesMode,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    anyhow::ensure!(
        !cli.issuer_key.is_empty(),
        "coordinator requires at least one --issuer-key <key-id>@<public-key>"
    );

    let interest_secret = iroh::SecretKey::from_bytes(&decode_key(&cli.interest_secret)?);
    let issuer = InterestIssuer::new(
        interest_secret.clone(),
        IssuerKeyId::new(cli.interest_key_id),
    );
    let secret_key = cli
        .secret_key
        .as_deref()
        .map(|hex| Ok::<_, anyhow::Error>(iroh::SecretKey::from_bytes(&decode_key(hex)?)))
        .transpose()?;

    let witness_issuer = cli
        .witness_master_secret
        .as_deref()
        .map(|hex| {
            Ok::<_, anyhow::Error>(WitnessEpochIssuer::new(
                interest_secret.clone(),
                IssuerKeyId::new(cli.interest_key_id),
                decode_key(hex)?,
                cli.witness_incarnation,
            ))
        })
        .transpose()?;

    #[cfg(feature = "fdb-state")]
    let strikes_posture_reader = {
        let context = orrery_persistd::FdbContext::connect_default()?;
        Some(Arc::new(FdbStrikesPostureReader(
            orrery_persistd::intent::FdbRampPostureStore::from_context(&context),
        )) as orrery_coordinator::SharedStrikesPostureReader)
    };

    let config = ServerConfig {
        bind: cli.bind.parse::<SocketAddr>()?,
        secret_key,
        witness_issuer,
        grid: GridId::new(cli.grid),
        token_clock: Arc::new(SystemUnixClock),
        presence_clock: Arc::new(SystemPresenceClock::default()),
        strikes_posture: strikes_posture(&cli),
        #[cfg(feature = "fdb-state")]
        strikes_posture_reader,
        ..ServerConfig::new(cli.issuer_key.iter().map(|key| key.0.clone()), issuer)
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let server = CoordinatorServer::spawn(config).await?;

        {
            // The readiness line names both identities a deployment needs: who
            // to dial, and whose signature gateways must trust.
            use std::io::Write;
            let json = serde_json::json!({
                "node_id": server.id().to_string(),
                "bind_addr": server
                    .addr()
                    .ip_addrs()
                    .next()
                    .map(|addr| addr.to_string()),
                "interest_public_key": interest_secret.public().to_string(),
                "interest_key_id": cli.interest_key_id,
                "grid": cli.grid,
                // Whether this coordinator seeds witness sets is a deployment
                // fact a harness must be able to read without guessing: with
                // it off, attestations have no announced set to be checked
                // against and the peer-side fallback is what is running.
                "witness_seeding": cli.witness_master_secret.is_some(),
                "witness_incarnation": cli.witness_incarnation,
            });
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            let _ = writeln!(handle, "{json}");
            let _ = handle.flush();
        }
        tracing::info!(
            coordinator = %server.id(),
            interest_key = %interest_secret.public(),
            "coordinator up"
        );

        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C, shutting down"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
        }
        let stats = server.stats().await;
        tracing::info!(
            presence_reports = stats.presence_reports,
            grants_issued = stats.grants_issued,
            manifests_pushed = stats.manifests_pushed,
            "coordinator shutting down"
        );
        server.shutdown().await;
        Ok::<_, anyhow::Error>(())
    })
}

#[derive(Debug, Clone)]
struct IssuerKeySpec(IssuerKey);

impl FromStr for IssuerKeySpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (key_id, public_key) = value
            .split_once('@')
            .ok_or_else(|| "expected issuer key as <key-id>@<public-key>".to_string())?;
        let key_id = key_id
            .parse::<u32>()
            .map_err(|error| format!("invalid issuer key id `{key_id}`: {error}"))?;
        let public_key = public_key
            .parse::<NodeId>()
            .map_err(|error| format!("invalid issuer public key `{public_key}`: {error}"))?;
        Ok(Self(IssuerKey::new(IssuerKeyId::new(key_id), public_key)))
    }
}

fn decode_key(value: &str) -> anyhow::Result<[u8; 32]> {
    let bytes: Vec<u8> = (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(
                value
                    .get(index..index + 2)
                    .ok_or_else(|| anyhow::anyhow!("odd-length hex key"))?,
                16,
            )
            .map_err(|error| anyhow::anyhow!("invalid hex key: {error}"))
        })
        .collect::<anyhow::Result<_>>()?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("secret keys are 32 bytes"))
}

/// C5's startup default reaches the one posture cell every coordinator
/// standing consumer shares.
fn strikes_posture(cli: &Cli) -> StrikesPosture {
    StrikesPosture::new(cli.strikes)
}

#[cfg(test)]
mod tests {
    use super::*;

    static RAMP_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn strikes_flag_selects_all_three_modes_at_the_coordinator_posture() {
        let _lock = RAMP_ENV_LOCK.lock().expect("ramp environment lock");
        let defaulted =
            Cli::try_parse_from(["orrery-coordinator", "--interest-secret", &"00".repeat(32)])
                .expect("bare C5 invocation parses");
        assert_eq!(defaulted.strikes, StrikesMode::Off);

        for (spelling, expected) in [
            ("off", StrikesMode::Off),
            ("shadow", StrikesMode::Shadow),
            ("live", StrikesMode::Live),
        ] {
            let parsed = Cli::try_parse_from([
                "orrery-coordinator",
                "--interest-secret",
                &"00".repeat(32),
                "--strikes",
                spelling,
            ])
            .expect("C5 posture parses");
            let config = ServerConfig {
                strikes_posture: strikes_posture(&parsed),
                ..ServerConfig::new(
                    std::iter::empty(),
                    InterestIssuer::new(iroh::SecretKey::from_bytes(&[1; 32]), IssuerKeyId::new(1)),
                )
            };
            assert_eq!(config.strikes_posture.get(), expected);
        }
    }

    #[test]
    fn strikes_mode_does_not_read_the_environment() {
        const NAME: &str = "ORRERY_STRIKES";
        let _lock = RAMP_ENV_LOCK.lock().expect("ramp environment lock");
        let previous = std::env::var_os(NAME);
        std::env::set_var(NAME, "shadow");
        let from_env =
            Cli::try_parse_from(["orrery-coordinator", "--interest-secret", &"00".repeat(32)]);
        let from_flag = Cli::try_parse_from([
            "orrery-coordinator",
            "--interest-secret",
            &"00".repeat(32),
            "--strikes",
            "live",
        ]);
        match previous {
            Some(value) => std::env::set_var(NAME, value),
            None => std::env::remove_var(NAME),
        }

        assert_eq!(
            from_env.expect("bare invocation parses").strikes,
            StrikesMode::Off,
            "ORRERY_STRIKES must not move C5 off its D32 default"
        );
        assert_eq!(
            from_flag.expect("explicit flag parses").strikes,
            StrikesMode::Live
        );
    }
}
