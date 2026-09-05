//! A9 P-4: kill the observer, and the sidecar's state is unchanged.
//!
//! The claim is about a *process*, so this test has one to kill. A real
//! `orrery-observer` is spawned, dials the sidecar's real listener over real
//! TCP, and is sent `SIGKILL` mid-run — no clean shutdown, no close frame,
//! no notice. What the sidecar sees is what a crashed renderer actually
//! looks like: an `ECONNRESET` on a socket owned by a thread the ruleset has
//! never heard of.
//!
//! # What "unchanged" is checked against
//!
//! Not "nothing obviously broke". The synthetic ruleset advances its one
//! entity by exactly one millimetre per canonical tick
//! (`crates/orrery_synthetic/src/lib.rs:83`), so the whole run is a
//! prediction: after the kill the positions must continue the same arithmetic
//! progression, at consecutive ticks, with no gap, no repetition and no
//! pause. A sidecar that stalled on a dead socket, dropped a tick while the
//! writer thread failed, or replayed one, fails on the sequence rather than
//! on a liveness check that a wedged process could also pass.
//!
//! The second test is the other half of the same property: the sidecar not
//! only survives the death, it is still serving afterwards, and a fresh
//! observer connecting to the same listener is brought fully up to date by
//! the next extraction — because a frames batch is a complete one, so there
//! is no join protocol and nothing to resend.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use lightyear::prelude::P2P;

use common::{session_tick, warm_up, ENTITY};
use orrery_ipc_transport::observer::{ObserverLink, Polled, Timeline};
use orrery_sidecar::{
    secret, sidecar_serving, spawn_predicted, IpcServer, StepObservation, StepTrace,
};

/// Canonical ticks driven before the kill, and again after it.
const RUN: usize = 90;

/// How long any "wait for the other process" loop may take before the test
/// declares the fixture, rather than the property, broken.
const PATIENCE: Duration = Duration::from_secs(20);

/// A sidecar serving on an OS-chosen port, warmed up, holding one entity.
///
/// The listener is bound before the app is built so the port is known to the
/// test; that is the whole reason `sidecar_serving` takes a bound server
/// rather than an address.
fn serving_sidecar(seed: u8) -> (App, std::net::SocketAddr) {
    let server = IpcServer::bind("127.0.0.1:0").expect("an ephemeral port is available");
    let addr = server.bound();
    let key = secret(seed);
    let authority = key.public();
    let mut app = sidecar_serving(key, true, server);
    app.world_mut().spawn(P2P);
    warm_up(&mut app);
    spawn_predicted(&mut app, authority, ENTITY);
    (app, addr)
}

