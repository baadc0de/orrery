//! Minting and refreshing session tokens.
//!
//! `docs/09-services-and-ops.md` §8 gives the token contract in one line —
//! `{account, node_id, issued_at, ttl: 1 h, standing}`, "Clients refresh at
//! half-TTL over a reliable stream" — and the claims type that carries it
//! already exists. So does the verifier. What follows is the issuing side of
//! the same contract, and it is written so that the two halves agree without
//! either being adjusted to the other: [`IdentityService::issue`] produces
//! bytes that `orrery_protocol::SessionTokenVerifier::verify`
//! accepts, and every refusal below is a refusal the verifier would also have
//! made, moved to the instant of issuance.
//!
//! # The four questions a mint asks, in order
//!
//! ```text
//!   1. is the requested TTL inside MAX_SESSION_TOKEN_TTL_MS?   -> TtlAboveCap
//!   2. does `da ‖ account` exist?                              -> UnknownAccount
//!   3. is this node bound to that account, right now?          -> NotBound
//!   4. what standing does the ledger hold for it?              -> StandingUnavailable
//! ```
//!
//! Question 3 is the one worth arguing for. A token binds `(account, node)` and
//! the verifier checks the node against the connected peer, so a token minted
//! for a node the account does not hold would be a durable statement that the
//! `d` rows contradict — and the gateway's `owner(n)` would resolve it to a
//! *different* account than the token claims. Refusing at issuance keeps the
//! signed claim and the reverse index as one fact rather than two.
//!
//! Question 4 fails closed, per D33 clause (f). The party able to make the
//! standing lookup unavailable would otherwise select the branch that admits a
//! ban, which is D31 clause (f)'s attacker-controlled-unknown problem with a
//! worse outcome.

use crate::issuer::IssuerKeyring;
use crate::store::{AccountStore, IdentityError};
use orrery_protocol::AccountId;
use orrery_protocol::{
    NodeId, SessionStanding, SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1, TokenClock,
    UnixMillis, MAX_SESSION_TOKEN_TTL_MS,
};
use std::collections::HashMap;
use std::sync::RwLock;

/// The lifetime this service issues when a caller states no preference: the
/// full hour `docs/09` §8 specifies and
/// [`MAX_SESSION_TOKEN_TTL_MS`] caps.
pub const DEFAULT_SESSION_TOKEN_TTL_MS: u64 = MAX_SESSION_TOKEN_TTL_MS;

/// Where an account's enforcement standing comes from.
///
/// D33 (proposed) puts standing behind the `ya` strike ledger, whose sole
/// writer is the adjudication executor and whose scorer is this service:
/// `S(t) = Σ wᵢ · 2^(−ageᵢ / 14 d)`, evaluated at read time, with
/// quarantine/cooldown/ban at configured boundaries. The read-only
/// implementation is [`crate::ComputedStanding`]; this trait remains the seam
/// so issuance does not acquire a FoundationDB dependency in its hot logic.
///
/// `Err(IdentityError::StandingUnavailable)` is the honest answer to "the
/// ledger could not be read", and the service refuses to mint on it. Returning
/// `Ok(Good)` instead would be the one thing D33's Alternatives rejects by
/// name.
#[async_trait::async_trait]
pub trait StandingSource: Send + Sync {
    /// The standing to stamp into a token for `account`.
    ///
    /// # Errors
    ///
    /// [`IdentityError::StandingUnavailable`] when no answer can be
    /// established. It is never softened into `Good`.
    async fn standing(&self, account: AccountId) -> Result<SessionStanding, IdentityError>;
}

/// The default standing source: there is no ledger, so nothing resolves.
///
/// Read with D33 clause (f) this is the strictest configuration rather than an
/// inert one — every mint and every refresh is refused — and it is the honest
/// default in the same way `orrery_persistd::gateway::UnboundBindingAuthority`
/// is for `owner(n)`. A deployment substitutes a real source; a test uses
/// [`StaticStanding`].
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableStanding;

#[async_trait::async_trait]
impl StandingSource for UnavailableStanding {
    async fn standing(&self, account: AccountId) -> Result<SessionStanding, IdentityError> {
        Err(IdentityError::StandingUnavailable(account))
    }
}

/// A fixed table of standings, for tests and for harnesses that have no ledger
/// but do have a policy.
///
/// The counterpart of `orrery_persistd::gateway::SnapshotBindingAuthority`, and
/// the same caveat applies: it is a table somebody typed, so it proves what the
/// service does with a standing and never how one was computed.
#[derive(Debug, Clone)]
pub struct StaticStanding {
    standings: HashMap<AccountId, SessionStanding>,
    default: Option<SessionStanding>,
}

