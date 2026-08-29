//! Coordinator wire surface (docs/10-crates.md §12, docs/02-networking.md §3).
//!
//! The coordinator (`orrery_coordinator`) is a Bevy-free binary; peers speak to
//! it over iroh. The message set and the island/topology types it carries are
//! defined here, engine-agnostic, so both the Bevy-free coordinator and the
//! Bevy `orrery_net` plugin share one wire surface.

use serde::{Deserialize, Serialize};

use crate::identity::{IssuerKey, IssuerKeyId, Signature};
use crate::{AccountId, CellId, Epoch, GridId, NodeId, SeqPair, Tick};

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
    /// Every peer in the island, **including the recipient**.
    ///
    /// One manifest is broadcast to everyone it names, so it cannot be relative
    /// to any one of them; a peer filters itself out on receipt
    /// (`orrery_net::IslandMembership::apply_manifest`). It is also what makes
    /// the roster self-describing: a peer can tell whether the coordinator still
    /// considers it a member without a second message.
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
/// The 27-cell neighbourhood is the baseline interest set (D5). A one-period
/// directional sweep at Regolith v18's campaign ceiling is bounded at 54
/// cells; keeping the wire allowance finite prevents a signed grant becoming
/// an unbounded membership test. The allowance remains 64 for compatibility.
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

// ---------------------------------------------------------------------------
// Witness-set seeding: the announcement envelope and the draw (D28).
//
// D10 item 4 requires a witness set to be "seeded by the coordinator per
// cell-epoch … never self-chosen", and until D28 nothing in the tree did that:
// `orrery_witness`'s `WitnessSet` is left empty and its fan-out falls back to
// the island roster, which is a peer choosing its own witnesses — a cheat
// picking its own collaborators.
//
// The envelope below is that seeding made checkable, and it is deliberately
// the *same* shape as `InterestGrantV1` above: signed claims, one canonical
// domain-separated preimage, a size- and version-bounded decode, and a
// verification error enum a caller can act on. Delivery is the same too — the
// coordinator hands the bytes to a peer, and the peer couriers them to
// whichever gateway it is talking to. There is no coordinator→gateway edge in
// this design and D28 clause (a) exists to keep it that way.

/// The ASCII prefix included in every V1 witness-epoch signature.
pub const WITNESS_EPOCH_V1_DOMAIN: &[u8] = b"orrery/witness-epoch/v1";
/// The context string for deriving a per-epoch seed key (D28 clause (c)).
///
/// Used by the coordinator's HKDF over its master secret; named here so the
/// derivation and everything that checks its output agree on one string.
pub const WITNESS_EPOCH_KEY_V1_DOMAIN: &[u8] = b"orrery/witness-epoch-key/v1";
/// The domain tag under which a seed key is committed to.
pub const WITNESS_EPOCH_COMMIT_V1_DOMAIN: &[u8] = b"orrery/witness-epoch-commit/v1";
/// The domain tag under which the shuffle seed is derived from a seed key.
pub const WITNESS_EPOCH_SEED_V1_DOMAIN: &[u8] = b"orrery/witness-epoch-seed/v1";
/// The version serialized in [`WitnessEpochClaimsV1`].
pub const WITNESS_EPOCH_V1_VERSION: u8 = 1;
/// The maximum accepted encoded announcement size before postcard decoding.
///
/// A full pool of [`MAX_EPOCH_CANDIDATES`] plus a drawn set of
/// [`WITNESS_SET_TARGET_N`] is 39 public keys — 1 248 bytes — plus two 32-byte
/// hashes and the scalar fields. The bound leaves headroom over that and
/// refuses to hand an unauthenticated buffer to an unbounded decode.
pub const MAX_WITNESS_EPOCH_BYTES: usize = 2048;
/// The most candidates one announced pool may carry.
///
/// D6 puts 32 peers at the interest-mesh ceiling, above which the island is
/// promoted to a field host, so a pool larger than that describes a population
/// the topology does not have.
pub const MAX_EPOCH_CANDIDATES: usize = 32;
/// Maximum raw account-id payload added to one witness-epoch announcement.
///
/// D34 caps the parallel account vector at 32 `u64` account ids: 256 bytes
/// before postcard's sequence framing and integer encoding. The encoded delta
/// for realistic account ids is measured by the protocol tests rather than
/// inferred from this raw-width bound.
pub const MAX_CANDIDATE_ACCOUNTS_BYTES: usize =
    MAX_EPOCH_CANDIDATES * core::mem::size_of::<AccountId>();
/// The witness set size the coordinator draws for (D16, D28 clause (c)).
///
/// 7 is `orrery_witness`'s `MAX_WITNESS_LINKS`, which is the bandwidth bound
/// D9's log fan-out already lives inside: a larger set would be a set this
/// peer cannot afford to stream to.
pub const WITNESS_SET_TARGET_N: usize = 7;
/// The smallest pool a coordinator will seed an epoch from (D16, D28).
///
/// Below this the draw is not a draw — a set of 4 that verifies as if it were
/// 7 is exactly the collusion hole K-of-N exists to close, so the coordinator
/// announces nothing and the cell takes D29's low-population path instead.
pub const WITNESS_SET_FLOOR_N: usize = 5;
/// The longest epoch length or acceptance grace a verifier accepts.
///
/// The same cap [`MAX_INTEREST_GRANT_TTL_MS`] puts on a grant, and for the
/// same reason: both quantities are durations a coordinator asks a verifier to
/// honour, and an unbounded one is an announcement that never goes stale.
pub const MAX_WITNESS_EPOCH_MS: u64 = 300_000;