/// Drive the app until `ready` holds, or fail the fixture.
fn drive_until(app: &mut App, what: &str, mut ready: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !ready(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.update();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn stats_written(app: &App) -> u64 {
    app.world()
        .resource::<IpcServer>()
        .stats()
        .written
        .load(std::sync::atomic::Ordering::Relaxed)
}

fn trace(app: &App) -> Vec<StepObservation> {
    app.world().resource::<StepTrace>().0.clone()
}

/// Every step in the window is one tick and one millimetre after the last.
///
/// This is the whole assertion. `StepTrace` is append-only and outside
/// rollback state, so a replayed tick would appear as a repeat rather than
/// overwrite its predecessor, and a dropped one as a gap.
fn assert_unbroken(steps: &[StepObservation], what: &str) {
    assert!(
        steps.len() >= 2,
        "{what}: too few steps to say anything ({})",
        steps.len()
    );
    for pair in steps.windows(2) {
        let [before, after] = pair else {
            unreachable!()
        };
        assert_eq!(
            after.tick,
            before.tick + 1,
            "{what}: the canonical clock skipped or repeated at tick {}",
            before.tick
        );
        assert_eq!(
            after.position_mm,
            before.position_mm + 1,
            "{what}: the ruleset's own progression broke at tick {}",
            before.tick
        );
        assert_eq!(
            after.pose.position.x, after.position_mm,
            "{what}: the pose published for the authority left the rules' value"
        );
    }
}

/// Spawn a real observer process against `addr`, and wait until it says it
/// connected — so a later `SIGKILL` is killing something that was watching.
fn spawn_observer(addr: std::net::SocketAddr) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_orrery-observer"))
        .arg("--addr")
        .arg(addr.to_string())
        .arg("--quiet")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the observer binary is built beside this test");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let first = lines
        .next()
        .expect("the observer prints one line on connect")
        .expect("its stdout is readable");
    assert!(
        first.contains("watching 1 sidecar"),
        "unexpected first line from the observer: {first}"
    );
    child
}

/// The property, end to end.
#[test]
fn killing_the_observer_leaves_the_canonical_run_untouched() {
    let (mut app, addr) = serving_sidecar(51);

    let mut observer = spawn_observer(addr);
    drive_until(&mut app, "the sidecar to accept the observer", |app| {
        app.world().resource::<IpcServer>().streaming()
    });
    drive_until(&mut app, "frames to reach the observer", |app| {
        stats_written(app) >= 8
    });

    // The observed stretch.
    for _ in 0..RUN {
        app.update();
    }
    let observed_tick = session_tick(&app);
    let observed_writes = stats_written(&app);
    assert!(
        observed_writes >= 8,
        "precondition: the observer must actually have been served"
    );

    // The kill. `Child::kill` is `SIGKILL` on Unix: no unwinding, no close.
    observer.kill().expect("the observer process is killed");
    let status = observer.wait().expect("the killed observer is reaped");
    assert!(
        !status.success(),
        "a SIGKILLed process must not report success: {status:?}"
    );

    // The sidecar keeps running. It is given no opportunity to notice: no
    // resource carries the observer's fate into the world, so there is
    // nothing a system could have branched on even if one wanted to.
    for _ in 0..RUN {
        app.update();
    }

    let after = session_tick(&app);
    assert_eq!(
        after - observed_tick,
        u32::try_from(RUN).expect("RUN fits"),
        "the canonical clock must advance exactly one tick per update, kill or no kill"
    );

    assert_unbroken(&trace(&app), "across the kill");

    // The kill was real, and it was absorbed where it belongs: on the serving
    // thread, as a failed write or a lost connection. This is the only place
    // in the process that knows the observer died.
    drive_until(&mut app, "the serving thread to notice the death", |app| {
        !app.world().resource::<IpcServer>().streaming()
    });
    let server = app.world().resource::<IpcServer>();
    assert_eq!(
        server
            .stats()
            .accepted
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "exactly one observer was ever accepted"
    );

    // And the rules kept going while it noticed.
    let before_idle = session_tick(&app);
    for _ in 0..RUN {
        app.update();
    }
    assert_eq!(
        session_tick(&app) - before_idle,
        u32::try_from(RUN).expect("RUN fits"),
        "an unobserved sidecar runs at the same rate as an observed one"
    );
    assert_unbroken(&trace(&app), "after the observer is gone");
}

/// A second dial *replaces* the first rather than queueing behind it.
///
/// This test exists because the first cut of the serving thread did the other
/// thing. One thread did both `accept` and the writing, so a dial while an
/// observer was connected sat in the listen backlog until that observer died
/// — and an observer that *hangs* rather than dies would have held the
/// listener for as long as it lived. It was found by an Unreal observer
/// running with `-ObserverTicks=0`: the probe that was meant to read the
/// sidecar's tick across the kill connected, was accepted by the kernel, and
/// then waited forever for a frame. The behaviour the docs described was
/// correct; the implementation was not, and only a second *live* observer
/// could tell them apart.
#[test]
fn a_second_dial_replaces_the_first_rather_than_queueing_behind_it() {
    let (mut app, addr) = serving_sidecar(53);

    let mut first = ObserverLink::connect(addr).expect("the listener accepts");
    drive_until(&mut app, "the first observer to be accepted", |app| {
        app.world().resource::<IpcServer>().streaming()
    });
    drive_until(&mut app, "frames to reach the first observer", |app| {
        stats_written(app) >= 8
    });

    // The first observer is alive and well — it is simply not reading. A
    // queueing server would leave the second dial invisible behind it.
    let second = ObserverLink::connect(addr).expect("the listener accepts a second dial");
    drive_until(&mut app, "the replacement to be served", |app| {
        app.world()
            .resource::<IpcServer>()
            .stats()
            .accepted
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 2
    });

    // The newcomer gets frames, without the incumbent having to die first.
    let reader = std::thread::spawn(move || {
        let mut second = second;
        while second.view().frames_applied() == 0 {
            assert_eq!(
                second.poll().expect("the replacement is served"),
                Polled::Applied
            );
        }
        second
    });
    let deadline = Instant::now() + PATIENCE;
    while !reader.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the second dial never received a frame: it queued behind the first"
        );
        app.update();
        std::thread::sleep(Duration::from_millis(1));
    }
    let second = reader.join().expect("the reader thread does not panic");
    assert!(
        second.view().get(ENTITY).is_some(),
        "the replacement sees the entity from its first complete extraction"
    );

    // And the displaced one is told, by its stream ending, rather than left
    // waiting on a socket nothing will ever write to again.
    let mut drained = 0;
    loop {
        assert!(drained < 10_000, "the displaced observer was never closed");
        match first.poll() {
            Ok(Polled::Applied) => drained += 1,
            Ok(Polled::Closed) => break,
            Err(_) => break,
        }
    }
    assert_unbroken(&trace(&app), "across the replacement dial");
}

