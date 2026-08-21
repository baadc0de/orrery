//! Seeding witness sets per cell-epoch (D10 item 4, D28).
//!
//! D10 says a witness set is "seeded by the coordinator per cell-epoch …
//! **never self-chosen** — anti-collusion", and until this module nothing in
//! the tree seeded anything: `orrery_witness` left its `WitnessSet` empty and
//! streamed to the first few island peers in `NodeId` order, which is a peer
//! choosing its own witnesses. That is only tolerable while reports have no
//! consequences; the moment an attestation gates a commit, a cheat that picks
//! its own witnesses picks its own collaborators.
//!
//! So the choosing happens here, and it happens in a way the coordinator can
//! later be held to:
//!
//! - the **pool** is announced, not just the draw, so an auditor can see what
//!   the coordinator had to choose from;
//! - the draw is a **secret-keyed shuffle**, so nobody can pre-position
//!   colluders against a set that has not been announced yet;
//! - the key is committed to at announcement and **revealed in the next
//!   announcement for the same cell**, so a coordinator cannot issue a usable
//!   epoch `e + 1` without opening `e`. Withholding a reveal costs the
//!   coordinator the cell rather than costing an auditor the proof.
//!
//! Delivery is the interest grant's model exactly ([`crate::interest`]): the
//! coordinator signs, hands the bytes to the peers covering the cell, and one
//! of them couriers them to whichever gateway it is talking to. There is no
//! coordinator→gateway connection here either, and D28 clause (a) is the
//! record that says there must not be one.

use std::collections::{HashMap, HashSet};

use orrery_protocol::coord::{
    draw_witness_set, witness_epoch_commitment, witness_epoch_seed, WitnessEpochClaimsV1,
    WitnessEpochV1, MAX_EPOCH_CANDIDATES, WITNESS_EPOCH_KEY_V1_DOMAIN, WITNESS_SET_FLOOR_N,
};
use orrery_protocol::{
    witness_epoch_binding, AccountId, CellId, GridId, IssuerKey, IssuerKeyId, NodeId,
    SessionStanding,
};

use crate::registry::IslandRegistry;

/// The coordinator's witness-epoch signing key, master secret and incarnation.
///
/// The signing half mirrors [`InterestIssuer`](crate::InterestIssuer) — a
/// secret key plus the rotation identifier a verifier selects it by, and only
/// the public half ever crosses to a verifier. What is new is the **master
/// secret**, and why it is a master secret rather than a fresh random key per
/// epoch is the interesting part.
///
/// A freshly drawn per-epoch key lives only in the leader's memory, so a
/// failover between issuance and reveal loses it permanently and every epoch
/// it covered becomes unauditable — precisely the window an attacker would
/// choose. Deriving `k_e = HKDF-SHA256(K_master, DOMAIN ‖ grid ‖ cell ‖ epoch)`
/// makes a warm standby able to reveal an epoch it did not issue, from the
/// provisioned secret alone. HKDF is one-way, so revealing `k_e` says nothing
/// about `K_master` or about `k_{e+1}`, which is what keeps consecutive draws
/// independent and the anti-grind argument standing.
#[derive(Debug, Clone)]
pub struct WitnessEpochIssuer {
    key: iroh_base::SecretKey,
    key_id: IssuerKeyId,
    master: [u8; 32],
    incarnation: u64,
}

impl WitnessEpochIssuer {
    /// Bind a signing key, a master secret and a leader incarnation.
    ///
    /// `incarnation` is the coordinator's leader-lease generation. It is the
    /// high 16 bits of every handle this issuer mints, so a failover cannot
    /// produce a handle colliding with its predecessor's without also having
    /// won the lease.
    #[must_use]
    pub fn new(
        key: iroh_base::SecretKey,
        key_id: IssuerKeyId,
        master: [u8; 32],
        incarnation: u64,
    ) -> Self {
        Self {
            key,
            key_id,
            master,
            incarnation,
        }
    }

    /// The rotation identifier stamped into issued announcements.
    #[must_use]
    pub fn key_id(&self) -> IssuerKeyId {
        self.key_id
    }

    /// The leader incarnation this issuer stamps into handles.
    #[must_use]
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// The entry a verifier must be configured with to accept announcements.
    ///
    /// Only the public half — a gateway verifies, it never seeds. That is the
    /// property clause (a) refuses to give up: a gateway that could store a
    /// row it had not verified could seed its own witness sets, and D10's
    /// "never self-chosen" would bind peers and not the cluster.
    #[must_use]
    pub fn trusted_key(&self) -> IssuerKey {
        IssuerKey::new(self.key_id, self.key.public())
    }

