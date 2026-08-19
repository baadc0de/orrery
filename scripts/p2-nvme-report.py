#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §4.4, derived from its two data files.

§4.4 is the re-measurement §4.3 asked for: the same `scripts/p2-kill9-gate.sh`,
unmodified, on storage with a power-loss-protected write cache. Sixteen runs,
eight per FoundationDB storage engine, arms interleaved; plus two diagnostic
legs and eleven `fio` jobs that decide what the journal's tail actually is.

    python3 scripts/p2-nvme-report.py             # the section's numbers
    python3 scripts/p2-nvme-report.py --self-test # hold them to the files

The section's argument is a *negative* one -- a 40x better barrier bought no
p99 -- so the self-test is written to fail in the direction the section could
be wrong in. It pins the elimination chain (device cleared, filesystem cleared,
CPU cleared, writeback reproduces) rather than only the headline, because a
data edit that broke any link would leave the headline standing on nothing.
"""
import json
import pathlib
import statistics as st
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
RUNS = ROOT / "docs/data/p2-nvme-2026-08-19.jsonl"
DEVICE = ROOT / "docs/data/p2-nvme-device-2026-08-19.json"

GATED = ("journal_commit_ms", "bulk_ack_ms", "intent_commit_ms", "area_first_page_ms")
BUDGET_MS = {"journal_commit_ms": 2, "bulk_ack_ms": 5,
             "intent_commit_ms": 10, "area_first_page_ms": 50}


def load():
    runs = [json.loads(x) for x in RUNS.read_text().splitlines() if x.strip()]
    dev = json.loads(DEVICE.read_text())
    for r in runs:
        r["p99"] = {k: r["series"][k]["p99_us"] / 1000 for k in GATED}
    matrix = [r for r in runs if r["leg"] == "matrix"]
    return runs, matrix, dev


def fio_job(dev, name):
    for j in dev["fio"]:
        if j["job"] == name:
            return j
    raise KeyError(name)


def band(pop, key):
    v = [r["p99"][key] for r in pop]
    return min(v), max(v), st.median(v)


def report():
    runs, matrix, dev = load()
    ssd = [r for r in matrix if r["arm"] == "ssd"]
    mem = [r for r in matrix if r["arm"] == "memory"]
    h = dev["host"]
    ref = dev["reference_box_fio_quoted"]

    print(f"docs/08-persistence.md §4.4 — {RUNS.name} + {DEVICE.name}")
    print(f"  {h['machine_type']} / {h['zone']}, {h['vcpu']} vCPU, {h['ram_gb']} GB, "
          f"{h['storage']}")
    print(f"  write cache: {h['write_cache']} (nvme vwc={h['nvme_vwc']}), FDB {h['foundationdb']}")
    print(f"  {len(matrix)} gate runs ({len(ssd)} ssd / {len(mem)} memory, interleaved) "
          f"+ {len(runs) - len(matrix)} diagnostic\n")

    # -- the device -------------------------------------------------------
    req = ref["derived_storage_requirement"]
    print("  the barrier, 8 KiB write + fdatasync, 1 thread")
    print(f"    {'job':<12} {'barriers/s':>11} {'p50':>8} {'p99':>8} {'p99.9':>8} {'max':>9}")
    for name in ("t1-60s", "t1-15s-r1", "t1-15s-r2", "t1-15s-r3", "t1-15s-r4"):
        j = fio_job(dev, name)
        print(f"    {name:<12} {j['barriers_per_s']:11.1f} {j['p50_ms']:8.3f} "
              f"{j['p99_ms']:8.3f} {j['p99_9_ms']:8.3f} {j['max_ms']:9.3f}")
    t = ref["t1_60s"]
    print(f"    {'[ref box]':<12} {t['barriers_per_s']:11.1f} {t['p50_ms']:8.3f} "
          f"{t['p99_ms']:8.3f} {t['p99_9_ms']:8.3f} {t['max_ms']:9.3f}   ({ref['storage']})")
    reps = [fio_job(dev, f"t1-15s-r{i}") for i in range(1, 5)]
    p99s = [j["p99_ms"] for j in reps]
    rates = [j["barriers_per_s"] for j in reps]
    print(f"    §4.3's derived requirement: p99 <= {req['p99_ms_at_most']} ms at "
          f">= {req['barriers_per_s_at_least']} barriers/s")
    print(f"    this device, 4 repeats:     p99 {min(p99s):.3f}-{max(p99s):.3f} ms at "
          f"{min(rates):.0f}-{max(rates):.0f} barriers/s  "
          f"-> {req['p99_ms_at_most'] / max(p99s):.0f}x on latency, "
          f"{min(rates) / req['barriers_per_s_at_least']:.0f}x on rate\n")

    # -- the gate ---------------------------------------------------------
    print("  every gate run")
    print(f"    {'run':<11} {'journal':>8} {'ack':>7} {'intent':>7} {'area':>6} "
          f"{'j p50':>7} {'fsync':>8} {'us/flush':>9} {'fl/s':>6} {'rho':>5} "
          f"{'acks':>8} {'rec':>4}")
    for r in matrix:
        j = r["journal_stage"]
        print(f"    {r['label']:<11} {r['p99']['journal_commit_ms']:8.1f} "
              f"{r['p99']['bulk_ack_ms']:7.1f} {r['p99']['intent_commit_ms']:7.1f} "
              f"{r['p99']['area_first_page_ms']:6.1f} "
              f"{r['series']['journal_commit_ms']['p50_us'] / 1000:7.2f} "
              f"{r['journal_sync_max_ms']:8.1f} {j['sync_data_us_per_flush']:9.1f} "
              f"{j['flushes_per_s']:6.0f} {j['rho']:5.2f} {r['durable_acks']:8d} "
              f"{'yes' if r['recovery']['pass'] else 'NO':>4}")
    print()

    print(f"  {'series':<22} {'budget':>7} {'range (ms)':>16} {'median':>7} {'passing':>9}")
    for k in GATED:
        lo, hi, med = band(matrix, k)
        npass = sum(1 for r in matrix if r["series"][k]["gate"] == "pass")
        print(f"    {k:<20} {BUDGET_MS[k]:7g} {f'{lo:g}-{hi:g}':>16} {med:7g} "
              f"{f'{npass} of {len(matrix)}':>9}")
    print()

    print("  the two FoundationDB storage engines")
    for k in GATED:
        a, b = band(ssd, k), band(mem, k)
        overlap = not (a[1] < b[0] or b[1] < a[0])
        print(f"    {k:<22} ssd {a[0]:5g}-{a[1]:5g} (med {a[2]:5g})   "
              f"memory {b[0]:5g}-{b[1]:5g} (med {b[2]:5g})   "
              f"{'overlap' if overlap else 'DISJOINT'}")
    print()

    # -- what the tail is -------------------------------------------------
    print("  the elimination chain")
    a, b, c, d = (fio_job(dev, n) for n in ("diagA", "diagB", "diagC", "diagD"))
    for j in (a, b, c, d):
        print(f"    {j['job']:<7} {j['description'][:58]:<58} "
              f"p99.9 {j['p99_9_ms']:8.3f}  p99.99 {j['p99_99_ms']:9.3f}  max {j['max_ms']:9.3f}")
    ins = dev["instrumented"]
    print(f"    cpu     idle {ins['cpu_idle_pct_range'][0]}-{ins['cpu_idle_pct_range'][1]} %, "
          f"psi cpu some avg10 max {ins['psi_cpu_some_avg10_max_pct']} %, "
          f"run queue {ins['runqueue_max']} of {ins['vcpu']}")
    print(f"    -> A and D differ only in a concurrent {dev['harness_writeback']['approx_mb_per_s']} MB/s "
          f"buffered writer; p99.9 moves {a['p99_9_ms']:.3f} -> {d['p99_9_ms']:.3f} ms, "
          f"p99.99 moves {a['p99_99_ms']:.3f} -> {d['p99_99_ms']:.1f} ms")
    fsyncs = [r["journal_sync_max_ms"] for r in matrix]
    print(f"    -> the gate's own worst journal fsync: {min(fsyncs):.1f}-{max(fsyncs):.1f} ms, "
          f"against {a['max_ms']:.2f} ms for the same barriers with no writeback\n")

    # -- the distribution --------------------------------------------------
    hist = {int(k): v for k, v in dev["journal_commit_histogram_us"].items()}
    total = sum(hist.values())
    print(f"  journal_commit_ms distribution, ccdf leg, n={total}")
    for t in (500, 2000, 10000, 15000, 50000):
        above = sum(c for b_, c in hist.items() if b_ > t)
        print(f"    > {t / 1000:6g} ms : {above:7d}  {100 * above / total:7.3f} %"
              + ("   <- the D16 budget" if t == 2000 else "")
              + ("   <- what puts p99 at 15 ms" if t == 15000 else ""))
    below = total - sum(c for b_, c in hist.items() if b_ > 500)
    print(f"    <= 0.5 ms  : {below:7d}  {100 * below / total:7.3f} %   <- the fast mode")
    print(f"    discrete stall events in that run: {len(dev['ccdf_stall_steps_ms']) - 2} "
          f"(running max stepped {dev['ccdf_stall_steps_ms']})\n")

    # -- durability --------------------------------------------------------
    print(f"  recovery {sum(1 for r in matrix if r['recovery']['pass'])} of {len(matrix)} | "
          f"leases_lost max {max(r['client']['leases_lost'] for r in matrix):.0f} | "
          f"diff_nacks max {max(r['client']['diff_nacks'] for r in matrix):.0f} | "
          f"acks {min(r['durable_acks'] for r in matrix)}-{max(r['durable_acks'] for r in matrix)}")


def self_test():
    runs, matrix, dev = load()
    ssd = [r for r in matrix if r["arm"] == "ssd"]
    mem = [r for r in matrix if r["arm"] == "memory"]
    ref = dev["reference_box_fio_quoted"]
    req = ref["derived_storage_requirement"]
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    check("population", len(matrix) == 16 and len(ssd) == 8 and len(mem) == 8,
          f"{len(matrix)} matrix runs, {len(ssd)} ssd / {len(mem)} memory")
    check("legs", {r["leg"] for r in runs} == {"matrix", "ccdf", "instrumented"},
          f"legs present: {sorted({r['leg'] for r in runs})}")

    # -- the premise: the device really is the one §4.3 asked for ----------
    reps = [fio_job(dev, f"t1-15s-r{i}") for i in range(1, 5)]
    check("device meets §4.3's requirement",
          all(j["p99_ms"] <= req["p99_ms_at_most"] for j in reps)
          and all(j["barriers_per_s"] >= req["barriers_per_s_at_least"] for j in reps),
          "a repeat missed p99 <= 1.5 ms at >= 400 barriers/s")
    check("device beats the reference box outright",
          max(j["p99_ms"] for j in reps) < min(ref["t1_15s_repeats"]["p99_ms"]),
          "the two boxes' 1-thread p99 populations are not disjoint")
    check("power-loss protected", dev["host"]["nvme_vwc"] == 0
          and dev["host"]["write_cache"] == "write through",
          "the device under test is not write-through")

    # -- the finding, which is a negative ---------------------------------
    check("journal_commit_ms still fails everywhere",
          all(r["series"]["journal_commit_ms"]["gate"] == "fail" for r in matrix),
          "a run passed journal_commit_ms, which would change the section's conclusion")
    check("bulk_ack_ms still fails everywhere",
          all(r["series"]["bulk_ack_ms"]["gate"] == "fail" for r in matrix),
          "a run passed bulk_ack_ms")
    jmode = st.mode([r["p99"]["journal_commit_ms"] for r in matrix])
    check("the modal p99 is still 15 ms", jmode == 15.0,
          f"modal journal_commit_ms p99 is {jmode}, not the reference box's 15 ms")
    # The body of the distribution DID improve; the section says so, and a data
    # edit that lost it would make the section's contrast wrong in the other
    # direction.
    check("the median did improve",
          all(r["series"]["journal_commit_ms"]["p50_us"] <= 500 for r in matrix),
          "a run's journal_commit_ms p50 exceeded 0.5 ms")
    check("per-flush sync fell below the reference box's 922 us",
          max(r["journal_stage"]["sync_data_us_per_flush"] for r in matrix) < 400,
          "per-flush sync_data is no longer far below the reference box's")
    check("the writer is idle", max(r["journal_stage"]["rho"] for r in matrix) < 0.2,
          "rho is no longer far below the reference box's 0.54")

    # -- the elimination chain, link by link ------------------------------
    a, b, c, d = (fio_job(dev, n) for n in ("diagA", "diagB", "diagC", "diagD"))
    check("device+fs cleared at the gate's own shape", a["max_ms"] < 1.0,
          f"diagA max {a['max_ms']} ms -- the device can produce the gate's stalls after all")
    check("device+fs cleared under saturation", b["max_ms"] < 1.0,
          f"diagB max {b['max_ms']} ms")
    check("raw device carries no flush cost", c["max_ms"] < 1.0,
          f"diagC max {c['max_ms']} ms")
    check("CPU cleared", dev["instrumented"]["psi_cpu_some_avg10_max_pct"] < 1.0
          and dev["instrumented"]["runqueue_max"] < dev["instrumented"]["vcpu"],
          "CPU pressure or run queue no longer rules out scheduling delay")
    check("writeback reproduces the stall", d["p99_99_ms"] > 100,
          f"diagD p99.99 {d['p99_99_ms']} ms -- the writeback mechanism no longer reproduces")
    # The crux: writeback must leave the BODY alone and move only the far tail.
    # A version of diagD that degraded p99.9 too would be a different mechanism.
    check("writeback leaves the body alone", d["p99_9_ms"] < 2 * a["p99_9_ms"],
          f"diagD p99.9 {d['p99_9_ms']} vs diagA {a['p99_9_ms']} -- not a far-tail-only effect")
    check("the stall sizes are in family",
          a["max_ms"] < min(r["journal_sync_max_ms"] for r in matrix) < d["max_ms"],
          "the gate's worst fsync no longer sits between the no-writeback and writeback maxima")

    # -- the distribution the p99 is drawn from ---------------------------
    hist = {int(k): v for k, v in dev["journal_commit_histogram_us"].items()}
    total = sum(hist.values())
    ccdf = [r for r in runs if r["leg"] == "ccdf"][0]
    check("histogram matches its run", total == ccdf["series"]["journal_commit_ms"]["n"],
          f"histogram sums to {total}, run reports n={ccdf['series']['journal_commit_ms']['n']}")
    fast = (total - sum(v for k, v in hist.items() if k > 500)) / total
    check("96 % of the run is in the fast mode", fast > 0.96,
          f"only {100 * fast:.2f} % of samples are <= 0.5 ms")
    over15 = sum(v for k, v in hist.items() if k > 15000) / total
    check("just over 1 % exceeds 15 ms", 0.01 < over15 < 0.02,
          f"{100 * over15:.2f} % above 15 ms -- the p99 is no longer set by the far tail")

    # -- the engine comparison, stated as *not separable* ------------------
    for k in GATED:
        x, y = band(ssd, k), band(mem, k)
        check(f"{k} engines overlap", not (x[1] < y[0] or y[1] < x[0]),
              f"ssd {x[0]}-{x[1]} vs memory {y[0]}-{y[1]} are disjoint -- the section says "
              f"the engines are not separable")

    # -- the hedges the section is most at risk of losing -------------------
    ip = sum(1 for r in matrix if r["series"]["intent_commit_ms"]["gate"] == "pass")
    check("intent_commit_ms passes some but not all", 0 < ip < len(matrix),
          f"{ip} of {len(matrix)} runs passed intent_commit_ms -- the section claims neither "
          f"a pass nor a uniform failure")
    check("area_first_page_ms passes throughout",
          all(r["series"]["area_first_page_ms"]["gate"] == "pass" for r in matrix),
          "a run failed area_first_page_ms")
    check("the gate is red in every run", all(r["gate"] == "fail" for r in matrix),
          "a run passed the gate outright")

    # -- durability, every run --------------------------------------------
    check("recovery", all(r["recovery"]["pass"] for r in matrix), "a run failed recovery")
    check("no lease lost", all(r["client"]["leases_lost"] == 0 for r in matrix), "a run lost a lease")
    check("leases held", all(r["client"]["leases"] == 10000 for r in matrix),
          "a run held fewer than 10 000 leases")
    check("no diff nack", all(r["client"]["diff_nacks"] == 0 for r in matrix), "a run was nacked")
    check("acks in family with the reference baseline",
          all(539000 <= r["durable_acks"] <= 542000 for r in matrix),
          "a run's durable ack count left the 539 352-541 264 family")
    check("no unrecognized series", all(r["unknown_series"] == 0 for r in matrix),
          "a producer drifted from orrery_protocol::metrics")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(matrix)} gate runs and {len(dev['fio'])} fio jobs, "
          f"every §4.4 claim holds against {RUNS.name} + {DEVICE.name}")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else (report() or 0))
