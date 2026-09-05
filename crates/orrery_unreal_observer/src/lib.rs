//! The engine side of the sidecar IPC crossing, behind a C ABI (#898 step 3).
//!
//! An Unreal module links this archive, includes one header, and moves actors
//! from a flat array of records. It never decodes a frame, never learns what a
//! `PersistId` is made of, and never sees a socket.
//!
//! # The three properties the ABI is shaped around
//!
//! **1. `poll` must not block the game thread.** A renderer calling `read` on
//! a socket has handed its frame rate to the sidecar's scheduler and to the
//! network stack. So the blocking read lives on a thread this crate owns; the
//! game thread's `poll` takes a lock, reads two counters, and returns. The
//! thread is where a stall goes to be harmless.
//!
//! **2. State crosses by copy-out, never by pointer.** `snapshot` takes the
//! caller's buffer, its capacity, and an out-parameter for the required
//! length — the convention D53 clause (b) records for `orrery_sim_host`, and
//! the reason nothing here allocates on the caller's behalf or hands back a
//! pointer whose lifetime the two languages would have to agree about. A
//! snapshot is taken under one lock, so the array a renderer iterates is one
//! consistent presentation set rather than a torn read of two.
//!
//! **3. A panic does not cross.** Every entry point is wrapped in
//! [`catch_unwind`](std::panic::catch_unwind); a handle that panicked is
//! poisoned and answers [`ORRERY_OBSERVER_POISONED`] to everything but
//! `destroy` — as `orrery_sim_host` does, and for the same reason: unwinding
//! into Unreal's frame is undefined behaviour, not an error path.
//!
//! # What this crate deliberately does not do
//!
//! It does not send. The link is one-directional by construction: there is no
//! `submit` symbol here, and an engine holding this handle cannot produce a
//! canonical fact with it. That is D53 clause (f) items 1 and 2 enforced by
//! the shape of the header rather than by a rule someone has to remember, and
//! it is why an observer built on it adds no inbound channel to the two the
//! record allows.

#![warn(missing_docs)]

extern crate alloc;

use alloc::sync::Arc;
use core::ffi::CStr;
use core::ffi::{c_char, c_void};
use core::panic::AssertUnwindSafe;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::panic::catch_unwind;
use std::sync::Mutex;
use std::thread;

use orrery_ipc_transport::observer::{ObserverLink, ObserverView, Polled, Timeline};

/// Version of this C ABI. Bumped when the header's shapes or contracts change.
pub const OBSERVER_ABI_VERSION: u32 = 1;

/// The call succeeded.
pub const ORRERY_OBSERVER_OK: i32 = 0;
/// A null handle, or a null out-pointer where one is required.
pub const ORRERY_OBSERVER_BAD_ARGUMENT: i32 = 1;
/// The sidecar closed the stream cleanly. The last snapshot stays readable.
pub const ORRERY_OBSERVER_LINK_CLOSED: i32 = 2;
/// The link failed: the sidecar died, or sent something undecodable.
pub const ORRERY_OBSERVER_LINK_FAILED: i32 = 3;
/// The caller's buffer was too small; `out_required` says how large it must be.
pub const ORRERY_OBSERVER_TOO_SMALL: i32 = 4;
/// Rust panicked inside this call. The handle is now poisoned.
pub const ORRERY_OBSERVER_PANIC: i32 = 5;
/// A previous call panicked; only `destroy` is accepted.
pub const ORRERY_OBSERVER_POISONED: i32 = 6;

/// Timeline tag: this peer's own predicted timeline.
pub const ORRERY_OBSERVER_PREDICTED: u8 = 0;
/// Timeline tag: a remote timeline rendered between two confirmed snapshots.
pub const ORRERY_OBSERVER_INTERPOLATED: u8 = 1;