    /// Derive the secret seed key for one cell-epoch.
    ///
    /// Kept secret until the reveal: it is the MAC key the shuffle seed comes
    /// from, so anyone holding it can recompute the draw, which is exactly
    /// what must not be possible *before* the epoch is over.
    #[must_use]
    pub fn epoch_key(&self, grid: GridId, cell: CellId, epoch: u32) -> [u8; 32] {
        let mut info = Vec::with_capacity(WITNESS_EPOCH_KEY_V1_DOMAIN.len() + 16);
        info.extend_from_slice(WITNESS_EPOCH_KEY_V1_DOMAIN);
        info.extend_from_slice(&witness_epoch_binding(grid, cell, epoch));
        let mut key = [0u8; 32];
        hkdf::Hkdf::<sha2::Sha256>::new(None, &self.master)
            .expand(&info, &mut key)
            .expect("32 bytes is well under HKDF-SHA256's output limit");
        key
    }

    /// Sign prepared claims into the opaque bytes a peer couriers.
    pub fn sign(&self, claims: WitnessEpochClaimsV1) -> Result<Vec<u8>, postcard::Error> {
        WitnessEpochV1::sign(claims, &self.key)?.encode()
    }
}

/// The epoch cadence and eligibility windows a seeder runs to (D16, D28).
#[derive(Debug, Clone)]
pub struct WitnessSeedConfig {
    /// How long an epoch runs before elapsed time alone forces a reseed.
    pub epoch_ms: u64,
    /// How long past the epoch a stale-epoch attestation is still admitted.
    ///
    /// D16's 30 s, which is one epoch length, and that is docs/07 §7's
    /// reconnect promise: a netsplit survivor gets one whole epoch's grace.
    pub accept_grace_ms: u64,
    /// The floor between two reseeds of the same cell.
    ///
    /// The rate limit is half of what makes a churn reseed un-grindable (the
    /// cooldown below is the other half): without it, a colluder could force
    /// redraws as fast as it could reconnect.
    pub reseed_min_ms: u64,
    /// How long an account is out of the pool after one of its sessions ends.
    ///
    /// D16's 60 s, and the number is chosen against the reseed floor rather
    /// than picked: at `6 × reseed_min_ms` a bounce forfeits six draws to buy
    /// one, so leaving and returning is strictly losing. It also exceeds the
    /// epoch length, so a bouncer misses a whole natural epoch too.
    pub reseed_cooldown_ms: u64,
    /// How long a peer must have held a coordinator session to be eligible.
    ///
    /// docs/07 §4.1's "present in the island ≥ 10 s". The coordinator times
    /// its own sessions, so this needs no new observation and no new edge.
    pub min_presence_ms: u64,
}

impl Default for WitnessSeedConfig {
    fn default() -> Self {
        Self {
            epoch_ms: 30_000,
            accept_grace_ms: 30_000,
            reseed_min_ms: 10_000,
            reseed_cooldown_ms: 60_000,
            min_presence_ms: 10_000,
        }
    }
}

/// What the coordinator knows about one connected peer, from its own session.
///
/// The account and the standing are lifted out of the identity token the peer
/// presented at `Hello` — a signature the coordinator already verifies, whose
/// claims used to be discarded on the floor. Four of D28 clause (e)'s six
/// eligibility rows rest on nothing more than keeping them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionFacts {
    account: AccountId,
    standing: SessionStanding,
    joined_ms: u64,
}

/// One cell's current epoch, as the coordinator remembers it.
#[derive(Debug, Clone)]
struct CellEpoch {
    epoch: u32,
    /// The secret key for `epoch`, held back until the *next* announcement
    /// for this cell reveals it. This field is the chained reveal.
    seed_key: [u8; 32],
    pool: Vec<NodeId>,
    announced_ms: u64,
}

/// A signed announcement and the peers that may courier it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeededEpoch {
    /// The claims that were signed, for the coordinator's own logs and tests.
    pub claims: WitnessEpochClaimsV1,
    /// The postcard-encoded, signed `WitnessEpochV1` a peer forwards.
    pub announcement: Vec<u8>,
    /// Every peer whose presence covers the cell, in ascending byte order.
    ///
    /// Deliberately wider than the eligible pool: a peer that is not eligible
    /// to *witness* still submits intents for the cell and still needs the
    /// bytes its gateway will judge them against. Any of them can be the
    /// courier, which is what makes the delivery path survive one peer
    /// leaving.
    pub recipients: Vec<NodeId>,
}

