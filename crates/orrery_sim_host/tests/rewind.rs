//! Snapshot, restore and replay on the host, on both backends.
//!
//! The claim under test is field-exact restore: after `restore`, the host's
//! output bytes equal the snapshot-time bytes, and stepping it through the
//! same post-snapshot inputs reproduces the original run's state hashes and
//! output bytes.  The observation tick and the queued next-tick inputs are
//! each proven to be part of that claim by a scenario that diverges without
//! them.

use orrery_core::{CoreCodec, Executor, TickBackend};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::abi::{
    orrery_host_collect_states, orrery_host_destroy, orrery_host_next_tick, orrery_host_restore,
    orrery_host_snapshot, orrery_host_step, OrreryHost, OrreryHostResult,
};
use orrery_sim_host::ecs::EcsBackend;
use synthetic::{Synthetic, SyntheticAdapter, SyntheticInput, SyntheticState};

#[path = "support/synthetic.rs"]
mod synthetic;
use orrery_sim_host::{
    HostError, HostSnapshot, SimulationHost, SimulationHostConfig, StepReport, TickCount,
};

const WATCHER: PersistId = PersistId(1);
const WATCHED: PersistId = PersistId(2);

type Host<B> = SimulationHost<Synthetic, SyntheticAdapter, B>;

fn config(first_tick: u64) -> SimulationHostConfig {
    SimulationHostConfig::new(UniverseSeed([7; 32])).starting_at(Tick::new(first_tick))
}

fn executor_host(first_tick: u64) -> Host<Executor<Synthetic>> {
    SimulationHost::new(config(first_tick), Synthetic, SyntheticAdapter)
}

fn ecs_host(first_tick: u64) -> Host<EcsBackend<Synthetic>> {
    SimulationHost::on_backend(
        config(first_tick),
        EcsBackend::new(Synthetic, UniverseSeed([7; 32])),
        SyntheticAdapter,
    )
}

fn watcher() -> SyntheticState {
    SyntheticState {
        velocity_um_per_tick: [1_234, 0, -5],
        health: 100,
        target: WATCHED.0,
        ..SyntheticState::default()
    }
}

fn watched() -> SyntheticState {
    SyntheticState {
        position_um: [5_000, 0, 0],
        velocity_um_per_tick: [0, 999, 0],
        health: 9,
        target: WATCHER.0,
        ..SyntheticState::default()
    }
}

fn command(entity: PersistId, input: SyntheticInput) -> Vec<u8> {
    let mut bytes = entity.0.to_le_bytes().to_vec();
    bytes.extend(input.to_canonical());
    bytes
}

fn output<B: TickBackend<Synthetic>>(host: &Host<B>) -> Vec<u8> {
    host.collect_output_bytes()
        .expect("states fit the buffer")
        .into_bytes()
}

fn state<B: TickBackend<Synthetic>>(host: &Host<B>, entity: PersistId) -> SyntheticState {
    SyntheticState::decode(&host.state_bytes(entity).expect("entity is held"))
        .expect("state decodes")
}

/// Run the post-snapshot history: one command, then four ticks.
fn replay<B: TickBackend<Synthetic>>(host: &mut Host<B>) -> StepReport {
    host.submit_command_bytes(&command(WATCHER, SyntheticInput::Impulse([0, 0, 777])))
        .expect("canonical impulse decodes");
    host.step(TickCount::new(4))
}

fn snapshot_step_restore_step<B: TickBackend<Synthetic>>(mut host: Host<B>) {
    host.install_state(WATCHER, watcher());
    host.install_state(WATCHED, watched());
    // Three ticks in: the watcher has struck, so an adapter-delivered damage
    // input is queued for the next tick at the moment of the snapshot.
    host.step(TickCount::new(3));
    let snapshot = host.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(
        snapshot.queued_inputs().count(),
        2,
        "the strikes routed by the adapter, one each way, are queued at the snapshot boundary"
    );
    let at_snapshot = output(&host);

    let first = replay(&mut host);
    let first_output = output(&host);
    host.install_state(PersistId::new(3), SyntheticState::default());
    assert_ne!(first_output, at_snapshot, "four ticks moved the population");

    host.restore(&snapshot)
        .expect("the host's own snapshot restores");
    assert_eq!(
        output(&host),
        at_snapshot,
        "restore is field-exact on the output bytes"
    );
    assert_eq!(host.next_tick(), snapshot.next_tick());
    assert!(
        host.state_bytes(PersistId::new(3)).is_none(),
        "an entity installed after the snapshot does not survive the restore"
    );

    let second = replay(&mut host);
    assert_eq!(second, first, "the replayed ticks produce the same hashes");
    assert_eq!(output(&host), first_output, "and the same bytes");
    assert!(
        state(&host, WATCHED).health < 9,
        "the queued strike landed on the replay as it did on the first run"
    );
}

