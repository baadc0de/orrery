//! Spike (#793): what `OrderedQuery` actually logs, and whether that log is
//! the shape `replay.rs:325` compares.
//!
//! These tests live in the facade crate because building a `World` needs to
//! name one, and only the facade can. The systems they run are written in
//! `facade_game`, which cannot — that split is the point.

use bevy_ecs::prelude::*;
use bevy_ecs::system::RunSystemOnce;
use facade_game::{Occluder, Rock};
use orrery_ecs_facade::{
    Access, AccessKind, AccessLog, KeyIndex, ObservedAt, OrderedQuery, PersistKey, ReadWindow,
};
use orrery_protocol::{PersistId, Tick};

/// Regolith's cap today (`crates/orrery_games/src/regolith/mod.rs:608` →
/// `visibility.rs:47`, one slot each for locker, rock and collision).
const REGOLITH_MAX_NEIGHBOR_READS: usize = 3;

#[derive(Resource)]
struct Wanted(PersistId);

fn id(n: u64) -> PersistId {
    PersistId(n)
}

/// A world with a rock per id, spawned in the order `order` names.
fn world_with(order: &[u64], hp: u32) -> World {
    let mut world = World::new();
    world.insert_resource(AccessLog::default());
    // `ReadWindow::open()` — these tests are about what the log records, and
    // the identity and staleness refusals are measured against a real replay
    // in `replay_through_ordered_query.rs` instead.
    world.insert_resource(ReadWindow::open());
    let mut index = KeyIndex::default();
    for n in order {
        let entity = world
            .spawn((PersistKey(id(*n)), ObservedAt(Tick::new(0)), Rock { hp }))
            .id();
        index.insert(id(*n), entity);
    }
    world.insert_resource(index);
    world
}

fn log_of(world: &World) -> Vec<Access> {
    world.resource::<AccessLog>().entries().to_vec()
}

fn seek(mut rocks: OrderedQuery<&'static Rock>, wanted: Res<Wanted>) {
    let _ = facade_game::read_named_neighbour(&mut rocks, wanted.0);
}

// ── The worked example: a search that finds nothing ──────────────────────────

/// The whole argument in one assertion.
///
/// `query.get(e)` hands back a `Result` whose discriminant is an existence bit
/// obtained with the log untouched — that is #798's
/// `option_b_leaks_absence_with_an_empty_log`. Here the log is not untouched:
/// the search is recorded, the tap is not, and the caller's `None` and the
/// log's single `Searched` entry say the same thing.
#[test]
fn a_search_that_finds_nothing_still_records_the_search() {
    let mut world = world_with(&[1, 2, 3], 10);
    world.insert_resource(Wanted(id(9)));
    world.run_system_once(seek).unwrap();

    assert_eq!(
        log_of(&world),
        vec![Access {
            key: id(9),
            kind: AccessKind::Searched
        }],
        "the search must be recorded even though nothing was found"
    );
    assert_eq!(
        world.resource::<AccessLog>().neighbor_reads(),
        vec![id(9)],
        "and it must reach `neighbor_reads` exactly as a found read would"
    );
}

/// The found case, for contrast: two entries, and `neighbor_reads` cannot tell
/// the two cases apart — which is the property that makes absence unprofitable.
#[test]
fn a_search_that_finds_something_records_both_halves() {
    let mut world = world_with(&[1, 2, 3], 10);
    world.insert_resource(Wanted(id(2)));
    world.run_system_once(seek).unwrap();

    assert_eq!(
        log_of(&world),
        vec![
            Access {
                key: id(2),
                kind: AccessKind::Searched
            },
            Access {
                key: id(2),
                kind: AccessKind::Tapped
            },
        ]
    );
    assert_eq!(world.resource::<AccessLog>().neighbor_reads(), vec![id(2)]);
}

// ── The replay-contract bridge ──────────────────────────────────────────────