impl StaticStanding {
    /// A table with an explicit fallback for accounts it does not name.
    ///
    /// `None` makes an unnamed account unresolvable, which is the fail-closed
    /// arm; `Some(Good)` is the permissive dev posture and is spelled out at
    /// the call site rather than being the default, because "unnamed means
    /// Good" is precisely the assumption D33 clause (f) refuses.
    #[must_use]
    pub fn new(
        standings: impl IntoIterator<Item = (AccountId, SessionStanding)>,
        default: Option<SessionStanding>,
    ) -> Self {
        Self {
            standings: standings.into_iter().collect(),
            default,
        }
    }

    /// Every account resolves to `Good`. Development and tests only.
    #[must_use]
    pub fn all_good() -> Self {
        Self::new([], Some(SessionStanding::Good))
    }
}

#[async_trait::async_trait]
impl StandingSource for StaticStanding {
    async fn standing(&self, account: AccountId) -> Result<SessionStanding, IdentityError> {
        self.standings
            .get(&account)
            .copied()
            .or(self.default)
            .ok_or(IdentityError::StandingUnavailable(account))
    }
}

/// The host's wall clock, as [`TokenClock`].
///
/// `orrery_protocol` ships only `FixedTokenClock`, because a verifier in a test
/// wants a clock it controls. An issuer in production wants the real one.
/// Before the Unix epoch — an unset RTC, essentially — this reports 0, which
/// makes every token it then signs fail the verifier's `Future` check rather
/// than silently minting from a wrong instant.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl TokenClock for SystemClock {
    fn now_ms(&self) -> UnixMillis {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        UnixMillis::new(millis)
    }
}

/// One minted session, and when its holder should come back for another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedSession {
    /// The signed envelope.
    pub token: SessionTokenV1,
    /// The postcard bytes a client presents and a verifier decodes.
    pub encoded: Vec<u8>,
    /// The instant at which the holder should refresh: `issued_at + ttl / 2`.
    ///
    /// `docs/09` §8's "clients refresh at half-TTL". Carried on the reply
    /// rather than left to the client to compute, so a client that gets the
    /// arithmetic wrong is a client that ignored an instruction rather than one
    /// that was never given it.
    pub refresh_at_ms: UnixMillis,
}

impl IssuedSession {
    /// The signed claims.
    #[must_use]
    pub fn claims(&self) -> &SessionTokenClaimsV1 {
        &self.token.claims
    }
}

/// D12's identity service: an account store, a standing source, and a keyring.
///
/// Generic over all three so the default build links no `libfdb_c` and a test
/// can drive a fixed clock. The keyring sits behind an `RwLock` because a
/// rotation is an operator action against a running service — signing takes the
/// read side, and only [`Self::rotate`] and [`Self::retire_issuer_key`] take
/// the write side.
pub struct IdentityService<S, T, C> {
    store: S,
    standing: T,
    clock: C,
    keyring: RwLock<IssuerKeyring>,
}

