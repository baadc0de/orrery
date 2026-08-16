//! Contact-island weak-authority propagation (D7 §5).
//!
//! Weak authority is acquired by *interaction*, and physics interactions are
//! not one body at a time: shoving a crate into a pile moves the whole pile, so
//! the claim has to follow the contact graph recursively or the pile is
//! simulated by two peers at its seam. Gaffer's recursive interaction rule is
//! the whole of the mechanism — what this module adds is the part that makes it
//! survive contact with the registrar's own limits.
//!
//! **Why a planner rather than a loop over contacts.** The naive
//! implementation — walk the contact graph, send a `Claim` for everything it
//! reaches — is unshippable against D7 §10, and the arithmetic is not close.
//! The contact-propagation cap is 64 entities per *tick*; at the 60 Hz sim tick
//! of D16 that is 3840 claims/s against a token bucket of **20/s sustained,
//! burst 64**. A peer that emits its permitted batch every tick is rate-limited
//! into `Deny{RateLimited}` within the first three ticks of a pile collapse and
//! then, because §10 feeds sustained bucket-camping into the witness/strike
//! telemetry of D10, looks exactly like a griefer while playing the game as
//! designed. So the per-tick cap is a ceiling, not a budget: this planner
//! carries the registrar's bucket locally and spends against it, deferring the
//! rest **in contact order** so the frontier nearest the interaction is claimed
//! first and the pile is acquired progressively from the point of contact
//! outwards, which is also what it looks like physically.
//!
//! Three more filters exist for the same reason — every claim they drop is a
//! claim that would have been refused anyway, at the cost of a token:
//!
//! - **Strong ownership blocks propagation** (INV-5). A body another peer
//!   strong-owns is not weakly claimable at any sequence, so the traversal
//!   stops there rather than spending a token to be told `Deny{StrongHeld}`.
//!   This is also where D7 §5's "the pile partitions along the contact
//!   frontier" comes from.
//! - **The plausibility gate is honoured client-side.** §10 accepts a weak
//!   claim only if the claimant's active interest covers the entity's cell.
//!   The peer knows its own coordinator grant, so it can decline to spend a
//!   token on a claim the gateway will refuse — and, more to the point, avoid
//!   generating the gate-failure telemetry that §10 routes to the strike
//!   pipeline.
//! - **`Deny` back-off** (§10: 250 ms doubling to a 2 s cap). A refused body is
//!   skipped while it is cooling rather than queued, because a still-touching
//!   body reappears in the next tick's contacts by itself; queueing it would
//!   grow an unbounded retry list to rediscover a fact physics already knows.
//!
//! Ephemeral bodies ([`crate::ephemeral`]) take part in the same traversal —
//! a projectile is exactly a body that enters its target's contact island — but
//! their claims resolve in-island and so are **not** charged to the registrar's
//! bucket. They are still subject to the per-tick cap, which is a wire-traffic
//! limit as much as a registrar one.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bevy_ecs::prelude::*;
use orrery_protocol::{
    CellId, ClaimBasis, ClaimKind, GridId, InterestGrantV1, PersistId, SeqPair, Tick,
};

use crate::ephemeral::{Ephemeral, EphemeralId, IslandClient};
use crate::{InterestGrant, LeaseClaim, LeaseClient};

/// D7 §5 / §12: contact-propagation batch cap, entities per tick.
pub const CONTACT_BATCH_CAP: usize = 64;
/// D7 §10: sustained per-peer claim rate at the gateway.
pub const CLAIM_RATE_PER_SEC: u64 = 20;
/// D7 §10: claim token-bucket burst, matching the contact-batch cap.
pub const CLAIM_BURST: u64 = 64;
/// D7 §10: first re-claim cooldown after a `Deny`.
pub const DENY_COOLDOWN_MS: u64 = 250;
/// D7 §10: cooldown ceiling after repeated denials.
pub const DENY_COOLDOWN_MAX_MS: u64 = 2_000;

/// A body in the contact graph, of either persistence class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContactNode {
    /// A registrar-arbitrated persistent entity.
    Persistent(PersistId),
    /// An in-island ephemeral entity (D7 §6).
    Ephemeral(EphemeralId),
}

