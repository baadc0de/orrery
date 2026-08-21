//! Arm (b) of the P5 dupe gauntlet: the double-spend race across two sibling
//! gateways (issue #152, `docs/11-roadmap.md` §P5).
//!
//! # The claim, stated narrowly
//!
//! One item, offered twice at the same instant, through **two gateway
//! processes**, leaves exactly one owner. `crates/orrery_persistd/tests/
//! intent_commit.rs` already proves that inside one process against one
//! executor; what it cannot reach is the configuration D26's whole ownership
//! model rests on — two `persistd` nodes that share nothing but the durable
//! tier. This leg is that configuration, and its verdict is read back out of
//! FoundationDB rather than inferred from the two acks.
//!
//! # There is no routing to get wrong here, and that is worth stating
//!
//! On the authority path a request for a cell a gateway does not own is
//! answered `WrongOwner` (#117), and the sibling gate exercises that
//! instrument in this same run with a deliberately misaddressed claim. **The
//! intent path has no such identity, by construction.** An `IntentOp` carries
//! no cell (`orrery_protocol::persist`), so an intent cannot be attributed to
//! a shard at all — which is exactly why `IntentFence` fences an executor on
//! its *whole* activated shard set rather than per shard
//! (`crates/orrery_persistd/src/intent/fdb.rs`). And `ledger/item/{uid}` is
//! not in any grid's `CellId` space: it is a flat ledger key
//! (`crates/orrery_persistd/src/keyspace.rs`), owned by no shard and therefore
//! by no gateway.
//!
//! So both siblings are *correct* addressees for the same item, and neither
//! racer is misrouted or redirected. Nothing arbitrates between them above
//! FoundationDB — which is the point: D11's anti-duplication mechanism is the
//! serializable read of one row, not an ownership check that happens to be
//! above it. Two disjoint `--shard` subtrees still matter to this leg, because
//! both executors are *fenced* on their own set and both fences must hold for
//! both transactions to be admitted at all; but the arbitration itself is
//! FDB's conflict resolver and nothing else.
//!
//! # What makes the run honest rather than lucky
//!
//! Four independent things, because any one of them alone has a plausible way
//! of passing on a broken cluster:
//!
//! - **The overlap is measured, per round, in microseconds.** Two attempts
//!   that were never in flight together are a *sequence*, and a sequence
//!   produces the same two acks a race does — one commit and one
//!   `REASON_NOT_ITEM_OWNER`. The leg fails a round whose in-flight intervals
//!   do not intersect. See [`crate::trader`] for how the overlap is
//!   constructed.
//! - **Conflicts are counted from the cluster itself.** FoundationDB reports
//!   `cluster.workload.transactions.conflicted` in `\xff\xff/status/json`;
//!   the delta across the leg is the number of transactions the resolver
//!   actually aborted, and zero fails the leg however clean the acks looked.
//!   Read it as *corroboration*, not as the decisive clause: the counter is
//!   **cluster-wide**, so it also sees any conflict the two `persistd`
//!   processes generate between themselves, and a lone stray one would
//!   satisfy a bare "> 0". Measured, on a passing run: 24 conflicts across 24
//!   rounds — one per round, which is what a race whose loser always retries
//!   looks like. Measured on a deliberately de-synchronized one (the mutation
//!   check for the overlap clause, gateway B's rounds fired 200 ms late): 1.
//!   So the overlap clause is the one that catches a degenerate leg, and this
//!   one is the cluster agreeing with it. (The per-intent retry counter that
//!   would be decisive — `IntentStageSnapshot::attempts`, whose
//!   `attempts - executed` is exactly the retry count — is process-global and
//!   `persistd` exports it to no artifact, so it is not readable from here.)
//! - **The verdict is durable state, not acks.** For each round the leg reads
//!   the item's ownership row, both attempts' `intent/{intent_id}` rows and
//!   the `ledger/receipt/` range. A rejected intent writes *nothing* — not
//!   even an idempotency row — so "exactly one attempt has a committed row"
//!   is a statement about the database.
//! - **Value is conserved.** Every buyer is funded through the gateway with a
//!   real credit op, and afterwards the seller's balance must equal the number
//!   of commits times the price and each buyer's must equal its funding less
//!   what it won. A ledger that moved an item without moving the money, or
//!   twice, fails here even if the ownership row happens to look right.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use foundationdb::options::{StreamingMode, TransactionOption};
use foundationdb::{Database, FdbBindingError, KeySelector, RangeOption};
use futures::TryStreamExt as _;
use orrery_persistd::keyspace;
use orrery_protocol::{AccountId, AssetId, IntentOutcome, ItemUid, NodeId};

