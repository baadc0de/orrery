//! The one factory a game adds to the generic host ABI, for a real ruleset.
//!
//! Identical in shape to spike #1043's `crates/orrery_unreal_host/src/skirmish.rs`
//! (branch `spike/1043-staticlib-c-consumer`, `0f48fa8`), so the two prongs
//! drive the same ruleset through the same entry points and their numbers sit
//! on one graph. `orrery_games::Skirmish` is the ruleset: kinematic movement
//! over `libm` and integer combat with cooldowns and reach. The factory is the
//! shape the header documents (`crates/orrery_sim_host/include/orrery_sim_host.h:9-15`);
//! the two helpers beside it exist so the C driver never has to reimplement
//! the game's spawn table or its honest pilot — it asks for the bytes and
//! pushes them through the *generic* entry points, which keeps every mutation
//! of the host on the ABI under test.
//!
//! The ruleset is hosted on the `Executor` backend, not on
//! `orrery_sim_host::ecs::EcsBackend`: that backend requires
//! `Ruleset::CoreState: Sectioned` (`crates/orrery_sim_host/src/ecs.rs:91-96`)
//! and only `RegolithState` implements it
//! (`crates/orrery_games/src/regolith/state.rs:493`). The "`bevy_ecs` backend
//! that exists" (D53 §"could not establish" item 5) cannot host Skirmish
//! today; that is a finding, recorded in the README.

use orrery_core::{tick_rng, CoreCodec, Ruleset};
use orrery_games::game::Game;
use orrery_games::skirmish::Skirmish;
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::abi::{export_handle, OrreryHost, OrreryHostResult};
use orrery_sim_host::{Delivery, RulesetAdapter, SimulationHost, SimulationHostConfig};

/// Routes Skirmish's cross-entity events the way the game itself does.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkirmishAdapter;

impl RulesetAdapter<Skirmish> for SkirmishAdapter {
    fn deliver(
        &self,
        event: &<Skirmish as Ruleset>::CoreEvent,
    ) -> Option<Delivery<<Skirmish as Ruleset>::CoreInput>> {
        Skirmish::honest()
            .deliver(event)
            .map(|(recipient, input)| Delivery::new(recipient, input))
    }
}

/// Reads a 32-byte seed the caller promised.
///
/// # Safety
///
/// `seed` must be null or name 32 readable bytes.
const unsafe fn read_seed(seed: *const u8) -> Option<UniverseSeed> {
    if seed.is_null() {
        return None;
    }
    let mut key = [0; 32];
    // SAFETY: the caller promises `seed` names 32 readable bytes.
    key.copy_from_slice(unsafe { core::slice::from_raw_parts(seed, 32) });
    Some(UniverseSeed(key))
}

/// Copies `bytes` into caller storage under the host ABI's
/// `(out, capacity, out_required)` convention.
///
/// # Safety
///
/// `out_required` must be writable; when `capacity` suffices, `out` must name
/// that many writable bytes.
const unsafe fn copy_out(
    bytes: &[u8],
    out: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    // SAFETY: the caller supplies writable storage for one `size_t`.
    let Some(out_required) = (unsafe { out_required.as_mut() }) else {
        return OrreryHostResult::NullArgument;
    };
    *out_required = bytes.len();
    if capacity < bytes.len() {
        return OrreryHostResult::BufferTooSmall;
    }
    if bytes.is_empty() {
        return OrreryHostResult::Ok;
    }
    if out.is_null() {
        return OrreryHostResult::NullArgument;
    }
    // SAFETY: the caller provided at least `bytes.len()` writable bytes,
    // established by the capacity check above.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len()) };
    OrreryHostResult::Ok
}

/// Creates a host running Skirmish's honest rules.
///
/// # Safety
///
/// `seed` must name 32 readable bytes; `out_host` must name writable storage
/// for one handle pointer.
#[no_mangle]
pub unsafe extern "C" fn orrery_skirmish_host_create(
    seed: *const u8,
    first_tick: u64,
    out_host: *mut *mut OrreryHost,
) -> OrreryHostResult {
    // SAFETY: as documented on this function.
    let Some(seed) = (unsafe { read_seed(seed) }) else {
        return OrreryHostResult::NullArgument;
    };
    // SAFETY: `out_host` is the caller's writable handle storage, as
    // `export_handle` requires.
    unsafe {
        export_handle(out_host, || {
            SimulationHost::new(
                SimulationHostConfig::new(seed).starting_at(Tick::new(first_tick)),
                Skirmish::honest(),
                SkirmishAdapter,
            )
        })
    }
}

/// Copies the canonical bytes of the craft Skirmish spawns in `slot`, for
/// the caller to hand to `orrery_host_install_state`.
///
/// # Safety
///
/// `out_required` must be writable; when `capacity` suffices, `out_bytes`
/// must name that many writable bytes.
#[no_mangle]
pub unsafe extern "C" fn orrery_skirmish_spawn_state(
    entity: u64,
    slot: u64,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    let state = Skirmish::honest().spawn(PersistId::new(entity), slot);
    // SAFETY: as documented on this function.
    unsafe { copy_out(&state.to_canonical(), out_bytes, capacity, out_required) }
}

