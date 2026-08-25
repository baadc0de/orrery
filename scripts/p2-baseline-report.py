#!/usr/bin/env python3
"""Print every number docs/08 §2.2.2 quotes, from the baseline's raw summaries.

§2.2.1 is bound to `scripts/intent-tail-derive.py` because that section was
published three times and corrected twice, always in the same way: replacement
measurements asserted in prose and never re-derived. §2.2.2 is a different
experiment -- the full `scripts/p2-kill9-gate.sh`, not the capacity sweep -- so
that script cannot produce these numbers and its `--audit-doc` is scoped away
from this section. This file is the equivalent for §2.2.2, and the same habits
apply: every range carries its n and the runs behind it, every subset states
the rule that made it a subset, and no aggregate is reported without its
spread.

Unlike the sweep's ~10 GB of JSONL, this baseline's evidence is small enough to
version: `scripts/p2-baseline-extract.py` reduces each ~1 GB gate directory to
one JSON object, and all of them are in
`docs/data/p2-phase-baseline-2026-08-19.jsonl`. So §2.2.2 is re-derivable from
the tree alone.

Usage::

    scripts/p2-baseline-report.py [SUMMARIES]   # a .jsonl file, or a directory
                                                # of per-run .json summaries
    scripts/p2-baseline-report.py --self-test   # the shape the doc depends on
"""

from __future__ import annotations

import collections
import json
import pathlib
import statistics
import sys

DEFAULT = (pathlib.Path(__file__).resolve().parent.parent
           / "docs" / "data" / "p2-phase-baseline-2026-08-19.jsonl")

# The four D16 series and their budgets (docs/16, docs/09 §; the gate reads
# them out of `gates/p2-dashboard`'s own thresholds, and these are here only to label
# the tables).
GATED = ("journal_commit_ms", "bulk_ack_ms", "intent_commit_ms", "area_first_page_ms")
BUDGET_MS = {"journal_commit_ms": 2, "bulk_ack_ms": 5, "intent_commit_ms": 10,
             "area_first_page_ms": 50}

# The sweep's regime rule, unchanged: slow iff the run's worst journal
# `sync_data` is at or above this (docs/08 §2.2.1, §4.3). Repeated rather than
# imported because `intent-tail-derive.py` reads the sweep's directory layout.
SLOW_REGIME_SYNC_MS = 150.0


def load(path: pathlib.Path) -> list:
    if path.is_dir():
        runs = [json.loads(p.read_text()) for p in sorted(path.glob("*.json"))]
    else:
        runs = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    return sorted(runs, key=lambda r: (r["arm"], r["label"]))


def p99(run: dict, series: str):
    """The dashboard's p99 for one series, in ms, or None if it had no samples.

    This is a lattice bucket's upper bound, never an interpolation
    (`orrery_protocol::metrics::LATENCY_BOUNDARIES_US`). The neighbours around
    the intent budget are 9, 10, 15, 20 ms, so a run printed at 15 ms has a
    true p99 somewhere in (10, 15] -- a real miss of a 10 ms budget, and
    possibly a miss by microseconds. Every caller prints it as a bucket.
    """
    s = (run.get("series") or {}).get(series)
    if not s or s.get("p99_us") is None:
        return None
    return s["p99_us"] / 1000


def gate_of(run: dict, series: str):
    return ((run.get("series") or {}).get(series) or {}).get("gate")


def g(x) -> str:
    return f"{x:g}" if x is not None else "-"


def spread(pop: list, series: str):
    vals = sorted((p99(r, series), r["label"]) for r in pop if p99(r, series) is not None)
    return vals[0], vals[-1], len(vals)


def head(title: str) -> None:
    print()
    print("=" * 78)
    print(title)
    print("=" * 78)


def section_population(runs: list) -> None:
    head("The population")
    ph = [r for r in runs if r["arm"] == "phased"]
    un = [r for r in runs if r["arm"] == "unphased"]
    print(f"  runs {len(runs)}: phased {len(ph)}, unphased {len(un)}, interleaved run by run")
    reached = sum(1 for r in runs if r.get("series"))
    print(f"  runs that reached the latency gate (every earlier proof stage passed): "
          f"{reached} of {len(runs)}")
    it = sorted(r["stage"]["intents"] for r in runs)
    ack = sorted(r["durable_acks"] for r in runs)
    print(f"  intents executed per run {it[0]}-{it[-1]}; "
          f"durable bulk acks per run {ack[0]}-{ack[-1]}")


