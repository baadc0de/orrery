//! D32 clause (c)'s 2 s bound and clause (i)'s reader-side authentication,
//! measured against a live FoundationDB cluster and the shipped `orrery-ramp`
//! binary — not against the gate harness's `--posture-file` lever.
//!
//! #875's acceptance evidence asks for the decision-to-effect number to be
//! taken "against a real binary". So the *writer* here is the compiled
//! `orrery-ramp` executable, invoked as an operator would invoke it, and the
//! *reader* is `FdbRampPostureStore::read`, which is the exact call every
//! shipped poller makes and where clause (i)'s verification lives. What is
//! reproduced rather than linked is the twelve-line tokio ticker around that
//! call: `spawn_authority_correction_poller` is private to the `persistd`
//! binary and an integration test cannot name it. The ticker is stated inline
//! below so the substitution is visible rather than implied.
//!
//! These tests self-skip without a cluster, like every other FDB-gated test in
//! this crate. They write and clear `ramp/strikes` only.

mod support;

#[cfg(feature = "fdb")]
use std::process::Command;
#[cfg(feature = "fdb")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "fdb")]
use std::time::{Duration, Instant};

#[cfg(feature = "fdb")]
use orrery_persistd::gateway::STRIKES_CONTROL;
#[cfg(feature = "fdb")]
use orrery_persistd::intent::posture::{self, PostureRefusal, PostureVerdict};
#[cfg(feature = "fdb")]
use orrery_persistd::intent::{FdbRampPostureStore, PostureSource, RampMode, RampPosture};
#[cfg(feature = "fdb")]
use orrery_persistd::FdbContext;

/// Serialises this file's tests against each other.
///
/// All four drive the *real* `ramp/strikes` row rather than a synthetic control
/// name, because "de-hardening" and the startup-default table are properties of
/// the real control names and a test on a made-up one would be testing a
/// different rule. One row and four tests means they must take turns: two
/// pollers watching the same key while two writers alternate its mode is a race
/// with no correct answer, and it fails the way a real race does — sometimes.
#[cfg(feature = "fdb")]
fn ramp_row_guard() -> &'static tokio::sync::Mutex<()> {
    static GUARD: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GUARD.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// D32 clause (c): "one poll interval plus apply, bounded at 2 s wall clock".
#[cfg(feature = "fdb")]
const CLAUSE_C_BOUND: Duration = Duration::from_millis(2_000);

/// The poll interval the gateway's maintenance sweep already runs at.
#[cfg(feature = "fdb")]
const POLL_INTERVAL: Duration = Duration::from_millis(1_000);

#[cfg(feature = "fdb")]
fn operator_secret() -> iroh::SecretKey {
    iroh::SecretKey::from_bytes(&[42; 32])
}

#[cfg(feature = "fdb")]
fn ramp_binary() -> &'static str {
    env!("CARGO_BIN_EXE_orrery-ramp")
}

/// Write the operator secret where the shipped binary expects it: a file, not
/// an argument, because a secret in `argv` is a secret in every process listing.
#[cfg(feature = "fdb")]
fn secret_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("operator.key");
    let hex: String = operator_secret()
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    std::fs::write(&path, hex).expect("write operator secret");
    path
}

/// Invoke the shipped writer exactly as an operator would.
#[cfg(feature = "fdb")]
fn ramp_set(cluster: &str, secret: &std::path::Path, mode: &str) -> std::process::Output {
    Command::new(ramp_binary())
        .args([
            "--fdb-cluster-file",
            cluster,
            "set",
            "--control",
            STRIKES_CONTROL,
            "--mode",
            mode,
            "--reason",
            "clause (c) decision-to-effect measurement",
            "--operator-secret-file",
            &secret.display().to_string(),
        ])
        .output()
        .expect("run orrery-ramp")
}

#[cfg(feature = "fdb")]
fn ramp_clear(cluster: &str) -> std::process::Output {
    Command::new(ramp_binary())
        .args([
            "--fdb-cluster-file",
            cluster,
            "clear",
            "--control",
            STRIKES_CONTROL,
        ])
        .output()
        .expect("run orrery-ramp clear")
}

