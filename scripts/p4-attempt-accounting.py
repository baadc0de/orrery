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

# ── The impairment tolerance band (#973) ─────────────────────────────────────
#
# `impairment_mismatch` is computed by the client, in
# `clients/regolith/src/session.rs::CampaignSession::finish`, against a band
# (#718). This file *recomputes* the flag to check the row is internally
# coherent, and until #973 it recomputed with three bare `!=` against the
# configured floats. A measurement never lands exactly on its configuration, so
# the recomputation disagreed with every honest row the client ever signed, and
# `p4-ledger.sh`'s "impairment verified applied" criterion could only pass on a
# fixture that had written both sides equal.
#
# A recomputer is not an independent judge: it is a coherence check on the
# producer's arithmetic. It must therefore use the producer's band, not one of
# its own — a narrower band here would refuse rows the client correctly called
# clean, which is the same defect pointed the other way. `--self-test` holds
# these against `session.rs` and against `p4-ledger.sh`'s jq, so the three
# cannot drift apart.
#
# Why the loss band is 2.0 *percentage points*, absolute:
#
#   * Its floor is set by how far an honest measurement can sit from the
#     configuration. Two independent errors stack. The host's own acceptance
#     band for a leg is three binomial sigmas at the sample floor above —
#     +/-1.62 pp at p = 0.03, n = 1000 — so a link this file has already
#     *accepted* may genuinely have run 1.62 pp off the configured rate. On top
#     of that the client's estimator carries a residual: post-#976 the shipped
#     client reads 3.12-3.27% against host counters of 2.98-3.11% on the same
#     links, i.e. it over-reports by 0.14-0.27 pp. 1.62 + 0.27 = 1.89 pp is the
#     narrowest honest band; anything tighter flags links the host accepted.
#   * Its ceiling is set by the property the flag exists to provide. A session
#     that did not receive its impairment reads ~0% against a configured 3.0%,
#     a gap of the full 3.0 pp. The band must stay below that, and 2.0 leaves a
#     whole percentage point of margin.
#
# So the band is bracketed by [1.89, 3.00) and 2.0 is the round number inside
# it. It is also, not by coincidence, the width of the criterion's own 3-5%
# loss band, which is the reasoning `session.rs` records.
#
# Why jitter is an absolute millisecond band and loss is not a *relative* one:
# the unimpaired profile configures 0% loss and 0 ms jitter, and a relative
# band collapses to zero width there — it would flag every clean session ever
# recorded. Both bands are therefore absolute.
#
# Why 40 ms: the profile injects 100 ms into a tenth of the datagrams, and the
# percentiles are taken over inter-arrival *deviations* on a 20 Hz send grid,
# so a percentile can legitimately land a fraction of a send slot (50 ms) away
# from the configured figure. 40 ms is under one slot and well under the 100 ms
# the profile injects, so an unapplied delay (100 ms of gap) is still flagged.
IMPAIRMENT_LOSS_TOLERANCE_PCT = 2.0
IMPAIRMENT_JITTER_TOLERANCE_MS = 40.0

