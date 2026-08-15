//! Coordinator wire surface (docs/10-crates.md §12, docs/02-networking.md §3).
//!
//! The coordinator (`orrery_coordinator`) is a Bevy-free binary; peers speak to
//! it over iroh. The message set and the island/topology types it carries are
//! defined here, engine-agnostic, so both the Bevy-free coordinator and the
//! Bevy `orrery_net` plugin share one wire surface.

use serde::{Deserialize, Serialize};

use crate::identity::{IssuerKey, IssuerKeyId, Signature};
use crate::{CellId, Epoch, GridId, NodeId};

/// A coordinator-allocated island identifier (docs/02-networking.md §3).
///
/// An island is one replication session: a connected set of populated cells
/// plus the peers in them (D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IslandId(pub u64);

impl IslandId {
    /// An island id from a raw u64.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }
}

impl core::fmt::Display for IslandId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "island:{}", self.0)
    }
}

/// The topology regime of an island (D6, docs/02-networking.md §6).
///
/// - [`Mesh`](TopologyRegime::Mesh): ≤ 8 peers, full mesh.
/// - [`InterestMesh`](TopologyRegime::InterestMesh): 9–32 peers, partial mesh
///   with the bounded high-rate set and 1–4 Hz proxies.
/// - [`Promoted`](TopologyRegime::Promoted): > 32 sustained, a coordinator-
///   spawned field host holds cell-entity authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TopologyRegime {
    /// Full mesh, ≤ 8 peers.
    Mesh,
    /// Interest mesh, 9–32 peers.
    InterestMesh,
    /// Coordinator-spawned field host, > 32 sustained.
    Promoted {
        /// The field host's NodeId.
        host: NodeId,
    },
}

impl TopologyRegime {
    /// The population threshold at which an island leaves the full-mesh regime.
    pub const MESH_MAX: usize = 8;
    /// The population threshold at which an island enters the promoted regime.
    pub const INTEREST_MAX: usize = 32;

    /// The regime for a population, given the optional promoted host.
    #[must_use]
    pub fn for_population(pop: usize, host: Option<NodeId>) -> Self {
        match host {
            Some(host) => Self::Promoted { host },
            None if pop <= Self::MESH_MAX => Self::Mesh,
            None => Self::InterestMesh,
        }
    }
}

/// A peer's entry in an island manifest (docs/02-networking.md §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEntry {
    /// The peer's NodeId.
    pub node: NodeId,
    /// The cells this peer occupies.
    pub cells: Vec<CellId>,
}

/// An island manifest: the coordinator's membership handout (D12).
///
/// Epochs make manifests idempotent — a peer applies only monotonically newer
/// manifests (docs/02-networking.md §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IslandManifest {
    /// The coordinator-allocated island id.
    pub island: IslandId,
    /// Bumped on any membership/topology change.
    pub epoch: u32,
    /// The populated cells this island covers.
    pub cells: Vec<CellId>,
    /// The topology regime.
    pub regime: TopologyRegime,
    /// The peers in the island (excluding the local peer).
    pub peers: Vec<PeerEntry>,
}

/// The coordinator's current authorization snapshot for one peer's active
/// interest set.
///
/// Gateways consume this value as an immutable handout from the coordinator;
/// client area-load requests never create or extend its coverage. It is the
/// gateway-local form of an [`InterestGrantV1`]: the signed wire grant carries
/// a lifetime, and the gateway turns that into the deadline below against its
/// own clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorInterestSnapshot {
    /// The peer whose interest set this snapshot authorizes.
    pub peer: NodeId,
    /// Monotonic coordinator epoch for replacing older snapshots.
    pub epoch: Epoch,
    /// The grid containing every covered cell.
    pub grid: GridId,
    /// Cells covered by the peer's coordinator-confirmed active interest.
    pub covered_cells: Vec<CellId>,
    /// Deadline in the **holding gateway's** monotonic milliseconds; the
    /// snapshot is stale at and after this instant. Never a coordinator
    /// timestamp: the two processes have unrelated monotonic origins.
    pub valid_until_ms: u64,
}

