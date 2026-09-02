//! D32 open question 1 spike — how an operator authenticates a `ramp/{control}`
//! posture write.
//!
//! Propose-only. This binary is NOT the writer. It exists so that the owner
//! decides open question 1 against a running mechanism rather than against an
//! ergonomics opinion, and it is deliberately outside the workspace so nothing
//! in the tree can depend on it.
//!
//! It runs three candidate mechanisms end to end against a real FoundationDB
//! cluster, plus the clause (c) latency measurement and the clause (f)
//! direction rule.
//!
//! Row shape and key are the tree's, not invented here:
//!   key   = b"vr" ‖ control                     (keyspace.rs `ramp_key`)
//!   value = postcard(RampPosture)               (intent/ramp.rs `read`)

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// The tree's row vocabulary, mirrored exactly so bytes written here are bytes
// `FdbRampPostureStore::read` accepts. Field order is load-bearing: postcard is
// a non-self-describing format.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum RampMode {
    Off,
    Shadow,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum PostureSource {
    Default,
    Operator,
    AutoSuspend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RampPosture {
    mode: RampMode,
    source: PostureSource,
    set_at_ms: u64,
    reason: String,
    incident_id: Option<[u8; 16]>,
}

fn ramp_key(control: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + control.len());
    key.extend_from_slice(b"vr");
    key.extend_from_slice(control.as_bytes());
    key
}

// ─────────────────────────────────────────────────────────────────────────────
// M3's proposed row: the same row with an authenticator appended. Because
// postcard is positional, the added fields sit AFTER every field the landed
// reader consumes, so a landed `FdbRampPostureStore::read` still decodes the
// prefix. That property is measured in `probe_prefix_compatibility`.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SignedRampPosture {
    // ── the landed prefix, byte-for-byte ──
    mode: RampMode,
    source: PostureSource,
    set_at_ms: u64,
    reason: String,
    incident_id: Option<[u8; 16]>,
    // ── the authenticator ──
    /// Which operator key signed this row. Verifiers hold the key set.
    signer: [u8; 32],
    /// Ed25519 over `posture_preimage`, as two halves because serde has no
    /// impl for `[u8; 64]`. A real row would use `serde_big_array`.
    signature: [[u8; 32]; 2],
    /// Mandatory for a de-hardening write; `None` otherwise. After this
    /// instant every poller reverts to its CLI startup default.
    expires_at_ms: Option<u64>,
}

/// The signed preimage. Domain-separated so a posture signature can never be
/// replayed as any other Orrery signature, and it binds the control name so a
/// row signed for `authority_correction` cannot be moved to `strikes`.
fn posture_preimage(control: &str, row: &SignedRampPosture) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"orrery/d32/ramp-posture/v1\0");
    hasher.update(&(control.len() as u32).to_le_bytes());
    hasher.update(control.as_bytes());
    hasher.update(&[match row.mode {
        RampMode::Off => 0,
        RampMode::Shadow => 1,
        RampMode::Live => 2,
    }]);
    hasher.update(&[match row.source {
        PostureSource::Default => 0,
        PostureSource::Operator => 1,
        PostureSource::AutoSuspend => 2,
    }]);
    hasher.update(&row.set_at_ms.to_le_bytes());
    hasher.update(&(row.reason.len() as u32).to_le_bytes());
    hasher.update(row.reason.as_bytes());
    match row.incident_id {
        None => hasher.update(&[0u8]),
        Some(id) => {
            hasher.update(&[1u8]);
            hasher.update(&id)
        }
    };
    match row.expires_at_ms {
        None => hasher.update(&[0u8]),
        Some(at) => {
            hasher.update(&[1u8]);
            hasher.update(&at.to_le_bytes())
        }
    };
    hasher.finalize().as_bytes().to_vec()
}

// ─────────────────────────────────────────────────────────────────────────────
// The clause (f) direction rule, made a predicate instead of a convention.
// ─────────────────────────────────────────────────────────────────────────────

/// Hardening rank. Higher acts more; this is NOT "safer".
fn rank(mode: RampMode) -> u8 {
    match mode {
        RampMode::Off => 0,
        RampMode::Shadow => 1,
        RampMode::Live => 2,
    }
}

/// D32 clause (c)'s startup default table.
fn d32_default(control: &str) -> RampMode {
    match control {
        // C2 is the only control that ships live.
        "quarantine_validation" => RampMode::Live,
        _ => RampMode::Off,
    }
}

