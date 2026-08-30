//! The engine-neutral, fixed-step [`SimulationHost`] seam.
//!
//! A host owns the existing [`Executor`] and nothing else canonical.  It
//! seals flat command bytes at the start of each explicit [`TickCount`], drives
//! the executor in stable [`PersistId`] order, routes emitted events through a
//! game-supplied [`RulesetAdapter`], and exposes events and state as flat
//! buffers.  It never reads a clock: a variable-rate caller owns any fixed-step
//! accumulator and calls [`SimulationHost::step`] with the exact number of
//! ticks to execute.
//!
//! That shape is deliberately expressible across a C ABI.  A foreign caller
//! owns an opaque `SimulationHost` handle, passes `(bytes, len)` command
//! buffers to [`SimulationHost::submit_command_bytes`], calls `step(ticks)`,
//! then copies the buffers returned by [`SimulationHost::drain_event_bytes`]
//! and [`SimulationHost::collect_output_bytes`].  No callback or Rust lifetime
//! needs to cross that boundary.

#![warn(missing_docs)]

use std::collections::BTreeMap;

use orrery_core::{CoreCodec, Executor, Ruleset};
use orrery_protocol::{PersistId, Tick, UniverseSeed};

const PERSIST_ID_BYTES: usize = size_of::<u64>();

/// Explicit construction parameters for one [`SimulationHost`] lifetime.
///
/// The caller chooses the first simulated tick and universe seed when it
/// creates the host.  The host owns the executor from construction until it is
/// consumed by [`SimulationHost::into_executor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationHostConfig {
    /// Seed used by the kernel's deterministic per-entity tick RNG.
    pub seed: UniverseSeed,
    /// Absolute tick executed by the first call to [`SimulationHost::step`].
    pub first_tick: Tick,
}

impl SimulationHostConfig {
    /// Construct configuration for a host whose first simulated tick is zero.
    #[must_use]
    pub const fn new(seed: UniverseSeed) -> Self {
        Self {
            seed,
            first_tick: Tick::new(0),
        }
    }

    /// Set the absolute tick which the host executes first.
    #[must_use]
    pub const fn starting_at(mut self, first_tick: Tick) -> Self {
        self.first_tick = first_tick;
        self
    }
}

/// An exact number of fixed ticks to execute.
///
/// This is intentionally not a duration.  A caller with a variable-rate frame
/// loop keeps its accumulator outside the host and turns it into this value;
/// the host never observes wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickCount(u64);

impl TickCount {
    /// Construct an exact fixed-tick count.
    #[must_use]
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    /// Return the number of fixed ticks to execute.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A game-specific bridge for the part of event delivery that the frozen
/// [`Ruleset`] trait deliberately does not own.
///
/// The adapter routes an emitted event into a sealed input for a later tick.
/// It cannot access the executor or mutate canonical state, so the existing
/// executor remains the implementation of D43's canonical stages, including
/// quantization before hashing.
pub trait RulesetAdapter<R: Ruleset>: Send + Sync + 'static {
    /// Return the next-tick delivery for `event`, or `None` when it is only an
    /// externally observable event.
    fn deliver(&self, event: &R::CoreEvent) -> Option<Delivery<R::CoreInput>>;
}

/// One named, next-tick input delivery produced from an emitted event.
///
/// A named record keeps the stable recipient and its input together without a
/// positional tuple at the host boundary.
pub struct Delivery<I> {
    recipient: PersistId,
    input: I,
}

impl<I> Delivery<I> {
    /// Construct one delivery to `recipient`.
    #[must_use]
    pub const fn new(recipient: PersistId, input: I) -> Self {
        Self { recipient, input }
    }
}

/// A routing adapter for rulesets whose emitted events have no next-tick
/// recipient.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEventRouting;

impl<R: Ruleset> RulesetAdapter<R> for NoEventRouting {
    fn deliver(&self, _event: &R::CoreEvent) -> Option<Delivery<R::CoreInput>> {
        None
    }
}

