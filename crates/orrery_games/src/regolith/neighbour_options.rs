//! **SPIKE #793 — propose-only. Do not merge.**
//!
//! Four ways to reach a neighbour under a `bevy_ecs`-native ruleset, built
//! side by side so the recorded-neighbour discipline of D43 (e)(5) can be
//! scored against each rather than argued about.
//!
//! The question this module exists to answer is narrow and worth stating
//! before any code:
//!
//! > [`StateView::neighbor`](orrery_core::StateView::neighbor) takes
//! > `&mut self` because **reading has a side effect on the log**. Which, if
//! > any, of the query-shaped alternatives keeps that side effect a fact about
//! > the type rather than a fact about the author's discipline?
//!
//! # The finding this module was built to test, stated up front
//!
//! **A recorded read is a *lookup*, not a *dereference*.** `neighbor(id)`
//! records before it returns, so it records the absent case too — the log
//! carries a `NeighborFrame { present: false, .. }` and replay installs
//! nothing for it. Every query-shaped option separates *finding* the
//! neighbour from *reading its payload*, and the split is where the guarantee
//! escapes: `query.get(e).is_ok()` answers "does this entity exist, and is it
//! fresh" before any token is exchanged, and nothing in the type system
//! obliges the caller to say so.
//!
//! That is not a subtlety. The absent read is precisely the one a dishonest
//! rule wants unrecorded — "I never saw the occluder" is the cover claim that
//! pays.
//!
//! # What violating the gate buys, again
//!
//! Compiling this file needs `bevy_ecs` in `orrery_games`, which
//! `scripts/core-gates.sh` clause 1 refuses by name and which D42 (a) and
//! D43 (e)(1) reserve to the owner. This spike **does not amend them and does
//! not edit the gate**, exactly as `super::native` does not. Option E cannot
//! be evaluated at all without a real `bevy_ecs::Schedule` to initialize, and
//! an evaluation that stubbed it would have measured nothing.

use bevy_ecs::prelude::{Component, Entity, IntoSystem, Query, Resource, World};
use bevy_ecs::query::FilteredAccessSet;
use bevy_ecs::system::System;
use orrery_protocol::{PersistId, Tick};

// ── the recording obligation, extracted ─────────────────────────────────────

/// The read log every option must feed, factored out of
/// [`orrery_core::StateView`] so the four options differ *only* in how they
/// reach it.
///
/// `staleness_cap` and the future-stamp refusal live here for the same reason
/// they live in `StateView::neighbor` today: a read that is too old, or
/// stamped ahead of its reader, must resolve to `None` **and still be
/// recorded**, or live execution and `ReplayHarness` would disagree about the
/// same log (`crates/orrery_core/src/replay.rs:256-262`).
#[derive(Resource, Debug)]
pub struct ReadLog {
    reads: Vec<PersistId>,
    reader: PersistId,
    tick: Tick,
    staleness_cap: u64,
}

impl ReadLog {
    /// A log for `reader` stepping at `tick`.
    #[must_use]
    pub fn new(reader: PersistId, tick: Tick, staleness_cap: u64) -> Self {
        Self {
            reads: Vec::new(),
            reader,
            tick,
            staleness_cap,
        }
    }

    /// The reads performed, in first-read order — what the executor turns into
    /// `NeighborFrame` records and what `ReplayHarness` compares for exact
    /// sequence equality (`replay.rs:325-327`).
    #[must_use]
    pub fn reads(&self) -> &[PersistId] {
        &self.reads
    }

    /// Record that `id` was asked about. Idempotent within a tick, matching
    /// `StateView::neighbor`'s `if !self.reads.contains(&id)`.
    fn record(&mut self, id: PersistId) {
        if !self.reads.contains(&id) {
            self.reads.push(id);
        }
    }

    /// The freshness predicate, identical in shape to the one in
    /// `StateView::neighbor`: checked subtraction, so an observation stamped
    /// ahead of the reader is refused rather than saturating to zero.
    fn fresh(&self, id: PersistId, observed: Tick) -> bool {
        id != self.reader
            && self
                .tick
                .0
                .checked_sub(observed.0)
                .is_some_and(|age| age <= self.staleness_cap)
    }
}

