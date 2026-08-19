#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §2.2.5, derived from its data file.

The section measures one thing on the real rig: what taking `LeaseStore::locate`
off the renewal path (§2.2.4) did to the P2 kill-9 gate. The experiment is
`pre`/`post` `persistd` binaries interleaved run by run -- the arrangement
§2.2.2 established, because this box swings about twofold on per-flush fsync
cost on a tens-of-seconds scale and blocked arms would confound the arm with
the device.

    python3 scripts/p2-locate-removal-report.py             # the section's numbers
    python3 scripts/p2-locate-removal-report.py --self-test # hold them to the file
"""
import json
import pathlib
import statistics as st
import sys

DATA = pathlib.Path(__file__).resolve().parents[1] / "docs/data/p2-locate-removal-2026-08-19.jsonl"


def load():
    rows = [json.loads(line) for line in DATA.read_text().splitlines() if line.strip()]
    pre = [r for r in rows if r["arm"] == "pre"]
    post = [r for r in rows if r["arm"] == "post"]
    return rows, pre, post


def band(pop, path):
    vals = []
    for r in pop:
        d = r
        for p in path:
            d = d[p]
        vals.append(d)
    return min(vals), max(vals), st.median(vals)


def report():
    rows, pre, post = load()
    print(f"docs/08-persistence.md §2.2.5 — {DATA.name}")
    print(f"  {len(rows)} runs, {len(pre)} pre / {len(post)} post, interleaved\n")

    print("  run-by-run")
    print(f"    {'label':9s} {'GRV mean ms':>12} {'run-total GRV s':>16} "
          f"{'fsync max ms':>13} {'journal p99':>12} {'intent p99':>11} {'lost':>5}")
    for r in rows:
        s = r["stage"]
        print(f"    {r['label']:9s} {s['grv_mean_ms']:12.3f} {s['grv_total_s']:16.2f} "
              f"{r['journal_sync_max_ms']:13.1f} "
              f"{r['series']['journal_commit_ms']['p99_us']/1000:10.0f}ms "
              f"{r['series']['intent_commit_ms']['p99_us']/1000:9.0f}ms "
              f"{r['client']['leases_lost']:5.0f}")

    print("\n  what moved")
    for key, label in (("grv_mean_ms", "GRV mean (ms)"), ("grv_total_s", "run-total GRV (s)")):
        a = band(pre, ("stage", key))
        b = band(post, ("stage", key))
        disjoint = b[1] < a[0] or a[1] < b[0]
        print(f"    {label:20s} pre {a[0]:.3f}-{a[1]:.3f} (med {a[2]:.3f})  "
              f"post {b[0]:.3f}-{b[1]:.3f} (med {b[2]:.3f})  "
              f"{'disjoint' if disjoint else 'OVERLAPPING'}  "
              f"{100*(b[2]-a[2])/a[2]:+.1f}%")
    sa = band(pre, ("stage", "grv_mean_ms"))
    sb = band(post, ("stage", "grv_mean_ms"))
    print(f"    within-arm spread    pre {sa[1]/sa[0]:.2f}x  post {sb[1]/sb[0]:.2f}x")

    # Robustness: the one pair whose device states differ sharply.
    pre4 = [r for r in pre if r["label"] != "pre-r3"]
    post4 = [r for r in post if r["label"] != "post-r3"]
    a = band(pre4, ("stage", "grv_mean_ms"))
    b = band(post4, ("stage", "grv_mean_ms"))
    print(f"    excluding pair r3    pre {a[0]:.3f}-{a[1]:.3f}  post {b[0]:.3f}-{b[1]:.3f}  "
          f"{'disjoint' if b[1] < a[0] else 'OVERLAPPING'}  {100*(b[2]-a[2])/a[2]:+.1f}%")

    print("\n  what did not move")
    fp = band(pre, ("journal_sync_max_ms",))
    fo = band(post, ("journal_sync_max_ms",))
    print(f"    fsync max (device)   pre {fp[0]:.1f}-{fp[1]:.1f} (med {fp[2]:.1f})  "
          f"post {fo[0]:.1f}-{fo[1]:.1f} (med {fo[2]:.1f})")
    ep = band(pre, ("stage", "executed"))
    eo = band(post, ("stage", "executed"))
    print(f"    intents executed     pre {ep[0]:.0f}-{ep[1]:.0f}  post {eo[0]:.0f}-{eo[1]:.0f}")
    print(f"    gate verdict         {sum(1 for r in rows if r['gate'] == 'fail')} of {len(rows)} fail")
    print(f"    root cause           journal_commit_ms in "
          f"{sum(1 for r in rows if r['root_causes'] == ['journal_commit_ms'])} of {len(rows)}")
    ack = band(rows, ("durable_acks",))
    print(f"    durable acks         {ack[0]:.0f}-{ack[1]:.0f}")

    print("\n  durability, every run")
    print(f"    leases held 10 000   {sum(1 for r in rows if r['client']['leases'] == 10000)} of {len(rows)}")
    print(f"    leases_lost 0        {sum(1 for r in rows if r['client']['leases_lost'] == 0)} of {len(rows)}")
    print(f"    diff nacks 0         {sum(1 for r in rows if r['client']['diff_nacks'] == 0)} of {len(rows)}")
    print(f"    duplicate durable 0  {sum(1 for r in rows if r['client']['duplicate_durable_acks'] == 0)} of {len(rows)}")
    print(f"    recovery verified    {sum(1 for r in rows if r['recovery']['pass'])} of {len(rows)}")


def self_test():
    """Hold the section's load-bearing claims to the data file."""
    rows, pre, post = load()
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    check("population", len(pre) == 5 and len(post) == 5, f"{len(pre)}/{len(post)}")
    for key in ("grv_mean_ms", "grv_total_s"):
        a = band(pre, ("stage", key))
        b = band(post, ("stage", key))
        check(f"{key} disjoint", b[1] < a[0], f"pre {a[0]:.3f}-{a[1]:.3f} post {b[0]:.3f}-{b[1]:.3f}")
        check(f"{key} direction", b[2] < a[2], "post median is not below pre median")
    a = band(pre, ("stage", "grv_mean_ms"))
    b = band(post, ("stage", "grv_mean_ms"))
    delta = 100 * (b[2] - a[2]) / a[2]
    check("grv delta ~-15.7%", -17.0 < delta < -14.0, f"{delta:+.1f}%")
    # Robustness without the pair whose device states differ sharply.
    a4 = band([r for r in pre if r["label"] != "pre-r3"], ("stage", "grv_mean_ms"))
    b4 = band([r for r in post if r["label"] != "post-r3"], ("stage", "grv_mean_ms"))
    check("disjoint without r3", b4[1] < a4[0], f"pre {a4[0]:.3f}-{a4[1]:.3f} post {b4[0]:.3f}-{b4[1]:.3f}")
    # The verdict the section says did not move.
    check("gate red in all", all(r["gate"] == "fail" for r in rows), "a run passed")
    check("one root cause", all(r["root_causes"] == ["journal_commit_ms"] for r in rows), "root cause moved")
    # The durability properties the runs exist to exercise.
    check("leases held", all(r["client"]["leases"] == 10000 for r in rows), "a run held fewer than 10 000")
    check("no lease lost", all(r["client"]["leases_lost"] == 0 for r in rows), "a run lost a lease")
    check("no diff nack", all(r["client"]["diff_nacks"] == 0 for r in rows), "a run nacked a diff")
    check("recovery", all(r["recovery"]["pass"] for r in rows), "a run failed recovery")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(rows)} runs, every §2.2.5 claim holds against {DATA.name}")
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    report()