use crate::peer::Side;
use crate::trader::{RaceRound, RaceStart, TraderEvent, TraderSpec};

/// The account that owns every contended item and is credited each price.
///
/// A third party with no session of its own, because a transfer naming one
/// account as both parties is refused before any durable read
/// (`REASON_ITEM_TRANSFER_TO_SELF`) and the two racers must both be *buyers*:
/// `BaselineIntentValidator` admits a transfer only when the debit side is the
/// submitting connection's own account.
const SELLER_ACCOUNT: u64 = 9_001;
/// Gateway A's racer's account.
const TRADER_A_ACCOUNT: u64 = 9_101;
/// Gateway B's racer's account.
const TRADER_B_ACCOUNT: u64 = 9_102;
/// The asset every price is denominated in.
const RACE_ASSET: u64 = 9_000;
/// The price of one contended item.
const RACE_PRICE: i64 = 25;
/// High bits of every contended [`ItemUid`], so the leg's rows are legible in
/// a key dump and cannot collide with a seeded world's.
const ITEM_BASE: u64 = 0x0152_0000_0000_0000;

/// How long the orchestrator waits for both traders to reach the barrier.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long it waits for both traders to finish their rounds afterwards.
const FINISH_TIMEOUT: Duration = Duration::from_secs(120);
/// Polling interval for both waits.
const POLL: Duration = Duration::from_millis(50);
/// How long the leg waits before re-reading the cluster's conflict counter.
///
/// `status json` is produced by the cluster controller's periodic status
/// gather, so the workload counters lag the transactions they count. Reading
/// the "after" value immediately would routinely observe the "before" one and
/// report a race that conflicted as a race that did not.
const STATUS_SETTLE: Duration = Duration::from_secs(8);

/// The `\xff\xff/status/json` special key: the cluster's own account of its
/// workload, including how many transactions its resolver aborted.
const STATUS_JSON_KEY: &[u8] = b"\xff\xff/status/json";

/// Everything the race leg measured, and whether it held.
pub struct RaceClause {
    /// Whether every clause held.
    pub passed: bool,
    /// The report fragment, published under `race` in `report.json`.
    pub report: serde_json::Value,
}

/// One round's two attempts, as the traders' logs recorded them.
#[derive(Debug, Default, Clone)]
struct AttemptPair {
    /// `(sent_us, acked_us, committed, reason)` per side.
    sides: BTreeMap<Side, Attempt>,
}

/// One trader's attempt in one round.
#[derive(Debug, Clone, Copy)]
struct Attempt {
    sent_us: u64,
    acked_us: Option<u64>,
    committed: bool,
    reason: Option<u16>,
}

/// The durable answer for one round, read back from FoundationDB.
#[derive(Debug, Clone)]
struct DurableRound {
    /// The account `ledger/item/{uid}` names after the race.
    owner: Option<u64>,
    /// The attempts that have a committed `intent/{intent_id}` row.
    committed_ids: Vec<u128>,
    /// The attempts that have an `intent/{intent_id}` row at all.
    recorded_ids: Vec<u128>,
    /// `ledger/receipt/` rows naming either of this round's intent ids.
    receipts: usize,
}

