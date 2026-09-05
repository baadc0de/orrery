//! `SimulationHost::seat_at` (#1113): moving an unstepped host's clock in
//! place, and refusing once the host is no longer indistinguishable from a
//! freshly constructed one at the target tick.
//!
//! Before this call, `clients/regolith`'s `CampaignRuntime::seat_host_at`
//! rebuilt the host at the join tick and reinstalled everything it held —
//! correct because nothing is queued, stepped or emitted before `Joined`,
//! but a rebuild standing in for a capability the seam lacked. The test
//! below proves the two paths agree; the three refusal tests each prove one
//! of "queued, stepped, emitted" actually gates the call, with a control
//! host that has not done that thing succeeding on the identical call.

use orrery_core::CoreCodec;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::{HostError, SimulationHost, SimulationHostConfig, TickCount};
use synthetic::{Synthetic, SyntheticAdapter, SyntheticInput, SyntheticState};

#[path = "support/synthetic.rs"]
mod synthetic;

type Host = SimulationHost<Synthetic, SyntheticAdapter>;

const ENTITY: PersistId = PersistId(1);
const OTHER: PersistId = PersistId(2);
const OBSERVED_TICK: u64 = 40;
const FIRST_TICK: u64 = 50;
const SEATED_TICK: u64 = 12_345;

fn state() -> SyntheticState {
    SyntheticState {
        velocity_um_per_tick: [1_000, 0, 0],
        health: 100,
        // No target: this entity never sights a neighbour, so stepping it
        // never emits — the "stepped" and "emitted" refusals stay separable.
        target: 0,
        ..SyntheticState::default()
    }
}

/// A host that has done nothing since [`SimulationHost::new`]: the shape
/// `seat_at` is meant for, holding one entity installed at [`OBSERVED_TICK`]
/// the way a joining client's `launch` installs its spawn pose before the
/// join tick is known.
fn unstepped() -> Host {
    let mut host = SimulationHost::new(
        SimulationHostConfig::new(UniverseSeed([3; 32])).starting_at(Tick::new(FIRST_TICK)),
        Synthetic,
        SyntheticAdapter,
    );
    host.install_state_observed(ENTITY, state(), Tick::new(OBSERVED_TICK));
    host
}

// ── Success: re-seating matches building fresh at the target tick ─────────

#[test]
fn seat_at_moves_the_clock_and_leaves_installed_state_untouched() {
    let mut seated = unstepped();

    seated
        .seat_at(Tick::new(SEATED_TICK))
        .expect("unstepped host seats");

    assert_eq!(seated.next_tick(), Tick::new(SEATED_TICK));
    assert_eq!(
        seated.observed_tick(ENTITY),
        Some(Tick::new(OBSERVED_TICK)),
        "the observation stamp installed before seating is unaffected by moving the clock"
    );
    assert_eq!(
        seated.state_bytes(ENTITY),
        Some(state().to_canonical()),
        "and the installed bytes are unaffected too"
    );
}

#[test]
fn seat_at_matches_rebuilding_the_host_at_the_target_tick() {
    // What `seat_host_at` did before #1113: build a fresh host at the target
    // tick and reinstall what the old one held, byte-identically. `seat_at`
    // must be indistinguishable from that from here on, since nothing was
    // queued, stepped or emitted before either call runs.
    let mut seated = unstepped();
    seated
        .seat_at(Tick::new(SEATED_TICK))
        .expect("unstepped host seats");

    let mut rebuilt = SimulationHost::new(
        SimulationHostConfig::new(UniverseSeed([3; 32])).starting_at(Tick::new(SEATED_TICK)),
        Synthetic,
        SyntheticAdapter,
    );
    rebuilt.install_state_observed(ENTITY, state(), Tick::new(OBSERVED_TICK));

    assert_eq!(seated.next_tick(), rebuilt.next_tick());
    assert_eq!(seated.state_bytes(ENTITY), rebuilt.state_bytes(ENTITY));
    assert_eq!(seated.observed_tick(ENTITY), rebuilt.observed_tick(ENTITY));

    let seated_report = seated.step(TickCount::new(3));
    let rebuilt_report = rebuilt.step(TickCount::new(3));
    assert_eq!(
        seated_report, rebuilt_report,
        "stepping forward from the seated clock reproduces the same state hashes and \
         neighbour frames as stepping forward from a host rebuilt at that tick"
    );
    assert_eq!(seated.state_bytes(ENTITY), rebuilt.state_bytes(ENTITY));
}

