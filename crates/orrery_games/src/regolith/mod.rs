//! **Regolith** — planar combat, deterministic density, death loops and scoring.
//!
//! A split is described entirely by an ordered event. Its parent records the
//! monotone split counter in its own state, so materialization is adjudicable.

/// Declare a canonical system that runs only on entities carrying `$variant`.
///
/// This is the projection half of an ECS query, and deliberately only that
/// half. The runner never scans a population — it hands each system the one
/// entity being stepped — so a rule cannot reach a neighbour it did not
/// record, and the `match own { .. }` four-way dispatch that used to open
/// `Ruleset::step` disappears into the table instead of being rewritten in
/// every rule body.
macro_rules! projected_system {
    ($name:literal, $variant:ident, $body:path) => {
        ::orrery_core::System {
            name: ::orrery_core::SystemName($name),
            run: {
                fn run(
                    state: &mut $crate::regolith::state::RegolithState,
                    cx: &mut $crate::regolith::Cx<'_>,
                ) {
                    if let $crate::regolith::state::RegolithState::$variant(component) = state {
                        $body(component, cx);
                    }
                }
                run
            },
        }
    };
}

/// A system over `&mut Craft`.
macro_rules! craft_system {
    ($name:literal, $body:path) => {
        projected_system!($name, Craft, $body)
    };
}

/// A system over `&mut Rock`.
macro_rules! rock_system {
    ($name:literal, $body:path) => {
        projected_system!($name, Rock, $body)
    };
}

/// A system over `&mut Pickup`.
macro_rules! pickup_system {
    ($name:literal, $body:path) => {
        projected_system!($name, Pickup, $body)
    };
}

/// A system over `&mut BloomDirector`.
macro_rules! director_system {
    ($name:literal, $body:path) => {
        projected_system!($name, BloomDirector, $body)
    };
}

/// A system over the whole state enum, for rules that span components.
macro_rules! state_system {
    ($name:literal, $body:path) => {
        ::orrery_core::System {
            name: ::orrery_core::SystemName($name),
            run: $body,
        }
    };
}

/// Declare the one kind of system that may read a recorded neighbour.
///
/// An [`::orrery_core::Observation`] is the only system shape handed a
/// `StateView`. Every other system receives `&mut RegolithState` and has
/// nothing to read a neighbour *from* — the audited-predicate rule that
/// `scripts/core-gates.sh` clause 5 enforces textually is enforced here by the
/// signature as well.
macro_rules! observation {
    ($name:literal, $body:path) => {
        ::orrery_core::Observation {
            name: ::orrery_core::SystemName($name),
            run: $body,
        }
    };
}

pub mod archetype;
mod craft;
#[doc(hidden)]
pub mod craft_ecs;
pub mod invariants;
#[doc(hidden)]
pub mod native_ecs;
pub mod order;
pub mod pilot;
pub mod state;
mod visibility;
pub mod weapon;
mod world;
#[doc(hidden)]
pub mod world_ecs;

use crate::game::{Game, GameMeta, Tamper};
use archetype::Archetype;
use order::{ChildSpec, LockBreakReason, Order, Outcome, ShotResult};
use orrery_compose::{
    AmbiguityDetection, CanonicalSchedule, CompatibilityManifest, ComponentCapabilities,
    ComponentSchemaId, ComponentSchemaManifest, ExecutorPolicy, GameId, ManifestFormatVersion,
    ModuleId, ModuleManifest, ModuleVersion, PersistenceCapability, ProfileId, ProjectionVersion,
    ReplicationCapability, RollbackCapability, ScheduleOrderingEdge, ScheduleStageId,
    ScheduleStageManifest, StateSectionId, SystemId, WitnessCapability, WriteAuthorityCapability,
};
use orrery_core::{
    EntityMaterialization, Invariant, Observation, OrderedInputs, QPos, QVel, Ruleset, Schedule,
    Scheduled, Stage, StageName, StateView, StepCtx, StepOutput, System, TickRng, TICK_HZ,
};
use orrery_protocol::{PersistId, RulesetId, Tick};
use rand_core::RngCore;
use state::{
    BloomDirector, BloomMembership, Craft, LockClass, Pickup, RegolithState, Rock, RockTier,
    TAU_URAD,
};

const DT: f64 = 1.0 / TICK_HZ as f64;
/// Drag shared by craft rules and stage-1 acceleration bounds.
pub const DRAG_PER_SEC_PER_MILLE: i64 = 50;
const DRAG_PER_SEC: f64 = DRAG_PER_SEC_PER_MILLE as f64 / 1_000.0;
const SPAWN_RADIUS_MM: f64 = 150_000.0;
const GOLDEN_ANGLE_URAD: i64 = 2_399_963;
/// Bloom cadence: 60 seconds at 60 Hz.
pub const BLOOM_CADENCE_TICKS: u64 = 3_600;
/// Maximum bloom-site lifetime: 90 seconds at 60 Hz.
pub const BLOOM_LIFETIME_TICKS: u64 = 5_400;
/// Rocks seeded by one bloom: 2 Large, 3 Medium and 5 Small.
pub const BLOOM_ROCK_COUNT: u16 = 10;
/// Largest live descendant population reachable from one bloom batch.
pub const BLOOM_MAX_LIVE_ROCKS: u16 = 19;
/// Half-width of the square central region used for bloom site draws.
pub const BLOOM_CENTRAL_RADIUS_MM: i64 = 250_000;
/// Wreck countdown: two seconds at 60 Hz.
pub const RESPAWN_TICKS: u16 = 120;
/// Maximum craft windows in one island.
pub const ISLAND_CRAFT_BUDGET: u16 = 8;
/// Steady-state rock-window target in one island.
pub const ISLAND_ROCK_BUDGET: u16 = 24;
/// Outstanding pickup-window target in one island.
pub const ISLAND_PICKUP_BUDGET: u16 = 4;
/// BloomDirector windows in one island.
pub const ISLAND_DIRECTOR_BUDGET: u16 = 1;
/// Published total island-window budget.
pub const ISLAND_WINDOW_BUDGET: u16 =
    ISLAND_CRAFT_BUDGET + ISLAND_ROCK_BUDGET + ISLAND_PICKUP_BUDGET + ISLAND_DIRECTOR_BUDGET;
/// Score value of one delivered craft kill.
pub const KILL_SCORE_POINTS: u64 = 25;
/// Score value of one delivered pickup win.
pub const PICKUP_SCORE_POINTS: u64 = 5;
/// Rocks reflect from this square island edge with integer velocity negation.
pub const ISLAND_BOUNDARY_MM: i64 = 1_000_000;

// ── the island tether (#955) ────────────────────────────────────────────
//
// The island edge above is the ruleset's own statement of where the game is:
// every rock reflects off it, blooms seed inside a quarter of it, and craft
// spawn within 150 m of its centre. Craft were the one body it did not reach.
//
// #955 measured what that cost: the replication interest block is 1536 m
// across and the interceptor ceiling is 480 m/s, so three seconds of held
// throttle carried the 2026-09-02 volunteer out of the populated volume, and
// the remaining fourteen minutes of their session witnessed an empty region.
// The throttle was binary and authored — the flying was legitimate, which is
// why no host-side signal could catch it.
//
// The tether is not a wall and not a reflection. Outside the boundary, and
// only on a velocity component that points *further out*, an extra linear
// drag ramps in over one interest cell. Inward motion is never touched, so a
// pilot who turns around is free the instant they do, and nothing here can
// trap a craft or bounce it in a direction it did not ask for. Leaving stays
// possible; it stops being free.

/// Width of the tether's ramp band beyond [`ISLAND_BOUNDARY_MM`], millimetres.
///
/// One interest cell edge ([`CAMPAIGN_CELL_EDGE_M`]). The band is the stretch
/// over which the restoring drag grows from nothing to full, and sizing it to
/// a cell edge keeps the rule and the skin's cue in one currency: a craft
/// crosses at most one cell's worth of space while the tether comes in.
pub const TETHER_BAND_MM: i64 = 512_000;
const _: () = assert!(TETHER_BAND_MM as f64 == CAMPAIGN_CELL_EDGE_M * 1_000.0);

/// Seconds between two bloom announcements.
const BLOOM_CADENCE_SECS: i64 = BLOOM_CADENCE_TICKS as i64 / TICK_HZ as i64;

/// Outward speed a fully tethered craft can still hold, millimetres per second.
///
/// The island's own diameter per bloom cadence: a pilot who holds the throttle
/// outward against a fully-ramped tether needs the whole interval between two
/// bloom announcements to put one island-width between themselves and it.
/// #955's measurement was a comparable distance crossed in three seconds; this
/// is the same distance turned back into a decision.
pub const TETHER_ESCAPE_SPEED_MMS: i64 = ISLAND_BOUNDARY_MM * 2 / BLOOM_CADENCE_SECS;

/// Total drag a fully tethered craft flies against, per-mille per second.
///
/// Under linear drag a craft at constant thrust settles at `accel / drag`, so
/// this is the drag that puts the *fastest* chassis at
/// [`TETHER_ESCAPE_SPEED_MMS`]. Every slower chassis settles slower still,
/// which is the right ordering: the interceptor that produced #955 is the one
/// the number is sized against.
const TETHER_TOTAL_DRAG_PER_SEC_PER_MILLE: i64 =
    archetype::Archetype::Interceptor.limits().max_accel_mmss * 1_000 / TETHER_ESCAPE_SPEED_MMS;

/// Extra drag the tether adds at full ramp, on the outward component only.
pub const TETHER_DRAG_PER_SEC_PER_MILLE: i64 =
    TETHER_TOTAL_DRAG_PER_SEC_PER_MILLE - DRAG_PER_SEC_PER_MILLE;

