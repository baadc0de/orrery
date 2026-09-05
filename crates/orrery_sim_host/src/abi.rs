//! The ruleset-generic C ABI over [`SimulationHost`].
//!
//! Every exported function here is non-generic and named in
//! `include/orrery_sim_host.h`, which mentions no ruleset, no state type and
//! no game.  The ruleset is erased behind [`OrreryHost`], an opaque handle a
//! game's own library produces from one factory function — the only symbol a
//! game adds — and everything after creation is this module.
//!
//! # How state crosses without types
//!
//! State crosses as the canonical bytes the kernel already commits to, in the
//! same `[PersistId: u64 LE] [length: u32 LE] [bytes]` framing the host uses
//! for its Rust callers.  The alternatives were weighed against what an
//! Unreal consumer would have to write:
//!
//! - **A fixed projection struct** (what the retired `orrery_sim` did with
//!   its craft transform) puts a game's field names in the header.  Adding a
//!   field to the game changes the ABI.  Rejected.
//! - **A caller-supplied projection callback** still has to hand the callback
//!   *something*, and the only ruleset-independent something is the bytes.
//!   It adds a C-to-Rust re-entry to catch panics around, for no gain.
//! - **A typed column schema** would need a second per-state declaration in
//!   Rust beside `CoreCodec`, and the two can drift silently, because the
//!   frozen kernel derives the codec from neither.
//!
//! So the consumer writes exactly one thing per state type: a C++ mirror of
//! its own `CoreCodec::decode`, which it already had to write in Rust.  The
//! header does not change when a field is added.  Drift between the two is
//! caught by two things: [`orrery_host_ruleset_id`], which a consumer checks
//! at creation against the identity it was compiled for, and the
//! cross-language fixture test in this crate, which decodes on the C side
//! bytes the live host produced.
//!
//! # Panics
//!
//! An unwind across `extern "C"` is undefined behaviour, so every entry point
//! runs under `catch_unwind` and reports [`OrreryHostResult::Panic`].  A panic
//! inside a mutating call leaves the host mid-tick, so the handle is then
//! poisoned: every later call but destroy returns
//! [`OrreryHostResult::Poisoned`].

#![deny(unsafe_op_in_unsafe_fn)]

use std::panic::{catch_unwind, AssertUnwindSafe};

use orrery_core::{Ruleset, TickBackend};
use orrery_protocol::{PersistId, RulesetId, Tick};

use crate::{
    EventBuffer, HostError, HostSnapshot, OutputBuffer, RulesetAdapter, SimulationHost, StateHash,
    StepReport, TickCount,
};

/// The ABI version `orrery_host_abi_version` reports; matches the header.
pub const ABI_VERSION: u32 = 1;

/// Result codes shared with `orrery_sim_host.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrreryHostResult {
    /// The call completed.
    Ok = 0,
    /// A required pointer was null.
    NullArgument = 1,
    /// A caller-owned output buffer was too small; `out_required` says by
    /// how much and nothing was written or drained.
    BufferTooSmall = 2,
    /// A command, state, or snapshot buffer was malformed, or a snapshot
    /// named another ruleset.  Nothing was applied.
    MalformedInput = 3,
    /// The named entity is not installed.
    NotFound = 4,
    /// A record does not fit the buffer format's `u32` length field.
    RecordTooLarge = 5,
    /// An earlier call panicked inside the host; only destroy is accepted.
    Poisoned = 6,
    /// A panic was caught at the boundary.  The host is poisoned if the call
    /// could have mutated it.
    Panic = 7,
}

impl From<HostError> for OrreryHostResult {
    fn from(error: HostError) -> Self {
        match error {
            HostError::MalformedCommand
            | HostError::MalformedState
            | HostError::MalformedSnapshot
            | HostError::SnapshotRulesetMismatch => Self::MalformedInput,
            HostError::BufferTooLarge => Self::RecordTooLarge,
            // No exported function calls `SimulationHost::seat_at`, so this
            // arm is unreached today; it exists only so this match stays
            // exhaustive over `HostError`. `MalformedInput` is the nearest
            // existing code if that ever changes: the call is refused and
            // nothing is applied, same as the other arms mapped here.
            HostError::HostAlreadyActive => Self::MalformedInput,
        }
    }
}

/// The ruleset identity, as a C record.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrreryHostRulesetId {
    /// Game-assigned monotonic rules version.
    pub version: u32,
    /// 32-byte build digest.
    pub digest: [u8; 32],
}