/// `neighbor_reads()` must deduplicate and keep first-mention order, because
/// `StateView::neighbor` does (`ruleset.rs:196`, `ruleset.rs:207`) and
/// `replay.rs:325` compares the two sequences elementwise.
#[test]
fn neighbor_reads_deduplicates_in_first_mention_order() {
    let mut world = world_with(&[1, 2, 3], 10);
    world.insert_resource(Wanted(id(3)));
    world.run_system_once(seek).unwrap();
    world.insert_resource(Wanted(id(1)));
    world.run_system_once(seek).unwrap();
    world.insert_resource(Wanted(id(3)));
    world.run_system_once(seek).unwrap();

    assert_eq!(
        world.resource::<AccessLog>().neighbor_reads(),
        vec![id(3), id(1)],
        "first mention wins and repeats collapse — the shape replay compares"
    );
}

// ── Enumeration: honest, and priced ─────────────────────────────────────────

fn enumerate_occluders(mut occluders: OrderedQuery<&'static Occluder>) {
    let _ = facade_game::count_occluders(&mut occluders);
}

fn occluder_world(order: &[u64]) -> World {
    let mut world = World::new();
    world.insert_resource(AccessLog::default());
    world.insert_resource(ReadWindow::open());
    let mut index = KeyIndex::default();
    for n in order {
        let entity = world
            .spawn((PersistKey(id(*n)), ObservedAt(Tick::new(0)), Occluder))
            .id();
        index.insert(id(*n), entity);
    }
    world.insert_resource(index);
    world
}

/// Enumeration is canonical: two permuted insertion orders yield the same
/// sequence. This is D43 (e)(4)'s property, held by the query rather than by
/// the author remembering to sort.
#[test]
fn enumeration_is_persistid_ordered_under_permuted_insertion() {
    let mut a = occluder_world(&[7, 1, 4]);
    let mut b = occluder_world(&[4, 7, 1]);
    a.run_system_once(enumerate_occluders).unwrap();
    b.run_system_once(enumerate_occluders).unwrap();

    let expected = vec![id(1), id(4), id(7)];
    assert_eq!(a.resource::<AccessLog>().neighbor_reads(), expected);
    assert_eq!(b.resource::<AccessLog>().neighbor_reads(), expected);
}

/// The other direction, so the sort is measured rather than decorative: raw
/// query iteration order *does* follow insertion, so the sort is doing work.
#[test]
fn raw_query_iteration_follows_insertion_order() {
    fn raw(world: &mut World) -> Vec<PersistId> {
        let mut state = world.query::<(&PersistKey, &Occluder)>();
        state.iter(world).map(|(key, _)| key.0).collect()
    }

    let mut a = occluder_world(&[7, 1, 4]);
    let mut b = occluder_world(&[4, 7, 1]);
    assert_ne!(
        raw(&mut a),
        raw(&mut b),
        "if these ever agree, the sort in `enumerate` is untested, not unnecessary"
    );
}

/// The price, stated as an assertion rather than as prose.
///
/// `replay.rs:275-278` rejects a window carrying more frames than
/// `Ruleset::max_neighbor_reads()`. An enumeration records the population, so
/// the population is what the cap must admit — and a cap set to a population
/// bound bounds nothing. This is #798's option D, and this is its bill.
#[test]
fn enumeration_costs_one_recorded_read_per_entity_and_blows_the_cap() {
    let mut world = occluder_world(&[1, 2, 3, 4, 5, 6, 7, 8]);
    world.run_system_once(enumerate_occluders).unwrap();

    let reads = world.resource::<AccessLog>().neighbor_reads();
    assert_eq!(reads.len(), 8, "one recorded read per entity in the query");
    assert!(
        reads.len() > REGOLITH_MAX_NEIGHBOR_READS,
        "a population of 8 does not fit a cap of {REGOLITH_MAX_NEIGHBOR_READS}"
    );
}
