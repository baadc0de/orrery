//! The issuer's signing keys, and the dual-accept rotation window.
//!
//! `docs/09-services-and-ops.md` §8 states key hygiene in one line: "rotate =
//! publish new well-known NodeId, dual-accept for one client-release cycle".
//! [`orrery_protocol::IssuerKeyId`] and the verifier's multi-key set
//! already exist to support exactly that; what was missing is the side that
//! holds more than one *secret* and chooses which one signs.
//!
//! A rotation is therefore three separate moments, and the type keeps them
//! separate because collapsing them is what breaks a fleet:
//!
//! ```text
//!   1. add            both keys published, old key active   verifiers accept both
//!   2. activate       both keys published, new key active   verifiers accept both
//!   3. retire(old)    only the new key published            old tokens stop verifying
//! ```
//!
//! Step 3 is what makes the window a window. Retiring at the same instant as
//! activating would strand every unexpired token signed by the old key — up to
//! [`orrery_protocol::MAX_SESSION_TOKEN_TTL_MS`] of them — with
//! `UnknownIssuer`, which reads to an operator like a forged token and is not
//! one. [`IssuerKeyring::published_keys`] is the set a verifier should be
//! configured with at each moment, and it is derived from the keyring rather
//! than maintained beside it, so the two halves of a rotation cannot drift.

use orrery_protocol::{IssuerKey, IssuerKeyId, NodeId, SessionTokenClaimsV1, SessionTokenV1};
use std::fmt;

/// One issuer signing key: a rotation identifier and the secret behind it.
///
/// Debug-prints the identifier and the *public* key only. A secret key that
/// prints itself ends up in a log the first time somebody debugs a rotation.
#[derive(Clone)]
pub struct IssuerSigningKey {
    key_id: IssuerKeyId,
    secret: iroh_base::SecretKey,
}

impl IssuerSigningKey {
    /// Wrap a secret key under the rotation identifier it signs as.
    #[must_use]
    pub fn new(key_id: IssuerKeyId, secret: iroh_base::SecretKey) -> Self {
        Self { key_id, secret }
    }

    /// This key's rotation identifier.
    #[must_use]
    pub fn key_id(&self) -> IssuerKeyId {
        self.key_id
    }

    /// The public half, which is what a verifier is configured with.
    #[must_use]
    pub fn public_key(&self) -> NodeId {
        self.secret.public()
    }

    /// The verifier entry for this key.
    #[must_use]
    pub fn issuer_key(&self) -> IssuerKey {
        IssuerKey::new(self.key_id, self.public_key())
    }

    /// Return the portable secret bytes for the lifecycle module.
    ///
    /// This stays crate-private: callers should only persist it through the
    /// encrypted escrow and restrictive runtime-credential paths.
    pub(crate) fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }
}

impl fmt::Debug for IssuerSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuerSigningKey")
            .field("key_id", &self.key_id.0)
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// A failure while changing the keyring's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RotationError {
    /// A key with this identifier is already held.
    DuplicateKeyId(IssuerKeyId),
    /// No key with this identifier is held.
    UnknownKeyId(IssuerKeyId),
    /// The active key cannot be retired.
    ///
    /// Retiring it would leave the service unable to sign anything, and the
    /// failure would surface at the next login rather than here. Activate the
    /// successor first; that is step 2 of the window, and it is the whole
    /// reason the window has three steps rather than two.
    RetiringActiveKey(IssuerKeyId),
}

impl fmt::Display for RotationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKeyId(id) => write!(f, "issuer key {} is already held", id.0),
            Self::UnknownKeyId(id) => write!(f, "issuer key {} is not held", id.0),
            Self::RetiringActiveKey(id) => {
                write!(f, "issuer key {} is active and cannot be retired", id.0)
            }
        }
    }
}

impl core::error::Error for RotationError {}

/// A failure while signing claims.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SigningError {
    /// The keyring's active identifier names no held key.
    NoActiveKey(IssuerKeyId),
    /// The claims could not be encoded for signing.
    Encode(postcard::Error),
}

impl fmt::Display for SigningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActiveKey(id) => write!(f, "no held issuer key with id {}", id.0),
            Self::Encode(error) => write!(f, "encode session-token claims: {error}"),
        }
    }
}

impl core::error::Error for SigningError {}

