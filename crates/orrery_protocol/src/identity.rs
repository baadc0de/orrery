//! Transport identity and signatures (D3).
//!
//! iroh's ed25519 key is the transport identity: a peer's `NodeId` is its
//! [`iroh_base::PublicKey`] (aliased `EndpointId`). Signatures use iroh's
//! ed25519 `Signature` type. These are re-exported here so wire types can name
//! them without depending on iroh's full endpoint machinery.

pub use iroh_base::Signature;

use crate::AccountId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A peer's transport identity — iroh's ed25519 public key (D3).
pub type NodeId = iroh_base::PublicKey;

/// The ASCII prefix included in every V1 session-token signature.
pub const SESSION_TOKEN_V1_DOMAIN: &[u8] = b"orrery/session-token/v1";
/// The claims version serialized before probation was carried in the token.
///
/// Still accepted by [`SessionTokenVerifier`], and only there: nothing mints
/// it any more. It exists so that an identity service upgraded ahead of the
/// gateways — or behind them — does not black out logins for the length of a
/// rollout, which is the same dual-accept window `docs/09-services-and-ops.md`
/// §8 requires of an issuer-key rotation. The window is bounded by
/// [`MAX_SESSION_TOKEN_TTL_MS`]: one hour after the last old issuer stops, no
/// version-1 token can still be inside its signed lifetime.
pub const SESSION_TOKEN_V1_VERSION: u8 = 1;
/// The claims version this build mints, carrying
/// [`SessionTokenClaimsV1::on_probation`].
///
/// The *envelope* is still V1 — same domain string, same
/// `{claims, signature}` framing, same key selection — because none of that
/// changed. What moved is the claims body, and it moved by appending one byte,
/// which postcard's positional encoding makes readable only by a decoder that
/// expects it. The version byte is the first field of the body, so which
/// decoder to use is answerable before anything else is parsed.
pub const SESSION_TOKEN_V2_VERSION: u8 = 2;
/// The maximum accepted encoded token size before postcard decoding.
pub const MAX_SESSION_TOKEN_BYTES: usize = 512;
/// The longest session-token lifetime accepted by a verifier.
pub const MAX_SESSION_TOKEN_TTL_MS: u64 = 3_600_000;

/// A Unix wall-clock timestamp measured in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnixMillis(pub u64);

impl UnixMillis {
    /// Creates a timestamp from Unix milliseconds.
    #[must_use]
    pub const fn new(milliseconds: u64) -> Self {
        Self(milliseconds)
    }
}

/// A session-token lifetime measured in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionTokenTtlMs(pub u64);

impl SessionTokenTtlMs {
    /// Creates a session-token lifetime from milliseconds.
    #[must_use]
    pub const fn new(milliseconds: u64) -> Self {
        Self(milliseconds)
    }
}

/// A key identifier selected by the identity issuer for signature rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IssuerKeyId(pub u32);

impl IssuerKeyId {
    /// Creates an issuer key identifier from its stable numeric value.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
}

/// The standing carried in a session token.
///
/// Deliberately two-valued: cooldown and ban are admission decisions identity
/// makes at mint time, not claims a connected peer must interpret (D33 clause
/// (e)), so they never widen this enum or the token that carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStanding {
    /// The account is not quarantined.
    Good,
    /// The account may connect but requires additional write validation.
    Quarantined,
}

/// One account whose outstanding session tokens identity has invalidated
/// (D33 clause (e)).
///
/// Published when an account's standing crosses into cooldown or ban. A token
/// is dead when its signed `issued_at_ms` is **older than**
/// `effective_from_ms`: identity refused that account at `effective_from_ms`,
/// so anything it signed earlier was admitted under a standing identity no
/// longer holds. A token issued *after* the watermark was minted by an
/// identity that answered for the account again — a lifted cooldown or an
/// upheld appeal — and passes normally. No new token field carries this; the
/// bound rides the timestamp every V1 claim already signs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountInvalidation {
    /// The account whose outstanding tokens are invalidated.
    pub account: AccountId,
    /// Identity's read instant when the refusal began, in Unix milliseconds.
    ///
    /// Tokens with `issued_at_ms < effective_from_ms` are refused while this
    /// entry stands; tokens minted at or after it were issued past the
    /// refusal and are accepted on their own merits.
    pub effective_from_ms: UnixMillis,
}