/// One state hash produced by an executed tick, as a C record.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrreryHostStateHash {
    /// The entity advanced.
    pub entity: u64,
    /// The tick that advanced it.
    pub tick: u64,
    /// blake3 of the entity's quantized canonical state after that tick.
    pub hash: [u8; 32],
}

/// The type-erased host operations the C ABI needs.
///
/// Implemented for every [`SimulationHost`]; a game never implements it.
pub trait ErasedHost {
    /// See [`SimulationHost::ruleset_id`].
    fn ruleset_id(&self) -> RulesetId;
    /// See [`SimulationHost::next_tick`].
    fn next_tick(&self) -> Tick;
    /// See [`SimulationHost::submit_command_bytes`].
    fn submit_command_bytes(&mut self, bytes: &[u8]) -> Result<(), HostError>;
    /// See [`SimulationHost::install_state_bytes`].
    fn install_state_bytes(
        &mut self,
        entity: PersistId,
        observed_tick: Tick,
        bytes: &[u8],
    ) -> Result<(), HostError>;
    /// See [`SimulationHost::remove_state`]; `true` if the entity existed.
    fn remove_state(&mut self, entity: PersistId) -> bool;
    /// See [`SimulationHost::step`].
    fn step(&mut self, ticks: TickCount) -> StepReport;
    /// See [`SimulationHost::drain_event_bytes`].
    fn drain_event_bytes(&mut self) -> Result<EventBuffer, HostError>;
    /// See [`SimulationHost::peek_event_bytes`].
    fn peek_event_bytes(&self) -> Result<EventBuffer, HostError>;
    /// See [`SimulationHost::clear_events`].
    fn clear_events(&mut self);
    /// See [`SimulationHost::collect_output_bytes`].
    fn collect_output_bytes(&self) -> Result<OutputBuffer, HostError>;
    /// See [`SimulationHost::state_bytes`].
    fn state_bytes(&self, entity: PersistId) -> Option<Vec<u8>>;
    /// See [`SimulationHost::snapshot`].
    fn snapshot(&self) -> HostSnapshot;
    /// See [`SimulationHost::restore`].
    fn restore(&mut self, snapshot: &HostSnapshot) -> Result<(), HostError>;
}

impl<R, A, B> ErasedHost for SimulationHost<R, A, B>
where
    R: Ruleset,
    A: RulesetAdapter<R>,
    B: TickBackend<R>,
{
    fn ruleset_id(&self) -> RulesetId {
        Self::ruleset_id(self)
    }

    fn next_tick(&self) -> Tick {
        Self::next_tick(self)
    }

    fn submit_command_bytes(&mut self, bytes: &[u8]) -> Result<(), HostError> {
        Self::submit_command_bytes(self, bytes)
    }

    fn install_state_bytes(
        &mut self,
        entity: PersistId,
        observed_tick: Tick,
        bytes: &[u8],
    ) -> Result<(), HostError> {
        Self::install_state_bytes(self, entity, observed_tick, bytes)
    }

    fn remove_state(&mut self, entity: PersistId) -> bool {
        Self::remove_state(self, entity).is_some()
    }

    fn step(&mut self, ticks: TickCount) -> StepReport {
        Self::step(self, ticks)
    }

    fn drain_event_bytes(&mut self) -> Result<EventBuffer, HostError> {
        Self::drain_event_bytes(self)
    }

    fn peek_event_bytes(&self) -> Result<EventBuffer, HostError> {
        Self::peek_event_bytes(self)
    }

    fn clear_events(&mut self) {
        Self::clear_events(self);
    }

    fn collect_output_bytes(&self) -> Result<OutputBuffer, HostError> {
        Self::collect_output_bytes(self)
    }

    fn state_bytes(&self, entity: PersistId) -> Option<Vec<u8>> {
        Self::state_bytes(self, entity)
    }

    fn snapshot(&self) -> HostSnapshot {
        Self::snapshot(self)
    }

    fn restore(&mut self, snapshot: &HostSnapshot) -> Result<(), HostError> {
        Self::restore(self, snapshot)
    }
}

/// The opaque handle a C caller owns: `orrery_host` in the header.
pub struct OrreryHost {
    host: Box<dyn ErasedHost>,
    hashes: Vec<StateHash>,
    poisoned: bool,
}

