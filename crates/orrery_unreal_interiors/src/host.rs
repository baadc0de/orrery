//! The one factory a game adds to the generic host ABI, plus the scene.
//!
//! The same shape as spike #1052's `orrery_unreal_direct/src/skirmish.rs`
//! (`orrery_skirmish_host_create` / `orrery_skirmish_spawn_state`), so the C
//! driver lifted from that prong needs no new entry point.
//!
//! The population is fixed and named here rather than in the consumer so the
//! C run and the Unreal run start from the same bytes:
//!
//! | id | kind | frame | pose |
//! |---|---|---|---|
//! | 1 | station | universe | 100 km along +x from the origin (`STATION_X_MM`), yaw 0 — far enough that Unreal's world-space reprojection shows LWC's contribution |
//! | 2 | ship | station | docked at the station's bay: 50 m along +y, yaw 0 |
//! | 3 | mech | ship | 6 m along +x in the ship |
//! | 4 | avatar | station | 40 m along +y, 10 m short of the docked ship's origin |
//!
//! No adapter routing: a `FrameChanged` or `Refused` is an observable event
//! with no next-tick recipient.

use orrery_core::{CoreCodec, QPos};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use orrery_sim_host::abi::{export_handle, OrreryHost, OrreryHostResult};
use orrery_sim_host::{NoEventRouting, SimulationHost, SimulationHostConfig};

use crate::rules::{Body, Interiors, Kind, UNIVERSE};

/// The station's distance from the universe origin along +x, in mm.
pub const STATION_X_MM: i64 = 100_000_000;

/// The scene population, ascending by id.
#[must_use]
pub fn scene() -> Vec<(PersistId, Body)> {
    vec![
        (
            PersistId::new(1),
            Body::at_rest(
                Kind::Station,
                UNIVERSE,
                QPos {
                    x: STATION_X_MM,
                    y: 0,
                    z: 0,
                },
                0,
            ),
        ),
        (
            PersistId::new(2),
            Body::at_rest(
                Kind::Ship,
                1,
                QPos {
                    x: 0,
                    y: 50_000,
                    z: 0,
                },
                0,
            ),
        ),
        (
            PersistId::new(3),
            Body::at_rest(
                Kind::Mech,
                2,
                QPos {
                    x: 6_000,
                    y: 0,
                    z: 0,
                },
                0,
            ),
        ),
        (
            PersistId::new(4),
            Body::at_rest(
                Kind::Avatar,
                1,
                QPos {
                    x: 0,
                    y: 40_000,
                    z: 0,
                },
                0,
            ),
        ),
    ]
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

/// Copies `bytes` out under the host ABI's `(out, capacity, out_required)`
/// convention.
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

/// Creates a host running the nested-frame rules, empty.
///
/// # Safety
///
/// `seed` must name 32 readable bytes; `out_host` must name writable storage
/// for one handle pointer.
#[no_mangle]
pub unsafe extern "C" fn orrery_interiors_host_create(
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
                Interiors,
                NoEventRouting,
            )
        })
    }
}

/// How many bodies [`scene`] holds.
#[no_mangle]
pub extern "C" fn orrery_interiors_scene_len() -> u64 {
    scene().len() as u64
}

