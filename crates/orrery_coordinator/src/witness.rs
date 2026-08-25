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
    witness_epoch_binding, AccountId, AccountStandings, CellId, GridId, IssuerKey, IssuerKeyId,
    NodeId, SessionStanding, SessionTokenClaimsV1, UnixMillis,
};

use crate::registry::IslandRegistry;
use crate::StrikesMode;

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
    /// The token's signed `on_probation`, D28 clause (e)'s account-age row.
    ///
    /// Lifted from the same signature as `account` and `standing` and for the
    /// same reason: the coordinator cannot read `da ‖ account` — D31 clause (d)
    /// gives it no FoundationDB at all — so the only trustworthy answer to "is
    /// this account past its probation window" is the one identity signed.
    on_probation: bool,
    /// The `issued_at_ms` the session's token was signed with.
    ///
    /// Retained for exactly one purpose: it is the left-hand side of the
    /// watermark comparison in
    /// [`AccountStandings::pending`](orrery_protocol::AccountStandings::pending),
    /// so a session that re-handshook with a token identity signed *after* an
    /// assertion is not walked backwards by it. Without it a reconnect during
    /// a lifted quarantine would be re-quarantined by a stale assertion.
    issued_at_ms: UnixMillis,
    joined_ms: u64,
    /// Set when the session was admitted on docs/09 §8's token grace, so its
    /// `standing` is only as fresh as an expired token. Such a peer plays;
    /// it does not witness. See [`WitnessSeeder::note_grace_session`].
    graced: bool,
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
    /// a peer that keeps talking must not keep resetting its own presence
    /// timer.
    ///
    /// The whole verified claims value rather than a field list, so that every
    /// eligibility filter reads something the caller could only have obtained
    /// from `SessionTokenVerifier::verify`. `node` stays a separate argument
    /// because it is the *connected* remote — `server.rs` checks it against
    /// `claims.node` before it gets here, and passing it separately keeps that
    /// check somewhere a reader can see it.
    pub fn note_session(&mut self, node: NodeId, claims: &SessionTokenClaimsV1, now_ms: u64) {
        self.insert_session(node, claims, now_ms, false);
    }

    /// Record a session admitted on token grace (docs/09 §8) as ineligible.
    ///
    /// The account is kept — the cooldown in [`Self::forget_session`] is
    /// keyed by it, and dropping it would let a grace reconnect launder one
    /// away — but the session never enters a candidate pool.
    ///
    /// D28 clause (e) reads witness eligibility off the token's *signed*
    /// `standing`, and an expired token's standing is exactly as stale as the
    /// identity outage is long: a quarantine applied during the outage is
    /// invisible, and so is one lifted. Both directions are invisible, so the
    /// choice is which mistake to make, and the safe one is refusing to
    /// witness rather than seating a quarantined account on a set that judges
    /// intents. The cost is a smaller pool during an outage, which D29's
    /// low-population path already covers and clause (g)'s floor already
    /// refuses to paper over with a short set.
    pub fn note_grace_session(&mut self, node: NodeId, claims: &SessionTokenClaimsV1, now_ms: u64) {
        self.insert_session(node, claims, now_ms, true);
    }

    /// The insert both note-calls share: first writer wins, every eligibility
    /// field lifted from the same verified signature.
    fn insert_session(
        &mut self,
        node: NodeId,
        claims: &SessionTokenClaimsV1,
        now_ms: u64,
        graced: bool,
    ) {
        self.sessions.entry(node).or_insert(SessionFacts {
            account: claims.account,
            standing: claims.standing,
            on_probation: claims.on_probation,
            issued_at_ms: claims.issued_at_ms,
            joined_ms: now_ms,
            graced,
        });
    }

    /// Drain identity's standing assertions and move the sessions they name,
    /// leaving everything else about those sessions alone (D33 clause (e)).
    ///
    /// Returns the sessions moved, for the caller's log. Both directions are
    /// this one call, because the watermark rule in
    /// [`AccountStandings::pending`](orrery_protocol::AccountStandings::pending)
    /// is direction-free: a quarantine applied drops the account out of the
    /// next [`Self::eligible_pool`] without waiting for a `Hello`, and a
    /// quarantine lifted lets it back in on the same terms as any other peer.
    ///
    /// `mode` is control C5's posture — the same cell `StandingState` holds
    /// for the cooldown/ban half, not a second lever. `Off` consults nothing,
    /// `Shadow` evaluates fully and counts, `Live` applies.
    ///
    /// # Why this is not `note_session` with a different `or_insert`
    ///
    /// [`Self::note_session`] inserts and never updates, on purpose: `joined_ms`
    /// is what "present in the island ≥ 10 s" is measured from, and a peer that
    /// keeps talking must not keep resetting its own presence timer. Widening
    /// it to an upsert would hand a peer that reset for free — and a standing
    /// change is *account*-addressed, arriving from identity rather than from
    /// the peer, so it is not that call's shape at all. This updates one field
    /// of an existing row and creates none: an account with no session here is
    /// simply not this coordinator's problem, and the next `note_session` will
    /// carry a token identity signed after the change anyway.
    ///
    /// `graced` is untouched for the same reason it is set: a session admitted
    /// on an expired token still does not witness, whatever its standing now
    /// says.
    pub fn apply_standing_updates(
        &mut self,
        standings: &AccountStandings,
        mode: StrikesMode,
    ) -> Vec<(NodeId, SessionStanding)> {
        // D32 clause (b): "Off observes nothing." Not even the poll — which
        // costs nothing to honour and means a promotion converges on its first
        // evaluated tick with the queue intact.
        if mode == StrikesMode::Off {
            return Vec::new();
        }
        standings.poll();
        let mut moved = Vec::new();
        for (node, facts) in &mut self.sessions {
            let Some(resolved) =
                standings.pending(facts.account, facts.issued_at_ms, facts.standing)
            else {
                continue;
            };
            if mode == StrikesMode::Shadow {
                standings.record_observed();
                continue;
            }
            standings.record_applied();
            facts.standing = resolved;
            moved.push((*node, resolved));
        }
        moved
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
    /// - **account age past probation** — the token's signed `on_probation`,
    ///   so an account bought this morning cannot witness this afternoon;
    /// - **presence ≥ `min_presence_ms`** — timed against this coordinator's
    ///   own session;
    /// - **one slot per account** — dedup on the signed `account`, taking the
    ///   lowest `NodeId` so the choice is deterministic;
    /// - **not on cooldown** — the anti-grind exclusion above;
    /// - **not admitted on token grace** — an expired token's `standing` is
    ///   as stale as the outage is long ([`Self::note_grace_session`]).
    ///
    /// And, said out loud because a silent no-op reads like enforcement: the
    /// strike-score threshold is still **approximated** by the coarse
    /// quarantine flag (D33 clause (f) declines to widen the token for it), and
    /// per-account exclusion holds only **within this coordinator**. The `id/`
    /// rows that map an account to every NodeId bound to it now exist and are
    /// written (`orrery_identity`, D31 clause (a)), but D31 clause (d) bars
    /// this process from reading them — "the coordinator reads nothing: every
    /// candidate it seeds from has a live token-verified session, so its
    /// account is already in hand" — and D31's Consequences say the durable
    /// index would not close the gap anyway: a NodeId connected to a
    /// *different* coordinator is absent from this pool entirely, so no lookup
    /// over this pool can exclude it.
    ///
    /// So the residual, exactly: a Sybil whose same-account NodeIds hold
    /// sessions on **concurrently seeding coordinators** (separate regions or
    /// incarnations) can hold one witness slot per coordinator, and party
    /// exclusion over an account's *unconnected* NodeIds is not evaluated here
    /// — the gateway's admission-time check (D31 clauses (e)/(f), #211) is the
    /// half that reads the durable map. Collusion across *different* paid
    /// accounts is a separate miss no binding table answers. `docs/09` §3's
    /// reference topology runs one coordinator (a warm standby does not seed,
    /// and scaled production stays "logically one per universe"), which is why
    /// the per-coordinator boundary is where the accepted records drew the
    /// line; D28 clause (e)'s table grades each row accordingly.
    ///
    /// Probation is enforced from the signed field and is therefore exactly as
    /// fresh as the token: an account that crossed its window mid-session is
    /// still excluded until it refreshes, at most one hour late
    /// (`MAX_SESSION_TOKEN_TTL_MS`) and in practice half that. Late in the
    /// direction that excludes.
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
            if facts.standing != SessionStanding::Good || facts.graced {
                continue;
            }
            // D28 clause (e)'s account-age row, and the identity half of
            // `docs/07` §6's four-term collusion cost: a colluding pod has to
            // wait out the probation window before any of its accounts can
            // reach a witness slot, so the cost of a burned account is money
            // *and* time rather than money alone.
            if facts.on_probation {
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
    /// The `issued_at_ms` these fixtures' tokens carry. A separate clock from
    /// [`T0`] on purpose: presence is timed on the coordinator's monotonic
    /// clock and token issuance on a wall clock, and the standing watermark
    /// compares against the second one.
    const TOKEN_ISSUED: UnixMillis = UnixMillis::new(500_000);

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

    /// The verified claims a `Hello` would have handed the seeder, past
    /// probation. Stated rather than minted: the seeder consumes a value the
    /// verifier already returned, and a fixture that signed one would be
    /// testing `orrery_protocol`'s verifier a second time.
    fn session_claims(
        node: NodeId,
        account: AccountId,
        standing: SessionStanding,
        issued_at_ms: UnixMillis,
    ) -> SessionTokenClaimsV1 {
        SessionTokenClaimsV1::new(
            account,
            node,
            issued_at_ms,
            orrery_protocol::SessionTokenTtlMs::new(orrery_protocol::MAX_SESSION_TOKEN_TTL_MS),
            standing,
            IssuerKeyId::new(3),
            false,
        )
    }

    /// The same, for an account still inside its probation window.
    fn probationary_claims(
        node: NodeId,
        account: AccountId,
        standing: SessionStanding,
        issued_at_ms: UnixMillis,
    ) -> SessionTokenClaimsV1 {
        let mut claims = session_claims(node, account, standing, issued_at_ms);
        claims.on_probation = true;
        claims
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
                &session_claims(
                    node(index),
                    AccountId::new(u64::from(index)),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
                T0,
            );
        }
        (registry, seeder)
    }

    /// A consumer plus its publisher half. The posture is C5's and is passed
    /// to `apply_standing_updates` per call, not held here — see that method.
    fn standings() -> (
        std::sync::Arc<orrery_protocol::QueuedStandingUpdates>,
        AccountStandings,
    ) {
        let feed = std::sync::Arc::new(orrery_protocol::QueuedStandingUpdates::new());
        let consumer =
            AccountStandings::new(std::sync::Arc::clone(&feed)
                as std::sync::Arc<dyn orrery_protocol::StandingUpdateFeed>);
        (feed, consumer)
    }

    fn standing_update(
        account: u64,
        standing: SessionStanding,
        effective_from_ms: u64,
    ) -> orrery_protocol::AccountStandingUpdate {
        orrery_protocol::AccountStandingUpdate {
            account: AccountId::new(account),
            standing,
            effective_from_ms: UnixMillis::new(effective_from_ms),
        }
    }

    /// D28 clause (e) reads witness eligibility off a session's standing, so a
    /// quarantine filed mid-session has to leave the pool without waiting for
    /// a `Hello` that a peer holding its connection open may never send.
    #[test]
    fn a_quarantine_applied_mid_session_leaves_the_eligible_pool() {
        let (registry, mut seeder) = populated(12);
        let now = T0 + 20_000;
        assert!(seeder
            .eligible_pool(&registry, cell(0), now)
            .contains(&node(3)));

        let (feed, consumer) = standings();
        feed.publish(standing_update(
            3,
            SessionStanding::Quarantined,
            TOKEN_ISSUED.0 + 1,
        ));
        assert_eq!(
            seeder.apply_standing_updates(&consumer, StrikesMode::Live),
            vec![(node(3), SessionStanding::Quarantined)]
        );

        let pool = seeder.eligible_pool(&registry, cell(0), now);
        assert!(!pool.contains(&node(3)));
        assert_eq!(pool.len(), 11, "only the struck account leaves");
    }

    /// The lift direction, and the reason it is not optional: an account still
    /// excluded from every draw after its quarantine expired is being punished
    /// by a cache.
    #[test]
    fn a_lifted_quarantine_returns_an_account_to_the_pool() {
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        registry.report_presence(node(1), vec![cell(0)]);
        seeder.note_session(
            node(1),
            &session_claims(
                node(1),
                AccountId::new(1),
                SessionStanding::Quarantined,
                TOKEN_ISSUED,
            ),
            T0,
        );
        let now = T0 + 20_000;
        assert!(seeder.eligible_pool(&registry, cell(0), now).is_empty());

        let (feed, consumer) = standings();
        feed.publish(standing_update(
            1,
            SessionStanding::Good,
            TOKEN_ISSUED.0 + 1,
        ));
        seeder.apply_standing_updates(&consumer, StrikesMode::Live);

        assert_eq!(seeder.eligible_pool(&registry, cell(0), now), vec![node(1)]);
    }

    /// `joined_ms` is what "present in the island ≥ 10 s" is measured from, and
    /// a standing update must not reset it in either direction.
    ///
    /// Read through the only observable that depends on it: a peer that has
    /// been present for `min_presence_ms` stays eligible across an update, and
    /// would drop straight back out of the pool if the update had restamped
    /// its arrival. The lift arm is the one that could plausibly have been
    /// written as a re-`note_session`, which is exactly the mistake.
    #[test]
    fn a_standing_update_does_not_reset_joined_ms() {
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        registry.report_presence(node(1), vec![cell(0)]);
        seeder.note_session(
            node(1),
            &session_claims(
                node(1),
                AccountId::new(1),
                SessionStanding::Quarantined,
                TOKEN_ISSUED,
            ),
            T0,
        );
        // Exactly at the presence threshold: one millisecond of reset would
        // show.
        let now = T0 + seeder.config.min_presence_ms;

        let (feed, consumer) = standings();
        feed.publish(standing_update(
            1,
            SessionStanding::Good,
            TOKEN_ISSUED.0 + 1,
        ));
        seeder.apply_standing_updates(&consumer, StrikesMode::Live);

        assert_eq!(
            seeder.eligible_pool(&registry, cell(0), now),
            vec![node(1)],
            "the peer's presence must be measured from its arrival, not from the update"
        );
    }

    /// A session whose token identity signed *after* the assertion is not
    /// walked backwards by it — the reconnect case `issued_at_ms` exists for.
    #[test]
    fn a_session_on_a_newer_token_is_not_moved_by_an_older_assertion() {
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        registry.report_presence(node(1), vec![cell(0)]);
        seeder.note_session(
            node(1),
            &session_claims(
                node(1),
                AccountId::new(1),
                SessionStanding::Good,
                UnixMillis::new(TOKEN_ISSUED.0 + 10_000),
            ),
            T0,
        );

        let (feed, consumer) = standings();
        feed.publish(standing_update(
            1,
            SessionStanding::Quarantined,
            TOKEN_ISSUED.0 + 1,
        ));
        assert!(seeder
            .apply_standing_updates(&consumer, StrikesMode::Live)
            .is_empty());
        assert_eq!(
            seeder.eligible_pool(&registry, cell(0), T0 + 20_000),
            vec![node(1)]
        );
    }

    /// The ramp's observing position, at the coordinator: the pool is
    /// unchanged and the counter is not.
    #[test]
    fn in_shadow_the_pool_does_not_move_and_the_counter_does() {
        let (registry, mut seeder) = populated(12);
        let (feed, consumer) = standings();
        feed.publish(standing_update(
            3,
            SessionStanding::Quarantined,
            TOKEN_ISSUED.0 + 1,
        ));

        // Off first: D32 clause (b) says it observes nothing, not even the
        // poll, so the queue survives to be drained by the next posture.
        assert!(seeder
            .apply_standing_updates(&consumer, StrikesMode::Off)
            .is_empty());
        assert_eq!(consumer.observed(), 0);

        assert!(seeder
            .apply_standing_updates(&consumer, StrikesMode::Shadow)
            .is_empty());
        assert!(seeder
            .eligible_pool(&registry, cell(0), T0 + 20_000)
            .contains(&node(3)));
        assert_eq!(consumer.observed(), 1);
        assert_eq!(consumer.applied(), 0);

        // Promotion needs no reconnect: the same session moves next tick, on
        // the same C5 cell that governs the termination half.
        assert_eq!(
            seeder.apply_standing_updates(&consumer, StrikesMode::Live),
            vec![(node(3), SessionStanding::Quarantined)]
        );
        assert!(!seeder
            .eligible_pool(&registry, cell(0), T0 + 20_000)
            .contains(&node(3)));
    }

    /// A graced session does not witness whatever its standing now says, so a
    /// lift reaching it must not seat it (`note_grace_session`'s reasoning).
    #[test]
    fn lifting_a_graced_sessions_quarantine_still_does_not_seat_it() {
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        registry.report_presence(node(1), vec![cell(0)]);
        seeder.note_grace_session(
            node(1),
            &session_claims(
                node(1),
                AccountId::new(1),
                SessionStanding::Quarantined,
                TOKEN_ISSUED,
            ),
            T0,
        );

        let (feed, consumer) = standings();
        feed.publish(standing_update(
            1,
            SessionStanding::Good,
            TOKEN_ISSUED.0 + 1,
        ));
        assert_eq!(
            seeder.apply_standing_updates(&consumer, StrikesMode::Live),
            vec![(node(1), SessionStanding::Good)]
        );
        assert!(seeder
            .eligible_pool(&registry, cell(0), T0 + 20_000)
            .is_empty());
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
                &session_claims(
                    node(index),
                    AccountId::new(u64::from(index)),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
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
    fn a_session_admitted_on_token_grace_plays_but_does_not_witness() {
        // docs/09 §8 keeps an established peer connected through an identity
        // outage. What it cannot do is refresh the `standing` D28 clause (e)
        // filters on, so the coordinator declines to seat that peer on a set
        // rather than trusting an hour-old quarantine flag.
        let mut registry = IslandRegistry::new();
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        for index in 1..=6u8 {
            registry.report_presence(node(index), vec![cell(0)]);
            seeder.note_session(
                node(index),
                &session_claims(
                    node(index),
                    AccountId::new(u64::from(index)),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
                T0,
            );
        }
        assert_eq!(
            seeder.eligible_pool(&registry, cell(0), T0 + 10_000).len(),
            6
        );

        // The same peers, one of them back on grace instead. Its token said
        // `Good` when identity last spoke, and that is exactly the claim this
        // filter refuses to keep believing.
        let mut graced = WitnessSeeder::new(GridId::ROOT);
        for index in 1..=5u8 {
            graced.note_session(
                node(index),
                &session_claims(
                    node(index),
                    AccountId::new(u64::from(index)),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
                T0,
            );
        }
        graced.note_grace_session(
            node(6),
            &session_claims(
                node(6),
                AccountId::new(6),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
            T0,
        );

        let pool = graced.eligible_pool(&registry, cell(0), T0 + 10_000);
        assert_eq!(pool.len(), 5, "the graced session costs its own slot only");
        assert!(
            !pool.contains(&node(6)),
            "a session running on an expired token's standing must not witness"
        );
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
                &session_claims(
                    node(index),
                    AccountId::new(u64::from(index)),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
                T0,
            );
        }
        seeder.note_session(
            node(5),
            &session_claims(
                node(5),
                AccountId::new(5),
                SessionStanding::Quarantined,
                TOKEN_ISSUED,
            ),
            T0,
        );
        seeder.note_session(
            node(6),
            &session_claims(
                node(6),
                AccountId::new(6),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
            T0 + 5_000,
        );
        seeder.note_session(
            node(7),
            &session_claims(
                node(7),
                AccountId::new(1),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
            T0,
        );
        seeder.note_session(
            node(8),
            &session_claims(
                node(8),
                AccountId::new(2),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
            T0,
        );

        let pool = seeder.eligible_pool(&registry, cell(0), T0 + 10_000);
        assert_eq!(
            pool.len(),
            4,
            "quarantine, presence and dedup each cost a slot"
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
    fn probation_closes_the_pool_to_a_fresh_account_and_opens_it_once_past() {
        // D28 clause (e)'s account-age row, which that record graded *skipped*
        // until the token carried it. Both directions are asserted from one
        // fixture: the refusal alone would pass just as well against a filter
        // that excluded everybody.
        let (mut registry, mut seeder) = populated(12);
        registry.report_presence(node(20), vec![cell(0)]);
        registry.report_presence(node(21), vec![cell(0)]);
        seeder.note_session(
            node(20),
            &probationary_claims(
                node(20),
                AccountId::new(20),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
            T0,
        );
        seeder.note_session(
            node(21),
            &session_claims(
                node(21),
                AccountId::new(21),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
            T0,
        );

        let pool = seeder.eligible_pool(&registry, cell(0), T0 + 10_000);

        // Everything else about the two is identical — same cell, same arrival
        // instant, same `Good` standing, neither on cooldown, neither graced,
        // one account each — so the only thing that can separate them is the
        // signed probation flag.
        assert!(
            !pool.contains(&node(20)),
            "an account inside its probation window must not be witness-eligible"
        );
        assert!(
            pool.contains(&node(21)),
            "an account past its probation window must be witness-eligible"
        );
    }

    #[test]
    fn probation_is_read_from_the_signed_claim_and_not_from_time_on_the_socket() {
        // The house rule that an authenticated value is never substituted by
        // anything the peer supplies alongside it. A session first seen on
        // probation stays out however long it stands there: waiting is not a
        // way past the field, only a fresher token minted by identity is.
        let (registry, _) = populated(12);
        let mut seeder = WitnessSeeder::new(GridId::ROOT);
        for index in 1..=12u8 {
            seeder.note_session(
                node(index),
                &probationary_claims(
                    node(index),
                    AccountId::new(u64::from(index)),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
                T0,
            );
        }

        assert!(
            seeder
                .eligible_pool(&registry, cell(0), T0 + 10 * 60 * 1_000)
                .is_empty(),
            "ten minutes on the socket is not seven days of account age"
        );
        assert_eq!(
            seeder.maybe_seed(&issuer(1), &registry, cell(0), T0 + 10 * 60 * 1_000),
            SeedOutcome::BelowFloor { eligible: 0 },
            "a cell of nothing but fresh accounts seeds no epoch at all"
        );
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
            &session_claims(
                node(3),
                AccountId::new(3),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
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

    /// D28 clause (e)'s one-slot-per-account row, with the dedup as the
    /// **only** refuser. Every other filter passes for the doubled NodeId —
    /// proven by a control peer treated identically except for the shared
    /// account, which *is* seated — so deleting the dedup admits both and
    /// this test alone catches it, rather than some other clause refusing
    /// the fixture for its own reasons.
    #[test]
    fn one_account_on_two_live_sessions_holds_one_slot_and_dedup_is_the_only_refuser() {
        let (mut registry, mut seeder) = populated(8);
        // node(40) presents a verified token for account 3 — already held by
        // node(3) — and node(41) an identical token for a fresh account. The
        // two differ in nothing but the account their signed claims carry.
        for (peer, account) in [(node(40), 3u64), (node(41), 41u64)] {
            registry.report_presence(peer, vec![cell(0)]);
            seeder.note_session(
                peer,
                &session_claims(
                    peer,
                    AccountId::new(account),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
                T0,
            );
        }

        let pool = seeder.eligible_pool(&registry, cell(0), T0 + 10_000);
        assert!(
            pool.contains(&node(41)),
            "the control peer proves every other filter passes for this shape"
        );
        let doubled = [node(3), node(40)];
        assert_eq!(
            usize::from(pool.contains(&doubled[0])) + usize::from(pool.contains(&doubled[1])),
            1,
            "one account on two live sessions took two witness slots"
        );
        // And the survivor is the deterministic one: the lowest NodeId.
        let lower = *doubled.iter().min_by_key(|n| *n.as_bytes()).unwrap();
        assert!(
            pool.contains(&lower),
            "the dedup kept the higher NodeId; the tiebreak is not deterministic"
        );
    }

    /// The reconnect trick the cooldown map's doc names: a Sybil cannot dodge
    /// its own cooldown by reconnecting under a second NodeId, because the
    /// cooldown is keyed by the token's signed account and the fresh NodeId's
    /// token carries the same account. A control peer in the identical
    /// position on a fresh account is seated, so the cooldown is the only
    /// refuser here.
    #[test]
    fn a_cooldown_bars_a_fresh_nodeid_carrying_the_cooled_accounts_token() {
        let (mut registry, mut seeder) = populated(12);
        // Account 3 bounces: its session drops, putting the *account* on
        // cooldown until T0 + 70_000.
        seeder.forget_session(node(3), T0 + 10_000);
        // It returns under a NodeId this coordinator has never seen a `Hello`
        // from — but the token still names account 3, because identity signed
        // it. node(51) is the control: same arrival, fresh account.
        for (peer, account) in [(node(50), 3u64), (node(51), 51u64)] {
            registry.report_presence(peer, vec![cell(0)]);
            seeder.note_session(
                peer,
                &session_claims(
                    peer,
                    AccountId::new(account),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
                T0 + 11_000,
            );
        }

        // Presence is satisfied for both; only the cooldown separates them.
        let pool = seeder.eligible_pool(&registry, cell(0), T0 + 25_000);
        assert!(pool.contains(&node(51)), "the control peer must be seated");
        assert!(
            !pool.contains(&node(50)),
            "a second NodeId dodged its account's cooldown by reconnecting"
        );
        // Once the account's cooldown lapses, the new NodeId is eligible on
        // the same terms as anyone — the exclusion is a cooldown, not a ban.
        assert!(seeder
            .eligible_pool(&registry, cell(0), T0 + 71_000)
            .contains(&node(50)));
    }

    /// D31 clause (f)'s direction, applied at the seeder: a candidate whose
    /// account binding this coordinator cannot establish — no token-verified
    /// session, which per D31 clause (d) is the only binding source the
    /// coordinator has — is excluded, never admitted. The same peer is seated
    /// the moment a verified session supplies the binding, so the missing
    /// binding is the only refuser.
    #[test]
    fn an_unresolvable_binding_excludes_and_never_admits() {
        let (mut registry, mut seeder) = populated(8);
        // node(60) covers the cell — presence says it is there — but it has
        // never presented a session token, so no signed fact binds it to any
        // account.
        registry.report_presence(node(60), vec![cell(0)]);
        assert!(
            !seeder
                .eligible_pool(&registry, cell(0), T0 + 10_000)
                .contains(&node(60)),
            "a candidate with no verified account binding was admitted"
        );
        // Supplying the binding — and nothing else — seats it.
        seeder.note_session(
            node(60),
            &session_claims(
                node(60),
                AccountId::new(60),
                SessionStanding::Good,
                TOKEN_ISSUED,
            ),
            T0,
        );
        assert!(seeder
            .eligible_pool(&registry, cell(0), T0 + 10_000)
            .contains(&node(60)));
    }

    /// The pool is a pure function of its inputs (the design commitment on
    /// [`WitnessSeeder`]): the same facts produce byte-for-byte the same
    /// vector regardless of insertion order and across repeated calls, with
    /// the same-account tiebreak landing on the same NodeId both ways. D28
    /// clause (c)'s audit replays the draw from the announced pool, so a pool
    /// that depended on iteration order would unmake the audit.
    #[test]
    fn the_pool_is_deterministic_across_insertion_orders_and_repeated_calls() {
        // Six accounts, one of them (account 2) on two NodeIds.
        let facts: Vec<(NodeId, u64)> = vec![
            (node(1), 1),
            (node(2), 2),
            (node(3), 3),
            (node(4), 4),
            (node(5), 5),
            (node(6), 6),
            (node(20), 2),
        ];
        let build = |order: &[(NodeId, u64)]| {
            let mut registry = IslandRegistry::new();
            let mut seeder = WitnessSeeder::new(GridId::ROOT);
            for (peer, account) in order {
                registry.report_presence(*peer, vec![cell(0)]);
                seeder.note_session(
                    *peer,
                    &session_claims(
                        *peer,
                        AccountId::new(*account),
                        SessionStanding::Good,
                        TOKEN_ISSUED,
                    ),
                    T0,
                );
            }
            seeder.eligible_pool(&registry, cell(0), T0 + 10_000)
        };
        let ascending = build(&facts);
        let reversed: Vec<_> = facts.iter().rev().copied().collect();
        let descending = build(&reversed);
        assert_eq!(
            ascending, descending,
            "the pool depends on session insertion order"
        );
        assert_eq!(ascending, build(&facts), "repeated calls disagree");
        let mut sorted = ascending.clone();
        sorted.sort_by_key(|n| *n.as_bytes());
        assert_eq!(
            ascending, sorted,
            "the pool is not in ascending NodeId order"
        );
        // The tiebreak survivor is the byte-lower of the doubled pair.
        let lower = *[node(2), node(20)]
            .iter()
            .min_by_key(|n| *n.as_bytes())
            .unwrap();
        assert_eq!(ascending.len(), 6);
        assert!(ascending.contains(&lower));
    }

    /// The ordinary case is provably unperturbed: every candidate on its own
    /// account, all connected here, all past every filter — the pool is
    /// exactly the candidates, in ascending NodeId order, nobody deduped.
    #[test]
    fn an_all_distinct_connected_pool_seats_every_candidate() {
        let (registry, seeder) = populated(12);
        let pool = seeder.eligible_pool(&registry, cell(0), T0 + 10_000);
        let mut expected: Vec<NodeId> = (1..=12u8).map(node).collect();
        expected.sort_by_key(|n| *n.as_bytes());
        assert_eq!(pool, expected);
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
                &session_claims(
                    node(index),
                    AccountId::new(u64::from(index)),
                    SessionStanding::Good,
                    TOKEN_ISSUED,
                ),
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
