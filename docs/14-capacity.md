# 14 — Capacity of a single box

How much load one machine running `persistd` and FoundationDB absorbs before it
stops keeping up, which resource runs out first, and what that is worth in
entities and players. This is a **measurement** document: every number below
came off one box on 2026-08-18 with the harness in
[`scripts/p2-capacity-sweep.sh`](../scripts/p2-capacity-sweep.sh) and the
reducer in [`scripts/p2-capacity-report.py`](../scripts/p2-capacity-report.py),
and every one is reproducible by re-running them. It sizes demo environments;
it is not a sizing model for the cluster, which is
[08-persistence.md](08-persistence.md) §13.

The short answer: **the box stops keeping up at about 40 000 offered bulk
records/s, and what runs out is one thread** — the FoundationDB client's
network thread inside `persistd`, because every fenced bulk diff does an FDB
read. Fifteen of the box's sixteen threads, the NVMe array, the NIC, FDB's own
server process and RAM are all still idle at that point.

## 1. The box, and what was on it

| | |
|---|---|
| CPU | AMD Ryzen 7 7700, 8 cores / 16 threads |
| RAM | 62 GB |
| NIC | 1 Gbit `eno1` (unused — see §7) |
| Disk | root on `md2`, RAID1 of two consumer QLC NVMe (Solidigm P41 Plus), no power-loss protection |
| Under test | one `persistd` primary (128 shards, `--fdb-cluster-file`) + one passive chain follower, both on this host, both journaling to `md2` |
| FDB | one `fdbserver` 7.3.63 in a throwaway container, `configure single memory` |
| Rig | `p2-load` on the same box, driving a 10 000-entity seeded world (`p2demo`, profile `demo`) |

Each point is a 30 s run against a freshly cleared and re-seeded cluster.
Configurations were interleaved rather than run in blocks, and every one was
run at least twice; the tables report **min–max across repeats**, not medians,
because two runs of identical code on this box differ by up to 2× on
per-flush fsync cost (§7).

## 2. What "stops keeping up" means here

Stated before the data, and checkable in any run's own output:

1. **`shed_slow_route / admitted ≤ 1 %`.** The gateway drops a bulk diff whose
   age exceeds a 25 ms route-admission budget (`MAX_ROUTE_ADMISSION_WAIT_US`,
   `gateway.rs`) and counts it. It is a shed valve, not a rate limit: on a
   gateway that is keeping up it does not fire. This is the sharpest signal
   the system produces about itself.
2. **The durable ack rate still rises when offered load rises.** Measured as
   `durable_acks / duration` against `entities × diff_hz`.
3. **`intent_commit_ms` p99 ≤ 100 ms** (10× its D16 target). Intents ride the
   same FDB client as the bulk path's lease locate (§5), so a bulk queue shows
   up here first.
4. **No lease withdrawn mid-run** (`leases_lost = 0`).

Offered load is `entities × diff_hz` — what the world would generate if nothing
throttled it. It is deliberately **not** `diffs_sent`, which the rig's uplink
scheduler caps and which re-counts a shed diff when the client re-offers it.

## 3. The sweep

10 000 entities, 128 shards, 30 s, `--intent-mix trade=0.02,craft=0.01`
throughout. `p2-load`'s fan-out assert requires
`sessions × 160 ≥ entities × diff_hz`, so the rate leg has to raise sessions
with rate; the concurrency leg holds the rate at 2 Hz and raises sessions
alone, and the two legs share their session counts so the effects separate.

### 3.1 Rate leg (offered load rising)