/// Is this write de-hardening — i.e. does it leave the fleet acting *less*
/// than D32's own default for the control?
///
/// This is the distinction #875 insists on. For C1/C4/C5 the default is `off`,
/// so no write can go below it and nothing is de-hardening. For C2 the default
/// is `live` because C2 is already live, so both `shadow` and `off` reduce
/// hardening below shipped behaviour.
fn is_de_hardening(control: &str, mode: RampMode) -> bool {
    rank(mode) < rank(d32_default(control))
}

#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    BadSignature,
    UnknownSigner,
    Unsigned,
    AutoSuspendMayNotPromote,
    DeHardeningNeedsExpiry,
    C2OffArmDoesNotExist,
    Expired,
}

/// M3's admission predicate, run by every poller before a row may take effect.
fn admit(
    control: &str,
    row: &SignedRampPosture,
    operator_keys: &[VerifyingKey],
    now_ms: u64,
    current: RampMode,
) -> Result<RampMode, Refusal> {
    // D32 open question 3, answered in the negative by this spike's
    // recommendation: C2 has no `off` arm, so no row can select it.
    if control == "quarantine_validation" && row.mode == RampMode::Off {
        return Err(Refusal::C2OffArmDoesNotExist);
    }

    // Clause (f): automation may make the fleet safer without asking, never
    // less safe. An AutoSuspend row is accepted unsigned *only* as a demotion.
    if row.source == PostureSource::AutoSuspend {
        if rank(row.mode) >= rank(current) {
            return Err(Refusal::AutoSuspendMayNotPromote);
        }
        return Ok(row.mode);
    }

    // Every operator row is authenticated at read time, by the consumer.
    if row.signature == [[0u8; 32]; 2] {
        return Err(Refusal::Unsigned);
    }
    let key = VerifyingKey::from_bytes(&row.signer).map_err(|_| Refusal::UnknownSigner)?;
    if !operator_keys.contains(&key) {
        return Err(Refusal::UnknownSigner);
    }
    let signature = Signature::from_bytes(&join(row.signature));
    key.verify(&posture_preimage(control, row), &signature)
        .map_err(|_| Refusal::BadSignature)?;

    // A de-hardening write must say when it stops. An incident demotion that
    // outlives its incident is how a ramp silently un-ships its own hardening.
    if is_de_hardening(control, row.mode) {
        match row.expires_at_ms {
            None => return Err(Refusal::DeHardeningNeedsExpiry),
            Some(at) if at <= now_ms => return Err(Refusal::Expired),
            Some(_) => {}
        }
    }
    if let Some(at) = row.expires_at_ms
        && at <= now_ms
    {
        return Err(Refusal::Expired);
    }
    Ok(row.mode)
}

// ─────────────────────────────────────────────────────────────────────────────
// Harness
// ─────────────────────────────────────────────────────────────────────────────

fn split(sig: [u8; 64]) -> [[u8; 32]; 2] {
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    a.copy_from_slice(&sig[..32]);
    b.copy_from_slice(&sig[32..]);
    [a, b]
}

fn join(sig: [[u8; 32]; 2]) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&sig[0]);
    out[32..].copy_from_slice(&sig[1]);
    out
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

fn plain(mode: RampMode, source: PostureSource, reason: &str) -> RampPosture {
    RampPosture {
        mode,
        source,
        set_at_ms: now_ms(),
        reason: reason.into(),
        incident_id: None,
    }
}

fn signed(
    control: &str,
    mode: RampMode,
    source: PostureSource,
    reason: &str,
    expires_at_ms: Option<u64>,
    key: Option<&SigningKey>,
) -> SignedRampPosture {
    let mut row = SignedRampPosture {
        mode,
        source,
        set_at_ms: now_ms(),
        reason: reason.into(),
        incident_id: None,
        signer: key.map_or([0u8; 32], |k| k.verifying_key().to_bytes()),
        signature: [[0u8; 32]; 2],
        expires_at_ms,
    };
    if let Some(k) = key {
        row.signature = split(k.sign(&posture_preimage(control, &row)).to_bytes());
    }
    row
}

struct Db(Arc<foundationdb::Database>);

