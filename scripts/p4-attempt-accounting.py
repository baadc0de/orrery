#!/usr/bin/env python3
"""The attempt and accounting contract for multi-human campaigns (#572).

Piece 1 of #563's adopted decomposition, and it lands *before* the hours it
accounts for. Today one attempt means one human, so an attempt's hours are
unambiguous: the host writes `player_hours = peers * seconds / 3600`
(`gates/p1-swarm/src/swarm.rs:1499`), assembly copies that number verbatim onto
the one human row (`scripts/p4-campaign-session.sh` `cmd_assemble`), and the
ledger banks it without ever comparing it to the signed `banked_minutes`
(`scripts/p4-ledger.sh` `cmd_append`). With N humans in one attempt that stops
being merely coarse and becomes wrong in the one direction #240 cannot tolerate:
the whole cohort's hours would be attributed to *each* participant. #240's entire
discipline is an auditable denominator, so the denominator has to be fixed while
there are still no cohort hours banked against it.

This script is the executable half of that contract. It reads one `AttemptReport`
plus the attempt's signed client rows and derives the ledger inputs:

    one bot contribution        player_hours = B * valid_attempt_seconds / 3600
    one row per signed human    player_hours = banked_minutes / 60

Every human row is bound to exactly one exterior `(attempt_id, slot, session_id,
node)`, and no two rows may bind to the same one. That bijection is the property:
a schema-shaped test proves nothing about it.

`docs/plans/multi-human-attempt-accounting.md` is the normative statement; this
file is what makes it fail when it is broken. It does not replace
`scripts/p4-campaign-session.sh` — that single-human assembler and the ledger's
own repair are #563's piece 7, which consumes this contract rather than
restating it.

usage:
  p4-attempt-accounting.py derive <attempt.json> <records.jsonl> <out-dir>
  p4-attempt-accounting.py --self-test
"""

from __future__ import annotations

import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, NoReturn

NAME = "p4-attempt-accounting"
ROOT = Path(__file__).resolve().parent.parent

UUID_V7 = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)

# Restated from `scripts/p4-ledger.sh`, because a contract that derives a row is
# a contract that must refuse to derive an unbankable one. These are *retained*
# — the ledger keeps checking every one of them against the file it is handed,
# and nothing here loosens any of them. Checking them earlier only means the
# refusal names the attempt rather than the derived row.
MIN_COVERAGE = 0.95
BAND_LO = 0.03
BAND_HI = 0.05

# The same four trees `scripts/p4-ledger.sh` and `scripts/p4-campaign-session.sh`
# hash, in the same order. `--self-test` holds this list against the ledger's, so
# a stamped digest cannot drift out of the cross-check it exists to satisfy.
PIPELINE_TREES = (
    "crates/orrery_witness",
    "crates/orrery_core",
    "crates/orrery_games",
    "gates/p1-swarm",
)

# ── The per-leg impairment band, chosen here because #240 requires impairment
# *applied* and a cohort attempt has one leg per human ────────────────────────
#
# The router draws per packet, so a slot's directed links are a binomial sample
# of the configured loss p. The acceptance band is three standard deviations of
# that binomial, which is plain arithmetic rather than a tolerance invented to
# make a run pass:
#
#     sigma = sqrt(p * (1 - p) / n)
#     band  = [p - 3*sigma, p + 3*sigma]
#
# At p = 0.03 and the floor below that is 0.03 +/- 0.0162, i.e. [1.38%, 4.62%].
# The floor exists because the band is meaningless at small n: at n = 20 the
# three-sigma band reaches past 14% and admits a link that dropped nothing.
LINK_SAMPLE_FLOOR = 1000
LINK_BAND_SIGMAS = 3.0

# A close reason that leaves the leg's evidence intact. `queue_overflow` does
# not: the host counted downlink frames it could not deliver, so that human's
# observed link is the pump's backlog and not the declared profile.
BANKABLE_CLOSES = ("goodbye", "attempt_end", "disconnected")
KNOWN_CLOSES = BANKABLE_CLOSES + ("queue_overflow", "never_connected")


def die(detail: str) -> NoReturn:
    raise SystemExit(f"{NAME}: {detail}")


def note(detail: str) -> None:
    print(f"{NAME}: {detail}", file=sys.stderr)


def refuse(detail: str) -> NoReturn:
    die(f"refusing to derive: {detail}")


# ── Reading the attempt ──────────────────────────────────────────────────────


def normalize_exteriors(attempt: dict[str, Any]) -> list[dict[str, Any]]:
    """`exteriors` is the contract; `external` is the one-slot spelling.

    #571 replaces `Option<ExteriorSlot>` with slot-indexed exteriors and will
    serialize `exteriors` directly. Until it lands, a today's-shape report with
    a single `external` block reads as a one-element cohort, so this contract
    can be exercised against real host output before the host emits the plural
    field. The two are never both authoritative: a report carrying both must
    agree, or the accounting is being asked to choose between two accounts of
    the same attempt.
    """
    plural = attempt.get("exteriors")
    single = attempt.get("external")
    if plural is None and single is None:
        return []
    if plural is None:
        return [
            {
                "slot": single.get("slot", single.get("index")),
                "session_id": single.get("session_id"),
                "node": single.get("node"),
                "connected_ticks": single.get("connected_ticks"),
                "frames": {
                    "uplink": single.get("uplink_frames", 0),
                    "downlink": single.get("downlink_frames", 0),
                    "downlink_dropped": single.get("downlink_dropped", 0),
                },
                "close": single.get(
                    "close", "goodbye" if single.get("said_goodbye") else "disconnected"
                ),
            }
        ]
    if not isinstance(plural, list):
        refuse("attempt.exteriors is not a list")
    if single is not None:
        slots = {entry.get("slot") for entry in plural}
        if single.get("index", single.get("slot")) not in slots:
            refuse(
                "the attempt carries both `external` and `exteriors` and they name "
                "different slots; one attempt has one account of its seats"
            )
    return plural