// The tether must actually restrain, and its per-tick retention must stay in
// (0, 1) or the integrator would invert velocity instead of damping it —
// which is the reflection this deliberately is not.
const _: () = assert!(TETHER_DRAG_PER_SEC_PER_MILLE > 0);
const _: () = assert!(TETHER_TOTAL_DRAG_PER_SEC_PER_MILLE < 1_000 * TICK_HZ as i64);
const JITTER_MIN_URAD: u32 = 785_398;
const JITTER_MAX_URAD: u32 = 1_308_997;
/// Pickup lifetime: 30 seconds at 60 Hz.
pub const PICKUP_TTL_TICKS: u16 = 1_800;
/// Maximum eligible grab distance, in millimetres.
pub const GRAB_RADIUS_MM: i64 = 25_000;
/// Held-lock ticks required to acquire a target lock.
pub const LOCK_ACQUISITION_TICKS: u16 = 30;
/// A held lock takes the same half-second premise to break as to acquire.
pub const LOCK_BREAK_TICKS: u16 = LOCK_ACQUISITION_TICKS;
/// Progress removed per occluded tick, derived from the acquisition and break windows.
pub const LOCK_DECAY_PER_TICK: u16 = LOCK_ACQUISITION_TICKS.div_ceil(LOCK_BREAK_TICKS);
const _: () = assert!(
    LOCK_BREAK_TICKS > 1,
    "lock breaking must span ticks or the decay acceptance test asserts nothing"
);
/// Visibility-transition claims are capped at four per second.
pub const COVER_CLAIM_INTERVAL_TICKS: u16 = (TICK_HZ / 4) as u16;
/// Maximum distinct neighbour frames the audited claims stage can read per step.
pub const MAX_NEIGHBOR_READS: usize = visibility::MAX_NEIGHBOR_READS;
/// Claims arrive at 2 Hz; one missed claim is tolerated before refusing a frame.
pub const MAX_NEIGHBOR_STALENESS_TICKS: u64 = TICK_HZ as u64;
/// Two-centimetre inward margin: twice VC-7's one-centimetre position epsilon.
pub const OCCLUSION_MARGIN_MM: i64 = 20;
const REFERENCE_SIGNATURE_RADIUS_MM: u128 = 3_000;
const CHANCE_SCALE: u128 = 1_000_000;
const CAMPAIGN_ORBIT_RADIUS_M: f64 = 2_500.0;
const CAMPAIGN_CROWD_ARC_RAD: f64 = 0.08;
const CAMPAIGN_RADIAL_SPREAD: f64 = 0.003;
/// Rocks present at campaign start: one Large, two Medium and three Small.
pub const CAMPAIGN_ROCK_COUNT: usize = 6;
const CAMPAIGN_ROCK_RADII_MM: [i64; CAMPAIGN_ROCK_COUNT] = [
    2_710_000, 2_790_000, 2_320_000, 2_840_000, 2_260_000, 2_890_000,
];
const CAMPAIGN_ROCK_TIERS: [RockTier; CAMPAIGN_ROCK_COUNT] = [
    RockTier::Large,
    RockTier::Medium,
    RockTier::Medium,
    RockTier::Small,
    RockTier::Small,
    RockTier::Small,
];
/// The widest body a weapon can be locked onto, in millimetres.
///
/// `projectile_resolution` adds the *target's* radius to the weapon envelope
/// before it compares centre-to-centre range, and interest membership keys off
/// a body's centre. So the AOI has to cover the weapon envelope plus the
/// largest radius anything adjudicable can carry — craft chassis and rock
/// tiers alike, both read from their own tables rather than restated.
pub const MAX_TARGET_RADIUS_MM: i64 = {
    let mut widest = 0;
    let mut index = 0;
    while index < Archetype::ALL.len() {
        let radius = Archetype::ALL[index].limits().radius_mm;
        if radius > widest {
            widest = radius;
        }
        index += 1;
    }
    let mut index = 0;
    while index < RockTier::ALL.len() {
        let radius = RockTier::ALL[index].limits().radius_mm;
        if radius > widest {
            widest = radius;
        }
        index += 1;
    }
    widest
};

/// The longest centre-to-centre range at which the ruleset will still resolve
/// a shot, in millimetres: the widest weapon envelope plus the widest target.
///
/// 400 m at the current table — Heavy's 300 m optimal plus 60 m falloff plus
/// a 40 m Large rock.
pub const MAX_ENGAGEMENT_RANGE_MM: i64 =
    weapon::MAX_WEAPON_REACH_MM.saturating_add(MAX_TARGET_RADIUS_MM);

/// The hysteresis margin as a fraction of the cell edge, in per-mille.
///
/// This restates `orrery_spatial::SpatialConfig`'s 0.10 default (D16) in the
/// integer form the sizing below needs. `orrery_games` does not link Bevy, so
/// the two cannot be one declaration; `orrery`'s
/// `the_campaign_aoi_uses_the_frameworks_own_hysteresis_margin` pins them
/// together from the side that sees both.
pub const CAMPAIGN_HYSTERESIS_PER_MILLE: i64 = 100;

/// Commitment lags the campaign's engagement budget must absorb.
///
/// `docs/01-spatial-model.md` §7 calls `edge − m` the one-body visibility
/// radius: it accounts for the observer's hysteretic commitment. Interest
/// membership compares two *committed* cells, though, and
/// `orrery_spatial::hysteresis` latches each entity's commitment
/// independently — "the committed `Cell` lags the geometric cell by at most
/// the hysteresis margin", per entity. So the target can sit a full margin
/// inside the observer's block geometrically while still being committed to
/// the cell outside it, and the two lags compose adversarially.
///
/// This is why the campaign budgets `edge − 2m` rather than shaving an
/// arbitrary safety factor off `edge − m`: the headroom is a mechanism, not a
/// fudge. It is also why Stock's old 20.8 m of margin was not margin at all.
pub const CAMPAIGN_COMMITMENT_LAGS: i64 = 2;

/// Cell edges the campaign may choose from: whole multiples of the framework
/// default, so the campaign grid stays a coarsening of the framework's.
const CAMPAIGN_EDGE_QUANTUM_MM: i64 = 128_000;

/// The smallest campaign cell edge whose engagement budget still covers the
/// ruleset's longest engagement, in metres.
///
/// With the budget at `edge − 2m` and `m = edge / 10` that is `0.8 · edge`,
/// so covering an engagement range `r` needs
///
/// ```text
/// 0.8 · edge ≥ r        =>        edge ≥ r · 10 / 8
/// ```
///
/// rounded up to the next whole framework cell.
///
/// **This is a diagnostic, not the edge.** The campaign edge is held at 512 m
/// deliberately (see [`CAMPAIGN_CELL_EDGE_M`]); what this figure does is name
/// the edge a table would *need*, so that a weapon growing past the budget
/// fails with an actionable number instead of a bare inequality. #520 sized
/// the edge to the stock weapon's 400 m and #545 arrived one weapon later, so
/// the bound is derived from the whole table rather than from whichever
/// weapon was longest when someone last looked.
pub const CAMPAIGN_MIN_CELL_EDGE_M: f64 = {
    let budget_per_mille = 1_000 - CAMPAIGN_COMMITMENT_LAGS * CAMPAIGN_HYSTERESIS_PER_MILLE;
    // Integer division truncates, so round the quotient up before quantising:
    // an edge one millimetre short of the requirement is still short.
    let needed_mm = (MAX_ENGAGEMENT_RANGE_MM * 1_000 + budget_per_mille - 1) / budget_per_mille;
    let quantised = needed_mm.div_euclid(CAMPAIGN_EDGE_QUANTUM_MM) * CAMPAIGN_EDGE_QUANTUM_MM;
    let quantised = if quantised < needed_mm {
        quantised + CAMPAIGN_EDGE_QUANTUM_MM
    } else {
        quantised
    };
    quantised as f64 / 1_000.0
};

/// Campaign interest-cell edge. **The weapon table is sized to this, not the
/// other way round.**
///
/// The framework default is 128 m, whose 27-cell AOI guarantees only 102.4 m
/// for a pairwise interaction — inside every weapon's envelope. 512 m is the
/// coarsening that gives the campaign a usable engagement budget while
/// keeping the block small enough that craft actually cross cells.
///
/// That crossing is the point. The campaign is the shakedown for interest
/// churn under jitter and loss: contacts entering and leaving the set,
/// commitment latching and unlatching, replicas expiring and re-installing. A
/// block wide enough to swallow the encounter — the crowd orbits at roughly
/// 2.5 km — would delete that test surface, so widening the edge to fit a
/// long gun is the wrong trade. When a weapon does not fit, the weapon
/// yields: `every_weapons_reach_fits_inside_the_campaign_aoi_guarantee`
/// fails, and [`CAMPAIGN_MIN_CELL_EDGE_M`] names what it would have cost.
pub const CAMPAIGN_CELL_EDGE_M: f64 = 512.0;

/// The radius around an observer in which the 27-cell AOI is *guaranteed* to
/// hold a body, in metres, for a given cell edge.
///
/// `edge − m` with `m` the hysteresis margin — the observer's commitment
/// lagging position by a full margin (`docs/01-spatial-model.md` §7).
/// 460.8 m at the campaign edge.
#[must_use]
pub fn campaign_guaranteed_aoi_radius_m(cell_edge_m: f64) -> f64 {
    cell_edge_m * (1_000 - CAMPAIGN_HYSTERESIS_PER_MILLE) as f64 / 1_000.0
}

/// The centre-to-centre range the weapon table is allowed to occupy, in
/// metres, for a given cell edge: `edge − 2m`.
///
/// The guarantee above minus one further commitment lag, because the target
/// latches its own commitment too — see [`CAMPAIGN_COMMITMENT_LAGS`]. 409.6 m
/// at the campaign edge, against a 400 m table.
#[must_use]
pub fn campaign_engagement_budget_m(cell_edge_m: f64) -> f64 {
    cell_edge_m * (1_000 - CAMPAIGN_COMMITMENT_LAGS * CAMPAIGN_HYSTERESIS_PER_MILLE) as f64
        / 1_000.0
}

/// Regolith v24's rules identity: v23's canonical behaviour under protocol v7,
/// with the remaining `regolith.craft` module executed as native ECS
/// components and systems alongside `regolith.world`.
///
/// The F-4 differential compares the legacy and native paths over D-1 through
/// D-4 and requires byte parity. The version still advances because D49's
/// source identity moved; the unchanged component schema and projection axes
/// remain independently equal.
///
/// Regolith v23 moved `regolith.world` to native ECS execution.
///
/// Regolith v22's rules identity was v21's gameplay rules under protocol v7.
/// Adding the server-owned restore record moved the pinned first-party source
/// closure, so the identity advances even though the canonical step does not.
///
/// Regolith v21 changed v20 by keeping campaign participants in one
/// replicating crowd (#788).
///
/// The canonical step is unchanged, but campaign initialization is not. At 32
/// seats the former 10% radial spread quantized the bots' orbit inputs across
/// 22 turn rates, shearing an initially compact crowd across kilometres during
/// the campaign hour. The 0.3% spread keeps all seats on the same integer turn
/// rate and their initial canonical states inside one overlapping AOI.
///
/// **Bumped on principle, not on a moved golden.** The committed corpus uses
/// [`Game::spawn`]'s compact scenario ring, not [`campaign_spawn_pose`], so
/// its chains remain byte-identical. Campaign peers still derive different
/// tick-zero positions either side of this change. Calling both builds v20
/// would let them enter one session and disagree before the first input; v21
/// makes admission refuse that mixed campaign instead.
/// **v25 is the island tether** (#955). Craft became subject to
/// [`ISLAND_BOUNDARY_MM`], the island edge every rock already reflects off, as
/// an outward-only restoring drag. It is a rules change and not a cue, so it
/// moves the ruleset digest, the schedule digest and the state goldens
/// together, and admission must refuse a v24 peer rather than let one enter a
/// session where held throttle means something different.
pub const REGOLITH_RULESET: RulesetId = RulesetId {
    version: 25,
    digest: crate::ruleset_digest::RULESET_DIGEST,
};

/// Regolith's two statically linked rule domains.
///
/// [`Ruleset::step`] remains the sole executor entry point; what changed is
/// that `schedule_stages` is no longer `&[]`. Each module now names the stages
/// whose systems it owns, and those stages hold the actual `const` tables the
/// tick runs ([`craft::CONTROL`], [`world::RESOLUTION`], and so on) — so a
/// module is a place rules live rather than a description of where they would
/// live if there were anywhere to put them. The craft module owns `Craft`
/// state and player/combat orders; the world module owns rocks, pickups, and
/// the bloom director. Their only coupling is the existing ordered `Outcome`
/// -> next-tick `Order` delivery channel owned by [`Game::deliver`].
pub const REGOLITH_MODULES: &[ModuleManifest] = &[
    ModuleManifest {
        id: ModuleId("regolith.craft"),
        version: ModuleVersion(1),
        dependencies: &[ModuleId("regolith.world")],
        state_sections: &[StateSectionId("craft")],
        inputs: &[orrery_compose::InputVocabularyId(
            "craft-control-and-resolution",
        )],
        events: &[orrery_compose::EventVocabularyId(
            "craft-requests-and-credit",
        )],
        // The audited observation stage and the claim-application stage are
        // declared here rather than on `regolith.world` because the claims
        // themselves are craft-originated — a cover claim names a locker, a
        // collision claim names a craft's counterparty. `claims-apply` writes
        // one world-owned field (a rock's overflow flag), which is why the
        // world module declares it too.
        schedule_stages: &[
            STAGE_OBSERVE,
            STAGE_CRAFT_CONTROL,
            STAGE_CRAFT_MOTION,
            STAGE_CLAIMS_APPLY,
        ],
    },
    ModuleManifest {
        id: ModuleId("regolith.world"),
        version: ModuleVersion(1),
        dependencies: &[],
        state_sections: &[
            StateSectionId("rock"),
            StateSectionId("pickup"),
            StateSectionId("bloom-director"),
        ],
        inputs: &[orrery_compose::InputVocabularyId(
            "world-resolution-and-lifecycle",
        )],
        events: &[orrery_compose::EventVocabularyId(
            "world-responses-and-materialization",
        )],
        schedule_stages: &[
            STAGE_WORLD_RESOLUTION,
            STAGE_WORLD_LIFECYCLE,
            STAGE_CLAIMS_APPLY,
        ],
    },
];