def section_device(runs: list) -> None:
    head("The device, and whether the sweep's regime cut still bisects this workload")
    xs = sorted((r["journal_sync_max_ms"], r["label"]) for r in runs)
    below = [v for v, _ in xs if v < SLOW_REGIME_SYNC_MS]
    above = [v for v, _ in xs if v >= SLOW_REGIME_SYNC_MS]
    print(f"  worst journal sync_data {xs[0][0]:.1f}-{xs[-1][0]:.1f} ms "
          f"(n={len(xs)}, min {xs[0][1]}, max {xs[-1][1]})")
    print(f"  fast (< {SLOW_REGIME_SYNC_MS:.0f} ms) n={len(below)}; "
          f"slow (>= {SLOW_REGIME_SYNC_MS:.0f} ms) n={len(above)}")
    if below and above:
        print(f"  the gap the {SLOW_REGIME_SYNC_MS:.0f} ms cut sits in: "
              f"{max(below):.1f} -> {min(above):.1f} ms")
    else:
        print("  the cut is not straddled by this population; it does not bisect it")


def section_series(runs: list) -> None:
    head("The four D16 series, per arm, with the spread and the pass count")
    print("  | series | budget | phased p99 | passes | unphased p99 | passes |")
    print("  |---|---|---|---|---|---|")
    for name in GATED:
        cells = [f"  | `{name}` | {BUDGET_MS[name]} ms "]
        for arm in ("phased", "unphased"):
            pop = [r for r in runs if r["arm"] == arm]
            lo, hi, n = spread(pop, name)
            npass = sum(1 for r in pop if gate_of(r, name) == "pass")
            cells.append(f"| **{g(lo[0])}-{g(hi[0])} ms** (n={n}) | {npass} of {len(pop)} ")
        print("".join(cells) + "|")

    print()
    print("  p99 bucket histograms -- the doc quotes the median and the modal bucket")
    for arm in ("phased", "unphased"):
        pop = [r for r in runs if r["arm"] == arm]
        print(f"    {arm} (n={len(pop)})")
        for name in GATED:
            vals = [p99(r, name) for r in pop if p99(r, name) is not None]
            c = collections.Counter(vals)
            hist = " ".join(f"{k:g}ms x{c[k]}" for k in sorted(c))
            print(f"      {name:<20} median {statistics.median(vals):g} ms | {hist}")


def section_regime(runs: list) -> None:
    head("Regime dependence: the sweep's own rule, applied inside each arm")
    for arm in ("phased", "unphased"):
        for regime in ("fast", "slow"):
            pop = [r for r in runs if r["arm"] == arm
                   and ((r["journal_sync_max_ms"] >= SLOW_REGIME_SYNC_MS)
                        == (regime == "slow"))]
            if not pop:
                print(f"  {arm} / {regime}: n=0")
                continue
            print(f"  {arm} / {regime}: n={len(pop)} "
                  f"[{', '.join(sorted(r['label'] for r in pop))}]")
            for name in GATED:
                lo, hi, n = spread(pop, name)
                npass = sum(1 for r in pop if gate_of(r, name) == "pass")
                print(f"     {name:<20} p99 {g(lo[0])}-{g(hi[0])} ms "
                      f"(budget {BUDGET_MS[name]}) pass {npass}/{len(pop)} "
                      f"(min {lo[1]}, max {hi[1]})")


def section_burst(runs: list) -> None:
    head("What phasing moved: GRV, and what intent latency is now set by")
    for arm in ("phased", "unphased"):
        pop = [r for r in runs if r["arm"] == arm]
        grv = sorted((r["stage"]["grv_total_s"], r["label"]) for r in pop)
        tg = sorted((r["stage"]["tail_grv_mean_ms"], r["label"]) for r in pop)
        tc = sorted((r["stage"]["tail_commit_mean_ms"], r["label"]) for r in pop)
        print(f"  {arm} (n={len(pop)})")
        print(f"     run-total GRV      {grv[0][0]:.2f}-{grv[-1][0]:.2f} s "
              f"(min {grv[0][1]}, max {grv[-1][1]})")
        print(f"     tail grv mean      {tg[0][0]:.2f}-{tg[-1][0]:.2f} ms "
              f"(min {tg[0][1]}, max {tg[-1][1]})")
        print(f"     tail commit mean   {tc[0][0]:.2f}-{tc[-1][0]:.2f} ms "
              f"(min {tc[0][1]}, max {tc[-1][1]})")
        ratios = sorted((p99(r, "intent_commit_ms") / p99(r, "journal_commit_ms"), r["label"])
                        for r in pop
                        if p99(r, "intent_commit_ms") and p99(r, "journal_commit_ms"))
        print(f"     intent p99 / journal p99  {ratios[0][0]:.2f}-{ratios[-1][0]:.2f}x "
              f"(n={len(ratios)}, min {ratios[0][1]}, max {ratios[-1][1]})")


