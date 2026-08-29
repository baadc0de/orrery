//! Signing the coordinator's interest handouts (D7 §5, D12).
//!
//! A gateway will not take a peer's word for what it is interested in: interest
//! is what gates weak claims and successor selection, so self-declared interest
//! would be self-granted authority. The coordinator is the only party allowed
//! to assert it, and it does so by signing.
//!
//! Delivery is deliberately not the coordinator's problem. It signs a grant,
//! hands it to the peer, and the peer presents it to whichever gateway it is
//! talking to — the same handout model as an identity token. That is why there
//! is no coordinator→gateway connection anywhere in this crate: adding gateways
//! does not add coordinator fan-out, and a gateway needs only the coordinator's
//! *public* key to check the claim.

use orrery_protocol::coord::InterestGrantClaimsV1;
use orrery_protocol::{
    GridId, InterestCellCrossing, InterestGrantV1, IssuerKey, IssuerKeyId, NodeId,
    MAX_PRESENCE_CELLS,
};

use crate::registry::{IslandRegistry, MembershipChange};

/// The atomic coordinator result of an immediate cell crossing.
///
/// The membership change and replacement grant are produced from the same
/// registry update so a caller cannot publish a new roster while leaving the
/// mover with authorization for the old coverage.
#[derive(Debug)]
pub struct IssuedInterestCrossing {
    /// Roster manifests and drains caused by the crossing.
    pub membership: MembershipChange,
    /// Signed post-crossing interest grant for the moving peer.
    pub grant: Vec<u8>,
}

/// Why an immediate interest-cell crossing was refused.
#[derive(Debug)]
pub enum InterestCrossingError {
    /// `from` and `to` name the same committed cell.
    NoCellChange,
    /// The source cell or authority order is older than coordinator state.
    StaleCrossing,
    /// The post-crossing set is empty or exceeds the wire bound.
    CellCount,
    /// The post-crossing set does not contain the newly committed cell.
    DestinationNotCovered,
    /// The replacement grant could not be encoded.
    Encode(postcard::Error),
}

impl core::fmt::Display for InterestCrossingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoCellChange => f.write_str("crossing did not change the committed cell"),
            Self::StaleCrossing => f.write_str("crossing source or authority order is stale"),
            Self::CellCount => write!(
                f,
                "post-crossing interest must contain 1..={MAX_PRESENCE_CELLS} cells"
            ),
            Self::DestinationNotCovered => {
                f.write_str("post-crossing interest does not cover the destination cell")
            }
            Self::Encode(error) => write!(f, "encode replacement interest grant: {error}"),
        }
    }
}

impl core::error::Error for InterestCrossingError {}

/// The coordinator's interest-grant signing key and its rotation identifier.
#[derive(Debug, Clone)]
pub struct InterestIssuer {
    key: iroh_base::SecretKey,
    key_id: IssuerKeyId,
}

impl InterestIssuer {
    /// Bind a signing key to the identifier gateways will select it by.
    #[must_use]
    pub fn new(key: iroh_base::SecretKey, key_id: IssuerKeyId) -> Self {
        Self { key, key_id }
    }

    /// The rotation identifier stamped into issued grants.
    #[must_use]
    pub fn key_id(&self) -> IssuerKeyId {
        self.key_id
    }

    /// The entry a gateway must be configured with to accept these grants.
    ///
    /// Only the public half crosses the boundary — a gateway verifies, it
    /// never mints.
    #[must_use]
    pub fn trusted_key(&self) -> IssuerKey {
        IssuerKey::new(self.key_id, self.key.public())
    }

    /// Sign prepared claims into the opaque bytes a peer forwards.
    pub fn sign(&self, claims: InterestGrantClaimsV1) -> Result<Vec<u8>, postcard::Error> {
        InterestGrantV1::sign(claims, &self.key)?.encode()
    }

    /// Mint and sign the grant authorizing `node`'s current coverage.
    ///
    /// `None` when the peer has reported no presence — there is nothing to
    /// authorize, and an empty grant is refused by verifiers in any case.
    pub fn issue(
        &self,
        registry: &IslandRegistry,
        node: NodeId,
        grid: GridId,
    ) -> Option<Result<Vec<u8>, postcard::Error>> {
        registry
            .interest_claims(node, grid, self.key_id)
            .map(|claims| self.sign(claims))
    }

