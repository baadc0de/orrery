//! The `orrery-identity` service binary (#861, docs/09-services-and-ops.md
//! §8, docs/10-crates.md §13).
//!
//! Login, half-TTL refresh, and invite redemption over iroh, minted against
//! the durable account row and the standing ledger rather than typed into a
//! CLI. The mint path is [`ComputedStanding`] over the executor-owned `ya`
//! strike family read from the same FoundationDB cluster that holds the `id/`
//! account subspace (D31): D33 clause (f)'s fail-closed read is the deployed
//! posture here, not [`crate::UnavailableStanding`], and nothing on this
//! path hardcodes `Good`.
//!
//! Deployment shape mirrors `orrery-coordinator`: a single-line
//! machine-readable readiness contract on stdout, everything else on stderr
//! via tracing, so a harness can parse the address without scraping logs.
//!
//! The binary requires the `fdb` feature. A build without it has no durable
//! account store to mint against, and a mint path with no store is not a
//! weaker service, it is no service — so it refuses to run rather than
//! falling back to an in-memory stand-in that would silently forget every
//! account and binding across restarts.

#[cfg(not(feature = "fdb"))]
fn main() {
    eprintln!(
        "orrery-identity requires the `fdb` feature: \
         cargo build -p orrery_identity --features fdb"
    );
    std::process::exit(70);
}

#[cfg(feature = "fdb")]
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    let ledger = cli.ledger.clone();
    let secret_key = cli
        .secret_key
        .as_deref()
        .map(|hex| Ok::<_, anyhow::Error>(iroh::SecretKey::from_bytes(&decode_key(hex)?)))
        .transpose()?;
    let thresholds = thresholds(&cli)?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        // D33 clause (f): the deployed standing source scores the
        // executor-owned `ya` family at read time through the durable
        // dwell policy. An unreadable ledger refuses the mint; it is never
        // read as `Good`.
        let store = std::sync::Arc::new(FdbAccountStore::connect(&cli.cluster_file)?);
        let strike_rows = FdbStrikeRowSource::connect(&cli.cluster_file)?;
        let scorer = ComputedStanding::new(strike_rows, system_now_ms, thresholds)
            .map_err(|error| anyhow::anyhow!("standing thresholds: {error}"))?;
        let standing = CooldownStanding::new(std::sync::Arc::clone(&store), scorer);

        let key = load_runtime_credential(&cli.issuer_credential)?;
        let service = IdentityService::new(
            std::sync::Arc::clone(&store),
            standing,
            SystemClock,
            IssuerKeyring::new(key),
        )
        .with_standing_thresholds(thresholds)?;

        let server = IdentityServer::spawn(IdentityServerConfig {
            service: std::sync::Arc::new(service),
            clock: SystemClock,
            ledger,
            bind: cli.bind,
            alpn: IDENTITY_ALPN.to_vec(),
            relay_mode: iroh::RelayMode::Disabled,
            secret_key,
        })
        .await?;

        {
            // The readiness line names the identities a deployment needs:
            // who to dial, and whose signature the gateways must trust.
            use std::io::Write;
            let json = serde_json::json!({
                "node_id": server.id().to_string(),
                "bind_addr": server
                    .addr()
                    .ip_addrs()
                    .next()
                    .map(|addr| addr.to_string()),
                "active_issuer_key_id": server.service().active_issuer_key_id().0,
            });
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            let _ = writeln!(handle, "{json}");
            let _ = handle.flush();
        }
        tracing::info!(identity = %server.id(), "identity service up");

        // D33 clause (e)'s filing-driven half. Spawned after readiness so a
        // sweep can never delay the line a harness parses, and held only to
        // be aborted at shutdown: a sweep that outlives the server would keep
        // writing `dc` rows for a process that has stopped serving.
        let filing = spawn_filing_reactor(&cli, &store, thresholds)?;

        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C, shutting down"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
        }
        for task in filing {
            task.abort();
        }
        server.shutdown().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// D33 clause (d)'s deployment dials. Every value falls back to the default
/// package; the set actually used is validated as a whole, because a
/// deployment may set different thresholds but may not set incoherent ones.
#[cfg(feature = "fdb")]
fn thresholds(cli: &Cli) -> anyhow::Result<StandingThresholds> {
    let mut thresholds = DEFAULT_STANDING_THRESHOLDS;
    if let Some(value) = cli.quarantine_milli {
        thresholds.quarantine_milli = value;
    }
    if let Some(value) = cli.cooldown_milli {
        thresholds.cooldown_milli = value;
    }
    if let Some(value) = cli.ban_milli {
        thresholds.ban_milli = value;
    }
    if let Some(value) = cli.intended_major_findings {
        thresholds.intended_major_findings = value;
    }
    if let Some(value) = cli.cooldown_min_ms {
        thresholds.cooldown_min_ms = value;
    }
    if let Some(value) = cli.probation_ms {
        thresholds.probation_ms = value;
    }
    thresholds
        .validate()
        .map_err(|error| anyhow::anyhow!("standing thresholds: {error}"))?;
    Ok(thresholds)
}