/// Plan, run and judge the double-spend race.
///
/// `rounds` repetitions, because one round is a coin flip and this leg's
/// interesting failure mode — a race that quietly degenerated into a sequence
/// — is only visible in the distribution of the overlaps.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn run_race(
    issuer: &iroh::SecretKey,
    cluster_file: &str,
    out: &Path,
    rounds: u32,
    round_period_ms: u64,
    gateway_a: (&str, &str),
    gateway_b: (&str, &str),
    exe: &Path,
) -> Result<RaceClause> {
    anyhow::ensure!(rounds > 0, "a race leg with no rounds proves nothing");
    let context = orrery_persistd::FdbContext::connect(cluster_file)
        .map_err(|error| anyhow::anyhow!("open the durable tier at {cluster_file}: {error}"))?;
    let db = context.database();

    // ── The contended items ─────────────────────────────────────────────
    // Written directly, as fixture: this is the seeder's job for the world
    // and there is no seeder for the ledger. What must *not* be written here
    // is any effect the race itself is supposed to produce — the balances the
    // buyers spend come from real credit intents through the gateways.
    let items: Vec<u64> = (0..rounds).map(|r| ITEM_BASE | u64::from(r)).collect();
    seed_items(&db, &items, SELLER_ACCOUNT).await?;

    let conflicts_before = cluster_conflicts(&db).await;

    // ── The two traders ─────────────────────────────────────────────────
    let start_file = out.join("race-start.json");
    let mut children = Vec::new();
    let mut ready_files = Vec::new();
    let mut logs = Vec::new();
    for (index, (side, account, gateway)) in [
        (Side::A, TRADER_A_ACCOUNT, gateway_a),
        (Side::B, TRADER_B_ACCOUNT, gateway_b),
    ]
    .into_iter()
    .enumerate()
    {
        let secret = trader_secret(index as u8);
        let node: NodeId = secret.public();
        let log = out.join(format!("trader-{side:?}.jsonl").to_lowercase());
        let ready = out.join(format!("trader-{index}.ready"));
        let spec = TraderSpec {
            gateway_addr: gateway.0.to_owned(),
            gateway_node: gateway.1.to_owned(),
            side,
            secret: crate::encode_hex(&secret.to_bytes()),
            token: crate::encode_hex(&crate::mint_token_for(issuer, node, account)?),
            account,
            seller: SELLER_ACCOUNT,
            asset: RACE_ASSET,
            price: RACE_PRICE,
            credit_intent_id: credit_intent_id(side),
            rounds: items
                .iter()
                .enumerate()
                .map(|(round, item)| RaceRound {
                    round: round as u32,
                    item: *item,
                    intent_id: attempt_intent_id(side, round as u32),
                })
                .collect(),
            round_period_ms,
            ready_file: ready.clone(),
            start_file: start_file.clone(),
            log: log.clone(),
        };
        let spec_path = out.join(format!("trader-{index}.json"));
        std::fs::write(&spec_path, serde_json::to_vec(&spec)?)?;
        let child = tokio::process::Command::new(exe)
            .arg("--trader-spec")
            .arg(&spec_path)
            // The orchestrator's own required flags are irrelevant in trader
            // mode but still parsed, so echo them through.
            .args(["--gateway-a-addr", gateway_a.0])
            .args(["--gateway-a-node", gateway_a.1])
            .args(["--gateway-b-addr", gateway_b.0])
            .args(["--gateway-b-node", gateway_b.1])
            .args(["--coordinator-addr", "127.0.0.1:1"])
            .args(["--coordinator-node", &"0".repeat(64)])
            .args(["--issuer-secret", &crate::encode_hex(&issuer.to_bytes())])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(std::fs::File::create(
                out.join(format!("trader-{index}.log")),
            )?))
            .kill_on_drop(false)
            .spawn()
            .context("spawn racer")?;
        children.push(child);
        ready_files.push(ready);
        logs.push((side, log));
    }

    // ── The barrier ─────────────────────────────────────────────────────
    // Published only once *both* racers are connected, funded and pre-signed.
    // A start instant computed before they were spawned would be a start
    // instant one of them could miss entirely, and a missed round is a round
    // whose loser refuses for a reason that has nothing to do with the race.
    let ready_deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if ready_files.iter().all(|path| path.exists()) {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < ready_deadline,
            "one of the two racers never reached the barrier; see {}",
            out.display()
        );
        tokio::time::sleep(POLL).await;
    }
    let start_unix_ms = crate::unix_ms() + 1_000;
    std::fs::write(
        &start_file,
        serde_json::to_vec(&RaceStart { start_unix_ms })?,
    )?;
    tracing::info!(rounds, start_unix_ms, "double-spend race armed");

    let finish_deadline = tokio::time::Instant::now() + FINISH_TIMEOUT;
    let mut finished = false;
    while tokio::time::Instant::now() < finish_deadline {
        if logs
            .iter()
            .all(|(_, log)| read_trader_events(log).iter().any(is_done))
        {
            finished = true;
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    for mut child in children {
        let _ = child.kill().await;
    }

    // ── What the wire said ──────────────────────────────────────────────
    let mut attempts: BTreeMap<u32, AttemptPair> = BTreeMap::new();
    let mut funded: BTreeMap<Side, bool> = BTreeMap::new();
    let mut unanswered = 0usize;
    let mut sent_per_side: BTreeMap<Side, usize> = BTreeMap::new();
    for (side, log) in &logs {
        let mut pending: BTreeMap<String, (u32, u64)> = BTreeMap::new();
        for event in read_trader_events(log) {
            match event {
                TraderEvent::Funded { committed, .. } => {
                    funded.insert(*side, committed);
                }
                TraderEvent::Submitted {
                    round,
                    intent_id,
                    at_us,
                    ..
                } => {
                    *sent_per_side.entry(*side).or_default() += 1;
                    pending.insert(intent_id, (round, at_us));
                    attempts.entry(round).or_default().sides.insert(
                        *side,
                        Attempt {
                            sent_us: at_us,
                            acked_us: None,
                            committed: false,
                            reason: None,
                        },
                    );
                }
                TraderEvent::Acked {
                    round,
                    intent_id,
                    committed,
                    reason,
                    at_us,
                    ..
                } => {
                    pending.remove(&intent_id);
                    if let Some(attempt) = attempts
                        .get_mut(&round)
                        .and_then(|pair| pair.sides.get_mut(side))
                    {
                        attempt.acked_us = Some(at_us);
                        attempt.committed = committed;
                        attempt.reason = reason;
                    }
                }
                TraderEvent::Unanswered { .. } => unanswered += 1,
                TraderEvent::Done { .. } => {}
            }
        }
        unanswered += pending.len();
    }

    // ── What the database says ──────────────────────────────────────────
    tokio::time::sleep(STATUS_SETTLE).await;
    let conflicts_after = cluster_conflicts(&db).await;
    let conflicts_observed = match (conflicts_before, conflicts_after) {
        (Some(before), Some(after)) => Some(after.saturating_sub(before)),
        _ => None,
    };
    let receipts = read_receipt_intent_ids(&db).await?;
    let mut durable: BTreeMap<u32, DurableRound> = BTreeMap::new();
    for (round, item) in items.iter().enumerate() {
        let round = round as u32;
        let owner = read_item_owner(&db, *item).await?;
        let mut committed_ids = Vec::new();
        let mut recorded_ids = Vec::new();
        let mut round_receipts = 0usize;
        for side in [Side::A, Side::B] {
            let id = attempt_intent_id(side, round);
            if let Some(outcome) = read_intent_outcome(&db, id).await? {
                recorded_ids.push(id);
                if matches!(outcome, IntentOutcome::Committed { .. }) {
                    committed_ids.push(id);
                }
            }
            round_receipts += receipts.iter().filter(|banked| **banked == id).count();
        }
        durable.insert(
            round,
            DurableRound {
                owner,
                committed_ids,
                recorded_ids,
                receipts: round_receipts,
            },
        );
    }
    let balances = read_balances(
        &db,
        &[SELLER_ACCOUNT, TRADER_A_ACCOUNT, TRADER_B_ACCOUNT],
        RACE_ASSET,
    )
    .await?;

    // ── The verdict, round by round ─────────────────────────────────────
    let mut rows = Vec::new();
    let mut rounds_overlapped = 0u32;
    let mut rounds_one_owner = 0u32;
    let mut rounds_one_commit = 0u32;
    let mut rounds_one_receipt = 0u32;
    let mut rounds_loser_refused = 0u32;
    let mut commits_a = 0u32;
    let mut commits_b = 0u32;
    // `None` until a round has been seen with an attempt on each side. Not
    // zero: a leg that never saw two attempts would otherwise report a perfect
    // skew, which is the most flattering possible reading of the worst
    // possible run.
    let mut max_dispatch_skew_us: Option<u64> = None;
    let mut refusal_reasons: BTreeMap<u16, u32> = BTreeMap::new();
    for round in 0..rounds {
        let pair = attempts.get(&round).cloned().unwrap_or_default();
        let empty = DurableRound {
            owner: None,
            committed_ids: Vec::new(),
            recorded_ids: Vec::new(),
            receipts: 0,
        };
        let state = durable.get(&round).unwrap_or(&empty);
        let a = pair.sides.get(&Side::A).copied();
        let b = pair.sides.get(&Side::B).copied();

        let overlapped = overlapped(a, b);
        if overlapped {
            rounds_overlapped += 1;
        }
        let dispatch_skew_us = match (a, b) {
            (Some(a), Some(b)) => a.sent_us.abs_diff(b.sent_us),
            _ => u64::MAX,
        };
        if dispatch_skew_us != u64::MAX {
            max_dispatch_skew_us = Some(
                max_dispatch_skew_us.map_or(dispatch_skew_us, |worst| worst.max(dispatch_skew_us)),
            );
        }

        // Durable: exactly one committed row, and the item names its buyer.
        let one_commit = state.committed_ids.len() == 1 && state.recorded_ids.len() == 1;
        if one_commit {
            rounds_one_commit += 1;
        }
        let winner_side = state.committed_ids.first().and_then(|id| {
            [Side::A, Side::B]
                .into_iter()
                .find(|side| attempt_intent_id(*side, round) == *id)
        });
        match winner_side {
            Some(Side::A) => commits_a += 1,
            Some(Side::B) => commits_b += 1,
            None => {}
        }
        let expected_owner = winner_side.map(|side| match side {
            Side::A => TRADER_A_ACCOUNT,
            Side::B => TRADER_B_ACCOUNT,
        });
        let one_owner = state.owner.is_some() && state.owner == expected_owner;
        if one_owner {
            rounds_one_owner += 1;
        }
        if state.receipts == 1 {
            rounds_one_receipt += 1;
        }

        // The loser: a definitive refusal, never a silent success, never a
        // hang. `REASON_NOT_ITEM_OWNER` is the honest re-check after the
        // conflict; `REASON_CONTENTION_EXHAUSTED` is the bounded-retry
        // refusal. Anything else — an insufficient balance, a malformed op —
        // would mean the round refused for a reason that is not this arm's.
        let loser = match winner_side {
            Some(Side::A) => b,
            Some(Side::B) => a,
            None => None,
        };
        let loser_refused = loser.is_some_and(|attempt| {
            !attempt.committed
                && matches!(
                    attempt.reason,
                    Some(orrery_protocol::REASON_NOT_ITEM_OWNER)
                        | Some(orrery_protocol::REASON_CONTENTION_EXHAUSTED)
                )
        });
        if loser_refused {
            rounds_loser_refused += 1;
        }
        if let Some(reason) = loser.and_then(|attempt| attempt.reason) {
            *refusal_reasons.entry(reason).or_default() += 1;
        }

        rows.push(serde_json::json!({
            "round": round,
            "item": format!("{:#x}", items[round as usize]),
            "winner": winner_side.map(|side| format!("{side:?}")),
            "owner_after": state.owner,
            "expected_owner": expected_owner,
            "intent_rows": state.recorded_ids.len(),
            "committed_rows": state.committed_ids.len(),
            "receipts": state.receipts,
            "loser_reason": loser.and_then(|attempt| attempt.reason),
            "dispatch_skew_us": (dispatch_skew_us != u64::MAX).then_some(dispatch_skew_us),
            "overlapped": overlapped,
        }));
    }

    // ── Conservation ────────────────────────────────────────────────────
    // The seller was paid once per commit and the buyers paid for exactly what
    // they won. Stated over the whole leg rather than per round because the
    // balances are one running total each.
    let commits = commits_a + commits_b;
    let funding = i128::from(rounds) * i128::from(RACE_PRICE);
    let seller_expected = i128::from(commits) * i128::from(RACE_PRICE);
    let a_expected = funding - i128::from(commits_a) * i128::from(RACE_PRICE);
    let b_expected = funding - i128::from(commits_b) * i128::from(RACE_PRICE);
    let value_conserved = balances.get(&SELLER_ACCOUNT).copied() == Some(seller_expected)
        && balances.get(&TRADER_A_ACCOUNT).copied() == Some(a_expected)
        && balances.get(&TRADER_B_ACCOUNT).copied() == Some(b_expected);

    let attempts_a = sent_per_side.get(&Side::A).copied().unwrap_or(0);
    let attempts_b = sent_per_side.get(&Side::B).copied().unwrap_or(0);
    let both_gateways_served = attempts_a == rounds as usize && attempts_b == rounds as usize;
    let both_funded = funded.get(&Side::A) == Some(&true) && funded.get(&Side::B) == Some(&true);

    let passed = finished
        && both_funded
        && both_gateways_served
        && unanswered == 0
        && rounds_overlapped == rounds
        && rounds_one_commit == rounds
        && rounds_one_owner == rounds
        && rounds_one_receipt == rounds
        && rounds_loser_refused == rounds
        && conflicts_observed.is_some_and(|seen| seen > 0)
        && value_conserved;

    let report = serde_json::json!({
        "rounds": rounds,
        "round_period_ms": round_period_ms,
        "finished": finished,
        "both_funded": both_funded,
        // Both siblings served this leg, which is what makes it a two-gateway
        // race rather than a one-gateway race with a spare process running.
        "attempts_gateway_a": attempts_a,
        "attempts_gateway_b": attempts_b,
        "both_gateways_served": both_gateways_served,
        "unanswered_attempts": unanswered,
        // The honesty clause. A leg whose attempts never overlapped is a
        // sequence, and a sequence produces exactly the acks a race does.
        "rounds_overlapped": rounds_overlapped,
        "max_dispatch_skew_us": max_dispatch_skew_us,
        // FoundationDB's own count of transactions its resolver aborted,
        // across this leg. `null` means the cluster would not answer, which
        // fails the leg rather than passing it silently.
        "conflicts_observed": conflicts_observed,
        "conflicts_counter_before": conflicts_before,
        "conflicts_counter_after": conflicts_after,
        // Every number below is read back out of FoundationDB after both
        // attempts settled, never inferred from an ack.
        "commits": commits,
        "commits_gateway_a": commits_a,
        "commits_gateway_b": commits_b,
        "rounds_with_one_commit": rounds_one_commit,
        "rounds_with_one_owner": rounds_one_owner,
        "rounds_with_one_receipt": rounds_one_receipt,
        "rounds_loser_definitively_refused": rounds_loser_refused,
        "loser_refusal_reasons": refusal_reasons
            .iter()
            .map(|(reason, count)| (reason.to_string(), *count))
            .collect::<BTreeMap<_, _>>(),
        "price": RACE_PRICE,
        "seller_balance": balances.get(&SELLER_ACCOUNT),
        "trader_a_balance": balances.get(&TRADER_A_ACCOUNT),
        "trader_b_balance": balances.get(&TRADER_B_ACCOUNT),
        "value_conserved": value_conserved,
        // Which path the run took, because "the loser was refused" would mean
        // something entirely different if an attestation check had refused it.
        // K-of-N enforcement is not in this build; the intents carry no
        // attestations and the refusals are serializability's alone.
        "attestation_mode": "unattested (K-of-N enforcement not present in this build)",
        "rounds_detail": rows,
        "passed": passed,
    });
    Ok(RaceClause { passed, report })
}

/// Were the two attempts ever in flight at the same time?
///
/// This is the clause that keeps the leg from passing vacuously. Two attempts
/// that never overlapped are a *sequence*, and a sequence produces exactly the
/// acks a race does: one commit and one `REASON_NOT_ITEM_OWNER`. Intervals are
/// `[sent, acked]` per side and the test is ordinary interval intersection.
///
/// An attempt with no ack is deliberately **not** an overlap, even though its
/// interval is open-ended and would trivially intersect the other's. An
/// unanswered attempt is a failure of its own — the loser must receive a
/// definitive refusal, never a hang — and counting it here as well would let
/// the timeout that failed one clause satisfy another.
fn overlapped(a: Option<Attempt>, b: Option<Attempt>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => match (a.acked_us, b.acked_us) {
            (Some(a_ack), Some(b_ack)) => a.sent_us <= b_ack && b.sent_us <= a_ack,
            _ => false,
        },
        _ => false,
    }
}