/// What the local peer knows about one body's authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactStatus {
    /// This peer already writes it. A propagation *source*, never a target.
    Held,
    /// This peer has a claim in flight for it. Propagation continues through
    /// it optimistically — that is what "optimistic" means — but no second
    /// claim is sent.
    Pending,
    /// Weakly claimable: nothing fresher stands in the way.
    Claimable,
    /// Another peer holds strong ownership. INV-5 forbids taking it weakly, so
    /// the traversal stops here and the pile partitions at this body.
    StrongElsewhere,
}

/// One body observed in contact this tick.
#[derive(Debug, Clone, Copy)]
pub struct ContactBody {
    /// The body's identity.
    pub node: ContactNode,
    /// Grid containing `cell`. Ignored for ephemeral bodies.
    pub grid: GridId,
    /// The body's committed cell, checked against this peer's interest.
    /// Ignored for ephemeral bodies, which have no cell row to gate on.
    pub cell: CellId,
    /// The highest sequence pair this peer has seen for the body.
    pub observed: SeqPair,
    /// This peer's authority relationship to the body.
    pub status: ContactStatus,
}

/// The physics step's contact report for one tick.
///
/// Game code fills this from its solver and the planner consumes it; the
/// planner never reads physics types, which is what keeps it testable without
/// a solver and unchanged by the D13 physics posture.
///
/// [`propagate_contact_islands`] clears it after planning, so the report is
/// per-tick by construction and a solver system that stops running cannot leave
/// a stale contact graph generating claims. Fill it before that system runs;
/// anything written after it is read on the following tick.
#[derive(Debug, Default, Resource)]
pub struct ContactObservations {
    /// Every body named by a contact this tick.
    pub bodies: BTreeMap<ContactNode, ContactBody>,
    /// Contacts as unordered pairs, in solver order. Traversal order follows
    /// this vector, so "contact order" means the order physics reported.
    pub contacts: Vec<(ContactNode, ContactNode)>,
}

impl ContactObservations {
    /// Record a body and its authority status.
    pub fn observe(&mut self, body: ContactBody) {
        self.bodies.insert(body.node, body);
    }

    /// Record a contact between two observed bodies.
    pub fn touch(&mut self, a: ContactNode, b: ContactNode) {
        self.contacts.push((a, b));
    }

    /// Drop everything, ready for the next tick's solver output.
    pub fn clear(&mut self) {
        self.bodies.clear();
        self.contacts.clear();
    }
}

/// A persistent body the planner decided to claim this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactClaim {
    /// The entity to claim.
    pub persist: PersistId,
    /// Grid containing `cell`.
    pub grid: GridId,
    /// The entity's committed cell.
    pub cell: CellId,
    /// The sequence pair the claimant observed, carried into the `Claim`.
    pub observed: SeqPair,
}

/// One tick's propagation decision.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContactBurst {
    /// Persistent bodies to claim through the registrar, in contact order.
    pub registrar: Vec<ContactClaim>,
    /// Ephemeral bodies to claim in-island, in contact order.
    pub island: Vec<EphemeralId>,
    /// Candidates held back for a later tick, still in contact order.
    pub deferred: usize,
}

impl ContactBurst {
    /// Total entities named by this burst, against [`CONTACT_BATCH_CAP`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.registrar.len() + self.island.len()
    }

    /// Whether the burst claims nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The claim token bucket of D7 §10, carried on the client.
///
/// The gateway owns the authoritative copy; this one exists so a peer spends
/// its allowance on the claims nearest the interaction instead of discovering
/// the limit as a burst of `Deny{RateLimited}`. Accounting is in thousandths of
/// a token so a 60 Hz tick (16 ms, 0.32 tokens at 20/s) does not round to zero
/// and stall the bucket forever.
#[derive(Debug, Clone, Copy)]
struct ClaimBudget {
    milli_tokens: u64,
    last_ms: u64,
}

impl Default for ClaimBudget {
    fn default() -> Self {
        Self {
            milli_tokens: CLAIM_BURST * 1_000,
            last_ms: 0,
        }
    }
}

impl ClaimBudget {
    fn refill(&mut self, now_ms: u64) {
        let elapsed = now_ms.saturating_sub(self.last_ms);
        self.last_ms = now_ms;
        self.milli_tokens = self
            .milli_tokens
            .saturating_add(elapsed.saturating_mul(CLAIM_RATE_PER_SEC))
            .min(CLAIM_BURST * 1_000);
    }