/// Regolith's component-schema table, stated from the reviewed ledger.
///
/// This is the derived half of the split #750 settled:
/// `orrery_compose::registry::regolith` is canonical for D45 clause (a)'s
/// schema id of record — the `(ComponentTypeId, SchemaVersion)` pair — and
/// this table restates that pair with the two things a ledger does not carry,
/// the owning module and D45's five capability dimensions. The agreement is
/// asserted, not assumed: see
/// [`composition_tests::the_manifest_schema_table_agrees_with_the_reviewed_registry`].
///
/// **The capabilities are ADR-0045 clause (d)'s `Core` profile, whose in-tree
/// example is this exact component** — `P1` bulk persistence, `R1` rollback
/// membership, `W2` replay-adjudicated, `N1` interest-replicated, `A1`
/// lease-holder — and the row is legal under every clause (e) prohibition
/// that could reach it: `W2` has its single fenced writer (IV-1) and its
/// deterministic [`orrery_core::CoreCodec`] (IV-2); `P1` is not the `P2`
/// IV-3 and IV-5 constrain; the component is not on an ephemeral identity
/// (IV-4); and `N1` is not paired with `A0` (IV-6).
///
/// **On `owner`, honestly.** One allocation covers *both* modules' state
/// sections — [`state::RegolithState`]'s variants are exactly
/// `regolith.craft`'s `craft` plus `regolith.world`'s `rock`, `pickup` and
/// `bloom-director` — because the id predates the module split, and the
/// manifest names one owner per row. It names `regolith.craft`, the only
/// module from which every section of the component is reachable in the
/// declared dependency order (craft depends on world; world does not depend
/// on craft). Splitting `STATE` into per-section components would be a new
/// reviewed allocation and an at-rest change, so it is deliberately not done
/// here.
pub const REGOLITH_COMPONENT_SCHEMAS: &[ComponentSchemaManifest] = &[ComponentSchemaManifest {
    owner: ModuleId("regolith.craft"),
    id: ComponentSchemaId {
        component: components::STATE,
        version: orrery_protocol::atrest::SCHEMA_V0,
    },
    capabilities: ComponentCapabilities {
        persistence: PersistenceCapability::Bulk,
        rollback: RollbackCapability::Included,
        witness: WitnessCapability::ReplayAdjudicated,
        replication: ReplicationCapability::InterestReplicated,
        write_authority: WriteAuthorityCapability::LeaseHolder,
    },
}];

/// The assembled, validated-at-registration composition manifest for Regolith.
pub const REGOLITH_COMPOSITION: CompatibilityManifest = CompatibilityManifest {
    game_id: GameId("regolith"),
    manifest_format_version: ManifestFormatVersion(1),
    protocol_version: 7,
    toolchain_stamp: "rust-2024",
    ruleset: REGOLITH_RULESET,
    modules: REGOLITH_MODULES,
    component_schemas: REGOLITH_COMPONENT_SCHEMAS,
    schedule: REGOLITH_CANONICAL_SCHEDULE,
    canonical_constants: &[],
    projection_version: ProjectionVersion(1),
    profile_id: ProfileId("d9"),
    removed_components: &[],
};

/// One canonical rock installed by the campaign composition root.
///
/// `owner_slot` is content, not a transport guess: it gives every seeded
/// entity exactly one initial authority while allowing a peer to host more
/// than its player craft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignRockSeed {
    /// Stable persistent identity derived from the universe seed and slot.
    pub entity: PersistId,
    /// Headless host slot holding the rock's initial authority.
    pub owner_slot: usize,
    /// Complete canonical starting state.
    pub rock: Rock,
}

/// Build the rocks present at campaign start.
///
/// This is deliberately a direct seed, not a bloom director. The stock
/// director waits sixty seconds and draws sites inside 250 m of the origin;
/// the campaign crowd orbits at roughly 2.5 km, so that faithful bloom would
/// still be content nobody sees. Six rocks make every published tier present
/// without turning the crowd's orbit into a collision gauntlet: they sit in a
/// radial pocket just inside and outside the outer crowd, inside its 400 m
/// engagement envelope but off every bot's initial flight line. A player can
/// see, lock and deliberately fly into the pocket; orbiting bots do not begin
/// in it.
///
/// Identity, the small angular/radial variation, tier and owner are pure
/// functions of `(universe seed, campaign-rock slot, host peer count)`. No
/// clock, process RNG or other ambient input can enter replay.
#[must_use]
pub fn campaign_rock_seeds(
    seed: orrery_protocol::UniverseSeed,
    host_peer_count: usize,
) -> [CampaignRockSeed; CAMPAIGN_ROCK_COUNT] {
    core::array::from_fn(|slot| {
        let mut preimage = [0u8; 24];
        preimage[..20].copy_from_slice(b"regolith-campaign-v1");
        preimage[20..].copy_from_slice(&(slot as u32).to_le_bytes());
        let digest = blake3::keyed_hash(&seed.0, &preimage);
        let bytes = digest.as_bytes();

        // Keep the whole pocket close to the exterior slot for every seed;
        // variation makes the universe seed real content without allowing a
        // draw to move the rocks back to the empty central bloom region.
        let angular_jitter_urad =
            i64::from(u16::from_le_bytes([bytes[0], bytes[1]])) % 6_001 - 3_000;
        let radial_jitter_mm =
            (i64::from(u16::from_le_bytes([bytes[2], bytes[3]])) % 10_001) - 5_000;
        let angle = (68_000 + angular_jitter_urad) as f64 / 1_000_000.0;
        let radius_mm = CAMPAIGN_ROCK_RADII_MM[slot].saturating_add(radial_jitter_mm);
        let pos = QPos {
            x: (radius_mm as f64 * libm::cos(angle)) as i64,
            y: 0,
            z: (radius_mm as f64 * libm::sin(angle)) as i64,
        };

        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&bytes[8..16]);
        let entity = PersistId::new(
            0xC524_0000_0000_0000 | (u64::from_le_bytes(id_bytes) & 0x0000_FFFF_FFFF_FFFF),
        );
        CampaignRockSeed {
            entity,
            owner_slot: slot % host_peer_count.max(1),
            rock: Rock::spawned(CAMPAIGN_ROCK_TIERS[slot], 0, pos, QVel::default()),
        }
    })
}

/// Component classifications.
pub mod components {
    use orrery_core::ComponentTypeId;
    /// Verifiable Regolith state for every entity-window variant.
    pub const STATE: ComponentTypeId = orrery_compose::registry::regolith::STATE;
}

/// Regolith rules, optionally carrying one deliberate P4 tamper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Regolith {
    tamper: Option<Tamper>,
}

/// Nominate the nearest contact that is worth submitting as [`Order::Collide`].
///
/// This is the deliberately untrusted broad phase shared by live input sources.
/// It reads replicated snapshots outside the canonical step and grants no state
/// change: [`visibility::verify_claims`] repeats the integer predicate against a
/// recorded neighbour frame before the rules apply either body's force.
#[must_use]
pub fn collision_candidate<'a>(
    entity: PersistId,
    own: &RegolithState,
    neighbors: impl IntoIterator<Item = (PersistId, &'a RegolithState)>,
) -> Option<PersistId> {
    visibility::broad_phase_collision_candidate(entity, own, neighbors)
}

impl Regolith {
    /// Honest rules.
    #[must_use]
    pub const fn honest() -> Self {
        Self { tamper: None }
    }
    /// A modified build which still claims the honest identity.
    #[must_use]
    pub const fn cheating(tamper: Tamper) -> Self {
        Self {
            tamper: Some(tamper),
        }
    }
    const fn movement_cap(self, base: i64) -> i64 {
        match self.tamper {
            Some(Tamper::SpeedMultiplier) => base * 3 / 2,
            _ => base,
        }
    }
    const fn damage(self, roll: i32) -> i32 {
        match self.tamper {
            Some(Tamper::DamageInflation) => roll * 2,
            _ => roll,
        }
    }
    const fn honours_cooldown(self) -> bool {
        !matches!(self.tamper, Some(Tamper::NoCooldown))
    }
}

impl Ruleset for Regolith {
    const OVERFLOW_IS_CANONICAL: bool = true;
    type CoreState = RegolithState;
    type CoreInput = Order;
    type CoreEvent = Outcome;
    fn id(&self) -> RulesetId {
        REGOLITH_RULESET
    }
    fn max_neighbor_reads(&self) -> usize {
        MAX_NEIGHBOR_READS
    }
    fn max_neighbor_staleness_ticks(&self) -> u64 {
        MAX_NEIGHBOR_STALENESS_TICKS
    }
    fn invariants(&self) -> &[Invariant<RegolithState>] {
        invariants::INVARIANTS
    }
    /// Run the declared schedule.
    ///
    /// The tick's shape is [`REGOLITH_SCHEDULE`], not this function. What used
    /// to live here — a four-way `match` on the state enum, a per-kind
    /// delegation, and three tacked-on claim fix-ups — is now a `const` table
    /// of named systems that the composition manifest declares and the
    /// schedule digest covers (D43 clause (g)).
    fn step(
        &self,
        view: &mut StateView<'_, RegolithState>,
        inputs: &OrderedInputs<'_, Order>,
        rng: &mut TickRng,
    ) -> StepOutput<Outcome> {
        orrery_core::run_schedule(self, view, inputs, rng)
    }
    fn materialize(&self, event: &Outcome, out: &mut Vec<EntityMaterialization<RegolithState>>) {
        match event {
            Outcome::Split {
                generation,
                children,
                ..
            } => {
                for child in children {
                    let mut rock = Rock::spawned(
                        child.tier,
                        generation.saturating_add(1),
                        child.pos,
                        child.vel,
                    );
                    rock.bloom = child.bloom;
                    rock.born_in_bloom = child.bloom.is_some();
                    out.push(EntityMaterialization::new(
                        child.id,
                        RegolithState::Rock(rock),
                    ));
                }
            }
            Outcome::SpawnPickup {
                id,
                pos,
                kind,
                expires_at,
            } => out.push(EntityMaterialization::new(
                *id,
                RegolithState::Pickup(Pickup::spawned(*pos, *kind, *expires_at)),
            )),
            Outcome::BloomSeeded {
                director,
                bloom_index,
                rocks,
                ..
            } => {
                for rock in rocks.iter() {
                    out.push(EntityMaterialization::new(
                        rock.id,
                        RegolithState::Rock(Rock::spawned_in_bloom(
                            rock.tier,
                            rock.pos,
                            rock.vel,
                            *director,
                            *bloom_index,
                        )),
                    ));
                }
            }
            _ => {}
        }
    }
}

// ── the canonical schedule ──────────────────────────────────────────────

