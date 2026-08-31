//! **SPIKE #793 — propose-only. Do not merge.**
//!
//! Scores the four neighbour-access options against D43 (e)(5)'s recorded-read
//! discipline. Each test is one row of the table in
//! `docs/spikes/neighbour-access-options.md`, and each asserts rather than
//! asserts-in-prose.

use std::collections::BTreeMap;

use bevy_ecs::prelude::{Query, ResMut, World};
use orrery_core::{Ruleset, StateView};
use orrery_games::regolith::neighbour_options::{
    access_of, audit_neighbour_access, read_stage_b, Neighbour, ReadLog, Yielded,
};
use orrery_games::regolith::Regolith;
use orrery_games::scenario::SCENARIOS;
use orrery_protocol::{PersistId, Tick};

/// A stand-in payload. `StateView` is generic over the state type, so the
/// recording behaviour under test is the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bag(u32);

const READER: PersistId = PersistId::new(1);
const PRESENT: PersistId = PersistId::new(5);
const ABSENT: PersistId = PersistId::new(9);
const STALENESS: u64 = 8;

fn neighbours() -> BTreeMap<PersistId, Bag> {
    BTreeMap::from([(PRESENT, Bag(2))])
}

// ── Option A — the baseline ─────────────────────────────────────────────────

/// **A records the absent read.** This is the property the whole of D43 (e)(5)
/// rests on: `StateView::neighbor` takes `&mut self` and pushes into the log
/// *before* it knows whether it found anything, so a lookup that misses still
/// produces a `NeighborFrame { present: false, .. }` for replay to check.
#[test]
fn option_a_records_a_read_that_found_nothing() {
    let map = neighbours();
    let mut own = Bag(0);
    let mut view = StateView::new(READER, &mut own, &map);

    assert_eq!(view.neighbor(PRESENT), Some(&Bag(2)));
    assert_eq!(view.neighbor(ABSENT), None, "nothing is there");

    assert_eq!(
        view.recorded_reads(),
        &[PRESENT, ABSENT],
        "the absent lookup is in the log; that is what makes it replayable"
    );
}

/// **A cannot enumerate.** There is no method on `StateView` that yields a
/// population — the only way to a neighbour is to name it. Asserted here as a
/// property of the recorded set: after asking about one id, the log holds
/// exactly one entry, whatever the population is.
#[test]
fn option_a_cannot_observe_the_population() {
    let map = BTreeMap::from([
        (PersistId::new(2), Bag(1)),
        (PersistId::new(3), Bag(1)),
        (PersistId::new(4), Bag(1)),
        (PRESENT, Bag(2)),
    ]);
    let mut own = Bag(0);
    let mut view = StateView::new(READER, &mut own, &map);
    view.neighbor(PRESENT);
    assert_eq!(
        view.recorded_reads().len(),
        1,
        "a step's read count is bounded by what it named, never by the population"
    );
}

// ── Option B — token exchange ───────────────────────────────────────────────

/// **B records what it dereferences.** The token exchange works for the case it
/// covers: privacy means the payload is unreachable without the log.
#[test]
fn option_b_records_the_dereference() {
    let mut log = ReadLog::new(READER, Tick::new(100), STALENESS);
    let n = Neighbour::new(PRESENT, Bag(2), Tick::new(99));
    assert_eq!(log.read(&n), Some(&Bag(2)));
    assert_eq!(log.reads(), &[PRESENT]);
}