/// An error decoding or assembling a host flat buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    /// A command buffer lacked its stable id or contained invalid canonical
    /// input bytes.
    MalformedCommand,
    /// An event or state record exceeds the fixed-width `u32` length field of
    /// the C-friendly buffer format.
    BufferTooLarge,
}

/// One state hash produced by an executed canonical tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateHash {
    /// Stable entity identity whose state was advanced.
    pub entity: PersistId,
    /// Absolute tick on which the entity was advanced.
    pub tick: Tick,
    /// Hash of that entity's quantized canonical state.
    pub hash: [u8; 32],
}

/// The work completed by one explicit [`SimulationHost::step`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    /// The first tick this call executed, or the host's current tick for zero
    /// requested ticks.
    pub first_tick: Tick,
    /// The tick that will execute on the next call after this report.
    pub next_tick: Tick,
    /// Hashes in canonical execution order: tick ascending, then `PersistId`
    /// ascending within a tick.
    pub state_hashes: Vec<StateHash>,
}

/// A caller-owned contiguous event buffer.
///
/// Its record format is repeated `[source PersistId: u64 little-endian]`
/// `[event length: u32 little-endian] [event canonical bytes]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBuffer {
    bytes: Vec<u8>,
}

impl EventBuffer {
    /// Borrow the flat event bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume this wrapper and return its caller-owned bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Whether no event records were collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// A caller-owned contiguous canonical-state output buffer.
///
/// Its record format is repeated `[entity PersistId: u64 little-endian]`
/// `[state length: u32 little-endian] [state canonical bytes]`, ordered by
/// `PersistId` ascending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBuffer {
    bytes: Vec<u8>,
}

impl OutputBuffer {
    /// Borrow the flat state-output bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume this wrapper and return its caller-owned bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

struct PendingInputs<I> {
    inputs: Vec<I>,
}

impl<I> PendingInputs<I> {
    fn push(&mut self, input: I) {
        self.inputs.push(input);
    }
}

impl<I> Default for PendingInputs<I> {
    fn default() -> Self {
        Self { inputs: Vec::new() }
    }
}

struct QueuedInput<I> {
    target: PersistId,
    input: I,
}

impl<I> QueuedInput<I> {
    const fn new(target: PersistId, input: I) -> Self {
        Self { target, input }
    }
}

struct EmittedEvent {
    source: PersistId,
    canonical: Vec<u8>,
}

/// The kernel-owned fixed-step driver shared by headless and engine hosts.
///
/// The host owns one lifetime of the existing executor.  It owns no wall-clock
/// accumulator and no presentation state: callers explicitly submit flat
/// commands, call [`Self::step`], then collect flat canonical outputs.
pub struct SimulationHost<R: Ruleset, A: RulesetAdapter<R>> {
    executor: Executor<R>,
    adapter: A,
    next_tick: Tick,
    pending_inputs: BTreeMap<PersistId, PendingInputs<R::CoreInput>>,
    emitted_events: Vec<EmittedEvent>,
}

impl<R: Ruleset, A: RulesetAdapter<R>> SimulationHost<R, A> {
    /// Create a host and start its explicit lifetime at `config.first_tick`.
    #[must_use]
    pub fn new(config: SimulationHostConfig, ruleset: R, adapter: A) -> Self {
        Self {
            executor: Executor::new(ruleset, config.seed),
            adapter,
            next_tick: config.first_tick,
            pending_inputs: BTreeMap::new(),
            emitted_events: Vec::new(),
        }
    }

    /// Consume the host and return its executor at the end of this host
    /// lifetime.  This is the only API that transfers its canonical storage.
    #[must_use]
    pub fn into_executor(self) -> Executor<R> {
        self.executor
    }

    /// The absolute tick that the next [`Self::step`] call will execute.
    #[must_use]
    pub const fn next_tick(&self) -> Tick {
        self.next_tick
    }

    /// Install canonical state under its stable id.
    ///
    /// The executor quantizes this state before it is available to any tick.
    /// Hosts use this for deterministic setup or decoded replication; it does
    /// not put canonical state in an engine application world.
    pub fn install_state(&mut self, entity: PersistId, state: R::CoreState) {
        self.executor.insert(entity, state);
    }