    fn take(&mut self) -> bool {
        if self.milli_tokens < 1_000 {
            return false;
        }
        self.milli_tokens -= 1_000;
        true
    }

    fn available(&self) -> u64 {
        self.milli_tokens / 1_000
    }
}

/// Exponential back-off state for one denied body.
#[derive(Debug, Clone, Copy)]
struct Cooldown {
    until_ms: u64,
    next_ms: u64,
}

/// The contact-island propagation planner (D7 §5).
#[derive(Debug, Resource)]
pub struct ContactPropagator {
    /// Candidates that exceeded a previous tick's cap or budget, in contact
    /// order. Drained ahead of new candidates so the frontier does not starve.
    deferred: VecDeque<ContactNode>,
    queued: BTreeSet<ContactNode>,
    cooldowns: BTreeMap<ContactNode, Cooldown>,
    budget: ClaimBudget,
    /// Entities per tick, defaulting to [`CONTACT_BATCH_CAP`].
    pub batch_cap: usize,
}

impl Default for ContactPropagator {
    fn default() -> Self {
        Self {
            deferred: VecDeque::new(),
            queued: BTreeSet::new(),
            cooldowns: BTreeMap::new(),
            budget: ClaimBudget::default(),
            batch_cap: CONTACT_BATCH_CAP,
        }
    }
}

/// This peer's own interest coverage, decoded from its coordinator grant.
///
/// Only used to *decline* claims, never to authorize them: the gateway does not
/// take a peer's word for its interest (D7 §2), and neither would this cache be
/// believed if it said something wider. Reading a grant the coordinator signed
/// for this very peer needs no signature check for that purpose — a peer that
/// lies to itself here only refuses claims it could have made.
#[derive(Debug, Default, Resource)]
pub struct InterestCoverage {
    grid: Option<GridId>,
    cells: BTreeSet<CellId>,
    /// Whether a grant has ever been decoded into this cache.
    known: bool,
}

impl InterestCoverage {
    /// Rebuild the cache from the peer's current coordinator grant.
    ///
    /// An absent or undecodable grant leaves the cache *unknown*, which admits
    /// every claim: the gateway is the real gate, and a client that silently
    /// stopped claiming because it could not parse its own handout would be a
    /// far worse failure than one that lets the gateway refuse it.
    pub fn refresh(&mut self, grant: &InterestGrant) {
        let Some(bytes) = grant.grant.as_deref() else {
            self.known = false;
            self.cells.clear();
            self.grid = None;
            return;
        };
        let Ok(decoded) = InterestGrantV1::decode(bytes) else {
            self.known = false;
            self.cells.clear();
            self.grid = None;
            return;
        };
        self.grid = Some(decoded.claims.grid);
        self.cells = decoded.claims.covered_cells.into_iter().collect();
        self.known = true;
    }

    /// Whether a weak claim for `cell` can pass the gateway's §10 gate.
    #[must_use]
    pub fn allows(&self, grid: GridId, cell: CellId) -> bool {
        !self.known || (self.grid == Some(grid) && self.cells.contains(&cell))
    }
}

impl ContactPropagator {
    /// Record a registrar refusal so the body is not immediately re-claimed.
    ///
    /// The back-off doubles from [`DENY_COOLDOWN_MS`] to
    /// [`DENY_COOLDOWN_MAX_MS`], mirroring the gateway's own per-entity
    /// cooldown, so a peer that keeps bumping a body it cannot have stops
    /// paying for it.
    pub fn note_deny(&mut self, node: ContactNode, now_ms: u64) {
        let entry = self.cooldowns.entry(node).or_insert(Cooldown {
            until_ms: 0,
            next_ms: DENY_COOLDOWN_MS,
        });
        entry.until_ms = now_ms.saturating_add(entry.next_ms);
        entry.next_ms = (entry.next_ms * 2).min(DENY_COOLDOWN_MAX_MS);
    }

    /// Forget a body's back-off, after it was granted or left the neighbourhood.
    pub fn clear_deny(&mut self, node: ContactNode) {
        self.cooldowns.remove(&node);
    }

    /// Claims this peer may still send before the §10 bucket empties.
    #[must_use]
    pub fn budget_available(&self) -> u64 {
        self.budget.available()
    }