impl Db {
    async fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        let db = Arc::clone(&self.0);
        db.run(move |tx, _| {
            let (key, value) = (key.clone(), value.clone());
            async move {
                tx.set(&key, &value);
                Ok(())
            }
        })
        .await
        .expect("write");
    }

    async fn get(&self, key: Vec<u8>) -> Option<Vec<u8>> {
        let db = Arc::clone(&self.0);
        db.run(move |tx, _| {
            let key = key.clone();
            async move { Ok(tx.get(&key, false).await?.map(|v| v.to_vec())) }
        })
        .await
        .expect("read")
    }

    async fn clear(&self, key: Vec<u8>) {
        let db = Arc::clone(&self.0);
        db.run(move |tx, _| {
            let key = key.clone();
            async move {
                tx.clear(&key);
                Ok(())
            }
        })
        .await
        .expect("clear");
    }
}

macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if $cond { println!("  PASS  {}", format!($($arg)*)); }
        else { println!("  FAIL  {}", format!($($arg)*)); FAILED.fetch_add(1, Ordering::Relaxed); }
    };
}

static FAILED: AtomicU8 = AtomicU8::new(0);

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let _guard = unsafe { foundationdb::boot() };
    let cluster = std::env::var("FDB_CLUSTER_FILE").expect("FDB_CLUSTER_FILE");
    let db = Db(Arc::new(
        foundationdb::Database::from_path(&cluster).expect("open cluster"),
    ));
    println!("D32 OQ1 spike — cluster {cluster}\n");

    let mut rng = rand::thread_rng();
    let operator = SigningKey::generate(&mut rng);
    let attacker = SigningKey::generate(&mut rng);
    let keys = vec![operator.verifying_key()];

    // ── M1: direct FDB write by an ops tool ────────────────────────────────
    println!("M1  direct FDB write (authority = possession of the cluster file)");
    let c5 = ramp_key("strikes");
    db.clear(c5.clone()).await;
    let row = plain(RampMode::Live, PostureSource::Operator, "promote C5");
    db.put(c5.clone(), postcard::to_allocvec(&row).expect("encode"))
        .await;
    let read: RampPosture =
        postcard::from_bytes(&db.get(c5.clone()).await.expect("row present")).expect("decode");
    check!(read == row, "a cluster-file holder sets ramp/strikes and it reads back");
    check!(
        read.source == PostureSource::Operator,
        "the row claims source=Operator and names no operator — the audit gap"
    );
    // The same authority can silence enforcement.
    let off = plain(RampMode::Off, PostureSource::Operator, "silence C5");
    db.put(c5.clone(), postcard::to_allocvec(&off).expect("encode"))
        .await;
    let read: RampPosture =
        postcard::from_bytes(&db.get(c5.clone()).await.expect("row")).expect("decode");
    check!(
        read.mode == RampMode::Off,
        "the same unauthenticated authority silences C5 fleet-wide"
    );

    // ── M2: signed envelope verified at WRITE time ─────────────────────────
    println!("\nM2  signed envelope, verified by a privileged writer, plain row stored");
    db.clear(c5.clone()).await;
    // The writer verifies, then commits the landed plain row.
    let envelope = signed(
        "strikes",
        RampMode::Shadow,
        PostureSource::Operator,
        "M2 demote",
        None,
        Some(&operator),
    );
    let verified = VerifyingKey::from_bytes(&envelope.signer)
        .ok()
        .filter(|k| keys.contains(k))
        .and_then(|k| {
            k.verify(
                &posture_preimage("strikes", &envelope),
                &Signature::from_bytes(&join(envelope.signature)),
            )
            .ok()
        })
        .is_some();
    check!(verified, "a correctly signed envelope is accepted by the writer");

    let forged = signed(
        "strikes",
        RampMode::Off,
        PostureSource::Operator,
        "forged",
        None,
        Some(&attacker),
    );
    let forged_ok = VerifyingKey::from_bytes(&forged.signer)
        .ok()
        .filter(|k| keys.contains(k))
        .is_some();
    check!(!forged_ok, "an envelope signed by a non-operator key is refused");

    // and now the residual gap, demonstrated rather than asserted:
    db.put(
        c5.clone(),
        postcard::to_allocvec(&plain(RampMode::Off, PostureSource::Operator, "bypass"))
            .expect("encode"),
    )
    .await;
    let read: RampPosture =
        postcard::from_bytes(&db.get(c5.clone()).await.expect("row")).expect("decode");
    check!(
        read.mode == RampMode::Off,
        "M2's guarantee is bypassed entirely by writing the row directly — it \
         authenticates the API, not the row"
    );

    // ── M3: signed row verified at READ time by every poller ───────────────
    println!("\nM3  signature stored in the row, verified by every poller before apply");
    db.clear(c5.clone()).await;
    // The M1/M2 bypass, replayed against an M3 verifier.
    db.put(
        c5.clone(),
        postcard::to_allocvec(&signed(
            "strikes",
            RampMode::Off,
            PostureSource::Operator,
            "bypass",
            None,
            None,
        ))
        .expect("encode"),
    )
    .await;
    let raw: SignedRampPosture =
        postcard::from_bytes(&db.get(c5.clone()).await.expect("row")).expect("decode");
    check!(
        admit("strikes", &raw, &keys, now_ms(), RampMode::Live) == Err(Refusal::Unsigned),
        "a raw cluster-file write is REFUSED by the poller — FDB access is no \
         longer authority over fleet enforcement"
    );
    let forged_row = signed(
        "strikes",
        RampMode::Off,
        PostureSource::Operator,
        "forged",
        None,
        Some(&attacker),
    );
    check!(
        admit("strikes", &forged_row, &keys, now_ms(), RampMode::Live)
            == Err(Refusal::UnknownSigner),
        "a row signed by an unknown key is refused"
    );
    let mut tampered = signed(
        "strikes",
        RampMode::Shadow,
        PostureSource::Operator,
        "demote",
        None,
        Some(&operator),
    );
    tampered.mode = RampMode::Off;
    check!(
        admit("strikes", &tampered, &keys, now_ms(), RampMode::Live) == Err(Refusal::BadSignature),
        "flipping the mode after signing is refused"
    );
    let mut moved = signed(
        "authority_correction",
        RampMode::Off,
        PostureSource::Operator,
        "demote C4",
        None,
        Some(&operator),
    );
    moved.source = PostureSource::Operator;
    check!(
        admit("strikes", &moved, &keys, now_ms(), RampMode::Live) == Err(Refusal::BadSignature),
        "a row signed for C4 cannot be moved to C5's key — the control is bound"
    );
    let good = signed(
        "strikes",
        RampMode::Shadow,
        PostureSource::Operator,
        "demote",
        None,
        Some(&operator),
    );
    check!(
        admit("strikes", &good, &keys, now_ms(), RampMode::Live) == Ok(RampMode::Shadow),
        "a correctly signed operator row is admitted"
    );

    // ── clause (f): auto-suspend may demote, never promote ─────────────────
    println!("\nclause (f)  the direction rule as a verification predicate");
    let trip = signed(
        "strikes",
        RampMode::Shadow,
        PostureSource::AutoSuspend,
        "verdict spike",
        None,
        None,
    );
    check!(
        admit("strikes", &trip, &keys, now_ms(), RampMode::Live) == Ok(RampMode::Shadow),
        "an unsigned AutoSuspend row demoting live->shadow is admitted"
    );
    let bad_trip = signed(
        "strikes",
        RampMode::Live,
        PostureSource::AutoSuspend,
        "promote",
        None,
        None,
    );
    check!(
        admit("strikes", &bad_trip, &keys, now_ms(), RampMode::Live)
            == Err(Refusal::AutoSuspendMayNotPromote),
        "an unsigned AutoSuspend row that would PROMOTE is refused"
    );

    // ── #875's hazard: C2 is the de-hardening lever ────────────────────────
    println!("\n#875 hazard  C2 is the only control whose demotion weakens the fleet");
    check!(
        !is_de_hardening("strikes", RampMode::Off),
        "C5 off is not de-hardening: D32's own default for C5 is off"
    );
    check!(
        is_de_hardening("quarantine_validation", RampMode::Shadow),
        "C2 shadow IS de-hardening: D32's default for C2 is live"
    );
    let c2_off = signed(
        "quarantine_validation",
        RampMode::Off,
        PostureSource::Operator,
        "mass-quarantine bug",
        Some(now_ms() + 3_600_000),
        Some(&operator),
    );
    check!(
        admit("quarantine_validation", &c2_off, &keys, now_ms(), RampMode::Live)
            == Err(Refusal::C2OffArmDoesNotExist),
        "C2's off arm does not exist — a correctly signed row still cannot select it (OQ3)"
    );
    let c2_shadow_forever = signed(
        "quarantine_validation",
        RampMode::Shadow,
        PostureSource::Operator,
        "incident",
        None,
        Some(&operator),
    );
    check!(
        admit(
            "quarantine_validation",
            &c2_shadow_forever,
            &keys,
            now_ms(),
            RampMode::Live
        ) == Err(Refusal::DeHardeningNeedsExpiry),
        "a de-hardening write with no expiry is refused — it cannot become permanent"
    );
    let c2_shadow_bounded = signed(
        "quarantine_validation",
        RampMode::Shadow,
        PostureSource::Operator,
        "incident",
        Some(now_ms() + 3_600_000),
        Some(&operator),
    );
    check!(
        admit(
            "quarantine_validation",
            &c2_shadow_bounded,
            &keys,
            now_ms(),
            RampMode::Live
        ) == Ok(RampMode::Shadow),
        "the same write with a one-hour expiry is admitted"
    );
    check!(
        admit(
            "quarantine_validation",
            &c2_shadow_bounded,
            &keys,
            now_ms() + 3_700_000,
            RampMode::Shadow
        ) == Err(Refusal::Expired),
        "past its expiry the poller refuses it and reverts to the CLI default"
    );

    // ── prefix compatibility with the landed reader ────────────────────────
    println!("\nmigration  does an M3 row still decode in the landed reader?");
    let bytes = postcard::to_allocvec(&good).expect("encode");
    let landed: Result<RampPosture, _> = postcard::from_bytes(&bytes);
    check!(
        landed.as_ref().map(|r| r.mode) == Ok(RampMode::Shadow),
        "HAZARD: postcard IS prefix-tolerant — an M3 row decodes cleanly in the \
         *landed* reader, which silently ignores the signature and applies the mode. \
         A rolling upgrade therefore has un-upgraded processes obeying rows they \
         never authenticated. Measured, not assumed."
    );
    let plain_bytes = postcard::to_allocvec(&plain(
        RampMode::Shadow,
        PostureSource::Operator,
        "demote",
    ))
    .expect("encode");
    println!(
        "        plain row {} B, signed row {} B (+{} B)",
        plain_bytes.len(),
        bytes.len(),
        bytes.len() - plain_bytes.len()
    );

    // ── clause (c): the 2 s bound, measured end to end ─────────────────────
    println!("\nclause (c)  operator write -> running poller applies, measured");
    db.clear(c5.clone()).await;
    let cell = Arc::new(AtomicU8::new(rank(RampMode::Live)));
    let poll_db = Arc::clone(&db.0);
    let poll_cell = Arc::clone(&cell);
    let poll_keys = keys.clone();
    let poller = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let key = ramp_key("strikes");
            let db = Arc::clone(&poll_db);
            let got = db
                .run(move |tx, _| {
                    let key = key.clone();
                    async move { Ok(tx.get(&key, false).await?.map(|v| v.to_vec())) }
                })
                .await
                .ok()
                .flatten();
            let current = match poll_cell.load(Ordering::Relaxed) {
                0 => RampMode::Off,
                1 => RampMode::Shadow,
                _ => RampMode::Live,
            };
            let mode = match got {
                None => RampMode::Off, // startup default for C5
                Some(bytes) => match postcard::from_bytes::<SignedRampPosture>(&bytes) {
                    Ok(row) => match admit("strikes", &row, &poll_keys, now_ms(), current) {
                        Ok(mode) => mode,
                        Err(_) => RampMode::Shadow, // refuse -> demote, never retain
                    },
                    Err(_) => RampMode::Shadow,
                },
            };
            poll_cell.store(rank(mode), Ordering::Relaxed);
        }
    });

    let mut samples = Vec::new();
    for round in 0..5 {
        let want = if round % 2 == 0 {
            RampMode::Shadow
        } else {
            RampMode::Live
        };
        let row = signed(
            "strikes",
            want,
            PostureSource::Operator,
            "measured",
            None,
            Some(&operator),
        );
        let started = Instant::now();
        db.put(c5.clone(), postcard::to_allocvec(&row).expect("encode"))
            .await;
        loop {
            if cell.load(Ordering::Relaxed) == rank(want) {
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        samples.push(started.elapsed());
    }
    poller.abort();
    let worst = samples.iter().max().copied().unwrap_or_default();
    for (i, s) in samples.iter().enumerate() {
        println!("        round {i}: {:?}", s);
    }
    check!(
        worst < Duration::from_secs(2),
        "worst observed decision->effect {worst:?} is inside clause (c)'s 2 s bound"
    );

    db.clear(c5).await;
    let failed = FAILED.load(Ordering::Relaxed);
    println!("\n{}", if failed == 0 { "spike: all checks passed" } else { "spike: FAILURES" });
    std::process::exit(i32::from(failed != 0));
}