impl CoordinatorInterestSnapshot {
    /// Localize a verified grant against the accepting gateway's clock.
    #[must_use]
    pub fn from_grant(claims: InterestGrantClaimsV1, accepted_at_ms: u64) -> Self {
        Self {
            peer: claims.peer,
            epoch: claims.epoch,
            grid: claims.grid,
            covered_cells: claims.covered_cells,
            valid_until_ms: accepted_at_ms.saturating_add(claims.ttl_ms),
        }
    }
}

/// The ASCII prefix included in every V1 interest-grant signature.
pub const INTEREST_GRANT_V1_DOMAIN: &[u8] = b"orrery/interest-grant/v1";
/// The version serialized in [`InterestGrantClaimsV1`].
pub const INTEREST_GRANT_V1_VERSION: u8 = 1;
/// The maximum accepted encoded grant size before postcard decoding.
///
/// A grant covers a peer's active interest set, which D5 bounds at the 27-cell
/// neighbourhood; this leaves generous headroom over that without letting an
/// unauthenticated buffer force an unbounded decode.
pub const MAX_INTEREST_GRANT_BYTES: usize = 1024;
/// The most cells one grant may cover.
///
/// The 27-cell neighbourhood is the interest set (D5); the allowance here is
/// deliberately larger so a grant can span a boundary crossing, and finite so
/// a signed grant cannot be inflated into an unbounded membership test.
pub const MAX_INTEREST_GRANT_CELLS: usize = 64;
/// The longest interest-grant lifetime a verifier accepts.
pub const MAX_INTEREST_GRANT_TTL_MS: u64 = 300_000;

/// The signature-protected body of a coordinator interest grant.
///
/// This is the *wire* form of what a gateway stores as a
/// [`CoordinatorInterestSnapshot`]. The difference is deliberate: the grant
/// carries a **duration** (`ttl_ms`), never a deadline. The coordinator and
/// the gateway are separate processes with unrelated monotonic origins, so a
/// coordinator-stamped instant is not a quantity a gateway can compare against
/// its own clock. The gateway derives its own deadline on receipt — the same
/// rule leases already follow, where a `Grant`'s `ttl_ms` establishes a fresh
/// local expiry rather than importing the registrar's clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterestGrantClaimsV1 {
    /// Envelope version; a decoder rejects anything else.
    pub version: u8,
    /// The peer whose interest set this grant authorizes.
    pub peer: NodeId,
    /// Monotonic coordinator epoch; a gateway keeps the highest it has seen
    /// per peer, so a replayed older grant cannot widen a narrowed interest.
    pub epoch: Epoch,
    /// The grid containing every covered cell.
    pub grid: GridId,
    /// Cells covered by the peer's coordinator-confirmed active interest.
    pub covered_cells: Vec<CellId>,
    /// How long the grant remains usable after the gateway accepts it.
    pub ttl_ms: u64,
    /// The coordinator key that signed this grant, for rotation.
    pub issuer_key_id: IssuerKeyId,
}

impl InterestGrantClaimsV1 {
    /// Build V1 claims at the current envelope version.
    #[must_use]
    pub fn new(
        peer: NodeId,
        epoch: Epoch,
        grid: GridId,
        covered_cells: Vec<CellId>,
        ttl_ms: u64,
        issuer_key_id: IssuerKeyId,
    ) -> Self {
        Self {
            version: INTEREST_GRANT_V1_VERSION,
            peer,
            epoch,
            grid,
            covered_cells,
            ttl_ms,
            issuer_key_id,
        }
    }
}

/// A postcard interest-grant envelope containing V1 claims and their signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterestGrantV1 {
    /// The signature-protected V1 claims.
    pub claims: InterestGrantClaimsV1,
    /// The coordinator's Ed25519 signature over the domain-separated claims.
    pub signature: Signature,
}