| offered/s | hz | sessions | durable acks/s | shed % | intent p99 | busiest thread (mean / peak) | persistd cores | keeping up? |
|---|---|---|---|---|---|---|---|---|
| 20 000 | 2 | 125 | 17 982–18 039 | 0.00–0.01 | 15–20 ms | 25.8 % / 43–45 % | 1.31–1.36 | yes |
| 30 000 | 3 | 250 | 27 622–27 681 | 1.94–2.15 | 100–150 ms | 35.3–35.4 % / 41–47 % | 1.85–1.95 | marginal |
| **40 000** | **4** | **250** | **33 515–33 692** | **0.83–1.38** | **50–75 ms** | **40.3–40.4 % / 46–47 %** | **2.15–2.28** | **knee** |
| 60 000 | 6 | 500 | 46 382–46 858 | 5.66–6.67 | 500 ms | 56.5–57.9 % / 67–69 % | 3.01–3.19 | no |
| 80 000 | 8 | 500 | 58 610–60 022 | 9.48–11.59 | 0.75–1.0 s | 73.8–75.2 % / 99 % | 3.79–4.00 | no (peak throughput) |
| 120 000 | 12 | 750 | 24 901–28 160 | 69.3–72.6 | 2–3 s | 97.5–98.6 % / 100 % | 4.09–4.14 | collapsed |
| 160 000 | 16 | 1000 | 19 688–22 590 | 75.6–78.6 | 3 s | 98.2–98.4 % / 100 % | 4.04–4.22 | collapsed |
| 320 000 | 32 | 2000 | **33–64** | 99.95–99.98 | — | 90.0–90.4 % / 99 % | 3.62–3.71 | livelocked |

Two knees, and they are different events:

* **Service knee at 40 000 offered / ~33.6 k durable records/s.** The last
  configuration where shedding is ~1 %, intents commit inside 100 ms and the
  ack rate still climbs. This is the number to size against.
* **Throughput turnover between 80 000 and 120 000 offered.** Peak achieved
  throughput is **59 k durable records/s** at 80 000 offered, but that point is
  already shedding 10 % of writes and taking a second to commit an intent.
  Past it the box does not plateau, it **collapses**: 120 000 offered yields
  *less* than 40 000 offered did, and at 320 000 offered the box makes 33–64
  records durable per second — a factor of 300 below its own baseline.

The collapse is not a measurement artifact. At 320 000 offered, FDB reports
82 639 transactions *started* per second and 1 657 reads/s served, against 33
durable acks/s: `persistd` is spending its entire FDB budget on transactions
that time out and retry.

### 3.2 Concurrency leg (sessions rising, rate fixed at 2 Hz)

| sessions | durable acks/s | shed % | `LeaseStore::locate` per apply | intent p99 | persistd cores | rig cores |
|---|---|---|---|---|---|---|
| 125 | 17 982–18 039 | 0.00–0.01 | 0.48–0.50 ms | 15–20 ms | 1.31–1.36 | 0.53–0.54 |
| 250 | 18 435–18 470 | 0.00–0.04 | 0.76–0.80 ms | 15–75 ms | 1.29–1.31 | 0.53–0.55 |
| 500 | 18 544–18 572 | 0.06–0.10 | 1.32–1.35 ms | 40 ms | 1.35–1.38 | 0.61–0.62 |
| 1000 | 18 431–18 497 | 0.55–0.97 | 1.73–1.99 ms | 40–75 ms | 1.39–1.45 | 0.75–0.80 |
| **2000** | 17 188–17 308 | **7.25–7.87** | 2.68–2.79 ms | 150–200 ms | 1.50–1.56 | 1.00–1.06 |

**Concurrency alone does not break the box up to 1000 connections.** Throughput
is flat within 3 % from 125 to 1000 sessions; what grows is the lease-locate
latency (4× for 8× the sessions) and, with it, the shed rate. The knee in this
direction is between 1000 and 2000 sessions: at 2000, 7.3–7.9 % of writes are
shed and throughput has fallen 4 % below the 125-session baseline, at the same
offered load.

That also settles what killed the 32 Hz / 2000-session point: it was the rate.
2000 sessions at 2 Hz is degraded but functional; 2 Hz at 2000 sessions is not
what livelocks the box.

No run in the whole sweep lost a lease. `leases_lost = 0` everywhere, including
the fully collapsed points — the registrar holds even when the write path does
not, because heartbeats ride their own lane.

## 4. CPU, attributed per process

`p2-load` runs on the same 16 threads as the thing it measures, which a real
deployment would not, so it is measured separately (`pidstat -u -h -p` on all
four PIDs, one sample/s, first 5 s dropped for the lease-claim phase). Cores =
`%CPU / 100`.