/// What one seeding attempt did, and if it did nothing, why not.
///
/// Four outcomes rather than an `Option` because they are operationally
/// different, and collapsing them is how a pool collapse would come to read
/// like a quiet success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedOutcome {
    /// A new epoch was drawn and signed.
    Seeded(Box<SeededEpoch>),
    /// A trigger fired but the cell is inside its reseed floor.
    Cooling,
    /// No trigger fired: the current epoch stands.
    Unchanged,
    /// The eligible pool is below [`WITNESS_SET_FLOOR_N`], so nothing was
    /// announced.
    ///
    /// A short set that verified as if it were a full one is the collusion
    /// hole K-of-N exists to close, so the coordinator announces nothing at
    /// all and the cell takes D29's low-population path instead. Note what
    /// this does *not* do: it does not retire the standing epoch, which
    /// remains usable for its own window.
    BelowFloor {
        /// How many eligible candidates there were.
        eligible: usize,
    },
}

/// The coordinator's per-cell witness-epoch state.
///
/// Pure and IO-free, like [`IslandRegistry`], so the draw, the triggers and
/// the eligibility filters are unit-testable without iroh or tokio.
#[derive(Debug)]
pub struct WitnessSeeder {
    /// The cadence and windows this seeder runs to.
    pub config: WitnessSeedConfig,
    grid: GridId,
    sessions: HashMap<NodeId, SessionFacts>,
    cells: HashMap<CellId, CellEpoch>,
    /// Accounts out of the pool until an instant, keyed by account so a Sybil
    /// cannot dodge its own cooldown by reconnecting under a second NodeId.
    cooldowns: HashMap<AccountId, u64>,
    counter: u64,
}

impl WitnessSeeder {
    /// A seeder for one grid, at the D16 defaults.
    #[must_use]
    pub fn new(grid: GridId) -> Self {
        Self::with_config(grid, WitnessSeedConfig::default())
    }

    /// A seeder for one grid, at an explicit cadence.
    #[must_use]
    pub fn with_config(grid: GridId, config: WitnessSeedConfig) -> Self {
        Self {
            config,
            grid,
            sessions: HashMap::new(),
            cells: HashMap::new(),
            cooldowns: HashMap::new(),
            counter: 0,
        }
    }

    /// Record the identity facts a peer's verified session token carried.
    ///
    /// Called once, when the token verifies — not per presence report, because
    /// `joined_ms` is what "present in the island ≥ 10 s" is measured from and
    /// a peer that keeps talking must not keep resetting its own probation.
    pub fn note_session(
        &mut self,
        node: NodeId,
        account: AccountId,
        standing: SessionStanding,
        now_ms: u64,
    ) {
        self.sessions.entry(node).or_insert(SessionFacts {
            account,
            standing,
            joined_ms: now_ms,
        });
    }

    /// Drop a peer's session and put its account on cooldown.
    ///
    /// D28 clause (g) puts the cooldown on an account "whose session loss
    /// contributed to a reseed". This applies it to **every** session loss,
    /// which is stricter in exactly one direction — more accounts are
    /// excluded, never fewer — and avoids the coordinator having to decide
    /// after the fact whether a particular departure was what moved the pool.
    /// A peer that leaves and returns is the case the cooldown is aimed at,
    /// and it is indistinguishable from the case this covers.
    pub fn forget_session(&mut self, node: NodeId, now_ms: u64) {
        if let Some(facts) = self.sessions.remove(&node) {
            let until = now_ms.saturating_add(self.config.reseed_cooldown_ms);
            let entry = self.cooldowns.entry(facts.account).or_insert(until);
            *entry = (*entry).max(until);
        }
    }

    /// The epoch counter this cell is on, if it has ever been seeded.
    #[must_use]
    pub fn current_epoch(&self, cell: CellId) -> Option<u32> {
        self.cells.get(&cell).map(|state| state.epoch)
    }

