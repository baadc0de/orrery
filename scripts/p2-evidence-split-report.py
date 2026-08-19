#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §4.5, derived from its data files.

§4.4 found the gate's own evidence files on the journal's filesystem and asked
for them to be moved off it. `P2_GATE_DATA_DIR` makes that possible; this is
the measurement of what it bought on the *reference* box, and the answer is
nothing detectable -- for a reason the same data explains.

    python3 scripts/p2-evidence-split-report.py             # the section's numbers
    python3 scripts/p2-evidence-split-report.py --self-test # hold them to the files
"""
import json
import pathlib
import statistics as st
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
RUNS = ROOT / "docs/data/p2-evidence-split-2026-08-19.jsonl"
DEV = ROOT / "docs/data/p2-evidence-split-device-2026-08-19.json"

# docs/08 §4.4's own fio numbers on the NVMe box, for the comparison that is
# the point of this section. Structural: quoted from that section, not measured
# here.
NVME = {"jobA": {"p99_9_ms": 0.226, "p99_99_ms": 0.317, "max_ms": 0.460},
        "jobD": {"p99_9_ms": 0.247, "p99_99_ms": 119.013, "max_ms": 303.377}}


def load():
    runs = [json.loads(l) for l in RUNS.read_text().splitlines() if l.strip()]
    dev = json.loads(DEV.read_text())
    return runs, dev, [r for r in runs if r["arm"] == "split"], [r for r in runs if r["arm"] == "together"]


def p99(r, series):
    return r["series"][series]["p99_us"] / 1000


def report():
    runs, dev, split, together = load()
    print(f"docs/08-persistence.md §4.5 — {RUNS.name} + {DEV.name}")
    print(f"  {len(runs)} gate runs, {len(split)} split / {len(together)} together, interleaved\n")

    print("  per pair, journal_commit_ms p99 (ms)")
    print(f"    {'pair':6} {'split':>8} {'together':>10}   winner")
    wins = {"split": 0, "together": 0, "tie": 0}
    for n in range(1, len(split) + 1):
        s = next(r for r in split if r["label"].endswith(f"r{n}"))
        t = next(r for r in together if r["label"].endswith(f"r{n}"))
        a, b = p99(s, "journal_commit_ms"), p99(t, "journal_commit_ms")
        w = "split" if a < b else ("together" if b < a else "tie")
        wins[w] += 1
        print(f"    r{n:<5} {a:8.1f} {b:10.1f}   {w}")
    print(f"    -> split {wins['split']}, together {wins['together']}, tie {wins['tie']}")

    print("\n  discrete stall events (steps in the running sync_data max)")
    for key, thr in (("gt20", 20), ("gt50", 50), ("gt90", 90)):
        a = sum(len(r["stall_steps_ms"][key]) for r in split)
        b = sum(len(r["stall_steps_ms"][key]) for r in together)
        print(f"    > {thr:3d} ms:  split {a:3d}   together {b:3d}")

    print("\n  the barrier on this box, and on §4.4's")
    print(f"    {'job':22} {'p99.9':>9} {'p99.99':>10} {'max':>10}")
    for name, label in (("jobA", "md2, no writer"), ("jobD", "md2, +5 MB/s buffered")):
        d = dev[name]
        print(f"    {label:22} {d['p99_9_ms']:9.3f} {d['p99_99_ms']:10.3f} {d['max_ms']:10.3f}")
    for name, label in (("jobA", "NVMe, no writer (§4.4)"), ("jobD", "NVMe, +5 MB/s (§4.4)")):
        d = NVME[name]
        print(f"    {label:22} {d['p99_9_ms']:9.3f} {d['p99_99_ms']:10.3f} {d['max_ms']:10.3f}")

    stalls = [x for r in runs for x in r["stall_steps_ms"]["gt20"]]
    print(f"\n  gate stalls observed here: {min(stalls):.0f}-{max(stalls):.0f} ms "
          f"(n={len(stalls)} across {len(runs)} runs)")
    print(f"  md2's bare barrier max, same shape, nothing else running: {dev['jobA']['max_ms']:.2f} ms")
    print("  -> signal and noise are the same size on this box; §4.4's box separates them by 170x")

    print("\n  durability, every run")
    print(f"    leases_lost max {max(r['client']['leases_lost'] for r in runs):.0f} | "
          f"recovery {sum(1 for r in runs if r['recovery']['pass'])} of {len(runs)} | "
          f"acks {min(r['durable_acks'] for r in runs)}-{max(r['durable_acks'] for r in runs)}")


def self_test():
    runs, dev, split, together = load()
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    check("population", len(split) == 5 and len(together) == 5, f"{len(split)}/{len(together)}")

    # The null. This is the section's headline and the thing an edit is most
    # likely to quietly turn into a result.
    wins = {"split": 0, "together": 0, "tie": 0}
    for n in range(1, 6):
        s = next(r for r in split if r["label"].endswith(f"r{n}"))
        t = next(r for r in together if r["label"].endswith(f"r{n}"))
        a, b = p99(s, "journal_commit_ms"), p99(t, "journal_commit_ms")
        wins["split" if a < b else ("together" if b < a else "tie")] += 1
    check("the result is a null", wins["split"] == wins["together"],
          f"split {wins['split']} together {wins['together']} tie {wins['tie']}")
    a = [p99(r, "journal_commit_ms") for r in split]
    b = [p99(r, "journal_commit_ms") for r in together]
    check("ranges overlap", not (max(a) < min(b) or max(b) < min(a)),
          f"split {min(a)}-{max(a)} together {min(b)}-{max(b)}")

    # The reason the null is uninformative rather than merely negative: on this
    # box the device's own tail is the size of the effect. If that stops being
    # true the section's conclusion no longer follows from its data.
    stalls = [x for r in runs for x in r["stall_steps_ms"]["gt20"]]
    check("stalls observed", len(stalls) >= 10, f"only {len(stalls)} stall events")
    check("bare barrier is the same magnitude as the gate's stalls",
          dev["jobA"]["max_ms"] > 0.5 * st.median(stalls),
          f"jobA max {dev['jobA']['max_ms']:.2f} ms vs median stall {st.median(stalls):.2f} ms")
    check("this box cannot separate them", dev["jobA"]["max_ms"] > 20 * NVME["jobA"]["max_ms"],
          f"md2 jobA max {dev['jobA']['max_ms']:.2f} vs NVMe {NVME['jobA']['max_ms']:.2f}")
    # Writeback is still visible here, which is what stops this being a
    # refutation of §4.4's mechanism.
    check("writeback still moves the tail here",
          dev["jobD"]["p99_99_ms"] > 2 * dev["jobA"]["p99_99_ms"],
          f"A p99.99 {dev['jobA']['p99_99_ms']:.2f} vs D {dev['jobD']['p99_99_ms']:.2f}")

    check("no lease lost", all(r["client"]["leases_lost"] == 0 for r in runs), "a run lost a lease")
    check("recovery", all(r["recovery"]["pass"] for r in runs), "a run failed recovery")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(runs)} runs, every §4.5 claim holds against {RUNS.name}")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else (report() or 0))