/// One presented entity, as an engine reads it.
///
/// Explicitly ordered widest-first so the layout is the same under every C
/// compiler this could meet, with no implicit padding to disagree about:
/// eight 8-byte fields, then seven 2-byte fields, then two 1-byte fields.
/// That sums to 80, which is already a multiple of the 8-byte alignment, so
/// the record carries no padding at all — implicit or declared. The tests
/// below assert the size and alignment rather than trusting the arithmetic.
///
/// Translation is the protocol's millimetre lattice, grid-relative.
/// Orientation is the protocol's signed-`i16` direction quantization as
/// forward and up vectors; their magnitude carries no meaning, only their
/// direction, so an engine normalises before building a rotation.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrreryObservedEntity {
    /// Stable identity. The engine's own actor map is keyed on this and on
    /// nothing else — there is no engine-native handle in this crossing.
    pub persist_id: u64,
    /// Translation in millimetres, on the lattice's x axis.
    pub x_mm: i64,
    /// Translation in millimetres, on the lattice's y axis.
    pub y_mm: i64,
    /// Translation in millimetres, on the lattice's z axis.
    pub z_mm: i64,
    /// Universe tick of the extraction that carried this entity.
    pub presented_at: u64,
    /// First tick of the basis the transform was produced on.
    pub basis_from: u64,
    /// Last tick of that basis. Equal to `basis_from` for an exact sample.
    pub basis_to: u64,
    /// Tick of the most recent correction, meaningful only when `corrected`.
    pub corrected_at: u64,
    /// Quantized forward direction, x.
    pub forward_x: i16,
    /// Quantized forward direction, y.
    pub forward_y: i16,
    /// Quantized forward direction, z.
    pub forward_z: i16,
    /// Quantized up direction, x.
    pub up_x: i16,
    /// Quantized up direction, y.
    pub up_y: i16,
    /// Quantized up direction, z.
    pub up_z: i16,
    /// Blend factor within the basis, as a unsigned 16-bit normal.
    pub basis_alpha: u16,
    /// [`ORRERY_OBSERVER_PREDICTED`] or [`ORRERY_OBSERVER_INTERPOLATED`].
    pub timeline: u8,
    /// Non-zero when the sidecar has reported a correction for this entity.
    pub corrected: u8,
}

/// What the reader thread and the game thread share.
struct Shared {
    view: Mutex<ObserverView>,
    applied: AtomicU64,
    closed: AtomicBool,
    failed: AtomicBool,
}

/// The handle a C caller holds.
struct Observer {
    shared: Arc<Shared>,
    /// Messages already reported to the caller, so `poll` can return a delta.
    reported: u64,
    poisoned: bool,
}

/// Turn a raw handle back into a reference, refusing null and poison.
///
/// # Safety
///
/// `handle` must be null or a pointer returned by [`orrery_observer_connect`]
/// and not yet passed to [`orrery_observer_destroy`].
unsafe fn observer<'a>(handle: *mut c_void) -> Result<&'a mut Observer, i32> {
    let observer = handle
        .cast::<Observer>()
        .as_mut()
        .ok_or(ORRERY_OBSERVER_BAD_ARGUMENT)?;
    if observer.poisoned {
        return Err(ORRERY_OBSERVER_POISONED);
    }
    Ok(observer)
}

/// Run `body`, converting a panic into a poisoned handle and a status code.
fn guarded(observer: &mut Observer, body: impl FnOnce(&mut Observer) -> i32) -> i32 {
    if let Ok(status) = catch_unwind(AssertUnwindSafe(|| body(observer))) {
        status
    } else {
        observer.poisoned = true;
        ORRERY_OBSERVER_PANIC
    }
}

/// The version of this C ABI the archive was built with.
#[no_mangle]
pub const extern "C" fn orrery_observer_abi_version() -> u32 {
    OBSERVER_ABI_VERSION
}

/// The size one [`OrreryObservedEntity`] occupies, for a caller that would
/// rather assert than trust its own header.
#[no_mangle]
pub extern "C" fn orrery_observer_entity_size() -> u32 {
    u32::try_from(core::mem::size_of::<OrreryObservedEntity>()).unwrap_or(0)
}

/// Dial a serving sidecar at `addr`, e.g. `"127.0.0.1:7899"`.
///
/// Returns null when `addr` is null, is not valid UTF-8, or cannot be dialled.
/// On success the returned handle owns one thread, and must be released with
/// [`orrery_observer_destroy`].
///
/// # Safety
///
/// `addr` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn orrery_observer_connect(addr: *const c_char) -> *mut c_void {
    let Some(addr) = addr.as_ref() else {
        return core::ptr::null_mut();
    };
    let Ok(addr) = CStr::from_ptr(addr).to_str() else {
        return core::ptr::null_mut();
    };
    let Ok(mut link) = ObserverLink::connect(addr) else {
        return core::ptr::null_mut();
    };

    let shared = Arc::new(Shared {
        view: Mutex::new(ObserverView::new()),
        applied: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        failed: AtomicBool::new(false),
    });
    let thread_shared = Arc::clone(&shared);
    // Detached on purpose. `destroy` does not join it: a blocking `read` on a
    // silent sidecar would make the engine's shutdown wait on the network,
    // which is the failure this whole arrangement exists to avoid. The thread
    // holds an `Arc` and exits on its own when the link ends.
    let spawned = thread::Builder::new()
        .name("orrery-observer-link".to_owned())
        .spawn(move || loop {
            match link.poll() {
                Ok(Polled::Applied) => {
                    if let Ok(mut view) = thread_shared.view.lock() {
                        view.clone_from(link.view());
                    }
                    thread_shared.applied.fetch_add(1, Ordering::Release);
                }
                Ok(Polled::Closed) => {
                    thread_shared.closed.store(true, Ordering::Release);
                    return;
                }
                Err(_) => {
                    thread_shared.failed.store(true, Ordering::Release);
                    return;
                }
            }
        });
    if spawned.is_err() {
        return core::ptr::null_mut();
    }

    Box::into_raw(Box::new(Observer {
        shared,
        reported: 0,
        poisoned: false,
    }))
    .cast()
}