    /// Look up the current canonical bytes for one stable id.
    ///
    /// This is a C-ABI-friendly lookup: foreign callers receive only owned
    /// canonical bytes, never a Rust reference into the host.
    #[must_use]
    pub fn state_bytes(&self, entity: PersistId) -> Option<Vec<u8>> {
        self.executor.state(entity).map(CoreCodec::to_canonical)
    }

    /// Queue one flat command for the next fixed tick.
    ///
    /// The format is `[target PersistId: u64 little-endian] [CoreInput
    /// canonical bytes]`.  A C ABI exposes this as `(const uint8_t *, size_t)`;
    /// the caller retains ownership of the source buffer for the call.
    pub fn submit_command_bytes(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        let target_bytes = bytes
            .get(..PERSIST_ID_BYTES)
            .ok_or(HostError::MalformedCommand)?;
        let target = PersistId::new(u64::from_le_bytes(
            target_bytes
                .try_into()
                .map_err(|_| HostError::MalformedCommand)?,
        ));
        let input = R::CoreInput::decode(&bytes[PERSIST_ID_BYTES..])
            .map_err(|_| HostError::MalformedCommand)?;
        self.queue_input(QueuedInput::new(target, input));
        Ok(())
    }

    /// Advance exactly `ticks` canonical ticks, and never read wall time.
    ///
    /// Inputs already queued at the call boundary are sealed for the first
    /// tick.  Events emitted while a tick runs are queued only after sealing,
    /// so their adapter deliveries can become inputs no earlier than the next
    /// tick, as D43 requires.
    pub fn step(&mut self, ticks: TickCount) -> StepReport {
        let first_tick = self.next_tick;
        let mut state_hashes = Vec::new();

        for _ in 0..ticks.get() {
            let tick = self.next_tick;
            // S0: all externally queued input becomes immutable for this tick.
            let mut sealed_inputs = std::mem::take(&mut self.pending_inputs);
            // The executor's BTreeMap is the D44 stable-id index and yields
            // canonical PersistId order.  Materializations happen inside each
            // step but are not in this snapshot, so cannot step this tick.
            let entities: Vec<PersistId> = self.executor.entities().copied().collect();
            for entity in entities {
                let inputs = sealed_inputs.remove(&entity).unwrap_or_default();
                let Some(outcome) = self.executor.step_entity(entity, tick, &inputs.inputs) else {
                    continue;
                };

                state_hashes.push(StateHash {
                    entity,
                    tick,
                    hash: outcome.state_hash,
                });
                for event in outcome.events {
                    if let Some(delivery) = self.adapter.deliver(&event) {
                        self.queue_input(QueuedInput::new(delivery.recipient, delivery.input));
                    }
                    self.emitted_events.push(EmittedEvent {
                        source: entity,
                        canonical: event.to_canonical(),
                    });
                }
            }
            self.next_tick = Tick::new(self.next_tick.0.saturating_add(1));
        }

        StepReport {
            first_tick,
            next_tick: self.next_tick,
            state_hashes,
        }
    }

    /// Drain emitted events into one owned flat buffer.
    ///
    /// If an event cannot fit the specified `u32` length field, no event is
    /// drained and the caller can report the error without losing output.
    pub fn drain_event_bytes(&mut self) -> Result<EventBuffer, HostError> {
        let bytes = encode_event_records(&self.emitted_events)?;
        self.emitted_events.clear();
        Ok(EventBuffer { bytes })
    }

    /// Collect current canonical states into one stable-id-ordered flat buffer.
    ///
    /// Collection is non-destructive; a presentation host can read it after
    /// every frame while canonical state remains solely inside the executor.
    pub fn collect_output_bytes(&self) -> Result<OutputBuffer, HostError> {
        let mut bytes = Vec::new();
        for entity in self.executor.entities() {
            let state = self
                .executor
                .state(*entity)
                .expect("executor entity index and state store agree");
            append_record(&mut bytes, entity.0, &state.to_canonical())?;
        }
        Ok(OutputBuffer { bytes })
    }

