#!/usr/bin/env python3
"""Fold one or more p2-capacity-sweep point directories into the intent-path
stage decomposition.

Reads, per point:

* ``primary-boundary.jsonl`` — the gateway's ``gateway_intent_stage`` records
  (scope ``all`` and scope ``slow``) and its ``gateway_intent_exemplar``
  records, appended once per report interval.
* ``load.jsonl`` — the rig's ``sample_batch`` records, from which the gated
  ``intent_commit_ms`` percentiles are reconstructed.
* ``load.stderr`` — the run footer, for the arrival-stamped percentiles and
  the delivered rates.
* ``point.json`` — the point's configuration.

**Denominators.** Gateway stages divide by ``intents`` (one per definitive
reply); FDB stages divide by ``executed`` (one per intent that reached the
executor). They are different numbers whenever anything is refused, and mixing
them is the error this script exists partly to make impossible.

**Percentiles are bucket upper bounds.** ``value_us`` in a ``sample_batch`` is
the boundary of the bucket the sample fell in, so a reported p99 of 150 000
means "somewhere in (100 000, 150 000]". Printed as a range for that reason.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

# The shared D16 lattice (orrery_protocol::metrics::LATENCY_BOUNDARIES_US),
# needed only to turn a bucket upper bound back into the interval it stands for.
BOUNDARIES = [
    50, 100, 150, 200, 300, 400, 500, 750, 1_000, 1_500, 2_000, 3_000, 4_000,
    5_000, 7_500, 10_000, 15_000, 20_000, 30_000, 40_000, 50_000, 75_000,
    100_000, 150_000, 200_000, 300_000, 400_000, 500_000, 750_000, 1_000_000,
    1_500_000, 2_000_000, 3_000_000, 4_000_000, 5_000_000, 7_500_000,
    10_000_000,
]

GATEWAY_STAGES = ["ingress", "admit", "spawn_wait", "exec", "reply", "server_gap"]
FDB_STAGES = [
    "alloc_wait", "alloc_refill", "grv", "idem_read", "fence", "commit",
    "backoff", "fdb_gap",
]


def percentiles(samples, qs=(0.5, 0.9, 0.99)):
    """Percentiles over (value_us, count) pairs, as (lower, upper) bounds."""
    total = sum(c for _, c in samples)
    if total == 0:
        return {q: None for q in qs}
    ordered = sorted(samples)
    out = {}
    for q in qs:
        target = q * total
        seen = 0
        for value, count in ordered:
            seen += count
            if seen >= target:
                idx = BOUNDARIES.index(value) if value in BOUNDARIES else None
                lower = BOUNDARIES[idx - 1] if idx else 0
                out[q] = (lower, value)
                break
        else:
            out[q] = (ordered[-1][0], ordered[-1][0])
    return out


def read_point(d: Path):
    point = {}
    pj = d / "point.json"
    if pj.exists():
        point = json.loads(pj.read_text().splitlines()[0])

    stage = {"all": {}, "slow": {}}
    exemplars = []
    boundary = d / "primary-boundary.jsonl"
    if boundary.exists():
        for line in boundary.read_text().splitlines():
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = rec.get("type")
            if kind == "gateway_intent_stage":
                scope = rec.get("scope", "all")
                acc = stage.setdefault(scope, {})
                for k, v in rec.items():
                    if k in ("type", "scope"):
                        continue
                    if k.endswith("_max") or k == "fence_read_max_us":
                        acc[k] = max(acc.get(k, 0), v)
                    else:
                        acc[k] = acc.get(k, 0) + v
            elif kind == "gateway_intent_exemplar":
                exemplars.append(rec)

    client = {}
    # The rig writes the client series; persistd writes its own server spans to
    # `--metrics-jsonl`. Both are read, because the whole point of the
    # arrival-stamped client series is to be compared with the server one.
    for load in (d / "load.jsonl", d / "primary-metrics.jsonl"):
        if not load.exists():
            continue
        for line in load.read_text().splitlines():
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if rec.get("type") == "sample_batch":
                client.setdefault(rec["series"], []).append(
                    (rec["value_us"], rec["count"])
                )

    footer = {}
    stderr = d / "load.stderr"
    if stderr.exists():
        text = re.sub(r"\x1b\[[0-9;]*m", "", stderr.read_text())
        for key in (
            "diffs", "acks", "durable_acks", "intents", "intent_acks",
            "intent_p99_us", "intent_arrival_p50_us", "intent_arrival_p90_us",
            "intent_arrival_p99_us", "intent_arrival_max_us", "bulk_p99_us",
        ):
            m = re.findall(rf"\b{key}=(\d+)", text)
            if m:
                footer[key] = int(m[-1])
    return point, stage, exemplars, client, footer


def fmt_us(v):
    if v is None:
        return "-"
    return f"{v/1000:.2f}ms" if v >= 1000 else f"{v}us"


def report(d: Path):
    point, stage, exemplars, client, footer = read_point(d)
    dur = point.get("duration_secs", 30)
    print(f"\n=== {d.name} ===")
    print(
        f"  config: sessions={point.get('sessions')} diff_hz={point.get('diff_hz')} "
        f"mix={point.get('intent_mix')} engine={point.get('storage_engine')} "
        f"dur={dur}s exit={point.get('load_exit')}"
    )
    if footer:
        diffs = footer.get("diffs", 0)
        ints = footer.get("intents", 0)
        print(
            f"  delivered: {diffs/dur:.0f} diffs/s, {ints/dur:.1f} intents/s "
            f"(intent_acks={footer.get('intent_acks')})"
        )

    cc = client.get("intent_commit_ms", [])
    if cc:
        p = percentiles(cc)
        print(f"  intent_commit_ms (gated, n={sum(c for _, c in cc)}):")
        for q in (0.5, 0.9, 0.99):
            lo, hi = p[q]
            print(f"      p{int(q*100):<3} ({fmt_us(lo)}, {fmt_us(hi)}]")
    if footer.get("intent_arrival_p99_us") is not None:
        print(
            "  arrival-stamped (rig poll delay removed): "
            f"p50={fmt_us(footer['intent_arrival_p50_us'])} "
            f"p90={fmt_us(footer['intent_arrival_p90_us'])} "
            f"p99={fmt_us(footer['intent_arrival_p99_us'])} "
            f"max={fmt_us(footer.get('intent_arrival_max_us'))}"
        )
    srv = client.get("gateway_intent_server_ms", [])
    if srv:
        p = percentiles(srv)
        print(f"  gateway_intent_server_ms (n={sum(c for _, c in srv)}):")
        for q in (0.5, 0.9, 0.99):
            lo, hi = p[q]
            print(f"      p{int(q*100):<3} ({fmt_us(lo)}, {fmt_us(hi)}]")

    for scope in ("all", "slow"):
        acc = stage.get(scope) or {}
        n = acc.get("intents", 0)
        if not n:
            continue
        ex = acc.get("executed", 0) or 1
        print(
            f"  stages [{scope}]  intents={n} executed={acc.get('executed')} "
            f"attempts={acc.get('attempts')} retries={acc.get('attempts',0)-acc.get('executed',0)} "
            f"refills={acc.get('alloc_refills')} "
            f"fence_reads/intent={acc.get('fence_reads',0)/ex:.1f}"
        )
        print(f"      {'stage':<12} {'mean':>10} {'max':>10}   denom")
        print(
            f"      {'server':<12} {fmt_us(acc.get('server_us_sum',0)//n):>10} "
            f"{fmt_us(acc.get('server_us_max')):>10}   intents"
        )
        for s in GATEWAY_STAGES:
            print(
                f"      {s:<12} {fmt_us(acc.get(s+'_us_sum',0)//n):>10} "
                f"{fmt_us(acc.get(s+'_us_max')):>10}   intents"
            )
        for s in FDB_STAGES:
            key_max = "fence_read_max_us" if s == "fence_read" else s + "_us_max"
            print(
                f"      {s:<12} {fmt_us(acc.get(s+'_us_sum',0)//ex):>10} "
                f"{fmt_us(acc.get(key_max)):>10}   executed"
            )
        print(
            f"      {'fence_read':<12} {'-':>10} "
            f"{fmt_us(acc.get('fence_read_max_us')):>10}   (slowest single read)"
        )

    if exemplars:
        worst = max(exemplars, key=lambda r: r["server_us"])
        print(f"  worst exemplar of {len(exemplars)} intervals:")
        order = [
            "server_us", "ingress_us", "admit_us", "spawn_wait_us", "exec_us",
            "alloc_wait_us", "alloc_refill_us", "grv_us", "idem_read_us",
            "fence_us", "fence_read_max_us", "commit_us", "backoff_us",
            "fdb_gap_us", "server_gap_us", "reply_us",
        ]
        for k in order:
            if k in worst:
                print(f"      {k:<20} {fmt_us(worst[k])}")
        print(f"      {'attempts':<20} {worst.get('attempts')}")
        print(f"      {'last_err_code':<20} {worst.get('last_err_code')}")
        top = sorted(exemplars, key=lambda r: -r["server_us"])[:5]
        print("  five slowest exemplars (server / exec / fence / commit / fdb_gap / spawn):")
        for e in top:
            print(
                f"      {fmt_us(e['server_us']):>9} {fmt_us(e['exec_us']):>9} "
                f"{fmt_us(e['fence_us']):>9} {fmt_us(e['commit_us']):>9} "
                f"{fmt_us(e['fdb_gap_us']):>9} {fmt_us(e['spawn_wait_us']):>9}"
            )


if __name__ == "__main__":
    for arg in sys.argv[1:]:
        report(Path(arg))