#[test]
fn snapshot_step_restore_step_reproduces_hashes_and_bytes_on_the_executor() {
    snapshot_step_restore_step(executor_host(0));
}

#[test]
fn snapshot_step_restore_step_reproduces_hashes_and_bytes_on_the_ecs_backend() {
    snapshot_step_restore_step(ecs_host(0));
}

#[test]
fn the_two_backends_agree_on_the_snapshot_bytes_and_on_the_replay() {
    let mut executor = executor_host(0);
    let mut ecs = ecs_host(0);
    for host in [&mut executor as &mut dyn Probe, &mut ecs] {
        host.seed();
    }
    assert_eq!(executor.snapshot(), ecs.snapshot());
    let executor_snapshot = executor.snapshot();
    ecs.restore(&executor_snapshot)
        .expect("an executor snapshot restores on the ECS backend");
    assert_eq!(replay(&mut executor), replay(&mut ecs));
    assert_eq!(output(&executor), output(&ecs));
}

trait Probe {
    fn seed(&mut self);
}

impl<B: TickBackend<Synthetic>> Probe for Host<B> {
    fn seed(&mut self) {
        self.install_state(WATCHER, watcher());
        self.install_state(WATCHED, watched());
        self.step(TickCount::new(3));
    }
}

/// The observation tick is part of the restored state.  Under the synthetic
/// staleness cap of two ticks, an entity installed as observed at tick 0 is
/// invisible to a watcher stepping at tick 10; a restore that re-stamped it
/// with the restore-time tick would make it visible and change the hash.
#[test]
fn restore_carries_the_observation_tick_not_the_restore_time() {
    let mut host = executor_host(10);
    host.install_state_observed(WATCHER, watcher(), Tick::new(10));
    host.install_state_observed(WATCHED, watched(), Tick::new(0));
    let snapshot = host.snapshot();
    assert_eq!(
        snapshot
            .entities()
            .map(|(entity, record)| (entity, record.observed_tick()))
            .collect::<Vec<_>>(),
        vec![(WATCHER, Tick::new(10)), (WATCHED, Tick::new(0))]
    );

    let first = host.step(TickCount::new(1));
    assert_eq!(
        state(&host, WATCHER).sightings,
        0,
        "a ten-tick-old observation is outside the staleness cap"
    );

    host.restore(&snapshot).expect("snapshot restores");
    assert_eq!(host.observed_tick(WATCHED), Some(Tick::new(0)));
    let second = host.step(TickCount::new(1));
    assert_eq!(second, first);
    assert_eq!(state(&host, WATCHER).sightings, 0);

    // Positive control: the same population with a fresh observation is
    // sighted, so the stamp is what the equality above depends on.
    let mut fresh = executor_host(10);
    fresh.install_state_observed(WATCHER, watcher(), Tick::new(10));
    fresh.install_state_observed(WATCHED, watched(), Tick::new(10));
    let sighted = fresh.step(TickCount::new(1));
    assert_eq!(state(&fresh, WATCHER).sightings, 1);
    assert_ne!(sighted, first);
}

#[test]
fn a_trimmed_snapshot_restores_exactly_its_own_population() {
    let mut host = executor_host(0);
    host.install_state(WATCHER, watcher());
    host.install_state(WATCHED, watched());
    host.step(TickCount::new(2));
    let mut snapshot = host.snapshot();
    assert!(snapshot.remove(WATCHED).is_some());
    host.restore(&snapshot).expect("trimmed snapshot restores");
    assert!(host.state_bytes(WATCHED).is_none());
    assert!(host.state_bytes(WATCHER).is_some());
}

