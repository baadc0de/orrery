#!/usr/bin/env python3
"""Server-side stage decomposition per run, folded across a run's intervals.

`p2-capacity-report.py` reports the client's view; this reports the boundary's
own `gateway_bulk_stage_delta`, `gateway_intent` and `journal_stage_delta`,
which is where "what binds now" is visible.
"""
from __future__ import annotations

import json
import pathlib
import sys

KINDS = ("gateway_intent", "journal_stage_delta", "gateway_bulk_stage_delta")


def totals(path: pathlib.Path) -> dict:
    tot: dict[str, dict] = {}
    if not path.exists():
        return tot
    for line in path.read_text(errors="replace").splitlines():
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        if r.get("type") not in KINDS:
            continue
        t = tot.setdefault(r["type"], {})
        for k, v in r.items():
            if k == "type":
                continue
            t[k] = max(t.get(k, 0), v) if k.endswith("_max") else t.get(k, 0) + v
    return tot


HEAD = [
    "run",
    "bulk_total_ms",
    "route_queue_ms",
    "router_apply_ms",
    "journal_wait_ms",
    "reply_ms",
    "bulk_total_max_ms",
    "intent_mean_ms",
    "intent_max_ms",
    "rec_per_flush",
    "flush_queue_ms",
    "flush_sync_ms",
    "flush_sync_max_ms",
]


def main() -> int:
    print("\t".join(HEAD))
    for arg in sys.argv[1:]:
        d = pathlib.Path(arg)
        t = totals(d / "primary-metrics.jsonl")
        gb, js, gi = (t.get(k, {}) for k in KINDS[::-1])
        a = gb.get("acknowledgements") or 1
        n = gi.get("replies") or 1
        f = js.get("flushes") or 1
        print(
            "\t".join(
                str(x)
                for x in [
                    d.name,
                    round(gb.get("total_us_sum", 0) / a / 1000, 3),
                    round(gb.get("route_queue_us_sum", 0) / a / 1000, 4),
                    round(gb.get("router_apply_us_sum", 0) / a / 1000, 4),
                    round(gb.get("journal_wait_us_sum", 0) / a / 1000, 3),
                    round(gb.get("reply_us_sum", 0) / a / 1000, 4),
                    round(gb.get("total_us_max", 0) / 1000),
                    round(gi.get("server_us_sum", 0) / n / 1000, 1),
                    round(gi.get("server_us_max", 0) / 1000, 1),
                    round(js.get("records", 0) / f, 1),
                    round(js.get("queue_wait_us_sum", 0) / f / 1000, 3),
                    round(js.get("sync_data_us_sum", 0) / f / 1000, 3),
                    round(js.get("sync_data_us_max", 0) / 1000),
                ]
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