    /// The eligible candidate pool for a cell (D28 clause (e)).
    ///
    /// Exactly the filters that are inside a signature the coordinator has
    /// already verified or an observation it already makes:
    ///
    /// - **good standing** — the token's signed `standing`, so a quarantined
    ///   account cannot witness;
    /// - **presence ≥ `min_presence_ms`** — timed against this coordinator's
    ///   own session;
    /// - **one slot per account** — dedup on the signed `account`, taking the
    ///   lowest `NodeId` so the choice is deterministic;
    /// - **not on cooldown** — the anti-grind exclusion above.
    ///
    /// And, said out loud because a silent no-op reads like enforcement:
    /// **account age past probation is not enforced** (it is not a token
    /// field), the strike-score threshold is **approximated** by the coarse
    /// quarantine flag, and per-account exclusion holds only within this
    /// coordinator — a NodeId bound to the same account but connected
    /// elsewhere is not deduped, because nothing writes the `id/` rows that
    /// would answer it. D28's Consequences carries the cost of each.
    #[must_use]
    pub fn eligible_pool(
        &self,
        registry: &IslandRegistry,
        cell: CellId,
        now_ms: u64,
    ) -> Vec<NodeId> {
        let mut by_account: HashMap<AccountId, NodeId> = HashMap::new();
        for node in registry.peers_covering(cell) {
            let Some(facts) = self.sessions.get(&node) else {
                continue;
            };
            if facts.standing != SessionStanding::Good {
                continue;
            }
            if now_ms.saturating_sub(facts.joined_ms) < self.config.min_presence_ms {
                continue;
            }
            if self
                .cooldowns
                .get(&facts.account)
                .is_some_and(|until| now_ms < *until)
            {
                continue;
            }
            by_account
                .entry(facts.account)
                .and_modify(|kept| {
                    if node.as_bytes() < kept.as_bytes() {
                        *kept = node;
                    }
                })
                .or_insert(node);
        }
        let mut pool: Vec<NodeId> = by_account.into_values().collect();
        pool.sort_by_key(|node| *node.as_bytes());
        // A pool above D6's interest-mesh ceiling describes a population the
        // topology does not have — such a cell is promoted to a field host.
        // Until promotion exists the excess is truncated rather than refused,
        // and the honest statement of what that costs is that the surviving
        // pool is the 32 lowest NodeIds rather than a sample of the cell.
        pool.truncate(MAX_EPOCH_CANDIDATES);
        pool
    }

    /// Seed a new epoch for `cell` if a trigger has fired (D28 clause (g)).
    ///
    /// The triggers, all of them observations the coordinator already makes:
    ///
    /// ```text
    /// reseed(c) at t  ⟺  t − t_last(c) ≥ reseed_min
    ///                 ∧ ( t − t_last ≥ epoch_ms          // elapsed
    ///                   ∨ |P_now △ P_last| > |P_last|/2  // >50% churn
    ///                   ∨ |P_now| < floor )              // pool collapse
    /// ```
    ///
    /// docs/07 §4.1 has the churn trigger firing on *gateway*-observed
    /// disconnects. Routing that to the coordinator would need a
    /// gateway→coordinator edge to carry a signal the coordinator already has
    /// — a peer that left the island dropped its coordinator session too — so
    /// this observes its own sessions instead. What makes a churn reseed
    /// un-grindable was never the identity of the observer; it is the rate
    /// limit and the cooldown, and both are here.
    pub fn maybe_seed(
        &mut self,
        issuer: &WitnessEpochIssuer,
        registry: &IslandRegistry,
        cell: CellId,
        now_ms: u64,
    ) -> SeedOutcome {
        let pool = self.eligible_pool(registry, cell, now_ms);
        let next_epoch = match self.cells.get(&cell) {
            None => 0,
            Some(state) => {
                let elapsed = now_ms.saturating_sub(state.announced_ms);
                if elapsed < self.config.reseed_min_ms {
                    return SeedOutcome::Cooling;
                }
                let churn = symmetric_difference(&pool, &state.pool);
                let triggered = elapsed >= self.config.epoch_ms
                    || churn * 2 > state.pool.len()
                    || pool.len() < WITNESS_SET_FLOOR_N;
                if !triggered {
                    return SeedOutcome::Unchanged;
                }
                state.epoch.saturating_add(1)
            }
        };
        if pool.len() < WITNESS_SET_FLOOR_N {
            return SeedOutcome::BelowFloor {
                eligible: pool.len(),
            };
        }

        let seed_key = issuer.epoch_key(self.grid, cell, next_epoch);
        let seed = witness_epoch_seed(&seed_key, self.grid, cell, next_epoch);
        let selected = draw_witness_set(&pool, &seed);
        // The reveal: the key for the epoch this one replaces travels inside
        // this announcement. A coordinator that wants a usable epoch here has
        // no way to keep the previous one closed.
        let prev_seed_key = self.cells.get(&cell).map(|state| state.seed_key);
        self.counter = self.counter.saturating_add(1);
        let claims = WitnessEpochClaimsV1::new(
            self.grid,
            cell,
            next_epoch,
            WitnessEpochClaimsV1::compose_handle(issuer.incarnation(), self.counter),
            self.config.epoch_ms,
            self.config.accept_grace_ms,
            pool.clone(),
            selected,
            witness_epoch_commitment(self.grid, cell, next_epoch, &seed_key),
            prev_seed_key,
            issuer.key_id(),
        );
        let Ok(announcement) = issuer.sign(claims.clone()) else {
            // Encoding cannot fail for claims this module built, and if it
            // ever did, an unsigned epoch is not something to paper over.
            return SeedOutcome::Unchanged;
        };
        self.cells.insert(
            cell,
            CellEpoch {
                epoch: next_epoch,
                seed_key,
                pool,
                announced_ms: now_ms,
            },
        );
        SeedOutcome::Seeded(Box::new(SeededEpoch {
            claims,
            announcement,
            recipients: registry.peers_covering(cell),
        }))
    }
}

