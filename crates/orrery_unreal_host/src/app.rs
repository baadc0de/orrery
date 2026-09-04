//! The headless `bevy_app::App` behind an opaque C handle.
//!
//! The `App` is built exactly as the spike names it — `MinimalPlugins`,
//! `StatesPlugin` (which lightyear's replication backend needs and
//! `MinimalPlugins` lacks, see `crates/orrery_sidecar/src/lib.rs:258-262`),
//! `OrreryNetPlugin` with relays disabled, `OrreryPredictPlugin` at D16's
//! defaults — then finished and cleaned up, and never `run()`: the foreign
//! caller owns the loop and calls [`orrery_app_update`] once per fixed tick.
//!
//! # Two clocks, and the one the caller chooses
//!
//! The sim host never reads a clock (`crates/orrery_sim_host/src/lib.rs:6-9`)
//! and the C accumulator decides how many ticks it steps. Bevy's `Time<Fixed>`
//! is a second accumulator, fed by `TimePlugin` from the wall clock, and
//! lightyear increments its `LocalTimeline` from *that* one in `FixedFirst`
//! (`crates/orrery_predict/src/plugin.rs:52-57`). So the `App` prong carries
//! two tick counters by construction, and #1043's stated unknown — whether
//! lightyear's tick bridge survives an externally-owned accumulator — is a
//! question about whether they stay equal. [`OrreryAppClock`] offers both
//! answers so the C driver can measure each: `Automatic` leaves Bevy on the
//! wall clock; `Manual` feeds Bevy the caller's `dt` through
//! `TimeUpdateStrategy::ManualDuration`, which is the mechanism by which a
//! foreign accumulator would own Bevy's clock. [`orrery_app_timeline_read`] reads
//! both counters back so the drift is a number, not an assertion.
//!
//! # Panics
//!
//! As the host ABI (`crates/orrery_sim_host/src/abi.rs:34-40`): every entry
//! point runs under `catch_unwind`, a panic is reported as
//! `ORRERY_HOST_PANIC`, and a panic inside `update` poisons the handle so
//! every later call but destroy returns `ORRERY_HOST_POISONED`. Bevy's
//! multi-threaded executor re-raises a system panic on the calling thread
//! (`bevy_ecs` 0.19.1 `schedule/executor/multi_threaded.rs:305-308`), which
//! is what makes the guard reachable at all; [`orrery_app_request_panic`]
//! exists so a C caller can prove that path rather than assume it.

use core::panic::AssertUnwindSafe;
use core::time::Duration;
use std::panic::catch_unwind;
use std::thread::{self, ThreadId};

use bevy::MinimalPlugins;
use bevy_app::{App, FixedUpdate, Update};
use bevy_diagnostic::FrameCount;
use bevy_ecs::prelude::{Res, ResMut, Resource};
use bevy_state::app::StatesPlugin;
use bevy_time::{Fixed, Time, TimeUpdateStrategy, Virtual};
use lightyear::prelude::LocalTimeline;
use orrery_net::plugin::NetConfig;
use orrery_net::{CoordinatorConfig, OrreryNetPlugin};
use orrery_predict::{OrreryPredictPlugin, TickBridge};
use orrery_sim_host::abi::OrreryHostResult;

/// The ABI version `orrery_app_abi_version` reports; matches the header.
pub const APP_ABI_VERSION: u32 = 1;

/// Who advances Bevy's clock: `TimePlugin` from the wall clock, or the
/// foreign accumulator through `TimeUpdateStrategy::ManualDuration`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrreryAppClock {
    /// `TimeUpdateStrategy::Automatic`: Bevy reads `Instant::now()` in
    /// `First`, and `Time<Fixed>` steps zero, one or two times per update
    /// depending on how the wall clock fell.
    Automatic = 0,
    /// `TimeUpdateStrategy::ManualDuration(dt)`: every `update` advances Bevy
    /// by exactly the `dt` the caller passed, so a caller passing one fixed
    /// step runs `FixedMain` exactly once per call.
    Manual = 1,
}

/// The tick counters the `App` prong carries, read back for the driver.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrreryAppTimeline {
    /// lightyear's `LocalTimeline` tick, as `orrery_predict`'s bridge sees
    /// it (`TickBridge::last_seen`).
    pub lightyear_tick: u32,
    /// The universe tick the bridge resolves that to.
    pub bridged_tick: u64,
    /// How many times `FixedUpdate` has run in this `App`.
    pub fixed_steps: u64,
    /// Bevy's `FrameCount`: how many times `update` ran the main schedule.
    pub frames: u32,
    /// `Time<Virtual>::elapsed`, nanoseconds.
    pub virtual_elapsed_ns: u64,
    /// `Time<Fixed>::timestep`, nanoseconds — what a `Manual` caller should
    /// pass as `dt_ns` to run exactly one fixed step per update.
    pub fixed_step_ns: u64,
}