/// The signed V1 session-token claims, serialized with postcard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTokenClaimsV1 {
    /// The authenticated wire-contract version.
    pub version: u8,
    /// The durable account identity that logged in.
    pub account: AccountId,
    /// The iroh transport identity this token authorizes.
    pub node: NodeId,
    /// The Unix millisecond instant at which identity issued the token.
    pub issued_at_ms: UnixMillis,
    /// The requested session lifetime, capped by the verifier.
    pub ttl_ms: SessionTokenTtlMs,
    /// The account enforcement standing at issuance.
    pub standing: SessionStanding,
    /// The identity issuer key used to select a verifier key.
    pub issuer_key_id: IssuerKeyId,
    /// Whether the account was still inside its probation window at issuance
    /// (D33 clause (d)'s 7-day default), and therefore may not witness.
    ///
    /// A boolean rather than an age or an age bucket, because the probation
    /// window is a *deployment* dial — D33 clause (d) names it alongside `Q`,
    /// `C` and `B` — and identity is the service configured with it. Sending an
    /// age would put a second copy of that dial in every coordinator, and two
    /// copies of one dial are two dials that can disagree. Sending the verdict
    /// keeps the policy where the configuration is and leaves the reader with
    /// nothing to interpret.
    ///
    /// `true` is the closed direction and is what an unknown resolves to: a
    /// pre-probation token (claims version 1) lifts to `true`, because a
    /// verifier that cannot tell must not seat the account on a set that judges
    /// other players' intents.
    ///
    /// It is only as fresh as the token. An account crossing its probation
    /// boundary keeps a `true` token until it next refreshes, so eligibility
    /// opens up to [`MAX_SESSION_TOKEN_TTL_MS`] late — half that in practice,
    /// since `docs/09` §8 refreshes at half-TTL. Late in the safe direction, and
    /// the alternative was a durable read on a path that has none (D31 clause
    /// (d): the coordinator reads nothing from FoundationDB).
    pub on_probation: bool,
}

impl SessionTokenClaimsV1 {
    /// Creates a claims value at the current version, ready for signing.
    #[must_use]
    pub fn new(
        account: AccountId,
        node: NodeId,
        issued_at_ms: UnixMillis,
        ttl_ms: SessionTokenTtlMs,
        standing: SessionStanding,
        issuer_key_id: IssuerKeyId,
        on_probation: bool,
    ) -> Self {
        Self {
            version: SESSION_TOKEN_V2_VERSION,
            account,
            node,
            issued_at_ms,
            ttl_ms,
            standing,
            issuer_key_id,
            on_probation,
        }
    }
}

/// The claims body as an issuer that predates probation serialized it.
///
/// Kept as a distinct type rather than reconstructed by trimming a field,
/// because postcard is positional: what makes a version-1 body readable is a
/// decoder with exactly its seven fields in exactly its order, and the only way
/// to keep that stable against a future edit to [`SessionTokenClaimsV1`] is for
/// it not to share a definition with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LegacySessionTokenClaims {
    version: u8,
    account: AccountId,
    node: NodeId,
    issued_at_ms: UnixMillis,
    ttl_ms: SessionTokenTtlMs,
    standing: SessionStanding,
    issuer_key_id: IssuerKeyId,
}

impl LegacySessionTokenClaims {
    /// Lift a version-1 body into the current shape, failing closed on the one
    /// field it cannot answer.
    fn widen(self) -> SessionTokenClaimsV1 {
        SessionTokenClaimsV1 {
            version: self.version,
            account: self.account,
            node: self.node,
            issued_at_ms: self.issued_at_ms,
            ttl_ms: self.ttl_ms,
            standing: self.standing,
            issuer_key_id: self.issuer_key_id,
            // Unknown, therefore ineligible. This is the direction D27 clause
            // (c) takes with an old-format attestation — accepted as a
            // signature, counted toward no required slot — and the direction
            // `WitnessSeeder::note_grace_session` takes with a stale standing.
            on_probation: true,
        }
    }
}

/// A postcard session-token envelope containing V1 claims and their signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTokenV1 {
    /// The signature-protected V1 claims.
    pub claims: SessionTokenClaimsV1,
    /// The issuer's Ed25519 signature over the domain-separated claims bytes.
    pub signature: Signature,
}

impl SessionTokenV1 {
    /// Signs V1 claims using an identity issuer's Ed25519 key.
    pub fn sign(
        claims: SessionTokenClaimsV1,
        key: &iroh_base::SecretKey,
    ) -> Result<Self, postcard::Error> {
        let payload = signature_payload(&claims)?;
        Ok(Self {
            claims,
            signature: key.sign(&payload),
        })
    }

    /// Encodes this fixed V1 envelope with postcard.
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_stdvec(self)
    }

    /// Decodes an envelope of either accepted claims version, after applying
    /// the wire-size bound.
    ///
    /// A version-1 body is lifted into the current claims shape with
    /// `on_probation = true`, keeping its own `version` value: the returned
    /// value therefore says which shape arrived, and re-[`Self::encode`]ing it
    /// produces bytes this function refuses rather than a second, subtly
    /// different token. Verification never re-serializes — see
    /// [`SessionTokenVerifier::verify`] — so the asymmetry costs nothing on the
    /// path that matters.
    pub fn decode(encoded: &[u8]) -> Result<Self, SessionTokenVerificationError> {
        let (claims, signature, _) = decode_parts(encoded)?;
        Ok(Self { claims, signature })
    }
}