/// Everything one entity's systems share for the length of one tick.
///
/// This is the whole of what a Regolith rule may carry between systems, and
/// it is deliberately small. The 440-line `step_craft` it replaces opened by
/// destructuring **twenty-eight** fields of `Craft` into `let mut` locals and
/// closed by naming twenty-five of them again in a struct literal with a
/// `..own` fallthrough — a shape in which forgetting a field silently retains
/// last tick's value and nothing complains. Systems mutate `&mut Craft` in
/// place, so all twenty-eight are gone; what is left here is exactly the state
/// that genuinely cannot live in the component:
///
/// * the **unquantized kinematic window** (VC-7), because position and
///   velocity must survive four systems in metres before they rejoin the
///   lattice, and
/// * three **tick-start premises** — was this craft alive, did it respawn,
///   what did a lock target answer — that later systems must read as of the
///   start of the tick rather than as of now.
///
/// It is reset to [`Default`] at the top of every entity's tick, never carried
/// between entities or between ticks. Carrying it would be state outside the
/// closed input set.
#[derive(Debug, Default)]
pub struct RegolithLocals {
    claims: visibility::VerifiedClaims,
    craft: CraftScratch,
    rock: RockScratch,
}

/// A craft's tick-scoped scratch. See [`RegolithLocals`].
#[derive(Debug, Default)]
pub(crate) struct CraftScratch {
    /// Position in metres, unquantized, between load and store.
    pub(crate) pos_m: [f64; 3],
    /// Velocity in metres per second, unquantized, between load and store.
    pub(crate) vel_mps: [f64; 3],
    /// Hull was positive when the tick began.
    pub(crate) was_alive: bool,
    /// The wreck countdown completed this tick.
    pub(crate) respawned: bool,
    /// A target's answer to a lock request, settled after the sealed inputs.
    pub(crate) lock_reply: Option<(PersistId, Option<LockClass>)>,
}

/// A rock's tick-scoped scratch. See [`RegolithLocals`].
#[derive(Debug, Default)]
pub(crate) struct RockScratch {
    /// Hull was positive when the tick began.
    pub(crate) was_alive: bool,
    /// Who landed the shot that took the hull to zero this tick.
    pub(crate) killer: Option<PersistId>,
}

/// The context every Regolith system receives beside its own component.
pub(crate) type Cx<'a> = StepCtx<'a, Regolith, RegolithLocals>;

/// Read the recorded neighbour frames this tick's claims need.
///
/// The single audited read site, unchanged from what
/// `scripts/core-gates.sh` clause 5 already names. It is an
/// [`orrery_core::Observation`] rather than a [`orrery_core::System`] because
/// observations are the only system shape that receives a [`StateView`] —
/// which is what makes "a rule cannot read a neighbour" a fact about the
/// signature rather than a fact about a text scan.
fn observe_claims(
    view: &mut StateView<'_, RegolithState>,
    inputs: &OrderedInputs<'_, Order>,
    locals: &mut RegolithLocals,
) {
    locals.claims = visibility::verify_claims(view, inputs);
}

/// Record the cover claim's verdict in the locking craft's own state.
fn apply_cover_claim(craft: &mut state::Craft, cx: &mut Cx<'_>) {
    if let Some(Outcome::LockVisibility { occluded, .. }) = &cx.locals.claims.visibility {
        craft.last_cover_occluded = *occluded;
    }
}

/// Carry an overflow seen inside the audited predicate into own state.
fn propagate_claim_overflow(state: &mut RegolithState, cx: &mut Cx<'_>) {
    if !cx.locals.claims.arithmetic_overflowed {
        return;
    }
    match state {
        RegolithState::Craft(craft) => craft.arithmetic_overflowed = true,
        RegolithState::Rock(rock) => rock.arithmetic_overflowed = true,
        RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => {}
    }
}

/// Emit the visibility outcome last, after every module event.
fn emit_visibility_outcome(_state: &mut RegolithState, cx: &mut Cx<'_>) {
    if let Some(outcome) = cx.locals.claims.visibility.take() {
        cx.emit(outcome);
    }
}

/// The audited observation stage.
const OBSERVE: &[Observation<Regolith, RegolithLocals>] =
    &[observation!("verify-claims", observe_claims)];

/// The stage that folds verified claims back into own state.
const CLAIMS_APPLY: &[System<Regolith, RegolithLocals>] = &[
    craft_system!("craft-apply-cover-claim", apply_cover_claim),
    state_system!("propagate-claim-overflow", propagate_claim_overflow),
    state_system!("emit-visibility-outcome", emit_visibility_outcome),
];

/// Regolith's canonical tick, as data.
///
/// This table **is** the tick. Reading it top to bottom is the whole answer to
/// "what happens in a Regolith tick, and in what order" — a question that
/// previously required reading 440 lines of `step_craft` and inferring the
/// order from statement sequence. Reordering two entries here is a canonical
/// change, and unlike a statement swap inside one function it moves the
/// schedule digest, which is exactly what D43 clause (g) asks a digest to
/// catch: topology drift that state goldens cannot see.
pub static REGOLITH_SCHEDULE: Schedule<Regolith, RegolithLocals> = Schedule {
    observe_stage: StageName(STAGE_OBSERVE.0),
    observe: OBSERVE,
    stages: &[
        Stage {
            name: StageName(STAGE_CRAFT_CONTROL.0),
            systems: craft::CONTROL,
        },
        Stage {
            name: StageName(STAGE_CRAFT_MOTION.0),
            systems: craft::MOTION,
        },
        Stage {
            name: StageName(STAGE_WORLD_RESOLUTION.0),
            systems: world::RESOLUTION,
        },
        Stage {
            name: StageName(STAGE_WORLD_LIFECYCLE.0),
            systems: world::LIFECYCLE,
        },
        Stage {
            name: StageName(STAGE_CLAIMS_APPLY.0),
            systems: CLAIMS_APPLY,
        },
    ],
};

impl Scheduled for Regolith {
    type Locals = RegolithLocals;
    fn schedule(&self) -> &'static Schedule<Self, RegolithLocals> {
        &REGOLITH_SCHEDULE
    }
}

/// The audited neighbour-reading stage.
pub const STAGE_OBSERVE: ScheduleStageId = ScheduleStageId("observe");
/// Craft cooldowns, locks and the sealed-input loop.
pub const STAGE_CRAFT_CONTROL: ScheduleStageId = ScheduleStageId("craft-control");
/// The craft's unquantized kinematic window and its close.
pub const STAGE_CRAFT_MOTION: ScheduleStageId = ScheduleStageId("craft-motion");
/// Rock, pickup and director rules driven by sealed inputs.
pub const STAGE_WORLD_RESOLUTION: ScheduleStageId = ScheduleStageId("world-resolution");
/// Rock drift and the bloom cadence.
pub const STAGE_WORLD_LIFECYCLE: ScheduleStageId = ScheduleStageId("world-lifecycle");
/// Folding verified claims back into own state.
pub const STAGE_CLAIMS_APPLY: ScheduleStageId = ScheduleStageId("claims-apply");

/// D43 clause (g)'s pinned schedule digest.
///
/// Moved at v25 (#955): `craft-apply-tether` joined the craft-motion stage
/// between `craft-apply-drag` and `craft-integrate`, with the ordering edges
/// that pin it there.
///
/// Recomputed by `schedule_tests::the_schedule_digest_is_pinned`. Moving a
/// system, renaming one, or changing an ordering edge moves this value, and
/// that is the point: state goldens hash states, not graphs, so a reorder that
/// happens not to change any pinned chain is otherwise invisible.
pub const REGOLITH_SCHEDULE_DIGEST: [u8; 32] = [
    0xc0, 0x73, 0x00, 0x01, 0xa3, 0xe5, 0xda, 0x56, 0xe4, 0xe2, 0x48, 0xff, 0xba, 0x9b, 0x78, 0xd6,
    0xb4, 0xad, 0xaf, 0x79, 0x9d, 0x19, 0x3a, 0xa4, 0xc3, 0xc1, 0x34, 0xe8, 0xe3, 0x86, 0x16, 0x22,
];

/// The declared schedule topology D43 clause (g) digests.
///
/// This is the manifest's statement of what
/// [`REGOLITH_SCHEDULE`] runs. It is written out rather than derived because
/// `CanonicalSchedule` is a `const` of `&'static` slices and the runnable
/// table holds function pointers; deriving one from the other in a `const`
/// context is not expressible today. The two are held together by
/// [`schedule_tests::the_declared_schedule_matches_the_table_that_runs`],
/// which fails on any drift in either direction — the same
/// declared-plus-asserted shape `REGOLITH_COMPONENT_SCHEMAS` already uses
/// against the reviewed registry.
///
/// **On `ambiguities: &[]`.** D43 clause (c)(1) requires composition to reject
/// an ambiguous schedule. Regolith declares none because it can have none:
/// [`REGOLITH_SCHEDULE`] is a list, so every pair of systems is already totally
/// ordered, and [`ExecutorPolicy::SingleThreaded`] runs them in exactly that
/// order. Ambiguity is a property of a schedule expressed as constraints to be
/// solved; this one is expressed as the solution. The rejector itself is
/// proven awake in both directions by `orrery_compose`'s own tests, which
/// initialize the real manifest `Ok` and an `ambiguities`-bearing fixture
/// `Err`.
///
/// **On `ordering_edges`, honestly.** These are the load-bearing edges: the
/// ones where swapping two systems changes canonical output. D43 clause (c)(1)
/// asks for an explicit edge on *every* pair with conflicting data access,
/// derived mechanically; that derivation needs per-system data-access
/// declarations, which this design does not yet carry. What is here is a
/// hand-written subset, checked against the runnable order rather than
/// asserted.
pub const REGOLITH_CANONICAL_SCHEDULE: CanonicalSchedule = CanonicalSchedule {
    stages: REGOLITH_SCHEDULE_STAGES,
    ordering_edges: REGOLITH_SCHEDULE_EDGES,
    ambiguities: &[],
    ambiguity_detection: AmbiguityDetection::Error,
    executor_policy: ExecutorPolicy::SingleThreaded,
};

/// The declared stage table. See [`REGOLITH_CANONICAL_SCHEDULE`].
pub const REGOLITH_SCHEDULE_STAGES: &[ScheduleStageManifest] = &[
    ScheduleStageManifest {
        id: STAGE_OBSERVE,
        systems: &[SystemId("verify-claims")],
    },
    ScheduleStageManifest {
        id: STAGE_CRAFT_CONTROL,
        systems: &[
            SystemId("craft-tick-cooldowns"),
            SystemId("craft-decay-lock"),
            SystemId("craft-load-kinematics"),
            SystemId("craft-apply-orders"),
            SystemId("craft-resolve-lock-reply"),
        ],
    },
    ScheduleStageManifest {
        id: STAGE_CRAFT_MOTION,
        systems: &[
            SystemId("craft-clamp-speed"),
            SystemId("craft-apply-drag"),
            SystemId("craft-apply-tether"),
            SystemId("craft-integrate"),
            SystemId("craft-respawn"),
            SystemId("craft-store-kinematics"),
            SystemId("craft-advance-trail"),
        ],
    },
    ScheduleStageManifest {
        id: STAGE_WORLD_RESOLUTION,
        systems: &[
            SystemId("rock-load"),
            SystemId("rock-resolve-orders"),
            SystemId("rock-resolve-destruction"),
            SystemId("rock-refuse-when-dead"),
            SystemId("pickup-expire"),
            SystemId("pickup-contest"),
            SystemId("bloom-apply-population"),
        ],
    },
    ScheduleStageManifest {
        id: STAGE_WORLD_LIFECYCLE,
        systems: &[
            SystemId("rock-drift"),
            SystemId("bloom-advance-clock"),
            SystemId("bloom-expire-site"),
            SystemId("bloom-seed"),
        ],
    },
    ScheduleStageManifest {
        id: STAGE_CLAIMS_APPLY,
        systems: &[
            SystemId("craft-apply-cover-claim"),
            SystemId("propagate-claim-overflow"),
            SystemId("emit-visibility-outcome"),
        ],
    },
];

