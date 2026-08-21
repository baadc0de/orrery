//! One racer in the double-spend race, as its own OS process (issue #152).
//!
//! A trader is not a peer. It claims nothing, holds no lease, reports no
//! presence and takes no interest grant — an [`Intent`] carries no cell
//! (`orrery_protocol::persist`), so nothing on this path is addressed to a
//! shard and nothing on it needs authorization over one. What a trader has is
//! a session token, which is the only thing the intent path authorizes
//! against: `BaselineIntentValidator` admits a `LEDGER_ITEM_TRANSFER_OP` only
//! when the **buyer** — the debit side — is the account this connection
//! authenticated as.
//!
//! # Why it is a process and not a task
//!
//! The whole claim of arm (b) is that the two racers are in different
//! processes and share only the durable tier. Two futures on the orchestrator's
//! runtime would prove the same thing `crates/orrery_persistd/tests/
//! intent_commit.rs` already proves in one process, against one executor, over
//! one `Database` handle. Here each trader is a `p3-siblings --trader-spec`
//! child holding a session to exactly *one* of the two gateways, so the two
//! transfers reach two `FdbIntentExecutor`s in two address spaces and meet for
//! the first time in FoundationDB's conflict resolver.
//!
//! # How the two submissions are made to overlap
//!
//! A race leg that never races passes vacuously, and that failure mode has
//! been paid for twice in this repository's self-tests. So the overlap is
//! *constructed*, in three steps, and then *measured* rather than assumed:
//!
//! 1. **Everything expensive happens before the clock starts.** Both traders
//!    connect, complete `Hello`, fund themselves, and sign every round's
//!    intent up front. At the firing instant the only work left is one
//!    `send_datagram`.
//! 2. **A barrier, then an absolute deadline.** Each trader touches its ready
//!    file and blocks until the orchestrator publishes a start instant; round
//!    `r` fires at `start + r · period` on the *same* wall clock, since both
//!    processes are on one machine. The last two milliseconds are spun rather
//!    than slept, because a timer wheel's granularity is the one term that
//!    would otherwise dominate the skew.
//! 3. **Both halves are stamped in microseconds.** Every submission logs the
//!    instant it went out and the instant its ack came back, so the
//!    orchestrator can state — per round, from data — whether the two attempts
//!    were ever in flight at the same time. A round whose intervals do not
//!    intersect is a sequence, and the leg fails on it rather than reporting
//!    it as a race that happened to have a winner.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use orrery_persistd::intent::{ItemTransferArgs, LEDGER_CREDIT_OP, LEDGER_ITEM_TRANSFER_OP};
use orrery_protocol::{
    AccountId, AssetId, CellEpoch, GatewayMsg, GatewayReply, Intent, IntentOp, IntentOutcome,
    ItemUid, NodeId,
};

use crate::peer::Side;
use crate::wire::Session;

/// How long a trader waits for the orchestrator to publish the start instant.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the barrier file is polled.
const BARRIER_POLL: Duration = Duration::from_millis(20);
/// How long an ack may take before the attempt is recorded as unanswered.
///
/// An unanswered attempt is a *failure* of the leg, not a slow round: the
/// criterion is that the loser receives a definitive `Rejected`, never a
/// silent success and never a hang (issue #152).
const ACK_TIMEOUT: Duration = Duration::from_secs(20);
/// The last stretch before a firing instant, spun instead of slept.
const SPIN_WINDOW: Duration = Duration::from_millis(2);

/// One round of the race, as the orchestrator planned it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RaceRound {
    /// Round index, shared by both traders.
    pub round: u32,
    /// The contended item. Both traders offer to buy *this* item in this
    /// round, from the same seller, at the same instant.
    pub item: u64,
    /// This trader's intent id for the round. Distinct per trader, because
    /// two submissions sharing one `intent_id` would be a *replay* — the
    /// idempotency row would answer the second one and no conflict would ever
    /// be reached. That is arm (a)'s property, and it is not this one.
    pub intent_id: u128,
}

