# p2-load

The **latency rig** for the P2 persistence MVP
([docs/11-roadmap.md](../docs/11-roadmap.md) §P2): a gateway-colocated load
generator that drives the demo load — 10 000 entities across 100+ cells at a
calibrated diff and intent mix — against a real `persistd` gateway and emits
one JSON record per line for the four D16 latency series. The sibling tool,
[`p2-dashboard`](../p2-dashboard/README.md), reads that stream and exits
non-zero when any p99 misses its D16 target.

The shape mirrors the P0 precedent ([`p0-nat-test`](../p0-nat-test/README.md)):
one binary, one `--json` contract, tracing on stderr, JSON on stdout.

## What it does

- **Dials the real gateway.** Raw iroh, ALPN `orrery/gateway/0`, against the
  `persistd` gateway's wire surface (admission uni-stream, then tagged
  datagrams + stream-framed control frames on one packet lane — roadmap
  decision C-1: there is no reliable-stream class in P2). The rig refuses to
  run against a gateway whose `HelloAck` names a different NodeId than
  `--gateway`.
- **Registers entities in the real scheduler.** `--entities` synthetic
  entities are registered in the D16 1–4 Hz `UplinkScheduler`
  (`orrery_persist_client`) at `--diff-hz`, flushed at 20 Hz against the D16
  1024-byte flush budget (`size = payload + 64`), and fanned out over
  `--sessions` iroh connections.
- **Moves entities across cells.** Movement is a closed-form trajectory
  program (docs/12-world-seeding.md §12.3): each entity walks a small circle
  centered on its inventory cell, radius just over one cell, so cell
  crossings are continuous and the diff stream exercises cross-cell routing.
  A recorded trace of 10k entities at 60 Hz for 30 minutes is gigabytes; the
  program is a few hundred bytes.
- **Interleaves intents.** `--intent-mix trade=0.02,craft=0.01` upgrades 2%
  of diff sends to a `trade` intent and 1% to a `craft` intent (the §12.3
  `intent_mix` semantics: fractions of the diff rate, not additive). Intents
  go through the real `IntentQueue` (idempotency-keyed, at-least-once, C-1);
  the P2 gateway's intent path is a stub (signature → `Ruleset` stub →
  optimistic commit), so the intent stream measures *commit latency*, not
  validation.
- **Measures with the shipped code.** Bulk-ack latency is sampled in
  `UplinkScheduler::on_ack`, intent-commit latency in `IntentQueue::on_ack`,
  and both live in the bounded-memory `LatencyHistogram`
  (`orrery_persist_client::latency`) — constant memory at 10k × 4 Hz × 30
  min, no `Vec`-and-sort anywhere. Area first-page-in is measured from
  `Subscribe` send to the first `AreaPage` per session.
- **Asserts the fan-out at startup.** One session sustains
  `1024 / (payload + 64)` diffs per flush × 20 flushes/s. If
  `sessions × capacity < entities × diff_hz` the rig refuses to run with a
  clear message, rather than silently reporting queueing delay as commit
  latency.
- **Dumps durable evidence.** With `--ack-log <path>`, every non-provisional
  bulk ack is appended with `(grid, cell, entity, tick, lsn, payload_digest)`;
  every intent ack carries its lossless idempotency key and exact known outcome.
  Provisional bulk acks are deliberately excluded. The recovery comparator
  asserts the final acknowledged write per entity and every intent outcome.

## JSONL contract (what `--json` emits)

One JSON object per line on stdout; logs on stderr. Three record kinds:

```json
{"type":"run_header","run":{"gateway":"<hex NodeId>","addr":"127.0.0.1:7777",
 "entities":10000,"cells":128,"sessions":6,"diff_hz":2.0,
 "intent_mix":{"trade":0.02,"craft":0.01},"duration_secs":1800}}
{"type":"sample","series":"bulk_ack_ms","value_us":2000}
{"type":"sample_batch","series":"journal_commit_ms","value_us":1000,"count":64}
{"type":"run_footer","note":"duration elapsed; diffs=… acks=… intents=…"}
```

`sample.series` is one of the four gated D16 keys:

| series               | source                                                  |
|----------------------|---------------------------------------------------------|
| `journal_commit_ms`  | **server-internal** — see "What this does not claim"    |
| `bulk_ack_ms`        | `UplinkScheduler::on_ack` (send → durable-ack round trip) |
| `intent_commit_ms`   | `IntentQueue::on_ack` (submit → commit round trip)      |
| `area_first_page_ms` | `Subscribe` send → first `AreaPage`, per session        |

persistd appends a fifth series, `gateway_bulk_server_ms` (receipt through
send call, the server-side half of `bulk_ack_ms`), to the same artifact. D16
sets no target for it; `p2-dashboard` folds and reports it and never gates on
it. The rig itself never emits it.

The names, the bucket boundaries and the reconstruction rule are **one
definition** — `orrery_protocol::metrics`, re-exported through
`orrery_persist_client::latency` — shared by the rig, persistd's journal and
gateway recorders, and the gate. There is no per-tool copy to keep in step.

`value_us` is the **bucket upper bound** of the rig-side histogram bucket the
sample landed in — the same bucketing `LatencyHistogram`'s own percentile
methods report, so the gate (`p2-dashboard`) reconstructs percentiles that
agree bucket-for-bucket with the rig's live view. The lattice refines the
sub-2 ms band (…, 1000, 1250, 1500, 1750, 2000, …) precisely because that is
the band the `journal_commit_ms` target gates on: with 1 ms and 2 ms adjacent,
every p99 in between read out as exactly the 2 ms threshold. `CellId`,
`PersistId` and `Tick` serialize as their plain numeric newtype values (the
wire form).