impl OrreryHost {
    /// Erase a host behind the handle type.
    #[must_use]
    pub fn new<H: ErasedHost + 'static>(host: H) -> Self {
        Self {
            host: Box::new(host),
            hashes: Vec::new(),
            poisoned: false,
        }
    }

    /// Transfer the handle to a foreign owner.  It comes back through
    /// [`orrery_host_destroy`].
    #[must_use]
    pub fn into_raw(self) -> *mut Self {
        Box::into_raw(Box::new(self))
    }

    /// Whether a panic has left this host unusable.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

/// The body of a game's factory function.
///
/// A game exports one `extern "C"` function that names its ruleset and
/// adapter, and calls this with the host it built.  Construction runs under
/// the same panic guard as every other entry point.
///
/// # Safety
///
/// `out_host` must be null or name writable storage for one handle pointer.
pub unsafe fn export_handle<H: ErasedHost + 'static>(
    out_host: *mut *mut OrreryHost,
    build: impl FnOnce() -> H,
) -> OrreryHostResult {
    // SAFETY: the caller supplies writable storage for one opaque handle, or
    // null, which becomes an error before anything is written.
    let Some(out_host) = (unsafe { out_host.as_mut() }) else {
        return OrreryHostResult::NullArgument;
    };
    match catch_unwind(AssertUnwindSafe(build)) {
        Ok(host) => {
            *out_host = OrreryHost::new(host).into_raw();
            OrreryHostResult::Ok
        }
        Err(_) => OrreryHostResult::Panic,
    }
}

fn with_host<F>(host: *mut OrreryHost, call: F) -> OrreryHostResult
where
    F: FnOnce(&mut OrreryHost) -> OrreryHostResult,
{
    // SAFETY: each exported function documents serialized calls on a live
    // handle produced by a factory; a null pointer becomes an error.
    let Some(host) = (unsafe { host.as_mut() }) else {
        return OrreryHostResult::NullArgument;
    };
    if host.poisoned {
        return OrreryHostResult::Poisoned;
    }
    match catch_unwind(AssertUnwindSafe(|| call(host))) {
        Ok(result) => result,
        Err(_) => {
            host.poisoned = true;
            OrreryHostResult::Panic
        }
    }
}

fn read_host<F>(host: *const OrreryHost, call: F) -> OrreryHostResult
where
    F: FnOnce(&OrreryHost) -> OrreryHostResult,
{
    // SAFETY: as `with_host`; a shared read forms only a shared reference.
    let Some(host) = (unsafe { host.as_ref() }) else {
        return OrreryHostResult::NullArgument;
    };
    if host.poisoned {
        return OrreryHostResult::Poisoned;
    }
    catch_unwind(AssertUnwindSafe(|| call(host))).unwrap_or(OrreryHostResult::Panic)
}

fn input_bytes<'a>(pointer: *const u8, len: usize) -> Result<&'a [u8], OrreryHostResult> {
    if len == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(OrreryHostResult::NullArgument);
    }
    // SAFETY: the C caller promises `pointer` names `len` readable bytes for
    // the duration of the call; null was rejected above.
    Ok(unsafe { std::slice::from_raw_parts(pointer, len) })
}

/// Copy `records` into caller storage under the `(out, capacity,
/// out_required)` convention every buffer-returning function shares.
fn copy_records<T: Copy>(
    records: &[T],
    out: *mut T,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    // SAFETY: the caller supplies writable storage for one `size_t`.
    let Some(out_required) = (unsafe { out_required.as_mut() }) else {
        return OrreryHostResult::NullArgument;
    };
    *out_required = records.len();
    if capacity < records.len() {
        return OrreryHostResult::BufferTooSmall;
    }
    if records.is_empty() {
        return OrreryHostResult::Ok;
    }
    if out.is_null() {
        return OrreryHostResult::NullArgument;
    }
    // SAFETY: the caller provided at least `records.len()` writable records,
    // established by the capacity check above, and `records` is a live slice.
    unsafe { std::ptr::copy_nonoverlapping(records.as_ptr(), out, records.len()) };
    OrreryHostResult::Ok
}

fn write_u64(out: *mut u64, value: u64) {
    // SAFETY: a null out-pointer means the caller does not want the value;
    // otherwise it names writable storage for one `uint64_t`.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = value;
    }
}