/// The declared ordering edges. See [`REGOLITH_CANONICAL_SCHEDULE`].
pub const REGOLITH_SCHEDULE_EDGES: &[ScheduleOrderingEdge] = &[
    // A collision claim must be verified against a recorded frame before the
    // sealed-input loop is allowed to apply either body's force.
    ScheduleOrderingEdge {
        before: SystemId("verify-claims"),
        after: SystemId("craft-apply-orders"),
    },
    // The unquantized window opens before anything writes into it and closes
    // after the last writer. This pair is VC-7's tick boundary, stated.
    ScheduleOrderingEdge {
        before: SystemId("craft-load-kinematics"),
        after: SystemId("craft-apply-orders"),
    },
    ScheduleOrderingEdge {
        before: SystemId("craft-apply-orders"),
        after: SystemId("craft-resolve-lock-reply"),
    },
    ScheduleOrderingEdge {
        before: SystemId("craft-clamp-speed"),
        after: SystemId("craft-apply-drag"),
    },
    ScheduleOrderingEdge {
        before: SystemId("craft-apply-drag"),
        after: SystemId("craft-apply-tether"),
    },
    // The tether reads the position it is restraining, so it must run while
    // that position is still this tick's *input* — before the integrator
    // moves the craft using the velocity the tether is there to damp.
    ScheduleOrderingEdge {
        before: SystemId("craft-apply-tether"),
        after: SystemId("craft-integrate"),
    },
    ScheduleOrderingEdge {
        before: SystemId("craft-integrate"),
        after: SystemId("craft-store-kinematics"),
    },
    ScheduleOrderingEdge {
        before: SystemId("craft-respawn"),
        after: SystemId("craft-store-kinematics"),
    },
    // The trail samples the *stored* position, so a respawn cannot draw one
    // enormous segment from the wreck to the new spawn point.
    ScheduleOrderingEdge {
        before: SystemId("craft-store-kinematics"),
        after: SystemId("craft-advance-trail"),
    },
    // A rock's tick-start hull is the premise of both branches below it.
    ScheduleOrderingEdge {
        before: SystemId("rock-load"),
        after: SystemId("rock-resolve-orders"),
    },
    ScheduleOrderingEdge {
        before: SystemId("rock-load"),
        after: SystemId("rock-refuse-when-dead"),
    },
    ScheduleOrderingEdge {
        before: SystemId("rock-resolve-orders"),
        after: SystemId("rock-resolve-destruction"),
    },
    // A rock destroyed this tick does not drift.
    ScheduleOrderingEdge {
        before: SystemId("rock-resolve-destruction"),
        after: SystemId("rock-drift"),
    },
    ScheduleOrderingEdge {
        before: SystemId("pickup-expire"),
        after: SystemId("pickup-contest"),
    },
    ScheduleOrderingEdge {
        before: SystemId("bloom-apply-population"),
        after: SystemId("bloom-advance-clock"),
    },
    ScheduleOrderingEdge {
        before: SystemId("bloom-advance-clock"),
        after: SystemId("bloom-expire-site"),
    },
    ScheduleOrderingEdge {
        before: SystemId("bloom-expire-site"),
        after: SystemId("bloom-seed"),
    },
    // The cover verdict is read into own state before it is consumed as an
    // emitted outcome.
    ScheduleOrderingEdge {
        before: SystemId("craft-apply-cover-claim"),
        after: SystemId("emit-visibility-outcome"),
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectileResolution {
    InFlight(u16),
    Hit,
    Miss,
    OutOfArc,
    Break(LockBreakReason),
}

#[allow(clippy::too_many_arguments)]
fn projectile_resolution(
    target_pos: QPos,
    target_vel: QVel,
    target_radius_mm: i64,
    target_alive: bool,
    attacker_pos: QPos,
    attacker_vel: QVel,
    attacker_yaw_urad: i32,
    attacker_archetype: Archetype,
    weapon_kind: weapon::WeaponKind,
    flight_ticks: Option<u16>,
    rng: &mut TickRng,
) -> ProjectileResolution {
    if !target_alive {
        return ProjectileResolution::Break(LockBreakReason::TargetDestroyed);
    }
    // The initial delivery decides the firing-time fact. `Some` is a
    // target-authored continuation of that accepted projectile; rechecking
    // against the target's later position turns movement during flight into
    // a retroactive OutOfArc refusal before the hit roll.
    if flight_ticks.is_none()
        && !in_firing_arc(
            attacker_archetype,
            attacker_yaw_urad,
            attacker_pos,
            target_pos,
        )
    {
        return ProjectileResolution::OutOfArc;
    }
    let weapon = weapon_kind.weapon();
    // Range deliberately stays live for target-authored continuations. The
    // target's current position is compared with the attacker's firing-time
    // origin so that escaping beyond weapon reach breaks the lock before the
    // projectile resolves; that mixed-time frame is the outrunning mechanic.
    let range_sq = nonnegative_distance_squared(target_pos, attacker_pos);
    let reach = weapon
        .optimal_mm
        .saturating_add(weapon.falloff_mm)
        .saturating_add(target_radius_mm);
    if range_sq > square_i64(reach) {
        return ProjectileResolution::Break(LockBreakReason::RangeExceeded);
    }

    match flight_ticks {
        None => {
            let ticks = projectile_flight_ticks(range_sq, weapon.projectile_speed_mms);
            if ticks > 1 {
                return ProjectileResolution::InFlight(ticks - 1);
            }
        }
        Some(ticks) if ticks > 1 => return ProjectileResolution::InFlight(ticks - 1),
        Some(_) => {}
    }

    let chance = hit_chance_ppm(
        target_pos,
        target_vel,
        target_radius_mm,
        attacker_pos,
        attacker_vel,
        weapon,
    );
    if uniform_below(rng, CHANCE_SCALE as u32) < chance {
        ProjectileResolution::Hit
    } else {
        ProjectileResolution::Miss
    }
}

/// Whether `target_pos` lies in one of the shooter's chassis firing arcs.
///
/// The relative bearing is obtained with integer CORDIC vectoring. That keeps
/// this persistent-value decision bit-exact without a platform float result.
#[must_use]
pub fn in_firing_arc(
    attacker_archetype: Archetype,
    attacker_yaw_urad: i32,
    attacker_pos: QPos,
    target_pos: QPos,
) -> bool {
    firing_arc_measurement(
        attacker_archetype,
        attacker_yaw_urad,
        attacker_pos,
        target_pos,
    )
    .inside
}

/// The exact integer geometry used by firing-arc adjudication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiringArcMeasurement {
    /// Target bearing in world space, or `None` for coincident positions.
    pub world_bearing_urad: Option<i32>,
    /// Target bearing relative to the attacker, or `None` when coincident.
    pub relative_urad: Option<i32>,
    /// Whether at least one chassis arc accepts the relative bearing.
    pub inside: bool,
}

/// Measures the exact geometry used by [`in_firing_arc`].
#[must_use]
pub fn firing_arc_measurement(
    attacker_archetype: Archetype,
    attacker_yaw_urad: i32,
    attacker_pos: QPos,
    target_pos: QPos,
) -> FiringArcMeasurement {
    let dx = i128::from(target_pos.x) - i128::from(attacker_pos.x);
    let dz = i128::from(target_pos.z) - i128::from(attacker_pos.z);
    let Some(world_bearing) = integer_bearing_urad(dx, dz) else {
        return FiringArcMeasurement {
            world_bearing_urad: None,
            relative_urad: None,
            inside: true,
        };
    };
    let relative = world_bearing
        .saturating_sub(attacker_yaw_urad)
        .rem_euclid(TAU_URAD);
    let inside = attacker_archetype
        .firing_arcs()
        .iter()
        .any(|arc| arc.contains(relative));
    FiringArcMeasurement {
        world_bearing_urad: Some(world_bearing),
        relative_urad: Some(relative),
        inside,
    }
}

fn integer_bearing_urad(mut x: i128, mut y: i128) -> Option<i32> {
    const ATAN_URAD: [i32; 21] = [
        785_398, 463_648, 244_979, 124_355, 62_419, 31_240, 15_624, 7_812, 3_906, 1_953, 977, 488,
        244, 122, 61, 31, 15, 8, 4, 2, 1,
    ];
    if x == 0 && y == 0 {
        return None;
    }
    let mut angle = 0_i32;
    if x < 0 {
        x = -x;
        y = -y;
        angle = if y <= 0 {
            TAU_URAD / 2
        } else {
            -(TAU_URAD / 2)
        };
    }
    x <<= 32;
    y <<= 32;
    for (shift, turn) in ATAN_URAD.into_iter().enumerate() {
        if y == 0 {
            break;
        }
        let (old_x, old_y) = (x, y);
        if old_y > 0 {
            x = old_x.saturating_add(old_y >> shift);
            y = old_y.saturating_sub(old_x >> shift);
            angle = angle.saturating_add(turn);
        } else {
            x = old_x.saturating_sub(old_y >> shift);
            y = old_y.saturating_add(old_x >> shift);
            angle = angle.saturating_sub(turn);
        }
    }
    Some(angle)
}

fn hit_chance_ppm(
    target_pos: QPos,
    target_vel: QVel,
    target_radius_mm: i64,
    attacker_pos: QPos,
    attacker_vel: QVel,
    weapon: weapon::Weapon,
) -> u32 {
    let rx = i128::from(target_pos.x).saturating_sub(i128::from(attacker_pos.x));
    let ry = i128::from(target_pos.y).saturating_sub(i128::from(attacker_pos.y));
    let rz = i128::from(target_pos.z).saturating_sub(i128::from(attacker_pos.z));
    let vx = i128::from(target_vel.x).saturating_sub(i128::from(attacker_vel.x));
    let vy = i128::from(target_vel.y).saturating_sub(i128::from(attacker_vel.y));
    let vz = i128::from(target_vel.z).saturating_sub(i128::from(attacker_vel.z));
    let range_sq = sum_squares([rx, ry, rz]);
    let range_mm = integer_sqrt(range_sq);

    let cross = [
        ry.saturating_mul(vz).saturating_sub(rz.saturating_mul(vy)),
        rz.saturating_mul(vx).saturating_sub(rx.saturating_mul(vz)),
        rx.saturating_mul(vy).saturating_sub(ry.saturating_mul(vx)),
    ];
    let cross_magnitude = integer_sqrt(sum_squares(cross));
    let angular_urad_per_sec = cross_magnitude
        .saturating_mul(1_000_000)
        .checked_div(range_sq)
        .unwrap_or(0);
    let tracking_denominator =
        u128::from(weapon.tracking_urad_per_sec).saturating_mul(target_radius_mm.max(1) as u128);
    let tracking_ratio = angular_urad_per_sec
        .saturating_mul(REFERENCE_SIGNATURE_RADIUS_MM)
        .saturating_mul(CHANCE_SCALE)
        / tracking_denominator.max(1);

    let optimal = weapon.optimal_mm.max(0) as u128;
    let range_ratio = range_mm
        .saturating_sub(optimal)
        .saturating_mul(CHANCE_SCALE)
        / (weapon.falloff_mm.max(1) as u128);
    let penalty = tracking_ratio
        .saturating_mul(tracking_ratio)
        .saturating_add(range_ratio.saturating_mul(range_ratio));
    let denominator = CHANCE_SCALE
        .saturating_mul(CHANCE_SCALE)
        .saturating_add(penalty);
    let chance = CHANCE_SCALE
        .saturating_mul(CHANCE_SCALE)
        .saturating_mul(CHANCE_SCALE)
        / denominator.max(1);
    u32::try_from(chance.min(CHANCE_SCALE)).unwrap_or(CHANCE_SCALE as u32)
}

/// Return the ruleset's flight duration for a squared range and projectile speed.
///
/// Presentation may use this to show the timing the ruleset will apply without
/// predicting the eventual shot result.
#[must_use]
pub fn projectile_flight_ticks(range_sq: u128, projectile_speed_mms: i64) -> u16 {
    let distance = integer_sqrt(range_sq);
    let numerator = distance.saturating_mul(u128::from(TICK_HZ));
    let speed = projectile_speed_mms.max(1) as u128;
    let ticks = numerator.saturating_add(speed - 1) / speed;
    u16::try_from(ticks.max(1)).unwrap_or(u16::MAX)
}

fn nonnegative_distance_squared(a: QPos, b: QPos) -> u128 {
    sum_squares([
        i128::from(a.x).saturating_sub(i128::from(b.x)),
        i128::from(a.y).saturating_sub(i128::from(b.y)),
        i128::from(a.z).saturating_sub(i128::from(b.z)),
    ])
}

/// Straight-line separation in millimetres, rounded down exactly as projectile flight time is.
#[must_use]
pub fn distance_mm(a: QPos, b: QPos) -> u128 {
    integer_sqrt(nonnegative_distance_squared(a, b))
}

fn square_i64(value: i64) -> u128 {
    let value = value.max(0) as u128;
    value.saturating_mul(value)
}

fn sum_squares(values: [i128; 3]) -> u128 {
    values.into_iter().fold(0, |sum, value| {
        let magnitude = value.unsigned_abs();
        sum.saturating_add(magnitude.saturating_mul(magnitude))
    })
}

fn velocity_within_limit(velocity: QVel, max_speed_mms: i64) -> bool {
    let speed_sq = [velocity.x, velocity.y, velocity.z]
        .into_iter()
        .map(|value| i128::from(value).unsigned_abs().pow(2))
        .sum::<u128>();
    speed_sq <= square_i64(max_speed_mms)
}

pub(crate) fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut estimate = 1u128 << (value.ilog2() / 2 + 1);
    loop {
        let next = (estimate + value / estimate) / 2;
        if next >= estimate {
            return estimate;
        }
        estimate = next;
    }
}