/// Counts `FixedUpdate` runs, so the driver can hold its own accumulator
/// against Bevy's.
#[derive(Debug, Default, Resource)]
struct FixedSteps(u64);

/// Set by [`orrery_app_request_panic`]; the next `update` panics inside a
/// system.
#[derive(Debug, Default, Resource)]
struct PanicRequested(bool);

fn count_fixed_steps(mut steps: ResMut<FixedSteps>) {
    steps.0 = steps.0.saturating_add(1);
}

fn panic_if_requested(requested: Res<PanicRequested>) {
    assert!(
        !requested.0,
        "boundary probe: a system panicked inside App::update"
    );
}

/// Build the headless `App` the spike names. Finished and cleaned up, not
/// run.
pub fn build_app(clock: OrreryAppClock) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(OrreryNetPlugin {
        config: NetConfig {
            // The facade's default for the same reason it gives
            // (`crates/orrery/src/lib.rs:105-111`): a headless run must not
            // reach n0's relay fleet.
            relay_mode: iroh::RelayMode::Disabled,
            secret_key: None,
        },
        coordinator: CoordinatorConfig::default(),
    });
    app.add_plugins(OrreryPredictPlugin::default());
    app.init_resource::<FixedSteps>();
    app.init_resource::<PanicRequested>();
    app.add_systems(FixedUpdate, count_fixed_steps);
    app.add_systems(Update, panic_if_requested);
    if clock == OrreryAppClock::Manual {
        let step = app.world().resource::<Time<Fixed>>().timestep();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    }
    app.finish();
    app.cleanup();
    app
}

/// The opaque handle a C caller owns: `orrery_app` in the header.
pub struct OrreryApp {
    app: App,
    clock: OrreryAppClock,
    poisoned: bool,
    created_on: ThreadId,
}

impl OrreryApp {
    fn timeline(&self) -> OrreryAppTimeline {
        let world = self.app.world();
        let bridge = world.resource::<TickBridge>();
        // lightyear's own reading, cross-checked against the bridge's copy of
        // it: the bridge is only advanced in `FixedLast`, so the two agree
        // exactly when the fixed schedule ran this frame.
        let lightyear_tick = world.resource::<LocalTimeline>().tick().0;
        OrreryAppTimeline {
            lightyear_tick,
            bridged_tick: bridge.resolve(bridge.last_seen()).0,
            fixed_steps: world.resource::<FixedSteps>().0,
            frames: world.resource::<FrameCount>().0,
            virtual_elapsed_ns: duration_ns(world.resource::<Time<Virtual>>().elapsed()),
            fixed_step_ns: duration_ns(world.resource::<Time<Fixed>>().timestep()),
        }
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn with_app<F>(app: *mut OrreryApp, call: F) -> OrreryHostResult
where
    F: FnOnce(&mut OrreryApp) -> OrreryHostResult,
{
    // SAFETY: each exported function documents serialized calls on a live
    // handle produced by `orrery_app_create`; a null pointer becomes an error.
    let Some(app) = (unsafe { app.as_mut() }) else {
        return OrreryHostResult::NullArgument;
    };
    if app.poisoned {
        return OrreryHostResult::Poisoned;
    }
    if let Ok(result) = catch_unwind(AssertUnwindSafe(|| call(app))) {
        result
    } else {
        app.poisoned = true;
        OrreryHostResult::Panic
    }
}

/// The ABI version this library was built with; compare with
/// `ORRERY_UNREAL_HOST_APP_ABI_VERSION` before any other call.
#[no_mangle]
pub const extern "C" fn orrery_app_abi_version() -> u32 {
    APP_ABI_VERSION
}

/// Creates the headless `App` on the calling thread.
///
/// `clock` is an [`OrreryAppClock`] discriminant; any other value is
/// `ORRERY_HOST_MALFORMED_INPUT`. Construction — plugin build, task-pool
/// spawn, the iroh endpoint's Startup system on the first update — runs under
/// the panic guard.
///
/// # Safety
///
/// `out_app` must be null or name writable storage for one handle pointer.
#[no_mangle]
pub unsafe extern "C" fn orrery_app_create(
    clock: u32,
    out_app: *mut *mut OrreryApp,
) -> OrreryHostResult {
    // SAFETY: the caller supplies writable storage for one opaque handle, or
    // null, which becomes an error before anything is written.
    let Some(out_app) = (unsafe { out_app.as_mut() }) else {
        return OrreryHostResult::NullArgument;
    };
    let clock = match clock {
        0 => OrreryAppClock::Automatic,
        1 => OrreryAppClock::Manual,
        _ => return OrreryHostResult::MalformedInput,
    };
    catch_unwind(AssertUnwindSafe(|| build_app(clock))).map_or(OrreryHostResult::Panic, |app| {
        *out_app = Box::into_raw(Box::new(OrreryApp {
            app,
            clock,
            poisoned: false,
            created_on: thread::current().id(),
        }));
        OrreryHostResult::Ok
    })
}

/// Runs one `App::update()`.
///
/// Under [`OrreryAppClock::Manual`], Bevy's clock advances by exactly
/// `dt_ns`; under `Automatic` the argument is ignored and Bevy reads the wall
/// clock. A panic inside any system is reported as `ORRERY_HOST_PANIC` and
/// poisons the handle.
///
/// # Safety
///
/// `app` must be a live handle and calls on it must be serialized.
#[no_mangle]
pub unsafe extern "C" fn orrery_app_update(app: *mut OrreryApp, dt_ns: u64) -> OrreryHostResult {
    with_app(app, |handle| {
        if handle.clock == OrreryAppClock::Manual {
            handle
                .app
                .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(
                    dt_ns,
                )));
        }
        handle.app.update();
        OrreryHostResult::Ok
    })
}