/// The poller under measurement.
///
/// This is the shipped refresh — `FdbRampPostureStore::read`, verification and
/// all — on the shipped cadence, and nothing else. It records the instant the
/// mode it is acting under changes, which is what "effect" means in clause
/// (c)'s "from an operator's decision to a control stopped in a running fleet".
#[cfg(feature = "fdb")]
fn spawn_poller(
    store: Arc<FdbRampPostureStore>,
    startup_default: RampMode,
    observed: Arc<Mutex<Vec<(RampMode, Instant)>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut acting = startup_default;
        observed
            .lock()
            .expect("lock")
            .push((acting, Instant::now()));
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let Ok(row) = store.read(STRIKES_CONTROL).await else {
                // A transaction failure retains the last known mode; a refused
                // row does not, and that difference is clause (i)'s.
                continue;
            };
            let mode = row.map_or(startup_default, |posture| posture.mode);
            if mode != acting {
                acting = mode;
                observed
                    .lock()
                    .expect("lock")
                    .push((acting, Instant::now()));
            }
        }
    })
}

#[cfg(feature = "fdb")]
async fn await_mode(
    observed: &Arc<Mutex<Vec<(RampMode, Instant)>>>,
    expected: RampMode,
    deadline: Duration,
) -> Option<Instant> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if let Some((mode, at)) = observed.lock().expect("lock").last().copied() {
            if mode == expected {
                return Some(at);
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    None
}

/// #875's headline acceptance: an authenticated operator write is observed by a
/// running poller inside clause (c)'s 2 s, measured against the real binary and
/// a real cluster.
#[cfg(feature = "fdb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authenticated_operator_write_takes_effect_within_clause_cs_two_seconds() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let _serialised = ramp_row_guard().lock().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = secret_file(dir.path());
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");

    // The process trusts exactly the operator key the tool signs with, which is
    // the deployment `--operator-key` describes.
    let store = Arc::new(
        FdbRampPostureStore::from_context(&context)
            .with_operator_keys([operator_secret().public()]),
    );
    store.clear(STRIKES_CONTROL).await.expect("start clean");

    let observed = Arc::new(Mutex::new(Vec::new()));
    let poller = spawn_poller(Arc::clone(&store), RampMode::Off, Arc::clone(&observed));

    // Five rounds, alternating between two modes so every round is a real
    // change rather than a repeat. Both are promotions above C5's `off`
    // default, so neither needs an expiry — clause (f)'s asymmetry.
    let mut worst = Duration::ZERO;
    let mut rounds = Vec::new();
    for round in 0..5 {
        let target = if round % 2 == 0 {
            (RampMode::Live, "live")
        } else {
            (RampMode::Shadow, "shadow")
        };
        let decided_at = Instant::now();
        let output = ramp_set(&cluster, &secret, target.1);
        assert!(
            output.status.success(),
            "orrery-ramp set failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let effect_at = await_mode(&observed, target.0, CLAUSE_C_BOUND * 3)
            .await
            .unwrap_or_else(|| panic!("round {round}: the fleet never applied {:?}", target.0));
        let elapsed = effect_at.duration_since(decided_at);
        rounds.push(elapsed);
        worst = worst.max(elapsed);
    }
    poller.abort();

    for (round, elapsed) in rounds.iter().enumerate() {
        eprintln!(
            "round {round}: decision -> effect {:.1} ms",
            elapsed.as_secs_f64() * 1_000.0
        );
    }
    eprintln!(
        "worst {:.4} s against clause (c)'s 2 s bound",
        worst.as_secs_f64()
    );
    assert!(
        worst < CLAUSE_C_BOUND,
        "worst decision-to-effect {worst:?} exceeds D32 clause (c)'s 2 s bound"
    );

    store.clear(STRIKES_CONTROL).await.expect("leave clean");
}

/// #875's second acceptance: removing the row restores the startup default.
#[cfg(feature = "fdb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_the_row_restores_the_startup_default() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let _serialised = ramp_row_guard().lock().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = secret_file(dir.path());
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let store = Arc::new(
        FdbRampPostureStore::from_context(&context)
            .with_operator_keys([operator_secret().public()]),
    );
    store.clear(STRIKES_CONTROL).await.expect("start clean");

    let observed = Arc::new(Mutex::new(Vec::new()));
    let poller = spawn_poller(Arc::clone(&store), RampMode::Off, Arc::clone(&observed));

    assert!(ramp_set(&cluster, &secret, "live").status.success());
    assert!(
        await_mode(&observed, RampMode::Live, CLAUSE_C_BOUND * 3)
            .await
            .is_some(),
        "the signed promotion must land before removal means anything"
    );

    let removed_at = Instant::now();
    let output = ramp_clear(&cluster);
    assert!(output.status.success());
    let restored_at = await_mode(&observed, RampMode::Off, CLAUSE_C_BOUND * 3)
        .await
        .expect("removing the row restores the CLI startup default");
    assert!(
        restored_at.duration_since(removed_at) < CLAUSE_C_BOUND,
        "the default returns inside the same bound the write does"
    );
    poller.abort();
}