fn uniform_below(rng: &mut TickRng, bound: u32) -> u32 {
    let limit = u32::MAX - u32::MAX % bound;
    loop {
        let draw = rng.next_u32();
        if draw < limit {
            return draw % bound;
        }
    }
}

fn uniform_percent(rng: &mut TickRng) -> u32 {
    rng.next_u32() % 100
}

fn split_children(
    parent: PersistId,
    rock: &Rock,
    tier: RockTier,
    rng: &mut TickRng,
) -> [ChildSpec; 2] {
    let jitter0 = uniform_jitter(rng);
    let jitter1 = uniform_jitter(rng);
    [
        child_spec(parent, rock, tier, 0, i64::from(jitter0)),
        child_spec(parent, rock, tier, 1, -i64::from(jitter1)),
    ]
}
fn uniform_jitter(rng: &mut TickRng) -> u32 {
    let width = JITTER_MAX_URAD - JITTER_MIN_URAD + 1;
    let limit = u32::MAX - u32::MAX % width;
    loop {
        let draw = rng.next_u32();
        if draw < limit {
            return JITTER_MIN_URAD + draw % width;
        }
    }
}
fn child_spec(
    parent: PersistId,
    rock: &Rock,
    tier: RockTier,
    slot: u8,
    signed_angle_urad: i64,
) -> ChildSpec {
    let angle = signed_angle_urad as f64 / 1_000_000.0;
    let (vx, vy, vz) = rock.vel.to_metres_per_sec();
    let scale = 1.4_f64;
    let (mut x, mut z) = (
        (vx * libm::cos(angle) - vz * libm::sin(angle)) * scale,
        (vx * libm::sin(angle) + vz * libm::cos(angle)) * scale,
    );
    let mut y = vy * scale;
    let speed = libm::sqrt(x * x + y * y + z * z);
    let ceiling = tier.limits().max_speed_mms as f64 / 1_000.0;
    if speed > ceiling && speed > 0.0 {
        let cap = ceiling / speed;
        x *= cap;
        y *= cap;
        z *= cap;
    }
    let vel = QVel::from_metres_per_sec(x, y, z);
    let speed =
        libm::sqrt((vel.x as f64).powi(2) + (vel.y as f64).powi(2) + (vel.z as f64).powi(2));
    let radius = rock.tier.limits().radius_mm as f64;
    let pos = if speed > 0.0 {
        QPos {
            x: rock
                .pos
                .x
                .saturating_add((vel.x as f64 * radius / speed) as i64),
            y: rock
                .pos
                .y
                .saturating_add((vel.y as f64 * radius / speed) as i64),
            z: rock
                .pos
                .z
                .saturating_add((vel.z as f64 * radius / speed) as i64),
        }
    } else {
        rock.pos
    };
    ChildSpec {
        id: child_id(parent, rock.generation, slot),
        tier,
        pos,
        vel,
        bloom: rock.bloom,
    }
}
fn bloom_spec(
    director: PersistId,
    bloom_index: u32,
    slot: usize,
    site_pos: QPos,
    rng: &mut TickRng,
) -> ChildSpec {
    let tier = match slot {
        0..=1 => RockTier::Large,
        2..=4 => RockTier::Medium,
        _ => RockTier::Small,
    };
    let slot = u8::try_from(slot).expect("ten bloom slots fit in u8");
    ChildSpec {
        id: bloom_rock_id(director, bloom_index, slot),
        tier,
        pos: site_pos,
        vel: bloom_velocity(tier, rng),
        bloom: Some(BloomMembership {
            director,
            bloom_index,
        }),
    }
}
fn bloom_velocity(tier: RockTier, rng: &mut TickRng) -> QVel {
    // Eight planar directions in fixed-point /1024. The diagonal coefficient
    // is round(1024 / sqrt(2)); no float enters the rules predicate or state.
    const DIRECTIONS: [(i64, i64); 8] = [
        (1_024, 0),
        (724, 724),
        (0, 1_024),
        (-724, 724),
        (-1_024, 0),
        (-724, -724),
        (0, -1_024),
        (724, -724),
    ];
    let limits = tier.limits();
    let floor = limits.max_speed_mms / 4;
    let width = u32::try_from(limits.max_speed_mms / 4).unwrap_or(1).max(1);
    let speed = floor.saturating_add(i64::from(uniform_below(rng, width)));
    let direction = DIRECTIONS[uniform_below(rng, DIRECTIONS.len() as u32) as usize];
    QVel {
        x: speed.saturating_mul(direction.0) / 1_024,
        y: 0,
        z: speed.saturating_mul(direction.1) / 1_024,
    }
}

fn flagged_add(left: i64, right: i64, overflowed: &mut bool) -> i64 {
    left.checked_add(right).unwrap_or_else(|| {
        *overflowed = true;
        left.saturating_add(right)
    })
}

fn flagged_neg(value: i64, overflowed: &mut bool) -> i64 {
    value.checked_neg().unwrap_or_else(|| {
        *overflowed = true;
        value.saturating_neg()
    })
}
fn bloom_rock_id(director: PersistId, bloom_index: u32, slot: u8) -> PersistId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regolith-bloom");
    hasher.update(&director.0.to_le_bytes());
    hasher.update(&bloom_index.to_le_bytes());
    hasher.update(&[slot]);
    PersistId::new(u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("digest prefix"),
    ))
}
fn draw_bloom_site(rng: &mut TickRng) -> QPos {
    let span = u64::try_from(BLOOM_CENTRAL_RADIUS_MM.saturating_mul(2).saturating_add(1))
        .expect("positive central-region span");
    let coordinate = |draw: u64| i64::try_from(draw % span).unwrap_or(0) - BLOOM_CENTRAL_RADIUS_MM;
    QPos {
        x: coordinate(rng.next_u64()),
        y: 0,
        z: coordinate(rng.next_u64()),
    }
}
fn child_id(parent: PersistId, generation: u32, slot: u8) -> PersistId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regolith-rock");
    hasher.update(&parent.0.to_le_bytes());
    hasher.update(&generation.to_le_bytes());
    hasher.update(&[slot]);
    PersistId::new(u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("digest prefix"),
    ))
}
fn pickup_id(rock: PersistId) -> PersistId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regolith-pickup");
    hasher.update(&rock.0.to_le_bytes());
    PersistId::new(u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("digest prefix"),
    ))
}
const fn reach_sq(range_mm: i64) -> i128 {
    (range_mm as i128) * (range_mm as i128)
}

/// Whether a ship at `ship_pos` is inside the pickup reach this ruleset
/// adjudicates a grab with.
///
/// [`Regolith::step_pickup`] calls exactly this, so a skin that wants to know
/// whether a grab would be granted can ask the ruleset instead of re-deriving
/// a "near enough" of its own. That divergence — client and host disagreeing
/// about a threshold both think they read from the table — is the #499/#505
/// failure mode; there is one expression here and both sides call it.
#[must_use]
pub fn within_grab_reach(pickup_pos: QPos, ship_pos: QPos) -> bool {
    pickup_pos.distance_squared(ship_pos) <= reach_sq(GRAB_RADIUS_MM)
}