// ── OPTION B — token exchange ───────────────────────────────────────────────

/// **Option B.** A neighbour whose payload is private to this module.
///
/// The only way to a `&S` is [`ReadLog::read`], so Rust privacy makes
/// recording a type obligation for anyone outside this file. A `Query` may
/// match it, filter on it and iterate it; it cannot deref it.
///
/// The identity is carried *inside* the component rather than passed
/// alongside, a deliberate deviation from the sketch in #793's brief: a
/// `read(&mut self, id, &Neighbour<S>)` lets the caller record one id and read
/// another, which is a hole with no upside.
#[derive(Component, Debug, Clone)]
pub struct Neighbour<S> {
    id: PersistId,
    state: S,
    observed: Tick,
}

impl<S> Neighbour<S> {
    /// Install a replicated observation. Constructing one is *not* a read.
    pub fn new(id: PersistId, state: S, observed: Tick) -> Self {
        Self {
            id,
            state,
            observed,
        }
    }

    /// The identity, which is **not** privileged: see
    /// [`enumeration`](self#enumeration-leakage) — knowing who is there is the
    /// leak that Option B does not close.
    #[must_use]
    pub fn id(&self) -> PersistId {
        self.id
    }
}

impl ReadLog {
    /// **Option B's whole mechanism.** Exchange the log for the payload.
    ///
    /// Records unconditionally — before the freshness test, so a stale
    /// neighbour reads as `None` and is still in the log, exactly as
    /// `StateView::neighbor` does it.
    pub fn read<'a, S>(&mut self, n: &'a Neighbour<S>) -> Option<&'a S> {
        self.record(n.id);
        self.fresh(n.id, n.observed).then_some(&n.state)
    }

    /// **Option B's hole, made explicit so a test can name it.**
    ///
    /// When the entity is absent the query does not match, there is no
    /// `Neighbour` to hand to [`Self::read`], and so there is nothing the
    /// privacy rule can attach to. Recording the absent case is available but
    /// **optional** — a plain method the author may simply not call. Under
    /// Option A the same case is unmissable, because the lookup *is* the
    /// recording call.
    pub fn read_absent(&mut self, id: PersistId) {
        self.record(id);
    }
}

// ── OPTION C — forbid iteration ─────────────────────────────────────────────

/// **Option C.** A neighbour query that can be asked about a named entity and
/// cannot be enumerated.
///
/// There is no `iter`, no `len` and no `is_empty`; the wrapped `Query` is
/// private. Enumeration is closed completely.
///
/// The recording is inside [`Self::get`], which is the only reason C bounds
/// anything B does not: because the lookup and the log are the same call, the
/// absent case records itself. Note what that makes C — a `get` that logs, by
/// named id, capped — which is `StateView::neighbor` with a `Query` behind it
/// and a `Entity`/`PersistId` translation step in front.
pub struct NamedNeighbours<'w, 's, S: Send + Sync + 'static> {
    query: Query<'w, 's, &'static Neighbour<S>>,
    index: &'s dyn Fn(PersistId) -> Option<Entity>,
}

impl<S: Send + Sync + 'static> NamedNeighbours<'_, '_, S> {
    /// Look up a neighbour **by canonical id**, recording the lookup whether or
    /// not it resolves.
    ///
    /// `&mut ReadLog` is taken rather than held, so the recording is visible in
    /// the call and a caller cannot obtain a `&S` without one in hand.
    pub fn get(&self, log: &mut ReadLog, id: PersistId) -> Option<&S> {
        log.record(id);
        let n = (self.index)(id).and_then(|e| self.query.get(e).ok())?;
        log.fresh(n.id, n.observed).then_some(&n.state)
    }
}

// ── OPTION D — logged enumeration ───────────────────────────────────────────