    /// Apply a committed-cell crossing and mint its replacement grant now.
    ///
    /// The ordinary presence report remains the one-hertz bulk repair path.
    /// This event path exists so the coordinator does not wait for that clock
    /// after a fast craft actually crosses. Predictive swept coverage is still
    /// required: an event is reactive, can be delayed by the network, and does
    /// not page the destination in before the crossing.
    pub fn apply_crossing(
        &self,
        registry: &mut IslandRegistry,
        node: NodeId,
        grid: GridId,
        crossing: InterestCellCrossing,
    ) -> Result<IssuedInterestCrossing, InterestCrossingError> {
        if crossing.from == crossing.to {
            return Err(InterestCrossingError::NoCellChange);
        }
        if !registry.accepts_crossing(node, &crossing) {
            return Err(InterestCrossingError::StaleCrossing);
        }
        if crossing.covered_cells.is_empty() || crossing.covered_cells.len() > MAX_PRESENCE_CELLS {
            return Err(InterestCrossingError::CellCount);
        }
        if !crossing.covered_cells.contains(&crossing.to) {
            return Err(InterestCrossingError::DestinationNotCovered);
        }

        let covered_cells = crossing.covered_cells.clone();
        let membership = registry.report_crossing(node, &crossing, covered_cells);
        let claims = registry
            .interest_claims(node, grid, self.key_id)
            .expect("validated non-empty crossing coverage was recorded");
        let grant = self.sign(claims).map_err(InterestCrossingError::Encode)?;
        Ok(IssuedInterestCrossing { membership, grant })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_games::regolith::archetype::Archetype;
    use orrery_games::regolith::CAMPAIGN_CELL_EDGE_M;
    use orrery_protocol::{
        verify_interest_grant, CellId, Epoch, InterestGrantVerificationError, INTEREST_LEVEL,
    };

    fn secret(seed: u8) -> iroh_base::SecretKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        iroh_base::SecretKey::from_bytes(&bytes)
    }

    fn node(seed: u8) -> NodeId {
        secret(seed).public()
    }

    fn cell(x: i32) -> CellId {
        CellId::from_coords(glam::IVec3::new(x, 0, 0), CellId::MAX_LEVEL).unwrap()
    }

    const INTEREST_REFRESH_PERIOD_S: f64 = 1.0;

    fn interceptor_ceiling_mps() -> f64 {
        Archetype::Interceptor.limits().max_speed_mms as f64 / 1_000.0
    }

    #[test]
    fn an_issued_grant_verifies_against_the_advertised_public_key() {
        // Given: a peer whose presence the coordinator has recorded.
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let mut registry = IslandRegistry::new();
        registry.report_presence(node(1), vec![cell(0), cell(1)]);

        // When: the coordinator issues its grant.
        let encoded = issuer
            .issue(&registry, node(1), GridId::ROOT)
            .expect("a peer with presence has something to authorize")
            .expect("grant encodes");

        // Then: a gateway holding only the public half accepts it, and the
        // coverage is exactly the reported presence, in sorted order.
        let claims = verify_interest_grant(&encoded, &node(1), &[issuer.trusted_key()])
            .expect("gateway accepts the coordinator's own signature");
        assert_eq!(claims.peer, node(1));
        assert_eq!(claims.covered_cells, {
            let mut expected = vec![cell(0), cell(1)];
            expected.sort();
            expected
        });
        assert_eq!(claims.ttl_ms, registry.config.interest_grant_ttl_ms);
    }

    #[test]
    fn a_peer_with_no_presence_gets_no_grant() {
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let registry = IslandRegistry::new();
        assert!(issuer.issue(&registry, node(1), GridId::ROOT).is_none());
    }

    #[test]
    fn v18_ceiling_sweep_covers_every_reachable_neighborhood() {
        let centre = CellId::from_coords(glam::IVec3::ZERO, INTEREST_LEVEL).unwrap();
        let edge_m = CAMPAIGN_CELL_EDGE_M;
        let max_speed_mps = interceptor_ceiling_mps();
        let direction = glam::DVec3::ONE.normalize();
        let velocity_mps = direction * max_speed_mps;
        // Close enough to the upper corner that every positive component
        // crosses, which is the maximum-growth orientation for this ceiling.
        let offset_m = glam::DVec3::splat(edge_m - f64::EPSILON * edge_m);
        let swept =
            centre.swept_neighbors27(offset_m, velocity_mps, INTEREST_REFRESH_PERIOD_S, edge_m);

        // Recompute the path from the same chassis, edge and refresh constants.
        // Sampling more finely than a cell crossing is not the proof: each
        // sample's entire neighborhood must be inside the analytical cuboid.
        let sample_count = 1_000;
        let (origin, _) = centre.coords();
        for sample in 0..=sample_count {
            let elapsed = INTEREST_REFRESH_PERIOD_S * sample as f64 / sample_count as f64;
            let position = offset_m + velocity_mps * elapsed;
            let delta = glam::IVec3::new(
                (position.x / edge_m).floor() as i32,
                (position.y / edge_m).floor() as i32,
                (position.z / edge_m).floor() as i32,
            );
            let reachable = CellId::from_coords(origin + delta, INTEREST_LEVEL).unwrap();
            for required in reachable.neighbors27() {
                assert!(
                    swept.contains(&required),
                    "v18 ceiling sweep omitted reachable AOI cell {required} at {elapsed:.3} s"
                );
            }
        }
    }