impl InterestGrantV1 {
    /// Sign V1 claims with a coordinator's Ed25519 key.
    pub fn sign(
        claims: InterestGrantClaimsV1,
        key: &iroh_base::SecretKey,
    ) -> Result<Self, postcard::Error> {
        let payload = grant_signature_payload(&claims)?;
        Ok(Self {
            claims,
            signature: key.sign(&payload),
        })
    }

    /// Encode this fixed V1 envelope with postcard.
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_stdvec(self)
    }

    /// Decode a V1 envelope after applying the wire-size and version bounds.
    pub fn decode(encoded: &[u8]) -> Result<Self, InterestGrantVerificationError> {
        if encoded.len() > MAX_INTEREST_GRANT_BYTES {
            return Err(InterestGrantVerificationError::Malformed);
        }
        let (grant, remainder) = postcard::take_from_bytes::<Self>(encoded)
            .map_err(|_| InterestGrantVerificationError::Malformed)?;
        if !remainder.is_empty() {
            return Err(InterestGrantVerificationError::Malformed);
        }
        if grant.claims.version != INTEREST_GRANT_V1_VERSION {
            return Err(InterestGrantVerificationError::Malformed);
        }
        Ok(grant)
    }
}

/// Why an interest grant was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterestGrantVerificationError {
    /// Oversized, undecodable, or a version this build does not accept.
    Malformed,
    /// No configured coordinator key carries the claimed identifier.
    UnknownIssuer(IssuerKeyId),
    /// The signature does not verify under the named coordinator key.
    BadSignature,
    /// The grant authorizes a peer other than the one presenting it.
    WrongPeer,
    /// The grant covers no cells, or more than [`MAX_INTEREST_GRANT_CELLS`].
    CellCount,
    /// The requested lifetime exceeds [`MAX_INTEREST_GRANT_TTL_MS`].
    OverTtl,
    /// A grant at or below the highest epoch already accepted for this peer.
    Superseded,
    /// This gateway is not configured to accept coordinator interest grants.
    Unsupported,
}

impl core::fmt::Display for InterestGrantVerificationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed => f.write_str("interest grant is malformed"),
            Self::UnknownIssuer(id) => write!(f, "unknown coordinator key {}", id.0),
            Self::BadSignature => f.write_str("interest grant signature does not verify"),
            Self::WrongPeer => f.write_str("interest grant authorizes a different peer"),
            Self::CellCount => f.write_str("interest grant covers an unusable number of cells"),
            Self::OverTtl => f.write_str("interest grant lifetime is too long"),
            Self::Superseded => f.write_str("interest grant epoch is not newer"),
            Self::Unsupported => f.write_str("interest grants are not accepted here"),
        }
    }
}

impl core::error::Error for InterestGrantVerificationError {}

/// Verify a presented grant against the configured coordinator keys.
///
/// `presenter` is the transport-authenticated identity of whoever handed the
/// grant over. A peer carrying its *own* coordinator-signed grant is the
/// intended flow — the same handout model as a session token — so the grant
/// must name the presenter. Relaying someone else's is rejected even though
/// the signature is genuine, because interest is what gates authority claims.
pub fn verify_interest_grant(
    encoded: &[u8],
    presenter: &NodeId,
    keys: &[IssuerKey],
) -> Result<InterestGrantClaimsV1, InterestGrantVerificationError> {
    let grant = InterestGrantV1::decode(encoded)?;
    let key = keys
        .iter()
        .find(|key| key.key_id == grant.claims.issuer_key_id)
        .ok_or(InterestGrantVerificationError::UnknownIssuer(
            grant.claims.issuer_key_id,
        ))?;
    let payload = grant_signature_payload(&grant.claims)
        .map_err(|_| InterestGrantVerificationError::Malformed)?;
    key.public_key
        .verify(&payload, &grant.signature)
        .map_err(|_| InterestGrantVerificationError::BadSignature)?;
    if grant.claims.peer != *presenter {
        return Err(InterestGrantVerificationError::WrongPeer);
    }
    if grant.claims.covered_cells.is_empty()
        || grant.claims.covered_cells.len() > MAX_INTEREST_GRANT_CELLS
    {
        return Err(InterestGrantVerificationError::CellCount);
    }
    if grant.claims.ttl_ms == 0 || grant.claims.ttl_ms > MAX_INTEREST_GRANT_TTL_MS {
        return Err(InterestGrantVerificationError::OverTtl);
    }
    Ok(grant.claims)
}