| point | persistd primary | chain follower | fdbserver | **subtotal (the deployment)** | p2-load (the rig) | box total |
|---|---|---|---|---|---|---|
| baseline, 20 k offered | 1.31–1.36 | 0.22–0.24 | 0.15–0.16 | **1.68–1.76** | 0.53–0.54 | 2.2–2.3 of 16 |
| knee, 40 k offered | 2.15–2.28 | 0.31–0.34 | 0.21–0.22 | **2.67–2.84** | 0.89–0.95 | 3.6–3.8 of 16 |
| peak, 80 k offered | 3.79–4.00 | 0.31–0.34 | 0.30–0.32 | **4.40–4.66** | 1.74–1.84 | 6.1–6.5 of 16 |
| collapsed, 160 k offered | 4.04–4.22 | 0.28–0.34 | 0.16–0.17 | **4.48–4.73** | 2.33–2.41 | 6.8–7.1 of 16 |

**Headroom after subtracting the rig: at the knee the deployment uses 2.7–2.8
of 16 threads — 17 % of the box. Thirteen threads are idle.** They stay idle at
the collapse: the fully wedged box is using 4.5–4.7 cores for persistence.

**Does the knee move with fewer rig threads?** No — this was measured, not
assumed. The 80 000-offered point was re-run with the rig pinned to four CPUs
and to two (`taskset`, `P2_CAP_LOAD_CPUS`):

| rig CPUs | rig cores used | durable acks/s | shed % |
|---|---|---|---|
| all 16 | 1.74–1.84 | 58 610–60 022 | 9.48–11.59 |
| 4 (`0-3`) | 1.29–1.32 | 58 853–59 355 | 10.49–11.22 |
| 2 (`0-1`) | 1.01–1.05 | 58 638–59 101 | 10.86–11.57 |

Cutting the rig's CPU by 45 % changed delivered throughput by under 1.2 %. The
knee is the box's, not the rig's.

## 5. What binds first

Ranked, with the measurement for each.

### 5.1 The FoundationDB client's network thread inside `persistd` — the binding constraint

`libfdb_c` runs **one** network thread per process, and `persistd` puts an FDB
transaction on it for **every fenced bulk diff**: `CellRuntime::apply_fenced`
calls `LeaseStore::locate`, whose FDB implementation is a `db.run` doing a
single `get` of `lease_location_key` (`lease/fdb.rs`). Bulk writes are supposed
to reach FDB only at the 20 s checkpoint ([08-persistence.md](08-persistence.md)
§2) — on the fenced path they take an FDB round trip per record.

That the busy thread is FDB's is not inferred from its name. `persistd` threads
were enumerated from `/proc/<pid>/task/*/comm` during a run:

```
node-id 1 (primary, --fdb-cluster-file):  16 tokio-rt-worker, 4 fjall:worker,
                                           1 journal-committ, 2 persistd
node-id 2 (follower, no FDB):              16 tokio-rt-worker, 4 fjall:worker,
                                           1 journal-committ, 1 persistd
```

The primary has one thread the follower does not, it appears only with
`--fdb-cluster-file`, it inherits the process's `comm`, and it is the busiest
thread in the process by a factor of four (`pidstat -t`).

Its utilisation is monotone in offered load and is the cleanest capacity signal
on the box:

| durable acks/s | FDB client thread, mean | peak | `locate` per apply |
|---|---|---|---|
| 17 982–18 039 | 25.8 % | 43–45 % | 0.48–0.50 ms |
| 27 622–27 681 | 35.3–35.4 % | 41–47 % | 0.93–0.96 ms |
| 33 515–33 692 | 40.3–40.4 % | 46–47 % | 0.97–1.05 ms |
| 46 382–46 858 | 56.5–57.9 % | 67–69 % | 1.90–1.92 ms |
| 58 610–60 022 | 73.8–75.2 % | **99 %** | 2.31–2.55 ms |
| 24 901–28 160 (collapsed) | 97.5–98.6 % | **100 %** | 13.3–13.8 ms |

Linear through the first five rows, the thread reaches 100 % at **~80 000
transactions/s** — and FDB independently reports 82 647 transactions started/s
at the livelocked point, which is the same ceiling seen from the other side.
The locate latency follows the queueing curve that utilisation implies: ×1 at
26 % busy, ×5 at 75 %, ×29 at 98 %.

**One thread of sixteen is the whole capacity of this box.**

### 5.2 The 25 ms route-admission valve — the symptom, and the collapse mechanism

`shed_slow_route` is how the queue behind §5.1 becomes lost writes:
0.0 % → 1.4 % → 6.7 % → 11.6 % → 78.6 % → 99.98 % across the rate leg. The
valve itself is correct and is doing its job — the client holds the diff and
re-offers it — but re-offering multiplies the offered load, which lengthens the
queue, which sheds more. That positive feedback is why the box collapses past
80 000 offered instead of plateauing.

