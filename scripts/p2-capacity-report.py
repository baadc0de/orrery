#!/usr/bin/env python3
"""Reduce one or more `p2-capacity-sweep.sh` output directories to one row each.

Every number here is read out of a file the harness already wrote; nothing is
re-derived from a rate. Two conventions matter and have misled readers before:

* `journal_stage_delta` samples ONCE PER FLUSH, so a per-flush cost is
  `<stage>_us_sum / flushes`. Dividing by `records` understates it by the batch
  size (~30-60x here).
* per-process CPU is `%CPU` from pidstat, which is a percentage of ONE core on
  a 16-thread box: 1600 is the ceiling, and `cores = %CPU / 100`.

The first `--skip` seconds of the pidstat series are dropped because the rig's
lease-claim phase runs there and is not part of the steady-state load.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

RUN_COMPLETE = re.compile(
    r"run complete .*?diffs=(\d+).*?acks=(\d+).*?durable_acks=(\d+).*?"
    r"duplicate_durable_acks=(\d+).*?provisional_acks=(\d+).*?diff_nacks=(\d+).*?"
    r"leases=(\d+).*?leases_lost=(\d+).*?intents=(\d+).*?intent_acks=(\d+).*?"
    r"bulk_p99_us=(\d+).*?max_rx_backlog=(\d+).*?intent_p99_us=(\d+)"
)
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def run_complete(path: pathlib.Path) -> dict:
    text = ANSI.sub("", path.read_text(errors="replace"))
    m = RUN_COMPLETE.search(text)
    if not m:
        return {}
    keys = [
        "diffs", "acks", "durable_acks", "duplicate_durable_acks", "provisional_acks",
        "diff_nacks", "leases", "leases_lost", "intents", "intent_acks",
        "bulk_p99_us", "max_rx_backlog", "intent_p99_us",
    ]
    return dict(zip(keys, (int(v) for v in m.groups())))


def claim_secs(path: pathlib.Path) -> float | None:
    text = ANSI.sub("", path.read_text(errors="replace"))
    m = re.search(r"claim phase complete .*?claim_secs=([0-9.]+)", text)
    return float(m.group(1)) if m else None


def entities(path: pathlib.Path) -> int | None:
    text = ANSI.sub("", path.read_text(errors="replace"))
    m = re.search(r"claiming leases .*?entities=(\d+)", text)
    return int(m.group(1)) if m else None


def jsonl(path: pathlib.Path):
    if not path.exists():
        return
    for line in path.read_text(errors="replace").splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def journal(path: pathlib.Path) -> dict:
    tot = {}
    n = 0
    for rec in jsonl(path):
        if rec.get("type") != "journal_stage_delta":
            continue
        n += 1
        for k, v in rec.items():
            if k == "type":
                continue
            if k.endswith("_max"):
                tot[k] = max(tot.get(k, 0), v)
            else:
                tot[k] = tot.get(k, 0) + v
    tot["intervals"] = n
    return tot


def bulk_stage(path: pathlib.Path) -> dict:
    tot = {}
    for rec in jsonl(path):
        if rec.get("type") != "gateway_bulk_stage_delta":
            continue
        for k, v in rec.items():
            if k == "type":
                continue
            tot[k] = max(tot.get(k, 0), v) if k.endswith("_max") else tot.get(k, 0) + v
    return tot


def last_of_kind(path: pathlib.Path, kind: str) -> dict:
    out = {}
    for rec in jsonl(path):
        if rec.get("type") == kind:
            out = rec
    return out


def route_stage(path: pathlib.Path) -> dict:
    tot = {}
    for rec in jsonl(path):
        if rec.get("type") != "gateway_route_stage":
            continue
        for k, v in rec.items():
            if k == "type":
                continue
            tot[k] = max(tot.get(k, 0), v) if k.endswith("_max") else tot.get(k, 0) + v
    return tot


def cpu(path: pathlib.Path, skip: int) -> dict:
    """Per-pid CPU series from pidstat -u -h. Returns cores mean/peak per command."""
    if not path.exists():
        return {}
    series: dict[tuple[str, str], list[float]] = {}
    order: list[str] = []
    for line in path.read_text(errors="replace").splitlines():
        f = line.split()
        # `pidstat -u -h` columns: <hh:mm:ss> <AM|PM> UID PID %usr %system
        # %guest %wait %CPU CPU Command. The clock is two tokens, which is why
        # the PID is f[3] and not f[2] (f[2] is the UID, and taking it merged
        # the two persistd processes into one series).
        if len(f) < 11 or f[0].startswith("#") or not f[3].isdigit():
            continue
        pid, pct, comm = f[3], float(f[8]), f[10]
        key = (comm, pid)
        if key not in series:
            series[key] = []
            order.append(key)
        series[key].append(pct)
    out = {}
    # Two persistd processes: the second to appear under load is the primary
    # (the follower is started first, so it has the lower pid).
    persistd = sorted((k for k in series if k[0] == "persistd"), key=lambda k: int(k[1]))
    names = {}
    for i, k in enumerate(persistd):
        names[k] = "persistd_follower" if i == 0 else "persistd_primary"
    for k in series:
        if k not in names:
            names[k] = k[0].replace("-", "_")
    for k, vals in series.items():
        v = vals[skip:] or vals
        out[names[k] + "_cores_mean"] = round(sum(v) / len(v) / 100, 3)
        out[names[k] + "_cores_peak"] = round(max(v) / 100, 3)
    return out


def threads(path: pathlib.Path, skip: int, top: int = 4) -> dict:
    """Busiest threads of the primary, from `pidstat -t -u -h`.

    Columns are <hh:mm:ss> <AM|PM> UID TGID TID %usr %system %guest %wait %CPU
    CPU Command; process rows carry a `-` in TID and are skipped, so what is
    summarised here is per-thread. A single thread parked at ~100 with the
    box's other fifteen idle is a different diagnosis from the same total
    spread evenly, and the process-level number cannot tell them apart.
    """
    if not path.exists():
        return {}
    series: dict[str, list[float]] = {}
    names: dict[str, str] = {}
    for line in path.read_text(errors="replace").splitlines():
        f = line.split()
        if len(f) < 12 or f[0].startswith("#") or f[4] == "-" or not f[4].isdigit():
            continue
        tid, pct, comm = f[4], float(f[9]), f[11]
        series.setdefault(tid, []).append(pct)
        names[tid] = comm.lstrip("|_")
    out = {}
    ranked = sorted(
        ((sum(v[skip:] or v) / len(v[skip:] or v), tid) for tid, v in series.items()),
        reverse=True,
    )
    out["thread_cores_top"] = [
        f"{names[t]}:{m / 100:.2f}" for m, t in ranked[:top] if m > 1.0
    ]
    if ranked:
        out["busiest_thread_pct_mean"] = round(ranked[0][0], 1)
        out["busiest_thread_pct_peak"] = round(max(series[ranked[0][1]][skip:] or [0]), 1)
    return out


def pressure(before: pathlib.Path, after: pathlib.Path, field: str) -> float | None:
    def total(p):
        if not p.exists():
            return None
        for line in p.read_text().splitlines():
            if line.startswith(field):
                m = re.search(r"total=(\d+)", line)
                return int(m.group(1)) if m else None
        return None

    a, b = total(after), total(before)
    return None if a is None or b is None else (a - b) / 1e6  # seconds stalled


def row(d: pathlib.Path, skip: int) -> dict:
    point = json.loads((d / "point.json").read_text()) if (d / "point.json").exists() else {}
    r = {"label": d.name, **point}
    r.update(run_complete(d / "load.stderr"))
    r["claim_secs"] = claim_secs(d / "load.stderr")
    dur = r.get("duration_secs") or 30
    if "durable_acks" in r:
        r["durable_acks_per_s"] = round(r["durable_acks"] / dur)
        r["offered_diffs_per_s"] = round(r["diffs"] / dur)
    j = journal(d / "primary-metrics.jsonl")
    if j.get("flushes"):
        r["flushes_per_s"] = round(j["flushes"] / max(j["intervals"], 1))
        r["records_per_s"] = round(j["records"] / max(j["intervals"], 1))
        r["journal_mb_per_s"] = round(j["bytes"] / max(j["intervals"], 1) / 1e6, 2)
        r["records_per_flush"] = round(j["records"] / j["flushes"], 1)
        r["sync_ms_per_flush"] = round(j["sync_data_us_sum"] / j["flushes"] / 1000, 3)
        r["sync_ms_max"] = round(j["sync_data_us_max"] / 1000, 2)
        r["queue_wait_ms_per_flush"] = round(j["queue_wait_us_sum"] / j["flushes"] / 1000, 3)
        r["queue_wait_ms_max"] = round(j["queue_wait_us_max"] / 1000, 2)
    b = bulk_stage(d / "primary-metrics.jsonl")
    if b.get("acknowledgements"):
        n = b["acknowledgements"]
        for stage in ("route_queue", "router_apply", "journal_wait", "reply", "total"):
            r[f"{stage}_ms_mean"] = round(b[f"{stage}_us_sum"] / n / 1000, 3)
        r["total_ms_max"] = round(b["total_us_max"] / 1000, 2)
    # The offered load is `entities x diff_hz` — what the world would generate
    # if nothing throttled it — and NOT `diffs_sent`, which the rig's uplink
    # scheduler caps and which re-counts a shed diff when the client re-offers
    # it. The delivery ratio below is against the former.
    ents = entities(d / "load.stderr")
    if ents and r.get("diff_hz"):
        r["entities"] = ents
        r["offered_per_s"] = round(ents * float(r["diff_hz"]))
        if "durable_acks_per_s" in r:
            r["delivered_ratio"] = round(r["durable_acks_per_s"] / r["offered_per_s"], 3)
    ing = last_of_kind(d / "primary-boundary.jsonl", "gateway_ingress")
    for k in ("admitted", "shed_saturated", "shed_stale", "shed_slow_route"):
        if k in ing:
            r[k] = ing[k]
    if ing.get("admitted"):
        r["shed_pct"] = round(100 * ing.get("shed_slow_route", 0) / ing["admitted"], 2)
    lease = last_of_kind(d / "primary-boundary.jsonl", "gateway_lease")
    for k in ("queued", "refused"):
        if k in lease:
            r["lease_" + k] = lease[k]
    rs = route_stage(d / "primary-boundary.jsonl")
    if rs.get("applies"):
        r["applies"] = rs["applies"]
        r["applies_per_s"] = round(rs["applies"] / dur)
        # Work done and then thrown away: every apply pays a lease locate and a
        # journal turn, and the 25 ms route valve can still shed the reply.
        if "durable_acks" in r:
            r["wasted_applies_pct"] = round(
                100 * max(rs["applies"] - r["durable_acks"], 0) / rs["applies"], 1
            )
        r["locate_ms_per_apply"] = round(rs["locate_us_sum"] / rs["applies"] / 1000, 3)
        r["gate_wait_ms_per_apply"] = round(rs["gate_wait_us_sum"] / rs["applies"] / 1000, 3)
        r["mailbox_ms_per_apply"] = round(rs["mailbox_us_sum"] / rs["applies"] / 1000, 3)
    r.update(cpu(d / "pidstat.txt", skip))
    r.update(threads(d / "pidstat-threads.txt", skip))
    r["cpu_stall_s"] = pressure(d / "cpu-pressure-before.txt", d / "cpu-pressure-after.txt", "some")
    r["io_stall_full_s"] = pressure(d / "io-pressure-before.txt", d / "io-pressure-after.txt", "full")
    return r


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("dirs", nargs="+", type=pathlib.Path)
    ap.add_argument("--skip", type=int, default=5, help="pidstat samples to drop (claim phase)")
    ap.add_argument("--fields", help="comma-separated field subset for the table")
    args = ap.parse_args()
    rows = [row(d, args.skip) for d in args.dirs if d.is_dir()]
    if args.fields:
        keys = args.fields.split(",")
        print("\t".join(keys))
        for r in rows:
            print("\t".join(str(r.get(k, "")) for k in keys))
    else:
        json.dump(rows, sys.stdout, indent=1, sort_keys=True)
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
