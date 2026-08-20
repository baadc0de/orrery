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

> **That answer is this document's original one and describes the pre-#86
> binary; §3–§8 are all measurements of it and are kept as such.** #86 took
> FoundationDB off the fenced bulk path
> ([08-persistence.md](08-persistence.md) §2.1.3), and §11 re-measures the box
> on both FDB storage engines afterwards. The short answer for the current
> build: **the bulk path has no located knee below ~143 000 delivered
> records/s on either engine**, and the one thread still binds — but it is
> reached through *intents* now, at about 1 300 intents/s, not through bulk
> diffs.

> **D19 changes the default journal after these sweeps.** Every capacity point
> in this document used Fjall. The full P2 gate is green on the indexed raw
> journal, but the capacity knee has not yet been re-measured with it; treat the
> absolute throughput figures as the pre-D19 baseline until that rerun lands.

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
   up here first. **Every measurement of this series in this document was taken
   with the rig's lease renewals unphased, which is no longer the default
   (2026-08-19).** The threshold above is left where it is — it is an alarm
   level for a capacity sweep, not a D16 target — but a run started today
   produces a much smaller number for a reason that has nothing to do with
   capacity, and the two must not be compared. See §8's note and
   [08-persistence.md](08-persistence.md) §2.2.2.
4. **No lease withdrawn mid-run** (`leases_lost = 0`).

Offered load is `entities × diff_hz` — what the world would generate if nothing
throttled it. It is deliberately **not** `diffs_sent`, which the rig's uplink
scheduler caps and which re-counts a shed diff when the client re-offers it.

> **Correction (2026-08-18): a nominal offered load is not a delivered one,
> and every table below is labelled with the nominal.** The reasoning above is
> half right — `diffs_sent` does re-count re-offers — but reporting only the
> nominal number hid something worse. `p2-load`'s fan-out assert
> (`check_fan_out`) allows `sessions × 160` diffs/s, so a point provisioned at
> exactly `entities × diff_hz == sessions × 160` has **zero** margin, and the
> rig silently drops what does not fit: `UplinkScheduler::queue` is
> newest-wins, on the client, where no server counter can see it. Six of the
> eight rate points in §3.1 — 20 000, 40 000, 80 000, 120 000, 160 000 and
> 320 000 — are provisioned with exactly zero margin.
>
> Re-measured on the same rig and box for the `fdb-off-bulk-path` study, with
> the pre-change binary — i.e. this document's own configuration — the
> delivered rate was:
>
> | nominal/s | sessions | rig cap/s | delivered/s (measured) | delivered |
> |---|---|---|---|---|
> | 20 000 | 125 | 20 000 | 16 417–18 031 | 82–90 % |
> | 40 000 | 250 | 40 000 | 33 601–33 906 | 84–85 % |
> | 60 000 | 500 | 80 000 | 48 896 | 82 % |
> | 80 000 | 500 | 80 000 | 65 989 | 82 % |
> | 120 000 | 750 | 120 000 | 97 962 | 82 % |
> | 160 000 | 1000 | 160 000 | 99 536 | 62 % |
>
> **The rig tops out at about 99.3 k diffs/s on this box**, whatever the
> session count: the 120 000 and 160 000 rows are the *same* delivered
> operating point, ~98–99.5 k, and no point in the sweep ever delivered its
> nominal load. Read the tables below as "at a nominal setting of N", never as
> "with N records/s arriving". `scripts/fenced-sweep-report.py` now prints
> `delivered_per_s`, `rig_cap_per_s` and `delivered_pct` beside the nominal
> and warns when delivery falls below 95 %, so this cannot recur;
> `scripts/p2-capacity-report.py`, which produced the tables below, does not.
>
> What this does **not** change: §5's conclusion about which resource binds.
> That was measured per-thread on the box, and at the delivered rates above it
> stands unaltered — 25.8 % of one core at ~18 k delivered, 100 % at collapse.
> What it changes is the x-axis label, and any statement of the form "the knee
> is at N offered" where N was above ~99 k: those points were never reached.

## 3. The sweep

10 000 entities, 128 shards, 30 s, `--intent-mix trade=0.02,craft=0.01`
throughout. `p2-load`'s fan-out assert requires
`sessions × 160 ≥ entities × diff_hz`, so the rate leg has to raise sessions
with rate; the concurrency leg holds the rate at 2 Hz and raises sessions
alone, and the two legs share their session counts so the effects separate.

### 3.1 Rate leg (offered load rising)

The first column is **nominal** — `entities × diff_hz`, the sweep's setting.
It is not what the rig delivered; see the correction in §2 for the measured
delivery, which is 82–90 % of nominal below 120 000 and hard-capped at about
99.3 k diffs/s above it. The 120 000, 160 000 and 320 000 rows therefore all
sit at roughly the same delivered load, and their *labels* are the sweep's
request, not the box's input.

| nominal/s | hz | sessions | durable acks/s | shed % | intent p99 | busiest thread (mean / peak) | persistd cores | keeping up? |
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

* **Service knee at 40 000 nominal / ~33.6 k durable records/s.** The last
  configuration where shedding is ~1 %, intents commit inside 100 ms and the
  ack rate still climbs. This is the number to size against. Note that the rig
  delivered ~33.9 k/s at that setting, so on this leg the box was acknowledging
  essentially everything that reached it — the knee is a statement about the
  *delivered* ~34 k, and "40 000" is the dial it was reached from.
* **Throughput turnover between 80 000 and 120 000 nominal.** Peak achieved
  throughput is **59 k durable records/s** at 80 000 nominal (~66 k delivered),
  but that point is already shedding 10 % of writes and taking a second to
  commit an intent. Past it the box does not plateau, it **collapses**:
  120 000 nominal yields *less* than 40 000 nominal did, and at 320 000
  nominal the box makes 33–64 records durable per second — a factor of 300
  below its own baseline. The collapse is real and is not a delivery artifact:
  the 120 000 and 320 000 rows deliver roughly the same ~99 k, and produce
  35 k and 33–64 durable acks/s respectively, so what separates them is
  retry-storm dynamics inside the box, not how much load arrived.

The collapse is not a measurement artifact. At 320 000 offered, FDB reports
82 639 transactions *started* per second and 1 657 reads/s served, against 33
durable acks/s: `persistd` is spending its entire FDB budget on transactions
that time out and retry.

### 3.2 Concurrency leg (sessions rising, rate fixed at 2 Hz)

Nominal 20 000/s throughout; measured delivery on this leg is 18 4xx–18 7xx
diffs/s (92–93 % of nominal), flat across the session counts, so the
comparisons *within* this leg are sound and only the absolute x-axis label is
optimistic.

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

  > **Measured (2026-08-19): the knee does not move, and this prediction was
  > right about the mechanism and wrong about the conclusion — see §11.** It
  > was made while every fenced bulk diff did an FDB read (§5.1), and #86
  > removed that read. On the bulk path the two engines are indistinguishable
  > to 143 000 delivered records/s. The mechanism it names is real and was
  > found exactly where the mechanism says it should be — on the *intent*
  > path, which still rides FDB: `ssd` costs a 37× worse FDB read tail, a 3.5×
  > worse commit tail and 2× the `fdbserver` CPU. It moves intent latency by
  > one histogram bucket and moves the knee not at all. The prediction is left
  > standing here because it was reasoning, correctly labelled as reasoning,
  > and because §11 is only legible next to it.
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
  hardware at every load, including idle**, and that is separate from capacity
  ([08-persistence.md](08-persistence.md) §4.3). It is *not* settled, and the
  half of that sentence reading "it is the device's fsync" was retracted on
  2026-08-19: the same gate on a power-loss-protected NVMe whose barrier p99 is
  0.09 ms still reads `journal_commit_ms` p99 15 ms in 11 of 16 runs
  ([08-persistence.md](08-persistence.md) §4.4). What is settled is that it is
  not capacity, which is all this document needs from it.

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

