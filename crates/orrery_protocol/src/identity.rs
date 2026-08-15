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
/// The version serialized in [`SessionTokenClaimsV1`].
pub const SESSION_TOKEN_V1_VERSION: u8 = 1;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStanding {
    /// The account is not quarantined.
    Good,
    /// The account may connect but requires additional write validation.
    Quarantined,
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
}

impl SessionTokenClaimsV1 {
    /// Creates a V1 claims value ready for signing.
    #[must_use]
    pub fn new(
        account: AccountId,
        node: NodeId,
        issued_at_ms: UnixMillis,
        ttl_ms: SessionTokenTtlMs,
        standing: SessionStanding,
        issuer_key_id: IssuerKeyId,
    ) -> Self {
        Self {
            version: SESSION_TOKEN_V1_VERSION,
            account,
            node,
            issued_at_ms,
            ttl_ms,
            standing,
            issuer_key_id,
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

    /// Decodes a V1 envelope after applying the wire-size and version bounds.
    pub fn decode(encoded: &[u8]) -> Result<Self, SessionTokenVerificationError> {
        if encoded.len() > MAX_SESSION_TOKEN_BYTES {
            return Err(SessionTokenVerificationError::Malformed);
        }
        let (token, remainder) = postcard::take_from_bytes::<Self>(encoded)
            .map_err(|_| SessionTokenVerificationError::Malformed)?;
        if !remainder.is_empty() {
            return Err(SessionTokenVerificationError::Malformed);
        }
        if token.claims.version != SESSION_TOKEN_V1_VERSION {
            return Err(SessionTokenVerificationError::Malformed);
        }
        Ok(token)
    }
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
    /// Decodes and verifies one V1 token for the connected iroh transport node.
    pub fn verify(
        &self,
        encoded: &[u8],
        expected_node: &NodeId,
    ) -> Result<SessionTokenClaimsV1, SessionTokenVerificationError> {
        let token = SessionTokenV1::decode(encoded)?;
        let issuer = self
            .issuer_keys
            .iter()
            .find(|issuer| issuer.key_id == token.claims.issuer_key_id)
            .ok_or(SessionTokenVerificationError::UnknownIssuer(
                token.claims.issuer_key_id,
            ))?;
        let payload = signature_payload(&token.claims)
            .map_err(|_| SessionTokenVerificationError::Malformed)?;
        issuer
            .public_key
            .verify(&payload, &token.signature)
            .map_err(|_| SessionTokenVerificationError::BadSignature)?;
        if &token.claims.node != expected_node {
            return Err(SessionTokenVerificationError::WrongNode);
        }
        if token.claims.ttl_ms.0 > MAX_SESSION_TOKEN_TTL_MS {
            return Err(SessionTokenVerificationError::OverTtl);
        }
        let now_ms = self.clock.now_ms();
        if token.claims.issued_at_ms > now_ms {
            return Err(SessionTokenVerificationError::Future);
        }
        if now_ms.0 - token.claims.issued_at_ms.0 >= token.claims.ttl_ms.0 {
            return Err(SessionTokenVerificationError::Expired);
        }
        Ok(token.claims)
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
    let claims = postcard::to_stdvec(claims)?;
    let mut payload = Vec::with_capacity(SESSION_TOKEN_V1_DOMAIN.len() + claims.len());
    payload.extend_from_slice(SESSION_TOKEN_V1_DOMAIN);
    payload.extend_from_slice(&claims);
    Ok(payload)
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
        version.claims.version = crate::SESSION_TOKEN_V1_VERSION + 1;
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
        let mut signature = token.clone();
        signature.signature = issuer.sign(b"tampered signature");
        let mut framed = token.encode().unwrap();
        framed.push(0);

        // Then: no altered claim or frame can reach authenticated output.
        assert_eq!(
            verifier.verify(&version.encode().unwrap(), &bound_node),
            Err(crate::SessionTokenVerificationError::Malformed)
        );
        for tampered in [account, node, issued_at_ms, ttl_ms, standing, signature] {
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
}