/// Copies the canonical bytes of scene body `entity` (1-based, see the module
/// table) for the caller to hand to `orrery_host_install_state`.
/// `ORRERY_HOST_NOT_FOUND` for an id outside the scene.
///
/// # Safety
///
/// `out_required` must be writable; when `capacity` suffices, `out_bytes`
/// must name that many writable bytes.
#[no_mangle]
pub unsafe extern "C" fn orrery_interiors_scene_state(
    entity: u64,
    out_bytes: *mut u8,
    capacity: usize,
    out_required: *mut usize,
) -> OrreryHostResult {
    let Some((_, body)) = scene().into_iter().find(|(id, _)| id.0 == entity) else {
        return OrreryHostResult::NotFound;
    };
    // SAFETY: as documented on this function.
    unsafe { copy_out(&body.to_canonical(), out_bytes, capacity, out_required) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Happening, Intent, BODY_ENCODED_LEN};
    use orrery_core::QVel;
    use orrery_sim_host::TickCount;

    fn host() -> SimulationHost<Interiors, NoEventRouting> {
        let mut host = SimulationHost::new(
            SimulationHostConfig::new(UniverseSeed([7; 32])),
            Interiors,
            NoEventRouting,
        );
        for (id, body) in scene() {
            host.install_state(id, body);
        }
        host
    }

    fn intent(target: u64, intent: &Intent) -> Vec<u8> {
        let mut command = target.to_le_bytes().to_vec();
        command.extend(intent.to_canonical());
        command
    }

    fn body(host: &SimulationHost<Interiors, NoEventRouting>, id: u64) -> Body {
        Body::decode(&host.state_bytes(PersistId::new(id)).expect("installed")).expect("decodes")
    }

    #[test]
    fn the_scene_bytes_are_the_scene() {
        let mut out = vec![0; 256];
        let mut required = 0;
        // SAFETY: `out` and `required` are writable storage of the sizes
        // passed.
        let result = unsafe {
            orrery_interiors_scene_state(2, out.as_mut_ptr(), out.len(), &raw mut required)
        };
        assert_eq!(result, OrreryHostResult::Ok);
        assert_eq!(required, BODY_ENCODED_LEN);
        assert_eq!(
            Body::decode(&out[..required]).expect("decodes"),
            scene()[1].1
        );
        assert_eq!(orrery_interiors_scene_len(), 4);
    }

    #[test]
    fn boarding_a_docked_ship_is_a_teleport_class_crossing_that_keeps_the_world_pose() {
        // The avatar walks 10 m up the bay at 2.4 m/s (40 mm per tick, an
        // exact lattice step; 2 m/s would be 33.33 mm and the tick-boundary
        // snap would lose the third) and boards: its station-local (0, 50 m)
        // becomes ship-local (0, 0) in one step, and the event says so.
        let mut host = host();
        host.submit_command_bytes(&intent(
            4,
            &Intent::Move {
                vel: QVel {
                    x: 0,
                    y: 2_400,
                    z: 0,
                },
                yaw_urad: 0,
            },
        ))
        .expect("move decodes");
        host.step(TickCount::new(250));
        assert_eq!(body(&host, 4).pos.y, 50_000);
        host.submit_command_bytes(&intent(4, &Intent::Enter { frame: 2 }))
            .expect("enter decodes");
        host.step(TickCount::new(1));
        let avatar = body(&host, 4);
        assert_eq!(avatar.frame, 2);
        assert_eq!(avatar.frame_changes, 1);
        // Still walking at 2.4 m/s in ship-local y: one tick past the crossing.
        assert_eq!(avatar.pos, QPos { x: 0, y: 40, z: 0 });
        let events = host
            .drain_event_bytes()
            .expect("events encode")
            .into_bytes();
        let len = u32::from_le_bytes(events[8..12].try_into().expect("len")) as usize;
        assert_eq!(
            Happening::decode(&events[12..12 + len]).expect("decodes"),
            Happening::FrameChanged {
                entity: 4,
                from: 1,
                to: 2
            }
        );
    }

    #[test]
    fn undocking_then_leaving_preserves_velocity_into_the_universe_frame() {
        // Ship undocks (station -> universe, at rest, so the pose composes
        // exactly), cruises at 500 m/s along +x; the avatar aboard leaves
        // (EVA): its universe velocity is the ship's plus its own, rotated.
        let mut host = host();
        host.submit_command_bytes(&intent(4, &Intent::Enter { frame: 2 }))
            .expect("enter");
        host.step(TickCount::new(1));
        host.submit_command_bytes(&intent(2, &Intent::Leave))
            .expect("undock");
        host.step(TickCount::new(1));
        let ship = body(&host, 2);
        assert_eq!(ship.frame, UNIVERSE);
        assert_eq!(
            ship.pos,
            QPos {
                x: STATION_X_MM,
                y: 50_000,
                z: 0
            }
        );
        host.submit_command_bytes(&intent(
            2,
            &Intent::Cruise {
                vel: QVel {
                    x: 500_000,
                    y: 0,
                    z: 0,
                },
                yaw_rate_urad_tick: 0,
                roll_rate_urad_tick: 1454,
            },
        ))
        .expect("cruise");
        host.submit_command_bytes(&intent(
            4,
            &Intent::Move {
                vel: QVel {
                    x: 0,
                    y: 2_000,
                    z: 0,
                },
                yaw_urad: 0,
            },
        ))
        .expect("move");
        host.step(TickCount::new(60));
        host.submit_command_bytes(&intent(4, &Intent::Leave))
            .expect("leave");
        host.step(TickCount::new(1));
        let avatar = body(&host, 4);
        assert_eq!(avatar.frame, UNIVERSE);
        assert_eq!(avatar.frame_changes, 2);
        assert_eq!(avatar.vel.x, 500_000);
        // Ship rolled 60 ticks * 1454 urad = 0.087 rad about +x: local +y
        // velocity tilts into y/z.
        let (_, _, _) = (avatar.vel.y, avatar.vel.z, 0);
        assert!((avatar.vel.y - 1_992).abs() <= 1, "{}", avatar.vel.y);
        assert!((avatar.vel.z - 174).abs() <= 1, "{}", avatar.vel.z);
    }

    #[test]
    fn entering_a_frame_that_is_not_coincident_is_refused() {
        // The avatar is in the station; the mech is in the ship. Not
        // coincident (§13.4), so nothing moves and the event says why.
        let mut host = host();
        host.submit_command_bytes(&intent(4, &Intent::Enter { frame: 3 }))
            .expect("enter");
        host.step(TickCount::new(1));
        assert_eq!(body(&host, 4).frame, 1);
        assert_eq!(body(&host, 4).frame_changes, 0);
    }

    #[test]
    fn a_snapshot_restores_the_frame_relation_across_a_crossing() {
        // Snapshot before the crossing, step through it, restore, replay:
        // the same hashes. D47's per-entity set carrying `frame` is enough
        // for the host's own guarantee; the C consumer asks the harder
        // question against a second host.
        let mut host = host();
        let before = host.snapshot();
        host.submit_command_bytes(&intent(4, &Intent::Enter { frame: 2 }))
            .expect("enter");
        let first = host.step(TickCount::new(1));
        host.restore(&before).expect("restore");
        host.submit_command_bytes(&intent(4, &Intent::Enter { frame: 2 }))
            .expect("enter");
        let second = host.step(TickCount::new(1));
        assert_eq!(first.state_hashes, second.state_hashes);
        assert_eq!(body(&host, 4).frame, 2);
    }
}