/// The signature-protected body of a coordinator witness-epoch announcement.
///
/// This is the whole of what a gateway, a witness, or a later auditor is asked
/// to trust, and it is self-describing on purpose: it names the cell, so a
/// peer never gets to say which cell's set it wants to be judged against; it
/// carries the pool as well as the draw, so the draw can be recomputed once
/// the key is revealed; and it carries **durations**, never deadlines, because
/// the coordinator and the verifier are separate processes with unrelated
/// monotonic origins (the rule [`InterestGrantClaimsV1`] already follows).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessEpochClaimsV1 {
    /// Envelope version; a decoder rejects anything else.
    pub version: u8,
    /// The grid the cell belongs to.
    ///
    /// Cell ids are grid-relative (D22), so an announcement without a grid
    /// would seed two nested grids' identically numbered cells from one draw.
    pub grid: GridId,
    /// The cell whose witnesses this announcement names.
    pub cell: CellId,
    /// The per-`(grid, cell)` epoch counter, monotone.
    pub epoch: u32,
    /// The globally unique epoch handle an [`Intent`](crate::Intent)'s
    /// `cell_epoch` names.
    ///
    /// `(incarnation << 48) | counter` (D28 clause (b)): the per-cell counter
    /// above is not unique — epoch 4 exists for every cell at once — so it
    /// cannot be what an intent points at. The incarnation is the
    /// coordinator's leader-lease generation, so a failover cannot mint a
    /// colliding handle without also winning the lease.
    pub handle: u64,
    /// How long the epoch runs, from the moment the verifier accepted it.
    pub epoch_ms: u64,
    /// How long past the epoch a stale-epoch attestation is still admitted.
    ///
    /// This is docs/07 §7's reconnect grace expressed as a duration on the
    /// envelope rather than as a rule someone has to remember.
    pub accept_grace_ms: u64,
    /// The eligible pool the draw ran over, in ascending byte order.
    ///
    /// Published rather than withheld: without the pool a coordinator could
    /// claim any `selected` was drawn from a pool it invented afterwards, and
    /// the reveal would prove nothing.
    pub candidates: Vec<NodeId>,
    /// The coordinator-resolved account for each candidate at issuance.
    ///
    /// Entries are positional: `candidate_accounts[i]` owns `candidates[i]`.
    /// The vector is signed with the rest of the claims, freezing the binding
    /// view for this cell-epoch (D34).
    pub candidate_accounts: Vec<AccountId>,
    /// The drawn witness set, in draw order, a subset of `candidates`.
    pub selected: Vec<NodeId>,
    /// blake3 commitment to the epoch's secret seed key.
    pub seed_commitment: [u8; 32],
    /// The **previous** epoch's seed key, opening its commitment.
    ///
    /// `None` only for a cell's first epoch. This is the chain that makes the
    /// reveal non-optional: a coordinator cannot issue a usable epoch `e + 1`
    /// for a cell without opening `e`, so withholding a reveal costs it the
    /// cell rather than costing an auditor the proof.
    pub prev_seed_key: Option<[u8; 32]>,
    /// The coordinator key that signed this announcement, for rotation.
    pub issuer_key_id: IssuerKeyId,
}

impl WitnessEpochClaimsV1 {
    /// Build V1 claims at the current envelope version.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        grid: GridId,
        cell: CellId,
        epoch: u32,
        handle: u64,
        epoch_ms: u64,
        accept_grace_ms: u64,
        candidates: Vec<NodeId>,
        selected: Vec<NodeId>,
        seed_commitment: [u8; 32],
        prev_seed_key: Option<[u8; 32]>,
        issuer_key_id: IssuerKeyId,
    ) -> Self {
        Self {
            version: WITNESS_EPOCH_V1_VERSION,
            grid,
            cell,
            epoch,
            handle,
            epoch_ms,
            accept_grace_ms,
            candidates,
            candidate_accounts: Vec::new(),
            selected,
            seed_commitment,
            prev_seed_key,
            issuer_key_id,
        }
    }

    /// Attach the account parallel to each candidate (D34).
    ///
    /// Verification rejects a non-empty vector whose length differs from the
    /// candidate vector. This builder preserves the established constructor
    /// while the coordinator's candidate-account collection lands in its own
    /// lane.
    #[must_use]
    pub fn with_candidate_accounts(mut self, candidate_accounts: Vec<AccountId>) -> Self {
        self.candidate_accounts = candidate_accounts;
        self
    }

    /// Compose the epoch handle D28 clause (b) specifies.
    ///
    /// The counter occupies the low 48 bits; a counter that overflowed them
    /// would silently collide with the next incarnation's handles, so it is
    /// masked here and the incarnation is what a caller must roll.
    #[must_use]
    pub const fn compose_handle(incarnation: u64, counter: u64) -> u64 {
        (incarnation << 48) | (counter & 0x0000_ffff_ffff_ffff)
    }
}

/// A postcard witness-epoch envelope containing V1 claims and their signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessEpochV1 {
    /// The signature-protected V1 claims.
    pub claims: WitnessEpochClaimsV1,
    /// The coordinator's Ed25519 signature over the domain-separated claims.
    pub signature: Signature,
}

impl WitnessEpochV1 {
    /// Sign V1 claims with a coordinator's Ed25519 key.
    pub fn sign(
        claims: WitnessEpochClaimsV1,
        key: &iroh_base::SecretKey,
    ) -> Result<Self, postcard::Error> {
        let payload = witness_epoch_signature_payload(&claims)?;
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
    pub fn decode(encoded: &[u8]) -> Result<Self, WitnessEpochVerificationError> {
        if encoded.len() > MAX_WITNESS_EPOCH_BYTES {
            return Err(WitnessEpochVerificationError::Malformed);
        }
        let (announcement, remainder) = postcard::take_from_bytes::<Self>(encoded)
            .map_err(|_| WitnessEpochVerificationError::Malformed)?;
        if !remainder.is_empty() {
            return Err(WitnessEpochVerificationError::Malformed);
        }
        if announcement.claims.version != WITNESS_EPOCH_V1_VERSION {
            return Err(WitnessEpochVerificationError::Malformed);
        }
        Ok(announcement)
    }
}

/// Why a witness-epoch announcement was not accepted (D28 clause (d)).
///
/// The first five are decidable from the envelope alone and are what
/// [`verify_witness_epoch`] returns. The rest name outcomes only a stateful
/// holder can reach — a gateway with an interest authority and a cache of
/// accepted epochs — and they are spelled out here so that side has one
/// vocabulary to reject with rather than inventing a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WitnessEpochVerificationError {
    /// Oversized, undecodable, or a version this build does not accept.
    Malformed,
    /// No configured coordinator key carries the claimed identifier.
    UnknownIssuer(IssuerKeyId),
    /// The signature does not verify under the named coordinator key.
    BadSignature,
    /// The pool is empty, over [`MAX_EPOCH_CANDIDATES`], unsorted, repeats a
    /// node, or the drawn set is oversized or not a subset of it.
    BadPool,
    /// An epoch length or acceptance grace outside `(0, MAX_WITNESS_EPOCH_MS]`.
    OverTtl,
    /// The presenter's interest does not cover this cell right now.
    ///
    /// An announcement names a cell, not a peer, so `WrongPeer` has no
    /// analogue here — but an unrestricted presenter could stuff a gateway's
    /// cache with epochs for cells it has nothing to do with.
    NotCovered,
    /// A higher epoch is already on file for this cell, or this handle is on
    /// file with different claims.
    Superseded,
    /// The revealed previous-epoch key does not open the stored commitment.
    BadReveal,
    /// The announced set is not what the revealed key draws from the pool.
    ///
    /// D28's step list does not name this one because it is the *fairness*
    /// tier's verdict rather than the live tier's: it can only be reached one
    /// epoch later, by an auditor holding the revealed key, and it is the
    /// finding that says the coordinator hand-picked rather than drew.
    BadDraw,
    /// This verifier is not configured to accept witness-epoch announcements.
    Unsupported,
}