/// Split an encoded envelope into its claims, its signature, and the exact
/// claims bytes the signature covers.
///
/// The third element is a slice of the input rather than a re-serialization.
/// That is what makes dual-accept honest: the payload a version-1 issuer signed
/// is its own seven-field body, and reconstructing it from the widened struct
/// would mean trusting a second encoder to agree with the first one byte for
/// byte. postcard writes struct fields in declaration order with nothing
/// between them, so the claims occupy a prefix of the envelope and the
/// signature is the rest.
fn decode_parts(
    encoded: &[u8],
) -> Result<(SessionTokenClaimsV1, Signature, &[u8]), SessionTokenVerificationError> {
    if encoded.len() > MAX_SESSION_TOKEN_BYTES {
        return Err(SessionTokenVerificationError::Malformed);
    }
    // The version is the body's first field and postcard writes a `u8` as one
    // raw byte, so which decoder to use is answerable before anything else is
    // parsed — no ambiguous "try the wide shape, fall back to the narrow one"
    // where a signature byte could pass for a trailing `bool`. The byte is only
    // a dispatch hint: each arm re-checks the version it actually decoded.
    match encoded.first().copied() {
        Some(SESSION_TOKEN_V2_VERSION) => {
            let (claims, signature, signed) =
                take_claims::<SessionTokenClaimsV1>(encoded, SESSION_TOKEN_V2_VERSION, |c| {
                    c.version
                })?;
            Ok((claims, signature, signed))
        }
        Some(SESSION_TOKEN_V1_VERSION) => {
            let (legacy, signature, signed) =
                take_claims::<LegacySessionTokenClaims>(encoded, SESSION_TOKEN_V1_VERSION, |c| {
                    c.version
                })?;
            Ok((legacy.widen(), signature, signed))
        }
        _ => Err(SessionTokenVerificationError::Malformed),
    }
}

/// Decode one claims body of shape `T` plus its trailing signature.
fn take_claims<T: serde::de::DeserializeOwned>(
    encoded: &[u8],
    expected_version: u8,
    version_of: impl Fn(&T) -> u8,
) -> Result<(T, Signature, &[u8]), SessionTokenVerificationError> {
    let (claims, rest) = postcard::take_from_bytes::<T>(encoded)
        .map_err(|_| SessionTokenVerificationError::Malformed)?;
    if version_of(&claims) != expected_version {
        return Err(SessionTokenVerificationError::Malformed);
    }
    let (signature, remainder) = postcard::take_from_bytes::<Signature>(rest)
        .map_err(|_| SessionTokenVerificationError::Malformed)?;
    if !remainder.is_empty() {
        return Err(SessionTokenVerificationError::Malformed);
    }
    Ok((claims, signature, &encoded[..encoded.len() - rest.len()]))
}

/// A trusted identity issuer key selected by [`IssuerKeyId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuerKey {
    /// The issuer's stable rotation identifier.
    pub key_id: IssuerKeyId,
    /// The Ed25519 public key permitted to verify this identifier.
    pub public_key: NodeId,
}

impl IssuerKey {
    /// Creates one trusted issuer-key entry.
    #[must_use]
    pub fn new(key_id: IssuerKeyId, public_key: NodeId) -> Self {
        Self { key_id, public_key }
    }
}

/// A source of Unix wall-clock time for token verification.
pub trait TokenClock {
    /// Returns the current Unix millisecond timestamp.
    fn now_ms(&self) -> UnixMillis;
}

/// A fixed clock useful for deterministic verification and simple embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedTokenClock(UnixMillis);

impl FixedTokenClock {
    /// Creates a clock that always returns the supplied Unix timestamp.
    #[must_use]
    pub const fn new(now_ms: UnixMillis) -> Self {
        Self(now_ms)
    }
}

impl TokenClock for FixedTokenClock {
    fn now_ms(&self) -> UnixMillis {
        self.0
    }
}

/// A decoder and verifier for a configured set of V1 identity issuer keys.
#[derive(Debug, Clone)]
pub struct SessionTokenVerifier<C> {
    clock: C,
    issuer_keys: Vec<IssuerKey>,
}

impl<C> SessionTokenVerifier<C> {
    /// Creates a verifier with an injected clock and trusted issuer-key set.
    pub fn new(clock: C, issuer_keys: impl IntoIterator<Item = IssuerKey>) -> Self {
        Self {
            clock,
            issuer_keys: issuer_keys.into_iter().collect(),
        }
    }
}