def section_proofs(runs: list) -> None:
    head("What the gate proves, on every run")
    bad = [r["label"] for r in runs if not (r.get("recovery") or {}).get("pass")]
    print(f"  recovery verification true in {len(runs) - len(bad)} of {len(runs)}"
          + (f"; NOT in {bad}" if bad else ""))
    mism = [r["label"] for r in runs
            if int((r.get("client") or {}).get("durable_acks", -1)) != r["durable_acks"]]
    print(f"  the ack log's durable diffs equal the client's own count in "
          f"{len(runs) - len(mism)} of {len(runs)}"
          + (f"; disagreements {mism}" if mism else ""))
    print(f"  max leases_lost {max(int((r.get('client') or {}).get('leases_lost', -1)) for r in runs)}; "
          f"max diff_nacks {max(int((r.get('client') or {}).get('diff_nacks', -1)) for r in runs)}; "
          f"max unknown_series {max(r.get('unknown_series', 0) for r in runs)}")
    leases = sorted({int((r.get("client") or {}).get("leases", -1)) for r in runs})
    print(f"  leases held at end of run: {leases}")
    roots = collections.Counter(tuple(r.get("root_causes") or []) for r in runs)
    for k, v in roots.most_common():
        print(f"  root causes {list(k)}: {v} of {len(runs)} runs")


def per_run_tables(runs: list) -> None:
    head("Per run -- no aggregate below is quoted without these behind it")
    for arm, title in (("phased", "Phased, the new default"),
                       ("unphased", "Unphased, `P2_LOAD_HEARTBEAT_PHASED=0`")):
        pop = sorted((r for r in runs if r["arm"] == arm),
                     key=lambda r: r["journal_sync_max_ms"])
        print()
        print(f"  **{title}** (n={len(pop)})")
        print("  | run | worst journal fsync | `journal_commit_ms` p99 | `bulk_ack_ms` p99 "
              "| `intent_commit_ms` p99 | `area_first_page_ms` p99 | run-total GRV |")
        print("  |---|---|---|---|---|---|---|")
        for r in pop:
            print(f"  | {r['label']} | {r['journal_sync_max_ms']:.1f} ms "
                  f"| {g(p99(r,'journal_commit_ms'))} ms | {g(p99(r,'bulk_ack_ms'))} ms "
                  f"| {g(p99(r,'intent_commit_ms'))} ms | {g(p99(r,'area_first_page_ms'))} ms "
                  f"| {r['stage']['grv_total_s']:.2f} s |")


def self_test(runs: list) -> int:
    """Assert the shape §2.2.2's argument rests on, not its values.

    Values move when the baseline is re-run; the shape must not. A run that
    never reached the latency gate, an arm that vanished, or a summary missing
    the field a claim is made from would each turn a sentence in the doc into
    an assertion about nothing.
    """
    ok = True

    def check(label: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'ok  ' if cond else 'FAIL'} {label}" + (f"  {detail}" if detail else ""))
        ok = ok and cond

    check("both arms present",
          {r["arm"] for r in runs} == {"phased", "unphased"},
          str(sorted({r["arm"] for r in runs})))
    check("every run carries all four gated series",
          all(all((r.get("series") or {}).get(n) for n in GATED) for r in runs))
    check("every run carries a recovery verdict",
          all("pass" in (r.get("recovery") or {}) for r in runs))
    check("every run carries the client footer the lease claim reads",
          all("leases_lost" in (r.get("client") or {}) for r in runs))
    check("every run carries the journal fsync the regime rule reads",
          all("journal_sync_max_ms" in r for r in runs))
    check("labels are unique", len({r["label"] for r in runs}) == len(runs))
    check("no run mixes arms with its label",
          all(r["label"].startswith("ph-") == (r["arm"] == "phased") for r in runs))
    print("SELF-TEST PASSED" if ok else "SELF-TEST FAILED")
    return 0 if ok else 1


def main(argv: list[str]) -> int:
    args = [a for a in argv[1:] if not a.startswith("--")]
    path = pathlib.Path(args[0]) if args else DEFAULT
    if not path.exists():
        print(f"summaries not found: {path}", file=sys.stderr)
        return 2
    runs = load(path)
    if "--self-test" in argv:
        return self_test(runs)
    print(f"p2-baseline-report: {len(runs)} runs from {path}")
    section_population(runs)
    section_device(runs)
    section_series(runs)
    section_regime(runs)
    section_burst(runs)
    section_proofs(runs)
    per_run_tables(runs)
    print()
    print("=" * 78)
    print("A number not printed here does not go in §2.2.2.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
