#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §2.2.7, derived from its data file.

The section measures one change on the real rig: the intent path's ownership
fence stopped reading its 128 shard rows as 128 point reads and started reading
them as one range read. Five `pre`/`post` pairs, interleaved run by run.

    python3 scripts/p2-intent-fence-report.py             # the section's numbers
    python3 scripts/p2-intent-fence-report.py --self-test # hold them to the file
"""
import json
import pathlib
import statistics as st
import sys

DATA = pathlib.Path(__file__).resolve().parents[1] / "docs/data/p2-intent-fence-2026-08-19.jsonl"


def load():
    rows = [json.loads(line) for line in DATA.read_text().splitlines() if line.strip()]
    for r in rows:
        t = r["intent_stage_all"]
        ex = max(t.get("executed", 1), 1)
        r["m"] = {
            "reads": t.get("fence_reads", 0) / ex,
            "fence_ms": t.get("fence_us_sum", 0) / ex / 1000,
            "commit_ms": t.get("commit_us_sum", 0) / ex / 1000,
            "server_ms": t.get("server_us_sum", 0) / ex / 1000,
            "fsync_ms": r["journal_sync_max_ms"],
            "intent_p99_ms": r["series"]["intent_commit_ms"]["p99_us"] / 1000,
            "journal_p99_ms": r["series"]["journal_commit_ms"]["p99_us"] / 1000,
        }
    return rows, [r for r in rows if r["arm"] == "pre"], [r for r in rows if r["arm"] == "post"]


def band(pop, key):
    v = [r["m"][key] for r in pop]
    return min(v), max(v), st.median(v)


METRICS = [
    ("reads", "fence reads/intent"),
    ("fence_ms", "fence stage (ms)"),
    ("commit_ms", "commit stage (ms)"),
    ("server_ms", "intent server span (ms)"),
    ("fsync_ms", "worst journal fsync (ms)"),
    ("journal_p99_ms", "journal_commit_ms p99 (ms)"),
    ("intent_p99_ms", "intent_commit_ms p99 (ms)"),
]


def report():
    rows, pre, post = load()
    print(f"docs/08-persistence.md §2.2.7 — {DATA.name}")
    print(f"  {len(rows)} runs, {len(pre)} pre / {len(post)} post, interleaved\n")
    print(f"  {'run':9} {'reads':>7} {'fence':>7} {'commit':>7} {'server':>7} {'fsync':>7} {'j_p99':>6} {'i_p99':>6} {'gate':>5}")
    for r in rows:
        m = r["m"]
        print(
            f"  {r['label']:9} {m['reads']:7.1f} {m['fence_ms']:7.3f} {m['commit_ms']:7.3f} "
            f"{m['server_ms']:7.3f} {m['fsync_ms']:7.1f} {m['journal_p99_ms']:6.0f} "
            f"{m['intent_p99_ms']:6.0f} {r['series']['intent_commit_ms']['gate']:>5}"
        )
    print()
    for key, label in METRICS:
        a, b = band(pre, key), band(post, key)
        disjoint = b[1] < a[0] or a[1] < b[0]
        print(
            f"  {label:28} pre {a[0]:8.2f}-{a[1]:8.2f} (med {a[2]:7.2f})  "
            f"post {b[0]:7.2f}-{b[1]:7.2f} (med {b[2]:6.2f})  "
            f"{'disjoint' if disjoint else 'overlap '}  {100 * (b[2] - a[2]) / a[2]:+6.1f}%"
        )
    print()
    passes = [r["label"] for r in rows if r["series"]["intent_commit_ms"]["gate"] == "pass"]
    print(f"  intent_commit_ms passes: {len(passes)} of {len(rows)} ({', '.join(passes) or 'none'})")
    print(f"  leases_lost max {max(r['client']['leases_lost'] for r in rows):.0f} | "
          f"recovery {sum(1 for r in rows if r['recovery']['pass'])} of {len(rows)} | "
          f"acks {min(r['durable_acks'] for r in rows)}-{max(r['durable_acks'] for r in rows)}")


def self_test():
    rows, pre, post = load()
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    check("population", len(pre) == 5 and len(post) == 5, f"{len(pre)}/{len(post)}")
    # The change itself, by construction.
    check("pre reads 128", all(abs(r["m"]["reads"] - 128.0) < 0.5 for r in pre), "a pre run did not read 128")
    check("post reads 1", all(abs(r["m"]["reads"] - 1.0) < 0.1 for r in post), "a post run did not read 1")
    # The direct effects the section states as established.
    for key in ("fence_ms", "server_ms"):
        a, b = band(pre, key), band(post, key)
        check(f"{key} disjoint", b[1] < a[0], f"pre {a[0]:.3f}-{a[1]:.3f} post {b[0]:.3f}-{b[1]:.3f}")
    # The device separation the section states as a *hypothesis*, not a finding.
    # It is still pinned, because if it stops holding the section's hedge stops
    # being about this data.
    a, b = band(pre, "fsync_ms"), band(post, "fsync_ms")
    check("fsync separated", b[1] < a[0], f"pre {a[0]:.1f}-{a[1]:.1f} post {b[0]:.1f}-{b[1]:.1f}")
    # The claim the section is most at risk of overstating.
    passes = [r for r in rows if r["series"]["intent_commit_ms"]["gate"] == "pass"]
    check("exactly one pass", len(passes) == 1, f"{len(passes)} runs passed intent_commit_ms")
    check("the pass is post", passes and passes[0]["arm"] == "post", "the pass was not a post run")
    check("post is not a passing gate", sum(1 for r in post if r["series"]["intent_commit_ms"]["gate"] == "pass") < len(post),
          "every post run passed, which would make the section's hedge wrong in the other direction")
    # Durability, every run.
    check("no lease lost", all(r["client"]["leases_lost"] == 0 for r in rows), "a run lost a lease")
    check("leases held", all(r["client"]["leases"] == 10000 for r in rows), "a run held fewer than 10 000")
    check("recovery", all(r["recovery"]["pass"] for r in rows), "a run failed recovery")
    check("delivered load equal", max(r["intent_stage_all"]["executed"] for r in rows)
          - min(r["intent_stage_all"]["executed"] for r in rows) < 200, "intent counts diverged between arms")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(rows)} runs, every §2.2.7 claim holds against {DATA.name}")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else (report() or 0))