def check_retained_attempt_clauses(attempt: dict[str, Any]) -> None:
    """Attempt-wide evidence. These invalidate *every* contribution, bot included.

    Section 7 of `docs/plans/multi-human-campaign.md`: one exterior's disconnect
    invalidates only that human's contribution, because the rest of the cohort's
    state, traffic and evidence remain measured. A false positive, a blind
    witness, an unbalanced deferral ledger or a clean link are properties of the
    shared evidence, so they take the whole attempt with them.
    """
    if attempt.get("witnessing") is not True:
        refuse("the witness did not run, so this attempt measured no false-positive rate")
    false_positives = attempt.get("total_false_positives", 0)
    if false_positives != 0:
        refuse(f"{false_positives} signal(s) raised against honest peers")
    coverage = attempt.get("observation_coverage", 0)
    if not isinstance(coverage, (int, float)) or coverage < MIN_COVERAGE:
        refuse(f"observation coverage {coverage} is below the {MIN_COVERAGE} floor")
    if attempt.get("deferral_ledger_balances") is not True:
        refuse("the deferral ledger does not balance, so coverage is a lower bound")
    impairment = attempt.get("identity", {}).get("impairment", {})
    loss = impairment.get("loss", 0)
    if not BAND_LO <= loss <= BAND_HI:
        refuse(f"loss {loss} is outside the criterion's {BAND_LO}-{BAND_HI} band")
    if not (impairment.get("jitter_ticks", 0) > 0 and impairment.get("jitter_rate", 0) > 0):
        refuse("no jitter was injected")
    if attempt.get("completed") is False:
        refuse(
            "the attempt did not complete; a partial attempt preserves its rows for "
            "diagnosis and banks none of them"
        )


def valid_attempt_seconds(attempt: dict[str, Any]) -> float:
    seconds = attempt.get("valid_attempt_seconds", attempt.get("seconds"))
    if not isinstance(seconds, (int, float)) or seconds <= 0:
        refuse("the attempt accumulated no valid seconds")
    return float(seconds)


def seconds_per_tick(attempt: dict[str, Any]) -> float:
    ticks = attempt.get("ticks")
    seconds = attempt.get("seconds")
    if not isinstance(ticks, int) or ticks <= 0:
        refuse("the attempt does not say how many ticks it ran; a connected span is unmeasurable")
    if not isinstance(seconds, (int, float)) or seconds <= 0:
        refuse("the attempt does not say how many seconds it ran")
    return float(seconds) / float(ticks)


# ── The per-leg impairment evidence ──────────────────────────────────────────


def link_evidence(attempt: dict[str, Any], slot: int) -> tuple[int, int, int]:
    links = attempt.get("per_link_impairment")
    if not isinstance(links, list):
        refuse(
            "the attempt carries no per_link_impairment; a cohort attempt's aggregate "
            "cannot verify impairment for one human's leg"
        )
    carried = dropped = delayed = 0
    for link in links:
        if link.get("from_slot") != slot and link.get("to_slot") != slot:
            continue
        carried += int(link.get("delivered", 0)) + int(link.get("dropped", 0))
        dropped += int(link.get("dropped", 0))
        delayed += int(link.get("delayed", 0))
    return carried, dropped, delayed


def check_link_impairment(attempt: dict[str, Any], slot: int) -> dict[str, Any]:
    carried, dropped, delayed = link_evidence(attempt, slot)
    if carried < LINK_SAMPLE_FLOOR:
        refuse(
            f"slot {slot} carried {carried} packets, below the {LINK_SAMPLE_FLOOR}-packet "
            "floor the loss band needs to mean anything"
        )
    if delayed == 0:
        refuse(f"slot {slot}'s links were never delayed; the jitter half was not exercised")
    if dropped == 0:
        refuse(f"slot {slot}'s links dropped nothing; that leg ran clean")
    configured = attempt["identity"]["impairment"]["loss"]
    sigma = math.sqrt(configured * (1.0 - configured) / carried)
    low = max(0.0, configured - LINK_BAND_SIGMAS * sigma)
    high = configured + LINK_BAND_SIGMAS * sigma
    observed = dropped / carried
    if not low <= observed <= high:
        refuse(
            f"slot {slot} observed {observed:.5f} loss over {carried} packets, outside the "
            f"{LINK_BAND_SIGMAS:g}-sigma band [{low:.5f}, {high:.5f}] around the configured "
            f"{configured}"
        )
    return {
        "slot": slot,
        "packets": carried,
        "dropped": dropped,
        "delayed": delayed,
        "observed_loss": observed,
        "band": [low, high],
        "band_sigmas": LINK_BAND_SIGMAS,
    }


# ── The pipeline digest, computed exactly as the ledger computes it ──────────


def sha256_hex(data: bytes) -> str:
    import hashlib

    return hashlib.sha256(data).hexdigest()


def pipeline_id(commit: str) -> str:
    override = os.environ.get("P4_PIPELINE_ID")
    if override:
        return override
    root = os.environ.get("P4_ROOT", str(ROOT))
    probe = subprocess.run(
        ["git", "-C", root, "rev-parse", "--verify", "--quiet", f"{commit}^{{commit}}"],
        capture_output=True,
        check=False,
    )
    if probe.returncode != 0:
        die(f"commit {commit} is not in this checkout; cannot hash the pipeline subtree it ran")
    lines = ""
    for tree in PIPELINE_TREES:
        result = subprocess.run(
            ["git", "-C", root, "rev-parse", f"{commit}:{tree}"],
            capture_output=True,
            check=False,
            text=True,
        )
        if result.returncode != 0:
            die(f"no tree {tree} at {commit}")
        lines += f"{tree}={result.stdout.strip()}\n"
    return sha256_hex(lines.encode())[:16]