    /// Candidates carried over from earlier ticks.
    #[must_use]
    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    fn cooling(&self, node: ContactNode, now_ms: u64) -> bool {
        self.cooldowns
            .get(&node)
            .is_some_and(|cooldown| now_ms < cooldown.until_ms)
    }

    /// Plan one tick's claim burst from the solver's contact report.
    ///
    /// The traversal is a breadth-first walk from every body this peer already
    /// writes, following contacts in the order physics reported them, which is
    /// what makes "contact order" a definition rather than an accident: two
    /// peers pushing one pile from opposite ends each expand from their own
    /// frontier and meet in the middle, exactly D7 §5's per-entity partition.
    pub fn plan(
        &mut self,
        observations: &ContactObservations,
        coverage: &InterestCoverage,
        now_ms: u64,
    ) -> ContactBurst {
        self.budget.refill(now_ms);

        let mut adjacency: BTreeMap<ContactNode, Vec<ContactNode>> = BTreeMap::new();
        for (a, b) in &observations.contacts {
            adjacency.entry(*a).or_default().push(*b);
            adjacency.entry(*b).or_default().push(*a);
        }

        // Deferred candidates lead, so a body held back last tick is not
        // starved by a fresh frontier arriving in front of it.
        let mut candidates: Vec<ContactNode> = Vec::new();
        let mut proposed: BTreeSet<ContactNode> = BTreeSet::new();
        for node in std::mem::take(&mut self.deferred) {
            self.queued.remove(&node);
            if observations.bodies.contains_key(&node) && proposed.insert(node) {
                candidates.push(node);
            }
        }

        let mut visited: BTreeSet<ContactNode> = BTreeSet::new();
        let mut frontier: VecDeque<ContactNode> = observations
            .bodies
            .values()
            .filter(|body| body.status == ContactStatus::Held)
            .map(|body| body.node)
            .collect();
        for seed in &frontier {
            visited.insert(*seed);
        }

        while let Some(node) = frontier.pop_front() {
            for touched in adjacency.get(&node).into_iter().flatten() {
                if !visited.insert(*touched) {
                    continue;
                }
                let Some(body) = observations.bodies.get(touched) else {
                    continue;
                };
                match body.status {
                    // INV-5: not weakly claimable, and the recursion stops.
                    ContactStatus::StrongElsewhere => continue,
                    // Already ours or already asked for: propagate through it,
                    // but do not claim it again.
                    ContactStatus::Held | ContactStatus::Pending => {
                        frontier.push_back(*touched);
                    }
                    ContactStatus::Claimable => {
                        frontier.push_back(*touched);
                        if proposed.insert(*touched) {
                            candidates.push(*touched);
                        }
                    }
                }
            }
        }

        let mut burst = ContactBurst::default();
        for node in candidates {
            let body = observations.bodies[&node];
            if self.cooling(node, now_ms) {
                // Deliberately dropped rather than deferred: a body still in
                // contact is re-proposed by the next tick's solver output, so
                // the retry queue would only duplicate what physics already
                // reports.
                continue;
            }
            if burst.len() >= self.batch_cap {
                self.defer(node);
                continue;
            }
            match node {
                ContactNode::Persistent(persist) => {
                    if !coverage.allows(body.grid, body.cell) {
                        continue;
                    }
                    if !self.budget.take() {
                        self.defer(node);
                        continue;
                    }
                    burst.registrar.push(ContactClaim {
                        persist,
                        grid: body.grid,
                        cell: body.cell,
                        observed: body.observed,
                    });
                }
                // Never charged to the registrar bucket: an in-island claim
                // reaches no gateway (D7 §6).
                ContactNode::Ephemeral(id) => burst.island.push(id),
            }
        }
        burst.deferred = self.deferred.len();
        burst
    }

    fn defer(&mut self, node: ContactNode) {
        if self.queued.insert(node) {
            self.deferred.push_back(node);
        }
    }
}