    #[test]
    fn v18_ceiling_sweep_has_a_derived_cell_count_bound() {
        let centre = CellId::from_coords(glam::IVec3::ZERO, INTEREST_LEVEL).unwrap();
        let edge_m = CAMPAIGN_CELL_EDGE_M;
        let max_speed_mps = interceptor_ceiling_mps();
        let crossed_cells_per_axis =
            (max_speed_mps * INTEREST_REFRESH_PERIOD_S / edge_m).ceil() as usize;
        // A straight segment crosses each axis monotonically. Every crossing
        // adds at most one 3x3 face to the preceding 27-cell neighbourhood.
        let derived_worst_cells = 27 + 3 * 9 * crossed_cells_per_axis;
        let offset_m = glam::DVec3::splat(edge_m - f64::EPSILON * edge_m);
        let diagonal = glam::DVec3::ONE.normalize() * max_speed_mps;
        let swept = centre.swept_neighbors27(offset_m, diagonal, INTEREST_REFRESH_PERIOD_S, edge_m);

        assert_eq!(
            swept.len(),
            derived_worst_cells,
            "v18 ceiling swept-cell maximum changed: speed {max_speed_mps} m/s, edge {edge_m} m, refresh {INTEREST_REFRESH_PERIOD_S} s"
        );
        assert!(
            derived_worst_cells <= MAX_PRESENCE_CELLS,
            "v18 ceiling needs {derived_worst_cells} swept cells but the signed presence/grant bound is {MAX_PRESENCE_CELLS}"
        );
    }

    #[test]
    fn crossing_replaces_roster_and_grant_without_waiting_for_bulk_refresh() {
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let mut registry = IslandRegistry::new();
        let from = cell(0);
        let to = cell(1);
        registry.report_presence(node(1), from.neighbors27());
        registry.report_presence(node(2), from.neighbors27());
        let old = registry
            .interest_claims(node(1), GridId::ROOT, issuer.key_id())
            .expect("initial presence");
        let covered_cells = to.neighbors27();

        let issued = issuer
            .apply_crossing(
                &mut registry,
                node(1),
                GridId::ROOT,
                InterestCellCrossing {
                    tick: orrery_protocol::Tick(1),
                    seq: orrery_protocol::SeqPair {
                        own_seq: 1,
                        auth_seq: 1,
                    },
                    from,
                    to,
                    covered_cells: covered_cells.clone(),
                },
            )
            .expect("a real crossing is applied on the event path");

        let claims = verify_interest_grant(&issued.grant, &node(1), &[issuer.trusted_key()])
            .expect("crossing grant verifies");
        assert!(
            claims.epoch > old.epoch,
            "crossing must replace the old grant immediately"
        );
        assert_eq!(claims.covered_cells, {
            let mut expected = covered_cells.clone();
            expected.sort();
            expected
        });
        let mover = issued
            .membership
            .manifests
            .iter()
            .flat_map(|manifest| &manifest.peers)
            .find(|entry| entry.node == node(1))
            .expect("crossing publishes a roster containing the mover");
        assert_eq!(mover.cells, claims.covered_cells);
    }

    #[test]
    fn a_stale_crossing_cannot_overwrite_newer_presence() {
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let mut registry = IslandRegistry::new();
        registry.report_presence(node(1), cell(0).neighbors27());
        issuer
            .apply_crossing(
                &mut registry,
                node(1),
                GridId::ROOT,
                InterestCellCrossing {
                    tick: orrery_protocol::Tick(1),
                    seq: orrery_protocol::SeqPair {
                        own_seq: 1,
                        auth_seq: 1,
                    },
                    from: cell(0),
                    to: cell(1),
                    covered_cells: cell(1).neighbors27(),
                },
            )
            .expect("first crossing establishes the committed centre");

        let error = issuer
            .apply_crossing(
                &mut registry,
                node(1),
                GridId::ROOT,
                InterestCellCrossing {
                    tick: orrery_protocol::Tick(2),
                    seq: orrery_protocol::SeqPair {
                        own_seq: 1,
                        auth_seq: 1,
                    },
                    from: cell(0),
                    to: cell(-1),
                    // The new coverage still contains cell 0, so a mere
                    // set-membership check would accept this reordered event.
                    covered_cells: cell(-1).neighbors27(),
                },
            )
            .expect_err("an event from superseded coverage is stale");
        assert!(matches!(error, InterestCrossingError::StaleCrossing));
    }

