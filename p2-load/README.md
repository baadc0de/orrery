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
- **Claims a lease per entity, before any load.** Every bulk write the
  gateway routes is fenced (`route_session_diff` sets
  `strict_authority: true`, unconditionally), so a diff without a granted
  `(lease_id, authority_seq)` is rejected before the journal. The rig sends a
  strong `Explicit` `LeaseMsg::Claim` per entity on the session that will
  write it, paced by the registrar's own per-`NodeId` bucket (64 burst, then
  20/s), and renews with a batched `Heartbeat` every 3 s against the 10 s
  TTL. The phase costs about two seconds at any scale up to the demo's
  10 000 entities. **There is no unleased write path**: a denied claim, a
  refused renewal, or a lease-bearing NACK fails the run. That fallback is
  precisely how a run of 541 408 rejections passed for a durability
  measurement.
- **Needs a seeded world.** A claim names the entity's *committed* cell and
  the registrar refuses one it cannot resolve, so an entity that was never
  journaled cannot be claimed and therefore cannot be written. Seed with
  `orrery-seed` and pass the emitted manifest as `--manifest`; `--entities`
  synthesizes a placement that only works against a world seeded to match it
  (`persistd --dev-seed <count>@<cell>` for a volatile harness).
- **Writes at the committed cell.** Entities do not move between cells. A
  leased writer cannot: `apply_fenced` admits a diff only where
  `by_cell[entity] == record.cell`, and the gateway answers a client-sent
  `LeaseMsg::Rekey` with an unconditional `Deny{NotEligible}`. Cross-cell
  coverage comes from the *placement* spanning ≥ `--cells` distinct cells,
  which is where that guarantee always lived.
- **One NodeId per session.** The gateway's peer registry is keyed by
  `NodeId` and only a peer's newest session is current, so N connections from
  one endpoint leave N−1 sessions whose every diff is nacked before routing.
  Each session binds its own endpoint; with `--secret-key` the family of
  identities is derived deterministically from it.
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

`sample.series` is one of the four gated D16 keys. Three of them the rig
measures itself, from the client side; the fourth it cannot see:

| series               | source                                                  |
|----------------------|---------------------------------------------------------|
| `journal_commit_ms`  | **persistd** — the journal's group-commit recorder, appended by `--metrics-jsonl`; see "What this does not claim" |
| `bulk_ack_ms`        | this rig — `UplinkScheduler::on_ack` (send → durable-ack round trip) |
| `intent_commit_ms`   | this rig — `IntentQueue::on_ack` (submit → commit round trip)      |
| `area_first_page_ms` | this rig — `Subscribe` send → first `AreaPage`, per session        |

persistd appends three further series to the same artifact, none of which the
rig ever emits and none of which D16 gates:

| series                             | source                                                        |
|------------------------------------|---------------------------------------------------------------|
| `gateway_bulk_server_ms`           | persistd — diff receipt → ack send call                        |
| `gateway_intent_server_ms`         | persistd — `SubmitIntent` receipt → reply send call             |
| `gateway_area_first_page_server_ms`| persistd — `Subscribe` receipt → first `AreaPage` send call     |

Each is the server-side half of the gated round trip above it, and each has
its **own name on purpose**. `p2-dashboard` folds by series name into one
histogram per name, with no source field, and `scripts/p2-kill9-gate.sh`
concatenates this rig's file with persistd's before gating — so a server span
recorded under a gated name would be folded into the client's histogram,
*lower* the gated p99 (a server span is strictly shorter than the round trip
containing it), and pass a gate it never measured. The gate folds and reports
these three and never fails on them.

persistd also appends non-latency counter records — `gateway_authority`,
`gateway_bulk_stage_delta`, `gateway_intent`, `gateway_area` and
`gateway_report` — which carry no `series` field and which the dashboard
ignores.

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

The rig writes under a lease, and a lease needs an entity that already
exists durably, so **the world is seeded first**. Two ways, and only the
first is a durable one:

```sh
# Durable: seed FDB, then take the seeder's manifest as the inventory.
ORRERY_FDB_CLUSTER_FILE=/etc/foundationdb/fdb.cluster \
  orrery-seed apply crates/orrery_seed/scenarios/p2demo.toml \
  --profile demo --allow-opaque --single-grid
ORRERY_FDB_CLUSTER_FILE=/etc/foundationdb/fdb.cluster \
  orrery-seed verify crates/orrery_seed/scenarios/p2demo.toml \
  --profile demo --single-grid --emit-manifest manifest.json

persistd --dir /tmp/p2-node --secret-key <hex> \
  --fdb-cluster-file /etc/foundationdb/fdb.cluster --issuer-key 1@<pub>
# …prints {"node_id":"…","bind_addr":"…"} on stdout.

p2-load --gateway <node_id> --addr 127.0.0.1:7777 \
        --manifest manifest.json --sessions 125 \
        --issuer-secret <hex> --issuer-key-id 1 \
        --duration-secs 60 --json --ack-log acks.jsonl > run.jsonl

# Gate it:
p2-dashboard --gate run.jsonl
```

```sh
# Volatile smoke run: --dev-seed spawns placeholder entities through the
# actor, which is what gives them a committed cell. It is refused whenever
# --fdb-cluster-file is set, so this configuration is not durable and is not
# the gate.
persistd --dir /tmp/p2-node --secret-key <hex> --allow-volatile-leases \
  --issuer-key 1@<pub> --dev-seed 1000@0,0,0@21
p2-load --gateway <node_id> --addr 127.0.0.1:7777 \
        --entities 1000 --cells 1 --sessions 13 \
        --issuer-secret <hex> --issuer-key-id 1 \
        --duration-secs 60 --json --ack-log acks.jsonl > run.jsonl
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