# ── The signed client rows ───────────────────────────────────────────────────


def load_records(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            die(f"{path}:{number} is not JSON: {error}")
        if not isinstance(row, dict):
            die(f"{path}:{number} is not a JSON object")
        rows.append(row)
    return rows


def verify_signature(row: dict[str, Any], node: str) -> None:
    result = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "verify-campaign-measurement.py"), node],
        input=json.dumps(row),
        capture_output=True,
        check=False,
        text=True,
    )
    if result.returncode != 0:
        refuse(
            "the client measurement signature did not verify for the node the host admitted "
            f"at that seat ({result.stdout.strip() or result.stderr.strip()})"
        )


def check_mismatch_flag(row: dict[str, Any]) -> None:
    """Retained from `p4-ledger.sh` and `p4-campaign-session.sh`, unchanged.

    The flag is recomputable from the row's own numbers; a row whose flag
    contradicts them is not evidence, in either direction.
    """
    configured = row.get("configured_impairment_profile", {})
    expected = (
        row.get("observed_loss_pct") != configured.get("loss_pct")
        or row.get("observed_jitter_p50_ms") != configured.get("jitter_p50_ms")
        or row.get("observed_jitter_p99_ms") != configured.get("jitter_p99_ms")
    )
    if row.get("impairment_mismatch") != expected:
        refuse("a client row's impairment_mismatch contradicts its own observed/configured numbers")


# ── The derivation ───────────────────────────────────────────────────────────


