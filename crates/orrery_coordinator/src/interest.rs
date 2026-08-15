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
use orrery_protocol::{GridId, InterestGrantV1, IssuerKey, IssuerKeyId, NodeId};

use crate::registry::IslandRegistry;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{verify_interest_grant, CellId, Epoch, InterestGrantVerificationError};

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