# The client suppresses the flag entirely below `MIN_IMPAIRMENT_SAMPLES` = 200
# observed packets, because below that a single drop moves the rate by whole
# percentage points. That denominator is *not* carried in the signed row, so it
# cannot be recomputed here. What the row does carry is how long the seat
# played, and 200 samples is one second of play at the 20 Hz send cadence the
# constant is named for; 200 / 20 / 60 minutes is the duration below which the
# client's suppression is a possible explanation for a clear flag. Above it,
# a clear flag over numbers outside the band is a row disagreeing with itself.
#
# The residual this leaves: a seat that played for minutes over a link that
# carried almost nothing has few samples and a long span, and its suppressed
# flag is refused here. That is the safe side — a session that banked minutes
# while measuring nothing is not evidence of impairment either way.
IMPAIRMENT_SUPPRESSION_SAMPLES = 200.0
IMPAIRMENT_SUPPRESSION_MINUTES = IMPAIRMENT_SUPPRESSION_SAMPLES / 20 / 60

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
    """`exteriors` is the contract; `external` is what the host actually emits.

    #571 landed as #579: `Option<ExteriorSlot>` is now a slot map and
    `SwarmReport.external` is a `Vec<ExteriorReport>` ordered by swarm slot, so
    the host's own spelling is already plural. This reads three shapes and
    normalizes them to one, because all three exist in the tree right now:

    * `exteriors`, this contract's field — a host that carries it is read from
      it directly;
    * `external` as a **list**, which is `gates/p1-swarm`'s output since #579;
    * `external` as a single **object**, the pre-#579 spelling, kept readable so
      an archived report from before that refactor still derives.

    The two field names are never both authoritative: a report carrying both
    must name the same seats, or the accounting is being asked to choose between
    two accounts of the same attempt.

    `ExteriorReport` (`gates/p1-swarm/src/swarm.rs`, after #579) carries `index`,
    `node`, `connected_ticks`, the host wall bracket added by #971
    (`connected_since_unix_millis` / `connected_until_unix_millis`), the frame
    counters, `said_goodbye`, `connected` and `witness_anchored`. A host that
    also stamps the invite id it seated makes `session_id` available per seat;
    one that does not leaves it `None`, and the seat's identity is then the node
    the host admitted there plus the operator's pinned id. Both paths are live,
    and `seat_for` in `derive` is where they meet — the second is the weaker of
    the two, because a node the host admitted at two seats has nothing left to
    tell them apart with.
    """

    def one(entry: Any) -> dict[str, Any]:
        if not isinstance(entry, dict):
            refuse("an exterior entry is not a JSON object")
        # `connected` is the bridge's belief at report time; a seat still
        # connected when the attempt ended closed *with* the attempt, which is
        # a bankable close and not a disconnect.
        close = entry.get("close")
        if close is None:
            if entry.get("said_goodbye"):
                close = "goodbye"
            elif entry.get("connected"):
                close = "attempt_end"
            else:
                close = "disconnected"
        return {
            "slot": entry.get("slot", entry.get("index")),
            "session_id": entry.get("session_id"),
            "node": entry.get("node"),
            "connected_ticks": entry.get("connected_ticks"),
            "connected_since_unix_millis": entry.get("connected_since_unix_millis"),
            "connected_until_unix_millis": entry.get("connected_until_unix_millis"),
            "frames": {
                "uplink": entry.get("uplink_frames", 0),
                "downlink": entry.get("downlink_frames", 0),
                "downlink_dropped": entry.get("downlink_dropped", 0),
            },
            "close": close,
        }

    plural = attempt.get("exteriors")
    single = attempt.get("external")
    if plural is None and single is None:
        return []
    if plural is None:
        if isinstance(single, list):
            return [one(entry) for entry in single]
        return [one(single)]
    if not isinstance(plural, list):
        refuse("attempt.exteriors is not a list")
    if single is not None:
        slots = {entry.get("slot") for entry in plural}
        singles = single if isinstance(single, list) else [single]
        for entry in singles:
            if not isinstance(entry, dict):
                refuse("an exterior entry is not a JSON object")
            if entry.get("index", entry.get("slot")) not in slots:
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


def connected_span_minutes(entry: dict[str, Any], per_tick: float) -> tuple[float, str]:
    """How long the host had this seat connected, in minutes, and on what basis.

    Two bases, and the difference between them is #971.

    The **wall bracket** — `connected_since_unix_millis` to
    `connected_until_unix_millis`, both stamped by the host — is the honest one,
    because the connected span is a wall-clock fact and this measures it
    directly. The host opens the bracket when it binds the seat, at or before
    the moment the client starts counting, and closes it at the first tick it
    saw the link down or at report time; so the bracket *contains* the client's
    interval and the one-tick tolerance at the call site is again only the
    boundary rounding between two clocks that its comment claims it is.

    The **tick count** — `connected_ticks` scaled by the report's nominal period
    — is the fallback, for a report that carries no stamps (one produced without
    `--stamp-wall-clock`, or archived from before this seam existed). It is
    *not* equivalent. `gates/p1-swarm`'s metronome sleeps out the remainder of a
    tick and never accumulates a deadline, so an overrun is lost permanently and
    the host runs at or below its nominal rate: 55.3-59.8 Hz measured against a
    60 Hz nominal, which understated a real 60 s seat by 13 to 153 ticks and
    refused seven honest attempts in a row.

    The fallback is safe precisely because it errs that way. A lagging host's
    tick basis is *shorter* than its wall bracket, so a report that omits the
    stamps can only be refused more readily, never less — omitting them is not a
    way to bank an interval the host did not seat.
    """
    def stamp(value: Any) -> bool:
        # `bool` is an `int` in Python and `true` is not a millisecond.
        return isinstance(value, int) and not isinstance(value, bool)

    since = entry.get("connected_since_unix_millis")
    until = entry.get("connected_until_unix_millis")
    if stamp(since) and stamp(until):
        if until < since:
            refuse(
                f"slot {entry.get('slot')} reports a seat released at {until} before it was "
                f"seated at {since}; a connected span cannot run backwards"
            )
        return (until - since) / 60_000.0, "host wall bracket"
    if since is not None or until is not None:
        refuse(
            f"slot {entry.get('slot')} stamps only one end of its connected span; a bracket "
            "needs both ends or neither"
        )
    connected_ticks = entry.get("connected_ticks")
    return connected_ticks * per_tick / 60.0, "host tick count at the nominal rate"


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