Note the valve sheds *before* the apply, so it is not wasting locate work:
`applies/s` equals `durable acks/s` exactly at every point up to 80 000 offered
(7–9 % of applies are wasted at 120 000–160 000). This is the retained bound
that `gateway.rs` says was "retained as a bound for workloads that study did
not run" — this is that workload, and the bound is load-bearing.

### 5.3–5.7 Everything else, with slack at the knee

3. **`persistd` CPU as a whole** — 2.15–2.28 cores at the knee, never above
   4.22. 26 % of the box at full collapse.
4. **The journal and the device** — *not* binding, and the evidence is direct.
   At the knee the journal takes 4.67–4.69 MB/s at 379–674 flush/s and 50–89
   records per flush, against the 8192-record flush cap (**1 %** of it). Two
   repeats of the 80 000-offered point ran at 360 and 775 flushes/s — the
   device's two regimes, a 2× difference in fsync cost — and delivered 59 100
   and 59 041 durable acks/s, a difference of **0.1 %**. The device's fsync
   tail owns `journal_commit_ms` and `bulk_ack_ms`
   ([08-persistence.md](08-persistence.md) §4.3); it does not own capacity.
5. **FoundationDB the server** — 0.13–0.32 cores at every point, 0.13–0.27 ms
   reads, **zero** conflicts in every run (two points saw 0.03–0.09 conflicts/s,
   i.e. one or two in 30 s), GRV latency 0.1 ms (1.1 ms in one sample). Commit latency 1.5–9.8 ms,
   rising to 17.6–22.8 ms only in the livelocked runs. FDB is idle while its
   *client* is saturated.
6. **Memory** — `persistd` RSS 407 MB at baseline, 530–575 MB at 20 000–30 000 offered, 1.34–1.37 GB at 160 000 offered, and 9.3–9.7 GB at the
   livelocked 320 000 point. Never binding on 62 GB, but the growth is the
   backlog: RSS above ~1 GB for a 10 000-entity world means the box is queueing,
   not working.
7. **The 1 Gbit NIC** — carried nothing in this sweep (see §7) and would have
   carried ~34 Mbps at the knee, ~61 Mbps at peak. 3–7 % of the link.

Ranked by when they run out: **FDB client thread (100 %) ≫ persistd CPU (26 %)
≈ journal flush rate (device-bound but load-insensitive) ≫ FDB server CPU
(2 %) ≫ NIC (7 %) ≫ RAM (2 %)**.

## 6. Converting to game units

Two different quantities, converted separately. **The rig's 125 sessions × 80
entities is not a player:entity ratio** — a `p2-load` session is a flush-budget
slot (1024 B / 20 Hz), not a player. A real player owns ~1 avatar entity (plus
1–2 authored core entities) and *observes* ~24 (D16's bounded high-rate
interest set).

### 6.1 Entities

Persistence load is `entities × diff_hz`, so the rate sweep at a fixed 10 000
entities is a proxy for a larger world at a lower per-entity rate. Against the
40 000 records/s service knee and the 80 000 offered turnover:

| per-entity diff rate | entities at the knee | entities at turnover (degraded) |
|---|---|---|
| 1 Hz (D16 floor) | 40 000 | 80 000 |
| **2 Hz** (the P2 operating point) | **20 000** | 40 000 |
| 4 Hz (D16 ceiling for bulk uplink) | 10 000 | 20 000 |
| 10 Hz | 4 000 | 8 000 |
| 20 Hz (D16 *send* rate) | 2 000 | 4 000 |

The 10 Hz and 20 Hz rows are hypothetical for persistence: D16 puts the bulk
uplink at **1–4 Hz per entity, priority-scheduled**, and 20 Hz is the network
send rate, not the durability rate. They are given because a game that pushed
every entity at frame rate into the journal would land there.

### 6.2 Players

Three independent routes, and they agree to within 20 %.

**By the repo's own sizing basis** ([08-persistence.md](08-persistence.md) §13:
10 diff records/s per player, 4 hot world entities + 1–2 authored core entities
per player):

* knee: 40 000 ÷ 10 = **4 000 players**, implying 20 000–24 000 entities —
  which is the 2 Hz row of §6.1, independently.
