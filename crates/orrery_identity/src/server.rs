//! The running mint: an iroh endpoint answering [`IdentityMsg`] with
//! [`IdentityReply`] (#861, docs/09-services-and-ops.md §8).
//!
//! Everything below the service line already existed — [`IdentityService`]
//! answers the four mint questions, `SessionTokenVerifier` verifies what it
//! produces — and what the tree lacked was a *process*: "A login/refresh
//! endpoint over `IdentityService::issue`/`refresh`", with a redeem path that
//! reaches [`crate::redeem_invite`] in a deployment rather than only from
//! `tests/invites.rs`. This module is that process, generic over the same
//! three parameters the service is, so the default build links no `libfdb_c`
//! and `bin/orrery-identity.rs` wires the FoundationDB halves in.
//!
//! # The transport identity is the connection, never the body
//!
//! Not one request type carries a `NodeId` field. The iroh handshake gives
//! this process the remote's ed25519 public key — the same `NodeId` a
//! gateway's `owner(n)` resolves and a coordinator's verifier checks against
//! (D3) — and every mint is made for *that* key. A login body claims only an
//! account, so claiming someone else's account buys exactly what the `db`
//! binding index says it does: a refusal, `NotBound` or `NodeBoundElsewhere`.
//! A wire-supplied node field would have made the binding check decorative.
//!
//! # The bootstrap is exact-version, and it is the only one
//!
//! `IdentityMsg::Hello { version }` must be the first request on a
//! connection, checked for exact equality against
//! [`orrery_protocol::PROTOCOL_VERSION`] — the same rule
//! `GatewayMsg::VersionedHello` established and the same reason: postcard
//! keys variants positionally, so a peer that does not speak this message
//! set must be turned away at a named refusal instead of mis-decoding
//! whatever arrives first. A request sent before an accepted `Hello` is
//! answered `HelloRefused` and the connection closes; there is no
//! unversioned back door.
//!
//! # Requests are one bi-stream each
//!
//! The client opens a stream, writes one postcard request, finishes; the
//! service reads it to end (bounded by
//! [`orrery_protocol::MAX_IDENTITY_REQUEST_BYTES`], with a patience
//! deadline), answers one postcard reply, finishes. Streams are processed
//! one at a time per connection — identity's load is "near-flat vs. CCU"
//! (docs/09 §8), so no pipelining is earned, and serial processing is what
//! keeps bootstrap ordering trivially sound. Refresh rides new streams on
//! the same connection, which is exactly docs/09 §8's "clients refresh at
//! half-TTL over a reliable stream".

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iroh::Endpoint;
use orrery_protocol::{
    IdentityMsg, IdentityRefusal, IdentityReply, NodeId, SessionTokenVerifier, TokenClock,
    IDENTITY_ALPN, MAX_IDENTITY_REQUEST_BYTES, PROTOCOL_VERSION,
};
use tokio::sync::oneshot;

use crate::invite::{redeem_invite, InviteRedemptionError};
use crate::service::{IdentityService, IssuedSession, StandingSource};
use crate::store::{AccountStore, IdentityError};

/// How long the service waits for one request to finish arriving before it
/// gives up on the stream. A client that opens a stream and writes nothing
/// must not hold the accept loop forever.
const REQUEST_PATIENCE: Duration = Duration::from_secs(30);

/// Configuration for [`IdentityServer::spawn`].
pub struct IdentityServerConfig<S, T, C> {
    /// The assembled service whose `issue`/`refresh` answer the requests.
    pub service: Arc<IdentityService<S, T, C>>,
    /// The same clock the service mints with, used here to verify presented
    /// refresh tokens. Two clocks that disagree would make "still valid to
    /// mint" and "still valid to present" different questions.
    pub clock: C,
    /// The invite ledger backing the `RedeemInvite` surface. `None` refuses
    /// every redeem with [`IdentityRefusal::InviteLedger`]: a deployment that
    /// creates no accounts should say so rather than fail mysteriously.
    pub ledger: Option<PathBuf>,
    /// Local address to bind.
    pub bind: std::net::SocketAddr,
    /// The application protocol to advertise. Defaults to
    /// [`IDENTITY_ALPN`].
    pub alpn: Vec<u8>,
    /// The iroh relay mode. `RelayMode::Disabled` for loopback tests.
    pub relay_mode: iroh::RelayMode,
    /// The service's own transport secret. `None` binds an ephemeral
    /// identity, which is right for tests and wrong for a deployment whose
    /// clients dial a well-known NodeId (docs/09 §8).
    pub secret_key: Option<iroh::SecretKey>,
}

impl<S, T, C> IdentityServerConfig<S, T, C> {
    /// A loopback test configuration: ephemeral identity, disabled relay,
    /// the default ALPN.
    #[must_use]
    pub fn for_tests(service: Arc<IdentityService<S, T, C>>, clock: C) -> Self {
        Self {
            service,
            clock,
            ledger: None,
            bind: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            alpn: IDENTITY_ALPN.to_vec(),
            relay_mode: iroh::RelayMode::Disabled,
            secret_key: None,
        }
    }
}