def impairment_number(row: dict[str, Any], key: str, value: Any) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        refuse(f"a client row's {key} is not a number; its impairment cannot be recomputed")
    return float(value)


def outside_impairment_band(row: dict[str, Any]) -> bool:
    """Does the row's observed impairment disagree with its configured profile?

    The band, and the reasoning behind its width, is at
    `IMPAIRMENT_LOSS_TOLERANCE_PCT`. The comparison is `>`, not `>=`, exactly as
    `clients/regolith/src/session.rs` writes it: a gap of exactly the tolerance
    is inside the band.
    """
    configured = row.get("configured_impairment_profile")
    if not isinstance(configured, dict):
        refuse("a client row carries no configured_impairment_profile to recompute against")
    gaps = (
        (
            abs(
                impairment_number(row, "observed_loss_pct", row.get("observed_loss_pct"))
                - impairment_number(row, "loss_pct", configured.get("loss_pct"))
            ),
            IMPAIRMENT_LOSS_TOLERANCE_PCT,
        ),
        (
            abs(
                impairment_number(
                    row, "observed_jitter_p50_ms", row.get("observed_jitter_p50_ms")
                )
                - impairment_number(row, "jitter_p50_ms", configured.get("jitter_p50_ms"))
            ),
            IMPAIRMENT_JITTER_TOLERANCE_MS,
        ),
        (
            abs(
                impairment_number(
                    row, "observed_jitter_p99_ms", row.get("observed_jitter_p99_ms")
                )
                - impairment_number(row, "jitter_p99_ms", configured.get("jitter_p99_ms"))
            ),
            IMPAIRMENT_JITTER_TOLERANCE_MS,
        ),
    )
    return any(gap > tolerance for gap, tolerance in gaps)


