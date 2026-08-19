#!/usr/bin/env python3
"""Correlate the intent path's FDB commit stage with the journal's own fsync.

Both fsync to the same md2 QLC RAID1, and this box has two fsync-cost regimes
that differ by about 2x and switch on a tens-of-seconds scale (docs/08 §4.3).
That is a confound for every P2 latency series *and* the only clean way to test
whether the intent tail is the device: the same configuration run in the two
regimes should differ in the intent's `commit` stage and in the journal's
`sync_data` **together**.

Per point it prints the journal's per-flush fsync cost (`JournalStageSnapshot`
— **per flush**, never per record; dividing by records understates it ~30x)
beside the intent path's commit stage over the tail. One line per point, so the
pairing is visible rather than argued.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def journal(d: Path):
    """Summed journal stage deltas over the run."""
    acc = {}
    p = d / "primary-metrics.jsonl"
    if not p.exists():
        return acc
    for line in p.read_text().splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "journal_stage_delta":
            continue
        for k, v in rec.items():
            if k == "type" or not isinstance(v, (int, float)):
                continue
            if k.endswith("_max"):
                acc[k] = max(acc.get(k, 0), v)
            else:
                acc[k] = acc.get(k, 0) + v
    return acc


def intent(d: Path):
    stage = {"all": {}, "slow": {}}
    p = d / "primary-boundary.jsonl"
    if not p.exists():
        return stage
    for line in p.read_text().splitlines():
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
    return stage


if __name__ == "__main__":
    print(
        f"{'point':<16} {'flushes':>8} {'sync/flush':>11} {'sync max':>9} | "
        f"{'intents':>8} {'slow%':>6} {'commit all':>11} {'commit tail':>12} "
        f"{'commit max':>11} {'grv tail':>9}"
    )
    for arg in sorted(sys.argv[1:]):
        d = Path(arg)
        j, s = journal(d), intent(d)
        a, t = s["all"], s["slow"]
        n = a.get("intents", 0)
        if not n:
            continue
        f = j.get("flushes", 0) or 1
        ex = a.get("executed", 0) or 1
        tx = t.get("executed", 0) or 1
        sync_key = next(
            (k for k in j if k.startswith("sync_data") and k.endswith("_us_sum")), None
        )
        sync_max = next(
            (k for k in j if k.startswith("sync_data") and k.endswith("_us_max")), None
        )
        print(
            f"{d.name:<16} {j.get('flushes',0):>8} "
            f"{(j.get(sync_key,0)/f/1000 if sync_key else 0):>10.2f}m "
            f"{(j.get(sync_max,0)/1000 if sync_max else 0):>8.1f}m | "
            f"{n:>8} {100*t.get('intents',0)/n:>5.1f}% "
            f"{a.get('commit_us_sum',0)/ex/1000:>10.2f}m "
            f"{t.get('commit_us_sum',0)/tx/1000:>11.2f}m "
            f"{a.get('commit_us_max',0)/1000:>10.1f}m "
            f"{t.get('grv_us_sum',0)/tx/1000:>8.2f}m"
        )