/// Everything one trader process needs, handed over as a file rather than argv.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TraderSpec {
    /// The gateway this trader submits to — one of the two, never both.
    pub gateway_addr: String,
    /// That gateway's `node_id`.
    pub gateway_node: String,
    /// Which sibling it is, carried so the orchestrator can attribute every
    /// commit to the gateway that made it.
    pub side: Side,
    /// This trader's hex-encoded iroh secret key.
    pub secret: String,
    /// This trader's hex-encoded session token.
    pub token: String,
    /// The account that token names — the buyer of every round.
    pub account: u64,
    /// The account that owns every contended item and is credited the price.
    pub seller: u64,
    /// The asset the price is denominated in.
    pub asset: u64,
    /// The price of one item.
    pub price: i64,
    /// The intent that funds this trader before the race starts.
    pub credit_intent_id: u128,
    /// The rounds, in order.
    pub rounds: Vec<RaceRound>,
    /// Milliseconds between rounds.
    pub round_period_ms: u64,
    /// Touched once this trader is connected, funded and pre-signed.
    pub ready_file: PathBuf,
    /// Polled for the orchestrator's start instant.
    pub start_file: PathBuf,
    /// Where this trader writes its JSONL event log.
    pub log: PathBuf,
}

/// What the orchestrator publishes to release the barrier.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RaceStart {
    /// Wall-clock milliseconds at which round 0 fires.
    pub start_unix_ms: u64,
}

/// A line in a trader's event log.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TraderEvent {
    /// The funding credit's outcome. Its own event because a race whose
    /// racers could not pay is a race every round of which refuses for
    /// `REASON_INSUFFICIENT_BALANCE` — a uniform, plausible-looking failure
    /// that has nothing to do with the invariant under test.
    Funded {
        /// The gateway that answered.
        side: Side,
        /// `true` if the credit committed.
        committed: bool,
        /// The rejection reason, when it did not.
        reason: Option<u16>,
    },
    /// An attempt left this process.
    Submitted {
        /// The round.
        round: u32,
        /// The item offered.
        item: u64,
        /// The intent id, in decimal.
        ///
        /// A **string**, and it has to be one. This enum is internally tagged,
        /// so serde routes each variant's body through its private `Content`
        /// buffer on the way in — and that buffer has no 128-bit integer. A
        /// `u128` field here serializes perfectly and then fails to
        /// deserialize, silently, because the reader below drops lines it
        /// cannot decode. The first run of this leg reported 24 commits, 24
        /// single owners, exactly one receipt each — and **zero attempts**,
        /// for precisely that reason.
        intent_id: String,
        /// The gateway it went to.
        side: Side,
        /// Wall-clock microseconds at the send.
        at_us: u64,
    },
    /// An attempt was answered.
    Acked {
        /// The round.
        round: u32,
        /// The intent id the ack names, in decimal. See
        /// [`TraderEvent::Submitted`] for why it is not a `u128`.
        intent_id: String,
        /// The gateway that answered.
        side: Side,
        /// `true` for `IntentOutcome::Committed`.
        committed: bool,
        /// The `REASON_*` code for a rejection.
        reason: Option<u16>,
        /// Wall-clock microseconds at the ack.
        at_us: u64,
    },
    /// An attempt went unanswered inside [`ACK_TIMEOUT`].
    Unanswered {
        /// The round.
        round: u32,
        /// The intent id nothing answered for, in decimal. See
        /// [`TraderEvent::Submitted`] for why it is not a `u128`.
        intent_id: String,
        /// The gateway that did not answer.
        side: Side,
    },
    /// The trader finished its rounds.
    Done {
        /// The gateway it was submitting to.
        side: Side,
        /// Attempts sent.
        attempts: usize,
    },
}

/// Wall-clock microseconds.
fn unix_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as u64)
}

/// Build and sign one intent carrying one op.
fn signed_intent(
    secret: &iroh::SecretKey,
    intent_id: u128,
    op: u16,
    args: Vec<u8>,
) -> Result<Intent> {
    let mut intent = Intent {
        evidence: None,
        intent_id,
        issuer: secret.public(),
        // Zero because nothing reads it yet: `cell_epoch` binds the seeded
        // witness set, and the K-of-N threshold that would consult it is not
        // in this build. The run says so in its report rather than implying an
        // attested path it did not take.
        cell_epoch: CellEpoch::new(0),
        ops: vec![IntentOp {
            op,
            args: Bytes::from(args),
        }],
        attestations: Vec::new(),
        signature: secret.sign(b"placeholder"),
    };
    intent.sign(secret);
    Ok(intent)
}