/// Start D33 clause (e)'s filing-driven evaluator and its C5 posture poller.
///
/// Returns the spawned tasks so `main` can abort them at shutdown. Two tasks
/// rather than one because the posture poller must keep refreshing the cell
/// even while a sweep is in flight — that is what makes D32 clause (f)'s
/// auto-suspend able to demote this control mid-sweep rather than after it.
///
/// The startup posture is `off`, so this changes nothing for a deployment that
/// does not ask for it: the queue is not even read. A `ramp/strikes` row, or
/// `--strikes`, is what arms it — a runtime dial an operator can turn on a
/// shipped binary, which is what a compile-time feature could never be.
#[cfg(feature = "fdb")]
fn spawn_filing_reactor(
    cli: &Cli,
    store: &std::sync::Arc<FdbAccountStore>,
    thresholds: StandingThresholds,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    use orrery_persistd::gateway::{
        spawn_strikes_posture_poller, StrikesEnforcement, StrikesPosture,
    };

    let startup: StrikesEnforcement = cli
        .strikes
        .parse()
        .map_err(|error: String| anyhow::anyhow!("--strikes: {error}"))?;
    let posture = StrikesPosture::new(startup);

    let context = orrery_persistd::FdbContext::connect(&cli.cluster_file)
        .map_err(|error| anyhow::anyhow!("--cluster-file {}: {error}", cli.cluster_file))?;
    let reader = std::sync::Arc::new(orrery_persistd::intent::FdbRampPostureStore::from_context(
        &context,
    )) as orrery_persistd::intent::SharedRampPostureReader;
    let poller = spawn_strikes_posture_poller(reader, posture.clone(), startup);

    let scorer = ComputedStanding::new(
        FdbStrikeRowSource::from_database(context.database()),
        system_now_ms,
        thresholds,
    )
    .map_err(|error| anyhow::anyhow!("standing thresholds: {error}"))?;
    let reactor = StandingFilingReactor::new(
        std::sync::Arc::clone(store),
        FdbFilingNoticeQueue::from_database(context.database()),
        scorer,
        posture,
    );

    let period = std::time::Duration::from_millis(cli.filing_sweep_ms.max(1));
    let sweeper = tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            match reactor.sweep().await {
                Ok(sweep) if sweep.seen > 0 => tracing::info!(
                    mode = ?sweep.mode,
                    seen = sweep.seen,
                    published = sweep.published,
                    would_publish = sweep.would_publish,
                    cleared = sweep.cleared,
                    failed = sweep.failed,
                    "swept the standing filing queue"
                ),
                Ok(_) => {}
                // The queue read itself failed. Log and try again next tick:
                // an identity replica that exits on a transient FoundationDB
                // error stops minting too, which is a far larger outage than
                // one late invalidation.
                Err(error) => tracing::warn!(
                    %error,
                    "could not read the standing filing queue; retrying next sweep"
                ),
            }
        }
    });
    Ok(vec![poller, sweeper])
}

/// The wall clock the scorer evaluates decay at.
#[cfg(feature = "fdb")]
fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Decode a 64-character hex key into its 32 bytes.
#[cfg(feature = "fdb")]
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
        .collect::<Result<_, _>>()?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("hex key must be 64 characters"))
}

#[cfg(feature = "fdb")]
use clap::Parser as _;
#[cfg(feature = "fdb")]
use orrery_identity::fdb::{FdbAccountStore, FdbFilingNoticeQueue, FdbStrikeRowSource};
#[cfg(feature = "fdb")]
use orrery_identity::filing::StandingFilingReactor;
#[cfg(feature = "fdb")]
use orrery_identity::server::{IdentityServer, IdentityServerConfig};
#[cfg(feature = "fdb")]
use orrery_identity::{
    load_runtime_credential, ComputedStanding, CooldownStanding, IdentityService, IssuerKeyring,
    StandingThresholds, SystemClock, DEFAULT_STANDING_THRESHOLDS,
};
#[cfg(feature = "fdb")]
use orrery_protocol::IDENTITY_ALPN;

