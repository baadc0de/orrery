//! Spike (#793): a curated `bevy_ecs` facade, and an `OrderedQuery` that logs
//! both halves of an access.
//!
//! **Propose-only.** Nothing in the shipped tree depends on this crate. It
//! exists to answer five questions with a compiler rather than an argument;
//! the answers are written up in `docs/spikes/orrery-ecs-facade.md`.
//!
//! # The mechanism
//!
//! A game crate declares this crate under the dependency key `bevy_ecs`:
//!
//! ```toml
//! bevy_ecs = { package = "orrery_ecs_facade", path = "…" }
//! ```
//!
//! Two properties follow, and they are the whole design.
//!
//! 1. **The boundary.** In Rust 2018+ a crate can only name a dependency it
//!    declares. Upstream `bevy_ecs` is a *transitive* dependency of the game
//!    crate and is therefore not in its extern prelude, so `bevy_ecs::…` in
//!    game code resolves here — to the list below — and there is no path at
//!    all to a `bevy_ecs` item this module does not re-export. That is a
//!    visibility boundary, not a lint.
//! 2. **The derives follow.** `bevy_ecs_macros` does not hardcode `::bevy_ecs`.
//!    It asks `bevy_macro_utils::BevyManifest::maybe_get_path("bevy_ecs")`,
//!    which reads the *calling crate's* `Cargo.toml` and keys on the
//!    dependency table's **key**, not the package name. The key above is
//!    `bevy_ecs`, so the derive emits `bevy_ecs::component::Component` and
//!    that resolves here too.
//!
//! # The allowlist discipline
//!
//! Every module below is hand-written. There is no `pub use bevy_ecs::*` and
//! no `pub use bevy_ecs::system::*`, because a glob re-export makes the
//! curated surface "whatever upstream exports", which is not a list anyone can
//! review. The same discipline as D43 (e)'s host allowlist: name what is
//! permitted, and let the absence of a name be the refusal.

#![allow(clippy::module_name_repetitions)]

use alloc::collections::BTreeMap;

extern crate alloc;

use orrery_protocol::{PersistId, Tick};

// ── The curated surface ─────────────────────────────────────────────────────
//
// The items below are what a game crate needs at the root: the derives it
// writes, plus `Entity`. Note what is *not* here
// and cannot be reached from here: `Query`, `World`, `Commands`, `EntityRef`,
// `EntityMut`, `UnsafeWorldCell`, `QueryState`, `DeferredWorld`.

pub use bevy_ecs::component::Component;
pub use bevy_ecs::entity::Entity;
pub use bevy_ecs::resource::Resource;
/// Re-exported so that the refusal below is a *refusal* and not an omission.
///
/// A game crate that writes `#[derive(SystemParam)]` gets an expansion naming
/// `bevy_ecs::world::World`, `bevy_ecs::world::DeferredWorld` and
/// `bevy_ecs::world::unsafe_world_cell` (`bevy_ecs_macros-0.19.1/src/lib.rs`,
/// `derive_system_param_impl`). This facade exposes no `world` module, so the
/// derive does not compile here — see `facade_game`'s
/// `system_param_is_refused` doctest.
///
/// That is the finding, not an oversight: `World` cannot be curated. Its
/// inherent `get`, `entity`, `entity_mut` and `query` reach any component of
/// any entity, and a facade that renamed or wrapped it would no longer satisfy
/// the trait signature the derive generates. So the facade may hand a game
/// crate components, or `#[derive(SystemParam)]`, but not both — and
/// `OrderedQuery` is written *here*, where `World` is already reachable, so
/// that game crates never need the second.
pub use bevy_ecs::system::SystemParam;

/// Everything `#[derive(Component)]` emits a path to.
///
/// Established by compiling the derive against a facade that started empty and
/// adding exactly what the compiler asked for — not by reading the macro and
/// guessing. `DefaultCloneBehaviorBase` and `DefaultCloneBehaviorViaClone` are
/// brought in by an autoderef-specialization block the expansion opens with a
/// `use`, which is why they are here and would be missed by any audit that
/// only looked at the emitted type paths.
pub mod component {
    pub use bevy_ecs::component::{
        Component, ComponentCloneBehavior, ComponentId, DefaultCloneBehaviorBase,
        DefaultCloneBehaviorSpecialization, DefaultCloneBehaviorViaClone, Immutable, Mutable,
        RequiredComponentsRegistrator, StorageType,
    };
}

/// Entity identity. `Entity` is an index, not a capability: holding one gets
/// you nothing without something to look it up in, and the only such thing a
/// game crate can name is [`OrderedQuery`].
pub mod entity {
    pub use bevy_ecs::entity::{Entity, EntityMapper, MapEntities};
}