* turnover: 8 000 players, at 10 % shed and 1 s intent commits. Not a place to
  run a demo.

**By the NIC and the AOI layer.** The Donnybrook pattern ([11-roadmap.md](11-roadmap.md) §P1, [Donnybrook SIGCOMM 2008](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/donnybrook.pdf))
bounds the high-rate interest set at n = 24 with ~12·n kb/s receive scaling:
**~288 kb/s downstream per player**. [02-networking.md](02-networking.md) §6 ("Why player-host
migration is banned") uses
~200 kb/s per player for the same traffic. A 1 Gbit link at ~940 Mbps usable
therefore caps concurrent players at

* **≈ 3 260 players** at 288 kb/s, or ≈ 4 700 at 200 kb/s.

**By the persistence uplink.** 4.3 MB/s ≈ 34 Mbps at the knee — 3.6 % of the
link. Persistence traffic will never be what fills this NIC.

**Which ceiling a demo hits first depends on what else is on the box.**

* **Persistence-only box** (`persistd` + FDB; peers mesh among themselves or
  talk to field hosts elsewhere): the FDB client thread binds at **~4 000
  players**, and the NIC is at 4 %.
* **Everything on one box** (field hosts co-located, so AOI egress is on this
  NIC): the NIC binds at **~3 300 players**, ~20 % before the persistence path
  — because AOI downstream is ~8× the persistence uplink per player, and
  because a field host wants ~4 vCPU per hot cell on top of the 2.7 cores
  persistence is using.

For scale: a *full* field-host island is 128 players
([02-networking.md](02-networking.md) §6), which is 1 280 records/s — **3 % of the
knee**. This box carries roughly thirty saturated islands' worth of
persistence.

## 7. Caveats that change the numbers

* **The FDB configuration under test is not what a demo should run.** The gate
  and this sweep use one `fdbserver` process, `configure single memory`. The
  `memory` engine keeps the whole keyspace in RAM (this cluster reported 1.0 GB
  of storage-server space) and loses anything the transaction log cannot
  replay. A demo must run `configure single ssd` at minimum. What changes with
  `ssd`: reads leave RAM and land on this QLC RAID1, so `LeaseStore::locate` —
  the operation that already binds — gets a disk-latency tail on cold pages;
  FDB commits add fsyncs to the same `md2` array the journal is already
  fsyncing; and FDB's own CPU rises from ~0.2 cores. **Expect the knee to move
  down, not up.** It was not measured here and should be before a demo is
  sized on this document.
* **Everything is loopback.** Rig, gateway, follower and FDB are all on
  127.0.0.1, so no packet touched `eno1`. Every NIC figure in §5 and §6 is
  computed from payload sizes, not measured.
* **The rig shares the box.** Measured at 0.53–2.41 cores and subtracted
  explicitly (§4); pinning it to 2 CPUs changed throughput by <1.2 %.
* **The device has two regimes and any single run can be ~2× off** on
  per-flush fsync cost (measured 360 vs 775 flushes/s inside one
  configuration). This is why the tables are ranges. It does not move
  throughput (§5.3, item 4) but it does move `journal_commit_ms` and `bulk_ack_ms`.
* **`intent_commit_ms` p99 rests on ~1024 samples per run** (the intent mix is
  3 % of a 30 s run), so its percentiles are coarse and non-monotone at low
  load. Use it as an order-of-magnitude alarm, not a gauge.
* **One node, one follower, one host.** Both `persistd` processes fsync to the
  same array. A real two-node chain does not.
* **`bulk_ack_ms` and `journal_commit_ms` miss their D16 targets on this
  hardware at every load, including idle**, and that is settled and separate
  ([08-persistence.md](08-persistence.md) §4.3): it is the device's fsync, not
  capacity.

## 8. Why `intent_commit_ms` misses its target (a by-product)

D16 budgets `intent_commit_ms` p99 at 10 ms; the P2 gate measures ~30 ms and it
was unstudied. This sweep did not target it, but the mechanism is visible:
intents execute an FDB serializable transaction on **the same single client
network thread** that is already carrying one lease-locate read per bulk diff.
The correlation is exact — intent p99 tracks that thread's utilisation, not the
intent rate, which was a fixed 1024 per run throughout:

| FDB client thread | 25.8 % | 40.4 % | 57 % | 75 % | 98 % |
|---|---|---|---|---|---|
| intent p99 | 15–20 ms | 50–75 ms | 500 ms | 0.75–1.0 s | 2–3 s |

At the P2 operating point the thread is already a quarter busy with ~18 000
lease locates/s, which is a queue an intent has to cross twice. This is a
hypothesis consistent with every point measured, not a proven cause; the
experiment that would settle it is to serve `LeaseStore::locate` from the
actor's own in-memory lease index (it already tracks the cell) and re-measure.

## 9. You have outgrown this box when…

Operator-checkable, in the order they trip:

| # | Check | Threshold | Where |
|---|---|---|---|
| 1 | FDB client thread utilisation | **> 60 % of one core** | `pidstat -t -p $(pgrep -f 'persistd.*--fdb-cluster-file')`, or `top -H`: the busiest thread, named `persistd`, that is not the main thread. Absent on a node with no `--fdb-cluster-file`. At 40 % you are at the knee; at 75 % you are at peak throughput and shedding 10 %; at 95 % you are collapsing. |
| 2 | Bulk shed rate | **> 1 %** of admitted | `shed_slow_route / admitted` in the `gateway_ingress` records of `ORRERY_GATEWAY_BOUNDARY_JSONL`, or the `gateway: shedding bulk diffs at ingress` warning, which is always logged. |
| 3 | Durable ack rate vs offered | ack rate **stops rising** when you add load | `durable_acks / duration` from `p2-load`'s `run complete` line against `entities × diff_hz`. |
| 4 | `intent_commit_ms` p99 | **> 100 ms** | `p2-dashboard --gate`. |
| 5 | `persistd` RSS | **> 1 GB** for a ~10 k-entity world | backlog, not state. |
| 6 | Registrar withdrawals | **any** `leases_lost > 0` | `p2-load` fails the run. Not seen anywhere in this sweep — if you see it, something other than this envelope is wrong. |

Rules of thumb for sizing a demo on this hardware:

* **Comfortable:** ≤ 20 000 records/s — 10 000 entities at 2 Hz, ~2 000 players
  by §6.2. Shedding zero, intents at 15–20 ms, 1.7 cores of 16.
* **The knee:** 40 000 records/s — 20 000 entities at 2 Hz, ~4 000 players.
  1 % shed, intents at 50–75 ms, 2.8 cores of 16.
* **Do not:** > 80 000 records/s. Throughput turns over and falls; at 4× the
  knee the box makes fewer writes durable than at 1×.

What to do when you outgrow it is **not** "bigger box": the box is 83 % idle at
its own knee. It is to take FDB off the per-diff bulk path (§5.1) — or, failing
that, to add nodes, which is what the architecture is for
([08-persistence.md](08-persistence.md) §3.2, §13).

## 10. Reproducing

```bash
# a throwaway FDB, never the shared one — this consumes its cluster
docker run -d --name my-fdb --network host -e FDB_PORT=4599 \
  -e FDB_NETWORKING_MODE=host -e FDB_COORDINATOR_PORT=4599 \
  -e FDB_CLUSTER_FILE_CONTENTS='capacity:capacity@127.0.0.1:4599' \
  -v /some/dir:/var/fdb/data foundationdb/foundationdb:7.3.63
fdbcli -C /some/fdb.cluster --exec 'configure new single memory'

cargo build --release -p orrery_persistd --features fdb \
  -p orrery_seed --features orrery_seed/fdb
(cd p2-load && cargo build --release)

export ORRERY_FDB_CLUSTER_FILE=/some/fdb.cluster P2_CAP_OUT=$PWD/sweep
export PERSISTD_BIN=target/release/persistd P2_LOAD_BIN=p2-load/target/release/p2-load
export ORRERY_SEED_BIN=target/release/orrery-seed
export FDB_PID=$(docker top my-fdb | awk '/fdbserver/{print $2}')

scripts/p2-capacity-sweep.sh hz4-s250 250 4 30      # one point
python3 scripts/p2-capacity-report.py sweep/*/      # one row per point
```

The harness clears and re-seeds the cluster on every point (the P2 path
consumes its cluster: `activate_shards` bumps `actor/{shard}` epochs), deletes
the ~1 GB of journal data as soon as the run's numbers are in the JSONL, and
takes `P2_CAP_LOAD_CPUS` to pin the rig.