/// **Option D.** Iteration is allowed, and the id-set it yielded is recorded
/// alongside the value reads.
///
/// This is the only option that closes the enumeration leak, and it closes it
/// by *paying* for it: every yielded id becomes part of the record. See
/// [`Yielded`] for what that costs, which is the argument against it.
pub struct LoggedNeighbours<'w, 's, S: Send + Sync + 'static> {
    query: Query<'w, 's, &'static Neighbour<S>>,
}

/// What Option D adds to a signed window.
///
/// Today a window says *these values were consulted*. Under D it says *these
/// entities were visible, and these values were consulted* — a strictly larger
/// claim, and one the authority must be able to substantiate, because a
/// witness that cannot reproduce the visible set now has a divergence where
/// before it had nothing to check.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Yielded {
    /// Every id the query matched, in canonical order.
    ///
    /// **Canonical, not iteration order.** `bevy_ecs` yields in archetype and
    /// spawn order, which is stable for one insertion history and not across
    /// permuted ones — the property `tier_h_projection_differential.rs`
    /// tests. An id-set that entered the record in iteration order would make
    /// the record itself insertion-order-dependent. This is the obligation
    /// Option D creates for #787, and only Option D creates it.
    pub ids: Vec<PersistId>,
}

impl<S: Send + Sync + 'static> LoggedNeighbours<'_, '_, S> {
    /// Iterate, recording the yielded id-set.
    ///
    /// Returns owned ids plus payload references. The sort is load-bearing,
    /// not decorative — see [`Yielded::ids`].
    pub fn iter_logged(&self, yielded: &mut Yielded) -> Vec<(PersistId, &S)> {
        let mut rows: Vec<_> = self.query.iter().map(|n| (n.id, &n.state)).collect();
        rows.sort_by_key(|(id, _)| *id);
        yielded.ids = rows.iter().map(|(id, _)| *id).collect();
        rows
    }
}

// ── OPTION E — registration-time refusal (not in the brief; buildable) ──────

/// **Option E.** Refuse, when the schedule is built, any system whose access
/// set mentions a neighbour component without also taking the log.
///
/// This is the same shape as the ambiguity canary the host already runs
/// (`crates/orrery_sim_host/src/ecs.rs:528`, `audit_ambiguity`): initialize the
/// schedule against a world and inspect what comes back. `bevy_ecs`'s
/// `System::initialize` returns a `FilteredAccessSet`
/// (`bevy_ecs-0.19.1/src/system/system.rs:162`), so "this system can see a
/// neighbour" is a decidable property of a built schedule.
///
/// It is a **build-time** guarantee, not a compile-time one — but it is total
/// over the registered schedule, which is more than B, C or D achieve, and it
/// is the only option that survives contact with the fact that under a native
/// ruleset a system's capabilities are whatever the *game author* wrote in the
/// signature.
///
/// One system offered to the audit: its manifest name, and a thunk that
/// initializes it against a world and yields the access set that decides its
/// fate.
pub type AuditedSystem = (
    &'static str,
    Box<dyn FnOnce(&mut World) -> FilteredAccessSet>,
);

/// Returns `Ok(())` when every audited system that can reach a neighbour also
/// holds the log; otherwise the name of the first offender.
pub fn audit_neighbour_access<S: Send + Sync + 'static>(
    world: &mut World,
    systems: Vec<AuditedSystem>,
) -> Result<(), String> {
    let neighbour = world.register_component::<Neighbour<S>>();
    let log = world.register_component::<ReadLog>();
    for (name, init) in systems {
        let access = init(world);
        let combined = access.combined_access();
        let sees_neighbour = combined.has_read(neighbour)
            || combined.has_write(neighbour)
            || combined.has_read_all();
        let holds_log = combined.has_write(log);
        if sees_neighbour && !holds_log {
            return Err(format!(
                "{name} can reach a neighbour without holding the read log"
            ));
        }
    }
    Ok(())
}