// `lifecycle` is deliberately NOT re-exported, and the reason generalises past
// this one module.
//
// `bevy_ecs::lifecycle::ComponentHook` is a type *alias*:
//
//     pub type ComponentHook = for<'w> fn(DeferredWorld<'w>, HookContext);
//
// Re-export the alias and a game crate can write
//
//     const HOOK: bevy_ecs::lifecycle::ComponentHook =
//         |mut world, ctx| { let _ = world.get::<Rock>(ctx.entity); };
//
// which compiles. The closure's first parameter is inferred to be a
// `DeferredWorld` from the alias, so the game holds one — and can call
// `get`, `entity` and `entity_mut` on it — having never named `DeferredWorld`,
// which the facade does not export. Measured, not reasoned: this exact
// snippet compiled against an earlier draft of this file.
//
// **The rule that follows.** A name allowlist is not sufficient. The surface
// must be closed under the types reachable through the *signatures* of the
// items on it: a re-exported alias, trait method or return type hands out
// values of types the list never mentions, and inherent methods need no
// import. Every item below was re-checked against that rule; `lifecycle` is
// the one that failed it.
//
// The cost, stated: component hooks are unavailable to game crates behind
// this facade. `#[derive(Component)]` itself does not need `lifecycle` — that
// was measured too, by deleting the module and rebuilding `facade_game` — so
// what is lost is `#[component(on_add = …)]`, not components.

/// `#[derive(Component)]`'s relationship machinery.
pub mod relationship {
    pub use bevy_ecs::relationship::{
        ComponentRelationshipAccessor, Relationship, RelationshipCloneBehaviorSpecialization,
        RelationshipTarget,
    };
}

/// `#[derive(Resource)]`. `IsResource` is the sealing marker the expansion
/// names; it is unusable on its own and is here only because the derive says
/// it must be.
pub mod resource {
    pub use bevy_ecs::resource::{IsResource, Resource};
}

// ── The access log ──────────────────────────────────────────────────────────

/// What an access was: the missing half is `Searched`.
///
/// `StateView::neighbor` (`crates/orrery_core/src/ruleset.rs:176`) records the
/// *lookup*, not the dereference — it pushes whether or not it found anything,
/// which is why `crates/orrery_core/src/replay.rs:325` can compare the
/// recorded sequence exactly. A query splits finding from reading and logs
/// only the second half, so absence — *"I never saw the occluder"* — becomes a
/// bit obtained with the log untouched. These three variants put the first
/// half back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    /// A lookup by name was performed. Recorded before the outcome is known,
    /// so an absent target is recorded exactly as a present one is.
    Searched,
    /// The lookup found a row and the caller received its data.
    Tapped,
    /// The id was yielded by an enumeration. The population is what was
    /// consulted, so the population is what is recorded.
    Enumerated,
}

/// One entry in the access log.
///
/// A named struct rather than a `(PersistId, AccessKind)` tuple: the two
/// fields are two different kinds of fact and a tuple would let them be read
/// positionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    /// The id that was searched for, tapped, or yielded.
    pub key: PersistId,
    /// Which of the three it was.
    pub kind: AccessKind,
}

/// The per-tick access log an [`OrderedQuery`] writes into.
#[derive(Resource, Debug, Default)]
pub struct AccessLog {
    entries: alloc::vec::Vec<Access>,
}

impl AccessLog {
    /// Every entry, in the order it was recorded.
    #[must_use]
    pub fn entries(&self) -> &[Access] {
        &self.entries
    }

    /// Drop everything. The host calls this at the start of each entity-tick,
    /// the way `StateView` is constructed fresh per entity-tick in
    /// `canonical_step` (`crates/orrery_core/src/executor.rs:382`).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The log in the shape `CanonicalOutcome::neighbor_reads` has today:
    /// deduplicated, in first-mention order.
    ///
    /// This is the bridge to `replay.rs:325`, which compares
    /// `outcome.neighbor_reads` against the ids of the `NeighborFrame` records
    /// in the window, elementwise. `StateView::neighbor` deduplicates
    /// (`ruleset.rs:196`, `if !self.reads.contains(&id)`) and keeps first-read
    /// order (`ruleset.rs:207`), so this does the same and the comparison is
    /// unchanged.
    ///
    /// `Tapped` contributes nothing a `Searched` did not already contribute,
    /// by construction — but `Enumerated` does, and that is the whole cost
    /// question. See the crate's spike document, §4.
    #[must_use]
    pub fn neighbor_reads(&self) -> alloc::vec::Vec<PersistId> {
        let mut out = alloc::vec::Vec::new();
        for entry in &self.entries {
            if !out.contains(&entry.key) {
                out.push(entry.key);
            }
        }
        out
    }