/// How many nodes are in exactly one of two sorted sets.
fn symmetric_difference(left: &[NodeId], right: &[NodeId]) -> usize {
    let left: HashSet<&NodeId> = left.iter().collect();
    let right: HashSet<&NodeId> = right.iter().collect();
    left.symmetric_difference(&right).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{
        audit_witness_epoch_draw, verify_witness_epoch, verify_witness_epoch_reveal,
        WitnessEpochSnapshot, WitnessEpochVerificationError, WITNESS_SET_TARGET_N,
    };

    const T0: u64 = 1_000_000;

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

    fn issuer(master: u8) -> WitnessEpochIssuer {
        WitnessEpochIssuer::new(secret(9), IssuerKeyId::new(3), [master; 32], 1)
    }

    /// A registry and a seeder holding `count` peers, each on its own account,
    /// all covering `cell(0)` and all long enough present to be eligible.
    fn populated(count: u8) -> (IslandRegistry, WitnessSeeder) {
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        for index in 1..=count {
            registry.report_presence(node(index), vec![cell(0)]);
            seeder.note_session(
                node(index),
                AccountId::new(u64::from(index)),
                SessionStanding::Good,
                T0,
            );
        }
        (registry, seeder)
    }

    fn seeded(outcome: SeedOutcome) -> SeededEpoch {
        match outcome {
            SeedOutcome::Seeded(epoch) => *epoch,
            other => panic!("expected an announcement, got {other:?}"),
        }
    }

    #[test]
    fn an_announcement_verifies_against_the_advertised_public_key() {
        let issuer = issuer(1);
        let (registry, mut seeder) = populated(12);

        let epoch = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000));

        // A verifier holding only the public half accepts it — the same
        // handout model, and the same one-way key crossing, as a grant.
        let claims = verify_witness_epoch(&epoch.announcement, &[issuer.trusted_key()])
            .expect("a gateway accepts the coordinator's own signature");
        assert_eq!(claims, epoch.claims);
        assert_eq!(claims.cell, cell(0));
        assert_eq!(claims.grid, GridId::ROOT);
        assert_eq!(claims.epoch, 0);
        assert_eq!(claims.selected.len(), WITNESS_SET_TARGET_N);
        assert_eq!(claims.candidates.len(), 12);
        assert!(
            claims.prev_seed_key.is_none(),
            "a cell's first epoch opens nothing"
        );

        // Every recipient is a peer covering the cell, and the courier set is
        // not narrowed to the drawn witnesses.
        assert_eq!(epoch.recipients.len(), 12);

        // A retired key stops being accepted; during an overlap both are.
        let rotated = WitnessEpochIssuer::new(secret(10), IssuerKeyId::new(4), [1u8; 32], 1);
        assert_eq!(
            verify_witness_epoch(&epoch.announcement, &[rotated.trusted_key()]),
            Err(WitnessEpochVerificationError::UnknownIssuer(
                IssuerKeyId::new(3)
            ))
        );
        assert!(verify_witness_epoch(
            &epoch.announcement,
            &[rotated.trusted_key(), issuer.trusted_key()]
        )
        .is_ok());
    }

    #[test]
    fn the_seed_key_is_absent_from_the_announcement_it_secures() {
        // The commitment is published; the key is not. An announcement that
        // shipped its own key would make the next epoch's draw predictable
        // from the moment this one landed.
        let issuer = issuer(1);
        let (registry, mut seeder) = populated(12);
        let epoch = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000));
        let key = issuer.epoch_key(GridId::ROOT, cell(0), 0);

        assert!(
            !epoch
                .announcement
                .windows(key.len())
                .any(|window| window == key),
            "the epoch's own seed key appears in the bytes it secures"
        );
        // And the commitment it did publish is the one that key opens.
        assert!(verify_witness_epoch_reveal(&epoch.claims, &key).is_ok());
    }

    #[test]
    fn the_next_epoch_opens_the_last_one_and_a_third_party_can_recheck_the_draw() {
        // D28 clause (c)'s central claim, end to end. An auditor holding
        // nothing but the two envelopes and the coordinator's public key can
        // recompute epoch e's draw from e's own pool and e+1's revealed key,
        // and must get e's announced set back.
        let issuer = issuer(1);
        let keys = [issuer.trusted_key()];
        let (registry, mut seeder) = populated(12);

        let first = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000));
        let second = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 45_000));

        // Both are authentic on their own, and the second is the successor.
        let a0 = verify_witness_epoch(&first.announcement, &keys).expect("epoch 0 verifies");
        let a1 = verify_witness_epoch(&second.announcement, &keys).expect("epoch 1 verifies");
        assert_eq!((a0.epoch, a1.epoch), (0, 1));
        assert_ne!(a0.handle, a1.handle);

        // The reveal: epoch 1 carries epoch 0's key, and it opens epoch 0's
        // published commitment.
        let revealed = a1
            .prev_seed_key
            .expect("a successor must open its predecessor");
        verify_witness_epoch_reveal(&a0, &revealed).expect("the reveal opens the commitment");

        // And the draw recomputes. This is the half a genuine key alone does
        // not give you: a coordinator that revealed a real key while
        // announcing a hand-picked set fails here and only here.
        audit_witness_epoch_draw(&a0, &revealed).expect("epoch 0's set is the drawn one");

        // A tampered "revealed" key is refused as a failed opening rather
        // than as a bad draw — the two findings are different accusations.
        let mut forged = revealed;
        forged[0] ^= 0xff;
        assert_eq!(
            audit_witness_epoch_draw(&a0, &forged),
            Err(WitnessEpochVerificationError::BadReveal)
        );
        // Epoch 1's own key is still withheld: the chain is one deep, not two.
        assert!(a1.prev_seed_key != Some(issuer.epoch_key(GridId::ROOT, cell(0), 1)));
    }

    #[test]
    fn consecutive_epochs_draw_independently_from_the_same_pool() {
        // If the epoch were not inside the derivation, an attacker holding one
        // revealed key would know every future set for the cell.
        let issuer = issuer(1);
        let (registry, mut seeder) = populated(20);
        let first = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000));
        let second = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 45_000));

        assert_eq!(first.claims.candidates, second.claims.candidates);
        assert_ne!(
            first.claims.selected, second.claims.selected,
            "the same pool one epoch later must not draw the same set"
        );
    }

    #[test]
    fn two_coordinators_with_different_masters_draw_different_sets() {
        // The master secret is what makes the draw the coordinator's and not
        // a public function of the pool. Same cell, same epoch, same peers.
        let (registry, mut left) = populated(20);
        let (_, mut right) = populated(20);
        let one = seeded(left.maybe_seed(&issuer(1), &registry, cell(0), T0 + 10_000));
        let two = seeded(right.maybe_seed(&issuer(2), &registry, cell(0), T0 + 10_000));

        assert_eq!(one.claims.candidates, two.claims.candidates);
        assert_ne!(one.claims.selected, two.claims.selected);
    }

    #[test]
    fn the_reseed_floor_collapses_a_burst_of_triggers_into_one_epoch() {
        // D16's 10 s minimum. Ten triggers inside one interval must produce
        // one epoch, not ten: an unmetered reseed is a colluder's redraw
        // button.
        let issuer = issuer(1);
        let (registry, mut seeder) = populated(12);
        assert!(matches!(
            seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000),
            SeedOutcome::Seeded(_)
        ));

        for step in 1..=9 {
            assert_eq!(
                seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000 + step * 1_000),
                SeedOutcome::Cooling,
                "a reseed landed inside the floor"
            );
        }
        assert_eq!(seeder.current_epoch(cell(0)), Some(0));

        // Past the floor, an unchanged pool inside the epoch is not a reason
        // to redraw either.
        assert_eq!(
            seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 21_000),
            SeedOutcome::Unchanged
        );
        // Elapsed time alone rolls it.
        assert!(matches!(
            seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 41_000),
            SeedOutcome::Seeded(_)
        ));
        assert_eq!(seeder.current_epoch(cell(0)), Some(1));
    }

    #[test]
    fn heavy_churn_rolls_the_epoch_before_it_would_have_elapsed() {
        let issuer = issuer(1);
        let (mut registry, mut seeder) = populated(8);
        assert!(matches!(
            seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000),
            SeedOutcome::Seeded(_)
        ));

        // Five newcomers against a pool of eight is over half the pool
        // changed, which is a reseed even though the epoch has 15 s to run.
        for index in 20..25 {
            registry.report_presence(node(index), vec![cell(0)]);
            seeder.note_session(
                node(index),
                AccountId::new(u64::from(index)),
                SessionStanding::Good,
                T0 + 10_000,
            );
        }
        assert!(matches!(
            seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 25_000),
            SeedOutcome::Seeded(_)
        ));
        assert_eq!(seeder.current_epoch(cell(0)), Some(1));
    }

    #[test]
    fn a_pool_below_the_floor_produces_no_announcement_at_all() {
        // Four witnesses that verify as if they were seven is the collusion
        // hole; a short set is refused rather than shipped.
        let issuer = issuer(1);
        let (registry, mut seeder) = populated(4);
        assert_eq!(
            seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000),
            SeedOutcome::BelowFloor { eligible: 4 }
        );
        assert_eq!(seeder.current_epoch(cell(0)), None);
    }

    #[test]
    fn eligibility_drops_what_the_signed_facts_say_to_drop() {
        let issuer = issuer(1);
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);

        // Eight peers cover the cell. One is quarantined, one has only just
        // arrived, and two share an account with peers already in the pool.
        for index in 1..=8u8 {
            registry.report_presence(node(index), vec![cell(0)]);
        }
        for index in 1..=4u8 {
            seeder.note_session(
                node(index),
                AccountId::new(u64::from(index)),
                SessionStanding::Good,
                T0,
            );
        }
        seeder.note_session(node(5), AccountId::new(5), SessionStanding::Quarantined, T0);
        seeder.note_session(
            node(6),
            AccountId::new(6),
            SessionStanding::Good,
            T0 + 5_000,
        );
        seeder.note_session(node(7), AccountId::new(1), SessionStanding::Good, T0);
        seeder.note_session(node(8), AccountId::new(2), SessionStanding::Good, T0);

        let pool = seeder.eligible_pool(&registry, cell(0), T0 + 10_000);
        assert_eq!(
            pool.len(),
            4,
            "quarantine, probation and dedup each cost a slot"
        );
        assert!(
            !pool.contains(&node(5)),
            "a quarantined account cannot witness"
        );
        assert!(
            !pool.contains(&node(6)),
            "5 s of presence is under the 10 s the filter asks for"
        );
        // One slot per account: whichever NodeId is lower survives, and the
        // other is gone. Which one is not the point; that only one is, is.
        for (first, second) in [(node(1), node(7)), (node(2), node(8))] {
            assert_eq!(
                usize::from(pool.contains(&first)) + usize::from(pool.contains(&second)),
                1,
                "two NodeIds on one account took two witness slots"
            );
        }
        // Four eligible is below the floor, so this cell gets no epoch — the
        // filters and the floor are the same refusal seen from two sides.
        assert_eq!(
            seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000),
            SeedOutcome::BelowFloor { eligible: 4 }
        );

        // A peer that never presented a token is not in the pool either: the
        // coordinator seeds from what it has verified, not from what it can
        // see on a socket.
        let mut anonymous = IslandRegistry::new();
        anonymous.report_presence(node(30), vec![cell(0)]);
        assert!(WitnessSeeder::new(GridId::ROOT)
            .eligible_pool(&anonymous, cell(0), T0 + 10_000)
            .is_empty());
    }

    #[test]
    fn a_bouncing_account_forfeits_draws_rather_than_buying_one() {
        // The anti-grind cooldown: leaving to force a redraw takes the leaver
        // out of the pool for six reseed intervals, so it is strictly losing.
        let (registry, mut seeder) = populated(12);
        assert!(seeder
            .eligible_pool(&registry, cell(0), T0 + 10_000)
            .contains(&node(3)));

        seeder.forget_session(node(3), T0 + 10_000);
        seeder.note_session(
            node(3),
            AccountId::new(3),
            SessionStanding::Good,
            T0 + 11_000,
        );

        // Back on the socket, still out of the pool — and still out of it a
        // whole epoch length later.
        assert!(!seeder
            .eligible_pool(&registry, cell(0), T0 + 25_000)
            .contains(&node(3)));
        assert!(!seeder
            .eligible_pool(&registry, cell(0), T0 + 69_000)
            .contains(&node(3)));
        // 60 s after the loss, it is eligible again.
        assert!(seeder
            .eligible_pool(&registry, cell(0), T0 + 71_000)
            .contains(&node(3)));
    }

    #[test]
    fn an_intent_is_judged_against_the_epoch_it_names_not_the_current_one() {
        // D28 clause (g). Two epochs run for one cell; an intent naming the
        // first is checked against the first's announced set even after the
        // second has landed, and a witness that has left the cell entirely is
        // still a valid signer for the epoch it signed under.
        let issuer = issuer(1);
        let keys = [issuer.trusted_key()];
        let (registry, mut seeder) = populated(20);

        let first = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000));
        let second = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 45_000));
        let a0 = verify_witness_epoch(&first.announcement, &keys).expect("epoch 0 verifies");
        let a1 = verify_witness_epoch(&second.announcement, &keys).expect("epoch 1 verifies");

        // The sets differ, which is what makes the question meaningful.
        let only_in_first: Vec<NodeId> = a0
            .selected
            .iter()
            .filter(|node| !a1.selected.contains(node))
            .copied()
            .collect();
        assert!(
            !only_in_first.is_empty(),
            "two independent draws over 20 candidates should not coincide"
        );

        // A gateway localizes each announcement against its own clock as it
        // accepts it, and resolves an intent by the handle it names.
        let epoch0 = WitnessEpochSnapshot::from_claims(a0.clone(), 100_000);
        let epoch1 = WitnessEpochSnapshot::from_claims(a1.clone(), 135_000);
        assert_ne!(epoch0.handle, epoch1.handle);

        // An attestation from a witness that only epoch 0 named is admissible
        // under epoch 0 and inadmissible under epoch 1 — judged against the
        // announced set, never against current presence.
        let witness = only_in_first[0];
        assert!(epoch0.admits(&witness));
        assert!(!epoch1.admits(&witness));

        // In flight across the boundary: epoch 1 landing does nothing to
        // epoch 0's window, which runs epoch_ms + accept_grace_ms from when
        // *this* process first saw it.
        assert!(
            epoch0.usable_at(136_000),
            "a newer epoch does not retire an older one"
        );
        assert!(epoch0.usable_at(159_999));
        assert!(
            !epoch0.usable_at(160_000),
            "past the grace it is stale, not forged"
        );
        assert!(epoch1.usable_at(160_000));
    }

    #[test]
    fn each_cell_gets_its_own_epoch_and_its_own_draw() {
        // The binding is per (grid, cell): two cells seeded at the same
        // instant from the same peers must not share a set, or a witness set
        // would be chosen by a population that is not the cell's.
        let issuer = issuer(1);
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        for index in 1..=12u8 {
            registry.report_presence(node(index), vec![cell(0), cell(1)]);
            seeder.note_session(
                node(index),
                AccountId::new(u64::from(index)),
                SessionStanding::Good,
                T0,
            );
        }

        let left = seeded(seeder.maybe_seed(&issuer, &registry, cell(0), T0 + 10_000));
        let right = seeded(seeder.maybe_seed(&issuer, &registry, cell(1), T0 + 10_000));
        assert_eq!(left.claims.candidates, right.claims.candidates);
        assert_ne!(left.claims.selected, right.claims.selected);
        assert_ne!(left.claims.handle, right.claims.handle);
        assert_eq!(left.claims.epoch, right.claims.epoch);
    }
}