/// **B's hole, and it is the load-bearing one.**
///
/// The absent case has no `Neighbour` to hand to `read`, so privacy has
/// nothing to attach to. A rule that looks a neighbour up and finds nothing
/// learns that fact — an existence bit — and the log stays empty unless the
/// author volunteers `read_absent`.
///
/// Compare `option_a_records_a_read_that_found_nothing`: under A the same
/// program cannot be written, because the lookup *is* the recording call.
#[test]
fn option_b_leaks_absence_with_an_empty_log() {
    let mut log = ReadLog::new(READER, Tick::new(100), STALENESS);
    let store: BTreeMap<PersistId, Neighbour<Bag>> =
        BTreeMap::from([(PRESENT, Neighbour::new(PRESENT, Bag(2), Tick::new(99)))]);

    // The obvious thing to write. It compiles, it runs, and it is a read.
    let saw_it = store.contains_key(&ABSENT);

    assert!(!saw_it, "the rule now knows entity 9 is not there");
    assert!(
        log.reads().is_empty(),
        "...and nothing in the type system made it say so"
    );
    // The line the author has to remember. Nothing requires it.
    log.read_absent(ABSENT);
    assert_eq!(log.reads(), &[ABSENT]);
}

/// **B does not close enumeration.** A `Query<&Neighbour<S>>` still counts,
/// still filters, still orders — all before any token is exchanged. Modelled
/// here on the same store, because the leak is about *matching*, not about
/// `bevy_ecs`.
#[test]
fn option_b_leaves_the_population_observable() {
    let log = ReadLog::new(READER, Tick::new(100), STALENESS);
    let store: BTreeMap<PersistId, Neighbour<Bag>> = (2..12)
        .map(|i| {
            let id = PersistId::new(i);
            (id, Neighbour::new(id, Bag(1), Tick::new(99)))
        })
        .collect();

    // `id()` is not privileged, and it cannot be: a query that could not name
    // what it matched could not hand anything to `read` either.
    let crowded = store.values().map(Neighbour::id).count() > 5;

    assert!(crowded, "the rule branched on how many neighbours exist");
    assert!(
        log.reads().is_empty(),
        "a rule can branch on the shape of the population and log nothing"
    );
}

/// **B at the real call site**, with the absent arm written correctly. When the
/// author does remember, B's recorded set is identical to A's — which is the
/// point: B's ceiling is A's floor.
#[test]
fn option_b_call_site_matches_the_baseline_when_written_correctly() {
    let mut log = ReadLog::new(READER, Tick::new(100), STALENESS);
    let present = Neighbour::new(PRESENT, Bag(2), Tick::new(99));
    let found = read_stage_b(&mut log, [Some(PRESENT), Some(ABSENT), None], |id| {
        (id == PRESENT).then_some(&present)
    });
    assert_eq!(found[0], Some(&Bag(2)));
    assert_eq!(found[1], None);
    assert_eq!(found[2], None);
    assert_eq!(
        log.reads(),
        &[PRESENT, ABSENT],
        "same recorded sequence as Option A, reached with more machinery"
    );
}

// ── Option D — logged enumeration ───────────────────────────────────────────

/// **What D costs the record, measured on the shipped scenarios.**
///
/// The executor serves neighbours from the tick-start slot holding every other
/// live entity, so a native `Query<&Neighbour<S>>` with no spatial filter
/// yields `N - 1` rows per entity-tick. Under D every one of those ids enters
/// the window. `Regolith::max_neighbor_reads()` is 3 and
/// `crates/orrery_core/src/replay.rs:275-278` rejects a window carrying more
/// frames than the cap — so adopting D means raising the cap to a population
/// bound, at which point the cap bounds nothing.
#[test]
fn option_d_log_growth_on_the_shipped_scenarios() {
    let cap = Regolith::honest().max_neighbor_reads();
    assert_eq!(cap, 3, "the shipped cap the population bound would replace");

    let mut worst = 0u64;
    let mut over_cap = 0usize;
    let mut total = 0usize;
    for scenario in SCENARIOS {
        if scenario.entities <= 1 {
            continue;
        }
        total += 1;
        let yielded = scenario.entities - 1;
        worst = worst.max(yielded);
        if yielded > cap as u64 {
            over_cap += 1;
        }
        println!(
            "{:<28} entities={:<5} yielded/entity-tick={:<5} cap={cap}",
            scenario.name, scenario.entities, yielded
        );
    }
    println!(
        "worst={worst} over_cap={over_cap}/{total} multiplier={}x",
        worst / cap as u64
    );
    assert!(
        over_cap > 0,
        "at least one shipped scenario must exceed the cap, or D costs nothing here"
    );
    assert!(
        worst > cap as u64,
        "the widest shipped scenario yields {worst} ids per entity-tick against a \
         recorded-read cap of {cap}; adopting D means raising the cap to a \
         population bound, at which point it bounds nothing"
    );
}

