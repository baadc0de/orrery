//! The reference C library: everything a game adds beyond the header.
//!
//! Built as a `cdylib`, this exports every generic `orrery_host_*` symbol
//! from `orrery_sim_host::abi` plus the one factory below.  A game's own
//! library is this file with its ruleset and adapter named in place of the
//! synthetic ones, and nothing else.

#![deny(unsafe_op_in_unsafe_fn)]

use orrery_protocol::{Tick, UniverseSeed};
use orrery_sim_host::abi::{export_handle, OrreryHost, OrreryHostResult};
use synthetic::{Synthetic, SyntheticAdapter};

#[path = "../tests/support/synthetic.rs"]
mod synthetic;
use orrery_sim_host::{SimulationHost, SimulationHostConfig};

/// Creates a host running the synthetic reference rules.
///
/// # Safety
///
/// `seed` must name 32 readable bytes; `out_host` must name writable storage
/// for one handle pointer.
#[no_mangle]
pub unsafe extern "C" fn orrery_synthetic_host_create(
    seed: *const u8,
    first_tick: u64,
    out_host: *mut *mut OrreryHost,
) -> OrreryHostResult {
    if seed.is_null() {
        return OrreryHostResult::NullArgument;
    }
    let mut key = [0; 32];
    // SAFETY: the caller promises `seed` names 32 readable bytes.
    key.copy_from_slice(unsafe { std::slice::from_raw_parts(seed, 32) });
    // SAFETY: `out_host` is the caller's writable handle storage, as
    // `export_handle` requires.
    unsafe {
        export_handle(out_host, || {
            SimulationHost::new(
                SimulationHostConfig::new(UniverseSeed(key)).starting_at(Tick::new(first_tick)),
                Synthetic,
                SyntheticAdapter,
            )
        })
    }
}