fn grant_signature_payload(claims: &InterestGrantClaimsV1) -> Result<Vec<u8>, postcard::Error> {
    let claims = postcard::to_stdvec(claims)?;
    let mut payload = Vec::with_capacity(INTEREST_GRANT_V1_DOMAIN.len() + claims.len());
    payload.extend_from_slice(INTEREST_GRANT_V1_DOMAIN);
    payload.extend_from_slice(&claims);
    Ok(payload)
}

/// The ALPN peers use to reach a coordinator.
pub const COORD_ALPN: &[u8] = b"orrery/coord/0";
/// The coordinator wire version reported in [`CoordMsg::Welcome`].
pub const COORD_PROTOCOL_VERSION: u16 = 0;
/// The most cells one presence report may carry.
///
/// Presence is the peer's active interest set, which D5 bounds at the 27-cell
/// neighbourhood. The allowance matches [`MAX_INTEREST_GRANT_CELLS`] because a
/// presence report is what a grant is minted from — a coordinator that
/// accepted more than it could sign would be storing an unusable set.
pub const MAX_PRESENCE_CELLS: usize = MAX_INTEREST_GRANT_CELLS;

/// A coordinator message (docs/10-crates.md §12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoordMsg {
    /// Peer join: authenticate and report coarse presence.
    Hello {
        /// The session token from `orrery_identity` login.
        token: Vec<u8>,
        /// The peer's NodeId.
        node: NodeId,
    },
    /// Coordinator's answer to a [`CoordMsg::Hello`] it accepted.
    Welcome {
        /// The coordinator's own NodeId, so a peer can confirm who answered.
        coordinator: NodeId,
        /// The negotiated wire version.
        protocol: u16,
    },
    /// Presence update: the cells this peer's active interest covers.
    ///
    /// This is the *input* to both island formation and interest issuance, so
    /// it carries the covered set rather than a single cell: a grant minted
    /// from one coarse cell could not authorize a claim on an entity in a
    /// finer one, because interest is matched exactly.
    Presence {
        /// The cells this peer covers, at most [`MAX_PRESENCE_CELLS`].
        cells: Vec<CellId>,
    },
    /// Island membership handout.
    IslandAssignment {
        /// The manifest.
        manifest: IslandManifest,
    },
    /// Hand a peer its signed active-interest grant.
    ///
    /// The peer forwards the opaque bytes to its gateway, which verifies the
    /// coordinator signature itself. The coordinator therefore does not need a
    /// connection to every gateway, and a gateway never has to trust a peer's
    /// word about what it is interested in.
    InterestGrant {
        /// A postcard-encoded [`InterestGrantV1`].
        grant: Vec<u8>,
    },
    /// Drain an island: leases released, cells parked.
    Drain {
        /// The island to drain.
        island: IslandId,
        /// Drain deadline as unix milliseconds.
        deadline: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    #[test]
    fn regime_thresholds() {
        assert_eq!(
            TopologyRegime::for_population(0, None),
            TopologyRegime::Mesh
        );
        assert_eq!(
            TopologyRegime::for_population(8, None),
            TopologyRegime::Mesh
        );
        assert_eq!(
            TopologyRegime::for_population(9, None),
            TopologyRegime::InterestMesh
        );
        assert_eq!(
            TopologyRegime::for_population(32, None),
            TopologyRegime::InterestMesh
        );
        assert_eq!(
            TopologyRegime::for_population(33, None),
            TopologyRegime::InterestMesh
        );
        // A promoted host overrides population.
        assert_eq!(
            TopologyRegime::for_population(4, Some(node(1))),
            TopologyRegime::Promoted { host: node(1) }
        );
    }

    #[test]
    fn manifest_roundtrips() {
        let manifest = IslandManifest {
            island: IslandId::new(7),
            epoch: 3,
            cells: vec![CellId::ROOT],
            regime: TopologyRegime::Mesh,
            peers: vec![PeerEntry {
                node: node(1),
                cells: vec![CellId::ROOT],
            }],
        };
        let bytes = postcard::to_stdvec(&manifest).unwrap();
        let back: IslandManifest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, manifest);
    }

    fn secret(seed: u8) -> iroh_base::SecretKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        iroh_base::SecretKey::from_bytes(&bytes)
    }

    fn grant_claims(peer: NodeId, cells: Vec<CellId>, ttl_ms: u64) -> InterestGrantClaimsV1 {
        InterestGrantClaimsV1::new(
            peer,
            Epoch::new(4),
            GridId::ROOT,
            cells,
            ttl_ms,
            IssuerKeyId::new(3),
        )
    }

    fn signed(claims: InterestGrantClaimsV1, key: &iroh_base::SecretKey) -> Vec<u8> {
        InterestGrantV1::sign(claims, key)
            .expect("sign grant")
            .encode()
            .expect("encode grant")
    }

    #[test]
    fn a_coordinator_signed_grant_verifies_for_the_peer_it_names() {
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let peer = node(1);
        let encoded = signed(grant_claims(peer, vec![CellId::ROOT], 30_000), &coordinator);

        let claims = verify_interest_grant(&encoded, &peer, &keys).expect("grant verifies");
        assert_eq!(claims.peer, peer);
        assert_eq!(claims.covered_cells, vec![CellId::ROOT]);
        assert_eq!(claims.ttl_ms, 30_000);
    }

    #[test]
    fn a_grant_is_useless_to_anyone_but_its_named_peer() {
        // Relaying a genuine grant for someone else is refused: interest is
        // what gates authority claims, so it is bound to one identity.
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let encoded = signed(
            grant_claims(node(1), vec![CellId::ROOT], 30_000),
            &coordinator,
        );

        assert_eq!(
            verify_interest_grant(&encoded, &node(2), &keys),
            Err(InterestGrantVerificationError::WrongPeer)
        );
    }

    #[test]
    fn only_the_configured_coordinator_key_can_author_interest() {
        // A peer minting its own grant is the attack this signature exists to
        // stop — self-declared interest would be self-granted authority.
        let impostor = secret(8);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), secret(9).public())];
        let peer = node(1);
        let encoded = signed(grant_claims(peer, vec![CellId::ROOT], 30_000), &impostor);

        assert_eq!(
            verify_interest_grant(&encoded, &peer, &keys),
            Err(InterestGrantVerificationError::BadSignature)
        );

        // An unknown key id is reported as such rather than as a bad
        // signature, so a rotation gap is diagnosable.
        let coordinator = secret(9);
        let mut claims = grant_claims(peer, vec![CellId::ROOT], 30_000);
        claims.issuer_key_id = IssuerKeyId::new(77);
        assert_eq!(
            verify_interest_grant(&signed(claims, &coordinator), &peer, &keys),
            Err(InterestGrantVerificationError::UnknownIssuer(
                IssuerKeyId::new(77)
            ))
        );
    }

    #[test]
    fn a_tampered_grant_never_verifies() {
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let peer = node(1);
        let encoded = signed(grant_claims(peer, vec![CellId::ROOT], 30_000), &coordinator);

        // Widening the covered set after signing must not survive.
        let mut grant = InterestGrantV1::decode(&encoded).expect("decode");
        grant.claims.covered_cells.push(CellId::ROOT.children()[0]);
        assert_eq!(
            verify_interest_grant(&grant.encode().unwrap(), &peer, &keys),
            Err(InterestGrantVerificationError::BadSignature)
        );

        // So must a truncated or trailing-garbage envelope.
        assert_eq!(
            verify_interest_grant(&encoded[..encoded.len() - 1], &peer, &keys),
            Err(InterestGrantVerificationError::Malformed)
        );
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            verify_interest_grant(&trailing, &peer, &keys),
            Err(InterestGrantVerificationError::Malformed)
        );
    }

    #[test]
    fn grant_bounds_reject_unusable_coverage_and_lifetimes() {
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let peer = node(1);

        // Zero cells authorizes nothing; the caller meant to send no grant.
        assert_eq!(
            verify_interest_grant(
                &signed(grant_claims(peer, vec![], 1_000), &coordinator),
                &peer,
                &keys
            ),
            Err(InterestGrantVerificationError::CellCount)
        );
        // An oversized set would turn a signed handout into an unbounded
        // membership test on the claim hot path.
        let too_many = vec![CellId::ROOT; MAX_INTEREST_GRANT_CELLS + 1];
        assert_eq!(
            verify_interest_grant(
                &signed(grant_claims(peer, too_many, 1_000), &coordinator),
                &peer,
                &keys
            ),
            Err(InterestGrantVerificationError::CellCount)
        );
        // A lifetime past the cap, or none at all, is refused rather than
        // clamped: the coordinator asked for something this build will not do.
        for ttl in [0, MAX_INTEREST_GRANT_TTL_MS + 1] {
            assert_eq!(
                verify_interest_grant(
                    &signed(grant_claims(peer, vec![CellId::ROOT], ttl), &coordinator),
                    &peer,
                    &keys
                ),
                Err(InterestGrantVerificationError::OverTtl)
            );
        }
    }

    #[test]
    fn localizing_a_grant_uses_the_accepting_clock_not_the_coordinators() {
        // The whole point of shipping a duration: the deadline is stamped in
        // the accepting process's own monotonic milliseconds.
        let claims = grant_claims(node(1), vec![CellId::ROOT], 30_000);
        let snapshot = CoordinatorInterestSnapshot::from_grant(claims.clone(), 1_000_000);
        assert_eq!(snapshot.valid_until_ms, 1_030_000);
        assert_eq!(snapshot.peer, claims.peer);
        assert_eq!(snapshot.epoch, claims.epoch);
        assert_eq!(snapshot.covered_cells, claims.covered_cells);
    }

    #[test]
    fn interest_grant_message_roundtrips() {
        let message = CoordMsg::InterestGrant {
            grant: signed(
                grant_claims(node(1), vec![CellId::ROOT], 30_000),
                &secret(9),
            ),
        };
        let bytes = postcard::to_stdvec(&message).unwrap();
        assert_eq!(postcard::from_bytes::<CoordMsg>(&bytes).unwrap(), message);
    }

    #[test]
    fn coordinator_interest_snapshot_roundtrips() {
        let snapshot = CoordinatorInterestSnapshot {
            peer: node(1),
            epoch: Epoch::new(3),
            grid: GridId::new(8),
            covered_cells: vec![CellId::ROOT],
            valid_until_ms: 4_000,
        };

        let bytes = postcard::to_stdvec(&snapshot).unwrap();
        let back: CoordinatorInterestSnapshot = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(back, snapshot);
    }

    #[test]
    fn coordinator_interest_snapshot_rejects_truncated_postcard() {
        assert!(postcard::from_bytes::<CoordinatorInterestSnapshot>(&[0]).is_err());
    }
}