def derive(attempt_path: Path, records_path: Path, out_dir: Path) -> list[Path]:
    try:
        attempt = json.loads(attempt_path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        die(f"unreadable attempt report {attempt_path}: {error}")
    if not isinstance(attempt, dict):
        die("the attempt report is not a JSON object")

    attempt_id = attempt.get("attempt_id")
    if not isinstance(attempt_id, str) or not UUID_V7.match(attempt_id):
        refuse("the attempt report carries no UUIDv7 attempt_id to bind rows to")
    check_retained_attempt_clauses(attempt)

    host_target = attempt.get("identity", {}).get("target")
    if not isinstance(host_target, str) or not host_target:
        refuse("the attempt report does not name the host target triple")
    bots = attempt.get("bots")
    if not isinstance(bots, int) or bots < 0:
        refuse("the attempt report does not say how many bot seats it ran (`bots`)")
    seconds = valid_attempt_seconds(attempt)
    per_tick = seconds_per_tick(attempt)
    commit = attempt.get("identity", {}).get("commit", "unknown")
    pipeline = pipeline_id(commit)

    exteriors = normalize_exteriors(attempt)
    by_session: dict[str, dict[str, Any]] = {}
    seen_slots: set[int] = set()
    for entry in exteriors:
        slot = entry.get("slot")
        if not isinstance(slot, int):
            refuse("an exterior entry carries no slot")
        if slot in seen_slots:
            refuse(f"the attempt reports slot {slot} twice; a seat is occupied once per attempt")
        seen_slots.add(slot)
        if slot < bots:
            refuse(f"exterior slot {slot} overlaps the bot seats [0, {bots})")
        close = entry.get("close")
        if close not in KNOWN_CLOSES:
            refuse(f"slot {slot} reports an unknown close reason {close!r}")
        session_id = entry.get("session_id")
        if session_id is None:
            continue
        if not isinstance(session_id, str) or not UUID_V7.match(session_id):
            refuse(f"slot {slot} names a session id that is not a UUIDv7")
        if session_id in by_session:
            refuse(
                f"session {session_id} is seated at two slots in one attempt; an interval is "
                "attributed exactly once"
            )
        by_session[session_id] = entry

    rows = load_records(records_path)
    # Nothing is written until every row has bound and validated. A refusal that
    # has already emitted the bot contribution is not a refusal: the operator is
    # left holding a directory of bankable-looking inputs for an attempt this
    # contract rejected.
    pending: list[tuple[str, dict[str, Any]]] = []

    # ── The bot contribution: one input, for the whole cohort of bots ────────
    bot_hours = bots * seconds / 3600.0
    bot_report = dict(attempt)
    bot_report.pop("session", None)
    bot_report.pop("external", None)
    bot_report["identity"] = dict(attempt["identity"])
    bot_report["identity"]["actor"] = "bot"
    bot_report["identity"]["target"] = host_target
    bot_report["identity"]["attempt_id"] = attempt_id
    bot_report["identity"].pop("human_session_id", None)
    bot_report["identity"].pop("slot", None)
    bot_report["player_hours"] = bot_hours
    bot_report["attempt"] = {
        "attempt_id": attempt_id,
        "host_target": host_target,
        "bots": bots,
        "valid_attempt_seconds": seconds,
    }
    bot_report["contribution"] = {
        "actor": "bot",
        "player_hours": bot_hours,
        "derivation": f"{bots} * {seconds:g} / 3600",
    }
    if bot_hours > 0:
        pending.append(("contribution-bot.json", bot_report))

    # ── One input per signed human interval ─────────────────────────────────
    #
    # The loop is over *rows*, not over seats: an attempt's denominator is not
    # constant, because a human seated for part of the attempt banks part of it.
    # The per-slot connected span is what makes that auditable, and it is the
    # ceiling every interval is held under below.
    claimed_sessions: set[str] = set()
    bound_slots: set[int] = set()
    human_total = 0.0
    for row in rows:
        session_id = row.get("session_id")
        if not isinstance(session_id, str) or not UUID_V7.match(session_id):
            refuse("a client row carries no UUIDv7 session_id")
        if session_id in claimed_sessions:
            refuse(
                f"session {session_id} appears in two client rows for one attempt; an interval "
                "is attributed exactly once"
            )
        claimed_sessions.add(session_id)
        if row.get("actor") != "human":
            refuse(f"the client row for {session_id} does not name a human actor")

        entry = by_session.get(session_id)
        if entry is None:
            refuse(
                f"session {session_id} is not seated in attempt {attempt_id}; every row binds to "
                "a matching exterior (attempt, slot, sid, node)"
            )
        slot = entry["slot"]
        if slot in bound_slots:
            refuse(f"two rows bind to slot {slot}; one seat carries one interval per attempt")
        bound_slots.add(slot)

        node = entry.get("node")
        if not isinstance(node, str) or len(node) != 64:
            refuse(f"slot {slot} does not name the QUIC-authenticated node it admitted")
        if row.get("measurement_node") != node:
            refuse(
                f"the row for session {session_id} was signed by "
                f"{row.get('measurement_node')!r}, not by the node the host admitted at slot "
                f"{slot}"
            )
        verify_signature(row, node)
        check_mismatch_flag(row)

        close = entry.get("close")
        if close not in BANKABLE_CLOSES:
            refuse(f"slot {slot} closed as {close!r}; that leg's evidence does not bank")
        connected_ticks = entry.get("connected_ticks")
        if not isinstance(connected_ticks, int) or connected_ticks <= 0:
            refuse(f"slot {slot} reports no connected ticks; an unseated slot banks nothing")
        connected_minutes = connected_ticks * per_tick / 60.0
        banked_minutes = row.get("banked_minutes")
        distinct = row.get("distinct_play_minutes")
        if not isinstance(banked_minutes, (int, float)) or banked_minutes < 0:
            refuse(f"the row for session {session_id} carries no banked_minutes")
        if not isinstance(distinct, (int, float)) or banked_minutes > distinct:
            refuse(f"the row for session {session_id} banks more than it played")
        # The honest caveat, made a refusal rather than an assumption: a client's
        # own claim about how long it played is bounded by the host's record of
        # how long that seat was connected. A tolerance of one tick absorbs the
        # boundary rounding between the two clocks and nothing else.
        if banked_minutes > connected_minutes + per_tick / 60.0:
            refuse(
                f"session {session_id} banks {banked_minutes:g} min but slot {slot} was "
                f"connected for {connected_minutes:.4f} min; an interval cannot exceed its seat's "
                "connected span"
            )

        platform = row.get("platform_triple")
        if not isinstance(platform, str) or not platform:
            refuse(f"the row for session {session_id} carries no platform_triple")

        hours = banked_minutes / 60.0
        human_total += hours
        report = dict(attempt)
        report["identity"] = dict(attempt["identity"])
        report["identity"]["actor"] = "human"
        report["identity"]["human_session_id"] = session_id
        # Mixed-platform rule. The measurement target of a human row is *that
        # participant's* signed platform triple, never the host's: a Linux host
        # assembling a Windows session must not have to lie about one side of it
        # (`scripts/p4-campaign-session.sh` refuses that case outright today).
        # `attempt.host_target` retains the host's own triple verbatim, and the
        # bot contribution above is the row that carries it.
        report["identity"]["target"] = platform
        report["identity"]["attempt_id"] = attempt_id
        report["identity"]["slot"] = slot
        report["player_hours"] = hours
        report["session"] = dict(row)
        report["session"]["pipeline_digest"] = pipeline
        # `p4-ledger.sh` verifies the row's signature against `.external.node`.
        # For a cohort that field is per-row, and it is the *bound* node.
        report["external"] = {"node": node}
        report["attempt"] = {
            "attempt_id": attempt_id,
            "host_target": host_target,
            "bots": bots,
            "valid_attempt_seconds": seconds,
        }
        report["binding"] = {
            "attempt_id": attempt_id,
            "slot": slot,
            "session_id": session_id,
            "node": node,
            "connected_ticks": connected_ticks,
            "connected_minutes": connected_minutes,
            "close": close,
        }
        report["contribution"] = {
            "actor": "human",
            "player_hours": hours,
            "derivation": f"{banked_minutes:g} / 60",
        }
        report["link_impairment"] = check_link_impairment(attempt, slot)
        pending.append((f"contribution-human-{slot}-{session_id}.json", report))

    out_dir.mkdir(parents=True, exist_ok=True)
    written: list[Path] = []
    for filename, report in pending:
        path = out_dir / filename
        path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        written.append(path)

    total = bot_hours + human_total
    note(
        f"attempt {attempt_id}: {len(written)} ledger input(s), "
        f"{bot_hours:g} bot + {human_total:g} human = {total:g} player-hours"
    )
    print(
        json.dumps(
            {
                "attempt_id": attempt_id,
                "host_target": host_target,
                "bots": bots,
                "valid_attempt_seconds": seconds,
                "bot_hours": bot_hours,
                "human_hours": human_total,
                "attempt_total_hours": total,
                "inputs": [str(path) for path in written],
            },
            indent=2,
            sort_keys=True,
        )
    )
    return written


# ── Self-test ────────────────────────────────────────────────────────────────
#
# Every case below is named, and the names are what a mutation check reports.
# The property under test is exactly-once attribution and the
# `(attempt, slot, sid, node)` binding — not the schema's shape, which a report
# can satisfy while attributing one interval to two people.

FIXTURE_ATTEMPT = "018f9000-0000-7000-8000-00000000a001"
SESSION_A = "018f9000-0000-7000-8000-0000000000a1"
SESSION_B = "018f9000-0000-7000-8000-0000000000b2"
SESSION_C = "018f9000-0000-7000-8000-0000000000c3"


def sign_row(row: dict[str, Any], secret_byte: int) -> dict[str, Any]:
    result = subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts" / "sign-campaign-measurement-fixture.py"),
            "--secret-byte",
            str(secret_byte),
        ],
        input=json.dumps(row),
        capture_output=True,
        check=True,
        text=True,
    )
    return json.loads(result.stdout)


