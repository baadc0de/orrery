# gates/p0-dashboard

The **punch-rate dashboard** for the P0 transport spike
([docs/11-roadmap.md](../../docs/11-roadmap.md) §P0). It aggregates the JSONL
telemetry emitted by `gates/p0-nat-test --json` into the permanent regression
artifact the demo criterion wants: a direct-path rate and direct-bytes fraction
compared against iroh's production baseline (~90% direct connections, ~95% of
bytes on direct paths).

## What it does

Reads one or more `.jsonl` files from `gates/p0-nat-test --json`, correlates the host
and peer sides of each unordered pair (via the `remote` NodeId in `connected`
records), and reports:

- **direct pairs / relay-only pairs** — how many pairs reached a direct path.
- **direct rate** — direct pairs ÷ total pairs, vs the iroh ~90% baseline.
- **direct bytes** — datagrams sent over direct paths ÷ total, vs the ~95%
  baseline (the relayed peer's traffic is the expected tail).
- **dropped** — total datagram drops across all pairs (the soak's `dropped=0`).
- **ttd p50** — median time-to-direct-path (ms).
- **rtt p50/p95** — roundtrip latency percentiles (µs).

## Build

```sh
cargo build --release
# binary: target/release/p0-dashboard
```

## Usage

```sh
# Human report
./p0-dashboard soak0.jsonl soak1.jsonl ... soak7.jsonl

# Machine-readable (for CI)
./p0-dashboard --json soak*.jsonl

# Gate (exit non-zero below threshold)
./p0-dashboard --gate soak*.jsonl
```

### Gating

The iroh baselines are population numbers. A small soak with one forced-relay
peer (e.g. 7 pairs, 6 direct = 85.7%) lands below the ~90% baseline *by
design* — the relayed peer is a product requirement, not a failure. Set the
thresholds for the sample size:

```sh
./p0-dashboard --gate --min-direct-rate 0.85 --min-direct-bytes 0.85 soak*.jsonl
```

`--gate` exits non-zero if either metric is below its threshold, so it can gate
CI. The `--json` output is the stable machine contract:

```json
{
  "records": 35,
  "malformed": 0,
  "pairs": 7,
  "direct_pairs": 6,
  "relay_only_pairs": 1,
  "direct_rate": 0.857,
  "direct_bytes": 0.857,
  "dropped": 0,
  "ttd_ms": 90,
  "rtt_p50_us": 72,
  "rtt_p95_us": 110
}
```

## Options

| Flag | Default | Meaning |
|---|---|---|
| `files` | — | One or more `.jsonl` telemetry files |
| `--json` | off | Emit the machine-readable summary instead of the human report |
| `--gate` | off | Exit non-zero if a metric is below its threshold |
| `--min-direct-rate` | `0.90` | Gate threshold for the direct-path rate |
| `--min-direct-bytes` | `0.95` | Gate threshold for the direct-bytes fraction |

## Test data

`testdata/soak.jsonl` is a synthetic 7-pair soak (6 direct, 1 forced-relay)
mirroring the real 8-node GCP result, for exercising the dashboard offline:

```sh
./p0-dashboard testdata/soak.jsonl
```