> **Superseded again (2026-08-19), on the rig rather than on the run: the
> renewals are phased now.** Everything below this line, and everything in
> §11.7, measures a rig that renewed every session's whole entity set in one
> pass of its drive loop. That default is gone: `p2-load` phases each session's
> renewal across the period, and `P2_LOAD_HEARTBEAT_PHASED=0` is now what
> reproduces the burst. The decision behind the flip is the workload's shape,
> not the measurement's — real player populations are diffuse in phase space,
> and the synchronized case belongs to admission control (a login queue,
> [11-roadmap.md](11-roadmap.md) §P6), not to the persistence path.
>
> **The re-baselined P2 gate — which series pass, which fail, and which
> failures are the device's — is [08-persistence.md](08-persistence.md)
> §2.2.2.** It is pointed at rather than restated here, for the same reason
> §11.7's note points at §2.2.1: the version of that note that restated
> numbers got five of them wrong in the same direction.

> **Superseded (2026-08-19): the table above measures the first second of each
> run, and is left visible because the reason it is wrong is worth keeping.**
> "The intent rate was a fixed 1024 per run throughout" is the tell. It was
> fixed at 1024 because `p2-load` never called `IntentQueue::retire`, so the
> 1024-entry queue filled — in under two seconds at a 3 % mix and 18 000
> diffs/s — and `submit` returned `None` for the rest of the run. Every one of
> those 1024 samples comes from the opening burst, while sessions are still
> connecting. The column is a cold gateway's response to 1024 simultaneous
> intents, at five different session counts.
>
> The rig now retires a settled intent, and §11.7 re-measures the series with
> 5 000–40 000 samples spread across each run. What survives: the FDB client
> thread is indeed the mechanism, and the correlation was pointing at the
> right resource. What changes: intent p50 at the P2 operating point is
> **6–8 ms**, not 15–20 ms, so D16's 10 ms budget is missed in the *tail*
> only; and intent latency is set by the **intent** rate, not the bulk rate —
> holding intents at ~1 000/s while bulk falls from 35 k to 18.6 k records/s
> leaves p50 unchanged at 15–20 ms.

## 9. You have outgrown this box when…

Operator-checkable, in the order they trip:

> **Still current after #86, with one substitution (§11.7).** Check 1 is the
> right check and still trips first — but on the current build that thread is
> driven by **intents**, not by bulk diffs: it reads 66–75 % of one core at
> ~1 000 intents/s and 94 % at ~1 300, where intent p50 becomes 750 ms. The
> bulk numbers in check 1's cell ("at 40 % you are at the knee") describe the
> pre-#86 binary. Check 4's threshold is unchanged and is now measurable:
> `p2-load` no longer stops submitting intents after 1 024 of them.

| # | Check | Threshold | Where |
|---|---|---|---|
| 1 | FDB client thread utilisation | **> 60 % of one core** | `pidstat -t -p $(pgrep -f 'persistd.*--fdb-cluster-file')`, or `top -H`: the busiest thread, named `persistd`, that is not the main thread. Absent on a node with no `--fdb-cluster-file`. At 40 % you are at the knee; at 75 % you are at peak throughput and shedding 10 %; at 95 % you are collapsing. |
| 2 | Bulk shed rate | **> 1 %** of admitted | `shed_slow_route / admitted` in the `gateway_ingress` records of `ORRERY_GATEWAY_BOUNDARY_JSONL`, or the `gateway: shedding bulk diffs at ingress` warning, which is always logged. **On a binary between #86 and 2026-08-19 this check reads the sampled invariant-J audit rather than route slowness — see §11.2** — so a JSONL captured in that window needs `shed_slow_route` compared against the audit counters before it means anything. Fixed since. |
| 3 | Durable ack rate vs offered | ack rate **stops rising** when you add load | `durable_acks / duration` from `p2-load`'s `run complete` line against `entities × diff_hz`. |
| 4 | `intent_commit_ms` p99 | **> 100 ms** | `p2-dashboard --gate`. The alarm level is calibrated on the unphased rig (§2, §8); on the phased default it fires much later, and [08-persistence.md](08-persistence.md) §2.2.2 carries the current gate baseline. |
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