/// Whether a trader log line is its final one.
fn is_done(event: &TraderEvent) -> bool {
    matches!(event, TraderEvent::Done { .. })
}

/// Deterministic per-trader identity, distinct from every peer's.
fn trader_secret(index: u8) -> iroh::SecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = index;
    bytes[1] = 0x52;
    bytes[2] = 0x01;
    iroh::SecretKey::from_bytes(&bytes)
}

/// The intent id one side uses to fund itself.
fn credit_intent_id(side: Side) -> u128 {
    (0x152u128 << 96) | (side_bit(side) << 64) | 0xC2ED17
}

/// The intent id one side submits in one round.
///
/// Distinct per side on purpose: two racers sharing an `intent_id` would be a
/// replay, answered by the idempotency row before any ledger read, and no
/// conflict would ever arise. That is arm (a)'s property and not this one's.
fn attempt_intent_id(side: Side, round: u32) -> u128 {
    (0x152u128 << 96) | (side_bit(side) << 64) | u128::from(round)
}

fn side_bit(side: Side) -> u128 {
    match side {
        Side::A => 0,
        Side::B => 1,
    }
}

/// Write one `ledger/item/{uid}` row per round, owned by the seller.
async fn seed_items(db: &Database, items: &[u64], owner: u64) -> Result<()> {
    let rows: Vec<(Vec<u8>, Vec<u8>)> = items
        .iter()
        .map(|item| {
            let key = keyspace::ledger_item_key(ItemUid::new(*item)).to_vec();
            let value = postcard::to_stdvec(&keyspace::ItemRow {
                owner: AccountId::new(owner),
                state: b"p3-siblings-race".to_vec(),
            })?;
            Ok::<_, anyhow::Error>((key, value))
        })
        .collect::<Result<_>>()?;
    db.run(|trx, _| {
        let rows = rows.clone();
        async move {
            for (key, value) in rows {
                trx.set(&key, &value);
            }
            Ok(())
        }
    })
    .await
    .map_err(|error: FdbBindingError| anyhow::anyhow!("seed contended items: {error}"))
}