def fixture_row(
    session_id: str,
    banked_minutes: float,
    platform: str,
    secret_byte: int,
) -> dict[str, Any]:
    return sign_row(
        {
            "session_id": session_id,
            "wall_start": "2026-08-27T12:00:00Z",
            "wall_end": "2026-08-27T13:00:00Z",
            "distinct_play_minutes": banked_minutes,
            "banked_minutes": banked_minutes,
            "platform_triple": platform,
            "client_rev": "self-test",
            "ruleset_id": "52",
            "ruleset_version": 16,
            "pipeline_digest": "unavailable-client-side",
            "actor": "human",
            "configured_impairment_profile": {
                "loss_pct": 3,
                "jitter_p50_ms": 100,
                "jitter_p99_ms": 100,
            },
            "observed_loss_pct": 3,
            "observed_jitter_p50_ms": 100,
            "observed_jitter_p99_ms": 100,
            "afk_seconds": 0,
            "afk_capped": False,
            "impairment_mismatch": False,
        },
        secret_byte,
    )


def fixture_links(slots: list[int], bots: int) -> list[dict[str, Any]]:
    """Directed links carrying enough packets for the band to mean something.

    3.0% of 4,000 is 120 drops, comfortably inside the three-sigma band at that
    n, and the delayed count is the 10% jitter rate.
    """
    links = []
    for slot in slots:
        for other in list(range(bots)) + [s for s in slots if s != slot]:
            for a, b in ((slot, other), (other, slot)):
                links.append(
                    {
                        "from_slot": a,
                        "to_slot": b,
                        "lane": "state",
                        "delivered": 1940,
                        "dropped": 60,
                        "delayed": 200,
                        "bytes": 1940 * 512,
                    }
                )
    return links


def fixture_attempt(
    exteriors: list[dict[str, Any]],
    bots: int = 4,
    host_target: str = "x86_64-unknown-linux-gnu",
) -> dict[str, Any]:
    return {
        "attempt_id": FIXTURE_ATTEMPT,
        "identity": {
            "seed": 5,
            "impairment": {
                "loss": 0.03,
                "jitter_ticks": 6,
                "jitter_rate": 0.1,
                "retransmit_ticks": 3,
            },
            "target": host_target,
            "commit": "0" * 40,
        },
        "started_at_unix_secs": 1750000000,
        "bots": bots,
        "peers": bots,
        "seconds": 3600,
        "ticks": 3600 * 30,
        "valid_attempt_seconds": 3600,
        "completed": True,
        "witnessing": True,
        "total_false_positives": 0,
        "observation_coverage": 1.0,
        "deferral_ledger_balances": True,
        "total_gaps": 164022,
        "total_shed": 162,
        "exteriors": exteriors,
        "per_link_impairment": fixture_links(
            [entry["slot"] for entry in exteriors], bots
        ),
    }


def fixture_exterior(
    slot: int, session_id: str, node: str, minutes: float, close: str = "goodbye"
) -> dict[str, Any]:
    return {
        "slot": slot,
        "session_id": session_id,
        "node": node,
        "connected_ticks": int(minutes * 60 * 30),
        "frames": {"uplink": 100000, "downlink": 400000, "downlink_dropped": 0},
        "close": close,
    }


class SelfTest:
    def __init__(self, directory: Path) -> None:
        self.directory = directory
        self.passed = 0
        self.names: list[str] = []

    def ok(self, name: str) -> None:
        self.passed += 1
        self.names.append(name)
        print(f"{NAME}: PASS {name}")

    def run(
        self, attempt: dict[str, Any], rows: list[dict[str, Any]], tag: str
    ) -> subprocess.CompletedProcess[str]:
        case = self.directory / tag
        if case.exists():
            shutil.rmtree(case)
        case.mkdir(parents=True)
        (case / "attempt.json").write_text(json.dumps(attempt))
        (case / "records.jsonl").write_text(
            "".join(json.dumps(row) + "\n" for row in rows)
        )
        return subprocess.run(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "derive",
                str(case / "attempt.json"),
                str(case / "records.jsonl"),
                str(case / "out"),
            ],
            capture_output=True,
            check=False,
            text=True,
        )

    def must_derive(
        self, attempt: dict[str, Any], rows: list[dict[str, Any]], tag: str
    ) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        result = self.run(attempt, rows, tag)
        if result.returncode != 0:
            die(f"self-test [{tag}]: an honest attempt refused to derive: {result.stderr.strip()}")
        manifest = json.loads(result.stdout)
        emitted = [json.loads(Path(path).read_text()) for path in manifest["inputs"]]
        return manifest, emitted

    def must_refuse(
        self, attempt: dict[str, Any], rows: list[dict[str, Any]], tag: str, fragment: str
    ) -> None:
        result = self.run(attempt, rows, tag)
        if result.returncode == 0:
            die(f"self-test [{tag}]: this must not derive, and it did")
        if fragment not in result.stderr:
            die(
                f"self-test [{tag}]: refused for the wrong reason; expected "
                f"{fragment!r} in {result.stderr.strip()!r}"
            )
        out = self.directory / tag / "out"
        if out.exists() and any(out.iterdir()):
            die(f"self-test [{tag}]: a refusal still wrote ledger inputs")