/// Clause (i)'s point, against a real cluster: possession of the cluster file
/// is not authority over fleet enforcement posture.
#[cfg(feature = "fdb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_cluster_file_write_is_refused_by_a_running_poller() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let _serialised = ramp_row_guard().lock().await;
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let store = FdbRampPostureStore::from_context(&context)
        .with_operator_keys([operator_secret().public()]);
    store.clear(STRIKES_CONTROL).await.expect("start clean");

    // Exactly what a cluster-file holder can produce: the row, correctly
    // shaped, claiming `Operator`, and unsigned — because producing a signature
    // is the one thing they cannot do.
    let forged = posture::SignedRampPosture {
        posture: RampPosture {
            mode: RampMode::Off,
            source: PostureSource::Operator,
            set_at_ms: 1,
            reason: "silence enforcement fleet-wide".to_string(),
            incident_id: None,
        },
        expires_at_ms: None,
        signer: None,
        signature: None,
    };
    store
        .write(STRIKES_CONTROL, &forged)
        .await
        .expect("the cluster file does let you write bytes");

    let seen = store
        .read(STRIKES_CONTROL)
        .await
        .expect("a refused row is not a transaction error");
    assert_eq!(
        seen, None,
        "clause (i): a refused row is reported as an absent one, so the control \
         falls back to the startup default an operator chose at launch — the \
         forger's `off` never reaches a poller, and neither does any other mode \
         the forger could have selected instead"
    );

    // The same row, through the poller-facing seam every control reads: the two
    // refusals compose, and neither is reachable around the other.
    assert_eq!(
        orrery_persistd::intent::ramp::admitted(seen.as_ref(), STRIKES_CONTROL),
        None
    );

    // "Absent" is the right *outcome* for a refusal and a lousy assertion on
    // its own: a store that returned `None` for everything — a broken refresh —
    // would satisfy every line above. So prove the refusal is selective by
    // showing the same store admits a correctly signed row at the same key.
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = secret_file(dir.path());
    assert!(ramp_set(&cluster, &secret, "live").status.success());
    let admitted = store
        .read(STRIKES_CONTROL)
        .await
        .expect("read the signed row")
        .expect("a signed row from a trusted key is admitted, or the refusal above proves nothing");
    assert_eq!(admitted.mode, RampMode::Live);
    assert_eq!(admitted.source, PostureSource::Operator);

    store.clear(STRIKES_CONTROL).await.expect("leave clean");
}

/// The migration hazard, against the real store rather than against a buffer.
///
/// The pre-amendment reader is reproduced here — `postcard::from_bytes::
/// <RampPosture>` over the raw value — because the code it models has been
/// replaced and cannot be called. What is real is the value: it is the bytes
/// the shipped writer puts in FoundationDB.
#[cfg(feature = "fdb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_un_upgraded_reader_refuses_the_bytes_the_writer_stores() {
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let _serialised = ramp_row_guard().lock().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = secret_file(dir.path());
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let store = FdbRampPostureStore::from_context(&context);
    store.clear(STRIKES_CONTROL).await.expect("start clean");

    assert!(ramp_set(&cluster, &secret, "live").status.success());

    let db = context.database();
    let key = orrery_persistd::keyspace::ramp_key(STRIKES_CONTROL);
    let raw: Vec<u8> = db
        .run(move |transaction, _| {
            let key = key.clone();
            async move {
                Ok(transaction
                    .get(&key, false)
                    .await?
                    .map(|bytes| bytes.as_ref().to_vec())
                    .unwrap_or_default())
            }
        })
        .await
        .expect("read the stored value");

    assert_eq!(
        raw.first().copied(),
        Some(posture::RAMP_POSTURE_SCHEMA),
        "the D38 schema tag is the first byte a prefix decoder meets"
    );
    assert!(
        postcard::from_bytes::<RampPosture>(&raw).is_err(),
        "an un-upgraded gateway REFUSES a signed row rather than half-reading it \
         and applying the mode without checking the signature — this is the \
         rolling-upgrade hazard the tag exists for"
    );

    // And a process trusting no operator key admits nothing, so the flag is not
    // decoration either.
    assert_eq!(
        posture::verdict(STRIKES_CONTROL, Some(&raw), &[], 0),
        PostureVerdict::Refused(PostureRefusal::UnknownSigner)
    );

    store.clear(STRIKES_CONTROL).await.expect("leave clean");
}