/// The account `ledger/item/{item}` names, if the row is there.
async fn read_item_owner(db: &Database, item: u64) -> Result<Option<u64>> {
    let key = keyspace::ledger_item_key(ItemUid::new(item));
    let raw = db
        .run(|trx, _| async move { Ok(trx.get(&key, false).await?) })
        .await
        .map_err(|error: FdbBindingError| anyhow::anyhow!("read item {item:#x}: {error}"))?;
    let Some(raw) = raw else { return Ok(None) };
    let row: keyspace::ItemRow =
        postcard::from_bytes(&raw).with_context(|| format!("decode item row {item:#x}"))?;
    Ok(Some(row.owner.0))
}

/// The recorded outcome at `intent/{intent_id}`, if the intent wrote one.
///
/// A durable refusal writes no row at all, which is why an absent row here is
/// evidence rather than a gap: it is how a rejected intent looks.
async fn read_intent_outcome(db: &Database, intent_id: u128) -> Result<Option<IntentOutcome>> {
    let key = keyspace::intent_key(intent_id);
    let raw = db
        .run(|trx, _| async move { Ok(trx.get(&key, false).await?) })
        .await
        .map_err(|error: FdbBindingError| anyhow::anyhow!("read intent {intent_id}: {error}"))?;
    let Some(raw) = raw else { return Ok(None) };
    let row: keyspace::IntentRow =
        postcard::from_bytes(&raw).with_context(|| format!("decode intent row {intent_id}"))?;
    Ok(Some(row.outcome))
}

