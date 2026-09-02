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
//! That shape is expressible across a C ABI, and [`abi`] exports it across
//! one.  A foreign caller owns an opaque handle, passes `(bytes, len)` command
//! buffers to [`SimulationHost::submit_command_bytes`], calls `step(ticks)`,
//! then copies the buffers returned by [`SimulationHost::drain_event_bytes`]
//! and [`SimulationHost::collect_output_bytes`].  No callback or Rust lifetime
//! needs to cross that boundary.
//!
//! The host also rewinds.  [`SimulationHost::snapshot`] captures every
//! installed entity as one per-entity record keyed by [`PersistId`] — the
//! D47 (b) grain — and [`SimulationHost::restore`] puts that population back
//! field-exactly, so a predicting consumer can snapshot, step forward, restore
//! and replay from the same bytes on either side of the ABI.

#![warn(missing_docs)]

pub mod abi;
pub mod ecs;

use std::collections::BTreeMap;

use orrery_core::{CoreCodec, Executor, Ruleset, SealedTickInputs, SteppedEntity, TickBackend};
use orrery_protocol::{PersistId, RulesetId, Tick, UniverseSeed};

const PERSIST_ID_BYTES: usize = size_of::<u64>();
const TICK_BYTES: usize = size_of::<u64>();
const LENGTH_BYTES: usize = size_of::<u32>();

/// The snapshot byte format this crate writes and accepts.
///
/// Bumped when the layout below changes; a snapshot carrying another version
/// is refused as [`HostError::MalformedSnapshot`] rather than misread.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

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
    /// Canonical state bytes offered for installation did not decode.
    MalformedState,
    /// A snapshot buffer was truncated, carried another format version, or
    /// held a record that does not decode.  Nothing was restored.
    MalformedSnapshot,
    /// A snapshot was taken under a different [`RulesetId`] than the host
    /// runs.  Nothing was restored.
    SnapshotRulesetMismatch,
}

/// One entity's snapshotted canonical state, keyed by its stable id in a
/// [`HostSnapshot`].
///
/// A record carries the two things the kernel keeps per entity: the quantized
/// canonical bytes, and the tick the state was observed at, which the
/// executor consults when the entity is read as a neighbour under
/// [`Ruleset::max_neighbor_staleness_ticks`].  Restoring without the second
/// would be field-exact on the bytes and still diverge on the next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySnapshot {
    observed_tick: Tick,
    canonical: Vec<u8>,
}

impl EntitySnapshot {
    /// The tick this state was observed at.
    #[must_use]
    pub const fn observed_tick(&self) -> Tick {
        self.observed_tick
    }

    /// The quantized canonical bytes.
    #[must_use]
    pub fn canonical(&self) -> &[u8] {
        &self.canonical
    }
}

/// A rewind point: the host's clock, every installed entity as its own
/// [`EntitySnapshot`] ordered by [`PersistId`], and the inputs queued for the
/// next tick.
///
/// This is the per-entity set D47 (b) names as the rollback unit.  A
/// consumer that predicts a subset keeps one of these per tick in its own
/// ring and may [`Self::remove`] entities it does not predict; a restore is
/// all-or-nothing over whatever the snapshot holds (D47 (e)).
///
/// Queued inputs are part of the point because the host itself produces
/// some of them: an event emitted on tick `T` is routed by the adapter into
/// an input for `T + 1`, and a snapshot taken between the two that dropped
/// it would step `T + 1` from a sealed set the original run never had.  The
/// consumer's own commands queued at that moment travel with them.  A
/// consumer replaying its input history after a restore therefore replays
/// only what it submitted *after* the snapshot.
///
/// Flat encoding, little-endian throughout:
/// `[format version: u32] [ruleset version: u32] [ruleset digest: 32 bytes]`
/// `[next tick: u64] [entity count: u64]`, then per entity
/// `[PersistId: u64] [observed tick: u64] [state length: u32] [state bytes]`,
/// then `[recipient count: u64]` and per recipient, ascending,
/// `[PersistId: u64] [input count: u32]` followed by that many
/// `[input length: u32] [input bytes]` in submission order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshot {
    ruleset: RulesetId,
    next_tick: Tick,
    entities: BTreeMap<PersistId, EntitySnapshot>,
    queued_inputs: BTreeMap<PersistId, Vec<Vec<u8>>>,
}