impl<C: TokenClock> SessionTokenVerifier<C> {
    /// Decodes and verifies one token of either accepted claims version for the
    /// connected iroh transport node.
    ///
    /// The signature is checked over the claims bytes **as they arrived**, not
    /// over a re-encoding of the decoded struct. A version-1 body signed seven
    /// fields; the widened struct would serialize eight.
    pub fn verify(
        &self,
        encoded: &[u8],
        expected_node: &NodeId,
    ) -> Result<SessionTokenClaimsV1, SessionTokenVerificationError> {
        let (claims, signature, signed_claims) = decode_parts(encoded)?;
        let issuer = self
            .issuer_keys
            .iter()
            .find(|issuer| issuer.key_id == claims.issuer_key_id)
            .ok_or(SessionTokenVerificationError::UnknownIssuer(
                claims.issuer_key_id,
            ))?;
        issuer
            .public_key
            .verify(&domain_separated(signed_claims), &signature)
            .map_err(|_| SessionTokenVerificationError::BadSignature)?;
        if &claims.node != expected_node {
            return Err(SessionTokenVerificationError::WrongNode);
        }
        if claims.ttl_ms.0 > MAX_SESSION_TOKEN_TTL_MS {
            return Err(SessionTokenVerificationError::OverTtl);
        }
        let now_ms = self.clock.now_ms();
        if claims.issued_at_ms > now_ms {
            return Err(SessionTokenVerificationError::Future);
        }
        if now_ms.0 - claims.issued_at_ms.0 >= claims.ttl_ms.0 {
            return Err(SessionTokenVerificationError::Expired);
        }
        Ok(claims)
    }
}

/// A typed rejection from V1 token framing or verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionTokenVerificationError {
    /// The encoded envelope exceeds its bound or is not a valid V1 postcard frame.
    Malformed,
    /// The token names an issuer key that is not trusted by this verifier.
    UnknownIssuer(IssuerKeyId),
    /// The trusted issuer key could not verify the domain-separated claims.
    BadSignature,
    /// The signed transport identity does not equal the connected remote node.
    WrongNode,
    /// The token issue time is later than the injected wall clock.
    Future,
    /// The token is at or beyond its signed lifetime.
    Expired,
    /// The signed token lifetime is longer than the one-hour policy cap.
    OverTtl,
}

impl fmt::Display for SessionTokenVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("malformed session token"),
            Self::UnknownIssuer(key_id) => {
                write!(formatter, "unknown session-token issuer key {}", key_id.0)
            }
            Self::BadSignature => formatter.write_str("invalid session-token signature"),
            Self::WrongNode => formatter.write_str("session token is bound to a different node"),
            Self::Future => formatter.write_str("session token was issued in the future"),
            Self::Expired => formatter.write_str("session token has expired"),
            Self::OverTtl => formatter.write_str("session token lifetime exceeds the policy cap"),
        }
    }
}

impl std::error::Error for SessionTokenVerificationError {}

fn signature_payload(claims: &SessionTokenClaimsV1) -> Result<Vec<u8>, postcard::Error> {
    Ok(domain_separated(&postcard::to_stdvec(claims)?))
}

/// The exact bytes an issuer signs: the domain prefix, then the claims body.
///
/// One function so the signing and verifying halves cannot drift, which is the
/// whole point of a domain-separated payload.
fn domain_separated(claims: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(SESSION_TOKEN_V1_DOMAIN.len() + claims.len());
    payload.extend_from_slice(SESSION_TOKEN_V1_DOMAIN);
    payload.extend_from_slice(claims);
    payload
}

#[cfg(test)]
mod tests {
    use crate::AccountId;

    /// Deterministic [`NodeId`] from a one-byte discriminant.
    fn node(n: u8) -> super::NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn sig() -> super::Signature {
        let seed = [0u8; 32];
        iroh_base::SecretKey::from_bytes(&seed).sign(b"test")
    }

    #[test]
    fn node_id_from_secret_key() {
        let a = node(1);
        let b = node(2);
        // Different seeds produce different public keys.
        assert_ne!(a, b);
        // The key is not the all-zeros sentinel.
        assert_ne!(a, super::NodeId::from_bytes(&[0u8; 32]).unwrap());
    }