/// Turn the planner's burst into outbound claims.
///
/// Persistent bodies go through [`LeaseClient`] exactly as an explicit claim
/// does, so they inherit correlation, optimistic marking and rollback with no
/// second code path. Ephemeral bodies go to [`IslandOutbox`] and touch nothing
/// the gateway adapter reads.
#[allow(clippy::too_many_arguments)]
pub fn propagate_contact_islands(
    mut client: LeaseClient,
    mut propagator: ResMut<ContactPropagator>,
    mut coverage: ResMut<InterestCoverage>,
    grant: Res<InterestGrant>,
    mut observations: ResMut<ContactObservations>,
    mut island: IslandClient,
    entities: Query<(Entity, &crate::PersistIdentity)>,
    ephemerals: Query<(Entity, &Ephemeral)>,
    tick: Res<ContactTick>,
) {
    if grant.is_changed() {
        coverage.refresh(&grant);
    }
    let now_ms = tick.now_ms;
    let burst = propagator.plan(&observations, &coverage, now_ms);
    for claim in burst.registrar {
        let Some((entity_ref, _)) = entities.iter().find(|(_, id)| id.0 == claim.persist) else {
            continue;
        };
        let _ = client.claim(LeaseClaim {
            entity: entity_ref,
            persist: claim.persist,
            grid: claim.grid,
            cell: claim.cell,
            kind: ClaimKind::Weak,
            basis: ClaimBasis::Contact { tick: tick.tick },
            observed: claim.observed,
            tick: tick.tick,
        });
    }
    for id in burst.island {
        let Some((entity_ref, _)) = ephemerals.iter().find(|(_, view)| view.0 == id) else {
            continue;
        };
        let _ = island.claim(entity_ref, id, tick.tick);
    }
    observations.clear();
}

/// The universe tick and local clock the propagation systems run against.
///
/// Separate from [`crate::AuthorityState`]'s clock because the claim it stamps
/// is *evidence*: `ClaimBasis::Contact{tick}` names the simulation tick the
/// contact happened on, which the registrar's plausibility gate and the D9
/// input log both read, while the back-off and token bucket run on wall time.
#[derive(Debug, Resource)]
pub struct ContactTick {
    /// Universe tick of the physics step that produced the contacts.
    pub tick: Tick,
    /// Client-process monotonic milliseconds, for back-off and rate limiting.
    pub now_ms: u64,
}

impl Default for ContactTick {
    fn default() -> Self {
        Self {
            tick: Tick::new(0),
            now_ms: 0,
        }
    }
}