/// Every `ledger/receipt/` row's `intent_id`, in commit order.
///
/// The whole family is scanned rather than a per-intent lookup, because the
/// key is a commit versionstamp: there is no way to address a receipt by the
/// intent that banked it, and "how many receipts name this intent" is the
/// question the audit trail has to answer.
async fn read_receipt_intent_ids(db: &Database) -> Result<Vec<u128>> {
    let begin = keyspace::ledger_receipt_key().to_vec();
    let mut end = begin.clone();
    end[1] = b's';
    let values = db
        .run(|trx, _| {
            let begin = begin.clone();
            let end = end.clone();
            async move {
                let range = RangeOption {
                    begin: KeySelector::first_greater_or_equal(begin.as_slice()),
                    end: KeySelector::first_greater_or_equal(end.as_slice()),
                    mode: StreamingMode::WantAll,
                    ..RangeOption::default()
                };
                let mut stream = trx.get_ranges_keyvalues(range, false);
                let mut values = Vec::new();
                while let Some(kv) = stream.try_next().await? {
                    values.push(kv.value().to_vec());
                }
                Ok(values)
            }
        })
        .await
        .map_err(|error: FdbBindingError| anyhow::anyhow!("scan ledger/receipt/: {error}"))?;
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        let row: keyspace::ReceiptRow =
            postcard::from_bytes(&value).context("decode receipt row")?;
        ids.push(row.intent_id);
    }
    Ok(ids)
}

