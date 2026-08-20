#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §4.7, derived from its two data files.

§4.6 eliminated every co-tenant outside `persistd` and left two candidates it
could not separate: fjall's own work, and the *shape* of the I/O an LSM asks
for. This section separates them and names the mechanism.

Three legs:

    gate      12 `p2-kill9-gate.sh` runs carrying the new instrumentation, so
              the worst barrier's bytes are recorded beside its cost
    placement 9 runs of the open-loop journal rig on ext4, xfs and **tmpfs** --
              no gateway, no FoundationDB, no follower, no network, and in the
              tmpfs arm no block device at all
    sweep     14 rig runs varying only fjall's `max_memtable_size`, which is
              what decides how often a commit meets fjall's backpressure sleep

    python3 scripts/p2-barrier-shape-report.py             # the section's numbers
    python3 scripts/p2-barrier-shape-report.py --self-test # and its claims
"""
import json
import pathlib
import statistics as st
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
GATE = ROOT / "docs/data/p2-barrier-shape-2026-08-19.jsonl"
RIG = ROOT / "docs/data/p2-journal-rig-2026-08-19.json"


def load():
    gate = [json.loads(x) for x in GATE.read_text().splitlines() if x.strip()]
    rig = json.loads(RIG.read_text())
    return gate, rig


def by(rows, key, value):
    return [r for r in rows if r[key] == value]


def report():
    gate, rig = load()
    print(f"docs/08-persistence.md §4.7 — {GATE.name} + {RIG.name}")
    print(f"  {rig['host']['machine_type']} / {rig['host']['zone']}, fjall "
          f"{rig['fjall']['version']}, default memtable "
          f"{rig['fjall']['default_max_memtable_size_mib']} MiB\n")

    # -- leg 1: the discriminator §4.6 asked for --------------------------
    print("  the gate: what the worst barrier was carrying")
    print(f"    {'run':<16}{'jc p99':>8}{'worst ms':>10}{'worst KB':>10}{'worst rec':>11}"
          f"{'ordinary KB':>13}{'x ordinary':>11}")
    for r in gate:
        b = r["barrier_shape"]
        print(f"    {r['label']:<16}{r['series']['journal_commit_ms']['p99_us'] / 1000:8.1f}"
              f"{b['worst_ms']:10.1f}{b['worst_kb']:10.2f}{b['worst_records']:11d}"
              f"{b['ordinary_kb_mean']:13.2f}{b['worst_kb'] / b['ordinary_kb_mean']:11.2f}")
    ratios = sorted(r["barrier_shape"]["worst_kb"] / r["barrier_shape"]["ordinary_kb_mean"]
                    for r in gate)
    typical = [x for x in ratios if x < 2]
    print(f"    -> {len(typical)} of {len(ratios)} worst barriers carried under 2x an ordinary "
          f"batch (median {st.median(ratios):.2f}x). The volume hypothesis is refuted: the "
          f"slowest barrier is an ordinary one.\n")

    # -- leg 2: the rig, and the tmpfs control ----------------------------
    print("  the open-loop rig: no gateway, no FoundationDB, no network")
    print(f"    {'storage':<8}{'n':>3}{'sync us/flush':>15}{'p99':>8}{'p99.9':>9}{'max':>8}"
          f"{'slow':>6}{'worst ms':>10}{'worst KB':>10}")
    for s in ("ext4", "xfs", "tmpfs"):
        v = by(rig["placement_leg"], "storage", s)
        print(f"    {s:<8}{len(v):>3}{st.median(r['sync_data_us_per_flush'] for r in v):15.1f}"
              f"{st.median(r['p99_ms'] for r in v):8.2f}{st.median(r['p99_9_ms'] for r in v):9.1f}"
              f"{st.median(r['max_ms'] for r in v):8.1f}{sum(r['slow_barriers'] for r in v):6d}"
              f"{max(r['worst_ms'] for r in v):10.1f}{st.median(r['worst_kb'] for r in v):10.2f}")
    t = by(rig["placement_leg"], "storage", "tmpfs")
    print(f"    -> tmpfs stalls too: {sum(r['slow_barriers'] for r in t)} barriers over "
          f"{t[0]['slow_threshold_ms']} ms, worst {max(r['worst_ms'] for r in t):.0f} ms, on a "
          f"'device' whose mean sync costs {st.median(r['sync_data_us_per_flush'] for r in t):.1f} us. "
          f"Storage is out.\n")

    # -- leg 3: the causal manipulation -----------------------------------
    print("  varying only fjall's max_memtable_size")
    print(f"    {'memtable':>10}{'secs':>6}{'n':>3}{'slow':>6}{'p99.9 ms':>11}{'max ms':>9}"
          f"{'worst ms':>10}{'worst KB':>10}")
    for mb, secs in sorted({(r["memtable_mib"], r["seconds"]) for r in rig["memtable_sweep"]}):
        v = [r for r in rig["memtable_sweep"]
             if r["memtable_mib"] == mb and r["seconds"] == secs]
        print(f"    {mb:>8} MiB{secs:>6}{len(v):>3}{sum(r['slow_barriers'] for r in v):6d}"
              f"{st.median(r['p99_9_ms'] for r in v):11.1f}{st.median(r['max_ms'] for r in v):9.1f}"
              f"{max(r['worst_ms'] for r in v):10.1f}{st.median(r['worst_kb'] for r in v):10.2f}")
    short = [r for r in rig["memtable_sweep"] if r["memtable_mib"] == 256 and r["seconds"] == 60]
    long_ = [r for r in rig["memtable_sweep"] if r["memtable_mib"] == 256 and r["seconds"] == 180]
    print(f"    -> 256 MiB looks like a fix at 60 s ({sum(r['slow_barriers'] for r in short)} stalls) "
          f"and is not one: at 180 s the same setting produces "
          f"{sum(r['slow_barriers'] for r in long_)} stalls with p99.9 "
          f"{st.median(r['p99_9_ms'] for r in long_):.0f} ms. It defers rotation and makes each "
          f"one worse.\n")

    print("  the mechanism")
    print(f"    {rig['fjall']['mechanism']}")
    print(f"    {rig['fjall']['source']}")
    worst = sorted(r["worst_ms"] for r in rig["placement_leg"] + rig["memtable_sweep"]
                   if r["slow_barriers"] > 0)
    print(f"    worst barrier per run, every leg (ms): "
          f"{', '.join(f'{x:.0f}' for x in worst)}")


def self_test():
    gate, rig = load()
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    check("gate population", len(gate) == 12, f"{len(gate)} gate runs")
    check("placement population", len(rig["placement_leg"]) == 9,
          f"{len(rig['placement_leg'])} rig runs")
    check("sweep population", len(rig["memtable_sweep"]) == 14,
          f"{len(rig['memtable_sweep'])} sweep runs")

    # -- the discriminator, and it must point AWAY from volume -------------
    ratios = [r["barrier_shape"]["worst_kb"] / r["barrier_shape"]["ordinary_kb_mean"]
              for r in gate]
    check("the worst barrier is an ordinary one", sum(1 for x in ratios if x < 2) >= 10,
          f"only {sum(1 for x in ratios if x < 2)} of {len(ratios)} worst barriers carried "
          f"under 2x an ordinary batch -- the section's refutation of the volume hypothesis "
          f"rests on this")
    check("and it is not a rounding artefact", st.median(ratios) < 2,
          f"median ratio {st.median(ratios):.2f}")

    # -- the tmpfs control is the whole storage elimination ---------------
    t = by(rig["placement_leg"], "storage", "tmpfs")
    check("tmpfs stalls", sum(r["slow_barriers"] for r in t) > 0,
          "no tmpfs run stalled -- storage would be back as a candidate")
    check("tmpfs stalls are large", max(r["worst_ms"] for r in t) > 100,
          f"worst tmpfs barrier {max(r['worst_ms'] for r in t):.1f} ms")
    check("tmpfs sync is nearly free",
          st.median(r["sync_data_us_per_flush"] for r in t) < 10,
          "tmpfs mean sync is no longer negligible, so the contrast is gone")
    for s in ("ext4", "xfs"):
        v = by(rig["placement_leg"], "storage", s)
        check(f"{s} stalls too", sum(r["slow_barriers"] for r in v) > 0, f"no {s} run stalled")

    # -- the causal manipulation, in both directions ----------------------
    short = [r for r in rig["memtable_sweep"] if r["memtable_mib"] == 256 and r["seconds"] == 60]
    long_ = [r for r in rig["memtable_sweep"] if r["memtable_mib"] == 256 and r["seconds"] == 180]
    check("a memtable size exists that shows no stall in 60 s",
          sum(r["slow_barriers"] for r in short) == 0,
          "the 256 MiB/60 s point no longer reads clean, which is half the section's point")
    check("and it is not a fix", sum(r["slow_barriers"] for r in long_) > 0,
          "the 256 MiB/180 s control no longer stalls -- the section would then be claiming "
          "a fix it does not have")
    check("the deferred stall is worse",
          st.median(r["p99_9_ms"] for r in long_) > 4 * max(
              st.median(r["p99_9_ms"] for r in
                        [x for x in rig["memtable_sweep"]
                         if x["memtable_mib"] == mb and x["seconds"] == 60])
              for mb in (64,)),
          "the 180 s 256 MiB tail is no longer far worse than the 64 MiB default's")
    check("the sweep moves the stall count", len({
        sum(r["slow_barriers"] for r in rig["memtable_sweep"]
            if r["memtable_mib"] == mb and r["seconds"] == 60)
        for mb in (8, 16, 32, 64, 128, 256)}) > 3,
        "varying the memtable size no longer moves the stall count, so the manipulation "
        "establishes nothing")

    # -- the mechanism is quoted from the dependency, not paraphrased ------
    check("the mechanism names the sleep", "sleep(Duration::from_millis(100))"
          in rig["fjall"]["mechanism"].replace("std::thread::", ""),
          "the recorded mechanism no longer names fjall's 100 ms backpressure sleep")
    check("the source is cited", "keyspace/mod.rs" in rig["fjall"]["source"]
          and "batch/mod.rs" in rig["fjall"]["source"],
          "the fjall source citation is incomplete")
    check("default memtable recorded", rig["fjall"]["default_max_memtable_size_mib"] == 64,
          "fjall's default memtable size is no longer recorded as 64 MiB")

    # -- durability on the gate leg ---------------------------------------
    check("recovery", all(r["recovery"]["pass"] for r in gate), "a gate run failed recovery")
    check("no lease lost", all(r["client"]["leases_lost"] == 0 for r in gate), "a run lost a lease")
    check("gate still red", all(r["gate"] == "fail" for r in gate), "a gate run passed")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(gate)} gate runs, {len(rig['placement_leg'])} rig runs and "
          f"{len(rig['memtable_sweep'])} sweep runs, every §4.7 claim holds")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else (report() or 0))