/// The 24-byte `account ‖ asset ‖ delta` triple of a [`LEDGER_CREDIT_OP`].
///
/// Spelled here because the executor decodes it inline and exports no encoder
/// for it — unlike the transfer, whose [`ItemTransferArgs::encode`] is the
/// single definition of its 40-byte layout and is used verbatim below.
fn credit_args(account: u64, asset: u64, delta: i64) -> Vec<u8> {
    let mut args = Vec::with_capacity(24);
    args.extend_from_slice(&account.to_le_bytes());
    args.extend_from_slice(&asset.to_le_bytes());
    args.extend_from_slice(&delta.to_le_bytes());
    args
}

/// Wait for `deadline_us`, sleeping most of the way and spinning the last
/// [`SPIN_WINDOW`].
///
/// The spin is the difference between a race and a coincidence. A bare
/// `sleep_until` wakes on the runtime's timer granularity, which under a
/// loaded box is milliseconds — the same order as the whole transaction this
/// leg is trying to overlap — so the two traders would fire "simultaneously"
/// with a skew larger than the window in which a conflict is possible.
async fn wait_until_us(deadline_us: u64) {
    let now = unix_us();
    if deadline_us > now {
        let remaining = Duration::from_micros(deadline_us - now);
        if let Some(coarse) = remaining.checked_sub(SPIN_WINDOW) {
            tokio::time::sleep(coarse).await;
        }
    }
    while unix_us() < deadline_us {
        std::hint::spin_loop();
    }
}

/// Read the start instant the orchestrator published, waiting for it to appear.
async fn await_start(path: &std::path::Path) -> Result<u64> {
    let deadline = tokio::time::Instant::now() + BARRIER_TIMEOUT;
    loop {
        if let Ok(raw) = std::fs::read(path) {
            if let Ok(start) = serde_json::from_slice::<RaceStart>(&raw) {
                return Ok(start.start_unix_ms);
            }
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the race never started: {} was never published",
            path.display()
        );
        tokio::time::sleep(BARRIER_POLL).await;
    }
}

/// Wait for the ack naming `intent_id`, ignoring anything else on the wire.
async fn await_ack(session: &Session, intent_id: u128) -> Option<IntentOutcome> {
    let deadline = tokio::time::Instant::now() + ACK_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match session.recv(remaining).await {
            Some(GatewayReply::IntentAck {
                intent_id: answered,
                outcome,
            }) if answered == intent_id => return Some(outcome),
            // A gateway serving other peers answers plenty this trader did not
            // ask for; only the ack it is waiting on ends the wait.
            Some(_) => {}
            None => return None,
        }
    }
}

/// Split an outcome into the two fields the log carries.
fn outcome_fields(outcome: &IntentOutcome) -> (bool, Option<u16>) {
    match outcome {
        IntentOutcome::Committed { .. } => (true, None),
        IntentOutcome::Rejected { reason } => (false, Some(*reason)),
        // D29's low-population path never admits a transfer (clause 3 refuses
        // every op naming a second account), and this harness sends nothing
        // else — so this is unreachable rather than merely unlikely. Logged as
        // "not committed" with no reason code, which is the honest shape for
        // an outcome the sibling gate's double-spend arithmetic has no place
        // for: a provisional commit is not a commit it may count.
        IntentOutcome::Provisional { .. } => (false, None),
    }
}