/// Copies what Skirmish's honest pilot in `slot` asks for at `tick`.
///
/// The bytes are `[u32 len LE][command bytes]` records where each command is
/// the flat `[target u64 LE][Order canonical bytes]` that
/// `orrery_host_submit_command` takes. `peers` names the other entities the
/// pilot may fire at. This is the same pilot the P4 harness plays
/// (`orrery_games::skirmish::pilot::honest_orders`), drawn from the same
/// per-tick RNG, so the remote population does what a scenario's population
/// does rather than coasting.
///
/// # Safety
///
/// `seed` must name 32 readable bytes; `peers` must name `peer_count`
/// readable `uint64_t`s when `peer_count` is nonzero; `out_required` must be
/// writable; when `capacity` suffices, `out_bytes` must name that many
/// writable bytes.
#[no_mangle]
pub unsafe extern "C" fn orrery_skirmish_honest_commands(
    seed: *const u8,
    entity: u64,
    slot: u64,
    tick: u64,
    peers: *const u64,
    peer_count: usize,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    // SAFETY: as documented on this function.
    let Some(seed) = (unsafe { read_seed(seed) }) else {
        return OrreryHostResult::NullArgument;
    };
    let peers: Vec<PersistId> = if peer_count == 0 {
        Vec::new()
    } else if peers.is_null() {
        return OrreryHostResult::NullArgument;
    } else {
        // SAFETY: the caller promises `peers` names `peer_count` readable
        // `u64`s; null was rejected above.
        unsafe { core::slice::from_raw_parts(peers, peer_count) }
            .iter()
            .map(|id| PersistId::new(*id))
            .collect()
    };
    let entity = PersistId::new(entity);
    let tick = Tick::new(tick);
    let mut rng = tick_rng(seed, entity, tick);
    let mut orders = Vec::new();
    Skirmish::honest().honest_inputs(entity, slot, tick, &peers, &mut rng, &mut orders);

    let mut bytes = Vec::new();
    for order in &orders {
        let mut command = entity.0.to_le_bytes().to_vec();
        command.extend(order.to_canonical());
        let Ok(len) = u32::try_from(command.len()) else {
            return OrreryHostResult::RecordTooLarge;
        };
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&command);
    }
    // SAFETY: as documented on this function.
    unsafe { copy_out(&bytes, out_bytes, capacity, out_required) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_games::skirmish::state::Craft;
    use orrery_sim_host::TickCount;

    #[test]
    fn the_spawn_bytes_decode_to_the_games_own_spawn() {
        let mut out = vec![0; 256];
        let mut required = 0;
        // SAFETY: `out` and `required` are writable storage of the sizes
        // passed.
        let result = unsafe {
            orrery_skirmish_spawn_state(7, 3, out.as_mut_ptr(), out.len(), &raw mut required)
        };
        assert_eq!(result, OrreryHostResult::Ok);
        let decoded = Craft::decode(&out[..required]).expect("craft decodes");
        assert_eq!(decoded, Skirmish::honest().spawn(PersistId::new(7), 3));
    }

    #[test]
    fn honest_commands_submit_and_the_host_routes_the_fire_back_as_damage() {
        let seed = [0x11; 32];
        let mut host = SimulationHost::new(
            SimulationHostConfig::new(UniverseSeed(seed)),
            Skirmish::honest(),
            SkirmishAdapter,
        );
        let game = Skirmish::honest();
        for slot in 0..2 {
            let entity = PersistId::new(slot + 1);
            host.install_state(entity, game.spawn(entity, slot));
        }
        let peers = [2_u64];
        let mut out = vec![0; 1024];
        let mut required = 0;
        // SAFETY: every pointer names storage of the size passed beside it.
        let result = unsafe {
            orrery_skirmish_honest_commands(
                seed.as_ptr(),
                1,
                0,
                0,
                peers.as_ptr(),
                peers.len(),
                out.as_mut_ptr(),
                out.len(),
                &raw mut required,
            )
        };
        assert_eq!(result, OrreryHostResult::Ok);
        let mut at = 0;
        let mut submitted = 0;
        while at < required {
            let len = u32::from_le_bytes(out[at..at + 4].try_into().expect("len")) as usize;
            at += 4;
            host.submit_command_bytes(&out[at..at + len])
                .expect("an honest command decodes");
            at += len;
            submitted += 1;
        }
        assert_eq!(submitted, 2, "a thrust and a fire");
        host.step(TickCount::new(1));
        let events = host.drain_event_bytes().expect("events encode");
        assert!(
            !events.is_empty(),
            "the fire rolled damage the adapter routes to the target"
        );
    }
}