/// `ledger/bal/{account}/{asset}` for each account, as the little-endian
/// integer `MutationType::Add` maintains it. An absent row is zero.
async fn read_balances(db: &Database, accounts: &[u64], asset: u64) -> Result<BTreeMap<u64, i128>> {
    let mut out = BTreeMap::new();
    for account in accounts {
        let key = keyspace::ledger_bal_key(AccountId::new(*account), AssetId::new(asset));
        let raw = db
            .run(|trx, _| async move { Ok(trx.get(&key, false).await?) })
            .await
            .map_err(|error: FdbBindingError| {
                anyhow::anyhow!("read balance {account}/{asset}: {error}")
            })?;
        let value = raw.map_or(0i128, |raw| {
            let mut buf = [0u8; 16];
            let n = raw.len().min(16);
            buf[..n].copy_from_slice(&raw[..n]);
            i128::from_le_bytes(buf)
        });
        out.insert(*account, value);
    }
    Ok(out)
}

/// The cluster's own count of transactions its resolver has aborted.
///
/// `None` when the special key cannot be read or the document does not carry
/// the counter — which fails the leg, because the alternative is a leg that
/// passes with no evidence that any two transactions ever met.
async fn cluster_conflicts(db: &Database) -> Option<u64> {
    let raw: Option<Vec<u8>> = db
        .run(|trx, _| async move {
            // The status module of the special key space. Both options are
            // set because a client library that gates the read behind either
            // one refuses it silently otherwise, and this read is advisory to
            // the transaction and load-bearing to the report.
            let _ = trx.set_option(TransactionOption::ReadSystemKeys);
            let _ = trx.set_option(TransactionOption::SpecialKeySpaceRelaxed);
            Ok(trx
                .get(STATUS_JSON_KEY, false)
                .await?
                .map(|value| value.to_vec()))
        })
        .await
        .ok()
        .flatten();
    let document: serde_json::Value = serde_json::from_slice(&raw?).ok()?;
    document
        .get("cluster")?
        .get("workload")?
        .get("transactions")?
        .get("conflicted")?
        .get("counter")?
        .as_u64()
}