# every point in this document predates 2026-08-19 and was measured with the
# rig's lease renewals unphased; that is now an opt-out, not the default
P2_LOAD_HEARTBEAT_PHASED=0 scripts/p2-capacity-sweep.sh hz4-s250 250 4 30
```

The harness clears and re-seeds the cluster on every point (the P2 path
consumes its cluster: `activate_shards` bumps `actor/{shard}` epochs), deletes
the ~1 GB of journal data as soon as the run's numbers are in the JSONL, and
takes `P2_CAP_LOAD_CPUS` to pin the rig.

## 11. The FoundationDB storage engine: `ssd` versus `memory`, measured

§7 said of `configure single ssd`: *"reads leave RAM and land on this QLC
RAID1 … FDB commits add fsyncs to the same `md2` array the journal is already
fsyncing … **Expect the knee to move down, not up.**"* That was reasoning, not
a measurement, and it was written while **every fenced bulk diff did an FDB
read** (§5.1). #86 removed that read. This section is the measurement, taken
2026-08-19 on the same box with the post-#86 binary.

**The answer, in one paragraph.** Two FoundationDB clusters differing only in
`configure new single {ssd,memory}`, driven by the same `persistd` binary over
the same points with the arms interleaved, delivered the same throughput to
within 2 % at every load from 18 k to 143 k records/s, shed **no bulk diff for
route slowness at all**, and lost no lease on either. Neither arm's knee was
reached: the load generator ran out first. Where `ssd` *does* cost something is the path that
still rides FoundationDB — **intents** — and there it buys a 48× worse FDB read
tail, a 2.2× worse commit tail, ~1.4× the median commit latency, 2× the
`fdbserver` CPU and one histogram bucket of intent latency, without moving the
point at which the intent path saturates. The prediction was right about its mechanism and wrong about its
conclusion, for the reason it named itself: it priced an FDB read per diff,
and there is no longer one.

**The verdict on "expect the knee to move down", in one line: right for the
wrong reason.** Every physical effect it named is real and was found — reads
leave RAM and get a disk tail, FDB's commits do fsync the journal's array,
FDB's CPU does rise. None of them moves the knee, because the load that would
have amplified them through `LeaseStore::locate` no longer exists, and what
remains is 0.1 % of the array's write load and 0.1 core.

Raw evidence for everything below — 73 point directories, per-point extracted
summaries, per-run FDB status samples and the reduced tables — is under
`~/ssd-study` on the box this was measured on; the reduced tables are
`ALL-POINTS.tsv` and `REPORT.tsv` there.

**Windowing rule, stated once and applied to both arms.** The per-point
harness files (`load.jsonl`, `pidstat`, `iostat`) only exist for the duration
of their own point, so anything read from them is in-window by construction.
FoundationDB's own `status json` is not: one sampler polls **both** clusters
every 2 s, continuously, including while the other arm is under test and while
the harness is between points. A sample therefore belongs to an arm **iff its
timestamp falls inside `[end − duration_secs, end]` of one of that arm's own
points**, where `end` is the mtime of that point's `load.jsonl` — the instant
the rig wrote its footer — and `duration_secs` comes from `point.json`.
Samples outside every window of the arm they name are inter-point box state,
not that arm's cost, and are excluded from **both** arms alike. §11.7 and
§11.8 restate their numbers under this rule, and `scripts/fdb-status-window.py`
applies it so the derivation is a command rather than a claim; §11.6's median
result — FoundationDB at 0.1 % of the array's write load — survives it
unchanged.

### 11.1 What was run

| | ssd arm | memory arm |
|---|---|---|
| container / port | `orrery-fdb-ssdarm`, 4601 | `orrery-fdb-memarm`, 4602 |
| configured | `configure new single ssd` | `configure new single memory` |
| `status json` reports | `storage_engine: ssd-2`, `log_engine: ssd-2` | `storage_engine: memory`, `log_engine: ssd-2` |
| data dir | its own, on `md2` | its own, on `md2` |

One variable; everything else held: the same `persistd` binary, the same rig,
the same 10 000-entity `p2demo` world re-seeded per point, the same box.
`p2-capacity-sweep.sh` records the engine each point actually ran against, read
back from `status json` into `point.json` — an engine-arm table that infers the
engine from a directory name is one mislabelled run away from being wrong.

Two things before any number:

* **The `memory` arm is not a "no disk" arm.** Its *storage* engine is in RAM;
  its transaction log is `ssd-2` and still fsyncs to `md2`. The comparison is
  storage-engine reads and B-tree writes, not disk versus no disk.
* **Both arms load the world.** Every point's `orrery-seed verify` checked
  30 000 rows and the rig took 10 000 leases before writing a diff. A silently
  empty world is the failure mode that produced a misleading study earlier in
  this project; here it is checked per point.

Arms are interleaved point by point, and which arm runs first alternates with
the repeat, because this box's per-flush fsync cost has two regimes that differ
~2× and switch on a tens-of-seconds scale
([08-persistence.md](08-persistence.md) §4.3) — blocking the arms would
attribute a regime to the engine. Every point ran at least twice; tables report
min–max across repeats.

**Warm-up:** none detectable. The first point ever run on the fresh `ssd`
cluster delivered 18 363/s; the same configuration ten points later delivered
18 475–18 477/s, a 0.6 % difference in the direction of *faster*. No point was
discarded as warm-up.

### 11.2 What "keeps up" means here

Unchanged from §2 and fixed before the sweep ran: `shed_slow_route / admitted
≤ 1 %`; the durable ack rate still rising when offered load rises;
`intent_commit_ms` p99 ≤ 100 ms; `leases_lost = 0`. One addition: a point whose
**delivered** rate falls well short of its nominal setting is reported as a
measurement of the rig, not of the box.

Which criteria were met, and where: criteria 1, 2 and 4 hold at **every** point
of the rate leg on **both** arms. Criterion 3 is the one that fails, it fails
on both arms alike, and §11.7 shows that as published it was measuring
something else.

> **Criterion 1 carries no weight in this study — corrected 2026-08-19.**
> `shed_slow_route` did not measure route slowness here. The gateway runs
> `Router::apply_fenced` inside a 25 ms timeout measured from the diff's
> arrival, and the sampled invariant-J audit (1 in 1 000 accepts) was awaited
> *inside* that timeout, so a sampled diff whose audit read overran the
> remaining budget was cancelled and counted as shed. Checked against every
> point directory of this study, the identity
>
> ```text
> shed_slow_route == (audits the sampler decided on) − (audits that completed)
> ```
>
> holds **exactly — zero deviation — at all 73 points**, on both engines, from
> 12 shed to 7 244 shed; `location_audit_us_max` sits at 11 555–26 526 µs
> across them, bunched against the 25 ms budget, which is the signature.
> (An earlier draft of this line said 20 771–26 526 µs "in every one of them".
> Seven points sit below that floor — the lowest is `ssd-ib40k-xhi-r2` at
> 11 555 µs — because a point whose audits are all cancelled early records a
> maximum below the budget rather than at it. The clamping is the signature;
> the floor was not a real one.)
> **Bulk shed attributable to actual route slowness is zero in this entire
> study.** So criterion 1 was satisfied by a diagnostic that could not have
> failed it for the right reason, and the "no knee" conclusion rests on
> criteria 2 (the durable ack rate still climbing steeply at the last point)
> and 4 (`leases_lost = 0` in all 28 runs) — which it does, on their own. The
> defect is fixed: the audit is detached from the request path
> (docs/08-persistence.md §2.1), so the counter means what it says again on
> the current binary.
>
> **Re-checked on the fixed binary**, `ssd`, a throwaway single-node cluster
> of its own (raw output under `~/auditfix/sweep` on the same box), at two
> points chosen because they are where this study shed most outside the
> saturated intent point:
>
> | point | offered | acknowledged | `shed_slow_route` | audits decided / completed / dropped |
> |---|---|---|---|---|
> | `ssd-ic40k-r1` (as published) | 2 115 869 | 2 115 414 | **455** | 2 116 / 1 661 / — |
> | `fix-ic40k-r1` (post-fix) | 2 118 198 | **2 118 198** | **0** | 2 119 / 2 119 / 0 |
> | `ssd-r80k-r1` (as published) | 1 989 965 | 1 989 829 | **136** | 1 990 / 1 854 / — |
> | `fix-r80k-r1` (post-fix) | 1 987 500 | **1 987 500** | **0** | 1 988 / 1 951 / 0 |
>
> The identity is broken in the only way that matters: `shed_slow_route` is 0
> at both points while the audits still run and still land. Every diff offered
> was acknowledged, on both. (The 37 audits outstanding on `fix-r80k-r1` are
> in flight at the reporter's last 1 s flush, not lost; `location_mismatches`
> is 0 on both, so invariant J still holds.)
>
> One number moved a long way and is worth its own line: `location_audit_us_max`
> is **158 ms** and **929 ms** on those two points, against 11 555–26 526 µs at
> every point of the study. It was never a measurement of how long the audit
> takes — an audit that would have exceeded the budget was cancelled before it
> could record one, so the statistic was censored at the budget by
> construction. The FDB locate tail under load is an order of magnitude worse
> than this document has ever shown, which costs nothing now that it is off the
> request path, and is consistent with §11.7: the second point runs a 3 %
> intent mix at ~1 190 intents/s, which is the client-thread saturation point,
> and an audit read queues behind it. That also makes `fix-r80k-r1` a joint
> bulk+intent point rather than a replica of the bulk-only `ssd-r80k-r1`
> beside it — its throughput is not comparable, only its shed is.

### 11.3 The rig quantizes offered load, which decides which points exist

§2's correction concluded that "the rig tops out at about 99.3 k diffs/s on
this box, whatever the session count". There is a ceiling, but that is not the
mechanism, and the mechanism decides which operating points are reachable at
all.

`p2-load` generates an entity's diffs on a **whole number of 50 ms flush
frames**: `registration_phase_slots` is `ceil(FLUSH_HZ / diff_hz)`,
`FLUSH_HZ = 20`, and an entity emits once per that many frames. The effective
per-entity rate is `20 / ceil(20 / diff_hz)`:

| `--diff-hz` | 2 | 4 | 6 | 8 | 12 | 16 | 20 |
|---|---|---|---|---|---|---|---|
| effective Hz | 2.00 | 4.00 | 5.00 | 6.67 | 10.0 | 10.0 | 20.0 |
| ceiling as % of nominal | 100 | 100 | 83.3 | 83.3 | 83.3 | **62.5** | 100 |

Measured delivery follows that row: 92 % at hz 2, 88 % at hz 4, 83 % at
hz 6/8/12, **62 %** at hz 16, 70 % at hz 20. So the "120 000" and "160 000"
points of every sweep in this document are the same 10 Hz × 10 000 = 100 000/s
operating point *by construction*, and hz 16 was never a higher offered load
than hz 12. This supersedes the reading in §2 that the two coincided because
the rig ran out of CPU: at those points it ran out of **frames**.

For anyone extending the sweep: with 10 000 entities the reachable delivered
rates are 10 000 × {2, 2.5, 3.33, 4, 5, 6.67, 10, 20}. Nothing exists between
100 k and 200 k, and raising `--diff-hz` inside that gap *lowers* delivery.

### 11.4 The rate leg

Sessions provisioned at **2× the rig's own `check_fan_out` capacity**
(`sessions × 160 ≥ 2 × entities × diff_hz`) at every point; the earlier sweep
ran six of eight rate points at exactly 1.0×. `margin_x` is 2.00 in every row
below, so under-delivery here is §11.3's frame quantization, not the fan-out
cap. 30 s per point, two repeats, arms interleaved, min–max across repeats,
`--intent-mix` at its 3 % default:

| nominal/s | sessions | arm | delivered/s | durable acks/s | shed % | journal fsync busy | journal wait/ack | persistd cores | fdbserver cores |
|---|---|---|---|---|---|---|---|---|---|
| 20 000 | 250 | memory | 18 417–18 421 | 18 416–18 420 | 0.003 | 69–71 % | 5.7–13.2 ms | 1.00 | 0.05 |
| 20 000 | 250 | **ssd** | 18 475–18 477 | 18 475–18 477 | 0.003 | 19–63 % | 0.9–4.8 ms | 1.02–1.11 | 0.06–0.07 |
| 40 000 | 500 | memory | 35 067–35 170 | 35 065–35 168 | 0.004 | 64–73 % | 4.3–8.5 ms | 1.55 | 0.06 |
| 40 000 | 500 | **ssd** | 35 127–35 143 | 35 125–35 142 | 0.003–0.004 | 68–69 % | 4.9–5.2 ms | 1.54–1.56 | 0.08 |
| 60 000 | 750 | memory | 49 539–49 598 | 49 535–49 594 | 0.007–0.008 | 68–69 % | 6.1–13.1 ms | 2.00–2.13 | 0.06 |
| 60 000 | 750 | **ssd** | 49 648–49 669 | 49 644–49 665 | 0.008–0.009 | 64–69 % | 5.8–13.2 ms | 2.00–2.09 | 0.09 |
| 80 000 | 1 000 | memory | 65 170–66 333 | 65 166–66 329 | 0.006–0.007 | 63–76 % | 7.3–43.6 ms | 2.46–2.58 | 0.07 |
| 80 000 | 1 000 | **ssd** | 66 325–66 332 | 66 320–66 328 | 0.007 | 65–78 % | 8.3–11.7 ms | 2.59–2.61 | 0.10 |
| 120 000 | 1 500 | memory | 98 539–99 621 | 98 532–99 605 | 0.007 | 71–74 % | 12.6–29.6 ms | 3.44–3.61 | 0.08 |
| 120 000 | 1 500 | **ssd** | 98 681–99 639 | 98 674–99 632 | 0.007 | 72–75 % | 13.9–25.2 ms | 3.56–3.60 | 0.10 |
| 160 000 | 2 000 | memory | 99 413–99 721 | 99 339–99 677 | 0.007 | 73–79 % | 12.1–23.1 ms | 3.78–3.90 | 0.08 |
| 160 000 | 2 000 | **ssd** | 99 090–99 790 | 99 077–99 778 | 0.008 | 70–79 % | 12.8–32.2 ms | 3.76–4.03 | 0.10 |
| 200 000 | 2 500 | memory | 138 400–140 817 | 137 736–140 111 | 0.008 | 67 % | 46.2–56.2 ms | 4.79–4.82 | 0.08 |
| 200 000 | 2 500 | **ssd** | 140 483–142 867 | 139 918–142 313 | 0.008 | 66–67 % | 43.0–50.2 ms | 4.81–4.85 | 0.10 |

`leases_lost`, `diff_nacks`, `locate_fallbacks` and `location_mismatches` are
**0** in all 28 runs, and `mailbox_turns / applies` is exactly 1.0 — the
post-#86 fast path answered every routing question without FoundationDB on both
engines.

**The shed column above is not shed bulk — corrected 2026-08-19.** The
sentence that stood here read *"Shedding stays three orders of magnitude below
its 1 % threshold at every point"*, offered as the first leg of the "no knee"
argument. Every one of those diffs was a cancelled invariant-J audit, not a
route that ran out of time: see §11.2's note, where the identity
`shed_slow_route == decided − completed audits` is shown to hold exactly at
all 73 points of the study. The 0.003–0.009 % column is therefore a reading of
the 1-in-1 000 audit sampler against a 25 ms budget, and it is left in the
table because it is what the counter recorded — not because it says anything
about the box's capacity. Bulk shed from route slowness was **zero**, which is
a stronger statement than the one it replaces, and the knee argument stands on
criteria 2 and 4.

**No knee on either arm.** The durable ack rate is still climbing steeply at
the last point (138–143 k against 99 k at the point before), and no lease was
lost in any of the 28 runs. Stated honestly:

> **ssd: ≥ 142 900 delivered records/s, knee not located.**
> **memory: ≥ 140 800 delivered records/s, knee not located.**
> The arms are within 1.5 % of each other, inside this box's run-to-run spread.

### 11.5 What binds, per arm

At the top point (200 000 nominal, 2 500 sessions, ~140 k delivered), every
candidate with its measurement. Both arms share a row wherever they agree,
which is nearly everywhere:

| candidate | at ~140 k delivered | binding? |
|---|---|---|
| the load generator | `p2-load` at 3.0–3.3 cores, its **single drive-loop thread at 74–76 % mean, 79–80 % peak** of one core, delivering 63–71 % of nominal | **yes — the ceiling that was reached** |
| `persistd` CPU total | 4.79–4.85 cores of 16 (30 % of the box), spread evenly over 16 tokio workers at ~20 % each | no |
| journal group-commit thread | 66–67 % of wall inside `sync_data`, 133–154 flushes/s, ~1 000 records per flush against an 8 192 cap | no — closest server-side resource |
| the NVMe array | 38–41 % `%util`, aqu-sz 3.8–4.6, 73–79 MB/s written, ~220 flush ops/s per member | no |
| the `libfdb_c` client thread | **8.1–12.9 %** of one core, run mean (97–99 % before #86); worst single 1 s sample 87 % | no — but see §11.7 |
| `fdbserver` CPU | memory 0.08 cores, **ssd 0.10 cores** | no |
| FDB commit / read / conflicts | zero conflicts in every sample on either arm | no |
| RAM | never a factor on 62 GB | no |
| the NIC | loopback only (§7) | not exercised |

Three deserve their evidence spelled out.

**The rig is the ceiling, and it is a frame-deadline ceiling rather than a
starved one.** `p2-load` runs one drive loop that must visit 2 500 sessions
twenty times a second; per-thread sampling — which the harness does for
`persistd` but not for the rig — puts that loop thread at **74.4 % mean, 79 %
peak** of one core while the process as a whole uses 3.19 cores and delivers
125 942/s. Re-running the identical point with the rig **pinned to 8 of the 16
threads** delivered 127 817/s — 1.5 % *more*, with the loop thread at 76 % —
so the rig is not short of CPU. It is short of time inside a 50 ms frame,
which is §11.3's quantization seen from the other side, and no amount of extra
cores fixes it.

**The journal's fsync duty cycle is the closest server-side resource, and it is
not engine-sensitive.** `sync_data_us_sum / duration` — the fraction of wall
time the single group-commit thread spends inside `fdatasync` — is 17–19 % at
18 k, 63–78 % at 66–100 k and 66–67 % at 140 k. It does not rise monotonically,
because group commit absorbs load by batching: at 140 k the journal writes
~1 000 records per flush at 134 flushes/s. The device's two regimes move this
number more than load does — the same arm and point, two repeats, 19 % and 63 %.

**The FDB client thread is no longer a bulk-path candidate on either engine.**
Its **run mean** is 8.1–12.9 % of one core across all 28 rate-leg runs (`ssd`
8.5–12.7, `memory` 8.1–12.9), against the 97–99 % run mean §5.1 measured
before #86. #86's claim holds on `ssd` as well as on `memory` — which is
precisely why §7's prediction does not.

> **Corrected 2026-08-19.** This paragraph read *"It peaked at 12.9 % of one
> core across all 28 rate-leg runs"*. 12.9 % is the largest **mean**; the
> largest single 1 s `pidstat` sample over the same 28 runs is **87 %**
> (`ssd`; `memory` 83 %). The mean is the right statistic for "is this thread
> the binding resource over a run" and the comparison against §5.1 is
> mean-against-mean, so the row's verdict does not change — but "peaked" was
> the wrong word for it by a factor of seven, and a reader checking the
> operator trip-wire in §9 ("client thread > 60 % of one core") against a
> per-second sample would have found it tripping on a bulk-only run.

### 11.6 Device contention between FDB's fsyncs and the journal's

This is the mechanism §7 expected to hurt, and the one genuinely new thing
`ssd` introduces. It is real, it is measurable, and it is one part in a
thousand of the write load.

At matched delivered load the arms' device counters overlap: at ~99 k
delivered, `md2` wrote 74.2–79.2 MB/s on `ssd` against 68.1–76.5 MB/s on
`memory`, at 300–352 versus 282–334 flush ops/s per NVMe member, `%util`
49.4–49.9 % versus 49.3–52.6 %. The number a contending fsync stream would
inflate — the journal's own per-flush `sync_data` cost — is 2.91–3.65 ms on
`ssd` and 3.06–4.01 ms on `memory` there: each inside the other's spread, and
both inside the ~2× regime swing this box shows between repeats of one
configuration.

Sampling FoundationDB's own `status json` every 2 s during 60 s points settles
why. Under the windowing rule stated at the head of §11, **FDB writes
78 kB/s** (median; 29–793 kB/s across samples) on the `ssd` arm and 75 kB/s
(median; 27–697) on `memory`, against the journal's ~70 MB/s. FDB's fsyncs do
land on the same array, exactly as predicted — and they are 0.1 % of what is
already there.

> **Corrected 2026-08-19.** The medians published here (78 / 77 kB/s) barely
> move under the rule — `ssd`'s is unchanged, `memory`'s goes 77 → 75 — and
> the conclusion, 0.1 % of the array's write load, is unchanged with them.
> That was re-derived rather than assumed. The **ranges** were not: `memory`'s
> was published as `34–201` because its first in-window sample pair, 696.6
> kB/s, was skipped while `ssd`'s matching first pair, 775.0, was kept. Both
> are the same thing — the lease-claim/seed burst at run start — and under one
> rule both arms show it. See §11.7's F2 note.

### 11.7 `intent_commit_ms`, and where `ssd` does cost something

**The measurement was broken first, in a way that made the engine question
unanswerable.** `IntentQueue` keeps a settled intent until the client calls
`retire()`; `p2-load` never did. The queue holds 1024, so after 1024
submissions `submit` returns `None` for the rest of the run — which is why
every run in this project reports exactly `intents=1024` whatever its duration,
rate or `--intent-mix`. At a 3 % mix and 18 000 diffs/s the cap is hit in under
two seconds: on a 30 s, 1 500-session point, **all 1 024 `intent_commit_ms`
samples fall in the first 3.7 % of the rig's JSONL stream**, while sessions are
still connecting. §8's table is a cold gateway answering a burst of 1 024
simultaneous intents, not intent latency under load. The rig now retires a
settled intent.

Re-measured with the fixed rig — `--intent-mix` scaled per point to hold the
intent rate near 200/s while bulk rises (`ia*`), then bulk held while the
intent rate rises (`ib*`) — 30 s, two repeats, min–max:

| point | bulk delivered/s | intents/s | arm | samples | p50 | p90 | p99 | server p50 | FDB client thread |
|---|---|---|---|---|---|---|---|---|---|
| ia20k | 18 465–18 483 | ~203 | memory | 6 083–6 086 | 7–8 ms | 10–40 ms | 150 ms | 5–6 ms | 18.2–18.3 % |
| ia20k | | | **ssd** | 6 070–6 083 | 6–8 ms | 8–15 ms | 150 ms | 3.5–6 ms | 18.4–19.8 % |
| ia80k | 66 333 | ~191 | memory | 5 723 | 9 ms | 15 ms | 150 ms | 6–7 ms | 20.8–21.9 % |
| ia80k | | | **ssd** | 5 723 | 15 ms | 20 ms | 200 ms | 8 ms | 22.8–23.4 % |
| ia200k | 127 083–129 067 | ~170 | memory | 5 107–5 145 | 20 ms | 50–100 ms | 200–300 ms | 7–8 ms | 20.9–23.1 % |
| ia200k | | | **ssd** | 5 033–5 077 | 20 ms | 50 ms | 200 ms | 7–8 ms | 23.4–23.7 % |
| ib40k-hi | 35 100–35 237 | ~1 032 | memory | 30 956–31 067 | 15 ms | 75–100 ms | 200 ms | 10–15 ms | 66.2–68.9 % |
| ib40k-hi | | | **ssd** | 30 929–31 013 | 20 ms | 100 ms | 300 ms | 20 ms | 73.9–75.1 % |
| ib40k-xhi | 35 090–35 123 | ~1 300 | memory | 39 287–40 407 | 750 ms | 1.0 s | 1.5 s | 750 ms | **94.3 %** |
| ib40k-xhi | | | **ssd** | 38 262–38 905 | 750 ms | 1.0 s | 1.5 s | 750 ms | **94.5 %** |

Five thousand to forty thousand samples per run instead of 1 024, spread across
the whole run. What that buys:

* **p50 is 6–8 ms at the P2 operating point** — at, not far above, D16's 10 ms
  budget, on both engines. The budget is missed in the **tail**, not the
  middle. §8 reported 15–20 ms for the same configuration because it was
  measuring the burst.
* **The tail is the server's.** Client p99 and the gateway's own
  receipt-to-reply p99 agree at every point, so the 150–300 ms excursions
  happen inside `persistd`, not in the rig's queue.
* **Intent latency is set by the intent rate, not the bulk rate.** Holding
  ~1 000 intents/s while bulk falls from 35 300 to 18 600 records/s leaves p50
  unchanged (15 ms memory, 20 ms ssd). Holding bulk and raising intents from
  200 to 1 300/s moves p50 from 15 ms to 750 ms.
* **The intent path saturates between ~1 030 and ~1 300 intents/s on both
  engines** — 50× the latency for 26 % more rate. This, not the bulk path, is
  the knee this box still has.

> **Superseded 2026-08-19 — the tail is decomposed, and it is two things.**
> The p50 and saturation findings above stand unchanged. The **tail** rows do
> not mean what this section implies.
>
> The "150 ms" in the `ia*` rows is **a load-generator artifact stacked on the
> device**, not an intent-path cost. `IntentStageMetrics` splits the intent
> span into `ingress / admit / spawn_wait / {alloc, grv, idem_read, fence,
> commit, backoff} / reply` plus two explicit residuals. `p2-load` renewed
> every session's whole entity set in one pass of its drive loop, so 10 000
> lease renewals reached the gateway inside a few milliseconds every
> `LEASE_HEARTBEAT` (3 s); inside a caught intent that time lands on **GRV**,
> and phasing the same renewals across the period (`P2_LOAD_HEARTBEAT_PHASED`,
> **the default since 2026-08-19** — see §8's note and
> [08-persistence.md](08-persistence.md) §2.2.2) drops run-total GRV by an
> order of magnitude and removes the periodicity.
> What phasing leaves behind is **FoundationDB's own commit fsync**, in the
> same device stall window as `journal_commit_ms`. The `ib40k-xhi` saturation
> rows are unaffected: at ~1 300 intents/s the FDB client thread is genuinely
> at 94 %.
>
> **"The tail is the server's" is now open, not established.** Its original
> evidence was one histogram bucket wide — both sides share the D16 lattice,
> whose neighbours here are 100 / 150 / 200 ms, so two p99s in the same bucket
> cannot agree to better than 50 ms, the width of the effect. The replacement
> evidence has since been withdrawn as well, and §2.2.1 does not re-establish
> the claim: over the 21 loaded runs the client's arrival-stamped excess over
> the server's maximum is 0.15–11.17 ms and exceeds 1 ms in **4 of them**.
> Bounding client-side time needs an instrument that does not exist yet.
>
> **The quantitative claims all live in
> [08-persistence.md](08-persistence.md) §2.2.1**, where every one of them is
> printed by `scripts/intent-tail-derive.py` from the raw sweep and carries the
> population it is drawn from. This note points at them rather than restating
> them, because the version of it published first *did* restate them and got
> five wrong in the same direction.
>
> **What this note said on first publication, and what §2.2.1 says now** — left
> visible for the same reason [08-persistence.md](08-persistence.md) §2.1.3's
> own correction is:
>
> * *"the client's arrival-stamped maximum for a run is within 1 ms of the
>   server's, 158.40 against 157.41 ms … the rig's poll cadence is worth ~2 ms
>   at p50 and nothing at p99."* The 1 ms bound is **withdrawn** (see above);
>   the poll-cadence figure is **deleted** — no artifact in the sweep produces
>   it. `IntentQueue::on_ack_at` stamping the ack on arrival remains the right
>   fix for the *measurement*; it is not evidence that the client side is quiet.
> * *"`batch_locks` reads 10 000 in exactly the intervals that spike and 0 in
>   every other."* **Corrected to 8 of 9.** One spike interval and one lock
>   interval do not pair — a burst straddling a 250 ms report boundary.
> * *"the whole stage set summing to within 20 µs of it."* **Deleted as
>   imprecise.** §2.2.1 closes the arithmetic three ways instead, against both
>   emitted residuals rather than against a hand-quoted tolerance.
> * *"collapses the tail's GRV from 65 ms to 0.7 ms."* **Withdrawn**: it paired
>   two single runs drawn from different legs. §2.2.1 states the run-total GRV
>   ranges with the populations behind them.
> * *"equal at the extremes (200.7/201.4, 175.9/175.3, 355.7/351.3 ms)."*
>   **Withdrawn**: three hand-picked rows of six, with the two that disagree
>   omitted. The supported claim is *the same device stall window*; the pooled
>   correlation is mostly the regime switch moving both columns together, and
>   within either regime it is much weaker.
> * *"0.0 % of intents past 20 ms."* **Corrected to 0.03 %** (2 of 6 089).
> * *"the decomposition at 972 intents/s agrees with the `status json` figures
>   below."* **Deleted.** Those figures come from an artifact set the derive
>   script does not read, so §2.2.1 makes no comparison against them and
>   neither does this note.


**What runs out there is the FoundationDB client's network thread — the same
single thread as before #86.** Its utilisation against intent rate, both arms,
bulk held at 35 k: 18–24 % at ~200 intents/s, 66–75 % at ~1 030, and
**94.3–94.5 %, peaking at 100–101 %, at ~1 300**, which is where p50 becomes
750 ms. One intent is *one* serializable FDB transaction that reads the
idempotency row and then re-reads the `actor/{shard}` row of **every shard this
node activated** — 128 of them, concurrently (`IntentFence`, `intent/fdb.rs`) —
before it writes. At 1 300 intents/s that is ~170 000 FDB operations per second
on one thread. #86 took the bulk path off that thread and left the intent path
on it: §5.1's conclusion has not been repealed, it has been relocated.

FoundationDB the *server* is not the limit there — with one caveat about
which leg the evidence comes from, stated because this section exists to kill
exactly that error. The `status json` figures below are from the **`ic` leg at
~1 000 intents/s**, not from the ~1 300 intents/s saturation point: only
`decomp-leg.sh` starts `fdb-sampler.sh`, and it runs `ic40k`/`ic20k` only, so
no in-window `status json` exists for the saturated point at all. In-window
commit latency never exceeded 17.60 ms (`ssd`) or 8.03 ms (`memory`), read
latency 4.78/0.10 ms, GRV 4.29/0.86 ms, **conflicts exactly zero** in every
sample on both arms, and `fdbserver` at 0.81/0.42 cores.

At the saturated point itself the only in-window instrument is each point's own
`pidstat.txt`, and it reads higher: `fdbserver` at **0.86 cores mean / 0.95
peak** on `ssd` (`ib40k-xhi-r1`; 0.84/0.95 on r2) against 0.42/0.48 and
0.43/0.50 on `memory` — close to a full core on a `configure single` cluster.
The conclusion stands on the two measurements that *are* in-window there (the
client thread at 94 %, and zero conflicts), but a reader sizing a cluster
should use 0.86–0.95 cores, not 0.81.

One artifact of the saturated point, so nobody reads it as a server failure:
at ~1 300 intents/s the rig's own `durable_acks` footer falls to
2 501–2 560/s while the gateway's per-second counter still acknowledges
35 298–35 397 bulk diffs/s. The bulk writes are being made durable; the rig's
drive loop is too busy signing and tracking intents to *process* their
replies. Every intent number above is corroborated server-side, which is why
the conclusion does not rest on that counter.

> **Corrected 2026-08-19.** The published version of that sentence ended "and
> sheds 0.1 %", which read as
> the gateway shedding bulk writes under intent pressure. It was not: those
> 1 050–1 051 shed diffs are cancelled invariant-J audits, every one of them —
> see §11.2 and §11.4. At that point the sampler decided on 1 053 audits and
> **2** of them completed.

**What `ssd` costs, from FDB's own side.** The four 60 s `ic` points per arm
(`ic40k` and `ic20k`, two repeats each), `status json` every 2 s,
min/median/max over the samples in which that arm was under test — under the
windowing rule stated at the head of §11, applied identically to both arms:

| | ssd | memory |
|---|---|---|
| commit latency | 0.85 / 4.13 / **17.60** ms | 0.76 / 2.95 / **8.03** ms |
| read latency | 0.01 / 0.02 / **4.78** ms | 0.01 / 0.01 / **0.10** ms |
| GRV latency | 0.06 / 0.70 / 4.29 ms | 0.05 / 0.10 / 0.86 ms |
| bytes written | 29 / 78 / 793 kB/s | 27 / 75 / 697 kB/s |
| conflicts | 0 | 0 |
| `fdbserver` CPU | 0.21 / 0.74 / 0.81 cores | 0.19 / 0.38 / 0.42 cores |
| in-window samples | 108 of 245 | 106 of 245 |

Its backing, per point, so no cell of the table above can be a different point
from the one beside it:

| point | arm | commit ms | read ms | GRV ms | written kB/s |
|---|---|---|---|---|---|
| ic40k-r1 | memory | 1.98 / 3.37 / 8.03 | 0.01 / 0.01 / 0.10 | 0.06 / 0.12 / 0.86 | 34.1 / 76.9 / 696.6 |
| ic40k-r2 | memory | 2.12 / 3.27 / 6.47 | 0.01 / 0.01 / 0.07 | 0.05 / 0.11 / 0.44 | 33.9 / 74.7 / 689.4 |
| ic20k-r1 | memory | 0.94 / 3.13 / 7.39 | 0.01 / 0.01 / 0.02 | 0.06 / 0.10 / 0.33 | 31.8 / 71.9 / 196.3 |
| ic20k-r2 | memory | 0.76 / 2.42 / 4.81 | 0.01 / 0.01 / 0.02 | 0.05 / 0.09 / 0.26 | 26.6 / 74.6 / 193.8 |
| ic40k-r1 | **ssd** | 0.85 / 3.60 / 9.00 | 0.01 / 0.02 / 3.72 | 0.06 / 0.69 / 3.82 | 35.8 / 78.6 / 775.0 |
| ic40k-r2 | **ssd** | 1.89 / 4.72 / 8.11 | 0.01 / 0.03 / 4.78 | 0.56 / 1.51 / 4.29 | 29.4 / 65.8 / 194.4 |
| ic20k-r1 | **ssd** | 1.99 / 3.61 / 17.60 | 0.01 / 0.02 / 2.17 | 0.06 / 0.36 / 2.05 | 31.6 / 74.1 / 689.0 |
| ic20k-r2 | **ssd** | 2.18 / 4.33 / 8.03 | 0.01 / 0.02 / 3.42 | 0.20 / 0.58 / 2.80 | 33.1 / 79.4 / 793.0 |

**What this says.** The **read** tail is where `ssd` is unambiguously
different, and it is the one difference with a mechanism nothing else can
supply: `memory`'s storage reads never leave RAM, so its read latency never
leaves 0.01–0.10 ms, while `ssd`'s reaches 2.17–4.78 ms in **every one of its
four points** — a QLC B-tree read on a page-cache miss. Write volume is a
wash: both arms peak at 0.69–0.79 MB/s on the same run-start burst, both sit
at 32–129 kB/s from ten seconds in, and their medians differ by 4 % (78.0
against 74.7 kB/s). Commit latency sits between the two: `ssd`'s median is
above `memory`'s at **every** matched point, by 0.2–1.9 ms
(3.60/4.72/3.61/4.33 against 3.37/3.27/3.13/2.42, in the table's order), 1.2 ms
pooled, and its worst in-window sample is 2.2× `memory`'s worst. `fdbserver` CPU is ~2×. All of it
reaches intent latency as one histogram bucket (p50 15→20 ms, p90
75–100→100–150 ms, p99 200→300 ms at ~1 030 intents/s) and none of it moves
the saturation point, because the client thread runs out before FoundationDB
does.

> **Corrected 2026-08-19 — two figures, both against `ssd`, both from taking
> an extreme outside the arm's own measurement window while excluding the
> other arm's matching extreme.** They are left visible because the error is
> the kind that recurs: the sampler runs continuously across both clusters, so
> "the highest number in the file with this arm's name on it" is not the same
> thing as "this arm's cost", and nothing but a stated rule keeps them apart.
>
> **F1 — commit latency, published as `ssd` 27.92 ms against `memory`
> 8.03 ms** (3.5×), and quoted in §11's headline and in this section's prose.
> The 27.92 ms sample is at 21:59:04 UTC, **one second after** `ssd-ic40k-r1`'s
> window closed (21:58:03 → 21:59:03). At that same instant the **`memory`**
> cluster — idle, 0.6 commits/s, not under test — read **35.47 ms**. It is a
> box-wide inter-point event (teardown / keyspace clear / journal delete), not
> an engine cost, and the sampler caught two more of exactly that shape:
> 22:01:23 (`ssd` 36.39 / `memory` 36.51) and 22:03:38 (`ssd` 32.66 /
> `memory` 32.80), both also outside every window. In-window, `ssd`'s worst is
> 17.60 ms and `memory`'s is 8.03 ms — 2.2×, not 3.5×. Even that is at the
> edge of what this box supports as an engine difference: an **idle** cluster
> read 10.8–13.9 ms while the *other* arm was under load, and the `memory`
> arm itself read 18.46 ms one sample after its own window closed. And on the
> checkpoint leg (§11.8) — same instrument, same rule, a different load — the
> in-window commit maximum is 20.93 ms on `ssd` against **49.52 ms on
> `memory`**. A tail that swaps arms when the load changes is this box's
> device regime ([08-persistence.md](08-persistence.md) §4.3), not a storage
> engine.
>
> **F2 — bytes written, published as `ssd` 32/78/775 kB/s against `memory`
> 34/77/201** (3.9× on the maximum). `ssd`'s 775.0 is its **first in-window
> sample pair**, the lease-claim/seed burst at run start. `memory`'s published
> 200.8 is its **second** pair; its first pair reads **696.6** — the identical
> burst, excluded. Matched, both arms peak at 0.69–0.79 MB/s: the real gap is
> **14 % at the peak and 4 % in the median**, not 3.9×. The published row was also assembled from two different points:
> `ssd`'s minimum 31.6 comes from `ic20k-r1` while the whole `memory` column
> came from `ic40k-r1`. The per-point table above exists so that cannot recur.
>
> **Three claims moved, and not all in `ssd`'s favour.** The commit tail
> (3.5× → 2.2×) and the write volume (3.9× → 1.14×) get better for `ssd`; the
> read tail gets **worse** (37× → 48×), because the published `ssd` read
> maximum came from `ic40k-r1` alone while `ic40k-r2` — in-window, same arm —
> reads 4.78 ms. And "the medians are identical", which stood here, is
> retracted: it was true of `ic40k-r1` against `ic40k-r1` (3.60 vs 3.37) and
> is not true of the four matched points pooled (4.13 vs 2.95). `ssd` costs
> 1.2 ms of median commit latency pooled, 0.2–1.9 ms point by point — small,
> consistent in sign at all four points, and not zero.
>
> **The section's conclusion strengthens.** "FoundationDB the *server* is not
> the limit" rested on the largest excursion in the file being 27.9 ms against
> a path that saturates on a client thread at 94 %; that excursion was not
> `ssd`'s and was not even FoundationDB's, and the real in-window worst case
> is 17.6 ms. §11.10's "run `ssd`" is unaffected, and §11.6's medians — the
> 0.1 %-of-the-array result — were re-derived under the rule and did not
> move.

### 11.8 Checkpoints: a smear, not a pulse

The 20 s checkpoint writes the world to FoundationDB, so on `ssd` it is real
disk I/O beside the journal's. Finding it needs runs longer than a 30 s point,
which contains barely one wave: 120 s points, both arms, twice, intent rate
held at ~50/s so FDB commits are dominated by checkpoint work, with FDB's
`status json` sampled every 2 s alongside.

**There is no 20 s signature on either arm in anything measured.**
Autocorrelation of the FDB write rate at lag 20 s is 0.03 (`ssd`) and 0.04
(`memory`); of `persistd`'s per-second journal fsync cost, −0.04 and 0.00; of
the per-second journal-wait excursion, −0.03. Under §11's windowing rule, and
dropping each window's first 10 s so the lease-claim/seed burst is out of both
arms alike, the FDB write rate is flat at a median of **44.7 kB/s** (`ssd`;
1.5–89.4 across samples) and **46.0 kB/s** (`memory`; 1.6–93.4), at 48.6–79.2
and 49.6–80.2 commits/s.

> **Corrected 2026-08-19.** The bands published here were "42–65 kB/s (`ssd`)
> and 34–61 kB/s (`memory`), at 50–66 commits/s" — narrower than anything a
> stated rule reproduces, and asymmetric between the arms in a direction the
> data does not support: the medians are 44.7 (`ssd`) and 46.0 (`memory`), so
> `memory` writes marginally *more* here, not less. The conclusion is
> untouched — ~45 kB/s against the journal's ~70 MB/s, no 20 s pulse on either
> arm, and the two arms indistinguishable.

That is the design working rather than an absence of checkpoints:
`spawn_checkpoint_scheduler` jitters each shard's period over
`[interval − jitter, interval + jitter]` = **[15 s, 25 s]**, seeded from the
shard id, so 128 shards checkpoint at 128 different phases. The aggregate is
deliberately smooth — and at ~50 kB/s against the journal's ~70 MB/s it could
not be seen in tail latency even if it were pulsed.

One caution for anyone repeating this. Both 120 s runs of the *first* repeat
show a ~16 s window, starting ~20 s in, where per-flush fsync cost drops from
~2.5–3.5 ms to ~0.48 ms — on **both** arms, and not at the same offset in the
second repeat. It is the device's two regimes
([08-persistence.md](08-persistence.md) §4.3). A single 120 s run would have
read it as a periodic signature.

### 11.9 The concurrency leg

The rate at 2 Hz, sessions raised alone, as in §3.2 — but run *after* the rig
fix, so the 3 % default mix now means ~545 intents/s rather than 1 024 intents
in the first second. That makes this leg a joint bulk+intent load, and it is
not comparable to §3.2's numbers; it is comparable across arms, which is what
it is here for. One repeat per point, 30 s:

| sessions | arm | delivered/s | shed % | intents | intent p50 | persistd cores | rig cores | FDB client thread |
|---|---|---|---|---|---|---|---|---|
| 500 | memory | 18 550 | 0.004 | 16 358 | 15 ms | 1.50 | 0.59 | 36.0 % |
| 500 | **ssd** | 18 583 | 0.005 | 16 400 | 15 ms | 1.58 | 0.61 | 40.5 % |
| 1 000 | memory | 18 592 | 0.006 | 16 396 | 15 ms | 1.60 | 0.77 | 38.1 % |
| 1 000 | **ssd** | 18 610 | 0.005 | 16 420 | 15 ms | 1.65 | 0.75 | 41.2 % |
| 2 000 | memory | 18 683 | 0.006 | 16 467 | 20 ms | 1.74 | 1.09 | 38.6 % |
| 2 000 | **ssd** | 18 673 | 0.014 | 16 459 | 30 ms | 1.72 | 1.02 | 41.1 % |
| 3 000 | memory | 18 630 | 0.015 | 16 415 | 30 ms | 1.80 | 1.37 | 36.9 % |
| 3 000 | **ssd** | 18 687 | 0.027 | 16 473 | 40 ms | 1.76 | 1.28 | 39.6 % |

Six-fold the connections and the delivered rate does not move (18.55–18.69 k)
and `leases_lost` stays 0. What connection count costs is CPU on both sides:
0.30 cores of `persistd` and 0.78 cores of rig between 500 and 3 000 sessions.
The `ssd` arm's intent p50 is one bucket above `memory`'s at 2 000 and 3 000
sessions, consistent with §11.7 and with nothing else in this leg.

> **Corrected 2026-08-19, same correction as §11.4.** This paragraph also read
> *"shedding rises from 0.004 % to 0.027 % — still 37× below its threshold"*,
> and drew the rise as a cost of connection count. It is the same artifact: at
> 500 sessions the gap between decided and completed audits is 25–28 and
> `shed_slow_route` is 25–28; at 3 000 it is 84–150 and `shed_slow_route` is
> 84–150. What rises with connection count is the audit's failure rate against
> a 25 ms budget, because more connections means more accepts and therefore
> more samples — not the gateway's willingness to serve them. The shed column
> is left in the table as what the counter recorded; the leg's conclusion,
> which is that delivered rate and `leases_lost` do not move, does not depend
> on it.

### 11.10 Which engine should a demo run, and why

**Run `ssd`.** §7 already required it for durability — the `memory` engine
loses anything its transaction log cannot replay — and this section removes the
capacity objection that made that requirement uncomfortable. At the P2
operating point the engines are indistinguishable on the bulk path; what `ssd`
costs is 0.1 of one core of `fdbserver` CPU, ~1.2 ms of median FDB commit
latency, one histogram bucket of intent tail, and a read tail that only matters
if you are running the intent path near its saturation point. There is no throughput reason to prefer `memory`, and
there is a durability reason not to.

Two operator notes specific to `ssd`:

* Its data directory is real disk on the same array as the journal. It is
  small — and it grows with the size of the world, not with the diff rate.
  (The "FDB wrote at most 0.8 MB/s in any sample here against the journal's
  ~70 MB/s" that stood here is true, but it is **not** specific to `ssd`:
  corrected 2026-08-19, both arms peak at 0.69–0.79 MB/s on the same
  run-start burst and both sit near 78/75 kB/s in the median — §11.7 F2. It
  belongs in §11.6, where it is a statement about FoundationDB on this box,
  not a caution about this engine.)
* `fdbserver` CPU is the number that moves: budget ~2× the `memory` arm's, and
  watch it against **intent** rate, not diff rate.

**Independently supported on other hardware, for the bulk path only.** Sixteen
interleaved `p2-kill9` runs on a datacenter NVMe (8 per engine,
[08-persistence.md](08-persistence.md) §4.4) could not separate the engines on
*any* gated series — identical medians on `journal_commit_ms` and
`bulk_ack_ms`, overlapping ranges on all four. That corroborates "the engines
are indistinguishable on the bulk path" on a second machine. It does **not**
corroborate §11.7: that run read `intent_commit_ms` slightly *better* on `ssd`
(med 9.5 vs 17.5 ms), on n=8 per arm with overlapping ranges and none of the
FDB-internal counters §11.7 rests on. Treat §11.7 as unreplicated rather than
contradicted, and note that the recommendation above never depended on it.

And one that applies to both engines, from §11.7: the operator check in §9 —
"FDB client thread utilisation > 60 % of one core" — is still the right check
and still trips first, but on this build it is driven by intents. At ~1 000
intents/s it reads 66–75 %; at ~1 300 it reads 94 % and intent p50 is 750 ms.
Divide by ~13 to convert intents/s into "% of that thread" on a 128-shard node.

### 11.11 Reproducing

```bash
# two throwaway clusters, one per engine, on their own ports and data dirs
for arm in ssd memory; do
  port=$([ "$arm" = ssd ] && echo 4601 || echo 4602)
  docker run -d --name orrery-fdb-$arm --network host \
    -e FDB_PORT=$port -e FDB_NETWORKING_MODE=host -e FDB_COORDINATOR_PORT=$port \
    -e FDB_CLUSTER_FILE_CONTENTS="$arm:$arm@127.0.0.1:$port" \
    -v /some/dir-$arm:/var/fdb/data foundationdb/foundationdb:7.3.63
  docker exec orrery-fdb-$arm fdbcli --exec "configure new single $arm"
  fdbcli -C /some/$arm.cluster --exec 'status minimal'    # must say available
