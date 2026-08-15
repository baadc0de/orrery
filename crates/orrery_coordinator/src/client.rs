//! A minimal raw-iroh coordinator client.
//!
//! The Bevy client lives in `orrery_net`; this exists for the processes that
//! are not games — the authority harness, integration tests, and any tool that
//! needs a peer's interest grant without standing up an ECS. It is deliberately
//! the smallest thing that is still the real protocol, so it doubles as
//! executable documentation of the session shape.

use std::time::Duration;

use bytes::Bytes;
use orrery_protocol::channels::{decode_stream_frame, encode_stream_frame, untag, Channel};
use orrery_protocol::{CellId, CoordMsg, IslandManifest, NodeId, COORD_ALPN};

/// What went wrong talking to a coordinator.
#[derive(Debug)]
pub enum ClientError {
    /// The endpoint could not be bound or dialled.
    Connect(String),
    /// The coordinator refused admission or the session tore down.
    Session(String),
    /// The coordinator did not answer within the caller's patience.
    Timeout(&'static str),
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Connect(error) => write!(f, "connect to coordinator: {error}"),
            Self::Session(error) => write!(f, "coordinator session: {error}"),
            Self::Timeout(what) => write!(f, "coordinator did not send {what} in time"),
        }
    }
}

impl core::error::Error for ClientError {}

/// An authenticated coordinator session.
pub struct CoordinatorClient {
    /// Held for its lifetime: dropping the endpoint closes the connection.
    _endpoint: iroh::Endpoint,
    connection: iroh::endpoint::Connection,
    coordinator: NodeId,
}

impl CoordinatorClient {
    /// Dial `address`, complete admission, and authenticate with `token`.
    pub async fn connect(
        secret: iroh::SecretKey,
        address: iroh::EndpointAddr,
        token: Vec<u8>,
        within: Duration,
    ) -> Result<Self, ClientError> {
        let node = secret.public();
        let coordinator = address.id;
        let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
            .alpns(vec![COORD_ALPN.to_vec()])
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(secret)
            .bind()
            .await
            .map_err(|error| ClientError::Connect(error.to_string()))?;
        let connection = endpoint
            .connect(address, COORD_ALPN)
            .await
            .map_err(|error| ClientError::Connect(error.to_string()))?;

        // The coordinator streams `[ACCEPTED]` before any datagrams flow;
        // reading it first is what keeps the hello from racing admission.
        let mut admission = connection
            .accept_uni()
            .await
            .map_err(|error| ClientError::Session(error.to_string()))?;
        let accepted = admission
            .read_to_end(16)
            .await
            .map_err(|error| ClientError::Session(error.to_string()))?;
        if accepted != vec![0u8] {
            return Err(ClientError::Session("admission refused".into()));
        }

        let client = Self {
            _endpoint: endpoint,
            connection,
            coordinator,
        };
        client.send(&CoordMsg::Hello { token, node })?;
        match client.recv(within).await {
            Some(CoordMsg::Welcome { coordinator, .. }) => {
                if coordinator != client.coordinator {
                    return Err(ClientError::Session(
                        "welcome named a different coordinator".into(),
                    ));
                }
                Ok(client)
            }
            Some(other) => Err(ClientError::Session(format!(
                "expected a welcome, got {other:?}"
            ))),
            None => Err(ClientError::Timeout("a welcome")),
        }
    }

    /// Report the cells this peer's interest covers.
    pub fn report_presence(&self, cells: Vec<CellId>) -> Result<(), ClientError> {
        self.send(&CoordMsg::Presence { cells })
    }

    /// Wait for the next interest grant, discarding manifests meanwhile.
    ///
    /// Returns the opaque signed bytes to forward to a gateway. Callers that
    /// need both grants and manifests should drive [`Self::recv`] instead.
    pub async fn next_grant(&self, within: Duration) -> Result<Vec<u8>, ClientError> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.recv(remaining).await {
                Some(CoordMsg::InterestGrant { grant }) => return Ok(grant),
                Some(_) => continue,
                None => return Err(ClientError::Timeout("an interest grant")),
            }
        }
    }

    /// Wait for the next island manifest, discarding grants meanwhile.
    pub async fn next_manifest(&self, within: Duration) -> Result<IslandManifest, ClientError> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.recv(remaining).await {
                Some(CoordMsg::IslandAssignment { manifest }) => return Ok(manifest),
                Some(_) => continue,
                None => return Err(ClientError::Timeout("an island manifest")),
            }
        }
    }

    /// Read the next coordinator message, or `None` on timeout or teardown.
    pub async fn recv(&self, within: Duration) -> Option<CoordMsg> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let packet =
                match tokio::time::timeout(remaining, self.connection.read_datagram()).await {
                    Ok(Ok(packet)) => packet,
                    Ok(Err(_)) | Err(_) => return None,
                };
            let Some((Channel::Control, _)) = untag(&packet) else {
                continue;
            };
            if let Some(message) = decode_stream_frame(&packet) {
                return Some(message);
            }
        }
    }

    /// Leave gracefully, so the coordinator learns immediately rather than
    /// waiting out an idle timeout.
    ///
    /// Dropping the client instead is the *crash* shape: QUIC cannot tell a
    /// departed peer from a silent one until its own timeout expires.
    pub async fn leave(self) {
        self.connection.close(0u32.into(), b"leaving");
        self._endpoint.close().await;
    }

    /// The underlying connection, for tests that need to send raw frames.
    #[must_use]
    pub fn connection(&self) -> &iroh::endpoint::Connection {
        &self.connection
    }

    fn send(&self, message: &CoordMsg) -> Result<(), ClientError> {
        self.connection
            .send_datagram(Bytes::from(encode_stream_frame(message)))
            .map_err(|error| ClientError::Session(error.to_string()))
    }
}