/// Take up whatever the link has applied since the last call.
///
/// Writes the number of newly applied messages to `out_applied` when it is
/// non-null. Returns [`ORRERY_OBSERVER_OK`] while the link is live,
/// [`ORRERY_OBSERVER_LINK_CLOSED`] after a clean end of stream, and
/// [`ORRERY_OBSERVER_LINK_FAILED`] after a failure — in both terminal cases
/// the last snapshot remains readable, because the last thing the sidecar
/// presented is still the best thing to draw.
///
/// # Safety
///
/// `handle` must be a live handle from [`orrery_observer_connect`], and
/// `out_applied` null or a writable `uint32_t`.
#[no_mangle]
pub unsafe extern "C" fn orrery_observer_poll(handle: *mut c_void, out_applied: *mut u32) -> i32 {
    let observer = match observer(handle) {
        Ok(observer) => observer,
        Err(status) => return status,
    };
    guarded(observer, |observer| {
        let applied = observer.shared.applied.load(Ordering::Acquire);
        let fresh = applied.saturating_sub(observer.reported);
        observer.reported = applied;
        if let Some(out) = out_applied.as_mut() {
            *out = u32::try_from(fresh).unwrap_or(u32::MAX);
        }
        if observer.shared.failed.load(Ordering::Acquire) {
            ORRERY_OBSERVER_LINK_FAILED
        } else if observer.shared.closed.load(Ordering::Acquire) {
            ORRERY_OBSERVER_LINK_CLOSED
        } else {
            ORRERY_OBSERVER_OK
        }
    })
}

/// Copy the whole presentation set into the caller's buffer.
///
/// Writes the number of entities presented to `out_required` (always, when
/// non-null) and, when `capacity` is large enough, that many records into
/// `out`. A buffer that is too small is [`ORRERY_OBSERVER_TOO_SMALL`] and
/// nothing is written: a renderer sizes from `out_required` and calls again.
/// Passing a null `out` with capacity `0` is the supported way to ask for the
/// size alone.
///
/// # Safety
///
/// `handle` must be live; `out` must be null or point to `capacity` writable
/// [`OrreryObservedEntity`] records; `out_required` null or writable.
#[no_mangle]
pub unsafe extern "C" fn orrery_observer_snapshot(
    handle: *mut c_void,
    out: *mut OrreryObservedEntity,
    capacity: u32,
    out_required: *mut u32,
) -> i32 {
    let observer = match observer(handle) {
        Ok(observer) => observer,
        Err(status) => return status,
    };
    guarded(observer, |observer| {
        let Ok(view) = observer.shared.view.lock() else {
            // The reader thread panicked while holding the lock. There is no
            // consistent set to hand back, and pretending otherwise would put
            // a torn read on screen.
            return ORRERY_OBSERVER_LINK_FAILED;
        };
        let required = u32::try_from(view.len()).unwrap_or(u32::MAX);
        if let Some(slot) = out_required.as_mut() {
            *slot = required;
        }
        if required > capacity {
            return ORRERY_OBSERVER_TOO_SMALL;
        }
        if required == 0 {
            return ORRERY_OBSERVER_OK;
        }
        let Some(out) = out.as_mut() else {
            return ORRERY_OBSERVER_BAD_ARGUMENT;
        };
        let records = core::slice::from_raw_parts_mut(core::ptr::from_mut(out), required as usize);
        for (slot, (id, entity)) in records.iter_mut().zip(view.entities()) {
            *slot = OrreryObservedEntity {
                persist_id: id.0,
                x_mm: entity.transform.translation.x,
                y_mm: entity.transform.translation.y,
                z_mm: entity.transform.translation.z,
                presented_at: entity.presented_at.0,
                basis_from: entity.basis.from.0,
                basis_to: entity.basis.to.0,
                corrected_at: entity.corrected_at.map_or(0, |tick| tick.0),
                forward_x: entity.transform.forward.x,
                forward_y: entity.transform.forward.y,
                forward_z: entity.transform.forward.z,
                up_x: entity.transform.up.x,
                up_y: entity.transform.up.y,
                up_z: entity.transform.up.z,
                basis_alpha: entity.basis.alpha.0,
                timeline: match entity.timeline {
                    Timeline::Predicted => ORRERY_OBSERVER_PREDICTED,
                    Timeline::Interpolated => ORRERY_OBSERVER_INTERPOLATED,
                },
                corrected: u8::from(entity.corrected_at.is_some()),
            };
        }
        ORRERY_OBSERVER_OK
    })
}