/// The identity service's command line. Every flag falls back to an
/// environment variable, per #865.
#[cfg(feature = "fdb")]
#[derive(Debug, clap::Parser)]
#[command(
    name = "orrery-identity",
    about = "Orrery identity: login, half-TTL refresh and invite redemption over iroh"
)]
struct Cli {
    /// FoundationDB cluster file for the `id/` account subspace (D31).
    #[arg(long, env = "ORRERY_IDENTITY_CLUSTER_FILE")]
    cluster_file: String,

    /// Plain runtime issuer credential from `orrery-issuer-key generate`/
    /// `load`; must be owner-readable only and outside every repository.
    #[arg(long, env = "ORRERY_IDENTITY_ISSUER_CREDENTIAL")]
    issuer_credential: std::path::PathBuf,

    /// Local tab-separated invite ledger backing the redeem surface;
    /// created if missing. Without it the service refuses every redeem by
    /// name rather than failing mysteriously.
    #[arg(long, env = "ORRERY_IDENTITY_LEDGER")]
    ledger: Option<std::path::PathBuf>,

    /// Local address to bind. Port `0` asks the OS for an ephemeral port.
    #[arg(long, default_value = "127.0.0.1:0", env = "ORRERY_IDENTITY_BIND")]
    bind: std::net::SocketAddr,

    /// Hex-encoded iroh secret key, pinning the service's NodeId across
    /// runs. A fresh identity is generated per boot when absent, which no
    /// dialling client can follow (docs/09 §8 pins service NodeIds).
    #[arg(long, value_name = "HEX")]
    secret_key: Option<String>,

    /// `Q`: `Good` becomes `Quarantined` at this score, in milli-points.
    #[arg(long, env = "ORRERY_IDENTITY_QUARANTINE_MILLI")]
    quarantine_milli: Option<i64>,

    /// `C`: `Quarantined` becomes `Cooldown` at this score, in milli-points.
    #[arg(long, env = "ORRERY_IDENTITY_COOLDOWN_MILLI")]
    cooldown_milli: Option<i64>,

    /// `B`: `Cooldown` becomes `Banned` at this score, in milli-points.
    #[arg(long, env = "ORRERY_IDENTITY_BAN_MILLI")]
    ban_milli: Option<i64>,

    /// The number of major findings by which ban should be reachable (D33
    /// clause (d) invariant (iii)).
    #[arg(long, env = "ORRERY_IDENTITY_INTENDED_MAJOR_FINDINGS")]
    intended_major_findings: Option<u32>,

    /// Minimum time an account remains in cooldown, in milliseconds.
    #[arg(long, env = "ORRERY_IDENTITY_COOLDOWN_MIN_MS")]
    cooldown_min_ms: Option<u64>,

    /// How long a fresh account remains on probation, in milliseconds.
    #[arg(long, env = "ORRERY_IDENTITY_PROBATION_MS")]
    probation_ms: Option<u64>,

    /// D32 control C5: this service's posture for D33 clause (e)'s
    /// *filing-driven* standing evaluation.
    ///
    /// `off` — the executor's `yd` filing notices are not read at all, which
    /// is the shipped default and the behaviour every existing deployment
    /// keeps. Notices accumulate, so promoting the control later still acts
    /// on everything filed while it was off.
    /// `shadow` — every notice is evaluated exactly as `live` would and the
    /// would-be invalidation is recorded on `orrery::ramp::shadow`, but no
    /// `dc` row is written and no notice is drained.
    /// `live` — an account found at or above `C` gets its durable `dc` entry,
    /// which is what `orrery_identity::invalidation` publishes to gateways
    /// and coordinators.
    ///
    /// This governs only the filing-driven sweep. The mint path's own refusal
    /// is D33 clause (f) and is not a ramp control: a token is never stamped
    /// `Good` against an unread ledger, at any posture.
    ///
    /// A durable `ramp/strikes` row overrides this within one poll interval;
    /// this flag is the startup default the poller falls back to when no row
    /// exists. Like every other posture selector in this tree it takes no
    /// environment fallback — an inherited variable would arm every identity
    /// replica a supervisor spawns.
    #[arg(long, value_name = "off|shadow|live", default_value = "off")]
    strikes: String,

    /// How often the filing-driven sweep drains the `yd` queue, in
    /// milliseconds.
    ///
    /// A cadence, not a posture, so it takes an environment fallback: an
    /// inherited value cannot arm a control, only change how often an already
    /// armed one looks.
    #[arg(
        long,
        default_value_t = 15_000,
        env = "ORRERY_IDENTITY_FILING_SWEEP_MS"
    )]
    filing_sweep_ms: u64,
}
