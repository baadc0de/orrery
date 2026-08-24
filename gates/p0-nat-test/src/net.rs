//! iroh endpoint lifecycle: build the endpoint, expose the NodeId, and shut
//! down cleanly. This is the transport primitive the whole P0 spike rides.

use anyhow::{Context, Result};
use iroh::endpoint::presets::Minimal;
use iroh::{Endpoint, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};

/// A handle to the iroh endpoint, kept alive for the lifetime of the process.
#[derive(Clone)]
pub struct EndpointHandle {
    endpoint: Endpoint,
    relay: RelayUrl,
}

impl EndpointHandle {
    /// Build a new iroh endpoint configured to use `relay` as its home relay.
    /// If `secret_key` is given, the endpoint keeps that stable identity
    /// (NodeId) instead of generating a fresh one.
    pub async fn new(relay: String, secret_key: Option<String>) -> Result<Self> {
        // The relay map is the address book: our self-hosted relay doubles as
        // the punch rendezvous and the fallback path (docs/02-networking.md §8).
        let relay_url: RelayUrl = relay
            .parse()
            .with_context(|| format!("invalid relay URL: {relay}"))?;
        let relay_map = RelayMap::try_from_iter([relay.as_str()])
            .with_context(|| format!("invalid relay URL: {relay}"))?;

        let mut builder = Endpoint::builder(Minimal)
            .relay_mode(RelayMode::Custom(relay_map))
            .alpns(vec![b"p0-nat-test".to_vec()]);
        if let Some(sk) = secret_key {
            let sk: SecretKey = sk
                .parse()
                .with_context(|| "invalid --secret-key (expected hex)")?;
            builder = builder.secret_key(sk);
        }
        let endpoint = builder
            .bind()
            .await
            .context("failed to bind iroh endpoint")?;

        Ok(Self {
            endpoint,
            relay: relay_url,
        })
    }

    /// This node's stable network identity (ed25519 public key).
    pub fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The home relay URL, used as the dial hint (the design's `relay_hint`).
    pub fn relay(&self) -> &RelayUrl {
        &self.relay
    }

    /// The underlying endpoint, for dialing and opening connections.
    pub fn inner(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Shut the endpoint down, closing all connections.
    pub async fn shutdown(&self) -> Result<()> {
        self.endpoint.close().await;
        Ok(())
    }
}