/// Initialize one system and hand back its access set, which is the whole of
/// what [`audit_neighbour_access`] needs from it.
///
/// `System::initialize` returns the `FilteredAccessSet`
/// (`bevy_ecs-0.19.1/src/system/system.rs:162`), so this is public API rather
/// than an inspection of `bevy_ecs` internals. The initialized system is
/// dropped: the audit is a question about the signature, not about a run.
pub fn access_of<M, T: IntoSystem<(), (), M> + 'static>(
    system: T,
) -> Box<dyn FnOnce(&mut World) -> FilteredAccessSet> {
    Box::new(move |world: &mut World| {
        let mut system = IntoSystem::into_system(system);
        system.initialize(world)
    })
}

// ── the real call site, under each option ───────────────────────────────────
//
// `visibility::verify_claims` is the workspace's one production neighbour
// read. Everything below its read stage — `verify_visibility`,
// `verify_collision`, the integer predicates — is untouched by every option,
// so only the read stage is reproduced here. The shipped stage is
// `crates/orrery_games/src/regolith/visibility.rs:169-172`:
//
// ```ignore
// let [locker, rock, collision] = NEIGHBOR_READ_SLOTS.map(|slot| {
//     slot.target(cover, collision_id)
//         .and_then(|id| view.neighbor(id).cloned())
// });
// ```
//
// Three slots, each `Option<PersistId>`, each resolved through the one call
// that logs. Note what the call site already is: **a lookup by named id, taken
// from the inputs**. `cover` comes from an `Order::ClaimCover`, `collision_id`
// from an `Order::Collide`. It never asks who is nearby, and there is no
// version of it that wants to.

/// The three ids one step may ask about, as the shipped slot table produces
/// them: two from a cover claim, one from a collision claim, each absent when
/// the corresponding order is absent.
pub type Targets = [Option<PersistId>; 3];

/// **Option B at the call site.**
///
/// The query must be resolved to components first, and *that resolution is the
/// unrecorded step*. `found` is `Option<&Neighbour<S>>`: obtaining it already
/// answered "does this entity exist, and did it replicate to me", and the log
/// has not been touched. Only the `Some` arm reaches [`ReadLog::read`].
///
/// Writing `read_absent` in the `None` arm is the correct thing to do and
/// nothing makes the author do it — omit it and the code still compiles, still
/// runs, and produces a window that replay rejects only if the *rest* of the
/// step happened to depend on the absent neighbour. That is the silent failure
/// mode #796 named.
pub fn read_stage_b<'a, S>(
    log: &mut ReadLog,
    targets: Targets,
    lookup: impl Fn(PersistId) -> Option<&'a Neighbour<S>>,
) -> [Option<&'a S>; 3]
where
    S: 'a,
{
    targets.map(|target| {
        let id = target?;
        match lookup(id) {
            Some(n) => log.read(n),
            None => {
                // The line a contributor may simply not write.
                log.read_absent(id);
                None
            }
        }
    })
}

/// **Option C at the call site.**
///
/// One call, recording inside it, absent case included — because `get` *is*
/// the lookup. Structurally identical to the shipped line, with a `PersistId`
/// → `Entity` translation hidden inside `NamedNeighbours`.
pub fn read_stage_c<'a, S: Send + Sync + 'static>(
    log: &mut ReadLog,
    targets: Targets,
    neighbours: &'a NamedNeighbours<'_, '_, S>,
) -> [Option<&'a S>; 3] {
    targets.map(|target| target.and_then(|id| neighbours.get(log, id)))
}

/// **Option D at the call site.**
///
/// D does not improve this call site at all — the three targets are still
/// named ids from the inputs, so D's `iter_logged` is never what it wants.
/// What D changes is the *record*: to permit iteration anywhere, the window
/// must carry the yielded set everywhere, and `max_neighbor_reads` must rise
/// from three to a population bound.
///
/// Shown here reading by name through the same log, so the comparison is
/// like-for-like: D is C plus an obligation, at this call site.
pub fn read_stage_d<'a, S: Send + Sync + 'static>(
    log: &mut ReadLog,
    targets: Targets,
    neighbours: &'a NamedNeighbours<'_, '_, S>,
) -> [Option<&'a S>; 3] {
    read_stage_c(log, targets, neighbours)
}