    fn push(&mut self, key: PersistId, kind: AccessKind) {
        self.entries.push(Access { key, kind });
    }
}

// ── Identity in the world ───────────────────────────────────────────────────

/// The canonical identity of a row, as a component.
///
/// `Entity` is a generational index whose value depends on spawn order, so it
/// is not a thing a log may contain and not a thing an ordering may be defined
/// by. `PersistId` is both.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PersistKey(pub PersistId);

/// The tick the row's state was observed at.
///
/// The ECS-side counterpart of `Executor::state_ticks`
/// (`crates/orrery_core/src/executor.rs:70`). Without it a query cannot answer
/// the #758 question at all — "is this state from my past, and how far past?"
/// — because a `World` row carries no provenance of its own.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedAt(pub Tick);

/// What the reader is allowed to see this entity-tick.
///
/// This is the resource that puts `StateView::neighbor`'s two refusals
/// (`crates/orrery_core/src/ruleset.rs:176`) inside the query: the
/// own-identity refusal (`id != self.entity`) and the `checked_sub` staleness
/// bound (`ruleset.rs:192`).
///
/// It is a *host* resource, and it has to be, because neither half is
/// derivable inside a rule: `Ruleset::step` is handed no tick
/// (`ruleset.rs:338`), and the stepping entity's identity reaches a rule only
/// through `StateView::entity`. Whoever drives the tick stamps this before the
/// step, exactly as `canonical_step` stamps `StateView::observed`
/// (`executor.rs:382`).
#[derive(Resource, Debug, Clone, Copy)]
pub struct ReadWindow {
    reader: Option<PersistId>,
    tick: Tick,
    max_staleness_ticks: Option<u64>,
}

impl ReadWindow {
    /// The window a host stamps: this reader, this tick, this ruleset's cap.
    ///
    /// The three arguments are exactly `StateView::observed`'s
    /// `entity`, `tick` and `staleness_cap`.
    #[must_use]
    pub const fn observed(reader: PersistId, tick: Tick, max_staleness_ticks: u64) -> Self {
        Self {
            reader: Some(reader),
            tick,
            max_staleness_ticks: Some(max_staleness_ticks),
        }
    }

    /// No identity refusal and no staleness bound — #815's `OrderedQuery`,
    /// reproduced.
    ///
    /// This exists so the refutations in
    /// `tests/replay_through_ordered_query.rs` stay *runnable* rather than
    /// becoming prose about a version of the type that no longer exists. It is
    /// not a mode any host may ship: a window stamped this way reads its own
    /// state as a neighbour and reads observations from any tick, and both
    /// diverge from `StateView` in ways that test file measures.
    #[must_use]
    pub const fn open() -> Self {
        Self {
            reader: None,
            tick: Tick::new(0),
            max_staleness_ticks: None,
        }
    }

    /// Whether a row observed at `observed` may be read this tick.
    ///
    /// `checked_sub`, not saturating, and for #758's reason: an observation
    /// stamped *ahead* of the reader is state from the reader's future,
    /// arrived by replication, which `ReplayHarness` refuses as a malformed
    /// frame (`crates/orrery_core/src/replay.rs:258`). A live read that
    /// accepted it would disagree with adjudication about the same log.
    fn admits(&self, observed: Tick) -> bool {
        let Some(cap) = self.max_staleness_ticks else {
            return true;
        };
        self.tick
            .0
            .checked_sub(observed.0)
            .is_some_and(|age| age <= cap)
    }

    /// Whether `key` is the stepping entity itself.
    fn is_reader(&self, key: PersistId) -> bool {
        self.reader == Some(key)
    }
}

/// `PersistId` → `Entity`, maintained by whoever spawns.
///
/// A search by name needs this; without it `get` would have to scan, and a
/// scan is an enumeration wearing a lookup's clothes.
#[derive(Resource, Debug, Default)]
pub struct KeyIndex {
    by_key: BTreeMap<PersistId, Entity>,
}

impl KeyIndex {
    /// Record a row's identity.
    pub fn insert(&mut self, key: PersistId, entity: Entity) {
        self.by_key.insert(key, entity);
    }

    /// Forget a row's identity.
    pub fn remove(&mut self, key: PersistId) {
        self.by_key.remove(&key);
    }

    /// The `Entity` a `PersistId` names, if the index holds one.
    ///
    /// Added by #815's follow-up lane, and the reason is worth recording: a
    /// host that mirrors an authoritative store into this `World` — which is
    /// what `tests/replay_through_ordered_query.rs` does — has to *update*
    /// a row it already spawned, and could not, because the map was
    /// write-only. Reading it is a host capability, not a game one:
    /// `Entity` is exported, but nothing a game crate can name turns one
    /// into a component, so this widens the host's surface and not the
    /// game's.
    #[must_use]
    pub fn entity(&self, key: PersistId) -> Option<Entity> {
        self.by_key.get(&key).copied()
    }
}