impl core::fmt::Display for WitnessEpochVerificationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed => f.write_str("witness epoch is malformed"),
            Self::UnknownIssuer(id) => write!(f, "unknown coordinator key {}", id.0),
            Self::BadSignature => f.write_str("witness epoch signature does not verify"),
            Self::BadPool => f.write_str("witness epoch pool or selection is unusable"),
            Self::OverTtl => f.write_str("witness epoch window is too long"),
            Self::NotCovered => f.write_str("presenter has no interest in the announced cell"),
            Self::Superseded => f.write_str("a newer witness epoch is already on file"),
            Self::BadReveal => f.write_str("revealed seed key does not open the commitment"),
            Self::BadDraw => f.write_str("announced witness set is not the drawn one"),
            Self::Unsupported => f.write_str("witness epochs are not accepted here"),
        }
    }
}

impl core::error::Error for WitnessEpochVerificationError {}

/// Verify a presented announcement against the configured coordinator keys.
///
/// This is D28 clause (d)'s steps 1–5 — everything decidable from the envelope
/// and the key set alone, which is exactly the part that needs no live state
/// and no secret. Steps 6–8 (`NotCovered`, `Superseded`, `BadReveal`) are the
/// holder's: they need an interest authority and a cache of accepted epochs,
/// so they belong to the gateway that keeps those, and the error vocabulary
/// above is shared so its answers read the same as these.
pub fn verify_witness_epoch(
    encoded: &[u8],
    keys: &[IssuerKey],
) -> Result<WitnessEpochClaimsV1, WitnessEpochVerificationError> {
    let announcement = WitnessEpochV1::decode(encoded)?;
    let key = keys
        .iter()
        .find(|key| key.key_id == announcement.claims.issuer_key_id)
        .ok_or(WitnessEpochVerificationError::UnknownIssuer(
            announcement.claims.issuer_key_id,
        ))?;
    let payload = witness_epoch_signature_payload(&announcement.claims)
        .map_err(|_| WitnessEpochVerificationError::Malformed)?;
    key.public_key
        .verify(&payload, &announcement.signature)
        .map_err(|_| WitnessEpochVerificationError::BadSignature)?;
    check_witness_epoch_pool(&announcement.claims)?;
    for duration in [
        announcement.claims.epoch_ms,
        announcement.claims.accept_grace_ms,
    ] {
        if duration == 0 || duration > MAX_WITNESS_EPOCH_MS {
            return Err(WitnessEpochVerificationError::OverTtl);
        }
    }
    Ok(announcement.claims)
}

/// The pool and selection bounds, separated so the seeding side can apply the
/// same test to claims it is about to sign.
fn check_witness_epoch_pool(
    claims: &WitnessEpochClaimsV1,
) -> Result<(), WitnessEpochVerificationError> {
    if claims.candidates.is_empty() || claims.candidates.len() > MAX_EPOCH_CANDIDATES {
        return Err(WitnessEpochVerificationError::BadPool);
    }
    if (!claims.candidate_accounts.is_empty()
        && claims.candidate_accounts.len() != claims.candidates.len())
        || claims.candidate_accounts.len() > MAX_EPOCH_CANDIDATES
    {
        return Err(WitnessEpochVerificationError::BadPool);
    }
    // Strictly ascending, which is both the canonical order the draw runs in
    // and — for free — a duplicate check: a pool listing one node twice would
    // give it two chances at the same draw.
    if claims
        .candidates
        .windows(2)
        .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
    {
        return Err(WitnessEpochVerificationError::BadPool);
    }
    if claims.selected.is_empty() || claims.selected.len() > WITNESS_SET_TARGET_N {
        return Err(WitnessEpochVerificationError::BadPool);
    }
    for (index, node) in claims.selected.iter().enumerate() {
        if !claims.candidates.contains(node) {
            return Err(WitnessEpochVerificationError::BadPool);
        }
        if claims.selected[..index].contains(node) {
            return Err(WitnessEpochVerificationError::BadPool);
        }
    }
    Ok(())
}

/// The canonical `grid ‖ cell ‖ epoch` binding every witness-epoch derivation
/// mixes in.
///
/// Big-endian and fixed-width on purpose: this goes through HKDF, HMAC and
/// blake3 rather than through postcard, so it has no self-describing framing
/// and a varint encoding would let two different `(cell, epoch)` pairs
/// serialize to the same bytes.
#[must_use]
pub fn witness_epoch_binding(grid: GridId, cell: CellId, epoch: u32) -> [u8; 16] {
    let mut binding = [0u8; 16];
    binding[..4].copy_from_slice(&grid.0.to_be_bytes());
    binding[4..12].copy_from_slice(&cell.to_bits().to_be_bytes());
    binding[12..].copy_from_slice(&epoch.to_be_bytes());
    binding
}

/// The commitment published in an announcement for its own seed key.
///
/// `blake3(DOMAIN ‖ grid ‖ cell ‖ epoch ‖ k_e)`. The binding is inside the
/// hash so a key revealed for one cell-epoch cannot be replayed as the opening
/// of another.
#[must_use]
pub fn witness_epoch_commitment(
    grid: GridId,
    cell: CellId,
    epoch: u32,
    seed_key: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(WITNESS_EPOCH_COMMIT_V1_DOMAIN);
    hasher.update(&witness_epoch_binding(grid, cell, epoch));
    hasher.update(seed_key);
    *hasher.finalize().as_bytes()
}