/// Why an endpoint failed to come up.
#[derive(Debug)]
pub enum IdentityServerError {
    /// The bind address was rejected.
    BindAddr(String),
    /// The iroh endpoint could not be bound.
    Bind(iroh::endpoint::BindError),
}

impl core::fmt::Display for IdentityServerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bind(error) => write!(f, "bind identity endpoint: {error}"),
            Self::BindAddr(error) => write!(f, "identity bind address: {error}"),
        }
    }
}

impl core::error::Error for IdentityServerError {}

/// State one accept loop serves. Cheap to clone per task via `Arc`.
struct Shared<S, T, C> {
    service: Arc<IdentityService<S, T, C>>,
    clock: C,
    ledger: Option<PathBuf>,
}

/// A running identity service: an iroh endpoint accepting client sessions.
pub struct IdentityServer<S, T, C> {
    endpoint: Arc<Endpoint>,
    service: Arc<IdentityService<S, T, C>>,
    shutdown: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<()>,
}

impl<S, T, C> IdentityServer<S, T, C>
where
    S: AccountStore + 'static,
    T: StandingSource + 'static,
    C: TokenClock + Clone + Send + Sync + 'static,
{
    /// Bind an endpoint from `config` and start accepting clients.
    ///
    /// # Errors
    ///
    /// [`IdentityServerError`] when the endpoint cannot bind.
    pub async fn spawn(config: IdentityServerConfig<S, T, C>) -> Result<Self, IdentityServerError> {
        let mut builder = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0);
        builder = builder
            .bind_addr(config.bind)
            .map_err(|error| IdentityServerError::BindAddr(error.to_string()))?;
        builder = builder.alpns(vec![config.alpn.clone()]);
        builder = builder.relay_mode(config.relay_mode.clone());
        if let Some(key) = &config.secret_key {
            builder = builder.secret_key(key.clone());
        }
        let endpoint = Arc::new(builder.bind().await.map_err(IdentityServerError::Bind)?);

        let shared = Arc::new(Shared {
            service: Arc::clone(&config.service),
            clock: config.clock,
            ledger: config.ledger,
        });

        let (shutdown, rx) = oneshot::channel();
        let join = tokio::spawn(accept_loop(Arc::clone(&endpoint), shared, rx));
        Ok(Self {
            endpoint,
            service: config.service,
            shutdown,
            join,
        })
    }

    /// The service's node id — a client dials this.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.endpoint.id()
    }

    /// The service's full dial document.
    #[must_use]
    pub fn addr(&self) -> iroh::EndpointAddr {
        self.endpoint.addr()
    }

    /// The service this server answers with, for operators performing issuer
    /// rotations against the running process and for tests proving them.
    #[must_use]
    pub fn service(&self) -> &Arc<IdentityService<S, T, C>> {
        &self.service
    }

    /// Stop accepting, close the endpoint, and await the accept task.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        self.endpoint.close().await;
        let _ = self.join.await;
    }
}

async fn accept_loop<S, T, C>(
    endpoint: Arc<Endpoint>,
    shared: Arc<Shared<S, T, C>>,
    mut shutdown: oneshot::Receiver<()>,
) where
    S: AccountStore + 'static,
    T: StandingSource + 'static,
    C: TokenClock + Clone + Send + Sync + 'static,
{
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let shared = Arc::clone(&shared);
                tokio::spawn(handle_connection(incoming, shared));
            }
        }
    }
}