impl HostSnapshot {
    /// The tick the host will execute next once this snapshot is restored.
    #[must_use]
    pub const fn next_tick(&self) -> Tick {
        self.next_tick
    }

    /// The ruleset identity the snapshot was taken under.
    #[must_use]
    pub const fn ruleset(&self) -> RulesetId {
        self.ruleset
    }

    /// Every snapshotted entity, ascending by id.
    pub fn entities(&self) -> impl Iterator<Item = (PersistId, &EntitySnapshot)> {
        self.entities
            .iter()
            .map(|(entity, record)| (*entity, record))
    }

    /// Whether the snapshot carries no entities.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// How many entities the snapshot carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Drop one entity from the snapshot.
    ///
    /// Restoring the trimmed snapshot removes that entity from the host: the
    /// population restored is exactly the population snapshotted.
    pub fn remove(&mut self, entity: PersistId) -> Option<EntitySnapshot> {
        self.entities.remove(&entity)
    }

    /// Encode to the flat format documented on the type.
    ///
    /// # Errors
    ///
    /// [`HostError::BufferTooLarge`] if one state exceeds the `u32` length
    /// field.
    pub fn to_bytes(&self) -> Result<Vec<u8>, HostError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SNAPSHOT_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.ruleset.version.to_le_bytes());
        bytes.extend_from_slice(&self.ruleset.digest);
        bytes.extend_from_slice(&self.next_tick.0.to_le_bytes());
        let count = u64::try_from(self.entities.len()).map_err(|_| HostError::BufferTooLarge)?;
        bytes.extend_from_slice(&count.to_le_bytes());
        for (entity, record) in &self.entities {
            bytes.extend_from_slice(&entity.0.to_le_bytes());
            bytes.extend_from_slice(&record.observed_tick.0.to_le_bytes());
            append_length_prefixed(&mut bytes, &record.canonical)?;
        }
        let recipients =
            u64::try_from(self.queued_inputs.len()).map_err(|_| HostError::BufferTooLarge)?;
        bytes.extend_from_slice(&recipients.to_le_bytes());
        for (recipient, inputs) in &self.queued_inputs {
            bytes.extend_from_slice(&recipient.0.to_le_bytes());
            let count = u32::try_from(inputs.len()).map_err(|_| HostError::BufferTooLarge)?;
            bytes.extend_from_slice(&count.to_le_bytes());
            for input in inputs {
                append_length_prefixed(&mut bytes, input)?;
            }
        }
        Ok(bytes)
    }

    /// The inputs queued for the next tick, by recipient, in submission
    /// order, as canonical bytes.
    pub fn queued_inputs(&self) -> impl Iterator<Item = (PersistId, &[Vec<u8>])> {
        self.queued_inputs
            .iter()
            .map(|(recipient, inputs)| (*recipient, inputs.as_slice()))
    }

    /// Decode from the flat format documented on the type.
    ///
    /// Decoding checks framing only; the state bytes are decoded by the host
    /// that restores them, because only it knows the ruleset's state type.
    ///
    /// # Errors
    ///
    /// [`HostError::MalformedSnapshot`] for a truncated buffer, trailing
    /// bytes, another format version, or a non-ascending entity order.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, HostError> {
        let mut reader = SnapshotReader { bytes, at: 0 };
        if reader.u32()? != SNAPSHOT_FORMAT_VERSION {
            return Err(HostError::MalformedSnapshot);
        }
        let version = reader.u32()?;
        let digest: [u8; 32] = reader
            .take(32)?
            .try_into()
            .map_err(|_| HostError::MalformedSnapshot)?;
        let next_tick = Tick::new(reader.u64()?);
        let count = reader.u64()?;
        let mut entities = BTreeMap::new();
        let mut previous: Option<PersistId> = None;
        for _ in 0..count {
            let entity = PersistId::new(reader.u64()?);
            if previous.is_some_and(|last| last >= entity) {
                return Err(HostError::MalformedSnapshot);
            }
            previous = Some(entity);
            let observed_tick = Tick::new(reader.u64()?);
            let length =
                usize::try_from(reader.u32()?).map_err(|_| HostError::MalformedSnapshot)?;
            let canonical = reader.take(length)?.to_vec();
            entities.insert(
                entity,
                EntitySnapshot {
                    observed_tick,
                    canonical,
                },
            );
        }
        let recipients = reader.u64()?;
        let mut queued_inputs = BTreeMap::new();
        let mut previous: Option<PersistId> = None;
        for _ in 0..recipients {
            let recipient = PersistId::new(reader.u64()?);
            if previous.is_some_and(|last| last >= recipient) {
                return Err(HostError::MalformedSnapshot);
            }
            previous = Some(recipient);
            let count = reader.u32()?;
            let mut inputs = Vec::new();
            for _ in 0..count {
                let length =
                    usize::try_from(reader.u32()?).map_err(|_| HostError::MalformedSnapshot)?;
                inputs.push(reader.take(length)?.to_vec());
            }
            queued_inputs.insert(recipient, inputs);
        }
        if reader.at != bytes.len() {
            return Err(HostError::MalformedSnapshot);
        }
        Ok(Self {
            ruleset: RulesetId { version, digest },
            next_tick,
            entities,
            queued_inputs,
        })
    }
}