/// The domain tag under which a gateway's per-cell-epoch `draw_key` is
/// committed to (D27 clause (d)).
///
/// Distinct from [`WITNESS_EPOCH_COMMIT_V1_DOMAIN`] because the two keys are
/// *different secrets held by different processes* and only ever look alike:
/// `k_epoch` is the coordinator's and seeds the set-selection shuffle;
/// `draw_key` is the gateway's and seeds the per-intent required-K draw.
/// Neither ever crosses to the other's holder, and a shared tag would make a
/// commitment to one openable as a commitment to the other.
pub const ATTESTATION_DRAW_COMMIT_V1_DOMAIN: &[u8] = b"orrery/attestation-draw-commit/v1";

/// The commitment a gateway publishes for a cell-epoch's `draw_key`
/// (D27 clause (d)).
///
/// `blake3(DOMAIN ‖ grid ‖ cell ‖ epoch ‖ d)`, the shape
/// [`witness_epoch_commitment`] uses, for the same reason: the binding is
/// inside the hash, so a key revealed for one cell-epoch cannot be replayed as
/// the opening of another's commitment.
///
/// **D27 writes the binding as `c ‖ e` and this adds the grid.** That is D28
/// clause (f)'s correction applied here too — cell ids are grid-relative
/// ([D22][d22]), so a binding without a grid would let two nested grids'
/// identically numbered cells share one commitment.
///
/// The ordering rule this exists to serve is the one that makes a retrospective
/// audit non-vacuous: the commitment must be durable before any intent in the
/// cell-epoch is admitted, so the gateway cannot choose `d` after seeing which
/// attestations arrived.
///
/// [d22]: https://github.com/baadc0de/orrery/blob/main/docs/adr/0022-grid-id-in-the-storage-key.md
#[must_use]
pub fn attestation_draw_commitment(
    grid: GridId,
    cell: CellId,
    epoch: u32,
    draw_key: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ATTESTATION_DRAW_COMMIT_V1_DOMAIN);
    hasher.update(&witness_epoch_binding(grid, cell, epoch));
    hasher.update(draw_key);
    *hasher.finalize().as_bytes()
}

/// The ChaCha20 seed the draw runs under.
///
/// `HMAC-SHA256(k_e, DOMAIN ‖ grid ‖ cell ‖ epoch)` — the seed key is the MAC
/// key rather than part of the message, so the seed is unpredictable to anyone
/// who has not been given the key, which is what keeps the draw unpredictable
/// until the reveal.
#[must_use]
pub fn witness_epoch_seed(seed_key: &[u8; 32], grid: GridId, cell: CellId, epoch: u32) -> [u8; 32] {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(seed_key)
        .expect("HMAC-SHA256 accepts a key of any length");
    mac.update(WITNESS_EPOCH_SEED_V1_DOMAIN);
    mac.update(&witness_epoch_binding(grid, cell, epoch));
    mac.finalize().into_bytes().into()
}

/// Draw a witness set from a candidate pool under a seed (D28 clause (c)).
///
/// The pool is sorted bytewise first, so the draw is a function of the *set* of
/// candidates and not of the order the caller happened to collect them in —
/// the same technique `orrery_witness::witness_links` uses for its fan-out, and
/// for the same reason: an unsorted input would silently change who witnesses
/// whenever a roster reordered. The shuffle is the standard downward
/// Fisher–Yates, and the first [`WITNESS_SET_TARGET_N`] are taken.
///
/// Determinism is the whole point. Every quantity here is fixed-width and
/// endian-explicit and the RNG is ChaCha20, so the same pool and seed produce
/// the same set on every platform — the obligation `orrery_core`'s determinism
/// rules carry, applied to a security-relevant draw.
#[must_use]
pub fn draw_witness_set(candidates: &[NodeId], seed: &[u8; 32]) -> Vec<NodeId> {
    use rand_chacha::rand_core::SeedableRng;

    let mut pool: Vec<NodeId> = candidates.to_vec();
    pool.sort_by_key(|node| *node.as_bytes());
    pool.dedup();
    let mut rng = rand_chacha::ChaCha20Rng::from_seed(*seed);
    for index in (1..pool.len()).rev() {
        let swap = bounded_index(&mut rng, index + 1);
        pool.swap(index, swap);
    }
    pool.truncate(WITNESS_SET_TARGET_N);
    pool
}

/// A uniform index in `0..bound`, by rejection sampling.
///
/// Written out rather than taken from `rand`'s distribution machinery because
/// the value drawn here has to be reproducible by an auditor forever: a
/// distribution whose internals change between releases would silently make
/// old announcements unauditable. Rejection over the largest multiple of
/// `bound` is unbiased and needs nothing but `next_u32`.
fn bounded_index(rng: &mut rand_chacha::ChaCha20Rng, bound: usize) -> usize {
    use rand_chacha::rand_core::RngCore;

    debug_assert!(bound > 0);
    let bound = u32::try_from(bound).unwrap_or(u32::MAX);
    let limit = u32::MAX - (u32::MAX % bound);
    loop {
        let value = rng.next_u32();
        if value < limit {
            return (value % bound) as usize;
        }
    }
}

/// Check that a revealed key opens an epoch's commitment (D28 clause (c)).
///
/// This is the live half of the chained reveal: a holder of `A_e` that is
/// offered `A_{e+1}` runs this over `A_{e+1}.prev_seed_key` and refuses the
/// successor if it does not open what `A_e` committed to.
pub fn verify_witness_epoch_reveal(
    revealed: &WitnessEpochClaimsV1,
    seed_key: &[u8; 32],
) -> Result<(), WitnessEpochVerificationError> {
    if witness_epoch_commitment(revealed.grid, revealed.cell, revealed.epoch, seed_key)
        == revealed.seed_commitment
    {
        Ok(())
    } else {
        Err(WitnessEpochVerificationError::BadReveal)
    }
}