/// Feed registrar verdicts back into the planner's back-off state.
///
/// Without this the client-side half of D7 §10 is decorative: the planner would
/// re-propose a body the registrar just refused on the very next contact
/// report, which is the behaviour §10 calls camping the rate limit and routes
/// at the strike pipeline as telemetry. A grant clears the back-off, so a body
/// that was contested and then won does not carry a penalty into the next
/// contest.
pub fn absorb_contact_denials(
    mut propagator: ResMut<ContactPropagator>,
    tick: Res<ContactTick>,
    mut events: MessageReader<crate::AuthorityEvent>,
    identities: Query<&crate::PersistIdentity>,
) {
    for event in events.read() {
        let (entity, denied) = match event {
            crate::AuthorityEvent::Denied { entity, .. } => (*entity, true),
            crate::AuthorityEvent::Granted { entity, .. }
            | crate::AuthorityEvent::Inherited { entity, .. } => (*entity, false),
            _ => continue,
        };
        let Ok(identity) = identities.get(entity) else {
            continue;
        };
        let node = ContactNode::Persistent(identity.0);
        if denied {
            propagator.note_deny(node, tick.now_ms);
        } else {
            propagator.clear_deny(node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::IslandId;

    fn node_id(seed: u8) -> orrery_protocol::NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        iroh_base::SecretKey::from_bytes(&bytes).public()
    }

    fn persistent(id: u64, status: ContactStatus) -> ContactBody {
        ContactBody {
            node: ContactNode::Persistent(PersistId::new(id)),
            grid: GridId::new(0),
            cell: CellId::ROOT,
            observed: SeqPair::default(),
            status,
        }
    }

    fn ephemeral(seq: u32, status: ContactStatus) -> ContactBody {
        ContactBody {
            node: ContactNode::Ephemeral(EphemeralId {
                island: IslandId::new(1),
                spawner: node_id(9),
                seq,
            }),
            grid: GridId::new(0),
            cell: CellId::ROOT,
            observed: SeqPair::default(),
            status,
        }
    }

    /// A chain `0 - 1 - 2 - ... - n`, with body 0 held locally.
    fn chain(n: u64) -> ContactObservations {
        let mut observations = ContactObservations::default();
        observations.observe(persistent(0, ContactStatus::Held));
        for id in 1..=n {
            observations.observe(persistent(id, ContactStatus::Claimable));
            observations.touch(
                ContactNode::Persistent(PersistId::new(id - 1)),
                ContactNode::Persistent(PersistId::new(id)),
            );
        }
        observations
    }

    #[test]
    fn authority_propagates_through_the_whole_pile_not_only_the_touched_body() {
        // The failure this catches: claiming only the directly contacted body,
        // so pushing a pile leaves everything behind the first crate simulated
        // by whoever held it — the seam D7 §5's recursion exists to remove.
        let mut propagator = ContactPropagator::default();
        let burst = propagator.plan(&chain(5), &InterestCoverage::default(), 0);

        assert_eq!(
            burst
                .registrar
                .iter()
                .map(|claim| claim.persist)
                .collect::<Vec<_>>(),
            (1..=5).map(PersistId::new).collect::<Vec<_>>()
        );
        assert_eq!(burst.deferred, 0);
    }

    #[test]
    fn propagation_stops_at_a_body_another_peer_strong_owns() {
        // The failure this catches: propagating through strong ownership, which
        // spends the claim budget on refusals (INV-5) and would, if the
        // registrar ever agreed, steal a grabbed object by shoving a crate at
        // its holder.
        let mut observations = chain(4);
        observations.observe(persistent(2, ContactStatus::StrongElsewhere));

        let mut propagator = ContactPropagator::default();
        let burst = propagator.plan(&observations, &InterestCoverage::default(), 0);

        assert_eq!(
            burst
                .registrar
                .iter()
                .map(|claim| claim.persist)
                .collect::<Vec<_>>(),
            vec![PersistId::new(1)],
            "the pile must partition at the strong-owned body"
        );
    }

    #[test]
    fn a_burst_over_the_batch_cap_defers_the_excess_in_contact_order() {
        // The failure this catches: dropping or reordering the overflow of a
        // large pile, so the far side of it is silently never claimed.
        let mut propagator = ContactPropagator::default();
        let observations = chain(80);

        let first = propagator.plan(&observations, &InterestCoverage::default(), 0);
        assert_eq!(first.len(), CONTACT_BATCH_CAP);
        assert_eq!(first.deferred, 80 - CONTACT_BATCH_CAP);
        assert_eq!(first.registrar[0].persist, PersistId::new(1));

        let second = propagator.plan(&observations, &InterestCoverage::default(), 1_000);
        assert_eq!(
            second
                .registrar
                .iter()
                .map(|claim| claim.persist)
                .take(3)
                .collect::<Vec<_>>(),
            vec![PersistId::new(65), PersistId::new(66), PersistId::new(67)],
            "the carried-over frontier must lead the next burst, in contact order"
        );
    }

    #[test]
    fn sustained_contact_churn_stays_inside_the_gateways_claim_rate_limit() {
        // The failure this catches: treating the 64-per-tick cap as a budget.
        // At 60 Hz that is 3840 claims/s against a 20/s bucket, so an honest
        // pile collapse would be rate-limited and then reported to the strike
        // telemetry of D7 §10 as claim spam.
        let mut propagator = ContactPropagator::default();
        let observations = chain(400);
        let mut sent = 0usize;
        // Ten seconds of 60 Hz ticks with the pile permanently in contact.
        for tick in 0..600u64 {
            let burst = propagator.plan(&observations, &InterestCoverage::default(), tick * 16);
            sent += burst.registrar.len();
        }

        let permitted = CLAIM_BURST as usize + (CLAIM_RATE_PER_SEC as usize * 10);
        assert!(
            sent <= permitted,
            "sent {sent} claims in 10 s against a {permitted}-claim allowance"
        );
        assert!(sent > 0, "the planner must still make progress");
    }

    #[test]
    fn a_denied_body_is_not_re_claimed_until_its_back_off_elapses() {
        // The failure this catches: re-proposing a refused body every tick,
        // which is the exact shape D7 §10 calls camping the rate limit.
        let observations = chain(1);
        let mut propagator = ContactPropagator::default();
        let denied = ContactNode::Persistent(PersistId::new(1));

        assert_eq!(
            propagator
                .plan(&observations, &InterestCoverage::default(), 0)
                .len(),
            1
        );
        propagator.note_deny(denied, 0);

        assert!(propagator
            .plan(&observations, &InterestCoverage::default(), 100)
            .is_empty());
        assert!(propagator
            .plan(&observations, &InterestCoverage::default(), 249)
            .is_empty());
        assert_eq!(
            propagator
                .plan(&observations, &InterestCoverage::default(), 250)
                .len(),
            1
        );
    }

    #[test]
    fn repeated_denials_back_off_exponentially_to_the_documented_ceiling() {
        // The failure this catches: a flat cooldown, which lets a body that
        // will never be granted cost a claim four times a second forever.
        let mut propagator = ContactPropagator::default();
        let node = ContactNode::Persistent(PersistId::new(1));
        let mut now = 0u64;
        let mut waits = Vec::new();
        for _ in 0..6 {
            propagator.note_deny(node, now);
            let until = propagator.cooldowns[&node].until_ms;
            waits.push(until - now);
            now = until;
        }
        assert_eq!(waits, vec![250, 500, 1_000, 2_000, 2_000, 2_000]);
    }

    #[test]
    fn a_projectile_claim_never_charges_the_registrars_claim_budget() {
        // The failure this catches: routing ephemeral contact claims through
        // the registrar's token bucket, which makes a firefight's projectiles
        // starve the persistent bodies they hit.
        let mut observations = ContactObservations::default();
        observations.observe(persistent(0, ContactStatus::Held));
        for seq in 0..40u32 {
            observations.observe(ephemeral(seq, ContactStatus::Claimable));
            observations.touch(
                ContactNode::Persistent(PersistId::new(0)),
                ContactNode::Ephemeral(EphemeralId {
                    island: IslandId::new(1),
                    spawner: node_id(9),
                    seq,
                }),
            );
        }

        let mut propagator = ContactPropagator::default();
        let before = propagator.budget_available();
        let burst = propagator.plan(&observations, &InterestCoverage::default(), 0);

        assert_eq!(burst.island.len(), 40);
        assert!(burst.registrar.is_empty());
        assert_eq!(
            propagator.budget_available(),
            before,
            "in-island claims reach no gateway and must cost no gateway token"
        );
    }

    #[test]
    fn a_projectile_hitting_a_crate_claims_the_crate_through_the_registrar() {
        // The failure this catches: treating the two classes as separate
        // graphs, so a projectile's impact never propagates weak authority to
        // the persistent body it hit (D7 §6.1 step 3).
        let mut observations = ContactObservations::default();
        observations.observe(ephemeral(0, ContactStatus::Held));
        observations.observe(persistent(77, ContactStatus::Claimable));
        observations.touch(
            ContactNode::Ephemeral(EphemeralId {
                island: IslandId::new(1),
                spawner: node_id(9),
                seq: 0,
            }),
            ContactNode::Persistent(PersistId::new(77)),
        );

        let mut propagator = ContactPropagator::default();
        let burst = propagator.plan(&observations, &InterestCoverage::default(), 0);

        assert_eq!(
            burst
                .registrar
                .iter()
                .map(|claim| claim.persist)
                .collect::<Vec<_>>(),
            vec![PersistId::new(77)]
        );
        assert!(burst.island.is_empty());
    }

    #[test]
    fn a_claim_outside_this_peers_interest_is_dropped_before_it_costs_a_token() {
        // The failure this catches: spending claim budget on bodies the
        // gateway's §10 plausibility gate will refuse, and generating the
        // gate-failure telemetry D7 §10 routes at the strike pipeline while
        // doing it.
        let mut observations = chain(1);
        let mut outside = persistent(1, ContactStatus::Claimable);
        outside.cell = CellId::ROOT.children()[1];
        observations.observe(outside);

        let mut coverage = InterestCoverage {
            grid: Some(GridId::new(0)),
            cells: [CellId::ROOT].into_iter().collect(),
            known: true,
        };

        let mut propagator = ContactPropagator::default();
        let before = propagator.budget_available();
        let burst = propagator.plan(&observations, &coverage, 0);

        assert!(burst.is_empty());
        assert_eq!(propagator.budget_available(), before);

        // And with no grant on file the cache must not gate anything: the
        // gateway is the real arbiter, and a peer that cannot read its own
        // handout must not silently stop playing.
        coverage.refresh(&InterestGrant::default());
        assert!(!propagator.plan(&observations, &coverage, 5_000).is_empty());
    }
}