/// Release the handle. Accepted on a poisoned handle, and a no-op on null.
///
/// # Safety
///
/// `handle` must be null or a handle from [`orrery_observer_connect`] that has
/// not already been destroyed.
#[no_mangle]
pub unsafe extern "C" fn orrery_observer_destroy(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    drop(Box::from_raw(handle.cast::<Observer>()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::ffi::CString;
    use core::time::Duration;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    use orrery_ipc_transport::FrameWriter;
    use orrery_protocol::{InterpBasis, LatticePoint, PersistId, QuantizedDir, Tick, UNorm16};

    /// The record's layout is a declared fact, not the compiler's discretion:
    /// a C++ module reading these offsets must not be told a different story
    /// by a different toolchain.
    #[test]
    fn the_record_layout_is_what_the_header_declares() {
        assert_eq!(core::mem::size_of::<OrreryObservedEntity>(), 80);
        assert_eq!(core::mem::align_of::<OrreryObservedEntity>(), 8);
        assert_eq!(orrery_observer_entity_size(), 80);
        assert_eq!(orrery_observer_abi_version(), OBSERVER_ABI_VERSION);
    }

    fn batch(tick: u64) -> Vec<u8> {
        use orrery_ipc::{EntityFrame, FrameBatch, QuantizedTransform, SidecarToEngine};
        SidecarToEngine::Frames(FrameBatch {
            extracted_at: Tick::new(tick),
            predicted: vec![EntityFrame {
                persist_id: PersistId::new(1),
                transform: QuantizedTransform {
                    translation: LatticePoint::new(i64::try_from(tick).expect("tick fits"), 0, 0),
                    forward: QuantizedDir::new(1, 0, 0),
                    up: QuantizedDir::new(0, 1, 0),
                },
                basis: InterpBasis::exact(Tick::new(tick)),
            }],
            interpolated: vec![EntityFrame {
                persist_id: PersistId::new(2),
                transform: QuantizedTransform {
                    translation: LatticePoint::new(50, 0, 0),
                    forward: QuantizedDir::new(1, 0, 0),
                    up: QuantizedDir::new(0, 1, 0),
                },
                basis: InterpBasis {
                    from: Tick::new(tick - 3),
                    to: Tick::new(tick),
                    alpha: UNorm16(16_384),
                },
            }],
        })
        .encode()
        .expect("batch encodes")
    }

    /// A stand-in sidecar: real listener, real framing, real codec.
    fn serving(batches: u64) -> core::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let addr = listener.local_addr().expect("bound address");
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the observer dials");
            let mut writer = FrameWriter::new(stream);
            for tick in 100..(100 + batches) {
                if writer.write_frame(&batch(tick)).is_err() {
                    return;
                }
                let _ = writer.flush();
            }
            // Hold the connection open so the link does not close before the
            // test has read the snapshot.
            thread::sleep(Duration::from_secs(2));
        });
        addr
    }

    fn wait_for(handle: *mut c_void, applied: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut total = 0_u64;
        while total < applied {
            assert!(
                Instant::now() < deadline,
                "the link never applied {applied} messages"
            );
            let mut fresh = 0_u32;
            let status = unsafe { orrery_observer_poll(handle, &raw mut fresh) };
            assert_eq!(status, ORRERY_OBSERVER_OK, "the link is live");
            total += u64::from(fresh);
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_snapshot_carries_both_timeline_classes_by_copy_out() {
        let addr = serving(4);
        let c_addr = CString::new(addr.to_string()).expect("no interior NUL");
        let handle = unsafe { orrery_observer_connect(c_addr.as_ptr()) };
        assert!(!handle.is_null(), "the observer dials the stand-in sidecar");
        wait_for(handle, 4);

        // Size-only query first, exactly as a renderer would.
        let mut required = 0_u32;
        let status = unsafe {
            orrery_observer_snapshot(handle, core::ptr::null_mut(), 0, &raw mut required)
        };
        assert_eq!(status, ORRERY_OBSERVER_TOO_SMALL);
        assert_eq!(required, 2, "one predicted capsule and one interpolated");

        let mut buffer = [OrreryObservedEntity::default(); 8];
        let status = unsafe {
            orrery_observer_snapshot(
                handle,
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).expect("small"),
                &raw mut required,
            )
        };
        assert_eq!(status, ORRERY_OBSERVER_OK);
        assert_eq!(required, 2);

        let predicted = buffer[0];
        assert_eq!(predicted.persist_id, 1);
        assert_eq!(predicted.timeline, ORRERY_OBSERVER_PREDICTED);
        assert_eq!(
            predicted.x_mm, 103,
            "the newest batch, applied by overwrite"
        );
        assert_eq!(predicted.basis_from, predicted.basis_to);
        assert_eq!(predicted.corrected, 0);

        let interpolated = buffer[1];
        assert_eq!(interpolated.persist_id, 2);
        assert_eq!(interpolated.timeline, ORRERY_OBSERVER_INTERPOLATED);
        assert_ne!(
            interpolated.basis_from, interpolated.basis_to,
            "an interpolated capsule carries the real bracket, not an exact tick"
        );
        assert_eq!(interpolated.basis_alpha, 16_384);

        unsafe { orrery_observer_destroy(handle) };
    }

    /// The sidecar dying is a status code, not a crash and not a hang: the
    /// last snapshot stays readable, which is what a renderer draws while it
    /// decides what to do.
    #[test]
    fn a_dead_sidecar_becomes_a_status_and_the_last_frame_survives() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let addr = listener.local_addr().expect("bound address");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the observer dials");
            let body = batch(100);
            let mut framed = u32::try_from(body.len())
                .expect("small")
                .to_le_bytes()
                .to_vec();
            framed.extend_from_slice(&body);
            let _ = stream.write_all(&framed);
            let _ = stream.flush();
            // Then vanish, abruptly.
            drop(stream);
        });

        let c_addr = CString::new(addr.to_string()).expect("no interior NUL");
        let handle = unsafe { orrery_observer_connect(c_addr.as_ptr()) };
        assert!(!handle.is_null());

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut status = ORRERY_OBSERVER_OK;
        while status == ORRERY_OBSERVER_OK {
            assert!(Instant::now() < deadline, "the link never ended");
            status = unsafe { orrery_observer_poll(handle, core::ptr::null_mut()) };
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            status, ORRERY_OBSERVER_LINK_CLOSED,
            "a dropped stream at a frame boundary is a clean close"
        );

        let mut buffer = [OrreryObservedEntity::default(); 4];
        let mut required = 0_u32;
        let snapshot =
            unsafe { orrery_observer_snapshot(handle, buffer.as_mut_ptr(), 4, &raw mut required) };
        assert_eq!(snapshot, ORRERY_OBSERVER_OK);
        assert_eq!(required, 2, "the last thing presented is still readable");
        assert_eq!(buffer[0].x_mm, 100);

        unsafe { orrery_observer_destroy(handle) };
    }

    #[test]
    fn a_null_handle_and_an_undialable_address_are_refused_rather_than_crashing() {
        assert_eq!(
            unsafe { orrery_observer_poll(core::ptr::null_mut(), core::ptr::null_mut()) },
            ORRERY_OBSERVER_BAD_ARGUMENT
        );
        assert_eq!(
            unsafe {
                orrery_observer_snapshot(
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    0,
                    core::ptr::null_mut(),
                )
            },
            ORRERY_OBSERVER_BAD_ARGUMENT
        );
        unsafe { orrery_observer_destroy(core::ptr::null_mut()) };

        assert!(unsafe { orrery_observer_connect(core::ptr::null()) }.is_null());
        // Port 1 on loopback, bound by nothing and reachable by no one.
        let refused = CString::new("127.0.0.1:1").expect("no interior NUL");
        assert!(
            TcpStream::connect("127.0.0.1:1").is_err(),
            "fixture: this address must genuinely refuse"
        );
        assert!(unsafe { orrery_observer_connect(refused.as_ptr()) }.is_null());
    }
}