/// Recompute an epoch's draw from its own pool and its revealed key.
///
/// The fairness tier in full (D28 clause (c)): the key must open the
/// commitment, *and* the announced set must be exactly what that key draws
/// from the announced pool. Both halves are needed — a coordinator that
/// revealed a real key while announcing a hand-picked set would pass the first
/// on its own.
///
/// A third party can run this with nothing but the two envelopes: `A_e` for
/// the pool and the commitment, `A_{e+1}` for the key.
pub fn audit_witness_epoch_draw(
    claims: &WitnessEpochClaimsV1,
    seed_key: &[u8; 32],
) -> Result<(), WitnessEpochVerificationError> {
    verify_witness_epoch_reveal(claims, seed_key)?;
    let seed = witness_epoch_seed(seed_key, claims.grid, claims.cell, claims.epoch);
    if draw_witness_set(&claims.candidates, &seed) == claims.selected {
        Ok(())
    } else {
        Err(WitnessEpochVerificationError::BadDraw)
    }
}

/// A verifier's localized view of an accepted witness epoch.
///
/// The envelope carries durations; this is what one becomes once a process
/// with its own clock has accepted it — the same relationship
/// [`CoordinatorInterestSnapshot`] has to an [`InterestGrantV1`], and it exists
/// for the same reason: the accepting process is the only one whose clock the
/// window can be expressed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessEpochSnapshot {
    /// The grid the cell belongs to.
    pub grid: GridId,
    /// The cell this epoch's set witnesses.
    pub cell: CellId,
    /// The per-cell epoch counter.
    pub epoch: u32,
    /// The handle an intent's `cell_epoch` names.
    pub handle: u64,
    /// The announced witness set, in draw order.
    pub selected: Vec<NodeId>,
    /// The commitment to this epoch's seed key, kept so the next
    /// announcement's reveal can be checked against it.
    pub seed_commitment: [u8; 32],
    /// When the holding process accepted the announcement, on its own clock.
    pub first_seen_ms: u64,
    /// The instant past which an intent naming this epoch is stale, on the
    /// holding process's clock.
    pub usable_until_ms: u64,
}

impl WitnessEpochSnapshot {
    /// Localize verified claims against the accepting process's clock.
    #[must_use]
    pub fn from_claims(claims: WitnessEpochClaimsV1, accepted_at_ms: u64) -> Self {
        Self {
            grid: claims.grid,
            cell: claims.cell,
            epoch: claims.epoch,
            handle: claims.handle,
            selected: claims.selected,
            seed_commitment: claims.seed_commitment,
            first_seen_ms: accepted_at_ms,
            usable_until_ms: accepted_at_ms
                .saturating_add(claims.epoch_ms)
                .saturating_add(claims.accept_grace_ms),
        }
    }

    /// Whether an intent arriving at `now_ms` may still be judged against this
    /// epoch (D28 clause (g)).
    ///
    /// Note what this is *not*: it is not a question about the current epoch.
    /// The arrival of a newer announcement does nothing to this window, which
    /// is what lets an attestation collected just before a boundary commit
    /// just after it.
    #[must_use]
    pub fn usable_at(&self, now_ms: u64) -> bool {
        now_ms >= self.first_seen_ms && now_ms < self.usable_until_ms
    }

    /// Whether `node` is in this epoch's announced set.
    ///
    /// Membership is judged against the announcement, never against who is in
    /// the cell now: a witness that left one second after signing is still a
    /// valid signer for the epoch it signed under.
    #[must_use]
    pub fn admits(&self, node: &NodeId) -> bool {
        self.selected.contains(node)
    }
}

fn witness_epoch_signature_payload(
    claims: &WitnessEpochClaimsV1,
) -> Result<Vec<u8>, postcard::Error> {
    let claims = postcard::to_stdvec(claims)?;
    let mut payload = Vec::with_capacity(WITNESS_EPOCH_V1_DOMAIN.len() + claims.len());
    payload.extend_from_slice(WITNESS_EPOCH_V1_DOMAIN);
    payload.extend_from_slice(&claims);
    Ok(payload)
}

/// The ALPN peers use to reach a coordinator.
pub const COORD_ALPN: &[u8] = b"orrery/coord/0";
/// The coordinator wire version reported in [`CoordMsg::Welcome`].
pub const COORD_PROTOCOL_VERSION: u16 = 0;
/// The most cells one presence report may carry.
///
/// Presence is the peer's active interest set: D5's 27-cell baseline plus the
/// bounded directional sweep. The allowance matches
/// [`MAX_INTEREST_GRANT_CELLS`] because a presence report is what a grant is
/// minted from — a coordinator that accepted more than it could sign would be
/// storing an unusable set.
pub const MAX_PRESENCE_CELLS: usize = MAX_INTEREST_GRANT_CELLS;