async fn handle_connection<S, T, C>(
    incoming: iroh::endpoint::Incoming,
    shared: Arc<Shared<S, T, C>>,
) where
    S: AccountStore + 'static,
    T: StandingSource + 'static,
    C: TokenClock + Clone + Send + Sync + 'static,
{
    let conn = match incoming.accept() {
        Ok(accepting) => match accepting.await {
            Ok(conn) => conn,
            Err(error) => {
                tracing::debug!(?error, "identity: handshake failed");
                return;
            }
        },
        Err(error) => {
            tracing::debug!(?error, "identity: accept failed");
            return;
        }
    };
    let remote = conn.remote_id();

    // One bi-stream per request, processed serially: see the module docs.
    let mut bootstrapped = false;
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let request = match tokio::time::timeout(
            REQUEST_PATIENCE,
            recv.read_to_end(MAX_IDENTITY_REQUEST_BYTES),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                tracing::debug!(%remote, ?error, "identity: request read failed");
                break;
            }
            Err(_) => {
                tracing::debug!(%remote, "identity: request exceeded its patience");
                break;
            }
        };
        let message: IdentityMsg = match postcard::from_bytes(&request) {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(%remote, ?error, "identity: undecodable request");
                break;
            }
        };

        let mut refused_bootstrap = false;
        let reply = match &message {
            IdentityMsg::Hello { version } => {
                if *version == PROTOCOL_VERSION {
                    bootstrapped = true;
                    IdentityReply::HelloAccepted
                } else {
                    refused_bootstrap = true;
                    IdentityReply::HelloRefused {
                        expected: PROTOCOL_VERSION,
                    }
                }
            }
            _ if !bootstrapped => {
                refused_bootstrap = true;
                IdentityReply::HelloRefused {
                    expected: PROTOCOL_VERSION,
                }
            }
            IdentityMsg::Login { account, ttl_ms } => mint(
                remote,
                shared.service.issue(*account, &remote, *ttl_ms).await,
            ),
            IdentityMsg::Refresh { token, ttl_ms } => {
                let verifier = SessionTokenVerifier::new(
                    shared.clock.clone(),
                    shared.service.published_issuer_keys(),
                );
                match verifier.verify(token, &remote) {
                    // A refresh is the same four questions on the same
                    // account and node the presented token names — D33 clause
                    // (e)'s "a quarantine takes effect no later than token
                    // refresh" lives in that re-run, not in the presented
                    // bytes.
                    Ok(claims) => mint(remote, shared.service.refresh(&claims, *ttl_ms).await),
                    Err(reason) => IdentityReply::Refused(IdentityRefusal::RefreshRejected(reason)),
                }
            }
            IdentityMsg::RedeemInvite { code, ttl_ms } => match shared.ledger.as_ref() {
                None => IdentityReply::Refused(IdentityRefusal::InviteLedger(
                    "this service holds no invite ledger".into(),
                )),
                Some(ledger) => {
                    let now = shared.clock.now_ms();
                    match redeem_invite(ledger, code, &remote, now, &shared.service, *ttl_ms).await
                    {
                        Ok(issued) => mint(remote, Ok(issued)),
                        Err(error) => IdentityReply::Refused(refuse_redemption(error)),
                    }
                }
            },
        };

        let Ok(encoded) = postcard::to_stdvec(&reply) else {
            tracing::error!(%remote, "identity: reply failed to encode");
            break;
        };
        if send.write_all(&encoded).await.is_err() || send.finish().is_err() {
            tracing::debug!(%remote, "identity: reply write failed");
            break;
        }
        if refused_bootstrap {
            tracing::info!(
                %remote,
                expected = PROTOCOL_VERSION,
                "identity: refused bootstrap; closing"
            );
            break;
        }
    }
}

/// Wrap an issued session for the wire.
fn mint(remote: NodeId, issued: Result<IssuedSession, IdentityError>) -> IdentityReply {
    match issued {
        Ok(session) => IdentityReply::Issued {
            token: session.encoded,
            refresh_at_ms: session.refresh_at_ms,
        },
        Err(error) => {
            // The refusal travels to the client by name; the same fact is
            // logged here, because a mint refusal is an admission decision
            // an operator may need to explain.
            tracing::info!(%remote, ?error, "identity: mint refused");
            IdentityReply::Refused(refuse(error))
        }
    }
}

/// Map a redemption failure onto the wire refusal set, propagating the
/// mint-time reasons untouched through `Identity`.
fn refuse_redemption(error: InviteRedemptionError) -> IdentityRefusal {
    match error {
        InviteRedemptionError::InvalidCode => IdentityRefusal::InvalidInviteCode,
        InviteRedemptionError::AlreadyConsumed => IdentityRefusal::InviteAlreadyConsumed,
        InviteRedemptionError::Ledger(error) => IdentityRefusal::InviteLedger(error.to_string()),
        InviteRedemptionError::Identity(error) => refuse(error),
    }
}

/// Map a mint-time failure onto the wire refusal set, one-for-one. The
/// service does not reinterpret here: a client refused `Cooldown` was
/// refused by `apply_dwell`, not by this mapping.
#[allow(clippy::too_many_lines)]
fn refuse(error: IdentityError) -> IdentityRefusal {
    match error {
        IdentityError::UnknownAccount(account) => IdentityRefusal::UnknownAccount(account),
        IdentityError::NotBound { node, account } => IdentityRefusal::NotBound { node, account },
        IdentityError::NodeBoundElsewhere { node, account } => {
            IdentityRefusal::NodeBoundElsewhere { node, account }
        }
        IdentityError::TooManyBoundNodes { account, cap } => {
            IdentityRefusal::TooManyBoundNodes { account, cap }
        }
        IdentityError::BindingRateLimited {
            account,
            window_ms,
            cap,
        } => IdentityRefusal::BindingRateLimited {
            account,
            window_ms,
            cap,
        },
        IdentityError::TtlAboveCap {
            requested_ms,
            cap_ms,
        } => IdentityRefusal::TtlAboveCap {
            requested_ms,
            cap_ms,
        },
        IdentityError::ZeroTtl => IdentityRefusal::ZeroTtl,
        IdentityError::StandingUnavailable(account) => {
            IdentityRefusal::StandingUnavailable(account)
        }
        IdentityError::Cooldown(account) => IdentityRefusal::Cooldown(account),
        IdentityError::Banned(account) => IdentityRefusal::Banned(account),
        IdentityError::AccountExists(account) => IdentityRefusal::AccountExists(account),
        IdentityError::Store(message) => IdentityRefusal::Store(message),
    }
}
