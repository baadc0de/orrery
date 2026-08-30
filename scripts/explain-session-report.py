#!/usr/bin/env python3
"""Answer "who could see whom" for one campaign session (#711, #612, A16).

    ./scripts/explain-session-report.py --session <uuid> --around 7m
    ./scripts/explain-session-report.py --session <uuid> --seat 7 --around 420 --window 30
    ./scripts/explain-session-report.py --self-test

Run this on the admission box when a player reports something.  It joins the
two halves of the evidence that already exist and have never been read
together:

  * the player's uploaded session -- `client-records.jsonl` and the
    `telemetry.jsonl` the client actually wrote, under `sessions/<id>/`;
  * the host's `replica_scope_capture` journal, which names every directed
    seat pair with a reason code (#612).

"Someone was shooting me and I could not see them" is the most expensive
question this project has, and it has already been answered once from evidence
that turned out to be a logging-order artifact.  The point of this tool is that
the answer comes from the host's own directed decisions rather than from
anybody's recollection, and that a report becomes a lookup instead of an
investigation.

It reads.  It never writes to the service, the journal or the control file.
"""
import argparse
import collections
import json
import re
import subprocess
import sys
from pathlib import Path

TICK_HZ = 60

# `swarm.rs`: replica_scope_capture host_tick=.. subject_seat=.. receiver_seat=..
# in_scope=.. scope_reason=.. subject_cell=.. receiver_cell=..
CAPTURE = re.compile(
    r"replica_scope_capture host_tick=(?P<tick>\d+) subject_seat=(?P<subject>\d+) "
    r"receiver_seat=(?P<receiver>\d+) in_scope=(?P<in_scope>true|false) "
    r"scope_reason=(?P<reason>\S+) subject_cell=(?P<subject_cell>\d+) "
    r"receiver_cell=(?P<receiver_cell>\d+)")


def parse_around(value: str) -> float:
    """Seconds from a plain number or a `7m` / `90s` shorthand a player would say."""
    text = value.strip().lower()
    if text.endswith("m"):
        return float(text[:-1]) * 60.0
    if text.endswith("s"):
        return float(text[:-1])
    return float(text)


def captures_in_window(lines, first_tick: int, last_tick: int):
    """Every directed decision whose host tick falls inside the window."""
    for line in lines:
        match = CAPTURE.search(line)
        if not match:
            continue
        tick = int(match["tick"])
        if first_tick <= tick <= last_tick:
            yield {
                "tick": tick,
                "subject": int(match["subject"]),
                "receiver": int(match["receiver"]),
                "in_scope": match["in_scope"] == "true",
                "reason": match["reason"],
            }


def unseen_pairs(rows, seat: int | None):
    """Directed pairs that were out of scope, and why.

    Keyed subject -> receiver, because that is the direction the complaint
    takes: the *receiver* is the player who could not see the *subject*.
    """
    tally: dict[tuple[int, int], collections.Counter] = {}
    for row in rows:
        if row["in_scope"]:
            continue
        if seat is not None and row["receiver"] != seat:
            continue
        tally.setdefault((row["subject"], row["receiver"]), collections.Counter())[row["reason"]] += 1
    return tally


def journal_lines(unit: str, since: str, until: str) -> list[str]:
    out = subprocess.run(
        ["journalctl", "-u", unit, "--since", since, "--until", until, "--no-pager"],
        capture_output=True, text=True, check=False)
    return out.stdout.splitlines()


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--session")
    p.add_argument("--sessions-dir", type=Path, default=Path("/var/lib/orrery-admission/sessions"))
    p.add_argument("--unit", default="orrery-p1-swarm.service")
    p.add_argument("--seat", type=int, help="the complaining player's seat; omit for every seat")
    p.add_argument("--around", default="0", help="seconds into the attempt, or 7m / 90s")
    p.add_argument("--window", type=float, default=20.0, help="seconds either side")
    p.add_argument("--journal", type=Path, help="read captures from this file instead of journalctl")
    p.add_argument("--self-test", action="store_true")
    a = p.parse_args()
    if a.self_test:
        return self_test()
    if not a.session:
        p.error("--session is required")

    session_dir = a.sessions_dir / a.session
    records = session_dir / "client-records.jsonl"
    if records.exists():
        rows = [json.loads(line) for line in records.read_text().splitlines() if line.strip()]
        for row in rows:
            print(f"session {row.get('session_id', a.session)}: actor={row.get('actor')} "
                  f"client_rev={str(row.get('client_rev'))[:8]} "
                  f"banked={row.get('banked_minutes')} "
                  f"impairment_mismatch={row.get('impairment_mismatch')}")
    else:
        print(f"no uploaded record at {records} -- the player's own evidence never arrived; "
              f"the host journal below is all there is", file=sys.stderr)

    centre = parse_around(a.around)
    first_tick = int(max(0.0, centre - a.window) * TICK_HZ)
    last_tick = int((centre + a.window) * TICK_HZ)
    lines = (a.journal.read_text().splitlines() if a.journal
             else journal_lines(a.unit, "-24h", "now"))
    rows = list(captures_in_window(lines, first_tick, last_tick))
    print(f"\n{len(rows)} directed scope decisions in ticks {first_tick}..{last_tick} "
          f"({centre - a.window:.0f}s..{centre + a.window:.0f}s)")
    if not rows:
        print("nothing to report: either the window is wrong, or the host was not run "
              "with --replica-scope-capture")
        return 0

    tally = unseen_pairs(rows, a.seat)
    if not tally:
        print("every pair in this window was in scope: nobody was invisible to anybody")
        return 0
    print("\nout-of-scope pairs -- 'receiver could not see subject':")
    for (subject, receiver), reasons in sorted(tally.items()):
        detail = ", ".join(f"{reason} x{count}" for reason, count in reasons.most_common())
        print(f"  seat {receiver} could not see seat {subject}: {detail}")
    return 0


def self_test() -> int:
    assert parse_around("7m") == 420.0
    assert parse_around("90s") == 90.0
    assert parse_around("12") == 12.0
    line = ("Aug 30 09:00:00 host python3[1]: replica_scope_capture host_tick=1200 "
            "subject_seat=3 receiver_seat=7 in_scope=false scope_reason=out_of_interest "
            "subject_cell=11 receiver_cell=22")
    inside = list(captures_in_window([line], 1000, 1400))
    assert len(inside) == 1 and inside[0]["reason"] == "out_of_interest", inside
    # A window that excludes the tick must exclude the row.
    assert list(captures_in_window([line], 0, 100)) == []
    # Unrelated journal chatter is not a decision.
    assert list(captures_in_window(["Aug 30 09:00:00 host python3[1]: hello"], 0, 9999)) == []
    tally = unseen_pairs(inside, seat=7)
    assert tally == {(3, 7): collections.Counter({"out_of_interest": 1})}, tally
    # The complaint is directional: seat 3 did not fail to see seat 7 here.
    assert unseen_pairs(inside, seat=3) == {}
    # An in-scope decision is never a complaint.
    assert unseen_pairs([dict(inside[0], in_scope=True)], seat=7) == {}
    print("explain-session-report self-test OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