/// An immediate committed-cell crossing and the interest coverage after it.
///
/// Bulk [`CoordMsg::Presence`] reports remain the low-rate source of complete
/// state. This event closes the interval between them: as soon as hysteresis
/// commits a different cell, the peer sends the new swept coverage and the
/// coordinator can replace the grant and island roster immediately. The swept
/// margin remains necessary because this event is reactive and spends a
/// network round trip after the boundary was crossed; the event remains
/// necessary because prediction is only a bound and must eventually converge
/// on the actual crossing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterestCellCrossing {
    /// Simulation tick at which the authority committed the crossing.
    pub tick: Tick,
    /// Entity authority order at which the crossing was committed.
    pub seq: SeqPair,
    /// The peer's previously committed interest cell.
    pub from: CellId,
    /// The newly committed interest cell.
    pub to: CellId,
    /// The complete swept interest coverage after the crossing.
    pub covered_cells: Vec<CellId>,
}

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
    /// Hand a peer the witness set for a cell it covers (D28 clause (a)).
    ///
    /// The same courier model as [`CoordMsg::InterestGrant`], one step
    /// stronger: the peer forwards the opaque bytes to its gateway, which
    /// verifies the coordinator signature itself, so there is no
    /// coordinator→gateway connection and a gateway never has to trust a
    /// peer's word about who is witnessing. The announcement names a *cell*
    /// rather than the recipient, so every peer covering that cell receives
    /// the same bytes and any of them can be the courier.
    ///
    /// Appended last deliberately: postcard keys variants positionally, so a
    /// new variant at the end is invisible to a decoder that never sees one.
    WitnessEpoch {
        /// A postcard-encoded [`WitnessEpochV1`].
        announcement: Vec<u8>,
    },
    /// Immediate committed-cell crossing.
    ///
    /// Appended after every older variant because postcard keys variants
    /// positionally. Hosts emit this on the crossing edge; the ordinary
    /// [`CoordMsg::Presence`] cadence still republishes the complete state.
    InterestCellCrossing {
        /// The crossing and the full post-crossing swept coverage.
        crossing: InterestCellCrossing,
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

    // -- witness-set seeding (D28) ------------------------------------------

    /// A pool of `count` distinct nodes, in the arbitrary order a caller might
    /// have collected them — never pre-sorted, because sorting is the draw's
    /// job and a pre-sorted fixture would hide it if it stopped happening.
    fn pool(count: u8) -> Vec<NodeId> {
        (1..=count).rev().map(node).collect()
    }

    fn ascending(mut nodes: Vec<NodeId>) -> Vec<NodeId> {
        nodes.sort_by_key(|node| *node.as_bytes());
        nodes
    }

    fn epoch_claims(
        seed_key: &[u8; 32],
        epoch: u32,
        candidates: Vec<NodeId>,
    ) -> WitnessEpochClaimsV1 {
        let cell = CellId::ROOT;
        let grid = GridId::ROOT;
        let seed = witness_epoch_seed(seed_key, grid, cell, epoch);
        let selected = draw_witness_set(&candidates, &seed);
        let candidates = ascending(candidates);
        let candidate_accounts = (0..candidates.len())
            .map(|index| AccountId::new(10_000_000 + index as u64))
            .collect();
        WitnessEpochClaimsV1::new(
            grid,
            cell,
            epoch,
            WitnessEpochClaimsV1::compose_handle(1, u64::from(epoch)),
            30_000,
            30_000,
            candidates,
            selected,
            witness_epoch_commitment(grid, cell, epoch, seed_key),
            None,
            IssuerKeyId::new(3),
        )
        .with_candidate_accounts(candidate_accounts)
    }

    fn signed_epoch(claims: WitnessEpochClaimsV1, key: &iroh_base::SecretKey) -> Vec<u8> {
        WitnessEpochV1::sign(claims, key)
            .expect("sign announcement")
            .encode()
            .expect("encode announcement")
    }

    #[test]
    fn an_announcement_verifies_under_the_coordinator_key_that_signed_it() {
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let claims = epoch_claims(&[7u8; 32], 4, pool(12));
        let encoded = signed_epoch(claims.clone(), &coordinator);

        let verified = verify_witness_epoch(&encoded, &keys).expect("announcement verifies");
        assert_eq!(verified, claims);
        assert_eq!(verified.selected.len(), WITNESS_SET_TARGET_N);

        // A peer minting its own set is the whole attack this signature
        // exists to stop: a cheat that picks its witnesses picks its friends.
        let impostor = secret(8);
        assert_eq!(
            verify_witness_epoch(&signed_epoch(claims.clone(), &impostor), &keys),
            Err(WitnessEpochVerificationError::BadSignature)
        );
        // An unknown key id is diagnosable as a rotation gap rather than as a
        // forgery.
        let mut rotated = claims;
        rotated.issuer_key_id = IssuerKeyId::new(77);
        assert_eq!(
            verify_witness_epoch(&signed_epoch(rotated, &coordinator), &keys),
            Err(WitnessEpochVerificationError::UnknownIssuer(
                IssuerKeyId::new(77)
            ))
        );
    }

    #[test]
    fn every_announced_field_is_inside_the_signature() {
        // Field by field, because "the signature covers the claims" is only
        // true if the preimage is the *whole* postcard body — a claims struct
        // that grew a field outside the preimage would still pass a blanket
        // "flip one byte" test.
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let original = epoch_claims(&[7u8; 32], 4, pool(12));
        let encoded = signed_epoch(original.clone(), &coordinator);
        assert!(verify_witness_epoch(&encoded, &keys).is_ok());

        let tampered: Vec<(&str, WitnessEpochClaimsV1)> = vec![
            ("grid", {
                let mut c = original.clone();
                c.grid = GridId::new(7);
                c
            }),
            ("cell", {
                let mut c = original.clone();
                c.cell = CellId::ROOT.children()[0];
                c
            }),
            ("epoch", {
                let mut c = original.clone();
                c.epoch += 1;
                c
            }),
            ("handle", {
                let mut c = original.clone();
                c.handle ^= 1;
                c
            }),
            ("epoch_ms", {
                let mut c = original.clone();
                c.epoch_ms += 1;
                c
            }),
            ("accept_grace_ms", {
                let mut c = original.clone();
                c.accept_grace_ms += 1;
                c
            }),
            ("seed_commitment", {
                let mut c = original.clone();
                c.seed_commitment[0] ^= 0xff;
                c
            }),
            ("prev_seed_key", {
                let mut c = original.clone();
                c.prev_seed_key = Some([9u8; 32]);
                c
            }),
            ("selected", {
                // The one that matters most: swapping a drawn witness for
                // another pool member is exactly the hand-pick the draw
                // exists to prevent.
                let mut c = original.clone();
                let outsider = c
                    .candidates
                    .iter()
                    .find(|node| !c.selected.contains(node))
                    .copied()
                    .expect("a pool of 12 has members outside a set of 7");
                c.selected[0] = outsider;
                c
            }),
            ("candidates", {
                let mut c = original.clone();
                c.candidates = ascending(pool(11));
                c
            }),
            ("candidate_accounts", {
                let mut c = original.clone();
                c.candidate_accounts[0] = AccountId::new(99_000_001);
                c
            }),
        ];

        for (field, claims) in tampered {
            // Re-encoding without re-signing: the signature travels with the
            // claims, so an edit is only ever caught by the signature.
            let mut envelope = WitnessEpochV1::decode(&encoded).expect("decode");
            envelope.claims = claims;
            assert_eq!(
                verify_witness_epoch(&envelope.encode().unwrap(), &keys),
                Err(WitnessEpochVerificationError::BadSignature),
                "tampering with {field} survived verification"
            );
        }
    }

    #[test]
    fn a_truncated_or_oversize_announcement_is_malformed_not_a_panic() {
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let encoded = signed_epoch(epoch_claims(&[7u8; 32], 4, pool(12)), &coordinator);

        assert_eq!(
            verify_witness_epoch(&encoded[..encoded.len() - 1], &keys),
            Err(WitnessEpochVerificationError::Malformed)
        );
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            verify_witness_epoch(&trailing, &keys),
            Err(WitnessEpochVerificationError::Malformed)
        );
        assert_eq!(
            verify_witness_epoch(&vec![0u8; MAX_WITNESS_EPOCH_BYTES + 1], &keys),
            Err(WitnessEpochVerificationError::Malformed)
        );
        assert_eq!(
            verify_witness_epoch(&[], &keys),
            Err(WitnessEpochVerificationError::Malformed)
        );
        // A version this build does not accept is refused before anything in
        // the body is believed.
        let mut wrong_version = WitnessEpochV1::decode(&encoded).expect("decode");
        wrong_version.claims.version = 2;
        assert_eq!(
            verify_witness_epoch(&wrong_version.encode().unwrap(), &keys),
            Err(WitnessEpochVerificationError::Malformed)
        );
    }

    #[test]
    fn pool_and_window_bounds_are_refused_rather_than_clamped() {
        let coordinator = secret(9);
        let keys = [IssuerKey::new(IssuerKeyId::new(3), coordinator.public())];
        let base = epoch_claims(&[7u8; 32], 4, pool(12));

        let bad_pools: Vec<(&str, WitnessEpochClaimsV1)> = vec![
            ("empty pool", {
                let mut c = base.clone();
                c.candidates = Vec::new();
                c
            }),
            ("pool over the interest-mesh ceiling", {
                let mut c = base.clone();
                c.candidates = ascending(pool(MAX_EPOCH_CANDIDATES as u8 + 1));
                c.selected = c.candidates[..WITNESS_SET_TARGET_N].to_vec();
                c
            }),
            ("unsorted pool", {
                let mut c = base.clone();
                c.candidates.swap(0, 1);
                c
            }),
            ("repeated candidate", {
                let mut c = base.clone();
                c.candidates[1] = c.candidates[0];
                c
            }),
            ("account vector shorter than the pool", {
                let mut c = base.clone();
                c.candidate_accounts.pop();
                c
            }),
            ("account vector over the interest-mesh ceiling", {
                let mut c = base.clone();
                c.candidate_accounts = vec![AccountId::new(1); MAX_EPOCH_CANDIDATES + 1];
                c
            }),
            ("selection outside the pool", {
                let mut c = base.clone();
                c.selected[0] = node(200);
                c
            }),
            ("selection over the target", {
                let mut c = base.clone();
                c.selected = c.candidates[..WITNESS_SET_TARGET_N + 1].to_vec();
                c
            }),
            ("repeated selection", {
                let mut c = base.clone();
                c.selected[1] = c.selected[0];
                c
            }),
            ("empty selection", {
                let mut c = base.clone();
                c.selected = Vec::new();
                c
            }),
        ];
        for (what, claims) in bad_pools {
            assert_eq!(
                verify_witness_epoch(&signed_epoch(claims, &coordinator), &keys),
                Err(WitnessEpochVerificationError::BadPool),
                "{what} was accepted"
            );
        }

        for window in [0, MAX_WITNESS_EPOCH_MS + 1] {
            let mut epoch_ms = base.clone();
            epoch_ms.epoch_ms = window;
            assert_eq!(
                verify_witness_epoch(&signed_epoch(epoch_ms, &coordinator), &keys),
                Err(WitnessEpochVerificationError::OverTtl)
            );
            let mut grace = base.clone();
            grace.accept_grace_ms = window;
            assert_eq!(
                verify_witness_epoch(&signed_epoch(grace, &coordinator), &keys),
                Err(WitnessEpochVerificationError::OverTtl)
            );
        }
    }

    #[test]
    fn the_draw_is_a_function_of_the_key_and_the_pool_as_a_set() {
        let candidates = pool(12);
        let seed = witness_epoch_seed(&[7u8; 32], GridId::ROOT, CellId::ROOT, 4);
        let drawn = draw_witness_set(&candidates, &seed);

        // Deterministic: the same key and pool, twice.
        assert_eq!(draw_witness_set(&candidates, &seed), drawn);

        // Order-independent: the pool is a *set*, and the bytewise sort is
        // what makes that true. A draw that moved when a roster reordered
        // would be a draw an attacker could steer by reconnecting.
        let mut reordered = candidates.clone();
        reordered.rotate_left(5);
        reordered.swap(0, 3);
        assert_eq!(draw_witness_set(&reordered, &seed), drawn);

        // Key-dependent: the same pool under a different key is a different
        // set, which is what makes the secret load-bearing.
        let other = witness_epoch_seed(&[8u8; 32], GridId::ROOT, CellId::ROOT, 4);
        assert_ne!(draw_witness_set(&candidates, &other), drawn);

        // And so is the epoch: the binding is inside the seed, so epoch 5
        // draws independently of epoch 4 even under the same key.
        let next = witness_epoch_seed(&[7u8; 32], GridId::ROOT, CellId::ROOT, 5);
        assert_ne!(draw_witness_set(&candidates, &next), drawn);

        // A pool at or below the target is taken whole — there is nothing to
        // choose between, and the draw must not invent members.
        let small = pool(WITNESS_SET_TARGET_N as u8);
        assert_eq!(
            ascending(draw_witness_set(&small, &seed)),
            ascending(small.clone())
        );
        assert_eq!(drawn.len(), WITNESS_SET_TARGET_N);
    }

    #[test]
    fn the_binding_separates_cells_and_grids_that_share_a_number() {
        // The reason the binding is inside every derivation: two grids' cell
        // 1 must not share a seed, a commitment, or a set.
        let key = [7u8; 32];
        let root = witness_epoch_binding(GridId::ROOT, CellId::ROOT, 4);
        let nested = witness_epoch_binding(GridId::new(1), CellId::ROOT, 4);
        assert_ne!(root, nested);
        assert_ne!(
            witness_epoch_seed(&key, GridId::ROOT, CellId::ROOT, 4),
            witness_epoch_seed(&key, GridId::new(1), CellId::ROOT, 4)
        );
        assert_ne!(
            witness_epoch_commitment(GridId::ROOT, CellId::ROOT, 4, &key),
            witness_epoch_commitment(GridId::new(1), CellId::ROOT, 4, &key)
        );
        // A key revealed for one cell-epoch does not open another's.
        let claims = epoch_claims(&key, 4, pool(12));
        let mut elsewhere = claims.clone();
        elsewhere.epoch = 5;
        assert!(verify_witness_epoch_reveal(&claims, &key).is_ok());
        assert_eq!(
            verify_witness_epoch_reveal(&elsewhere, &key),
            Err(WitnessEpochVerificationError::BadReveal)
        );
    }

    #[test]
    fn the_fairness_tier_catches_a_hand_picked_set() {
        // The revealed key alone is not enough: a coordinator could reveal a
        // genuine key and still have announced a set it chose. Recomputing
        // the draw is what closes that, and it is the claim D28 clause (c)
        // rests on.
        let key = [7u8; 32];
        let honest = epoch_claims(&key, 4, pool(12));
        assert!(audit_witness_epoch_draw(&honest, &key).is_ok());

        let mut hand_picked = honest.clone();
        let outsider = hand_picked
            .candidates
            .iter()
            .find(|node| !hand_picked.selected.contains(node))
            .copied()
            .expect("a pool of 12 has members outside a set of 7");
        hand_picked.selected[0] = outsider;
        assert_eq!(
            audit_witness_epoch_draw(&hand_picked, &key),
            Err(WitnessEpochVerificationError::BadDraw)
        );

        // A wrong key is reported as a failed opening, not as a bad draw:
        // "the coordinator will not open its commitment" and "the coordinator
        // did not draw what it announced" are different findings.
        assert_eq!(
            audit_witness_epoch_draw(&honest, &[8u8; 32]),
            Err(WitnessEpochVerificationError::BadReveal)
        );
    }

    #[test]
    fn an_epoch_window_is_measured_on_the_accepting_clock() {
        // Durations on the wire, deadlines locally — the same rule a grant
        // follows, and here it is also what makes an in-flight attestation
        // survive a boundary (D28 clause (g)).
        let claims = epoch_claims(&[7u8; 32], 4, pool(12));
        let selected = claims.selected.clone();
        let snapshot = WitnessEpochSnapshot::from_claims(claims, 1_000_000);

        assert_eq!(snapshot.first_seen_ms, 1_000_000);
        assert_eq!(snapshot.usable_until_ms, 1_060_000);
        assert!(snapshot.usable_at(1_000_000));
        assert!(snapshot.usable_at(1_059_999));
        assert!(!snapshot.usable_at(1_060_000));
        assert!(!snapshot.usable_at(999_999));
        assert!(snapshot.admits(&selected[0]));
        assert!(!snapshot.admits(&node(200)));
    }

    #[test]
    fn witness_epoch_message_roundtrips() {
        let message = CoordMsg::WitnessEpoch {
            announcement: signed_epoch(epoch_claims(&[7u8; 32], 4, pool(12)), &secret(9)),
        };
        let bytes = postcard::to_stdvec(&message).unwrap();
        assert_eq!(postcard::from_bytes::<CoordMsg>(&bytes).unwrap(), message);
    }

    #[test]
    fn interest_cell_crossing_message_roundtrips() {
        let from = CellId::from_coords(glam::IVec3::ZERO, CellId::MAX_LEVEL).unwrap();
        let to = CellId::from_coords(glam::IVec3::X, CellId::MAX_LEVEL).unwrap();
        let message = CoordMsg::InterestCellCrossing {
            crossing: InterestCellCrossing {
                tick: Tick(7),
                seq: SeqPair {
                    own_seq: 3,
                    auth_seq: 11,
                },
                from,
                to,
                covered_cells: to.neighbors27(),
            },
        };
        let bytes = postcard::to_stdvec(&message).unwrap();
        assert_eq!(postcard::from_bytes::<CoordMsg>(&bytes).unwrap(), message);
    }

    #[test]
    fn a_full_announcement_fits_the_wire_bound() {
        // The bound is only meaningful if the largest legitimate announcement
        // is under it: a cap that refused a full 32-peer pool would be a cap
        // against the coordinator rather than against an attacker.
        let encoded = signed_epoch(
            epoch_claims(&[7u8; 32], u32::MAX, pool(MAX_EPOCH_CANDIDATES as u8)),
            &secret(9),
        );
        assert!(
            encoded.len() <= MAX_WITNESS_EPOCH_BYTES,
            "a full pool encodes to {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn realistic_candidate_accounts_add_at_most_256_bytes() {
        // These are signed, fully populated announcements at the pool sizes
        // supported by D28: the five-candidate floor through the 32-peer
        // interest-mesh ceiling. Ten-million-range ids model a populated
        // account table without relying on postcard's one-byte small-id case.
        for pool_size in [5u8, 7, 8, 16, 24, MAX_EPOCH_CANDIDATES as u8] {
            let with_accounts = epoch_claims(&[7u8; 32], u32::MAX, pool(pool_size));
            let mut without_accounts = with_accounts.clone();
            without_accounts.candidate_accounts.clear();

            let encoded_with = signed_epoch(with_accounts, &secret(9));
            let encoded_without = signed_epoch(without_accounts, &secret(9));
            let added = encoded_with.len() - encoded_without.len();
            eprintln!(
                "candidate pool {pool_size:>2}: announcement {} B, candidate_accounts +{added} B",
                encoded_with.len()
            );
            assert!(
                added <= MAX_CANDIDATE_ACCOUNTS_BYTES,
                "{pool_size} realistic candidate accounts added {added} bytes"
            );
        }
    }

    #[test]
    fn the_handle_packs_an_incarnation_and_a_counter() {
        // Clause (b): the per-cell counter is not unique across cells, so the
        // handle an intent names is incarnation-scoped.
        assert_eq!(WitnessEpochClaimsV1::compose_handle(0, 1), 1);
        assert_eq!(
            WitnessEpochClaimsV1::compose_handle(1, 0),
            1u64 << 48,
            "a failover's first handle is above every handle the last one issued"
        );
        assert_ne!(
            WitnessEpochClaimsV1::compose_handle(1, 5),
            WitnessEpochClaimsV1::compose_handle(2, 5)
        );
        // A counter past 2^48 is masked rather than allowed to bleed into the
        // incarnation, which would silently collide with the next leader.
        assert_eq!(WitnessEpochClaimsV1::compose_handle(1, 1 << 48), 1u64 << 48);
    }
}