def self_test() -> None:
    for tool in ("openssl", "git", "jq"):
        if shutil.which(tool) is None:
            die(f"{tool} is required and not on PATH")

    # The stamped digest must be the ledger's arithmetic, tree for tree, in
    # order — the same two-source check `p4-campaign-session.sh` makes.
    ledger = (ROOT / "scripts" / "p4-ledger.sh").read_text()
    block = re.search(r"^readonly PIPELINE_TREES=\(\n(.*?)^\)$", ledger, re.M | re.S)
    if block is None:
        die("self-test: cannot read PIPELINE_TREES out of p4-ledger.sh")
    theirs = tuple(line.strip() for line in block.group(1).splitlines() if line.strip())
    if theirs != PIPELINE_TREES:
        die(
            "self-test: the pipeline tree list drifted from p4-ledger.sh; the stamped digest "
            "would fail its cross-check"
        )

    os.environ["P4_PIPELINE_ID"] = "selftestpipeline"
    with tempfile.TemporaryDirectory() as raw:
        directory = Path(raw)
        test = SelfTest(directory)

        node_a = fixture_row(SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11)[
            "measurement_node"
        ]
        node_b = fixture_row(SESSION_B, 42, "x86_64-unknown-linux-gnu", 0x22)[
            "measurement_node"
        ]
        node_c = fixture_row(SESSION_C, 30, "x86_64-pc-windows-msvc", 0x33)[
            "measurement_node"
        ]
        row_a = fixture_row(SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11)
        row_b = fixture_row(SESSION_B, 42, "x86_64-unknown-linux-gnu", 0x22)
        row_c = fixture_row(SESSION_C, 30, "x86_64-pc-windows-msvc", 0x33)

        cohort = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55),
                fixture_exterior(5, SESSION_B, node_b, 50),
            ]
        )

        # ── one input per actor, and exactly one ─────────────────────────────
        manifest, emitted = test.must_derive(cohort, [row_a, row_b], "honest-cohort")
        if len(emitted) != 3:
            die(
                "self-test [one_input_per_actor]: expected one bot contribution plus one row "
                f"per signed interval, got {len(emitted)}"
            )
        actors = sorted(report["identity"]["actor"] for report in emitted)
        if actors != ["bot", "human", "human"]:
            die(f"self-test [one_input_per_actor]: emitted actors {actors}")
        test.ok("one_input_per_actor")

        # ── the failure mode this contract exists to prevent ────────────────
        #
        # Today's host writes `player_hours = peers * seconds / 3600` and today's
        # assembler copies it onto the human row. With a cohort that is the whole
        # cohort's hours banked once per participant. Each row must carry its own
        # contribution and nothing else.
        cohort_total = (cohort["peers"] + len(cohort["exteriors"])) * 3600 / 3600.0
        for report in emitted:
            if report["identity"]["actor"] != "human":
                continue
            session = report["session"]
            expected = session["banked_minutes"] / 60.0
            if abs(report["player_hours"] - expected) > 1e-9:
                die(
                    "self-test [human_row_banks_its_own_interval_not_the_cohort_total]: row for "
                    f"{session['session_id']} banks {report['player_hours']} not {expected}"
                )
            if abs(report["player_hours"] - cohort_total) < 1e-9:
                die(
                    "self-test [human_row_banks_its_own_interval_not_the_cohort_total]: row for "
                    f"{session['session_id']} banks the whole cohort's {cohort_total} hours"
                )
        test.ok("human_row_banks_its_own_interval_not_the_cohort_total")

        # 4 bots * 3600/3600 + 50/60 + 42/60 = 4 + 1.5333... = 5.5333...
        expected_total = 4.0 + 50 / 60.0 + 42 / 60.0
        if abs(manifest["attempt_total_hours"] - expected_total) > 1e-9:
            die(
                "self-test [attempt_total_is_bot_plus_signed_intervals]: "
                f"{manifest['attempt_total_hours']} != {expected_total}"
            )
        if abs(sum(r["player_hours"] for r in emitted) - expected_total) > 1e-9:
            die("self-test [attempt_total_is_bot_plus_signed_intervals]: rows do not sum to it")
        test.ok("attempt_total_is_bot_plus_signed_intervals")

        # ── exactly-once, from both directions ──────────────────────────────
        test.must_refuse(
            cohort,
            [row_a, row_a],
            "duplicate-row",
            "appears in two client rows",
        )
        test.ok("one_interval_may_not_be_banked_twice")

        two_seats = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55),
                fixture_exterior(5, SESSION_A, node_b, 50),
            ]
        )
        test.must_refuse(two_seats, [row_a], "session-two-seats", "seated at two slots")
        test.ok("one_session_may_not_occupy_two_seats")

        # ── the binding ─────────────────────────────────────────────────────
        wrong_node = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_b, 55),
                fixture_exterior(5, SESSION_B, node_a, 50),
            ]
        )
        test.must_refuse(
            wrong_node, [row_a], "wrong-node", "not by the node the host admitted at slot"
        )
        test.ok("row_bound_to_the_wrong_node_is_refused")

        unseated = fixture_attempt([fixture_exterior(4, SESSION_A, node_a, 55)])
        test.must_refuse(unseated, [row_a, row_b], "unseated", "is not seated in attempt")
        test.ok("row_with_no_exterior_is_refused")

        # A row bound to a slot the host filled with a *bot* is the same defect
        # wearing a valid slot number.
        overlapping = fixture_attempt([fixture_exterior(2, SESSION_A, node_a, 55)])
        test.must_refuse(overlapping, [row_a], "bot-seat", "overlaps the bot seats")
        test.ok("human_row_bound_to_a_bot_seat_is_refused")

        emitted_by_slot = {
            report["binding"]["slot"]: report
            for report in emitted
            if report["identity"]["actor"] == "human"
        }
        if sorted(emitted_by_slot) != [4, 5]:
            die(f"self-test [binding_names_the_seated_slot]: slots {sorted(emitted_by_slot)}")
        for slot, report in emitted_by_slot.items():
            binding = report["binding"]
            seat = next(e for e in cohort["exteriors"] if e["slot"] == slot)
            if (
                binding["attempt_id"] != cohort["attempt_id"]
                or binding["session_id"] != seat["session_id"]
                or binding["node"] != seat["node"]
                or report["session"]["session_id"] != seat["session_id"]
                or report["identity"]["human_session_id"] != seat["session_id"]
                or report["identity"]["slot"] != slot
            ):
                die(f"self-test [binding_names_the_seated_slot]: slot {slot} bound to {binding}")
        test.ok("binding_names_the_seated_slot")

        # ── the non-constant denominator ────────────────────────────────────
        short_seat = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 20),
                fixture_exterior(5, SESSION_B, node_b, 50),
            ]
        )
        test.must_refuse(
            short_seat, [row_a, row_b], "over-span", "cannot exceed its seat's connected span"
        )
        test.ok("interval_may_not_exceed_its_seats_connected_span")

        # A human seated for part of the attempt banks its part, and the bot
        # contribution is unaffected: that is what makes the denominator
        # auditable rather than constant.
        partial = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55),
                fixture_exterior(5, SESSION_B, node_b, 45, close="disconnected"),
            ]
        )
        manifest_partial, emitted_partial = test.must_derive(
            partial, [row_a, row_b], "partial-seat"
        )
        if abs(manifest_partial["bot_hours"] - 4.0) > 1e-9:
            die("self-test [a_disconnect_costs_only_its_own_interval]: bot hours moved")
        if abs(manifest_partial["human_hours"] - (50 + 42) / 60.0) > 1e-9:
            die("self-test [a_disconnect_costs_only_its_own_interval]: human hours moved")
        if len(emitted_partial) != 3:
            die("self-test [a_disconnect_costs_only_its_own_interval]: an input went missing")
        test.ok("a_disconnect_costs_only_its_own_interval")

        overflowed = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55, close="queue_overflow"),
                fixture_exterior(5, SESSION_B, node_b, 50),
            ]
        )
        test.must_refuse(overflowed, [row_a], "overflow", "does not bank")
        test.ok("a_queue_overflow_leg_banks_nothing")

        # ── mixed platform ──────────────────────────────────────────────────
        mixed = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55),
                fixture_exterior(6, SESSION_C, node_c, 35),
            ]
        )
        _, mixed_emitted = test.must_derive(mixed, [row_a, row_c], "mixed-platform")
        targets = {}
        for report in mixed_emitted:
            key = report["identity"].get("human_session_id", "bot")
            targets[key] = report["identity"]["target"]
            if report["attempt"]["host_target"] != "x86_64-unknown-linux-gnu":
                die("self-test [mixed_platform_rows_carry_their_own_target]: host_target lost")
        if targets.get("bot") != "x86_64-unknown-linux-gnu":
            die(
                "self-test [mixed_platform_rows_carry_their_own_target]: the bot contribution "
                f"is not on the host target ({targets.get('bot')})"
            )
        if targets.get(SESSION_C) != "x86_64-pc-windows-msvc":
            die(
                "self-test [mixed_platform_rows_carry_their_own_target]: a Windows client on a "
                f"Linux host was stamped {targets.get(SESSION_C)}"
            )
        if targets.get(SESSION_A) != "x86_64-unknown-linux-gnu":
            die("self-test [mixed_platform_rows_carry_their_own_target]: the Linux row moved")
        test.ok("mixed_platform_rows_carry_their_own_target")

        # ── the retained refusals, checked through the real ledger ───────────
        #
        # The derived rows are not a parallel ledger format. They go through
        # `p4-ledger.sh append` unmodified, against every refusal it already
        # makes, and the totals it prints are the proof that the cohort's hours
        # were counted once.
        ledger_file = directory / "hours.jsonl"
        env = dict(os.environ, P4_LEDGER_FILE=str(ledger_file))
        case = directory / "mixed-platform" / "out"
        for path in sorted(case.iterdir()):
            appended = subprocess.run(
                [str(ROOT / "scripts" / "p4-ledger.sh"), "append", str(path)],
                capture_output=True,
                check=False,
                text=True,
                env=env,
            )
            if appended.returncode != 0:
                die(
                    f"self-test [derived_rows_bank_through_the_real_ledger]: {path.name} was "
                    f"refused: {appended.stderr.strip()}"
                )
        banked = [json.loads(line) for line in ledger_file.read_text().splitlines()]
        if len(banked) != 3:
            die(
                "self-test [derived_rows_bank_through_the_real_ledger]: "
                f"{len(banked)} ledger lines for a three-input attempt"
            )
        hours = sum(line["player_hours"] for line in banked)
        expected_mixed = 4.0 + 50 / 60.0 + 30 / 60.0
        if abs(hours - expected_mixed) > 1e-9:
            die(
                "self-test [derived_rows_bank_through_the_real_ledger]: banked "
                f"{hours} not {expected_mixed}"
            )
        if len({line["run_key"] for line in banked}) != 3:
            die("self-test [derived_rows_bank_through_the_real_ledger]: run keys collided")
        if len({line["measurement_key"] for line in banked}) != 3:
            die(
                "self-test [derived_rows_bank_through_the_real_ledger]: two of this attempt's "
                "actors are one measurement"
            )
        totals = subprocess.run(
            [str(ROOT / "scripts" / "p4-ledger.sh"), "total"],
            capture_output=True,
            check=False,
            text=True,
            env=env,
        ).stdout
        if "windows: 0.5 distinct hours" not in totals:
            die(
                "self-test [derived_rows_bank_through_the_real_ledger]: the Windows human's half "
                f"hour did not reach the windows platform ({totals})"
            )
        test.ok("derived_rows_bank_through_the_real_ledger")

        # Re-deriving and re-appending the same attempt must add no hours: the
        # identity each row carries is what the ledger dedups on.
        for path in sorted(case.iterdir()):
            subprocess.run(
                [str(ROOT / "scripts" / "p4-ledger.sh"), "append", str(path)],
                capture_output=True,
                check=False,
                text=True,
                env=env,
            )
        again = [json.loads(line) for line in ledger_file.read_text().splitlines()]
        if len(again) != 3:
            die(
                "self-test [reappending_an_attempt_banks_no_second_cohort_hours]: "
                f"{len(again)} lines after a second append"
            )
        test.ok("reappending_an_attempt_banks_no_second_cohort_hours")

        # The cross-platform host/client row the ledger must keep refusing: a
        # Windows session stamped with the Linux host's triple. The mixed rule
        # above is what makes an honest Windows row assemblable; it must not
        # have made a *dishonest* one assemblable too.
        forged = json.loads(
            next(p for p in case.iterdir() if SESSION_C in p.name).read_text()
        )
        forged["identity"]["target"] = "x86_64-unknown-linux-gnu"
        forged_path = directory / "forged-target.json"
        forged_path.write_text(json.dumps(forged))
        refused = subprocess.run(
            [str(ROOT / "scripts" / "p4-ledger.sh"), "append", str(forged_path)],
            capture_output=True,
            check=False,
            text=True,
            env=dict(os.environ, P4_LEDGER_FILE=str(directory / "forged.jsonl")),
        )
        if refused.returncode == 0:
            die(
                "self-test [cross_platform_host_client_row_is_refused]: a Windows session "
                "stamped with the host's Linux triple banked"
            )
        test.ok("cross_platform_host_client_row_is_refused")

        # ── retained client-row refusals ────────────────────────────────────
        tampered = json.loads(json.dumps(row_a))
        tampered["observed_loss_pct"] = 0
        test.must_refuse(
            cohort, [tampered, row_b], "tampered-observation", "signature did not verify"
        )
        test.ok("post_hoc_edit_of_a_signed_row_is_refused")

        flag_only = json.loads(json.dumps(row_a))
        flag_only["impairment_mismatch"] = True
        test.must_refuse(cohort, [flag_only, row_b], "tampered-flag", "signature did not verify")
        test.ok("flipping_the_mismatch_flag_is_refused")

        # ── attempt-wide clauses ────────────────────────────────────────────
        for mutation, fragment, name in (
            ({"witnessing": False}, "the witness did not run", "unwitnessed_attempt_banks_nothing"),
            (
                {"total_false_positives": 1},
                "raised against honest peers",
                "a_false_positive_invalidates_every_contribution",
            ),
            (
                {"observation_coverage": 0.90},
                "below the 0.95 floor",
                "coverage_below_the_floor_invalidates_every_contribution",
            ),
            (
                {"deferral_ledger_balances": False},
                "deferral ledger does not balance",
                "an_unbalanced_deferral_ledger_invalidates_every_contribution",
            ),
            (
                {"completed": False},
                "did not complete",
                "a_partial_attempt_banks_nothing",
            ),
        ):
            broken = json.loads(json.dumps(cohort))
            broken.update(mutation)
            test.must_refuse(broken, [row_a, row_b], name.replace("_", "-"), fragment)
            test.ok(name)

        clean = json.loads(json.dumps(cohort))
        clean["identity"]["impairment"]["loss"] = 0.0
        test.must_refuse(clean, [row_a, row_b], "clean-link", "outside the criterion")
        test.ok("a_clean_link_attempt_banks_nothing")

        # ── per-leg impairment ──────────────────────────────────────────────
        unexercised = json.loads(json.dumps(cohort))
        for link in unexercised["per_link_impairment"]:
            if link["from_slot"] == 4 or link["to_slot"] == 4:
                link["delivered"] += link["dropped"]
                link["dropped"] = 0
        test.must_refuse(unexercised, [row_a, row_b], "leg-clean", "dropped nothing")
        test.ok("a_human_leg_that_ran_clean_banks_nothing")

        thin = json.loads(json.dumps(cohort))
        for link in thin["per_link_impairment"]:
            if link["from_slot"] == 4 or link["to_slot"] == 4:
                link["delivered"] = 1
                link["dropped"] = 0 if link["from_slot"] == 4 else 1
        test.must_refuse(thin, [row_a, row_b], "leg-thin", "below the 1000-packet floor")
        test.ok("a_leg_below_the_sample_floor_banks_nothing")

        outside = json.loads(json.dumps(cohort))
        for link in outside["per_link_impairment"]:
            if link["from_slot"] == 4 or link["to_slot"] == 4:
                link["dropped"] = link["delivered"] // 4
        test.must_refuse(outside, [row_a, row_b], "leg-band", "sigma band")
        test.ok("a_leg_outside_the_loss_band_banks_nothing")

        aggregate_only = json.loads(json.dumps(cohort))
        del aggregate_only["per_link_impairment"]
        test.must_refuse(aggregate_only, [row_a, row_b], "no-links", "no per_link_impairment")
        test.ok("an_attempt_without_per_link_evidence_banks_no_human")

        # ── the one-slot spelling still reads ───────────────────────────────
        legacy = fixture_attempt([])
        legacy["exteriors"] = []
        del legacy["exteriors"]
        legacy["external"] = {
            "index": 4,
            "session_id": SESSION_A,
            "node": node_a,
            "connected_ticks": 55 * 60 * 30,
            "said_goodbye": True,
            "uplink_frames": 100,
            "downlink_frames": 400,
            "downlink_dropped": 0,
        }
        legacy["per_link_impairment"] = fixture_links([4], 4)
        legacy_manifest, _ = test.must_derive(legacy, [row_a], "legacy-single")
        if abs(legacy_manifest["attempt_total_hours"] - (4.0 + 50 / 60.0)) > 1e-9:
            die("self-test [the_one_slot_spelling_still_derives]: total moved")
        test.ok("the_one_slot_spelling_still_derives")

        print(f"{NAME}: self-test passed ({test.passed} fixtures)")


def main() -> None:
    argv = sys.argv[1:]
    if argv[:1] == ["--self-test"]:
        self_test()
        return
    if argv[:1] == ["derive"] and len(argv) == 4:
        derive(Path(argv[1]), Path(argv[2]), Path(argv[3]))
        return
    print(__doc__ or "", file=sys.stderr)
    die(f"unknown command '{argv[0] if argv else '<none>'}'")


if __name__ == "__main__":
    main()
