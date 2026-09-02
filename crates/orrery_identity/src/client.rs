//! A minimal raw-iroh identity client.
//!
//! The Bevy client will speak this surface through `orrery_net`; this exists
//! for the processes that are not games — tests, tools, and any deployment
//! step that needs a token without standing up an ECS. It is deliberately
//! the smallest thing that is still the real protocol, and it doubles as
//! executable documentation of the served mint shape: dial the service's
//! well-known NodeId, bootstrap with the exact protocol version, then one
//! request per bi-stream.

use std::time::Duration;

use orrery_protocol::{
    IdentityMsg, IdentityReply, NodeId, IDENTITY_ALPN, MAX_IDENTITY_REPLY_BYTES, PROTOCOL_VERSION,
};

/// What went wrong talking to an identity service.
#[derive(Debug)]
pub enum IdentityClientError {
    /// The endpoint could not be bound or dialled.
    Connect(String),
    /// The session was refused or a request failed on the wire.
    Session(String),
    /// The service accepted the connection but refused our protocol
    /// version; the field is the version it accepts, so a client can report
    /// the gap by name instead of guessing.
    Version(u16),
    /// The service did not answer within the caller's patience.
    Timeout(&'static str),
}

impl core::fmt::Display for IdentityClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Connect(error) => write!(f, "connect to identity service: {error}"),
            Self::Session(error) => write!(f, "identity session: {error}"),
            Self::Version(expected) => {
                write!(
                    f,
                    "identity service accepts protocol version {expected}; this build speaks {PROTOCOL_VERSION}"
                )
            }
            Self::Timeout(what) => write!(f, "identity service did not send {what} in time"),
        }
    }
}

impl core::error::Error for IdentityClientError {}

/// An authenticated identity-service session.
///
/// One connection, many requests: docs/09 §8's client refreshes at half-TTL
/// *over a reliable stream*, so the connection is held and each refresh is a
/// new bi-stream on it.
pub struct IdentityClient {
    /// Held for its lifetime: dropping the endpoint closes the connection.
    _endpoint: iroh::Endpoint,
    connection: iroh::endpoint::Connection,
    /// The dialling secret's public half — the transport identity every
    /// mint on this session is bound to.
    node: NodeId,
}

impl IdentityClient {
    /// Dial `address`, bootstrap with this build's protocol version, and
    /// hold the session.
    ///
    /// The dialling secret is the client's transport identity; every mint
    /// the service performs is bound to the public half of it.
    ///
    /// # Errors
    ///
    /// [`IdentityClientError::Version`] when the service refuses this
    /// build's [`PROTOCOL_VERSION`], with the accepted version as its
    /// payload.
    pub async fn connect(
        secret: iroh::SecretKey,
        address: iroh::EndpointAddr,
        within: Duration,
    ) -> Result<Self, IdentityClientError> {
        let node = secret.public();
        let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
            .alpns(vec![IDENTITY_ALPN.to_vec()])
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(secret)
            .bind()
            .await
            .map_err(|error| IdentityClientError::Connect(error.to_string()))?;
        let connection = endpoint
            .connect(address, IDENTITY_ALPN)
            .await
            .map_err(|error| IdentityClientError::Connect(error.to_string()))?;

        match Self::request(
            &connection,
            &IdentityMsg::Hello {
                version: PROTOCOL_VERSION,
            },
            within,
        )
        .await?
        {
            IdentityReply::HelloAccepted => Ok(Self {
                _endpoint: endpoint,
                connection,
                node,
            }),
            IdentityReply::HelloRefused { expected } => Err(IdentityClientError::Version(expected)),
            other => Err(IdentityClientError::Session(format!(
                "unexpected reply to Hello: {other:?}"
            ))),
        }
    }

    /// The client's transport identity — the node every mint will bind to.
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// Ask the service to mint a login token.
    ///
    /// # Errors
    ///
    /// Transport failures as [`IdentityClientError`]; the reply itself
    /// carries mint refusals as [`IdentityReply::Refused`].
    pub async fn login(
        &self,
        account: orrery_protocol::AccountId,
        ttl_ms: Option<u64>,
        within: Duration,
    ) -> Result<IdentityReply, IdentityClientError> {
        Self::request(
            &self.connection,
            &IdentityMsg::Login { account, ttl_ms },
            within,
        )
        .await
    }

    /// Ask the service to reissue from a token this service previously
    /// minted.
    ///
    /// # Errors
    ///
    /// As [`Self::login`].
    pub async fn refresh(
        &self,
        token: Vec<u8>,
        ttl_ms: Option<u64>,
        within: Duration,
    ) -> Result<IdentityReply, IdentityClientError> {
        Self::request(
            &self.connection,
            &IdentityMsg::Refresh { token, ttl_ms },
            within,
        )
        .await
    }

    /// Redeem an invite code: account creation, node binding, and a first
    /// mint, in one served request.
    ///
    /// # Errors
    ///
    /// As [`Self::login`].
    pub async fn redeem_invite(
        &self,
        code: String,
        ttl_ms: Option<u64>,
        within: Duration,
    ) -> Result<IdentityReply, IdentityClientError> {
        Self::request(
            &self.connection,
            &IdentityMsg::RedeemInvite { code, ttl_ms },
            within,
        )
        .await
    }

    /// One request on its own bi-stream: write, finish, read the reply to
    /// end, decode.
    async fn request(
        connection: &iroh::endpoint::Connection,
        message: &IdentityMsg,
        within: Duration,
    ) -> Result<IdentityReply, IdentityClientError> {
        let payload = postcard::to_stdvec(message)
            .map_err(|error| IdentityClientError::Session(error.to_string()))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| IdentityClientError::Session(error.to_string()))?;

        tokio::time::timeout(within, async move {
            send.write_all(&payload)
                .await
                .map_err(|error| IdentityClientError::Session(error.to_string()))?;
            send.finish()
                .map_err(|error| IdentityClientError::Session(error.to_string()))?;
            let bytes = recv
                .read_to_end(MAX_IDENTITY_REPLY_BYTES)
                .await
                .map_err(|error| IdentityClientError::Session(error.to_string()))?;
            postcard::from_bytes::<IdentityReply>(&bytes)
                .map_err(|error| IdentityClientError::Session(error.to_string()))
        })
        .await
        .map_err(|_| IdentityClientError::Timeout("reply"))?
    }
}