impl Game for Regolith {
    const META: GameMeta = GameMeta {
        name: "regolith",
        summary: "planar combat, deterministic bloom density and logged scoring",
        ruleset: REGOLITH_RULESET,
    };
    const COMPOSITION: CompatibilityManifest = REGOLITH_COMPOSITION;
    const GOLDEN_CHAINS: &'static [(&'static str, [u8; 32])] = &crate::golden::REGOLITH;
    fn honest() -> Self {
        Self::honest()
    }
    fn tampered(tamper: Tamper) -> Option<Self> {
        Some(Self::cheating(tamper))
    }
    fn spawn(&self, _entity: PersistId, slot: u64) -> RegolithState {
        let archetype = Archetype::for_slot(slot);
        let (pos, yaw) = spawn_pose(slot);
        RegolithState::Craft(Craft::spawned(archetype, pos, yaw))
    }
    fn spawn_world(&self, _entity: PersistId, slot: u64) -> Option<RegolithState> {
        Some(world_seed(slot))
    }
    fn honest_inputs(
        &self,
        entity: PersistId,
        slot: u64,
        tick: Tick,
        _peers: &[PersistId],
        rng: &mut TickRng,
        out: &mut Vec<Order>,
    ) {
        pilot::honest_orders(entity, slot, tick, rng, out);
    }
    fn deliver(&self, event: &Outcome) -> Option<(PersistId, Order)> {
        match event {
            Outcome::DamageDealt {
                attacker,
                target,
                amount,
                attacker_pos,
                attacker_vel,
                attacker_yaw_urad,
                attacker_archetype,
                attacker_weapon,
                flight_ticks,
            } => Some((
                *target,
                Order::Damage {
                    amount: *amount,
                    from: *attacker,
                    from_pos: *attacker_pos,
                    from_vel: *attacker_vel,
                    from_yaw_urad: *attacker_yaw_urad,
                    from_archetype: *attacker_archetype,
                    from_weapon: *attacker_weapon,
                    flight_ticks: *flight_ticks,
                },
            )),
            Outcome::GrabAttempted {
                pickup,
                ship,
                ship_pos,
            } => Some((
                *pickup,
                Order::GrabAttempt {
                    ship: *ship,
                    ship_pos: *ship_pos,
                },
            )),
            Outcome::Granted { ship, kind } => Some((*ship, Order::PickupGranted { kind: *kind })),
            Outcome::Denied { ship } => Some((*ship, Order::PickupDenied)),
            Outcome::Destroyed { by } => Some((*by, Order::KillCredit)),
            Outcome::RockDestroyed { by, points } => {
                Some((*by, Order::RockCredit { points: *points }))
            }
            Outcome::BloomPopulationChanged {
                director,
                bloom_index,
                delta,
            } => Some((
                *director,
                Order::BloomPopulationChanged {
                    bloom_index: *bloom_index,
                    delta: *delta,
                },
            )),
            Outcome::LockBroken {
                locker,
                target,
                reason,
            } => Some((
                *locker,
                Order::LockBroken {
                    target: *target,
                    reason: *reason,
                },
            )),
            Outcome::LockRequested { locker, target } => {
                Some((*target, Order::LockRequested { locker: *locker }))
            }
            Outcome::LockConfirmed {
                locker,
                target,
                class,
            } => Some((
                *locker,
                Order::LockConfirmed {
                    target: *target,
                    class: *class,
                },
            )),
            Outcome::LockRefused { locker, target } => {
                Some((*locker, Order::LockRefused { target: *target }))
            }
            Outcome::ShotResolved {
                attacker,
                target,
                result,
            } => Some((
                *attacker,
                Order::ShotResolved {
                    target: *target,
                    result: *result,
                },
            )),
            Outcome::ShotRefused { .. } => None,
            Outcome::LockVisibility {
                locker,
                target,
                occluded,
            } => Some((
                *locker,
                Order::LockVisibility {
                    target: *target,
                    occluded: *occluded,
                },
            )),
            Outcome::Collision {
                collider,
                target,
                target_velocity,
            } => Some((
                *target,
                Order::CollisionResolved {
                    from: *collider,
                    velocity: *target_velocity,
                },
            )),
            Outcome::Split { .. }
            | Outcome::SpawnPickup { .. }
            | Outcome::Expired { .. }
            | Outcome::BloomSeeded { .. } => None,
        }
    }
    fn trajectory(state: &RegolithState) -> (QPos, QVel) {
        match state {
            RegolithState::Craft(craft) => (craft.pos, craft.vel),
            RegolithState::Rock(rock) => (rock.pos, rock.vel),
            RegolithState::Pickup(pickup) => (pickup.pos, QVel::default()),
            RegolithState::BloomDirector(_) => (QPos::default(), QVel::default()),
        }
    }
}

/// One world-owned seed for a scenario slot: the island's bloom director at
/// slot 0, a pickup at slot 1, then rocks around the player spawn ring.
///
/// The director's `next_bloom_tick` is deliberately far below
/// [`BLOOM_CADENCE_TICKS`]: the stock director waits sixty seconds, which is
/// longer than any scenario in the corpus runs, so a stock seed would leave
/// the bloom, split, materialization and pickup paths unstepped and the
/// scenario would only look like it covered the module. Everything else is
/// stock, and every value is a pure function of `slot`.
fn world_seed(slot: u64) -> RegolithState {
    if slot == 0 {
        return RegolithState::BloomDirector(BloomDirector {
            clock_tick: 0,
            next_bloom_tick: 60,
            ..BloomDirector::spawned()
        });
    }
    if slot == 1 {
        // A pickup whose TTL runs out well inside the window, so the expiry
        // branch — and the `Expired` outcome it emits — is stepped rather
        // than merely reachable. The stock 30-second TTL outlives every
        // scenario in the corpus.
        return RegolithState::Pickup(Pickup {
            ttl_remaining: 300,
            ..Pickup::spawned(
                QPos::from_metres(SPAWN_RADIUS_MM / 1_000.0, 0.0, 0.0),
                weapon::WeaponKind::Volley,
                PICKUP_TTL_TICKS,
            )
        });
    }
    let rock_slot = slot - 2;
    let tier = match rock_slot % 3 {
        0 => RockTier::Large,
        1 => RockTier::Medium,
        _ => RockTier::Small,
    };
    // On the player spawn ring, angularly offset from every player pose, so
    // the rocks are inside weapon and collision range of an orbiting craft
    // without being spawned on top of one.
    let angle_urad =
        (rock_slot as i64).saturating_mul(GOLDEN_ANGLE_URAD) % i64::from(TAU_URAD) + 300_000;
    let angle = angle_urad as f64 / 1_000_000.0;
    let pos = QPos::from_metres(
        SPAWN_RADIUS_MM * libm::cos(angle) / 1_000.0,
        0.0,
        SPAWN_RADIUS_MM * libm::sin(angle) / 1_000.0,
    );
    let vel = QVel::from_metres_per_sec(libm::sin(angle) * 4.0, 0.0, -libm::cos(angle) * 4.0);
    RegolithState::Rock(Rock::spawned(tier, 0, pos, vel))
}

fn spawn_pose(slot: u64) -> (QPos, i32) {
    let angle_urad = (slot as i64).saturating_mul(GOLDEN_ANGLE_URAD) % i64::from(TAU_URAD);
    let angle = angle_urad as f64 / 1_000_000.0;
    let pos = QPos::from_metres(
        SPAWN_RADIUS_MM * libm::cos(angle) / 1_000.0,
        0.0,
        SPAWN_RADIUS_MM * libm::sin(angle) / 1_000.0,
    );
    let yaw = i32::try_from(angle_urad).unwrap_or(0) + TAU_URAD / 4;
    (pos, yaw.rem_euclid(TAU_URAD))
}

/// The campaign swarm's shared spawn pose for one slot.
///
/// Campaign participants must derive their initial canonical position from
/// the same function as the host's headless peers. A client using the compact
/// scenario ring instead starts kilometres outside the host crowd and cannot
/// put any target inside weapon range, even when its firing bearing is valid.
#[must_use]
pub fn campaign_spawn_pose(slot: usize, count: usize) -> (QPos, i32) {
    let share = slot as f64 / count.max(1) as f64;
    let radius_m = campaign_orbit_radius_m(slot, count);
    let arc = CAMPAIGN_CROWD_ARC_RAD * share;
    let pos = QPos::from_metres(libm::cos(arc) * radius_m, 0.0, libm::sin(arc) * radius_m);
    let yaw_urad = ((arc + core::f64::consts::FRAC_PI_2) * 1_000_000.0) as i32;
    (pos, yaw_urad)
}

/// The campaign orbit radius for one slot, in metres.
#[must_use]
pub fn campaign_orbit_radius_m(slot: usize, count: usize) -> f64 {
    let share = slot as f64 / count.max(1) as f64;
    CAMPAIGN_ORBIT_RADIUS_M * (1.0 + CAMPAIGN_RADIAL_SPREAD * (share - 0.5))
}

#[cfg(test)]
mod composition_tests {
    use super::archetype::Archetype;
    use super::state::{BloomDirector, Craft, Pickup, RegolithState, Rock, RockTier};
    use super::weapon::WeaponKind;
    use super::{REGOLITH_COMPOSITION, REGOLITH_MODULES};
    use orrery_compose::registry::regolith::COMPONENT_TYPE_IDS;
    use orrery_core::{QPos, QVel, Sectioned};

    #[test]
    fn assembled_regolith_manifest_validates() {
        assert_eq!(orrery_compose::validate(&REGOLITH_COMPOSITION), Ok(()));
    }

    /// The guard on #750's split: the manifest's schema table is the
    /// *derived* half, and this is what makes "derived" mean something.
    ///
    /// Without it, `REGOLITH_COMPOSITION.component_schemas` could name a
    /// component the reviewed ledger never allocated, or a `SchemaVersion`
    /// nobody reviewed, and every other test in the tree would stay green —
    /// which is exactly the state #750 found the empty table in. Both halves
    /// of D45 clause (a)'s pair are compared, in both directions, so neither
    /// a row the ledger does not carry nor a ledger row the manifest forgot
    /// can pass.
    #[test]
    fn the_manifest_schema_table_agrees_with_the_reviewed_registry() {
        let manifest: Vec<_> = REGOLITH_COMPOSITION
            .component_schemas
            .iter()
            .map(|schema| (schema.id.component, schema.id.version))
            .collect();
        let reviewed: Vec<_> = COMPONENT_TYPE_IDS
            .iter()
            .map(|entry| (entry.id, entry.schema_version))
            .collect();
        assert_eq!(
            manifest, reviewed,
            "REGOLITH_COMPOSITION.component_schemas must state exactly the \
             reviewed (ComponentTypeId, SchemaVersion) rows in \
             orrery_compose::registry::regolith::COMPONENT_TYPE_IDS"
        );
    }

    /// S7.4: the ECS migration frontier is the union of complete declared
    /// modules, not a hand-picked set of variants.
    ///
    /// `RegolithState::MIGRATED_SECTIONS` is what a decomposing host stores
    /// apart (`orrery_sim_host::ecs`). If it could name any subset of sections,
    /// "one module at a time" would be a claim in a PR body rather than a fact
    /// about the tree: a frontier cutting across two modules would still
    /// compile, still pass the differential, and still be described as module
    /// migration. Lane two has moved the second and final module, so this pins
    /// the frontier to the manifest's complete section set in both directions.
    #[test]
    fn the_migration_frontier_is_the_union_of_declared_modules() {
        let declared: Vec<&str> = REGOLITH_MODULES
            .iter()
            .rev()
            .flat_map(|module| module.state_sections.iter().map(|section| section.0))
            .collect();
        let frontier: Vec<&str> = RegolithState::MIGRATED_SECTIONS
            .iter()
            .map(|section| section.0)
            .collect();
        assert_eq!(
            frontier, declared,
            "the migrated section set must be exactly the complete declared \
             module section set, in migration order"
        );
    }

