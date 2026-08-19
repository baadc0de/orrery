#!/usr/bin/env python3
"""Extract one p2-kill9 gate run's numbers into a small JSON summary.

The gate's own output directory is ~1 GB of JSONL. This reads it once, writes
a few hundred bytes, and lets the caller delete the directory immediately --
which is what makes an n-run baseline fit on this box at all.

Regime classification is the same rule `scripts/intent-tail-derive.py` uses
(docs/08 §2.2.1): slow iff the journal's worst `sync_data` in the run is
>= 150 ms. It is repeated here rather than imported because that script reads
the intent-tail sweep's directory layout, not the gate's.

Usage: p2-baseline-extract.py <gate-out-dir> <label> [> summary.json]
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

SLOW_REGIME_SYNC_MS = 150.0
GATED = ("journal_commit_ms", "bulk_ack_ms", "intent_commit_ms", "area_first_page_ms")


def jsonl(path: pathlib.Path):
    if not path.exists():
        return
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def footer(path: pathlib.Path) -> dict:
    """The rig's `run complete` line: the client's own end-of-run counters."""
    last = None
    if path.exists():
        for line in path.read_text(errors="replace").splitlines():
            if "run complete" in line:
                last = line
    if last is None:
        return {}
    clean = re.sub(r"\x1b\[[0-9;]*m", "", last)
    return {k: float(v) for k, v in re.findall(r"(\w+)=([0-9.]+)", clean)}


def main(argv: list[str]) -> int:
    out = pathlib.Path(argv[1])
    label = argv[2]

    summary: dict = {"label": label, "dir": str(out)}

    # -- the four gated series, as the dashboard scored them ---------------
    report_path = out / "latency-report.json"
    if report_path.exists():
        report = json.loads(report_path.read_text())
        summary["gate"] = report.get("gate")
        summary["unknown_series"] = report.get("unknown_series", 0)
        series = {}
        for name in GATED:
            s = (report.get("series") or {}).get(name)
            if s is None:
                series[name] = None
                continue
            series[name] = {
                "n": s.get("n"),
                "p50_us": s.get("p50_us"),
                "p99_us": s.get("p99_us"),
                "max_us": s.get("max_us"),
                "threshold_us": s.get("threshold_us"),
                "gate": s.get("gate"),
                "failure_role": s.get("failure_role"),
            }
        summary["series"] = series
        summary["root_causes"] = report.get("root_causes", [])
        summary["run"] = report.get("run")
    else:
        summary["gate"] = "no-report"

    # -- the proof the gate exists to produce ------------------------------
    ver_path = out / "recovery-verification.json"
    if ver_path.exists():
        v = json.loads(ver_path.read_text())
        summary["recovery"] = {
            k: v.get(k)
            for k in ("pass", "checked", "verified", "missing_bulk", "missing_intent",
                      "mismatched", "skipped_after_cutoff", "entities", "acks")
            if k in v
        }
        summary["recovery"]["pass"] = v.get("pass")
    artifact = out / "artifact.json"
    summary["artifact_written"] = artifact.exists()

    # -- durable acknowledgements, counted the way the gate counts them ----
    acks = out / "acks.jsonl"
    if acks.exists():
        n = 0
        with acks.open(encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if '"type":"diff"' in line:
                    n += 1
        summary["durable_acks"] = n

    # -- the client's footer: leases lost, intents, arrival percentiles ----
    summary["client"] = footer(out / "load-before.stderr")

    # -- the device: journal fsync, which is what splits the regimes -------
    journal: dict = {}
    for rec in jsonl(out / "primary-metrics.jsonl"):
        if rec.get("type") != "journal_stage_delta":
            continue
        for k, val in rec.items():
            if k == "type" or not isinstance(val, (int, float)):
                continue
            if k.endswith("_max"):
                journal[k] = max(journal.get(k, 0), val)
            else:
                journal[k] = journal.get(k, 0) + val
    sync_max_ms = journal.get("sync_data_us_max", 0) / 1000
    summary["journal_sync_max_ms"] = sync_max_ms
    summary["regime"] = "slow" if sync_max_ms >= SLOW_REGIME_SYNC_MS else "fast"

    # -- the gateway's intent stages: GRV and commit, all and tail ---------
    stage_all: dict = {}
    stage_slow: dict = {}
    for rec in jsonl(out / "primary-boundary.jsonl"):
        if rec.get("type") != "gateway_intent_stage":
            continue
        acc = stage_slow if rec.get("scope") == "slow" else stage_all
        for k, val in rec.items():
            if k in ("type", "scope") or not isinstance(val, (int, float)):
                continue
            if k.endswith("_max") or k == "fence_read_max_us":
                acc[k] = max(acc.get(k, 0), val)
            else:
                acc[k] = acc.get(k, 0) + val
    executed = max(stage_all.get("executed", 0), 1)
    slow_executed = max(stage_slow.get("executed", 0), 1)
    summary["stage"] = {
        "intents": stage_all.get("intents", 0),
        "executed": stage_all.get("executed", 0),
        "slow_intents": stage_slow.get("intents", 0),
        "grv_total_s": stage_all.get("grv_us_sum", 0) / 1e6,
        "grv_mean_ms": stage_all.get("grv_us_sum", 0) / executed / 1000,
        "tail_grv_mean_ms": stage_slow.get("grv_us_sum", 0) / slow_executed / 1000,
        "tail_commit_mean_ms": stage_slow.get("commit_us_sum", 0) / slow_executed / 1000,
        "commit_max_ms": stage_all.get("commit_us_max", 0) / 1000,
        "server_max_ms": stage_all.get("server_us_max", 0) / 1000,
    }

    json.dump(summary, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