def check_mismatch_flag(row: dict[str, Any]) -> None:
    """Retained from `p4-ledger.sh` and `p4-campaign-session.sh`.

    The flag is recomputable from the row's own numbers *within the band the
    client computed it with* (#973); a row whose flag contradicts them is not
    evidence, in either direction.
    """
    flag = row.get("impairment_mismatch")
    if not isinstance(flag, bool):
        refuse("a client row's impairment_mismatch is not a boolean")
    outside = outside_impairment_band(row)
    if flag and not outside:
        refuse(
            "a client row's impairment_mismatch fired while its observed impairment sits "
            "inside the tolerance band its own configured profile allows"
        )
    if not flag and outside:
        played = row.get("distinct_play_minutes")
        if isinstance(played, bool) or not isinstance(played, (int, float)):
            refuse("a client row does not say how long it played; its clear flag cannot stand")
        if float(played) >= IMPAIRMENT_SUPPRESSION_MINUTES:
            refuse(
                "a client row's impairment_mismatch is clear while its observed impairment "
                "sits outside the tolerance band, over a span long enough to have sampled "
                "the packets the flag needs"
            )


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
    # The host's own account of a seat names a `node`, not an invite id:
    # `ExteriorReport` after #579 carries `index`, `node`, `connected_ticks`, the
    # frame counters and the close flags, and no `session_id`. The node is
    # therefore the seat identity a host-emitted attempt can be bound by, and it
    # is a *signed* one — the client's row carries the same value under its
    # Ed25519 signature. A node that named two seats would make that binding
    # ambiguous, so it is refused here as well as in the assembler.
    by_node: dict[str, list[dict[str, Any]]] = {}
    seen_seats: set[tuple[int, Any]] = set()
    for entry in exteriors:
        slot = entry.get("slot")
        if not isinstance(slot, int):
            refuse("an exterior entry carries no slot")
        # A seat is `(slot, session_id)`, not a slot (#1028). One volunteer who
        # relaunches inside an attempt is readmitted at the slot they held,
        # under a second pre-minted invite id, and the host reports that as two
        # entries on one index. Two entries on one index with *no* id to tell
        # them apart is still one seat reported twice, and still refused.
        if (slot, entry.get("session_id")) in seen_seats:
            if entry.get("session_id") is None:
                refuse(
                    f"the attempt reports slot {slot} twice with no session id to tell the "
                    "two seats apart; a seat is occupied once per attempt"
                )
            refuse(
                f"the attempt reports slot {slot} twice for session "
                f"{entry.get('session_id')}; a seat is occupied once per attempt"
            )
        seen_seats.add((slot, entry.get("session_id")))
        if slot < bots:
            refuse(f"exterior slot {slot} overlaps the bot seats [0, {bots})")
        close = entry.get("close")
        if close not in KNOWN_CLOSES:
            refuse(f"slot {slot} reports an unknown close reason {close!r}")
        node = entry.get("node")
        if isinstance(node, str) and node:
            # A node may be admitted at several seats — the same install
            # rejoining — but only when every one of those seats carries its own
            # invite id, which is what lets a signed row pick out which of them
            # it belongs to. Seated twice with an id missing on either side, the
            # binding is ambiguous and refused exactly as it was before #1028.
            prior = by_node.get(node)
            if prior is not None and (
                entry.get("session_id") is None
                or any(seat.get("session_id") is None for seat in prior)
            ):
                refuse(
                    f"node {node} is seated at two slots in one attempt with no session id to "
                    "tell them apart; a seat's admitted identity binds exactly one interval"
                )
            by_node.setdefault(node, []).append(entry)
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

    def seat_for(session_id: str, row: dict[str, Any]) -> dict[str, Any]:
        """The exterior this row binds to, by invite id when the host has one.

        When the host seats an invite id — this contract's `exteriors` spelling —
        that id is the binding, and a row naming an unseated id is refused. When
        it does not, which is every report `gates/p1-swarm` emits today, the
        binding is the QUIC-authenticated node the host admitted at that seat,
        matched against the `measurement_node` the client signed. Either way the
        row is bound to one seat by something the host recorded, never by
        position in a file; and when the host *does* carry an id it must be this
        row's, so the node path can never silently override a seated id.
        """
        seated = by_session.get(session_id)
        if seated is not None:
            return seated
        node = row.get("measurement_node")
        if isinstance(node, str) and node in by_node:
            seats = by_node[node]
            if len(seats) != 1:
                # Only reachable if the eager check above ever admits a node at
                # several id-carrying seats none of which is this row's session,
                # which `by_session` would already have bound. Named rather than
                # silently taking the first: a row bound by position is bound to
                # nobody in particular.
                refuse(
                    f"node {node} is seated at {len(seats)} slots and none of them is session "
                    f"{session_id}; a row binds to one exterior or to none"
                )
            claimed = seats[0].get("session_id")
            if claimed is not None and claimed != session_id:
                refuse(
                    f"slot {seats[0]['slot']} was pinned to session {claimed}, and the row "
                    f"signed by that seat's node names {session_id}; the host's copy and the "
                    "client's copy of the session id disagree"
                )
            return seats[0]
        if by_session:
            refuse(
                f"session {session_id} is not seated in attempt {attempt_id}; every row binds to "
                "a matching exterior (attempt, slot, sid, node)"
            )
        refuse(
            f"session {session_id} names node {row.get('measurement_node')!r}, which the host "
            f"admitted at no seat of attempt {attempt_id}; every row binds to a matching "
            "exterior (attempt, slot, sid, node)"
        )

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
    bound_seats: set[tuple[int, Any]] = set()
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

        entry = seat_for(session_id, row)
        slot = entry["slot"]
        # The seat, not the slot (#1028): a slot may hold two seats when one
        # install rejoined, and each of those seats carries one interval. Two
        # rows landing on the *same* seat is still one interval attributed
        # twice — including the case where a host that seats no ids has a single
        # entry for a node that two signed sessions both name.
        if (slot, entry.get("session_id")) in bound_seats:
            refuse(
                f"two rows bind to slot {slot} seat {entry.get('session_id')}; one seat carries "
                "one interval per attempt"
            )
        bound_seats.add((slot, entry.get("session_id")))

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
        connected_minutes, span_basis = connected_span_minutes(entry, per_tick)
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
                f"connected for {connected_minutes:.4f} min ({span_basis}); an interval cannot "
                "exceed its seat's connected span"
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
        # `p4-ledger.sh` verifies the row's signature against the node the host
        # admitted, and since #579 reads it out of `.external[]` — a list — and
        # requires it to appear there **exactly once**. A derived human row is
        # one seat's evidence, so its list is that one seat: the bound node, and
        # no other, which is what makes the ledger's "exactly once" check name
        # this seat rather than merely find the node somewhere in the cohort.
        report["external"] = [dict(entry, node=node)]
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
            "connected_since_unix_millis": entry.get("connected_since_unix_millis"),
            "connected_until_unix_millis": entry.get("connected_until_unix_millis"),
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
    observed: tuple[float, float, float] = (3, 100, 100),
    mismatch: bool = False,
) -> dict[str, Any]:
    """A signed client row.

    `observed` defaults to the configured profile exactly, which no measurement
    ever does; #973's regressions pass realistic figures instead. `mismatch` is
    the flag as the client would have written it, so a row can be signed with a
    flag that disagrees with its own numbers rather than edited into one (an
    edit fails the signature stage first, and never reaches the recomputation).
    """
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
            "observed_loss_pct": observed[0],
            "observed_jitter_p50_ms": observed[1],
            "observed_jitter_p99_ms": observed[2],
            "afk_seconds": 0,
            "afk_capped": False,
            "impairment_mismatch": mismatch,
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

    # #973: this file recomputes `impairment_mismatch` against a band, and a
    # recomputer using a different band from the producer refuses honest rows
    # just as surely as exact equality did. Hold all three copies together.
    session_rs = (ROOT / "clients" / "regolith" / "src" / "session.rs").read_text()
    for declaration, ours in (
        ("LOSS_TOLERANCE_PCT: f64", IMPAIRMENT_LOSS_TOLERANCE_PCT),
        ("JITTER_TOLERANCE_MS: u64", IMPAIRMENT_JITTER_TOLERANCE_MS),
        ("MIN_IMPAIRMENT_SAMPLES: u64", IMPAIRMENT_SUPPRESSION_SAMPLES),
    ):
        found = re.search(rf"^const {re.escape(declaration)} = ([0-9.]+);", session_rs, re.M)
        if found is None:
            die(f"self-test: cannot read {declaration} out of clients/regolith/src/session.rs")
        if float(found.group(1)) != ours:
            die(
                f"self-test: {declaration} is {found.group(1)} in session.rs but {ours} here; "
                "the recomputation would refuse rows the client called clean"
            )
    for fragment in ("| fabs) > 2.0", "| fabs) > 40", "(200 / 20 / 60)"):
        if fragment not in ledger:
            die(
                f"self-test: p4-ledger.sh's append-time recomputation no longer carries "
                f"{fragment!r}; its band drifted from this one"
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

        # ── #973: the flag is recomputed within a band ──────────────────────
        #
        # A real link configured at 3.0% loss and 100 ms jitter does not measure
        # 3.0 and 100. Until #973 this row refused, and P4's "impairment
        # verified applied" criterion could pass only on a fixture that had
        # written observed == configured by construction.
        honest = fixture_row(
            SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11, observed=(2.94, 103, 96)
        )
        test.must_derive(cohort, [honest, row_b], "honest-measurement")
        test.ok("an_honest_measurement_off_its_configuration_derives")

        # And the property the flag exists to provide survives: a seat that
        # plainly never received its impairment is still refused.
        unapplied = fixture_row(
            SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11, observed=(0, 0, 0)
        )
        test.must_refuse(
            cohort,
            [unapplied, row_b],
            "impairment-not-applied",
            "is clear while its observed impairment sits outside the tolerance band",
        )
        test.ok("a_session_that_never_received_its_impairment_is_refused")

        # Half-applied, too: the loss arrived and the delay did not.
        no_jitter = fixture_row(
            SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11, observed=(2.94, 3, 4)
        )
        test.must_refuse(
            cohort,
            [no_jitter, row_b],
            "jitter-not-applied",
            "is clear while its observed impairment sits outside the tolerance band",
        )
        test.ok("a_session_whose_delay_was_never_applied_is_refused")

        # The other direction, signed rather than edited so it reaches the
        # recomputation: a flag fired over numbers that agree.
        faked = fixture_row(
            SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11, observed=(2.94, 103, 96), mismatch=True
        )
        test.must_refuse(
            cohort,
            [faked, row_b],
            "faked-mismatch",
            "fired while its observed impairment sits inside the tolerance band",
        )
        test.ok("a_flag_fired_inside_the_band_is_refused")

        # The band's own edge, on the side that must still be refused: 3.0
        # configured read as 0.9 is a 2.1 point gap, wider than the band.
        edge = fixture_row(
            SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11, observed=(0.9, 100, 100)
        )
        test.must_refuse(
            cohort,
            [edge, row_b],
            "band-edge",
            "is clear while its observed impairment sits outside the tolerance band",
        )
        test.ok("a_gap_wider_than_the_band_is_refused")

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

        # ── the shape the host actually emits, since #579 ───────────────────
        #
        # `SwarmReport.external` is a `Vec<ExteriorReport>` ordered by swarm
        # slot, and `ExteriorReport` carries no `session_id`. So the binding
        # falls to the QUIC-authenticated node the host admitted at each seat,
        # matched against the `measurement_node` the client signed — a signed
        # value on both sides, and unique per seat since #579. Reading the
        # singular object off a list is what the pre-#579 code did, and it
        # raised `AttributeError` rather than refusing.
        def host_shaped(**overrides: Any) -> dict[str, Any]:
            attempt = fixture_attempt([])
            del attempt["exteriors"]
            attempt["external"] = [
                {
                    "index": 4,
                    "node": node_a,
                    "connected_ticks": 55 * 60 * 30,
                    "said_goodbye": True,
                    "connected": False,
                    "uplink_frames": 100000,
                    "downlink_frames": 400000,
                    "downlink_dropped": 0,
                    "witness_anchored": False,
                },
                {
                    "index": 5,
                    "node": node_b,
                    "connected_ticks": 55 * 60 * 30,
                    "said_goodbye": False,
                    "connected": True,
                    "uplink_frames": 100000,
                    "downlink_frames": 400000,
                    "downlink_dropped": 0,
                    "witness_anchored": False,
                },
            ]
            attempt["per_link_impairment"] = fixture_links([4, 5], 4)
            attempt.update(overrides)
            return attempt

        manifest, emitted = test.must_derive(host_shaped(), [row_a, row_b], "host-array")
        humans = [report for report in emitted if report["identity"]["actor"] == "human"]
        by_slot = {report["binding"]["slot"]: report for report in humans}
        if sorted(by_slot) != [4, 5]:
            die("self-test [the_hosts_exterior_array_binds_by_admitted_node]: seats not bound")
        if by_slot[4]["binding"]["node"] != node_a or by_slot[5]["binding"]["node"] != node_b:
            die("self-test [the_hosts_exterior_array_binds_by_admitted_node]: bound to the wrong node")
        if by_slot[4]["binding"]["session_id"] != SESSION_A:
            die("self-test [the_hosts_exterior_array_binds_by_admitted_node]: wrong session on slot 4")
        if abs(manifest["attempt_total_hours"] - (4.0 + 50 / 60.0 + 42 / 60.0)) > 1e-9:
            die("self-test [the_hosts_exterior_array_binds_by_admitted_node]: total moved")
        # A seat still connected at report time closed *with* the attempt.
        if by_slot[5]["binding"]["close"] != "attempt_end":
            die("self-test [the_hosts_exterior_array_binds_by_admitted_node]: a live seat read as a disconnect")
        test.ok("the_hosts_exterior_array_binds_by_admitted_node")

        # `.external` must still be a list on the derived row: `p4-ledger.sh`
        # reads the admitted node out of it and requires it exactly once.
        if [entry["node"] for entry in by_slot[4]["external"]] != [node_a]:
            die("self-test [a_derived_row_carries_its_one_seat_as_a_list]: external is not this seat")
        test.ok("a_derived_row_carries_its_one_seat_as_a_list")

        # The node is the seat identity, so a node at two seats is ambiguous and
        # refused before any row is read.
        ambiguous = host_shaped()
        ambiguous["external"][1]["node"] = node_a
        test.must_refuse(ambiguous, [row_a, row_b], "node-twice", "seated at two slots")
        test.ok("a_node_seated_at_two_slots_is_refused")

        # ── the rejoin, and the ambiguity that survives it (#1028) ──────────
        #
        # A persistent identity key belongs to an install, not to a seat. A
        # volunteer who closes the client and launches it again inside one
        # attempt is readmitted under that same key, at the slot they held,
        # against a second pre-minted invite id — which is exactly what #1015's
        # eviction hold invites them to do. Two seats, two signed intervals, and
        # the projection onto `node` collides without being ambiguous: each seat
        # carries the id its row names.
        row_rejoin = fixture_row(SESSION_C, 5, "x86_64-unknown-linux-gnu", 0x11)
        rejoin = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55),
                fixture_exterior(5, SESSION_B, node_b, 50),
                fixture_exterior(4, SESSION_C, node_a, 10),
            ]
        )
        _, rejoined = test.must_derive(rejoin, [row_a, row_b, row_rejoin], "rejoin")
        legs = sorted(
            (report["binding"]["session_id"], report["binding"]["slot"], report["player_hours"])
            for report in rejoined
            if report["identity"]["actor"] == "human"
            and report["binding"]["node"] == node_a
        )
        if legs != sorted([(SESSION_A, 4, 50 / 60.0), (SESSION_C, 4, 5 / 60.0)]):
            die(f"self-test [a_rejoining_identity_binds_one_seat_per_interval]: legs {legs}")
        if abs(sum(r["player_hours"] for r in rejoined) - (4.0 + (50 + 42 + 5) / 60.0)) > 1e-9:
            die(
                "self-test [a_rejoining_identity_binds_one_seat_per_interval]: the rejoin moved "
                "the attempt total"
            )
        test.ok("a_rejoining_identity_binds_one_seat_per_interval")

        # The same key at two seats under **one** invite id. Nothing tells the
        # two apart, so a row naming that id is bound to nobody in particular —
        # #579's ambiguity, in the shape that is still one after #1028.
        one_id_twice = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55),
                fixture_exterior(5, SESSION_A, node_a, 50),
            ]
        )
        test.must_refuse(one_id_twice, [row_a], "one-id-twice", "seated at two slots")
        test.ok("one_session_at_two_seats_is_refused_even_under_one_node")

        # And one slot reported twice for one invite id is one seat reported
        # twice, whatever key held it.
        slot_twice = fixture_attempt(
            [
                fixture_exterior(4, SESSION_A, node_a, 55),
                fixture_exterior(4, SESSION_A, node_a, 50),
            ]
        )
        test.must_refuse(slot_twice, [row_a], "slot-twice", "reports slot 4 twice")
        test.ok("one_slot_reported_twice_for_one_session_is_refused")

        # A row signed by a key this attempt admitted nowhere.
        stranger = fixture_row(SESSION_C, 30, "x86_64-unknown-linux-gnu", 0x33)
        test.must_refuse(
            host_shaped(), [row_a, stranger], "stranger-node", "admitted at no seat"
        )
        test.ok("a_row_whose_node_the_host_never_admitted_is_refused")

        # And when the host *does* seat an id, it wins: a row landing on that
        # seat by node while naming a different id is #476's two-copy
        # disagreement, not a second way in.
        pinned = host_shaped()
        pinned["external"][0]["session_id"] = SESSION_C
        moved = fixture_row(SESSION_A, 50, "x86_64-unknown-linux-gnu", 0x11)
        test.must_refuse(pinned, [moved], "pinned-disagrees", "disagree")
        test.ok("a_seated_id_that_disagrees_with_the_rows_id_is_refused")

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
