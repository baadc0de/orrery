#!/usr/bin/env python3
"""Compare before/after points of the fdb-off-bulk-path capacity study.

Adds three things `p2-capacity-report.py` cannot know about: the FDB client
thread named specifically (rather than "the busiest thread", which after the
change is no longer it), the new `gateway_route_stage` counters, and a
before/after fold that reports min-max across repeats — never a median,
because this box swings by up to 2x on per-flush fsync cost.

**`nominal_per_s` is not load that arrived.** It is `entities × diff_hz` —
what the world would generate if nothing throttled it — and the rig cannot
necessarily send it. `p2-load`'s own fan-out assert allows `sessions × 160`
diffs/s (`check_fan_out`), so a point provisioned at exactly that has zero
margin, and above about 99 k diffs/s this box's rig runs out regardless of
sessions. Under-delivery is silent: `UplinkScheduler::queue` is newest-wins,
so the diffs that do not fit are dropped by the *client*, not shed by the
gateway, and nothing in the server's telemetry can see it.

Reporting the nominal number alone produced a published table in which the
120 k and 160 k rows were the same operating point — 97 767 and 99 337
diffs/s delivered on the before arm, 99 238 and 99 126 on the after arm — and
in which "the knee moved" could not be told from "the rig ran out". So this
prints `delivered_per_s` (measured, from the rig's own `diffs=` count),
`rig_cap_per_s` (`sessions × 160`) and `delivered_pct` next to it. Read
`delivered_per_s` first. A point whose `delivered_per_s` is far below its
`nominal_per_s`, or at its `rig_cap_per_s`, measured the rig.
"""
from __future__ import annotations

import collections
import json
import pathlib
import re
import sys


def jsonl(path: pathlib.Path):
    if not path.exists():
        return
    for line in path.read_text(errors="replace").splitlines():
        try:
            yield json.loads(line)
        except json.JSONDecodeError:
            continue


def route_stage(path: pathlib.Path) -> dict:
    tot: dict[str, int] = {}
    for rec in jsonl(path):
        if rec.get("type") != "gateway_route_stage":
            continue
        for k, v in rec.items():
            if k == "type":
                continue
            tot[k] = max(tot.get(k, 0), v) if k.endswith("_max") else tot.get(k, 0) + v
    return tot


def last_ingress(path: pathlib.Path) -> dict:
    out = {}
    for rec in jsonl(path):
        if rec.get("type") == "gateway_ingress":
            out = rec
    return out


def run_complete(path: pathlib.Path) -> dict:
    if not path.exists():
        return {}
    text = re.sub(r"\x1b\[[0-9;]*m", "", path.read_text(errors="replace"))
    for line in reversed(text.splitlines()):
        if "run complete" in line:
            return {
                k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", line.split("run complete")[1])
            }
    return {}


def persistd_threads(path: pathlib.Path, skip: int = 5) -> list[tuple[str, float, float]]:
    """Per-thread mean/peak %CPU, ranked. `pidstat -t -u -h` columns:
    <hh:mm:ss> <AM|PM> UID TGID TID %usr %system %guest %wait %CPU CPU Command."""
    if not path.exists():
        return []
    series: dict[str, list[float]] = {}
    names: dict[str, str] = {}
    for line in path.read_text(errors="replace").splitlines():
        f = line.split()
        if len(f) < 12 or f[0].startswith("#") or f[4] == "-" or not f[4].isdigit():
            continue
        series.setdefault(f[4], []).append(float(f[9]))
        names[f[4]] = f[11].lstrip("|_")
    out = []
    for tid, vals in series.items():
        v = vals[skip:] or vals
        out.append((names[tid], sum(v) / len(v), max(v)))
    out.sort(key=lambda r: -r[1])
    return out


def fdb_client_thread(path: pathlib.Path) -> tuple[float, float]:
    """The `libfdb_c` network thread.

    It inherits the process `comm` (`persistd`) and is the busier of the two
    threads that carry it — the other is the main thread, which parks in
    `accept`. Identified by name and rank rather than by TID because TIDs are
    per-run. Returns (mean %, peak %) of one core.
    """
    ranked = [t for t in persistd_threads(path) if t[0] == "persistd"]
    return (ranked[0][1], ranked[0][2]) if ranked else (float("nan"), float("nan"))