impl<S, T, C> IdentityService<S, T, C>
where
    S: AccountStore,
    T: StandingSource,
    C: TokenClock,
{
    /// Assemble a service.
    pub fn new(store: S, standing: T, clock: C, keyring: IssuerKeyring) -> Self {
        Self {
            store,
            standing,
            clock,
            keyring: RwLock::new(keyring),
        }
    }

    /// The account store, for binding operations and durable reads.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The issuer keys a verifier should currently be configured with.
    ///
    /// During a rotation window this is both the outgoing and the incoming key
    /// — the dual-accept set of `docs/09` §8.
    pub fn published_issuer_keys(&self) -> Vec<orrery_protocol::IssuerKey> {
        self.with_keyring(IssuerKeyring::published_keys)
    }

    /// The identifier stamped into tokens minted from now on.
    pub fn active_issuer_key_id(&self) -> orrery_protocol::IssuerKeyId {
        self.with_keyring(IssuerKeyring::active_key_id)
    }

    /// Add a signing key and make it active, keeping every key already held.
    ///
    /// The old key stays *published*, so tokens signed by it keep verifying for
    /// the rest of their lifetime. Closing the window is
    /// [`Self::retire_issuer_key`], and it is a separate call on purpose.
    ///
    /// # Errors
    ///
    /// [`crate::RotationError::DuplicateKeyId`] if the identifier is already
    /// held.
    pub fn rotate(&self, key: crate::IssuerSigningKey) -> Result<(), crate::RotationError> {
        self.with_keyring_mut(|keyring| keyring.add_and_activate(key))
    }

    /// Stop publishing a key, ending its dual-accept window.
    ///
    /// # Errors
    ///
    /// [`crate::RotationError::RetiringActiveKey`] for the active key, and
    /// [`crate::RotationError::UnknownKeyId`] for one that is not held.
    pub fn retire_issuer_key(
        &self,
        key_id: orrery_protocol::IssuerKeyId,
    ) -> Result<(), crate::RotationError> {
        self.with_keyring_mut(|keyring| keyring.retire(key_id))
    }

    /// Mint a token for `(account, node)`.
    ///
    /// `requested_ttl_ms` is `None` for [`DEFAULT_SESSION_TOKEN_TTL_MS`].
    ///
    /// # Errors
    ///
    /// The four questions in this module's documentation, in that order, plus
    /// [`IdentityError::Store`] for a durable failure.
    pub async fn issue(
        &self,
        account: AccountId,
        node: &NodeId,
        requested_ttl_ms: Option<u64>,
    ) -> Result<IssuedSession, IdentityError> {
        let ttl_ms = self.checked_ttl(requested_ttl_ms)?;

        // 2. The account must exist. D33 clause (f)'s first absence: no `da`
        //    row is an authentication failure, never an implicit account.
        if self.store.account(account).await?.is_none() {
            return Err(IdentityError::UnknownAccount(account));
        }

        // 3. And the node must be bound to it *right now*. Current bindings
        //    only (D31 clause (g)); a node released a moment ago is a miss and
        //    a miss does not mint.
        match self.store.binding(node).await? {
            Some(row) if row.account == account => {}
            Some(row) => {
                return Err(IdentityError::NodeBoundElsewhere {
                    node: *node,
                    account: row.account,
                })
            }
            None => {
                return Err(IdentityError::NotBound {
                    node: *node,
                    account,
                })
            }
        }

        // 4. Standing is read, not computed, and an unreadable ledger refuses.
        let standing = self.standing.standing(account).await?;

        self.mint(account, node, ttl_ms, standing)
    }

    /// Reissue for an established `(account, node)` — the half-TTL refresh of
    /// `docs/09` §8.
    ///
    /// Deliberately the same four checks as [`Self::issue`] rather than a
    /// cheaper path keyed off the previous token. A refresh is where a
    /// quarantine takes effect (D33 clause (e): "a quarantine takes effect no
    /// later than token refresh") and where an unbinding stops producing
    /// tokens, so trusting the old token's claims would make refresh the one
    /// operation that cannot enforce anything.
    ///
    /// # Errors
    ///
    /// As [`Self::issue`].
    pub async fn refresh(
        &self,
        previous: &SessionTokenClaimsV1,
        requested_ttl_ms: Option<u64>,
    ) -> Result<IssuedSession, IdentityError> {
        self.issue(previous.account, &previous.node, requested_ttl_ms)
            .await
    }

    /// The TTL cap, enforced at issuance rather than only at verification.
    fn checked_ttl(&self, requested_ttl_ms: Option<u64>) -> Result<u64, IdentityError> {
        let ttl_ms = requested_ttl_ms.unwrap_or(DEFAULT_SESSION_TOKEN_TTL_MS);
        if ttl_ms == 0 {
            return Err(IdentityError::ZeroTtl);
        }
        if ttl_ms > MAX_SESSION_TOKEN_TTL_MS {
            return Err(IdentityError::TtlAboveCap {
                requested_ms: ttl_ms,
                cap_ms: MAX_SESSION_TOKEN_TTL_MS,
            });
        }
        Ok(ttl_ms)
    }

    /// Stamp and sign, once every question above has been answered.
    fn mint(
        &self,
        account: AccountId,
        node: &NodeId,
        ttl_ms: u64,
        standing: SessionStanding,
    ) -> Result<IssuedSession, IdentityError> {
        let issued_at_ms = self.clock.now_ms();
        let keyring = self.read_keyring();
        let claims = SessionTokenClaimsV1::new(
            account,
            *node,
            issued_at_ms,
            SessionTokenTtlMs::new(ttl_ms),
            standing,
            keyring.active_key_id(),
        );
        let token = keyring
            .sign(claims)
            .map_err(|error| IdentityError::Store(error.to_string()))?;
        let encoded = token.encode()?;
        drop(keyring);
        Ok(IssuedSession {
            token,
            encoded,
            // Integer division, so an odd TTL refreshes a millisecond early
            // rather than a millisecond late. The direction is the point.
            refresh_at_ms: UnixMillis::new(issued_at_ms.0.saturating_add(ttl_ms / 2)),
        })
    }

    fn read_keyring(&self) -> std::sync::RwLockReadGuard<'_, IssuerKeyring> {
        self.keyring
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn with_keyring<R>(&self, f: impl FnOnce(&IssuerKeyring) -> R) -> R {
        f(&self.read_keyring())
    }

    fn with_keyring_mut<R>(&self, f: impl FnOnce(&mut IssuerKeyring) -> R) -> R {
        let mut keyring = self
            .keyring
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut keyring)
    }
}
