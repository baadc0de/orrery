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
use orrery_coordinator::server::{ServerConfig, SystemPresenceClock, SystemUnixClock};
use orrery_coordinator::{CoordinatorServer, InterestIssuer};
use orrery_protocol::{GridId, IssuerKey, IssuerKeyId, NodeId};

#[derive(Debug, Parser)]
#[command(
    name = "orrery-coordinator",
    about = "Orrery coordinator: presence in, island manifests and interest grants out"
)]
struct Cli {
    /// Local address to bind. Port `0` asks the OS for an ephemeral port.
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,

    /// Trusted identity issuer key in `<key-id>@<public-key>` form.
    ///
    /// Presence decides island membership *and* what a peer may claim, so an
    /// unauthenticated peer reporting presence would be granting itself
    /// authority by another route. At least one key is required.
    #[arg(long, value_name = "KEY_ID@PUBLIC_KEY")]
    issuer_key: Vec<IssuerKeySpec>,

    /// Hex-encoded ed25519 secret used to sign interest grants.
    ///
    /// Gateways must be configured with the matching public half via
    /// `persistd --coordinator-key`. Without `--secret-key` this is also the
    /// only stable identity the coordinator has, so pin it in production.
    #[arg(long, value_name = "HEX")]
    interest_secret: String,

    /// Key id stamped into issued grants, for rotation.
    #[arg(long, default_value_t = 1)]
    interest_key_id: u32,

    /// Hex-encoded iroh secret key, pinning the coordinator's NodeId across
    /// runs. A fresh identity is generated per boot when absent.
    #[arg(long, value_name = "HEX")]
    secret_key: Option<String>,

    /// The grid whose cell space this coordinator serves (P-7: cell ids are
    /// grid-relative).
    #[arg(long, default_value_t = 0)]
    grid: u32,
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

    let config = ServerConfig {
        bind: cli.bind.parse::<SocketAddr>()?,
        secret_key,
        grid: GridId::new(cli.grid),
        token_clock: Arc::new(SystemUnixClock),
        presence_clock: Arc::new(SystemPresenceClock::default()),
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