done

cargo build --release -p orrery_persistd --features fdb -p orrery_seed --features orrery_seed/fdb
(cd p2-load && cargo build --release)

export P2_CAP_OUT=$PWD/sweep PERSISTD_BIN=target/release/persistd
export ORRERY_SEED_BIN=target/release/orrery-seed P2_LOAD_BIN=p2-load/target/release/p2-load
export SSD_CLUSTER_FILE=/some/ssd.cluster MEM_CLUSTER_FILE=/some/memory.cluster
export SSD_FDB_CONTAINER=orrery-fdb-ssd MEM_FDB_CONTAINER=orrery-fdb-memory

# rate leg: two repeats, arms interleaved, sessions at 2x the fan-out cap
scripts/fenced-ssd-driver.sh 2 r20k:250:2 r40k:500:4 r60k:750:6 r80k:1000:8 \
                               r120k:1500:12 r160k:2000:16 r200k:2500:20
# intent leg: the fifth field is --intent-mix (P2_CAP_INTENT_MIX)
scripts/fenced-ssd-driver.sh 2 ib40k-hi:500:4:30:trade=0.02,craft=0.01
# checkpoint leg: 120 s, so a shard checkpoints five times inside one run
scripts/fenced-ssd-driver.sh 2 k40k:500:4:120:trade=0.001,craft=0.0005