/// The listener outlives the observer, and a replacement is correct from its
/// first frames batch — no join protocol, nothing resent.
#[test]
fn a_replacement_observer_is_served_after_the_first_one_dies() {
    let (mut app, addr) = serving_sidecar(52);

    let mut first = spawn_observer(addr);
    drive_until(&mut app, "the first observer to be accepted", |app| {
        app.world().resource::<IpcServer>().streaming()
    });
    drive_until(&mut app, "frames to reach the first observer", |app| {
        stats_written(app) >= 8
    });
    first.kill().expect("the first observer is killed");
    let _ = first.wait();
    drive_until(&mut app, "the serving thread to notice the death", |app| {
        !app.world().resource::<IpcServer>().streaming()
    });

    // A second observer, dialled from this thread so the frames can be read
    // back and checked rather than merely counted.
    let mut link = ObserverLink::connect(addr).expect("the listener is still open");
    drive_until(&mut app, "the replacement to be accepted", |app| {
        app.world().resource::<IpcServer>().streaming()
    });
    // One update to produce a batch for it; the reader below blocks until the
    // batch arrives, and the driving updates keep the sidecar producing.
    let reader = std::thread::spawn(move || {
        assert_eq!(link.poll().expect("a batch arrives"), Polled::Applied);
        // Frames first, then whatever membership batch rode with it.
        while link.view().frames_applied() == 0 {
            assert_eq!(link.poll().expect("a batch arrives"), Polled::Applied);
        }
        link
    });
    let deadline = Instant::now() + PATIENCE;
    while !reader.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the replacement was never served"
        );
        app.update();
        std::thread::sleep(Duration::from_millis(1));
    }
    let link = reader.join().expect("the reader thread does not panic");

    let view = link.view();
    let entity = view
        .get(ENTITY)
        .expect("the replacement sees the entity from its first complete extraction");
    assert_eq!(
        entity.timeline,
        Timeline::Predicted,
        "the sidecar's own entity is presented by the predicted class"
    );
    assert!(
        entity.transform.translation.x > 0,
        "the replacement is handed the run in progress, not a run from zero"
    );

    let server = app.world().resource::<IpcServer>();
    assert_eq!(
        server
            .stats()
            .accepted
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the listener served two observers across one death"
    );
    assert_unbroken(&trace(&app), "across the replacement");
}
