#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §4.6, derived from its two data files.

§4.4 attributed the journal's 50-175 ms stalls to buffered writeback from the
harness's own evidence files, on the strength of an `fio` job that reproduced
them. §4.5 added `P2_GATE_DATA_DIR` and could not test the attribution on the
reference box, because that box's bare barrier already stalls at 78 ms. This
section runs the test on §4.4's hardware, and the attribution does not survive
it.

Three placements x two filesystems, six repeats each:

    together  journals, FoundationDB and the harness's evidence on one device
    split     evidence on tmpfs -- zero harness bytes reach the journal's device
    isolated  evidence AND FoundationDB's data directory on tmpfs, so the
              journal has the device entirely to itself

    python3 scripts/p2-nvme-isolation-report.py             # the section's numbers
    python3 scripts/p2-nvme-isolation-report.py --self-test # and its eliminations

The self-test pins the *negatives*, because that is the whole argument: removing
each co-tenant in turn must keep failing to remove the stall. A data edit that
made any arm come out clean would turn the section into its opposite.
"""
import json
import pathlib
import statistics as st
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
RUNS = ROOT / "docs/data/p2-nvme-isolation-2026-08-19.jsonl"
DEVICE = ROOT / "docs/data/p2-nvme-isolation-device-2026-08-19.json"

PATHS = ("together", "split", "isolated")
FSES = ("ext4", "xfs")
STALL_MS = 50.0


def load():
    runs = [json.loads(x) for x in RUNS.read_text().splitlines() if x.strip()]
    dev = json.loads(DEVICE.read_text())
    for r in runs:
        r["jc_p99_ms"] = r["series"]["journal_commit_ms"]["p99_us"] / 1000
        r["fsync_max_ms"] = r["journal_stage"]["sync_data_ms_max"]
    return runs, dev


def cell(runs, fs, path):
    return [r for r in runs if r["fs"] == fs and r["path"] == path]


def fio_job(dev, name):
    for j in dev["fio"]:
        if j["job"] == name:
            return j
    raise KeyError(name)


def band(pop, key):
    v = [r[key] for r in pop]
    return min(v), max(v), st.median(v)


def report():
    runs, dev = load()
    h = dev["host"]
    print(f"docs/08-persistence.md §4.6 — {RUNS.name} + {DEVICE.name}")
    print(f"  {h['machine_type']} / {h['zone']}, {h['vcpu']} vCPU")
    print(f"  ext4: {h['ext4_device']}")
    print(f"  xfs:  {h['xfs_device']}")
    print(f"  {len(runs)} gate runs, {len(PATHS)} placements x {len(FSES)} filesystems x 6\n")

    print(f"  {'cell':<18}{'n':>3}{'jc p99 med':>12}{'range':>12}{'fsync med':>11}"
          f"{'range':>14}{'>2ms%':>8}{'>15ms%':>8}{'stalls>50':>10}{'us/flush':>10}")
    for fs in FSES:
        for p in PATHS:
            c = cell(runs, fs, p)
            q, f = band(c, "jc_p99_ms"), band(c, "fsync_max_ms")
            print(f"  {fs + '-' + p:<18}{len(c):>3}{q[2]:12.1f}{f'{q[0]:g}-{q[1]:g}':>12}"
                  f"{f[2]:11.1f}{f'{f[0]:.0f}-{f[1]:.0f}':>14}"
                  f"{st.median(r['journal_commit_tail']['pct_over_2ms'] for r in c):8.3f}"
                  f"{st.median(r['journal_commit_tail']['pct_over_15ms'] for r in c):8.3f}"
                  f"{sum(r['stalls_over_50ms'] for r in c):10d}"
                  f"{st.median(r['journal_stage']['sync_data_us_per_flush'] for r in c):10.1f}")
    print()

    print("  the elimination: removing each co-tenant in turn")
    for key, vals in (("path", PATHS), ("fs", FSES)):
        for v in vals:
            g = [r for r in runs if r[key] == v]
            f = band(g, "fsync_max_ms")
            clean = sum(1 for r in g if r["stalls_over_50ms"] == 0)
            print(f"    {key:<5} {v:<10} n={len(g):<3} worst fsync med {f[2]:6.1f} "
                  f"[{f[0]:6.1f}-{f[1]:6.1f}]   runs with no stall >{STALL_MS:.0f} ms: "
                  f"{clean} of {len(g)}")
    stalled = sum(1 for r in runs if r["stalls_over_50ms"] > 0)
    print(f"    -> {stalled} of {len(runs)} runs stalled; "
          f"{sum(r['stalls_over_50ms'] for r in runs)} stalls >{STALL_MS:.0f} ms, "
          f"{sum(r['stalls_over_90ms'] for r in runs)} >90 ms\n")

    print("  what the two filesystems do to the device, not to the gate")
    for name in ("fio-A-ext4", "fio-D-ext4", "fio-A-xfs", "fio-D-xfs"):
        j = fio_job(dev, name)
        print(f"    {name:<12} p99.9 {j['p99_9_ms']:8.3f}  p99.99 {j['p99_99_ms']:9.3f}  "
              f"max {j['max_ms']:9.3f}   {j['description'][:52]}")
    de, dx = fio_job(dev, "fio-D-ext4"), fio_job(dev, "fio-D-xfs")
    print(f"    -> xfs is {de['max_ms'] / dx['max_ms']:.0f}x more resistant to writeback "
          f"interference at the device, and stalls the gate anyway\n")

    fast = [r["journal_commit_tail"]["pct_at_or_below_0_5ms"] for r in runs]
    print(f"  fast mode (<= 0.5 ms): {min(fast):.2f}-{max(fast):.2f} % of every run")
    print(f"  recovery {sum(1 for r in runs if r['recovery']['pass'])} of {len(runs)} | "
          f"leases_lost max {max(r['client']['leases_lost'] for r in runs):.0f} | "
          f"nacks max {max(r['client']['diff_nacks'] for r in runs):.0f} | "
          f"acks {min(r['durable_acks'] for r in runs)}-{max(r['durable_acks'] for r in runs)}")
    print(f"  gate pass {sum(1 for r in runs if r['gate'] == 'pass')} of {len(runs)} | "
          f"area_first_page {sum(1 for r in runs if r['series']['area_first_page_ms']['gate'] == 'pass')} | "
          f"intent {sum(1 for r in runs if r['series']['intent_commit_ms']['gate'] == 'pass')}")


def self_test():
    runs, dev = load()
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    check("population", len(runs) == 36, f"{len(runs)} runs")
    for fs in FSES:
        for p in PATHS:
            check(f"cell {fs}-{p}", len(cell(runs, fs, p)) == 6,
                  f"{len(cell(runs, fs, p))} runs")

    # -- the layouts were verified per run, not assumed ---------------------
    for r in runs:
        want_out = "tmpfs" if r["path"] in ("split", "isolated") else r["fs"]
        check(f"layout {r['label']}", r["evidence_fs"] == want_out and r["journal_fs"] == r["fs"],
              f"evidence={r['evidence_fs']} journal={r['journal_fs']} for {r['cell']}")
    check("isolated arm put FDB on tmpfs",
          all(r.get("fdb_fs") == "tmpfs" for r in runs if r["path"] == "isolated"),
          "an isolated run did not record FoundationDB on tmpfs")

    # -- the argument: every removal fails to remove the stall --------------
    for p in PATHS:
        g = [r for r in runs if r["path"] == p]
        stalled = sum(1 for r in g if r["stalls_over_50ms"] > 0)
        check(f"{p} still stalls", stalled >= len(g) - 1,
              f"only {stalled} of {len(g)} {p} runs stalled -- this arm would be a fix, "
              f"and the section says none of them is")
    check("splitting the evidence path does not help",
          st.median([r["fsync_max_ms"] for r in runs if r["path"] == "split"])
          >= st.median([r["fsync_max_ms"] for r in runs if r["path"] == "together"]),
          "split now has a lower worst-fsync median than together, which would make "
          "§4.4's first follow-up a fix after all")
    check("isolating FoundationDB does not help",
          st.median([r["fsync_max_ms"] for r in runs if r["path"] == "isolated"])
          >= st.median([r["fsync_max_ms"] for r in runs if r["path"] == "together"]),
          "isolated now beats together, which would put the cause back outside persistd")
    check("no arm is clean", not any(
        all(r["stalls_over_50ms"] == 0 for r in cell(runs, fs, p)) for fs in FSES for p in PATHS),
        "some cell produced no stall at all in six runs")

    # -- the filesystem is not the mechanism either ------------------------
    for fs in FSES:
        g = [r for r in runs if r["fs"] == fs]
        check(f"{fs} stalls", sum(1 for r in g if r["stalls_over_50ms"] > 0) >= len(g) - 2,
              f"{fs} mostly stopped stalling")
    d_ext4, d_xfs = fio_job(dev, "fio-D-ext4"), fio_job(dev, "fio-D-xfs")
    check("xfs really is the writeback-resistant one at the device",
          d_xfs["max_ms"] < d_ext4["max_ms"] / 3,
          f"xfs jobD max {d_xfs['max_ms']} vs ext4 {d_ext4['max_ms']} -- the section's point is "
          f"that a filesystem which largely fixes the fio effect does not fix the gate")
    for name in ("fio-A-ext4", "fio-A-xfs"):
        check(f"{name} clears the device", fio_job(dev, name)["max_ms"] < 1.0,
              f"{name} max {fio_job(dev, name)['max_ms']} ms")
    # xfs helps the gated p99 even though it does not touch the stalls; the
    # section reports both, and losing either half misstates it.
    check("xfs improves the gated p99",
          st.median([r["jc_p99_ms"] for r in runs if r["fs"] == "xfs"])
          < st.median([r["jc_p99_ms"] for r in runs if r["fs"] == "ext4"]),
          "xfs no longer reads a lower journal_commit_ms p99 than ext4")
    check("xfs has the tighter per-flush cost",
          max(r["journal_stage"]["sync_data_us_per_flush"] for r in runs if r["fs"] == "xfs")
          < min(r["journal_stage"]["sync_data_us_per_flush"] for r in runs if r["fs"] == "ext4"),
          "the two filesystems' per-flush sync populations are no longer separated")

    # -- the body of the distribution, unchanged from §4.4 ------------------
    fast = [r["journal_commit_tail"]["pct_at_or_below_0_5ms"] for r in runs]
    check("the fast mode still holds", min(fast) > 90,
          f"a run put only {min(fast):.2f} % of commits at or below 0.5 ms")

    # -- durability, every run ---------------------------------------------
    check("recovery", all(r["recovery"]["pass"] for r in runs), "a run failed recovery")
    check("no lease lost", all(r["client"]["leases_lost"] == 0 for r in runs), "a run lost a lease")
    check("leases held", all(r["client"]["leases"] == 10000 for r in runs), "a run held fewer than 10 000")
    check("no diff nack", all(r["client"]["diff_nacks"] == 0 for r in runs), "a run was nacked")
    check("acks in family", all(539000 <= r["durable_acks"] <= 542000 for r in runs),
          "a run's durable ack count left the family")
    check("gate red throughout", all(r["gate"] == "fail" for r in runs), "a run passed the gate")
    check("area_first_page passes throughout",
          all(r["series"]["area_first_page_ms"]["gate"] == "pass" for r in runs),
          "a run failed area_first_page_ms")
    ip = sum(1 for r in runs if r["series"]["intent_commit_ms"]["gate"] == "pass")
    check("intent passes some but not all", 0 < ip < len(runs),
          f"{ip} of {len(runs)} passed intent_commit_ms")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(runs)} gate runs across {len(PATHS)} placements and "
          f"{len(FSES)} filesystems, every §4.6 claim holds against {RUNS.name}")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else (report() or 0))