/// The ABI version this library was built with; compare with
/// `ORRERY_SIM_HOST_ABI_VERSION` before any other call.
#[no_mangle]
pub extern "C" fn orrery_host_abi_version() -> u32 {
    ABI_VERSION
}

/// Destroys a handle produced by a factory.
///
/// # Safety
///
/// `host` must be a live handle returned exactly once by a factory and not
/// used after this call.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_destroy(host: *mut OrreryHost) -> OrreryHostResult {
    if host.is_null() {
        return OrreryHostResult::NullArgument;
    }
    // SAFETY: the caller transfers back exactly one live handle and will not
    // use it afterwards.  Dropping a poisoned host is permitted: the erased
    // host's own drop runs under the guard so a second panic cannot unwind.
    catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(host) })))
        .map_or(OrreryHostResult::Panic, |()| OrreryHostResult::Ok)
}

/// Reads the identity of the rules the host runs.
///
/// # Safety
///
/// `host` must be live; `out_id` must name writable storage for one record.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_ruleset_id(
    host: *const OrreryHost,
    out_id: *mut OrreryHostRulesetId,
) -> OrreryHostResult {
    read_host(host, |host| {
        // SAFETY: the caller supplies writable storage for one record.
        let Some(out_id) = (unsafe { out_id.as_mut() }) else {
            return OrreryHostResult::NullArgument;
        };
        let id = host.host.ruleset_id();
        *out_id = OrreryHostRulesetId {
            version: id.version,
            digest: id.digest,
        };
        OrreryHostResult::Ok
    })
}

/// Reads the absolute tick the next step will execute.
///
/// # Safety
///
/// `host` must be live; `out_tick` must name writable storage.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_next_tick(
    host: *const OrreryHost,
    out_tick: *mut u64,
) -> OrreryHostResult {
    read_host(host, |host| {
        if out_tick.is_null() {
            return OrreryHostResult::NullArgument;
        }
        write_u64(out_tick, host.host.next_tick().0);
        OrreryHostResult::Ok
    })
}

/// Queues one flat command `[target PersistId: u64 LE] [input bytes]` for
/// the next step.
///
/// # Safety
///
/// `host` must be live and serialized; `bytes` must name `len` readable
/// bytes when `len` is nonzero.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_submit_command(
    host: *mut OrreryHost,
    bytes: *const u8,
    len: usize,
) -> OrreryHostResult {
    with_host(host, |host| {
        let bytes = match input_bytes(bytes, len) {
            Ok(bytes) => bytes,
            Err(error) => return error,
        };
        host.host
            .submit_command_bytes(bytes)
            .map_or_else(Into::into, |()| OrreryHostResult::Ok)
    })
}

/// Installs or replaces one entity's canonical state from its bytes,
/// observed at `observed_tick`.
///
/// # Safety
///
/// `host` must be live and serialized; `bytes` must name `len` readable
/// bytes when `len` is nonzero.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_install_state(
    host: *mut OrreryHost,
    entity: u64,
    observed_tick: u64,
    bytes: *const u8,
    len: usize,
) -> OrreryHostResult {
    with_host(host, |host| {
        let bytes = match input_bytes(bytes, len) {
            Ok(bytes) => bytes,
            Err(error) => return error,
        };
        host.host
            .install_state_bytes(PersistId::new(entity), Tick::new(observed_tick), bytes)
            .map_or_else(Into::into, |()| OrreryHostResult::Ok)
    })
}

/// Removes one entity.
///
/// # Safety
///
/// `host` must be live and serialized.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_remove_state(
    host: *mut OrreryHost,
    entity: u64,
) -> OrreryHostResult {
    with_host(host, |host| {
        if host.host.remove_state(PersistId::new(entity)) {
            OrreryHostResult::Ok
        } else {
            OrreryHostResult::NotFound
        }
    })
}

/// Advances exactly `ticks` fixed ticks and never reads wall time.
///
/// The state hashes each tick produced accumulate for
/// [`orrery_host_drain_state_hashes`].  Either out-pointer may be null.
///
/// # Safety
///
/// `host` must be live and serialized; non-null out-pointers must name
/// writable storage.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_step(
    host: *mut OrreryHost,
    ticks: u64,
    out_first_tick: *mut u64,
    out_next_tick: *mut u64,
) -> OrreryHostResult {
    with_host(host, |host| {
        let report = host.host.step(TickCount::new(ticks));
        write_u64(out_first_tick, report.first_tick.0);
        write_u64(out_next_tick, report.next_tick.0);
        host.hashes.extend(report.state_hashes);
        OrreryHostResult::Ok
    })
}