/// One trader's event log, in order.
fn read_trader_events(path: &PathBuf) -> Vec<TraderEvent> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(sent_us: u64, acked_us: Option<u64>) -> Option<Attempt> {
        Some(Attempt {
            sent_us,
            acked_us,
            committed: false,
            reason: None,
        })
    }

    /// The honesty clause, stated over its two interesting shapes: attempts
    /// whose in-flight windows touch are a race, and attempts that are strictly
    /// one after the other are a sequence — which is what this leg exists to
    /// refuse to call a race.
    #[test]
    fn overlap_separates_a_race_from_a_sequence() {
        assert!(overlapped(attempt(100, Some(900)), attempt(150, Some(950))));
        assert!(overlapped(attempt(100, Some(200)), attempt(200, Some(300))));
        assert!(!overlapped(
            attempt(100, Some(200)),
            attempt(201, Some(300))
        ));
        assert!(!overlapped(
            attempt(300, Some(400)),
            attempt(100, Some(200))
        ));
    }

    /// An attempt nothing answered is not an overlap. Its interval is open at
    /// the right-hand end and would otherwise intersect anything, which would
    /// let the ack timeout that fails the `unanswered_attempts` clause satisfy
    /// the `rounds_overlapped` one.
    #[test]
    fn an_unanswered_attempt_is_not_an_overlap() {
        assert!(!overlapped(attempt(100, None), attempt(150, Some(950))));
        assert!(!overlapped(attempt(100, Some(900)), None));
    }

    /// The two racers must never share an `intent_id`: a shared one is a
    /// *replay*, answered by the `intent/{intent_id}` idempotency row before
    /// any ledger read, so no conflict would ever arise and the leg would
    /// measure arm (a) while claiming arm (b).
    #[test]
    fn the_two_racers_never_share_an_intent_id() {
        for round in 0..64u32 {
            assert_ne!(
                attempt_intent_id(Side::A, round),
                attempt_intent_id(Side::B, round)
            );
        }
        let all: std::collections::BTreeSet<u128> = (0..64u32)
            .flat_map(|round| {
                [
                    attempt_intent_id(Side::A, round),
                    attempt_intent_id(Side::B, round),
                ]
            })
            .chain([credit_intent_id(Side::A), credit_intent_id(Side::B)])
            .collect();
        assert_eq!(all.len(), 64 * 2 + 2);
    }

    /// Each round contends over its own item, and every one of them carries the
    /// leg's discriminating prefix so a key dump distinguishes them from a
    /// seeded world's rows.
    #[test]
    fn every_round_has_its_own_item() {
        let items: std::collections::BTreeSet<u64> =
            (0..32u32).map(|r| ITEM_BASE | u64::from(r)).collect();
        assert_eq!(items.len(), 32);
        assert!(items.iter().all(|item| item & ITEM_BASE == ITEM_BASE));
    }
}