// ── Refusal: queued ────────────────────────────────────────────────────────

#[test]
fn seat_at_refuses_a_host_with_input_queued() {
    let mut queued = unstepped();
    queued.submit_input(ENTITY, SyntheticInput::Impulse([1, 0, 0]));
    let mut control = unstepped();

    let result = queued.seat_at(Tick::new(SEATED_TICK));

    assert_eq!(result, Err(HostError::HostAlreadyActive));
    assert_eq!(
        queued.next_tick(),
        Tick::new(FIRST_TICK),
        "the refused call leaves the clock untouched"
    );
    assert!(
        control.seat_at(Tick::new(SEATED_TICK)).is_ok(),
        "non-vacuity: the identical call on a host with nothing queued succeeds, so the \
         refusal above is the queued input, not something else about this scenario"
    );
}

// ── Refusal: stepped ────────────────────────────────────────────────────────

#[test]
fn seat_at_refuses_a_host_that_has_stepped() {
    let mut stepped = unstepped();
    let report = stepped.step(TickCount::new(1));
    assert!(
        stepped.peek_event_bytes().expect("events fit").is_empty(),
        "this scenario steps without emitting, so the refusal below is attributable to the \
         step alone and not to an emitted event as well"
    );
    assert!(
        !report.state_hashes.is_empty(),
        "non-vacuity: the tick actually ran"
    );
    let mut control = unstepped();

    let result = stepped.seat_at(Tick::new(SEATED_TICK));

    assert_eq!(result, Err(HostError::HostAlreadyActive));
    assert_eq!(
        stepped.next_tick(),
        Tick::new(FIRST_TICK + 1),
        "the refused call leaves the clock at wherever stepping left it"
    );
    assert!(
        control.seat_at(Tick::new(SEATED_TICK)).is_ok(),
        "non-vacuity: the identical call on a host that has not stepped succeeds"
    );
}

// ── Refusal: emitted ─────────────────────────────────────────────────────

#[test]
fn seat_at_refuses_a_host_holding_an_emitted_event() {
    let mut host = SimulationHost::new(
        SimulationHostConfig::new(UniverseSeed([3; 32])).starting_at(Tick::new(FIRST_TICK)),
        Synthetic,
        SyntheticAdapter,
    );
    // Two entities, each watching the other, observed one tick behind the
    // first tick and inside the staleness bound: each neighbour read
    // resolves, so the first step sights and strikes, which is what emits.
    let sighting_observed = Tick::new(FIRST_TICK - 1);
    host.install_state_observed(
        ENTITY,
        SyntheticState {
            target: OTHER.0,
            health: 100,
            ..SyntheticState::default()
        },
        sighting_observed,
    );
    host.install_state_observed(
        OTHER,
        SyntheticState {
            target: ENTITY.0,
            health: 100,
            ..SyntheticState::default()
        },
        sighting_observed,
    );
    host.step(TickCount::new(1));
    assert!(
        !host.peek_event_bytes().expect("events fit").is_empty(),
        "non-vacuity: the mutual sighting actually emitted"
    );

    let result = host.seat_at(Tick::new(SEATED_TICK));

    assert_eq!(result, Err(HostError::HostAlreadyActive));
    assert_eq!(host.next_tick(), Tick::new(FIRST_TICK + 1));

    // A control host that never stepped, so it never emitted, seats cleanly:
    // the refusal above is attributable to holding an emitted event.
    let mut control = SimulationHost::new(
        SimulationHostConfig::new(UniverseSeed([3; 32])).starting_at(Tick::new(FIRST_TICK)),
        Synthetic,
        SyntheticAdapter,
    );
    control.install_state_observed(ENTITY, state(), Tick::new(OBSERVED_TICK));
    assert!(control.seat_at(Tick::new(SEATED_TICK)).is_ok());
}