/// Drains the state hashes accumulated by steps into caller-owned records.
///
/// # Safety
///
/// `host` must be live and serialized; `out_required` must be writable; when
/// `capacity` suffices, `out_hashes` must name that many writable records.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_drain_state_hashes(
    host: *mut OrreryHost,
    out_hashes: *mut OrreryHostStateHash,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    with_host(host, |host| {
        let records: Vec<OrreryHostStateHash> = host
            .hashes
            .iter()
            .map(|hash| OrreryHostStateHash {
                entity: hash.entity.0,
                tick: hash.tick.0,
                hash: hash.hash,
            })
            .collect();
        let result = copy_records(&records, out_hashes, capacity, out_required);
        if result == OrreryHostResult::Ok {
            host.hashes.clear();
        }
        result
    })
}

/// Drains emitted events into a caller-owned flat buffer of
/// `[source PersistId: u64 LE] [length: u32 LE] [event bytes]` records.
///
/// # Safety
///
/// `host` must be live and serialized; `out_required` must be writable; when
/// `capacity` suffices, `out_bytes` must name that many writable bytes.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_drain_events(
    host: *mut OrreryHost,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    with_host(host, |host| {
        // Encode without draining first, so a too-small buffer loses nothing.
        let events = match host.host.peek_event_bytes() {
            Ok(events) => events,
            Err(error) => return error.into(),
        };
        let result = copy_records(events.as_bytes(), out_bytes, capacity, out_required);
        if result == OrreryHostResult::Ok {
            host.host.clear_events();
        }
        result
    })
}

/// Copies every entity's canonical state, ascending by id, as
/// `[PersistId: u64 LE] [length: u32 LE] [state bytes]` records.
///
/// # Safety
///
/// `host` must be live; `out_required` must be writable; when `capacity`
/// suffices, `out_bytes` must name that many writable bytes.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_collect_states(
    host: *const OrreryHost,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    read_host(host, |host| match host.host.collect_output_bytes() {
        Ok(output) => copy_records(output.as_bytes(), out_bytes, capacity, out_required),
        Err(error) => error.into(),
    })
}

/// Copies one entity's canonical state bytes.
///
/// # Safety
///
/// `host` must be live; `out_required` must be writable; when `capacity`
/// suffices, `out_bytes` must name that many writable bytes.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_state(
    host: *const OrreryHost,
    entity: u64,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    read_host(host, |host| {
        host.host
            .state_bytes(PersistId::new(entity))
            .map_or(OrreryHostResult::NotFound, |bytes| {
                copy_records(&bytes, out_bytes, capacity, out_required)
            })
    })
}

/// Copies a snapshot of the host's clock and every entity, in the flat
/// format documented on [`HostSnapshot`].
///
/// # Safety
///
/// `host` must be live; `out_required` must be writable; when `capacity`
/// suffices, `out_bytes` must name that many writable bytes.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_snapshot(
    host: *const OrreryHost,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    read_host(host, |host| match host.host.snapshot().to_bytes() {
        Ok(bytes) => copy_records(&bytes, out_bytes, capacity, out_required),
        Err(error) => error.into(),
    })
}

/// Restores a snapshot produced by [`orrery_host_snapshot`], all or nothing.
///
/// # Safety
///
/// `host` must be live and serialized; `bytes` must name `len` readable
/// bytes when `len` is nonzero.
#[no_mangle]
pub unsafe extern "C" fn orrery_host_restore(
    host: *mut OrreryHost,
    bytes: *const u8,
    len: usize,
) -> OrreryHostResult {
    with_host(host, |host| {
        let bytes = match input_bytes(bytes, len) {
            Ok(bytes) => bytes,
            Err(error) => return error,
        };
        let snapshot = match HostSnapshot::from_bytes(bytes) {
            Ok(snapshot) => snapshot,
            Err(error) => return error.into(),
        };
        match host.host.restore(&snapshot) {
            Ok(()) => {
                // Hashes from the abandoned timeline would be attributed to
                // ticks the restored host is about to execute again.
                host.hashes.clear();
                OrreryHostResult::Ok
            }
            Err(error) => error.into(),
        }
    })
}
