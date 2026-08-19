#!/usr/bin/env python3
"""One row per point: offered load, the gated percentiles, and the tail's own
stage decomposition side by side.

Companion to ``intent-stage-report.py`` (which prints one point in full). The
columns that matter are the ``slow`` ones: they are means over only those
intents whose server span exceeded the slow cut, i.e. the decomposition **of
the tail**, which is the thing the p99 is made of.
"""

from __future__ import annotations

import sys
from pathlib import Path

import json
import re

BOUNDARIES = [
    50, 100, 150, 200, 300, 400, 500, 750, 1_000, 1_500, 2_000, 3_000, 4_000,
    5_000, 7_500, 10_000, 15_000, 20_000, 30_000, 40_000, 50_000, 75_000,
    100_000, 150_000, 200_000, 300_000, 400_000, 500_000, 750_000, 1_000_000,
    1_500_000, 2_000_000, 3_000_000, 4_000_000, 5_000_000, 7_500_000,
    10_000_000,
]


def pct(samples, q):
    total = sum(c for _, c in samples)
    if not total:
        return None
    seen = 0
    for value, count in sorted(samples):
        seen += count
        if seen >= q * total:
            return value
    return None


def load(d: Path):
    point = json.loads((d / "point.json").read_text().splitlines()[0])
    stage = {"all": {}, "slow": {}}
    for line in (d / "primary-boundary.jsonl").read_text().splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "gateway_intent_stage":
            continue
        acc = stage.setdefault(rec.get("scope", "all"), {})
        for k, v in rec.items():
            if k in ("type", "scope"):
                continue
            if k.endswith("_max") or k == "fence_read_max_us":
                acc[k] = max(acc.get(k, 0), v)
            else:
                acc[k] = acc.get(k, 0) + v
    client = {}
    lj = d / "load.jsonl"
    if lj.exists():
        for line in lj.read_text().splitlines():
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if rec.get("type") == "sample_batch":
                client.setdefault(rec["series"], []).append((rec["value_us"], rec["count"]))
    text = re.sub(r"\x1b\[[0-9;]*m", "", (d / "load.stderr").read_text())
    footer = {}
    for key in ("diffs", "intents", "intent_arrival_p50_us", "intent_arrival_p99_us"):
        m = re.findall(rf"\b{key}=(\d+)", text)
        if m:
            footer[key] = int(m[-1])
    return point, stage, client, footer


def ms(v):
    return "-" if v is None else f"{v/1000:.1f}"


HEADER = (
    f"{'point':<16} {'diffs/s':>8} {'int/s':>6} {'n':>6} "
    f"{'p50':>6} {'p99':>7} {'slow%':>6} | "
    f"{'TAIL srv':>8} {'grv':>7} {'idem':>6} {'fence':>6} {'commit':>7} "
    f"{'fdbgap':>7} {'spawn':>6} {'srvgap':>7} {'retries':>7}"
)


def row(d: Path):
    point, stage, client, footer = load(d)
    dur = point.get("duration_secs", 30)
    a, s = stage.get("all", {}), stage.get("slow", {})
    n = a.get("intents", 0)
    if not n:
        return None
    sn = s.get("intents", 0) or 1
    sx = s.get("executed", 0) or 1
    cc = client.get("intent_commit_ms", [])
    return (
        f"{d.name:<16} {footer.get('diffs',0)/dur:>8.0f} "
        f"{footer.get('intents',0)/dur:>6.1f} {n:>6} "
        f"{ms(pct(cc,0.5)):>6} {ms(pct(cc,0.99)):>7} "
        f"{100*s.get('intents',0)/n:>5.1f}% | "
        f"{ms(s.get('server_us_sum',0)//sn):>8} "
        f"{ms(s.get('grv_us_sum',0)//sx):>7} "
        f"{ms(s.get('idem_read_us_sum',0)//sx):>6} "
        f"{ms(s.get('fence_us_sum',0)//sx):>6} "
        f"{ms(s.get('commit_us_sum',0)//sx):>7} "
        f"{ms(s.get('fdb_gap_us_sum',0)//sx):>7} "
        f"{ms(s.get('spawn_wait_us_sum',0)//sn):>6} "
        f"{ms(s.get('server_gap_us_sum',0)//sn):>7} "
        f"{a.get('attempts',0)-a.get('executed',0):>7}"
    )


if __name__ == "__main__":
    print("all times in ms; TAIL columns are means over intents past the slow cut")
    print(HEADER)
    for arg in sorted(sys.argv[1:]):
        try:
            r = row(Path(arg))
        except (FileNotFoundError, IndexError):
            continue
        if r:
            print(r)