    fn queue_input(&mut self, queued: QueuedInput<R::CoreInput>) {
        self.pending_inputs
            .entry(queued.target)
            .or_default()
            .push(queued.input);
    }
}

fn encode_event_records(events: &[EmittedEvent]) -> Result<Vec<u8>, HostError> {
    let mut bytes = Vec::new();
    for event in events {
        append_record(&mut bytes, event.source.0, &event.canonical)?;
    }
    Ok(bytes)
}

fn append_record(bytes: &mut Vec<u8>, entity: u64, payload: &[u8]) -> Result<(), HostError> {
    let length = u32::try_from(payload.len()).map_err(|_| HostError::BufferTooLarge)?;
    bytes.extend_from_slice(&entity.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use bevy_app::{App, Update};
    use bevy_ecs::prelude::{ResMut, Resource};
    use orrery_core::{
        CodecError, CoreClass, OrderedInputs, Quantized, StateView, StepOutput, TickRng,
    };
    use orrery_games::game::Game;
    use orrery_games::regolith::order::Order;
    use orrery_games::Regolith;
    use orrery_protocol::{RulesetId, UniverseSeed};

    use super::*;

    #[derive(Debug, Clone, Copy, Default)]
    struct RegolithAdapter;

    impl RulesetAdapter<Regolith> for RegolithAdapter {
        fn deliver(
            &self,
            event: &<Regolith as Ruleset>::CoreEvent,
        ) -> Option<Delivery<<Regolith as Ruleset>::CoreInput>> {
            Regolith::honest()
                .deliver(event)
                .map(|delivery| Delivery::new(delivery.0, delivery.1))
        }
    }

    fn host() -> SimulationHost<Regolith, RegolithAdapter> {
        let mut host = SimulationHost::new(
            SimulationHostConfig::new(UniverseSeed([0; 32])).starting_at(Tick::new(40)),
            Regolith::honest(),
            RegolithAdapter,
        );
        let entity = PersistId::new(7);
        host.install_state(entity, Regolith::honest().spawn(entity, 0));
        host
    }

    fn fire_command(entity: PersistId) -> Vec<u8> {
        let mut command = entity.0.to_le_bytes().to_vec();
        command.extend(Order::Fire.to_canonical());
        command
    }

    fn drive_one_tick(host: &mut SimulationHost<Regolith, RegolithAdapter>) -> StepReport {
        host.submit_command_bytes(&fire_command(PersistId::new(7)))
            .expect("canonical Regolith command decodes");
        host.step(TickCount::new(1))
    }

    #[derive(Resource)]
    struct BevyClientTestDouble {
        host: SimulationHost<Regolith, RegolithAdapter>,
        report: Option<StepReport>,
    }

    fn drive_client_host(mut client: ResMut<BevyClientTestDouble>) {
        client.report = Some(drive_one_tick(&mut client.host));
    }

    #[test]
    fn headless_and_bevy_client_test_double_drive_one_host_api() {
        let mut headless = host();
        let headless_report = drive_one_tick(&mut headless);
        let headless_events = headless
            .drain_event_bytes()
            .expect("events fit flat buffer");
        let headless_output = headless
            .collect_output_bytes()
            .expect("state fits flat buffer");

        let mut app = App::new();
        app.insert_resource(BevyClientTestDouble {
            host: host(),
            report: None,
        })
        .add_systems(Update, drive_client_host)
        .update();
        let mut client = app.world_mut().resource_mut::<BevyClientTestDouble>();
        let client_report = client.report.take().expect("system drove one tick");
        let client_events = client
            .host
            .drain_event_bytes()
            .expect("events fit flat buffer");
        let client_output = client
            .host
            .collect_output_bytes()
            .expect("state fits flat buffer");

        assert_eq!(client_report, headless_report);
        assert_eq!(client_events, headless_events);
        assert_eq!(client_output, headless_output);
        assert_eq!(headless_report.first_tick, Tick::new(40));
        assert_eq!(headless_report.next_tick, Tick::new(41));
        assert!(
            !headless_events.is_empty(),
            "Fire without a lock emits a canonical ShotRefused event"
        );
    }

    #[test]
    fn an_explicit_tick_count_is_independent_of_call_chunking() {
        let mut whole = host();
        let mut split = host();

        let whole_report = whole.step(TickCount::new(2));
        let first_split = split.step(TickCount::new(1));
        let second_split = split.step(TickCount::new(1));

        assert_eq!(whole.next_tick(), split.next_tick());
        assert_eq!(whole.collect_output_bytes(), split.collect_output_bytes());
        let split_hashes = first_split
            .state_hashes
            .into_iter()
            .chain(second_split.state_hashes)
            .collect::<Vec<_>>();
        assert_eq!(whole_report.state_hashes, split_hashes);
    }

    #[test]
    fn malformed_flat_commands_do_not_enter_the_sealed_input_log() {
        let mut host = host();
        assert_eq!(
            host.submit_command_bytes(&[0; PERSIST_ID_BYTES - 1]),
            Err(HostError::MalformedCommand)
        );
        let report = host.step(TickCount::new(1));
        assert_eq!(report.state_hashes.len(), 1);
        assert!(host
            .drain_event_bytes()
            .expect("empty event buffer")
            .is_empty());
    }

    const MICROMETRES_PER_MILLIMETRE: i64 = 1_000;
    const OFF_LATTICE_STEP: i64 = 1_234_567;
    const HOST_QUANTIZATION_GOLDEN: [u8; 32] = [
        45, 69, 174, 172, 189, 104, 83, 77, 72, 176, 229, 37, 48, 102, 139, 140, 176, 168, 236, 19,
        135, 25, 175, 152, 170, 3, 185, 58, 218, 78, 34, 52,
    ];

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct OffLatticeState {
        position_um: i64,
    }

    impl Quantized for OffLatticeState {
        fn quantize(&mut self) {
            let magnitude = (self.position_um.abs() + MICROMETRES_PER_MILLIMETRE / 2)
                / MICROMETRES_PER_MILLIMETRE
                * MICROMETRES_PER_MILLIMETRE;
            self.position_um = magnitude * self.position_um.signum();
        }
    }

    impl CoreCodec for OffLatticeState {
        fn encode(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.position_um.to_le_bytes());
        }

        fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
            let raw: [u8; 8] = bytes
                .try_into()
                .map_err(|_| CodecError("off-lattice state is 8 bytes"))?;
            Ok(Self {
                position_um: i64::from_le_bytes(raw),
            })
        }
    }