// ── OrderedQuery ────────────────────────────────────────────────────────────

/// A query that logs what was searched for as well as what was tapped, and
/// enumerates in `PersistId` order.
///
/// Built as a plain `#[derive(SystemParam)]` over upstream `Query` — no fork,
/// no vendoring. `crates/orrery_sim_host/src/ecs.rs:367` already does exactly
/// this for `SectionStore`, so the shape is established in this tree.
///
/// Two things it is not:
///
/// * It is **not** a drop-in for `Query`. There is no `Deref`, no `IntoIterator`
///   and no `get` that returns a `Result` — a `Result` discriminant is an
///   existence bit, and handing one back would reintroduce the leak the type
///   exists to close.
/// * It does **not** make honesty automatic. A system that calls
///   [`Self::enumerate`] and branches on the length has recorded the
///   population, which is the point; a system that never calls anything has
///   recorded nothing and learned nothing. What it removes is the shape where
///   a rule learns something and records nothing.
#[derive(bevy_ecs::system::SystemParam)]
pub struct OrderedQuery<'w, 's, D>
where
    D: bevy_ecs::query::QueryData + 'static,
{
    rows: bevy_ecs::system::Query<'w, 's, (&'static PersistKey, &'static ObservedAt, D)>,
    index: bevy_ecs::system::Res<'w, KeyIndex>,
    window: bevy_ecs::system::Res<'w, ReadWindow>,
    log: bevy_ecs::system::ResMut<'w, AccessLog>,
}

impl<'s, D> OrderedQuery<'_, 's, D>
where
    D: bevy_ecs::query::QueryData + 'static,
{
    /// Look one row up by name, recording the lookup before its outcome is
    /// known.
    ///
    /// The `Searched` entry is pushed on the first line, so the five ways this
    /// can return `None` — no such id, the id has no matching row, the row
    /// does not satisfy `D`, the id is the reader itself, the row's
    /// observation is staler than the window admits — are indistinguishable in
    /// the log from each other and from a hit. That is the property
    /// `query.get(e)` cannot have: its `Result` tells the caller which case it
    /// was, for free.
    ///
    /// The last two refusals arrived with [`ReadWindow`], and they are not
    /// optional decoration: a query without them reads its own state as a
    /// neighbour and consumes observations the log records as never delivered.
    /// Both are measured in `tests/replay_through_ordered_query.rs`, against a
    /// real `ReplayHarness`.
    pub fn get<'a>(
        &'a mut self,
        key: PersistId,
    ) -> Option<<D as bevy_ecs::query::QueryData>::Item<'a, 's>> {
        self.log.push(key, AccessKind::Searched);
        // The reader's own state is not a neighbour observation. `StateView`
        // refuses it by identity and records the ask anyway
        // (`ruleset.rs:176`); so does this, and in the same order.
        if self.window.is_reader(key) {
            return None;
        }
        let entity = self.index.entity(key)?;
        let window = *self.window;
        let (_, observed, item) = self.rows.get_mut(entity).ok()?;
        // Read the stamp out before anything else borrows: #758's bound is a
        // property of the row, and a row that fails it is *hidden*, not
        // absent — the search above is already recorded either way.
        if !window.admits(observed.0) {
            return None;
        }
        self.log.push(key, AccessKind::Tapped);
        Some(item)
    }

    /// Every matching row's id, in ascending `PersistId` order, recording each.
    ///
    /// Ordering by `PersistId` rather than by archetype is what makes
    /// iteration canonical: `bevy_ecs` iteration order is a function of
    /// insertion history, which is exactly what D43 (e)(4)'s
    /// permuted-insertion-order projection differential exists to catch.
    ///
    /// Every yielded id is recorded, because the yielded set *is* what was
    /// consulted. That is honest and it is expensive; §4 of the spike document
    /// costs it against `Ruleset::max_neighbor_reads`.
    pub fn enumerate(&mut self) -> alloc::vec::Vec<PersistId> {
        let window = *self.window;
        let mut keys: alloc::vec::Vec<PersistId> = self
            .rows
            .iter()
            .filter(|(key, observed, _)| !window.is_reader(key.0) && window.admits(observed.0))
            .map(|(key, _, _)| key.0)
            .collect();
        keys.sort_unstable();
        for key in &keys {
            self.log.push(*key, AccessKind::Enumerated);
        }
        keys
    }

    /// The log so far, for a host that wants to read it without a `Res`.
    #[must_use]
    pub fn log(&self) -> &AccessLog {
        &self.log
    }
}