def row(d: pathlib.Path) -> dict:
    point = json.loads((d / "point.json").read_text()) if (d / "point.json").exists() else {}
    load = run_complete(d / "load.stderr")
    rs = route_stage(d / "primary-boundary.jsonl")
    ing = last_ingress(d / "primary-boundary.jsonl")
    dur = point.get("duration_secs", 30)
    mean, peak = fdb_client_thread(d / "pidstat-threads.txt")
    applies = rs.get("applies", 0)
    r = {
        "label": d.name,
        "arm": d.name.split("-")[0],
        "point": "-".join(d.name.split("-")[1:-1]),
        # Nominal demand. Kept, because it is what a sizing question is asked
        # in, but never on its own: see the module docstring.
        "nominal_per_s": point.get("sessions", 0) and 10_000 * point.get("diff_hz", 0),
        # What the rig actually put on the wire, from its own `run complete`
        # line. This is the load the box saw.
        "delivered_per_s": round(load.get("diffs", 0) / dur) if load.get("diffs") else None,
        # The rig's own ceiling for this point, from `check_fan_out`:
        # `FLUSH_BUDGET_BYTES / (payload + DIFF_OVERHEAD_BYTES) * FLUSH_HZ`
        # per session, which is 160 diffs/s/session at the study's payload.
        # A point with `nominal_per_s == rig_cap_per_s` was provisioned with
        # zero margin and will under-deliver.
        "rig_cap_per_s": 160 * point.get("sessions", 0) or None,
        "delivered_pct": round(
            100 * load.get("diffs", 0) / dur / (10_000 * point.get("diff_hz", 1)), 1
        )
        if load.get("diffs") and point.get("diff_hz")
        else None,
        "durable_acks_per_s": round(load.get("durable_acks", 0) / dur),
        "shed_pct": round(100 * ing.get("shed_slow_route", 0) / ing["admitted"], 2)
        if ing.get("admitted")
        else None,
        "diff_nacks": load.get("diff_nacks"),
        "leases_lost": load.get("leases_lost"),
        "intent_p99_ms": load.get("intent_p99_us", 0) / 1000,
        "bulk_p99_ms": load.get("bulk_p99_us", 0) / 1000,
        "fdb_thread_pct_mean": round(mean, 1),
        "fdb_thread_pct_peak": round(peak, 1),
        "applies": applies,
        "locate_ms_per_apply": round(rs.get("locate_us_sum", 0) / applies / 1000, 3)
        if applies
        else None,
        "gate_wait_ms_per_apply": round(rs.get("gate_wait_us_sum", 0) / applies / 1000, 3)
        if applies
        else None,
        "mailbox_ms_per_apply": round(rs.get("mailbox_us_sum", 0) / applies / 1000, 3)
        if applies
        else None,
        # Absent from the pre-change binary, which is why this is `None`
        # rather than 0 on the before leg: it did not count turns.
        "turns_per_apply": round(rs["mailbox_turns"] / applies, 4)
        if applies and "mailbox_turns" in rs
        else None,
        "locate_fallbacks": rs.get("locate_fallbacks"),
        "location_audits": rs.get("location_audits"),
        "location_mismatches": rs.get("location_mismatches"),
    }
    return r


FIELDS = [
    "nominal_per_s",
    "delivered_per_s",
    "rig_cap_per_s",
    "delivered_pct",
    "durable_acks_per_s",
    "shed_pct",
    "intent_p99_ms",
    "bulk_p99_ms",
    "fdb_thread_pct_mean",
    "fdb_thread_pct_peak",
    "locate_ms_per_apply",
    "gate_wait_ms_per_apply",
    "mailbox_ms_per_apply",
    "turns_per_apply",
    "locate_fallbacks",
    "location_mismatches",
    "leases_lost",
    "diff_nacks",
]


def fold(rows: list[dict]) -> None:
    by = collections.defaultdict(list)
    for r in rows:
        by[(r["point"], r["arm"])].append(r)
    # `point` is empty for a one-off directory whose name carries no label
    # (`<arm>` alone rather than `<arm>-<label>-r<n>`), so index it defensively:
    # a reducer that raises on a stray directory loses the whole table with it.
    points = sorted({p for p, _ in by}, key=lambda p: (p[:1], int(re.sub(r"\D", "", p) or 0)))
    # Arm names come from the directory prefix, so this folds any two-armed
    # study, not just the binary one it was written for: the ssd/memory engine
    # arms of `fenced-ssd-driver.sh` fold here too. `before`/`after` keep their
    # order when present; anything else sorts alphabetically after them.
    order = {"before": 0, "after": 1}
    arms = sorted({a for _, a in by}, key=lambda a: (order.get(a, 2), a))
    print("\t".join(["point", "arm", "n"] + FIELDS))
    for point in points:
        for arm in arms:
            group = by.get((point, arm))
            if not group:
                continue
            cells = []
            for f in FIELDS:
                vals = [r[f] for r in group if r.get(f) is not None]
                if not vals:
                    cells.append("")
                elif min(vals) == max(vals):
                    cells.append(f"{min(vals)}")
                else:
                    cells.append(f"{min(vals)}-{max(vals)}")
            print("\t".join([point, arm, str(len(group))] + cells))


def main() -> int:
    dirs = [pathlib.Path(a) for a in sys.argv[1:]]
    rows = [row(d) for d in dirs if d.is_dir() and (d / "point.json").exists()]
    if not rows:
        print("no completed points", file=sys.stderr)
        return 1
    fold(rows)
    starved = sorted(
        {
            r["point"]
            for r in rows
            if r["delivered_pct"] is not None and r["delivered_pct"] < 95
        }
    )
    if starved:
        print(
            "\nWARNING: the rig delivered <95% of nominal at: "
            + ", ".join(starved)
            + "\nThose rows measure the load generator, not the box. Compare arms by "
            "delivered_per_s, and report an unreached knee as '>= <delivered>, not located'.",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
