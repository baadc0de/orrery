#!/usr/bin/env python3
"""Is the intent tail spread over the run, or does it arrive on a cadence?

Reads a point's ``primary-boundary.jsonl`` and prints, one line per report
interval (250 ms), the interval's slowest intent stage by stage beside the
lease-batch work the router did in that same interval. A tail that is *load*
is smeared across intervals; a tail that is a periodic burst is not, and the
spacing of the spikes names the period.

The lease columns come from the ``gateway_route_stage`` record, which counts
`heartbeat_leases`' own batched gate acquisitions and their
`LeaseStore::locate` reads. They are printed beside the intent because
"periodic at 3 s" only becomes a mechanism once the thing that is periodic at
3 s is in the same table.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

INTERVAL_MS = 250


def rows(d: Path):
    """Interval-aligned records. The reporter writes at most one of each kind
    per interval, in a fixed order, so a new intent exemplar closes a row."""
    out, cur = [], {}
    for line in (d / "primary-boundary.jsonl").read_text().splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = rec.get("type")
        if kind == "gateway_route_stage":
            if "route" in cur:
                out.append(cur)
                cur = {}
            cur["route"] = rec
        elif kind == "gateway_intent_exemplar":
            cur["intent"] = rec
            out.append(cur)
            cur = {}
    if cur:
        out.append(cur)
    return out


def main(d: Path, threshold_ms: float = 40.0):
    data = rows(d)
    print(f"=== {d.name}: {len(data)} report intervals of {INTERVAL_MS} ms")
    print(
        f"{'i':>4} {'t(s)':>6} {'srv':>7} {'grv':>7} {'fence':>6} {'commit':>6} | "
        f"{'batch_locks':>11} {'locate_max':>10} {'batch_hold_max':>14}"
    )
    spikes = []
    for i, row in enumerate(data):
        it = row.get("intent") or {}
        rt = row.get("route") or {}
        srv = it.get("server_us", 0) / 1000
        if srv >= threshold_ms:
            spikes.append(i)
        print(
            f"{i:>4} {i*INTERVAL_MS/1000:>6.2f} {srv:>7.1f} "
            f"{it.get('grv_us',0)/1000:>7.1f} {it.get('fence_us',0)/1000:>6.1f} "
            f"{it.get('commit_us',0)/1000:>6.1f} | "
            f"{rt.get('batch_locks',0):>11} "
            f"{rt.get('locate_us_max',0)/1000:>10.1f} "
            f"{rt.get('batch_hold_us_max',0)/1000:>14.1f}"
            + ("   <<<" if srv >= threshold_ms else "")
        )
    if len(spikes) > 1:
        gaps = [b - a for a, b in zip(spikes, spikes[1:])]
        print(
            f"\nintervals above {threshold_ms} ms: {len(spikes)}; "
            f"gaps (intervals): {gaps}"
        )
        print(
            f"gaps (ms): {[g*INTERVAL_MS for g in gaps]}  "
            f"median period {sorted(gaps)[len(gaps)//2]*INTERVAL_MS} ms"
        )


if __name__ == "__main__":
    thresh = float(sys.argv[2]) if len(sys.argv) > 2 else 40.0
    main(Path(sys.argv[1]), thresh)
