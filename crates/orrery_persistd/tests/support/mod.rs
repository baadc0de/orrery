#![allow(
    dead_code,
    reason = "each integration-test binary uses a different subset of the shared fixture"
)]

use std::sync::Arc;

use orrery_persistd::gateway::{
    GatewayAuthorizer, GatewayClock, IdentityHealth, InterestAuthority, SessionTokenV1Authorizer,
    SnapshotInterestAuthority,
};
use orrery_persistd::GatewayConfig;
use orrery_protocol::{
    AccountId, CellId, CoordinatorInterestSnapshot, Epoch, GridId, IssuerKey, IssuerKeyId, NodeId,
    SessionStanding, SessionTokenClaimsV1, SessionTokenTtlMs, SessionTokenV1, UnixMillis,
};

pub const TOKEN_NOW_MS: u64 = 1_000;
pub const TOKEN_ISSUED_AT_MS: u64 = 900;
pub const TOKEN_TTL_MS: u64 = 60_000;
pub const INTEREST_VALID_UNTIL_MS: u64 = 61_000;
pub const ISSUER_KEY_ID: u32 = 11;

pub fn secret(seed_byte: u8) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = seed_byte;
    iroh_base::SecretKey::from_bytes(&seed)
}

pub fn node(seed_byte: u8) -> NodeId {
    secret(seed_byte).public()
}

pub fn issuer() -> iroh_base::SecretKey {
    secret(42)
}

#[derive(Debug)]
struct FixedGatewayClock(u64);

impl GatewayClock for FixedGatewayClock {
    fn now_ms(&self) -> UnixMillis {
        UnixMillis::new(self.0)
    }
}

#[derive(Debug)]
struct AvailableIdentityHealth;

impl IdentityHealth for AvailableIdentityHealth {
    fn is_available(&self) -> bool {
        true
    }
}

pub fn fixed_clock(now_ms: u64) -> Arc<dyn GatewayClock> {
    Arc::new(FixedGatewayClock(now_ms))
}

pub fn available_identity_health() -> Arc<dyn IdentityHealth> {
    Arc::new(AvailableIdentityHealth)
}

pub fn authorizer(issuer: &iroh_base::SecretKey) -> Arc<dyn GatewayAuthorizer> {
    Arc::new(SessionTokenV1Authorizer::new([IssuerKey::new(
        IssuerKeyId::new(ISSUER_KEY_ID),
        issuer.public(),
    )]))
}

pub fn interest_authority(
    snapshots: impl IntoIterator<Item = CoordinatorInterestSnapshot>,
) -> Arc<dyn InterestAuthority> {
    Arc::new(SnapshotInterestAuthority::from_snapshots(snapshots))
}

pub fn interest_snapshot(
    peer: NodeId,
    grid: GridId,
    covered_cells: Vec<CellId>,
) -> CoordinatorInterestSnapshot {
    CoordinatorInterestSnapshot {
        peer,
        epoch: Epoch::new(1),
        grid,
        covered_cells,
        valid_until_ms: INTEREST_VALID_UNTIL_MS,
    }
}

pub fn session_token(
    issuer: &iroh_base::SecretKey,
    bound_node: NodeId,
    issued_at_ms: u64,
    ttl_ms: u64,
) -> Vec<u8> {
    SessionTokenV1::sign(
        SessionTokenClaimsV1::new(
            AccountId::new(7),
            bound_node,
            UnixMillis::new(issued_at_ms),
            SessionTokenTtlMs::new(ttl_ms),
            SessionStanding::Good,
            IssuerKeyId::new(ISSUER_KEY_ID),
        ),
        issuer,
    )
    .expect("sign deterministic session token")
    .encode()
    .expect("encode deterministic session token")
}

pub fn valid_session_token(bound_node: NodeId) -> Vec<u8> {
    session_token(&issuer(), bound_node, TOKEN_ISSUED_AT_MS, TOKEN_TTL_MS)
}

pub fn authority_config(peer: NodeId, grid: GridId, covered_cells: Vec<CellId>) -> GatewayConfig {
    let issuer = issuer();
    GatewayConfig {
        interest_authority: interest_authority([interest_snapshot(peer, grid, covered_cells)]),
        authorizer: authorizer(&issuer),
        identity_clock: fixed_clock(TOKEN_NOW_MS),
        identity_health: available_identity_health(),
        ..GatewayConfig::default()
    }
}

/// The cluster file for this crate's FDB-gated tests, or `None` if none is
/// configured.
///
/// One rule for every FDB-gated binary here, delegating to
/// [`orrery_persistd::fdb::discover_cluster_file`] — which `orrery_seed`'s
/// gates share, so the two crates' tiers cannot disagree about when they run.
/// Copies of this guard had already drifted: one variant read only
/// `ORRERY_FDB_CLUSTER_FILE` while the rest also walked up for a
/// `.fdb-dev/fdb.cluster`.
///
/// Every handle opened from the returned path is bounded (see
/// [`orrery_persistd::fdb`]), so a test pointed at a cluster that never
/// answers fails inside the transaction budget rather than hanging the suite.
#[cfg(feature = "fdb")]
pub fn fdb_cluster_file() -> Option<String> {
    orrery_persistd::fdb::discover_cluster_file()
}
