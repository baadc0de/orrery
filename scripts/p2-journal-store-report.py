#!/usr/bin/env python3
"""Every number in docs/08-persistence.md §4.8, derived from its data file.

§4.7 named P2's `journal_commit_ms` tail: fjall 3.1.9's `Batch::commit` calls
`local_backpressure()`, which sleeps in 100 ms steps while four or more sealed
memtables are queued. It left one question open, and it is a question about a
*second* store: is that pathology **fjall's**, or **an LSM's**?

`p2-journal-bench` drives fjall and RocksDB through the same write pattern the
journal produces -- same arrival process, same coalescing window and caps, same
monotonic keys, same two column families, one WAL fsync per batch -- on two
media and at two durations.

    python3 scripts/p2-journal-store-report.py             # the section's numbers
    python3 scripts/p2-journal-store-report.py --self-test # and its claims

The self-test pins the *asymmetry*, because that is the whole finding: fjall
stalling on tmpfs (where storage cannot be blamed) and RocksDB not, at both
durations. It also pins the two controls without which the comparison would be
worthless -- that both stores really fsync, and that the long runs wrote enough
to make the stores rotate.
"""
import json
import pathlib
import statistics as st
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
DATA = ROOT / "docs/data/p2-journal-store-2026-08-20.json"

STORES = ("fjall", "rocksdb", "wal-db")
MEDIA = ("nvme", "ram")
# D16 budgets the gated series at 2 ms p99; the bench measures the same barrier.
D16_P99_MS = 2.0


def load():
    return json.loads(DATA.read_text())


def cell(runs, store, medium, secs, leg=None):
    return [r for r in runs
            if r["store"] == store and r["medium"] == medium and r["seconds"] == secs
            and (leg is None or r["leg"] == leg)]


def med(rows, key):
    return st.median(r[key] for r in rows)


def report():
    d = load()
    runs = d["runs"]
    h = d["host"]
    print(f"docs/08-persistence.md §4.8 — {DATA.name}")
    print(f"  {h['machine_type']} / {h['zone']}")
    print(f"  nvme: {h['nvme']}")
    print(f"  ram:  {h['tmpfs']}")
    print(f"  fjall {d['versions']['fjall']} | rocksdb {d['versions']['rocksdb']} | "
          f"wal-db {d['versions']['wal-db']} — none tuned\n")

    for leg in ("two_store", "three_way"):
        sub = [r for r in runs if r["leg"] == leg]
        if not sub:
            continue
        print(f"  {leg}: {d['legs'][leg]}")
        for secs in sorted({r["seconds"] for r in sub}):
            recs = st.median(r["records"] for r in sub if r["seconds"] == secs)
            print(f"    --- {secs} s, {recs / 1e6:.2f} M records ---")
            print(f"    {'store':<9}{'medium':<7}{'n':>2}{'p50':>8}{'p99':>10}{'p99.9':>10}"
                  f"{'max':>10}{'slow':>6}{'worst ms':>10}")
            for medium in MEDIA:
                for store in STORES:
                    v = cell(sub, store, medium, secs)
                    if not v:
                        continue
                    flag = "  <= D16 2 ms p99" if med(v, "p99_ms") <= D16_P99_MS else ""
                    print(f"    {store:<9}{medium:<7}{len(v):>2}{med(v, 'p50_ms'):8.3f}"
                          f"{med(v, 'p99_ms'):10.3f}{med(v, 'p99_9_ms'):10.3f}"
                          f"{med(v, 'max_ms'):10.3f}{sum(r['slow_barriers'] for r in v):6d}"
                          f"{max(r['worst_ms'] for r in v):10.3f}{flag}")
        print()

    print("  stalls by store and medium, three-way leg")
    tw = [r for r in runs if r["leg"] == "three_way"]
    for store in STORES:
        per = {m: sum(r["slow_barriers"] for r in tw if r["store"] == store and r["medium"] == m)
               for m in MEDIA}
        print(f"    {store:<9} nvme {per['nvme']:>4}   tmpfs {per['ram']:>4}   "
              f"{'device-independent' if per['ram'] > 0.5 * max(per['nvme'], 1) else 'device-coupled'}")
    print()

    print("  controls: is the fsync real, and did the arms write comparable bytes?")
    for c in d["sync_control"]:
        print(f"    {c['store']:<8} {c['durability']:<22} mean barrier {c['mean_barrier_us']:7.1f} us   "
              f"on disk {c['on_disk_mb']:6.1f} MB")
    print()
    for k, v in d["caveats"].items():
        print(f"  [{k}] {v}")