## Usage

```sh
# Start a single-node gateway (in-memory stores for a smoke run; add
# --fdb-cluster-file for the durable tier):
persistd --dir /tmp/p2-node --secret-key <hex>
# …prints {"node_id":"…","endpoint_addr":"EndpointAddr { … }"} on stdout.

# Run the load (values from the persistd line above):
p2-load --gateway <node_id> --addr 127.0.0.1:7777 \
        --entities 1000 --cells 128 --sessions 13 \
        --duration-secs 60 --json --ack-log acks.jsonl > run.jsonl

# Gate it:
p2-dashboard --gate run.jsonl
```

For the P2 static mirror topology, start the follower first, then the primary
with the follower's reported `chain_addr` (both sides must use the same shard
list, if supplied):

```sh
persistd --dir /tmp/p2-follower --node-id 2 --chain-epoch 1 \
  --chain-primary 1 --chain-listen 127.0.0.1:7002
persistd --dir /tmp/p2-primary --node-id 1 --chain-epoch 1 \
  --chain-follower 2@127.0.0.1:7002
```

The primary still acknowledges bulk writes from its own journal; the follower
is asynchronous and never exposes a client gateway. The topology improves
recovery coverage but does not yet implement primary promotion or the full P2
kill-9 comparator.

The default `--entities 10000 --diff-hz 2` needs ≥ 125 sessions at the
default 64-byte payload (10 000 × 2 Hz = 20 000 diffs/s vs 160 diffs/s per
session); the startup assert enforces this rather than letting you measure a
queue. For a quick smoke, `--entities 1000 --sessions 13` clears the bar.

## Two-process kill-9 gate

`scripts/p2-kill9-gate.sh` is the P2 crash/recovery regression harness. It
requires an FDB-enabled `persistd`, `p2-load`, `p2-evidence-verify`, and
`p2-dashboard` binary plus `ORRERY_FDB_CLUSTER_FILE`:

```sh
ORRERY_FDB_CLUSTER_FILE=/path/to/fdb.cluster \
PERSISTD_BIN=target/release/persistd \
P2_LOAD_BIN=p2-load/target/release/p2-load \
P2_DASHBOARD_BIN=p2-dashboard/target/release/p2-dashboard \
scripts/p2-kill9-gate.sh
```

The harness starts a passive static follower before the fenced primary, drives
the calibrated 10k-entity/128-cell load and durable ack log, sends the primary
`SIGKILL`, and starts the follower with `--promote-from 1`. The verifier
compares the promoted gateway/FDB state to the eligible acknowledgements at the
reported recovery cutoff: final bulk write per entity and every intent outcome.
It then proves a new process carrying the old primary identity cannot pass FDB
fence admission. Finally it folds the primary/promoted `--metrics-jsonl`
records into the load JSONL and invokes `p2-dashboard --gate` for all four D16
series. Only then does it write `artifact.json` with `"result": "pass"`.

The script will never overwrite an existing output directory (`P2_GATE_OUT`)
and its exit trap terminates surviving child processes. `--self-test` is an
offline structural test for the required proof stages; it is not a durability
claim.

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--gateway` | — | The gateway's NodeId (required) |
| `--addr` | — | The gateway's socket address (required) |
| `--entities` | 10000 | Synthetic entity count |
| `--cells` | 128 | Minimum distinct interest cells the inventory spans |
| `--diff-hz` | 2.0 | Per-entity diff rate (D16 range 1–4 Hz) |
| `--intent-mix` | `trade=0.02,craft=0.01` | `kind=fraction` pairs, fractions of the diff rate |
| `--sessions` | 6 | Concurrent gateway sessions (fan-out; see the assert) |
| `--duration-secs` | 30 | Run duration |
| `--manifest` | — | Seeder manifest (JSONL, docs/12 §9.3) for the entity/cell inventory |
| `--scenario` | — | Scenario TOML whose `[[workload]]` block overrides `diff_hz`/`intent_mix`/`duration` (docs/12 §12.3) |
| `--json` | off | Emit the JSONL stream on stdout |
| `--ack-log` | — | Append-only ack log for the kill-9 harness |
| `--diff-payload-bytes` | 64 | Diff payload size (`size = payload + 64` in the flush budget) |
| `--secret-key` | — | Rig-local iroh key (hex), pinning its NodeId across runs |

## What this does not claim

- **The OTel bridge (D12) is deferred.** This crate adds no `opentelemetry`
  dependency — that stack would be a new D14 pinned dependency, which is an
  orchestrator decision. The JSONL contract above is the delivered telemetry
  mechanism; the `tracing` logs on stderr are diagnostic only and are not the
  D12 bridge.
- **`journal_commit_ms` is server-internal.** The D16 target (< 2 ms) is a
  property of the persistd journal's group commit; the wire has no message
  for it and the rig cannot observe it. The demo runbook sources those
  samples from the gateway operator's log/metrics pipeline and appends them
  to the JSONL stream before gating. With no journal samples the gate fails
  with `missing_data` — the intended posture, since the demo criterion
  requires all four series measured.
- **The intent path is a stub end-to-end.** The gateway accepts wire-shaped
  intents without signature/K-of-N/`Ruleset` validation (P5 work), so the
  rig's intents are wire-shaped and empty of game ops. What is measured is
  the *commit latency* the P2 demo criterion gates on.