    #[test]
    fn moving_bumps_the_epoch_so_a_stale_wider_grant_cannot_be_replayed() {
        // Given: a peer that was interested in two cells and has since moved
        // to one. The narrower grant must be the one that wins at a gateway.
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let mut registry = IslandRegistry::new();
        registry.report_presence(node(1), vec![cell(0), cell(1)]);
        let wide = registry
            .interest_claims(node(1), GridId::ROOT, issuer.key_id())
            .expect("presence recorded");

        registry.report_presence(node(1), vec![cell(0)]);
        let narrow = registry
            .interest_claims(node(1), GridId::ROOT, issuer.key_id())
            .expect("presence still recorded");

        // Then: the newer, narrower grant carries the higher epoch, which is
        // what lets a gateway refuse the older one on replay.
        assert!(narrow.epoch > wide.epoch);
        assert_eq!(narrow.covered_cells, vec![cell(0)]);
        assert_eq!(wide.covered_cells.len(), 2);
    }

    #[test]
    fn re_reporting_identical_presence_does_not_churn_the_epoch() {
        // A peer that keeps announcing the same cells should not invalidate
        // the grant it is already holding.
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let mut registry = IslandRegistry::new();
        registry.report_presence(node(1), vec![cell(0)]);
        let first = registry
            .interest_claims(node(1), GridId::ROOT, issuer.key_id())
            .expect("presence recorded");
        registry.report_presence(node(1), vec![cell(0)]);
        let second = registry
            .interest_claims(node(1), GridId::ROOT, issuer.key_id())
            .expect("presence recorded");

        assert_eq!(first.epoch, second.epoch);
        assert_eq!(first, second);
    }

    #[test]
    fn a_grant_is_bound_to_the_grid_it_was_minted_for() {
        // Cell ids are grid-relative (P-7), so a grant for one grid must not
        // authorize the same raw cell in another.
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let mut registry = IslandRegistry::new();
        registry.report_presence(node(1), vec![cell(0)]);

        let root = issuer
            .issue(&registry, node(1), GridId::ROOT)
            .expect("presence recorded")
            .expect("encodes");
        let nested = issuer
            .issue(&registry, node(1), GridId::new(7))
            .expect("presence recorded")
            .expect("encodes");

        let keys = [issuer.trusted_key()];
        assert_eq!(
            verify_interest_grant(&root, &node(1), &keys).unwrap().grid,
            GridId::ROOT
        );
        assert_eq!(
            verify_interest_grant(&nested, &node(1), &keys)
                .unwrap()
                .grid,
            GridId::new(7)
        );
        assert_ne!(root, nested);
    }

    #[test]
    fn a_rotated_out_key_stops_being_accepted() {
        // Rotation is why grants carry a key id at all: retiring the old key
        // from a gateway's trusted set must immediately stop its grants.
        let retired = InterestIssuer::new(secret(9), IssuerKeyId::new(3));
        let current = InterestIssuer::new(secret(10), IssuerKeyId::new(4));
        let mut registry = IslandRegistry::new();
        registry.report_presence(node(1), vec![cell(0)]);

        let old_grant = retired
            .issue(&registry, node(1), GridId::ROOT)
            .expect("presence recorded")
            .expect("encodes");

        assert_eq!(
            verify_interest_grant(&old_grant, &node(1), &[current.trusted_key()]),
            Err(InterestGrantVerificationError::UnknownIssuer(
                IssuerKeyId::new(3)
            ))
        );
        // During an overlap both are accepted, which is what makes a rotation
        // deployable without a flag day.
        assert!(verify_interest_grant(
            &old_grant,
            &node(1),
            &[current.trusted_key(), retired.trusted_key()]
        )
        .is_ok());
    }

    #[test]
    fn the_unsigned_snapshot_matches_what_a_verified_grant_localizes_to() {
        // The in-process shortcut and the signed path must not drift.
        let issuer = InterestIssuer::new(secret(9), IssuerKeyId::new(0));
        let mut registry = IslandRegistry::new();
        registry.report_presence(node(1), vec![cell(0)]);

        let direct = registry
            .interest_snapshot(node(1), GridId::ROOT, 5_000)
            .expect("presence recorded");
        let encoded = issuer
            .issue(&registry, node(1), GridId::ROOT)
            .expect("presence recorded")
            .expect("encodes");
        let claims = verify_interest_grant(&encoded, &node(1), &[issuer.trusted_key()]).unwrap();
        let localized = orrery_protocol::CoordinatorInterestSnapshot::from_grant(claims, 5_000);

        assert_eq!(direct, localized);
        assert_eq!(direct.epoch, Epoch::new(1));
    }
}