python3 scripts/fenced-sweep-report.py sweep/*/     # delivered, never nominal
```

`fenced-ssd-driver.sh` alternates which arm runs first on even repeats;
`fenced-sweep-report.py` folds whatever arm names the directories carry, warns
when a point delivered under 95 % of nominal, and prints `delivered_per_s`
beside `rig_cap_per_s` so a rig-limited row cannot be read as a box result. It
also prints `audits_vanished` and `audit_shed_gap` and warns when a point's
`shed_slow_route` is accounted for by cancelled invariant-J audits — the
defect §11.2 describes — so a JSONL captured on a binary between #86 and
2026-08-19 cannot be read as bulk shed again.

**Instrumentation used here that the harness does not do for you:** the
per-thread `pidstat` for `p2-load` (the harness samples `persistd`'s threads
only), and `fdbcli --exec 'status json'` sampled every 2 s for FDB's own commit
and read latency, conflict rate and write volume. Both are two-line additions
around a run; §11.5 through §11.8 rest on them.

**Reduce the FDB samples with the windowing rule, not by eye.** One sampler
covers both clusters continuously, so a per-arm extreme has to be cut to that
arm's own points before it means anything — §11.7's F1 and F2 are what happens
when it is not:

```bash
# the section 11.7 table, both arms, one rule, with the excluded samples named
scripts/fdb-status-window.py logs/decomp-fdb-status.jsonl sweep/ ic --per-point
# section 11.8's steady-state band: same rule, run-start burst out of both arms
scripts/fdb-status-window.py logs/ckpt-fdb-status.jsonl sweep/ k40k --skip-secs 10
```

**One warning if you re-run the rate leg on a current rig.** Its numbers were
taken with the pre-fix `p2-load`, whose intent queue capped a run at 1 024
intents; the intent load in those rows is therefore negligible. On the fixed
rig the same 3 % default mix at 140 k diffs/s asks for ~4 000 intents/s, which
is three times the intent path's saturation point (§11.7) and turns a bulk
sweep into an intent sweep. Set `P2_CAP_INTENT_MIX` low when what you want is
the bulk path.
