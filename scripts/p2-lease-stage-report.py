#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §2.2.6, derived from its data file.

The section answers one question: after §2.2.3-§2.2.5 took the renewal path
apart below the `Router` boundary, what does a heartbeat cost *above* it? The
data is the `gateway_lease_stage` records one P2 kill-9 gate run's primary
emitted -- 30 report intervals over a 30 s run.

    python3 scripts/p2-lease-stage-report.py             # the section's numbers
    python3 scripts/p2-lease-stage-report.py --self-test # hold them to the file
"""
import json
import pathlib
import sys

DATA = pathlib.Path(__file__).resolve().parents[1] / "docs/data/p2-lease-stages-2026-08-19.jsonl"

# (key, label) in the order the served span spends them.
STAGES = [
    ("session", "peer-state lock"),
    ("resolve", "resolve vs session table"),
    ("route", "router call"),
    ("recheck", "second lock"),
    ("encode", "ack encode + send"),
    ("gap", "unattributed"),
]


def load():
    """Fold the interval records into one total.

    Sums add; maxima take a max, because the reporter carries a run-to-date
    maximum through each interval rather than an interval maximum (an interval
    max is not recoverable from two cumulative ones).
    """
    rows = [json.loads(line) for line in DATA.read_text().splitlines() if line.strip()]
    total = {}
    for row in rows:
        for key, value in row.items():
            if key == "type":
                continue
            total[key] = max(total.get(key, 0), value) if key.endswith("_max") else total.get(key, 0) + value
    return rows, total


def report():
    rows, t = load()
    hb, rn, span = t["heartbeats"], t["renewals"], t["heartbeat_us_sum"]
    print(f"docs/08-persistence.md §2.2.6 — {DATA.name}")
    print(
        f"  {len(rows)} report intervals, {hb} heartbeats, {rn} renewals "
        f"(mean batch {rn / hb:.1f}, widest {t['entries_max']})\n"
    )
    print(f"  {'stage':26} {'per heartbeat':>14} {'per renewal':>13} {'share':>7} {'max':>10}")
    for key, label in STAGES:
        s, m = t[f"{key}_us_sum"], t[f"{key}_us_max"]
        print(
            f"    {label:24} {s / hb:11.1f} us {s / rn:10.2f} us "
            f"{100 * s / span:6.1f}% {m / 1000:8.2f} ms"
        )
    print(
        f"    {'served span':24} {span / hb:11.1f} us {span / rn:10.2f} us "
        f"{100.0:6.1f}% {t['heartbeat_us_max'] / 1000:8.2f} ms"
    )
    above = sum(t[f"{k}_us_sum"] for k, _ in STAGES if k != "route")
    print()
    print(f"  above the router   {100 * above / span:.1f}% of the span")
    print(f"  whole path         {span / 1e6:.3f} s over a 30 s run = {100 * span / 1e6 / 30:.3f}% of one core")
    claimed = sum(t[f"{k}_us_sum"] for k, _ in STAGES if k != "gap")
    print(f"  identity           stages {claimed} + gap {t['gap_us_sum']} = {claimed + t['gap_us_sum']} vs span {span}")


def self_test():
    rows, t = load()
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    hb, rn, span = t["heartbeats"], t["renewals"], t["heartbeat_us_sum"]
    check("intervals", len(rows) == 30, f"{len(rows)}")
    check("denominators differ", rn > hb, f"heartbeats {hb}, renewals {rn}")
    check("batch width", abs(rn / hb - 80.0) < 0.5, f"{rn / hb:.2f}")
    # The identity the instrument rests on: no stage overlaps another and
    # nothing hides between two of them.
    claimed = sum(t[f"{k}_us_sum"] for k, _ in STAGES if k != "gap")
    check("identity exact", claimed + t["gap_us_sum"] == span, f"{claimed + t['gap_us_sum']} vs {span}")
    # The section's conclusion, and the thing an edit could invert.
    route_share = 100 * t["route_us_sum"] / span
    check("router dominates", route_share > 90, f"router is {route_share:.1f}% of the span")
    above = sum(t[f"{k}_us_sum"] for k, _ in STAGES if k != "route")
    check("above-router is small", 100 * above / span < 10, f"{100 * above / span:.1f}%")
    # Both peer-state lock acquisitions were free; that is a finding, not noise.
    check("locks are free", t["session_us_sum"] + t["recheck_us_sum"] < span // 100,
          f"locks {t['session_us_sum'] + t['recheck_us_sum']} us of {span} us")
    check("path is ~1% of a core", 0.5 < 100 * span / 1e6 / 30 < 2.0, f"{100 * span / 1e6 / 30:.3f}%")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(rows)} intervals, every §2.2.6 claim holds against {DATA.name}")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else (report() or 0))
