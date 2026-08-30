//! A deliberately small C ABI spike over the headless Regolith simulation.
//!
//! This crate is not `SimulationHost`: S5 owns that design.  It proves that
//! its required seam can be called from C without leaking Rust types, while
//! decoding the replication packets a campaign already emits.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use orrery_core::{CoreCodec, Executor};
use orrery_games::regolith::order::Order;
use orrery_games::regolith::state::RegolithState;
use orrery_games::Regolith;
use orrery_protocol::channels::{apply_delta_patch, decode_replication, decode_replication_delta};
use orrery_protocol::{CellId, PersistId, Tick, UniverseSeed};

const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Result codes shared with `orrery_sim.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrrerySimResult {
    /// The requested operation completed.
    Ok = 0,
    /// A required pointer was null.
    NullArgument = 1,
    /// A caller-owned output buffer was too small.
    BufferTooSmall = 2,
    /// A command or replication packet was malformed.
    MalformedInput = 3,
    /// A replication delta did not match the retained keyframe.
    UnanchoredDelta = 4,
    /// Rust caught a panic before it could cross the ABI boundary.
    Panic = 5,
}

/// A C-layout craft mirror suitable for a renderer.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrrerySimCraftTransform {
    /// Stable canonical entity identifier.
    pub craft_id: u64,
    /// Canonical world x position in millimetres.
    pub x_mm: i64,
    /// Canonical world y position in millimetres.
    pub y_mm: i64,
    /// Canonical world z position in millimetres.
    pub z_mm: i64,
    /// Canonical yaw in microradians.
    pub yaw_urad: i32,
    /// Canonical pitch in microradians.
    pub pitch_urad: i32,
}

#[derive(Debug, Clone)]
struct ReplicationKeyframe {
    canonical: Vec<u8>,
    tick: u64,
}

#[derive(Debug, Clone)]
struct QueuedCommand {
    order: Order,
}

#[derive(Debug, Clone)]
struct EventRecord {
    source: PersistId,
    canonical: Vec<u8>,
}

/// Opaque simulation handle owned by C through `orrery_sim_create`.
pub struct OrrerySim {
    executor: Executor<Regolith>,
    tick: u64,
    commands: BTreeMap<PersistId, Vec<QueuedCommand>>,
    events: Vec<EventRecord>,
    keyframes: BTreeMap<PersistId, ReplicationKeyframe>,
}

impl OrrerySim {
    fn new() -> Self {
        Self {
            executor: Executor::new(Regolith::honest(), UniverseSeed([0; 32])),
            tick: 0,
            commands: BTreeMap::new(),
            events: Vec::new(),
            keyframes: BTreeMap::new(),
        }
    }

    fn step(&mut self, ticks: u64) {
        for _ in 0..ticks {
            let entities: Vec<_> = self.executor.entities().copied().collect();
            for entity in entities {
                let queued = self.commands.remove(&entity).unwrap_or_default();
                let orders: Vec<_> = queued.into_iter().map(|command| command.order).collect();
                if let Some(outcome) =
                    self.executor
                        .step_entity(entity, Tick::new(self.tick), &orders)
                {
                    self.events
                        .extend(outcome.events.into_iter().map(|event| EventRecord {
                            source: entity,
                            canonical: event.to_canonical(),
                        }));
                }
            }
            self.tick = self.tick.saturating_add(1);
        }
    }

    fn queue_command(&mut self, bytes: &[u8]) -> Result<(), OrrerySimResult> {
        let entity_bytes = bytes.get(..8).ok_or(OrrerySimResult::MalformedInput)?;
        let entity = PersistId::new(u64::from_le_bytes(
            entity_bytes
                .try_into()
                .map_err(|_| OrrerySimResult::MalformedInput)?,
        ));
        let order = Order::decode(&bytes[8..]).map_err(|_| OrrerySimResult::MalformedInput)?;
        self.commands
            .entry(entity)
            .or_default()
            .push(QueuedCommand { order });
        Ok(())
    }

    fn apply_replication(&mut self, bytes: &[u8]) -> Result<(), OrrerySimResult> {
        let decoded = if let Some((canonical, _cell, entity, tick)) =
            decode_replication::<(Vec<u8>, CellId, PersistId, u64)>(bytes)
        {
            self.keyframes.insert(
                entity,
                ReplicationKeyframe {
                    canonical: canonical.clone(),
                    tick,
                },
            );
            DecodedReplica {
                canonical,
                entity,
                tick,
            }
        } else {
            let delta = decode_replication_delta(bytes).ok_or(OrrerySimResult::MalformedInput)?;
            let keyframe = self
                .keyframes
                .get(&delta.entity)
                .ok_or(OrrerySimResult::UnanchoredDelta)?;
            if delta.tick.checked_sub(u64::from(delta.keyframe_age)) != Some(keyframe.tick) {
                return Err(OrrerySimResult::UnanchoredDelta);
            }
            let canonical = apply_delta_patch(&keyframe.canonical, &delta.patch)
                .ok_or(OrrerySimResult::MalformedInput)?;
            let entity = delta.entity;
            let tick = delta.tick;
            DecodedReplica {
                canonical,
                entity,
                tick,
            }
        };

        let state = RegolithState::decode(&decoded.canonical)
            .map_err(|_| OrrerySimResult::MalformedInput)?;
        self.executor
            .insert_observed(decoded.entity, state, Tick::new(decoded.tick));
        self.tick = self.tick.max(decoded.tick);
        Ok(())
    }

    fn craft_transforms(&self) -> Vec<OrrerySimCraftTransform> {
        self.executor
            .entities()
            .filter_map(|entity| match self.executor.state(*entity) {
                Some(RegolithState::Craft(craft)) => Some(OrrerySimCraftTransform {
                    craft_id: entity.0,
                    x_mm: craft.pos.x,
                    y_mm: craft.pos.y,
                    z_mm: craft.pos.z,
                    yaw_urad: craft.yaw_urad,
                    pitch_urad: craft.pitch_urad,
                }),
                Some(RegolithState::Rock(_))
                | Some(RegolithState::Pickup(_))
                | Some(RegolithState::BloomDirector(_))
                | None => None,
            })
            .collect()
    }

    fn event_bytes(&self) -> Result<Vec<u8>, OrrerySimResult> {
        let mut bytes = Vec::new();
        for event in &self.events {
            let event_len = u32::try_from(event.canonical.len())
                .map_err(|_| OrrerySimResult::MalformedInput)?;
            bytes.extend_from_slice(&event.source.0.to_le_bytes());
            bytes.extend_from_slice(&event_len.to_le_bytes());
            bytes.extend_from_slice(&event.canonical);
        }
        Ok(bytes)
    }
}

struct DecodedReplica {
    canonical: Vec<u8>,
    entity: PersistId,
    tick: u64,
}

fn ffi(call: impl FnOnce() -> OrrerySimResult) -> OrrerySimResult {
    catch_unwind(AssertUnwindSafe(call)).unwrap_or(OrrerySimResult::Panic)
}

fn input_bytes<'a>(pointer: *const u8, len: usize) -> Result<&'a [u8], OrrerySimResult> {
    if len > MAX_INPUT_BYTES {
        return Err(OrrerySimResult::MalformedInput);
    }
    if len == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(OrrerySimResult::NullArgument);
    }
    // SAFETY: the C caller promises `pointer` names `len` readable bytes for
    // the duration of this call; null and oversized inputs were rejected.
    Ok(unsafe { std::slice::from_raw_parts(pointer, len) })
}

fn mutable_sim<'a>(sim: *mut OrrerySim) -> Result<&'a mut OrrerySim, OrrerySimResult> {
    // SAFETY: each exported function documents serialized calls on a live
    // handle created by `orrery_sim_create`; a null pointer becomes an error.
    unsafe { sim.as_mut() }.ok_or(OrrerySimResult::NullArgument)
}

fn sim_ref<'a>(sim: *const OrrerySim) -> Result<&'a OrrerySim, OrrerySimResult> {
    // SAFETY: each exported function documents that `sim` is a live handle;
    // a null pointer becomes an error before Rust forms a reference.
    unsafe { sim.as_ref() }.ok_or(OrrerySimResult::NullArgument)
}

/// Creates a headless Regolith simulation mirror.
///
/// # Safety
///
/// `out_sim` must name writable storage for one handle.
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_create(out_sim: *mut *mut OrrerySim) -> OrrerySimResult {
    ffi(|| {
        // SAFETY: the caller supplies writable storage for one opaque handle.
        let Some(out_sim) = (unsafe { out_sim.as_mut() }) else {
            return OrrerySimResult::NullArgument;
        };
        *out_sim = Box::into_raw(Box::new(OrrerySim::new()));
        OrrerySimResult::Ok
    })
}

/// Destroys a simulation handle created by [`orrery_sim_create`].
///
/// # Safety
///
/// `sim` must be a live handle returned exactly once by [`orrery_sim_create`].
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_destroy(sim: *mut OrrerySim) -> OrrerySimResult {
    ffi(|| {
        if sim.is_null() {
            return OrrerySimResult::NullArgument;
        }
        // SAFETY: the caller transfers back exactly one live handle returned
        // by `orrery_sim_create` and will not use it afterwards.
        drop(unsafe { Box::from_raw(sim) });
        OrrerySimResult::Ok
    })
}

/// Advances the mirror's deterministic fixed tick by `ticks`.
///
/// # Safety
///
/// `sim` must be a live handle and callers must serialize access to it.
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_step(sim: *mut OrrerySim, ticks: u64) -> OrrerySimResult {
    ffi(|| match mutable_sim(sim) {
        Ok(sim) => {
            sim.step(ticks);
            OrrerySimResult::Ok
        }
        Err(error) => error,
    })
}

/// Queues one flat Regolith command frame for the next [`orrery_sim_step`].
///
/// # Safety
///
/// `sim` must be live and serialized; `command_bytes` must name
/// `command_len` readable bytes when the length is nonzero.
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_submit_command(
    sim: *mut OrrerySim,
    command_bytes: *const u8,
    command_len: usize,
) -> OrrerySimResult {
    ffi(|| {
        let bytes = match input_bytes(command_bytes, command_len) {
            Ok(bytes) => bytes,
            Err(error) => return error,
        };
        match mutable_sim(sim) {
            Ok(sim) => match sim.queue_command(bytes) {
                Ok(()) => OrrerySimResult::Ok,
                Err(error) => error,
            },
            Err(error) => error,
        }
    })
}

/// Decodes and mirrors one current `orrery_protocol` replication datagram.
///
/// # Safety
///
/// `sim` must be live and serialized; `replication_bytes` must name
/// `replication_len` readable bytes when the length is nonzero.
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_apply_replication(
    sim: *mut OrrerySim,
    replication_bytes: *const u8,
    replication_len: usize,
) -> OrrerySimResult {
    ffi(|| {
        let bytes = match input_bytes(replication_bytes, replication_len) {
            Ok(bytes) => bytes,
            Err(error) => return error,
        };
        match mutable_sim(sim) {
            Ok(sim) => match sim.apply_replication(bytes) {
                Ok(()) => OrrerySimResult::Ok,
                Err(error) => error,
            },
            Err(error) => error,
        }
    })
}

/// Returns how many craft records [`orrery_sim_copy_craft_transforms`] needs.
///
/// # Safety
///
/// `sim` must be a live handle and `out_count` must name writable storage.
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_craft_transform_count(
    sim: *const OrrerySim,
    out_count: *mut usize,
) -> OrrerySimResult {
    ffi(|| {
        let sim = match sim_ref(sim) {
            Ok(sim) => sim,
            Err(error) => return error,
        };
        // SAFETY: the caller supplies writable storage for one `size_t`.
        let Some(out_count) = (unsafe { out_count.as_mut() }) else {
            return OrrerySimResult::NullArgument;
        };
        *out_count = sim.craft_transforms().len();
        OrrerySimResult::Ok
    })
}

/// Copies every renderable craft transform into C-owned contiguous storage.
///
/// # Safety
///
/// `sim` must be live, `out_required` must be writable, and when `capacity`
/// is sufficient, `out_transforms` must name that many writable records.
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_copy_craft_transforms(
    sim: *const OrrerySim,
    out_transforms: *mut OrrerySimCraftTransform,
    capacity: usize,
    out_required: *mut usize,
) -> OrrerySimResult {
    ffi(|| {
        let sim = match sim_ref(sim) {
            Ok(sim) => sim,
            Err(error) => return error,
        };
        let transforms = sim.craft_transforms();
        // SAFETY: the caller supplies writable storage for one `size_t`.
        let Some(out_required) = (unsafe { out_required.as_mut() }) else {
            return OrrerySimResult::NullArgument;
        };
        *out_required = transforms.len();
        if capacity < transforms.len() {
            return OrrerySimResult::BufferTooSmall;
        }
        if transforms.is_empty() {
            return OrrerySimResult::Ok;
        }
        if out_transforms.is_null() {
            return OrrerySimResult::NullArgument;
        }
        // SAFETY: the caller provided at least `transforms.len()` writable
        // records, established by the capacity check above.
        unsafe {
            std::ptr::copy_nonoverlapping(transforms.as_ptr(), out_transforms, transforms.len());
        }
        OrrerySimResult::Ok
    })
}

/// Drains canonical event records into one C-owned flat buffer.
///
/// # Safety
///
/// `sim` must be live and serialized, `out_required` must be writable, and
/// when `capacity` is sufficient, `out_bytes` must name writable storage.
#[no_mangle]
pub unsafe extern "C" fn orrery_sim_drain_events(
    sim: *mut OrrerySim,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrrerySimResult {
    ffi(|| {
        let sim = match mutable_sim(sim) {
            Ok(sim) => sim,
            Err(error) => return error,
        };
        let events = match sim.event_bytes() {
            Ok(events) => events,
            Err(error) => return error,
        };
        // SAFETY: the caller supplies writable storage for one `size_t`.
        let Some(out_required) = (unsafe { out_required.as_mut() }) else {
            return OrrerySimResult::NullArgument;
        };
        *out_required = events.len();
        if capacity < events.len() {
            return OrrerySimResult::BufferTooSmall;
        }
        if !events.is_empty() {
            if out_bytes.is_null() {
                return OrrerySimResult::NullArgument;
            }
            // SAFETY: capacity was checked against `events.len()` above and
            // the C caller promises the output storage is writable.
            unsafe {
                std::ptr::copy_nonoverlapping(events.as_ptr(), out_bytes, events.len());
            }
        }
        sim.events.clear();
        OrrerySimResult::Ok
    })
}