    /// Every section a value can report is one some module owns.
    ///
    /// The complement of the test above: the frontier being a module means
    /// nothing if `section()` can return a name no module declares, because
    /// then the un-migrated remainder is not "the other modules" but "whatever
    /// is left over".
    #[test]
    fn every_reported_section_is_owned_by_a_module() {
        let owned: Vec<&str> = REGOLITH_MODULES
            .iter()
            .flat_map(|module| module.state_sections.iter().map(|section| section.0))
            .collect();
        for state in [
            RegolithState::Craft(Craft::spawned(Archetype::Interceptor, QPos::default(), 0)),
            RegolithState::Rock(Rock::spawned(
                RockTier::Large,
                0,
                QPos::default(),
                QVel::default(),
            )),
            RegolithState::Pickup(Pickup::spawned(QPos::default(), WeaponKind::Stock, 0)),
            RegolithState::BloomDirector(BloomDirector::spawned()),
        ] {
            assert!(
                owned.contains(&state.section().0),
                "section {:?} is reported by a state value but owned by no module",
                state.section().0
            );
        }
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::{
        REGOLITH_CANONICAL_SCHEDULE, REGOLITH_COMPOSITION, REGOLITH_MODULES, REGOLITH_SCHEDULE,
        REGOLITH_SCHEDULE_DIGEST,
    };
    use orrery_compose::{schedule_digest, ScheduleStageId, SystemId};

    /// The manifest is a *declaration*; this is what makes it mean something.
    ///
    /// `CanonicalSchedule` is what ships to a peer and what the digest covers,
    /// but the thing that actually runs is `REGOLITH_SCHEDULE`'s table of
    /// function pointers. Nothing in the language holds them together, so a
    /// system inserted in the runnable table and forgotten in the manifest
    /// would leave every other test green while the digest asserted a topology
    /// that had not existed since the edit. Both directions are compared,
    /// stage by stage and system by system, in order.
    #[test]
    fn the_declared_schedule_matches_the_table_that_runs() {
        let running: Vec<(String, Vec<String>)> = REGOLITH_SCHEDULE
            .stages_with_systems()
            .into_iter()
            .map(|(stage, systems)| {
                (
                    stage.0.to_owned(),
                    systems.into_iter().map(|s| s.0.to_owned()).collect(),
                )
            })
            .collect();
        let declared: Vec<(String, Vec<String>)> = REGOLITH_CANONICAL_SCHEDULE
            .stages
            .iter()
            .map(|stage| {
                (
                    stage.id.0.to_owned(),
                    stage.systems.iter().map(|s| s.0.to_owned()).collect(),
                )
            })
            .collect();
        assert_eq!(
            running, declared,
            "REGOLITH_CANONICAL_SCHEDULE must state exactly the stages and \
             systems REGOLITH_SCHEDULE runs, in order"
        );
    }

    /// A name that appears twice makes an ordering edge ambiguous and the
    /// digest a statement about something that does not exist.
    #[test]
    fn every_system_name_is_unique() {
        assert_eq!(REGOLITH_SCHEDULE.duplicate_system_name(), None);
    }

    /// Every declared ordering edge is satisfied by the order that runs.
    ///
    /// D43 clause (c)(1) asks that conflicting systems carry explicit edges.
    /// The declared set is a hand-written subset — see
    /// `REGOLITH_CANONICAL_SCHEDULE` — but a declared edge that the runnable
    /// table violates is a manifest telling a peer something false, so each
    /// one is checked rather than trusted.
    #[test]
    fn every_declared_ordering_edge_holds_in_the_running_order() {
        for edge in REGOLITH_CANONICAL_SCHEDULE.ordering_edges {
            let before = REGOLITH_SCHEDULE
                .position_of(orrery_core::SystemName(edge.before.0))
                .unwrap_or_else(|| panic!("edge names unknown system {}", edge.before.0));
            let after = REGOLITH_SCHEDULE
                .position_of(orrery_core::SystemName(edge.after.0))
                .unwrap_or_else(|| panic!("edge names unknown system {}", edge.after.0));
            assert!(
                before < after,
                "declared edge {} -> {} is violated by the running order \
                 (positions {before} and {after})",
                edge.before.0,
                edge.after.0
            );
        }
    }

    /// Every stage a module declares exists, and every stage exists because
    /// some module declared it.
    ///
    /// `orrery_compose::validate` only checks the first direction. A stage in
    /// the schedule that no module owns is a rule with no home — which is the
    /// state both Regolith modules were in while `schedule_stages` was `&[]`.
    #[test]
    fn every_stage_is_owned_by_at_least_one_module() {
        let declared: Vec<ScheduleStageId> = REGOLITH_CANONICAL_SCHEDULE
            .stages
            .iter()
            .map(|stage| stage.id)
            .collect();
        for stage in &declared {
            assert!(
                REGOLITH_MODULES
                    .iter()
                    .any(|module| module.schedule_stages.contains(stage)),
                "stage {} is declared by no module",
                stage.0
            );
        }
        for module in REGOLITH_MODULES {
            for stage in module.schedule_stages {
                assert!(
                    declared.contains(stage),
                    "module {} names undeclared stage {}",
                    module.id.0,
                    stage.0
                );
            }
        }
    }

    /// D43 clause (g): the digest is pinned so an accidental system reorder
    /// fails CI the way a golden does.
    ///
    /// This is the assertion the clause asks for and could not previously
    /// have: with `stages: &[]` and `ordering_edges: &[]` the digest was a
    /// constant of the empty schedule, identical for every game and moved by
    /// nothing. It now moves if a system is added, removed, renamed,
    /// reordered, moved between stages, or if an ordering edge changes.
    #[test]
    fn the_schedule_digest_is_pinned() {
        assert_eq!(
            schedule_digest(&REGOLITH_COMPOSITION.schedule),
            REGOLITH_SCHEDULE_DIGEST,
            "the canonical schedule moved; if the change is intended, update \
             REGOLITH_SCHEDULE_DIGEST in the same commit and say why"
        );
    }

    /// A reorder, a rename and a dropped edge each move the digest.
    ///
    /// Without this the pin above is untestable in the direction that
    /// matters: a digest that never moves passes forever.
    #[test]
    fn the_digest_moves_when_the_topology_does() {
        let base = schedule_digest(&REGOLITH_COMPOSITION.schedule);

        let mut swapped = REGOLITH_COMPOSITION.schedule;
        const SWAPPED_STAGES: &[orrery_compose::ScheduleStageManifest] =
            &[orrery_compose::ScheduleStageManifest {
                id: ScheduleStageId("craft-motion"),
                systems: &[SystemId("craft-apply-drag"), SystemId("craft-clamp-speed")],
            }];
        swapped.stages = SWAPPED_STAGES;
        assert_ne!(schedule_digest(&swapped), base);

        let mut fewer_edges = REGOLITH_COMPOSITION.schedule;
        fewer_edges.ordering_edges = &[];
        assert_ne!(schedule_digest(&fewer_edges), base);

        let mut relaxed = REGOLITH_COMPOSITION.schedule;
        relaxed.ambiguity_detection = orrery_compose::AmbiguityDetection::Warning;
        assert_ne!(schedule_digest(&relaxed), base);
    }

    /// Edge order at the declaration site is not part of the topology.
    #[test]
    fn the_digest_ignores_the_textual_order_of_edges() {
        let mut reversed = REGOLITH_COMPOSITION.schedule;
        let flipped: Vec<orrery_compose::ScheduleOrderingEdge> = REGOLITH_CANONICAL_SCHEDULE
            .ordering_edges
            .iter()
            .rev()
            .copied()
            .collect();
        let leaked: &'static [orrery_compose::ScheduleOrderingEdge] = Box::leak(flipped.into());
        reversed.ordering_edges = leaked;
        assert_eq!(
            schedule_digest(&reversed),
            schedule_digest(&REGOLITH_COMPOSITION.schedule)
        );
    }
}

#[cfg(test)]
mod system_tests {
    //! What a declared schedule buys a rules author, demonstrated rather than
    //! asserted: a rule is now a thing you can *hold*.
    //!
    //! Every test here drives **one named system** on **one constructed
    //! state**. None of them builds an `Executor`, seals an input log, plays a
    //! scenario, or steps a tick. Before the schedule none of that was
    //! optional: the smallest unit `Ruleset::step` exposed was a whole
    //! entity-tick, so a four-line rule about a wreck's countdown could only be
    //! reached by flying a craft into enough damage to produce one — which is
    //! why the existing acceptance tests in `tests/regolith.rs` are written as
    //! scenarios even where the thing under test is arithmetic.

    use super::state::{Craft, Pickup};
    use super::{
        archetype::Archetype, order::Order, order::Outcome, weapon::WeaponKind, Regolith,
        RegolithLocals, RegolithState, PICKUP_TTL_TICKS, REGOLITH_SCHEDULE, RESPAWN_TICKS,
    };
    use orrery_core::{run_system_as, OrderedInputs, QPos, SystemName};
    use orrery_protocol::{PersistId, Tick, UniverseSeed};

    fn rng() -> orrery_core::TickRng {
        orrery_core::tick_rng(UniverseSeed([3; 32]), PersistId::new(1), Tick::new(0))
    }

    /// The wreck countdown, tested as four lines of arithmetic.
    #[test]
    fn the_respawn_rule_can_be_driven_on_its_own() {
        let system = REGOLITH_SCHEDULE
            .system(SystemName("craft-respawn"))
            .expect("craft-respawn is in the schedule");
        let mut craft = Craft::spawned(Archetype::Interceptor, QPos::default(), 0);
        craft.hull = 0;
        craft.respawn_in = 1;
        craft.weapon = WeaponKind::Heavy;
        let mut state = RegolithState::Craft(craft);
        let mut locals = RegolithLocals::default();
        // The premise this rule reads: the craft entered the tick already dead.
        locals.craft.was_alive = false;

        let inputs: [Order; 0] = [];
        let events = run_system_as(
            PersistId::new(1),
            &Regolith::honest(),
            &mut state,
            &OrderedInputs::new(&inputs),
            &mut rng(),
            &mut locals,
            system,
        );

        assert!(events.is_empty(), "respawning emits nothing");
        assert!(locals.craft.respawned, "the countdown completed");
        let RegolithState::Craft(craft) = state else {
            unreachable!("the state is a craft")
        };
        assert_eq!(craft.hull, Archetype::Interceptor.limits().max_hull);
        assert_eq!(craft.weapon, WeaponKind::Stock, "a respawn re-equips stock");
        assert_eq!(craft.respawn_in, 0);
    }

    /// A live craft is not touched by the respawn rule at all.
    #[test]
    fn the_respawn_rule_declines_a_craft_that_entered_the_tick_alive() {
        let system = REGOLITH_SCHEDULE
            .system(SystemName("craft-respawn"))
            .expect("craft-respawn is in the schedule");
        let mut craft = Craft::spawned(Archetype::Interceptor, QPos::default(), 0);
        craft.respawn_in = RESPAWN_TICKS;
        let before = craft.clone();
        let mut state = RegolithState::Craft(craft);
        let mut locals = RegolithLocals::default();
        locals.craft.was_alive = true;

        let inputs: [Order; 0] = [];
        run_system_as(
            PersistId::new(1),
            &Regolith::honest(),
            &mut state,
            &OrderedInputs::new(&inputs),
            &mut rng(),
            &mut locals,
            system,
        );
        assert_eq!(state, RegolithState::Craft(before));
        assert!(!locals.craft.respawned);
    }

    /// A rock is not a craft, so the craft systems do not run on it.
    ///
    /// The projection is the selection an ECS query would perform, done per
    /// entity. It replaces the four-way `match` that used to open
    /// `Ruleset::step`, and it is what lets a rule be written against
    /// `&mut Craft` instead of against the whole state enum.
    #[test]
    fn a_craft_system_declines_a_state_that_is_not_a_craft() {
        let system = REGOLITH_SCHEDULE
            .system(SystemName("craft-tick-cooldowns"))
            .expect("craft-tick-cooldowns is in the schedule");
        let mut state = RegolithState::Pickup(Pickup::spawned(
            QPos::default(),
            WeaponKind::Volley,
            PICKUP_TTL_TICKS,
        ));
        let before = state.clone();
        let inputs: [Order; 0] = [];
        let events = run_system_as(
            PersistId::new(9),
            &Regolith::honest(),
            &mut state,
            &OrderedInputs::new(&inputs),
            &mut rng(),
            &mut RegolithLocals::default(),
            system,
        );
        assert_eq!(state, before);
        assert!(events.is_empty());
    }

    /// The pickup TTL rule, one system, one assertion.
    #[test]
    fn the_pickup_expiry_rule_can_be_driven_on_its_own() {
        let system = REGOLITH_SCHEDULE
            .system(SystemName("pickup-expire"))
            .expect("pickup-expire is in the schedule");
        let mut state =
            RegolithState::Pickup(Pickup::spawned(QPos::default(), WeaponKind::Volley, 1));
        let inputs: [Order; 0] = [];
        let events = run_system_as(
            PersistId::new(4),
            &Regolith::honest(),
            &mut state,
            &OrderedInputs::new(&inputs),
            &mut rng(),
            &mut RegolithLocals::default(),
            system,
        );
        assert_eq!(
            events,
            vec![Outcome::Expired {
                id: PersistId::new(4)
            }]
        );
    }
}