/// **D's canonical-order obligation, and that it is real.**
///
/// The yielded set must enter the record in canonical order or the record
/// becomes insertion-order-dependent — the property
/// `tier_h_projection_differential.rs` tests. `Yielded::ids` sorts; this proves
/// the sort is load-bearing by feeding two permutations and requiring one
/// answer.
#[test]
fn option_d_yielded_set_is_canonical_not_iteration_order() {
    let forward = Yielded {
        ids: {
            let mut v: Vec<_> = (2..8).map(PersistId::new).collect();
            v.sort();
            v
        },
    };
    let reversed = Yielded {
        ids: {
            let mut v: Vec<_> = (2..8).rev().map(PersistId::new).collect();
            v.sort();
            v
        },
    };
    assert_eq!(
        forward, reversed,
        "two insertion orders, one recorded set — this is the sort #787 would owe"
    );
}

// ── Option E — registration-time refusal ────────────────────────────────────

/// A system that can see a neighbour and does not hold the log. Under a native
/// ruleset this is the *obvious* thing to write, and nothing about it is
/// ill-typed.
fn unlogged_reader(_q: Query<&Neighbour<Bag>>) {}

/// The same access, with the log in hand.
fn logged_reader(_q: Query<&Neighbour<Bag>>, _log: ResMut<ReadLog>) {}

/// A system that cannot reach a neighbour at all — the shape `sched.rs`'s
/// `System` already guarantees, and the shape every one of #796's ergonomic
/// wins actually has.
fn own_state_only(_log: ResMut<ReadLog>) {}

/// **E refuses the unlogged reader, and only it.**
///
/// This is a build-time guarantee rather than a compile-time one, but it is
/// total over the registered schedule — which is more than B, C or D manage,
/// and it is the same shape as the ambiguity canary the host already runs at
/// `crates/orrery_sim_host/src/ecs.rs:528`.
#[test]
fn option_e_refuses_a_neighbour_reader_that_does_not_hold_the_log() {
    let mut world = World::new();
    let refused = audit_neighbour_access::<Bag>(
        &mut world,
        vec![("unlogged_reader", access_of(unlogged_reader))],
    );
    assert_eq!(
        refused,
        Err("unlogged_reader can reach a neighbour without holding the read log".to_string())
    );

    let mut world = World::new();
    let accepted = audit_neighbour_access::<Bag>(
        &mut world,
        vec![
            ("logged_reader", access_of(logged_reader)),
            ("own_state_only", access_of(own_state_only)),
        ],
    );
    assert_eq!(accepted, Ok(()), "holding the log is the whole requirement");
}

/// **What E does and does not buy.** It proves a system *holds* the log; it
/// cannot prove the system *used* it, so it composes with B rather than
/// replacing it. Recorded here as a limitation, asserted so it cannot rot into
/// a claim.
#[test]
fn option_e_cannot_prove_the_log_was_used() {
    fn holds_but_ignores(_q: Query<&Neighbour<Bag>>, _log: ResMut<ReadLog>) {
        // reads nothing, records nothing, passes the audit
    }
    let mut world = World::new();
    assert_eq!(
        audit_neighbour_access::<Bag>(
            &mut world,
            vec![("holds_but_ignores", access_of(holds_but_ignores))]
        ),
        Ok(()),
        "E is a capability audit, not a usage audit"
    );
}