    #[derive(Clone)]
    enum NoInputOrEvent {}

    impl CoreCodec for NoInputOrEvent {
        fn encode(&self, _out: &mut Vec<u8>) {
            match *self {}
        }

        fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
            Err(CodecError("no input or event is valid"))
        }
    }

    struct OffLatticeRuleset;

    impl Ruleset for OffLatticeRuleset {
        type CoreState = OffLatticeState;
        type CoreInput = NoInputOrEvent;
        type CoreEvent = NoInputOrEvent;

        fn id(&self) -> RulesetId {
            RulesetId {
                version: 1,
                digest: [0xA5; 32],
            }
        }

        fn classify_component(&self, _component: orrery_core::ComponentTypeId) -> CoreClass {
            CoreClass::Core
        }

        fn step(
            &self,
            view: &mut StateView<'_, Self::CoreState>,
            _inputs: &OrderedInputs<'_, Self::CoreInput>,
            _rng: &mut TickRng,
        ) -> StepOutput<Self::CoreEvent> {
            view.own_mut().position_um += OFF_LATTICE_STEP;
            StepOutput::default()
        }
    }

    #[test]
    fn chains_match_the_committed_golden() {
        let entity = PersistId::new(9);
        let mut host = SimulationHost::new(
            SimulationHostConfig::new(UniverseSeed([0x42; 32])),
            OffLatticeRuleset,
            NoEventRouting,
        );
        host.install_state(entity, OffLatticeState { position_um: 5_000 });

        let report = host.step(TickCount::new(2));
        let chain = report.state_hashes.iter().fold([0; 32], |chain, state| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&chain);
            hasher.update(&state.hash);
            *hasher.finalize().as_bytes()
        });

        assert_eq!(chain, HOST_QUANTIZATION_GOLDEN);
    }
}