/// Reads the `App`'s tick counters.
///
/// # Safety
///
/// `app` must be live; `out` must name writable storage for one record.
#[no_mangle]
pub unsafe extern "C" fn orrery_app_timeline_read(
    app: *mut OrreryApp,
    out: *mut OrreryAppTimeline,
) -> OrreryHostResult {
    with_app(app, |handle| {
        // SAFETY: the caller supplies writable storage for one record.
        let Some(out) = (unsafe { out.as_mut() }) else {
            return OrreryHostResult::NullArgument;
        };
        *out = handle.timeline();
        OrreryHostResult::Ok
    })
}

/// Whether the calling thread created the `App`: `1` if so, `0` if not.
///
/// Bevy's `NonSend` accounting is per world, and an Unreal game thread is not
/// necessarily the thread that loaded the module; the C driver reads this
/// beside what [`orrery_app_update`] returns from another thread.
///
/// # Safety
///
/// `app` must be live.
#[no_mangle]
pub unsafe extern "C" fn orrery_app_on_creating_thread(app: *const OrreryApp) -> u32 {
    // SAFETY: a shared read of a live handle; null reads as "not this thread".
    let Some(handle) = (unsafe { app.as_ref() }) else {
        return 0;
    };
    u32::from(handle.created_on == thread::current().id())
}

/// Arms a system that panics on the next `update`, so a C caller can observe
/// the `PANIC` then `POISONED` contract on this handle.
///
/// # Safety
///
/// `app` must be live and serialized.
#[no_mangle]
pub unsafe extern "C" fn orrery_app_request_panic(app: *mut OrreryApp) -> OrreryHostResult {
    with_app(app, |handle| {
        handle.app.world_mut().resource_mut::<PanicRequested>().0 = true;
        OrreryHostResult::Ok
    })
}

/// Destroys a handle. Accepted on a poisoned handle; the `App`'s drop — task
/// pools, the tokio runtime behind the iroh endpoint — runs under the guard.
///
/// # Safety
///
/// `app` must be a live handle returned exactly once by
/// [`orrery_app_create`] and not used after this call.
#[no_mangle]
pub unsafe extern "C" fn orrery_app_destroy(app: *mut OrreryApp) -> OrreryHostResult {
    if app.is_null() {
        return OrreryHostResult::NullArgument;
    }
    // SAFETY: the caller transfers back exactly one live handle and will not
    // use it afterwards.
    catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(app) })))
        .map_or(OrreryHostResult::Panic, |()| OrreryHostResult::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_app_runs_exactly_one_fixed_step_per_update() {
        let mut app = build_app(OrreryAppClock::Manual);
        let step = app.world().resource::<Time<Fixed>>().timestep();
        for _ in 0..120 {
            app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
            app.update();
        }
        // Observed, not designed: Bevy's first update is its zero-delta
        // startup frame (`Time<Real>` records `startup` and advances nothing),
        // so a foreign accumulator that feeds one step per update sees the
        // fixed schedule run once fewer than it called — a constant one-tick
        // offset unless the driver primes one update at creation.
        assert_eq!(app.world().resource::<FixedSteps>().0, 119);
        assert_eq!(
            app.world().resource::<LocalTimeline>().tick().0,
            119,
            "lightyear's timeline followed the fed clock step for step, one behind"
        );
    }

    #[test]
    fn the_handle_reports_a_system_panic_as_a_code_and_poisons() {
        let mut out: *mut OrreryApp = core::ptr::null_mut();
        // SAFETY: `out` is writable storage for one handle pointer.
        assert_eq!(
            unsafe { orrery_app_create(1, &raw mut out) },
            OrreryHostResult::Ok
        );
        let step = 16_666_667;
        // SAFETY: `out` is the live handle just created; calls are serialized.
        unsafe {
            assert_eq!(orrery_app_update(out, step), OrreryHostResult::Ok);
            assert_eq!(orrery_app_request_panic(out), OrreryHostResult::Ok);
            assert_eq!(orrery_app_update(out, step), OrreryHostResult::Panic);
            assert_eq!(orrery_app_update(out, step), OrreryHostResult::Poisoned);
            assert_eq!(orrery_app_destroy(out), OrreryHostResult::Ok);
        }
    }
}