    #[test]
    fn node_id_equality_and_clone() {
        let a = node(42);
        let b = node(42);
        assert_eq!(a, b);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn node_id_from_bytes_roundtrip() {
        let a = node(7);
        let raw = a.as_bytes();
        let back = super::NodeId::from_bytes(raw).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn signature_create_and_verify() {
        let msg = b"hello orrery";
        let sk = iroh_base::SecretKey::from_bytes(&[8u8; 32]);
        let pk = sk.public();
        let signature = sk.sign(msg);
        // The signature verifies against the public key that produced it.
        assert!(pk.verify(msg, &signature).is_ok());
    }

    #[test]
    fn signature_inequality() {
        let sk = iroh_base::SecretKey::from_bytes(&[8u8; 32]);
        let sig_a = sk.sign(b"message one");
        let sig_b = sk.sign(b"message two");
        // Different messages produce different signatures.
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn node_id_postcard_roundtrip() {
        let a = node(99);
        let bytes = postcard::to_stdvec(&a).unwrap();
        let back: super::NodeId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn signature_postcard_roundtrip() {
        let a = sig();
        let bytes = postcard::to_stdvec(&a).unwrap();
        let back: super::Signature = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn session_token_v1_verifies_signed_claims_for_the_bound_node() {
        // Given: deterministic issuer and transport keys with a fixed token clock.
        let issuer = iroh_base::SecretKey::from_bytes(&[1; 32]);
        let bound_node = node(2);
        let claims = crate::SessionTokenClaimsV1::new(
            AccountId::new(42),
            bound_node,
            crate::UnixMillis::new(1_000_000),
            crate::SessionTokenTtlMs::new(10_000),
            crate::SessionStanding::Good,
            crate::IssuerKeyId::new(7),
            false,
        );
        let token = crate::SessionTokenV1::sign(claims.clone(), &issuer).unwrap();
        let verifier = crate::SessionTokenVerifier::new(
            crate::FixedTokenClock::new(crate::UnixMillis::new(1_005_000)),
            [crate::IssuerKey::new(
                crate::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // When: the wire token is decoded and verified for the connected node.
        let encoded = token.encode().unwrap();
        let decoded = crate::SessionTokenV1::decode(&encoded).unwrap();
        let verified = verifier.verify(&encoded, &bound_node).unwrap();

        // Then: only the signed, bound claims are returned.
        assert_eq!(decoded, token);
        assert_eq!(verified, claims);
    }

    #[test]
    fn session_token_v1_rejects_altered_signed_claims() {
        // Given: a valid token whose signed account claim is changed without resigning.
        let issuer = iroh_base::SecretKey::from_bytes(&[3; 32]);
        let bound_node = node(4);
        let claims = super::SessionTokenClaimsV1::new(
            AccountId::new(42),
            bound_node,
            super::UnixMillis::new(1_000_000),
            super::SessionTokenTtlMs::new(10_000),
            super::SessionStanding::Good,
            super::IssuerKeyId::new(7),
            false,
        );
        let mut token = super::SessionTokenV1::sign(claims, &issuer).unwrap();
        token.claims.account = AccountId::new(43);
        let verifier = super::SessionTokenVerifier::new(
            super::FixedTokenClock::new(super::UnixMillis::new(1_005_000)),
            [super::IssuerKey::new(
                super::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // When: the altered token is verified.
        let result = verifier.verify(&token.encode().unwrap(), &bound_node);

        // Then: the verifier reports the signature boundary failure.
        assert_eq!(
            result,
            Err(super::SessionTokenVerificationError::BadSignature)
        );
    }

    #[test]
    fn session_token_v1_rejects_unknown_issuer_key_id() {
        // Given: a valid token whose issuer key id is not configured in the verifier.
        let issuer = iroh_base::SecretKey::from_bytes(&[5; 32]);
        let bound_node = node(6);
        let claims = super::SessionTokenClaimsV1::new(
            AccountId::new(42),
            bound_node,
            super::UnixMillis::new(1_000_000),
            super::SessionTokenTtlMs::new(10_000),
            super::SessionStanding::Good,
            super::IssuerKeyId::new(8),
            false,
        );
        let token = super::SessionTokenV1::sign(claims, &issuer).unwrap();
        let verifier = super::SessionTokenVerifier::new(
            super::FixedTokenClock::new(super::UnixMillis::new(1_005_000)),
            [super::IssuerKey::new(
                super::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // When: the token is verified against a rotated-out key set.
        let result = verifier.verify(&token.encode().unwrap(), &bound_node);

        // Then: the missing issuer is distinct from a bad signature.
        assert_eq!(
            result,
            Err(super::SessionTokenVerificationError::UnknownIssuer(
                super::IssuerKeyId::new(8)
            ))
        );
    }

    #[test]
    fn session_token_v1_rejects_a_different_connected_node() {
        // Given: a valid token bound to a different transport identity.
        let issuer = iroh_base::SecretKey::from_bytes(&[9; 32]);
        let bound_node = node(10);
        let claims = super::SessionTokenClaimsV1::new(
            AccountId::new(42),
            bound_node,
            super::UnixMillis::new(1_000_000),
            super::SessionTokenTtlMs::new(10_000),
            super::SessionStanding::Quarantined,
            super::IssuerKeyId::new(7),
            false,
        );
        let token = super::SessionTokenV1::sign(claims, &issuer).unwrap();
        let verifier = super::SessionTokenVerifier::new(
            super::FixedTokenClock::new(super::UnixMillis::new(1_005_000)),
            [super::IssuerKey::new(
                super::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // When: another remote NodeId presents the token.
        let result = verifier.verify(&token.encode().unwrap(), &node(11));

        // Then: the node binding is enforced after signature verification.
        assert_eq!(result, Err(super::SessionTokenVerificationError::WrongNode));
    }

    #[test]
    fn session_token_v1_rejects_future_expired_and_over_ttl_claims() {
        // Given: deterministic issuer, node, and verifier clock.
        let issuer = iroh_base::SecretKey::from_bytes(&[12; 32]);
        let bound_node = node(13);
        let verifier = super::SessionTokenVerifier::new(
            super::FixedTokenClock::new(super::UnixMillis::new(1_005_000)),
            [super::IssuerKey::new(
                super::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // When: the issuer signs claims outside the accepted clock and TTL bounds.
        let future = super::SessionTokenV1::sign(
            super::SessionTokenClaimsV1::new(
                AccountId::new(42),
                bound_node,
                super::UnixMillis::new(1_005_001),
                super::SessionTokenTtlMs::new(10_000),
                super::SessionStanding::Good,
                super::IssuerKeyId::new(7),
                false,
            ),
            &issuer,
        )
        .unwrap();
        let expired = super::SessionTokenV1::sign(
            super::SessionTokenClaimsV1::new(
                AccountId::new(42),
                bound_node,
                super::UnixMillis::new(1_000_000),
                super::SessionTokenTtlMs::new(5_000),
                super::SessionStanding::Good,
                super::IssuerKeyId::new(7),
                false,
            ),
            &issuer,
        )
        .unwrap();
        let over_ttl = super::SessionTokenV1::sign(
            super::SessionTokenClaimsV1::new(
                AccountId::new(42),
                bound_node,
                super::UnixMillis::new(1_000_000),
                super::SessionTokenTtlMs::new(super::MAX_SESSION_TOKEN_TTL_MS + 1),
                super::SessionStanding::Good,
                super::IssuerKeyId::new(7),
                false,
            ),
            &issuer,
        )
        .unwrap();

        // Then: each temporal failure has a typed outcome.
        assert_eq!(
            verifier.verify(&future.encode().unwrap(), &bound_node),
            Err(super::SessionTokenVerificationError::Future)
        );
        assert_eq!(
            verifier.verify(&expired.encode().unwrap(), &bound_node),
            Err(super::SessionTokenVerificationError::Expired)
        );
        assert_eq!(
            verifier.verify(&over_ttl.encode().unwrap(), &bound_node),
            Err(super::SessionTokenVerificationError::OverTtl)
        );
    }

    #[test]
    fn session_token_v1_rejects_wrong_domain_and_oversized_or_malformed_wire_input() {
        // Given: a known issuer and a compact V1 claims payload.
        let issuer = iroh_base::SecretKey::from_bytes(&[14; 32]);
        let bound_node = node(15);
        let claims = super::SessionTokenClaimsV1::new(
            AccountId::new(42),
            bound_node,
            super::UnixMillis::new(1_000_000),
            super::SessionTokenTtlMs::new(10_000),
            super::SessionStanding::Good,
            super::IssuerKeyId::new(7),
            false,
        );
        let wrong_domain_signature = issuer.sign(
            &[
                b"another/protocol/v1".as_slice(),
                postcard::to_stdvec(&claims).unwrap().as_slice(),
            ]
            .concat(),
        );
        let wrong_domain = super::SessionTokenV1 {
            claims,
            signature: wrong_domain_signature,
        };
        let verifier = super::SessionTokenVerifier::new(
            super::FixedTokenClock::new(super::UnixMillis::new(1_005_000)),
            [super::IssuerKey::new(
                super::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // When: domain-separated, oversized, and malformed frames are presented.
        let wrong_domain_result = verifier.verify(&wrong_domain.encode().unwrap(), &bound_node);
        let oversized_result =
            verifier.verify(&vec![0; super::MAX_SESSION_TOKEN_BYTES + 1], &bound_node);
        let malformed_result = verifier.verify(&[0xff], &bound_node);

        // Then: all are rejected before they can authenticate a session.
        assert_eq!(
            wrong_domain_result,
            Err(super::SessionTokenVerificationError::BadSignature)
        );
        assert_eq!(
            oversized_result,
            Err(super::SessionTokenVerificationError::Malformed)
        );
        assert_eq!(
            malformed_result,
            Err(super::SessionTokenVerificationError::Malformed)
        );
    }

    #[test]
    fn session_token_v1_rejects_each_tampered_signed_or_framed_field() {
        // Given: a public-API token fixture and verifier for one configured issuer.
        let issuer = iroh_base::SecretKey::from_bytes(&[16; 32]);
        let bound_node = node(17);
        let token = crate::SessionTokenV1::sign(
            crate::SessionTokenClaimsV1::new(
                AccountId::new(42),
                bound_node,
                crate::UnixMillis::new(1_000_000),
                crate::SessionTokenTtlMs::new(10_000),
                crate::SessionStanding::Good,
                crate::IssuerKeyId::new(7),
                false,
            ),
            &issuer,
        )
        .unwrap();
        let verifier = crate::SessionTokenVerifier::new(
            crate::FixedTokenClock::new(crate::UnixMillis::new(1_005_000)),
            [crate::IssuerKey::new(
                crate::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // When: each authenticated claim field, signature, and envelope frame changes alone.
        let mut version = token.clone();
        version.claims.version = crate::SESSION_TOKEN_V2_VERSION + 1;
        let mut account = token.clone();
        account.claims.account = AccountId::new(43);
        let mut node = token.clone();
        node.claims.node = iroh_base::SecretKey::from_bytes(&[18; 32]).public();
        let mut issued_at_ms = token.clone();
        issued_at_ms.claims.issued_at_ms = crate::UnixMillis::new(1_000_001);
        let mut ttl_ms = token.clone();
        ttl_ms.claims.ttl_ms = crate::SessionTokenTtlMs::new(10_001);
        let mut standing = token.clone();
        standing.claims.standing = crate::SessionStanding::Quarantined;
        let mut issuer_key_id = token.clone();
        issuer_key_id.claims.issuer_key_id = crate::IssuerKeyId::new(8);
        let mut on_probation = token.clone();
        on_probation.claims.on_probation = true;
        let mut signature = token.clone();
        signature.signature = issuer.sign(b"tampered signature");
        let mut framed = token.encode().unwrap();
        framed.push(0);

        // Then: no altered claim or frame can reach authenticated output.
        assert_eq!(
            verifier.verify(&version.encode().unwrap(), &bound_node),
            Err(crate::SessionTokenVerificationError::Malformed)
        );
        for tampered in [
            account,
            node,
            issued_at_ms,
            ttl_ms,
            standing,
            on_probation,
            signature,
        ] {
            assert_eq!(
                verifier.verify(&tampered.encode().unwrap(), &bound_node),
                Err(crate::SessionTokenVerificationError::BadSignature)
            );
        }
        assert_eq!(
            verifier.verify(&issuer_key_id.encode().unwrap(), &bound_node),
            Err(crate::SessionTokenVerificationError::UnknownIssuer(
                crate::IssuerKeyId::new(8)
            ))
        );
        assert_eq!(
            verifier.verify(&framed, &bound_node),
            Err(crate::SessionTokenVerificationError::Malformed)
        );
    }

    /// The claims body exactly as an issuer that predates probation wrote it.
    ///
    /// Declared here rather than reached for in `super`, so that the window is
    /// tested against a *second, independent* statement of the version-1 field
    /// order. Sharing the production definition would make this assert only
    /// that one struct round-trips through itself.
    #[derive(serde::Serialize)]
    struct PreProbationClaims {
        version: u8,
        account: AccountId,
        node: super::NodeId,
        issued_at_ms: super::UnixMillis,
        ttl_ms: super::SessionTokenTtlMs,
        standing: super::SessionStanding,
        issuer_key_id: super::IssuerKeyId,
    }

    /// Mint a version-1 token the way a not-yet-upgraded identity would.
    fn pre_probation_token(claims: &PreProbationClaims, key: &iroh_base::SecretKey) -> Vec<u8> {
        let body = postcard::to_stdvec(claims).unwrap();
        let signature = key.sign(&[super::SESSION_TOKEN_V1_DOMAIN, body.as_slice()].concat());
        [body, postcard::to_stdvec(&signature).unwrap()].concat()
    }

    fn pre_probation_claims(node: super::NodeId) -> PreProbationClaims {
        PreProbationClaims {
            version: crate::SESSION_TOKEN_V1_VERSION,
            account: AccountId::new(42),
            node,
            issued_at_ms: crate::UnixMillis::new(1_000_000),
            ttl_ms: crate::SessionTokenTtlMs::new(10_000),
            standing: crate::SessionStanding::Good,
            issuer_key_id: crate::IssuerKeyId::new(7),
        }
    }

    #[test]
    fn a_pre_probation_token_still_verifies_and_counts_as_on_probation() {
        // The rolling-upgrade window. Identity and the gateways are separate
        // services with no version handshake between them, so a rollout has an
        // interval in which one is minting the old shape and the other is
        // reading the new one. D27 clause (c) sets the house rule for that: the
        // old-format signature is accepted as a signature and counted toward
        // nothing, which here means "unknown age, therefore not eligible".
        let issuer = iroh_base::SecretKey::from_bytes(&[21; 32]);
        let bound_node = node(22);
        let encoded = pre_probation_token(&pre_probation_claims(bound_node), &issuer);
        let verifier = crate::SessionTokenVerifier::new(
            crate::FixedTokenClock::new(crate::UnixMillis::new(1_005_000)),
            [crate::IssuerKey::new(
                crate::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        let claims = verifier
            .verify(&encoded, &bound_node)
            .expect("a version-1 token authenticates its session");

        assert_eq!(claims.account, AccountId::new(42));
        assert_eq!(claims.standing, crate::SessionStanding::Good);
        assert_eq!(claims.version, crate::SESSION_TOKEN_V1_VERSION);
        assert!(
            claims.on_probation,
            "a token that cannot answer the age question must not be read as \
             having answered it favourably"
        );

        // And `decode` agrees with `verify` about the shape, since the grace
        // path in both the gateway and the coordinator reaches for it.
        let decoded = crate::SessionTokenV1::decode(&encoded).expect("decodes");
        assert!(decoded.claims.on_probation);
        assert_eq!(decoded.claims.version, crate::SESSION_TOKEN_V1_VERSION);
    }

    #[test]
    fn a_pre_probation_token_is_still_a_signed_token_and_not_a_free_pass() {
        // The window widens what decodes, and nothing else. A version-1 body
        // gets the identical signature, node-binding and clock treatment a
        // current one does — otherwise "accept the old shape" would be a
        // second, softer admission path, which is what D33 clause (f)'s
        // fail-closed posture exists to avoid.
        let issuer = iroh_base::SecretKey::from_bytes(&[23; 32]);
        let bound_node = node(24);
        let verifier = crate::SessionTokenVerifier::new(
            crate::FixedTokenClock::new(crate::UnixMillis::new(1_005_000)),
            [crate::IssuerKey::new(
                crate::IssuerKeyId::new(7),
                issuer.public(),
            )],
        );

        // Signed by somebody else.
        let forged = pre_probation_token(
            &pre_probation_claims(bound_node),
            &iroh_base::SecretKey::from_bytes(&[25; 32]),
        );
        assert_eq!(
            verifier.verify(&forged, &bound_node),
            Err(crate::SessionTokenVerificationError::BadSignature)
        );

        // Bound to another node.
        let honest = pre_probation_token(&pre_probation_claims(bound_node), &issuer);
        assert_eq!(
            verifier.verify(&honest, &node(26)),
            Err(crate::SessionTokenVerificationError::WrongNode)
        );

        // Expired.
        let mut short = pre_probation_claims(bound_node);
        short.ttl_ms = crate::SessionTokenTtlMs::new(1_000);
        assert_eq!(
            verifier.verify(&pre_probation_token(&short, &issuer), &bound_node),
            Err(crate::SessionTokenVerificationError::Expired)
        );

        // And a version-1 frame with the probation byte stapled on is not a
        // version-2 token: the append moves the signature, so nothing decodes.
        let mut appended = honest.clone();
        appended.push(1);
        assert_eq!(
            verifier.verify(&appended, &bound_node),
            Err(crate::SessionTokenVerificationError::Malformed)
        );
    }

    #[test]
    fn the_current_shape_is_not_readable_as_the_previous_one() {
        // The claim the `PROTOCOL_VERSION` bump rests on: postcard writes
        // fields positionally with no names and no length prefix, so a decoder
        // built for the seven-field body cannot read the eight-field one — it
        // does not skip the tail, it mis-attributes it and then runs out of
        // bytes. If this ever stopped holding, the bump would be unnecessary
        // and the compatibility window above would be doing nothing.
        let issuer = iroh_base::SecretKey::from_bytes(&[27; 32]);
        let bound_node = node(28);
        let current = crate::SessionTokenV1::sign(
            crate::SessionTokenClaimsV1::new(
                AccountId::new(42),
                bound_node,
                crate::UnixMillis::new(1_000_000),
                crate::SessionTokenTtlMs::new(10_000),
                crate::SessionStanding::Good,
                crate::IssuerKeyId::new(7),
                false,
            ),
            &issuer,
        )
        .unwrap()
        .encode()
        .unwrap();
        let previous = pre_probation_token(&pre_probation_claims(bound_node), &issuer);

        assert_eq!(current.len(), previous.len() + 1, "one appended byte");
        assert_eq!(current[0], crate::SESSION_TOKEN_V2_VERSION);
        assert_eq!(previous[0], crate::SESSION_TOKEN_V1_VERSION);

        // A version-1 reader, stated independently of both the production
        // decoder and the fixture above, reading a version-2 body: the seven
        // fields parse off the front and the remainder is one byte short of a
        // signature.
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct SevenFields {
            version: u8,
            account: AccountId,
            node: super::NodeId,
            issued_at_ms: super::UnixMillis,
            ttl_ms: super::SessionTokenTtlMs,
            standing: super::SessionStanding,
            issuer_key_id: super::IssuerKeyId,
        }
        let (_, rest) = postcard::take_from_bytes::<SevenFields>(&current).unwrap();
        let (recovered, trailing) = postcard::take_from_bytes::<super::Signature>(rest).unwrap();
        assert_eq!(
            trailing.len(),
            1,
            "a version-1 reader reaches the end of what it believes is the \
             signature one byte before the end of the frame"
        );
        // Which is the whole failure mode, in two parts. A reader that checks
        // for trailing bytes — every reader in this tree does — refuses the
        // frame outright. A reader that does not check recovers a signature
        // shifted one byte off the real one, and fails at the signature
        // instead. Neither ignores the tail, because there is no tail to
        // ignore: the bytes are a body, not an annotated record.
        let real = postcard::from_bytes::<super::Signature>(&current[current.len() - 64..])
            .expect("the last 64 bytes are the signature the issuer wrote");
        assert_ne!(
            recovered, real,
            "the probation byte displaces the signature rather than following it"
        );
    }
}