def self_test():
    d = load()
    runs = d["runs"]
    fails = []

    def check(name, cond, detail):
        if not cond:
            fails.append(f"{name}: {detail}")

    tw = [r for r in runs if r["leg"] == "three_way"]
    check("three-way population", len(tw) == 12, f"{len(tw)} three-way runs")
    check("all three stores present", {r["store"] for r in tw} == set(STORES),
          f"stores {sorted({r['store'] for r in tw})}")
    long = max(r["seconds"] for r in tw)
    check("the long duration writes enough to rotate",
          st.median(r["records"] for r in tw) > 4_000_000,
          "the three-way leg no longer writes enough to force rotation, which is exactly how a "
          "store that stalls reports zero stalls")

    # -- the finding: fjall stalls where storage cannot be blamed ----------
    f_ram = [r for r in tw if r["store"] == "fjall" and r["medium"] == "ram"]
    f_nvme = [r for r in tw if r["store"] == "fjall" and r["medium"] == "nvme"]
    check("fjall stalls on tmpfs", sum(r["slow_barriers"] for r in f_ram) > 0,
          "fjall stopped stalling on tmpfs, which is §4.7's result and this section's premise")
    check("fjall is device-independent",
          sum(r["slow_barriers"] for r in f_ram) > 0.5 * sum(r["slow_barriers"] for r in f_nvme),
          "fjall's tmpfs and NVMe stall counts diverged, so the device is back in play")

    # -- the comparison, in the direction the section claims ---------------
    for store in ("rocksdb", "wal-db"):
        g_ram = [r for r in tw if r["store"] == store and r["medium"] == "ram"]
        check(f"{store} does not stall on tmpfs",
              sum(r["slow_barriers"] for r in g_ram) == 0,
              f"{store} logged {sum(r['slow_barriers'] for r in g_ram)} tmpfs stalls -- the "
              f"asymmetry against fjall is the whole finding")
        check(f"{store} stalls far less than fjall overall",
              sum(r["slow_barriers"] for r in tw if r["store"] == store)
              < 0.5 * sum(r["slow_barriers"] for r in tw if r["store"] == "fjall"),
              f"{store} no longer stalls markedly less than fjall")
        for medium in MEDIA:
            check(f"{store} holds the D16 p99 budget ({medium})",
                  med(cell(tw, store, medium, long), "p99_ms") <= D16_P99_MS,
                  f"{store} p99 on {medium} left the 2 ms budget")
    for medium in MEDIA:
        check(f"fjall misses the D16 p99 budget ({medium})",
              med(cell(tw, "fjall", medium, long), "p99_ms") > D16_P99_MS,
              "fjall's p99 now fits the 2 ms budget, which would make the comparison moot")

    # -- the honest halves the section must not lose ----------------------
    check("rocksdb still stalls on a real device",
          sum(r["slow_barriers"] for r in tw if r["store"] == "rocksdb" and r["medium"] == "nvme") > 0,
          "rocksdb never stalled on NVMe -- the section says it does, and losing that overstates it")
    check("wal-db is not claimed to be flawless",
          sum(r["slow_barriers"] for r in tw if r["store"] == "wal-db") > 0,
          "wal-db logged zero stalls everywhere; the section reports that it stalls on NVMe too, "
          "rarely, and a dataset without that would overstate the result")

    # -- controls, without which none of the above means anything ---------
    ctl = {(c["store"], c["durability"]): c for c in d["sync_control"]}
    for store in STORES:
        synced, buffered = ctl[(store, "fsync per batch")], ctl[(store, "buffered (control)")]
        check(f"{store} really fsyncs",
              synced["mean_barrier_us"] > 2 * buffered["mean_barrier_us"],
              f"{store} synced barrier {synced['mean_barrier_us']} us vs buffered "
              f"{buffered['mean_barrier_us']} us -- too close to prove the fsync happened")
    a, b = ctl[("fjall", "fsync per batch")], ctl[("rocksdb", "fsync per batch")]
    check("the two LSM arms wrote comparable bytes",
          abs(a["on_disk_mb"] - b["on_disk_mb"]) / max(a["on_disk_mb"], 1) < 0.2,
          f"fjall {a['on_disk_mb']} MB vs rocksdb {b['on_disk_mb']} MB -- an arm that wrote far "
          f"less cannot be compared on latency")
    w = ctl[("wal-db", "fsync per batch")]
    check("wal-db is recorded as writing materially less",
          w["on_disk_mb"] < 0.75 * a["on_disk_mb"],
          f"wal-db {w['on_disk_mb']} MB is no longer well below fjall's {a['on_disk_mb']} MB -- "
          f"that gap IS the no-index caveat, and the section leans on it")

    # -- the caveats are part of the claim, not decoration ----------------
    for key in ("not_tuned", "waldb_does_less", "journal_needs_more", "maturity"):
        check(f"caveat {key} recorded", d["caveats"].get(key), "caveat text missing")

    if fails:
        print("SELF-TEST FAILED")
        for f in fails:
            print("  " + f)
        return 1
    print(f"SELF-TEST PASSED: {len(runs)} bench runs ({len(tw)} three-way) across {len(STORES)} "
          f"stores and {len(MEDIA)} media, every §4.8 claim holds")
    return 0


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else (report() or 0))