/// The signing keys this issuer holds, one of which is active.
///
/// Held keys are *published*: [`Self::published_keys`] is what every verifier
/// in the fleet should be configured with. Only the active key signs.
#[derive(Debug, Clone)]
pub struct IssuerKeyring {
    keys: Vec<IssuerSigningKey>,
    active: IssuerKeyId,
}

impl IssuerKeyring {
    /// Create a keyring holding one key, which is active.
    #[must_use]
    pub fn new(key: IssuerSigningKey) -> Self {
        let active = key.key_id();
        Self {
            keys: vec![key],
            active,
        }
    }

    /// The identifier stamped into every token this keyring signs.
    #[must_use]
    pub fn active_key_id(&self) -> IssuerKeyId {
        self.active
    }

    /// Every held key's verifier entry, in the order the keys were added.
    ///
    /// This is the dual-accept set: during a rotation window it contains both
    /// the outgoing and the incoming key, and a verifier configured with it
    /// accepts tokens signed by either.
    #[must_use]
    pub fn published_keys(&self) -> Vec<IssuerKey> {
        self.keys.iter().map(IssuerSigningKey::issuer_key).collect()
    }

    /// Whether this keyring holds `key_id`.
    #[must_use]
    pub fn holds(&self, key_id: IssuerKeyId) -> bool {
        self.keys.iter().any(|key| key.key_id() == key_id)
    }

    /// Step 1: hold an additional key without signing with it yet.
    ///
    /// The new key is published immediately, so every verifier can be brought
    /// up to the wider set before any token is signed under it. Reversing this
    /// order — signing first, publishing after — is a self-inflicted outage
    /// with `UnknownIssuer` on every new session.
    pub fn add(&mut self, key: IssuerSigningKey) -> Result<(), RotationError> {
        if self.holds(key.key_id()) {
            return Err(RotationError::DuplicateKeyId(key.key_id()));
        }
        self.keys.push(key);
        Ok(())
    }

    /// Step 2: sign with `key_id` from now on. It must already be held.
    pub fn activate(&mut self, key_id: IssuerKeyId) -> Result<(), RotationError> {
        if !self.holds(key_id) {
            return Err(RotationError::UnknownKeyId(key_id));
        }
        self.active = key_id;
        Ok(())
    }

    /// Steps 1 and 2 together: add `key` and make it active.
    ///
    /// Correct only when the fleet's verifiers already carry the new key —
    /// which is what [`Self::add`] followed by a rollout is for. It exists
    /// because a first deployment and a test have no fleet to lag.
    pub fn add_and_activate(&mut self, key: IssuerSigningKey) -> Result<(), RotationError> {
        let key_id = key.key_id();
        self.add(key)?;
        self.activate(key_id)
    }

    /// Step 3: stop publishing `key_id`, closing the dual-accept window.
    ///
    /// After this, tokens signed by that key are rejected with
    /// `SessionTokenVerificationError::UnknownIssuer` by any verifier
    /// reconfigured from [`Self::published_keys`]. Do it no sooner than one
    /// token lifetime after the activation that replaced it.
    pub fn retire(&mut self, key_id: IssuerKeyId) -> Result<(), RotationError> {
        if key_id == self.active {
            return Err(RotationError::RetiringActiveKey(key_id));
        }
        if !self.holds(key_id) {
            return Err(RotationError::UnknownKeyId(key_id));
        }
        self.keys.retain(|key| key.key_id() != key_id);
        Ok(())
    }

    /// Sign `claims` with the active key.
    ///
    /// The caller is expected to have set `claims.issuer_key_id` to
    /// [`Self::active_key_id`]; [`crate::IdentityService`] does. Signing with a
    /// key whose identifier the claims do not name produces a token that fails
    /// `BadSignature` at every verifier, so this is the one place worth being
    /// unambiguous about which key was used.
    /// # Errors
    ///
    /// [`SigningError::NoActiveKey`] cannot occur through this type's public
    /// API — every mutator maintains "the active key is held" — and it is
    /// returned rather than panicked on so that the invariant is checked
    /// instead of asserted.
    pub fn sign(&self, claims: SessionTokenClaimsV1) -> Result<SessionTokenV1, SigningError> {
        let key = self
            .keys
            .iter()
            .find(|key| key.key_id() == self.active)
            .ok_or(SigningError::NoActiveKey(self.active))?;
        SessionTokenV1::sign(claims, &key.secret).map_err(SigningError::Encode)
    }
}