struct SnapshotReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl SnapshotReader<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8], HostError> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(HostError::MalformedSnapshot)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(HostError::MalformedSnapshot)?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, HostError> {
        let raw: [u8; LENGTH_BYTES] = self
            .take(LENGTH_BYTES)?
            .try_into()
            .map_err(|_| HostError::MalformedSnapshot)?;
        Ok(u32::from_le_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, HostError> {
        let raw: [u8; TICK_BYTES] = self
            .take(TICK_BYTES)?
            .try_into()
            .map_err(|_| HostError::MalformedSnapshot)?;
        Ok(u64::from_le_bytes(raw))
    }
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
pub struct SimulationHost<R: Ruleset, A: RulesetAdapter<R>, B = Executor<R>> {
    backend: B,
    adapter: A,
    next_tick: Tick,
    pending_inputs: BTreeMap<PersistId, PendingInputs<R::CoreInput>>,
    emitted_events: Vec<EmittedEvent>,
    /// The observation stamps of entities installed since the last executed
    /// tick — the stamp [`TickBackend::insert_observed`] takes and the kernel
    /// keeps but does not read back out.  The host mirrors it so a snapshot
    /// can carry it.  Every executed tick stamps the whole population to
    /// `tick + 1` on both backends, stepped and materialized alike, so after
    /// a tick this map is empty and [`Self::stepped_at`] answers instead.
    installed_at: BTreeMap<PersistId, Tick>,
    /// The stamp every entity not in [`Self::installed_at`] carries: the
    /// `next_tick` after the last executed tick, or `None` before any tick.
    stepped_at: Option<Tick>,
}

impl<R: Ruleset, A: RulesetAdapter<R>> SimulationHost<R, A, Executor<R>> {
    /// Create a host on the canonical [`Executor`] store and start its
    /// explicit lifetime at `config.first_tick`.
    #[must_use]
    pub fn new(config: SimulationHostConfig, ruleset: R, adapter: A) -> Self {
        Self::on_backend(config, Executor::new(ruleset, config.seed), adapter)
    }

    /// Consume the host and return its executor at the end of this host
    /// lifetime.  This is the only API that transfers its canonical storage.
    #[must_use]
    pub fn into_executor(self) -> Executor<R> {
        self.backend
    }
}

impl<R: Ruleset, A: RulesetAdapter<R>, B: TickBackend<R>> SimulationHost<R, A, B> {
    /// Create a host over an explicit storage-and-scheduling substrate.
    ///
    /// The substrate is a [`TickBackend`]: it owns where canonical state
    /// lives, the entity iteration order, and what schedules a tick. It does
    /// not own — and structurally cannot own — the canonical arithmetic, which
    /// is `orrery_core::canonical_step` in both backends. That is the whole of
    /// D42 (d)'s "behind the seam": swapping this changes storage, never bytes.
    #[must_use]
    pub fn on_backend(config: SimulationHostConfig, backend: B, adapter: A) -> Self {
        Self {
            backend,
            adapter,
            next_tick: config.first_tick,
            pending_inputs: BTreeMap::new(),
            emitted_events: Vec::new(),
            installed_at: BTreeMap::new(),
            stepped_at: None,
        }
    }

    /// The identity of the rules this host runs.
    ///
    /// A foreign consumer that decodes state bytes itself compares this
    /// against the identity it was built for, so a codec drift fails at
    /// creation instead of misreading fields.
    #[must_use]
    pub fn ruleset_id(&self) -> RulesetId {
        self.backend.ruleset().id()
    }

    /// Consume the host and return its substrate.
    #[must_use]
    pub fn into_backend(self) -> B {
        self.backend
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
        self.install_state_observed(entity, state, Tick::new(0));
    }

    /// Install canonical state carrying the tick it was observed at.
    ///
    /// Replication consumers use this form: a state decoded from an
    /// authority's claim at tick `T` is observed at `T`, and neighbours read
    /// it under the ruleset's staleness bound from that stamp.
    pub fn install_state_observed(
        &mut self,
        entity: PersistId,
        state: R::CoreState,
        observed_tick: Tick,
    ) {
        self.backend.insert_observed(entity, state, observed_tick);
        self.installed_at.insert(entity, observed_tick);
    }

    /// Install canonical state from its flat bytes.
    ///
    /// This is the C-ABI-facing form of [`Self::install_state_observed`]: the
    /// bytes are what [`Self::state_bytes`] and the output buffer hand out,
    /// so a consumer can feed replicated or recorded state back in without
    /// naming the state type.
    ///
    /// # Errors
    ///
    /// [`HostError::MalformedState`] if the bytes do not decode.
    pub fn install_state_bytes(
        &mut self,
        entity: PersistId,
        observed_tick: Tick,
        bytes: &[u8],
    ) -> Result<(), HostError> {
        let state = R::CoreState::decode(bytes).map_err(|_| HostError::MalformedState)?;
        self.install_state_observed(entity, state, observed_tick);
        Ok(())
    }

    /// Remove one entity and return its state, or `None` if it was absent.
    pub fn remove_state(&mut self, entity: PersistId) -> Option<R::CoreState> {
        self.installed_at.remove(&entity);
        self.pending_inputs.remove(&entity);
        self.backend.take_state(entity)
    }

    /// The tick one installed entity's state was observed at.
    #[must_use]
    pub fn observed_tick(&self, entity: PersistId) -> Option<Tick> {
        self.installed_at
            .get(&entity)
            .copied()
            .or(self.stepped_at)
            .filter(|_| self.backend.state(entity).is_some())
    }

    /// Capture the host's clock, every installed entity's quantized canonical
    /// state as one record per entity keyed by [`PersistId`], and the inputs
    /// queued for the next tick.
    ///
    /// Undrained events are not part of a snapshot: they are already the
    /// consumer's output and stay in the drain buffer across a restore.
    ///
    /// # Panics
    ///
    /// If the backend's entity index and state store disagree, which no
    /// backend in this crate permits.
    #[must_use]
    pub fn snapshot(&self) -> HostSnapshot {
        let queued_inputs = self
            .pending_inputs
            .iter()
            .map(|(recipient, pending)| {
                let inputs = pending.inputs.iter().map(CoreCodec::to_canonical).collect();
                (*recipient, inputs)
            })
            .collect();
        let entities = self
            .backend
            .entities()
            .into_iter()
            .map(|entity| {
                let state = self
                    .backend
                    .state(entity)
                    .expect("backend entity index and state store agree");
                let observed_tick = self
                    .installed_at
                    .get(&entity)
                    .copied()
                    .or(self.stepped_at)
                    .unwrap_or(Tick::new(0));
                (
                    entity,
                    EntitySnapshot {
                        observed_tick,
                        canonical: state.to_canonical(),
                    },
                )
            })
            .collect();
        HostSnapshot {
            ruleset: self.ruleset_id(),
            next_tick: self.next_tick,
            entities,
            queued_inputs,
        }
    }

    /// Put a snapshot's population back, all or nothing.
    ///
    /// After a successful restore the host holds exactly the snapshot's
    /// entities — entities installed since are removed — each with its
    /// snapshotted bytes and observation tick, the next tick is the
    /// snapshot's, and the queued inputs are the snapshot's: anything queued
    /// since is dropped.  Stepping the restored host with the same inputs
    /// submitted after the snapshot reproduces the same state hashes and the
    /// same output bytes as the original run, which is the guarantee
    /// prediction needs and the one this crate's tests assert.
    ///
    /// The exactness rests on two contracts the kernel already imposes on
    /// every ruleset: [`CoreCodec`] round-trips, and
    /// [`orrery_core::Quantized::quantize`] is idempotent on state it already
    /// produced — the executor re-quantizes on every install, so a state
    /// that moved when re-quantized would already break the executor's own
    /// install-then-hash path.
    ///
    /// # Errors
    ///
    /// [`HostError::SnapshotRulesetMismatch`] if the snapshot names another
    /// ruleset; [`HostError::MalformedSnapshot`] if any record fails to
    /// decode.  In both cases the host is untouched.
    pub fn restore(&mut self, snapshot: &HostSnapshot) -> Result<(), HostError> {
        if snapshot.ruleset != self.ruleset_id() {
            return Err(HostError::SnapshotRulesetMismatch);
        }
        let mut decoded = Vec::with_capacity(snapshot.entities.len());
        for (entity, record) in &snapshot.entities {
            let state = R::CoreState::decode(&record.canonical)
                .map_err(|_| HostError::MalformedSnapshot)?;
            decoded.push((*entity, record.observed_tick, state));
        }
        let mut queued: BTreeMap<PersistId, PendingInputs<R::CoreInput>> = BTreeMap::new();
        for (recipient, inputs) in &snapshot.queued_inputs {
            let mut pending = PendingInputs::default();
            for input in inputs {
                pending
                    .push(R::CoreInput::decode(input).map_err(|_| HostError::MalformedSnapshot)?);
            }
            queued.insert(*recipient, pending);
        }

        for entity in self.backend.entities() {
            if !snapshot.entities.contains_key(&entity) {
                self.backend.take_state(entity);
            }
        }
        self.installed_at.clear();
        self.stepped_at = None;
        for (entity, observed_tick, state) in decoded {
            self.backend.insert_observed(entity, state, observed_tick);
            self.installed_at.insert(entity, observed_tick);
        }
        self.next_tick = snapshot.next_tick;
        self.pending_inputs = queued;
        Ok(())
    }

    /// Look up the current canonical bytes for one stable id.
    ///
    /// This is a C-ABI-friendly lookup: foreign callers receive only owned
    /// canonical bytes, never a Rust reference into the host.
    #[must_use]
    pub fn state_bytes(&self, entity: PersistId) -> Option<Vec<u8>> {
        self.backend.state(entity).map(CoreCodec::to_canonical)
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
            let queued = std::mem::take(&mut self.pending_inputs);
            let mut sealed = SealedTickInputs::new();
            for (target, pending) in queued {
                sealed.extend(target, pending.inputs);
            }
            // The backend snapshots its population at the tick boundary and
            // steps it in canonical PersistId order.  Materializations happen
            // inside a step but are not in that snapshot, so cannot step this
            // tick.
            for SteppedEntity { entity, outcome } in self.backend.step_tick(tick, &sealed) {
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
            // Every entity present at the boundary stepped and every
            // materialization settled at T+1, on both backends; the mirror
            // follows.  An entity installed since the last tick was stepped
            // too, so its install stamp is superseded.
            self.installed_at.clear();
            self.stepped_at = Some(self.next_tick);
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
        let buffer = self.peek_event_bytes()?;
        self.clear_events();
        Ok(buffer)
    }

    /// Encode emitted events without draining them.
    ///
    /// A caller-owned-buffer boundary sizes its copy from this and clears
    /// with [`Self::clear_events`] only once the copy succeeded, so a buffer
    /// that turned out too small loses nothing.
    ///
    /// # Errors
    ///
    /// [`HostError::BufferTooLarge`] if one event exceeds the `u32` length
    /// field.
    pub fn peek_event_bytes(&self) -> Result<EventBuffer, HostError> {
        let bytes = encode_event_records(&self.emitted_events)?;
        Ok(EventBuffer { bytes })
    }

    /// Discard every emitted event not yet drained.
    pub fn clear_events(&mut self) {
        self.emitted_events.clear();
    }

    /// Collect current canonical states into one stable-id-ordered flat buffer.
    ///
    /// Collection is non-destructive; a presentation host can read it after
    /// every frame while canonical state remains solely inside the executor.
    pub fn collect_output_bytes(&self) -> Result<OutputBuffer, HostError> {
        let mut bytes = Vec::new();
        for entity in self.backend.entities() {
            let state = self
                .backend
                .state(entity)
                .expect("backend entity index and state store agree");
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
    bytes.extend_from_slice(&entity.to_le_bytes());
    append_length_prefixed(bytes, payload)
}

fn append_length_prefixed(bytes: &mut Vec<u8>, payload: &[u8]) -> Result<(), HostError> {
    let length = u32::try_from(payload.len()).map_err(|_| HostError::BufferTooLarge)?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use bevy_app::{App, Update};
    use bevy_ecs::prelude::{ResMut, Resource};
    use orrery_core::{CodecError, OrderedInputs, Quantized, StateView, StepOutput, TickRng};
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