/// Run one trader to the end of its rounds.
pub async fn run(spec: TraderSpec) -> Result<()> {
    let mut log = std::fs::File::create(&spec.log)
        .with_context(|| format!("create trader log {}", spec.log.display()))?;
    let mut emit = move |event: &TraderEvent| {
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(log, "{line}");
            let _ = log.flush();
        }
    };

    let secret = iroh::SecretKey::from_bytes(&crate::decode_key(&spec.secret)?);
    let token = crate::decode_hex(&spec.token)?;
    let node: NodeId = secret.public();

    let session = Session::connect(
        secret.clone(),
        crate::endpoint_addr(&spec.gateway_node, &spec.gateway_addr)?,
    )
    .await?;
    session.send_control(&GatewayMsg::VersionedHello {
        token: token.clone(),
        node,
        version: orrery_protocol::PROTOCOL_VERSION,
    })?;
    let hello = session.recv(Duration::from_secs(10)).await;
    anyhow::ensure!(
        matches!(hello, Some(GatewayReply::HelloAck { .. })),
        "gateway {:?} did not accept the trader's hello: {hello:?}",
        spec.side
    );

    // ── Funding ─────────────────────────────────────────────────────────
    // Through the gateway, as an ordinary intent, rather than by writing the
    // balance row from the orchestrator: the credit op is the one this cluster
    // interprets for exactly this purpose, and a buyer funded out of band
    // would leave the debit side of every trade untested.
    let funding = i64::from(u32::try_from(spec.rounds.len()).unwrap_or(u32::MAX))
        .saturating_mul(spec.price)
        .max(spec.price);
    let credit = signed_intent(
        &secret,
        spec.credit_intent_id,
        LEDGER_CREDIT_OP,
        credit_args(spec.account, spec.asset, funding),
    )?;
    session.send_control(&GatewayMsg::SubmitIntent { intent: credit })?;
    let funded = await_ack(&session, spec.credit_intent_id).await;
    let (committed, reason) = funded.as_ref().map_or((false, None), outcome_fields);
    emit(&TraderEvent::Funded {
        side: spec.side,
        committed,
        reason,
    });
    anyhow::ensure!(
        committed,
        "trader on gateway {:?} could not fund itself: {funded:?}",
        spec.side
    );

    // ── Pre-signing ─────────────────────────────────────────────────────
    // Every round's intent, before the barrier. Signing is ~50 µs of ed25519
    // and the whole point of this leg is that the two sends are not separated
    // by anything at all.
    let mut prepared = Vec::with_capacity(spec.rounds.len());
    for round in &spec.rounds {
        let args = ItemTransferArgs {
            item: ItemUid::new(round.item),
            seller: AccountId::new(spec.seller),
            buyer: AccountId::new(spec.account),
            asset: AssetId::new(spec.asset),
            price: spec.price,
        }
        .encode();
        prepared.push((
            round.clone(),
            signed_intent(
                &secret,
                round.intent_id,
                LEDGER_ITEM_TRANSFER_OP,
                args.to_vec(),
            )?,
        ));
    }

    std::fs::write(&spec.ready_file, b"ready")
        .with_context(|| format!("publish readiness at {}", spec.ready_file.display()))?;
    let start_ms = await_start(&spec.start_file).await?;

    // ── The race ────────────────────────────────────────────────────────
    let mut attempts = 0usize;
    for (round, intent) in prepared {
        let fire_us =
            start_ms.saturating_mul(1_000) + u64::from(round.round) * spec.round_period_ms * 1_000;
        wait_until_us(fire_us).await;
        session.send_control(&GatewayMsg::SubmitIntent {
            intent: intent.clone(),
        })?;
        attempts += 1;
        emit(&TraderEvent::Submitted {
            round: round.round,
            item: round.item,
            intent_id: round.intent_id.to_string(),
            side: spec.side,
            at_us: unix_us(),
        });
        match await_ack(&session, round.intent_id).await {
            Some(outcome) => {
                let (committed, reason) = outcome_fields(&outcome);
                emit(&TraderEvent::Acked {
                    round: round.round,
                    intent_id: round.intent_id.to_string(),
                    side: spec.side,
                    committed,
                    reason,
                    at_us: unix_us(),
                });
            }
            None => emit(&TraderEvent::Unanswered {
                round: round.round,
                intent_id: round.intent_id.to_string(),
                side: spec.side,
            }),
        }
    }
    emit(&TraderEvent::Done {
        side: spec.side,
        attempts,
    });
    Ok(())
}