#[test]
fn snapshot_bytes_round_trip_and_refuse_truncation_and_trailing_bytes() {
    let mut host = executor_host(5);
    host.install_state(WATCHER, watcher());
    host.install_state(WATCHED, watched());
    host.step(TickCount::new(2));
    host.submit_command_bytes(&command(WATCHED, SyntheticInput::Damage(3)))
        .expect("canonical damage decodes");
    let snapshot = host.snapshot();
    let bytes = snapshot.to_bytes().expect("snapshot encodes");
    assert_eq!(HostSnapshot::from_bytes(&bytes), Ok(snapshot.clone()));

    for cut in 1..bytes.len() {
        assert_eq!(
            HostSnapshot::from_bytes(&bytes[..cut]),
            Err(HostError::MalformedSnapshot),
            "a snapshot truncated to {cut} bytes is refused"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        HostSnapshot::from_bytes(&trailing),
        Err(HostError::MalformedSnapshot)
    );
    let mut other_format = bytes;
    other_format[0] ^= 1;
    assert_eq!(
        HostSnapshot::from_bytes(&other_format),
        Err(HostError::MalformedSnapshot)
    );
}

#[test]
fn a_snapshot_under_another_ruleset_or_with_a_bad_record_leaves_the_host_untouched() {
    let mut host = executor_host(0);
    host.install_state(WATCHER, watcher());
    host.step(TickCount::new(1));
    let before = output(&host);
    let bytes = host.snapshot().to_bytes().expect("snapshot encodes");

    let mut foreign = bytes.clone();
    foreign[4] ^= 1; // the ruleset version
    assert_eq!(
        host.restore(&HostSnapshot::from_bytes(&foreign).expect("framing is intact")),
        Err(HostError::SnapshotRulesetMismatch)
    );

    // Shorten the one state record by a byte and fix the framing around it,
    // so the snapshot parses and only the state fails to decode.
    let mut short = bytes;
    let length_at = 4 + 4 + 32 + 8 + 8 + 8 + 8;
    let length = u32::from_le_bytes(short[length_at..length_at + 4].try_into().expect("u32"));
    short[length_at..length_at + 4].copy_from_slice(&(length - 1).to_le_bytes());
    short.remove(length_at + 4);
    assert_eq!(
        host.restore(&HostSnapshot::from_bytes(&short).expect("framing is intact")),
        Err(HostError::MalformedSnapshot)
    );

    host.step(TickCount::new(0));
    assert_eq!(output(&host), before, "neither refusal touched the host");
    assert_eq!(host.next_tick(), Tick::new(1));
}

/// The C entry points, called from Rust: null handling and a caught panic.
/// The real ABI test is `tests/c_consumer.rs`; this covers what a C caller
/// cannot conveniently assert, the Rust-visible poison state.
#[test]
fn the_entry_points_refuse_null_and_report_a_panic_without_unwinding() {
    let mut required = 0_usize;
    let mut tick = 0_u64;
    // SAFETY: null handles and buffers are what is under test; every pointer
    // that is not null names live storage on this stack frame.
    unsafe {
        assert_eq!(
            orrery_host_next_tick(std::ptr::null(), &mut tick),
            OrreryHostResult::NullArgument
        );
        assert_eq!(
            orrery_host_destroy(std::ptr::null_mut()),
            OrreryHostResult::NullArgument
        );

        let mut host = executor_host(0);
        host.install_state(WATCHER, watcher());
        host.submit_command_bytes(&command(WATCHER, SyntheticInput::Poison))
            .expect("the poison input decodes");
        let handle = OrreryHost::new(host).into_raw();
        assert_eq!(
            orrery_host_snapshot(handle, std::ptr::null_mut(), 0, &mut required),
            OrreryHostResult::BufferTooSmall
        );
        assert!(required > 0);
        assert_eq!(
            orrery_host_collect_states(handle, std::ptr::null_mut(), 0, std::ptr::null_mut()),
            OrreryHostResult::NullArgument
        );
        assert_eq!(
            orrery_host_restore(handle, std::ptr::null(), 4),
            OrreryHostResult::NullArgument
        );
        assert_eq!(
            orrery_host_restore(handle, [0; 4].as_ptr(), 4),
            OrreryHostResult::MalformedInput
        );
        assert_eq!(
            orrery_host_step(handle, 1, std::ptr::null_mut(), std::ptr::null_mut()),
            OrreryHostResult::Panic
        );
        assert!((*handle).is_poisoned());
        assert_eq!(
            orrery_host_next_tick(handle, &mut tick),
            OrreryHostResult::Poisoned
        );
        assert_eq!(orrery_host_destroy(handle), OrreryHostResult::Ok);
    }
}
