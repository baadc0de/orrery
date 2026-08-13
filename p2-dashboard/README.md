# p2-dashboard

The **latency gate** for the P2 persistence MVP
([docs/11-roadmap.md](../docs/11-roadmap.md) §P2). It consumes the JSONL
telemetry emitted by `p2-load --json` and reports the four D16 latency series
against the demo-criterion targets verbatim
([docs/DECISIONS.md](../docs/DECISIONS.md) D16):

| series               | D16 target (p99, in-region) |
|----------------------|-----------------------------|
| `journal_commit_ms`  | < 2 ms (server-internal)    |
| `bulk_ack_ms`        | < 5 ms (client-observed)    |
| `intent_commit_ms`   | < 10 ms                     |
| `area_first_page_ms` | < 50 ms                     |

The sibling tool is [`p0-dashboard`](../p0-dashboard/README.md): same shape
(read JSONL, compute percentiles, print a pass/fail table, `--gate` exits
non-zero), different numbers.

## What it does

Reads one or more `.jsonl` files from `p2-load --json`. The stream is one JSON
object per line:

- `{"type":"run_header","run":{...}}` — run context (gateway id, entity/cell/
  session counts, duration). Echoed into the report so a viewer knows which
  run the numbers came from.
- `{"type":"sample","series":"<name>","value_us":<u64>}` — one raw latency
  sample. Samples are bucketed into the **bounded-memory D16 histogram** from
  `orrery_persist_client::latency` — the same recorder the rig uses live — so
  percentiles resolve within one bucket width of the true value in constant
  memory (a 30-minute soak at 10k entities × 4 Hz is ~72M samples; nothing is
  materialized into a `Vec` and sorted, on either side of the wire).
- `{"type":"run_footer","note":"..."}` — end-of-run marker; counted, not gated.

Per series the report carries `n`, `p50_us`, `p99_us`, `max_us`, the threshold
it was gated against, and a per-series verdict (`pass` / `fail` /
`missing_data`). A series with **no samples fails the gate** — the D16 demo
criterion requires all four series measured, and an empty series cannot pass
by omission.

## Build

```sh
cargo build --release --manifest-path p2-dashboard/Cargo.toml
# binary: p2-dashboard/target/release/p2-dashboard
```

## Usage

```sh
# Human report
./p2-dashboard run.jsonl [run2.jsonl ...]

# Machine-readable (the stable CI contract)
./p2-dashboard --json run.jsonl

# Gate: exit non-zero when any p99 misses its D16 target
./p2-dashboard --gate run.jsonl
```

### Gating

`--gate` compares each series' **p99** against its threshold and exits
non-zero on the first miss (including a missing series). Per-series overrides:

```sh
./p2-dashboard --gate --bulk-ack-ms 8000 run.jsonl
```

The `--json` `Report` is the stable machine contract:

```json
{
  "records": 402,
  "malformed": 0,
  "gate": "pass",
  "run": { "gateway": "ea4a…", "entities": 10000, "cells": 128, "sessions": 6,
           "diff_hz": 2.0, "intent_mix": {"trade": 0.02, "craft": 0.01},
           "duration_secs": 30 },
  "series": {
    "bulk_ack_ms":      { "n": 100, "p50_us": 2000, "p99_us": 3000,
                          "max_us": 3000, "threshold_us": 5000, "gate": "pass" },
    "journal_commit_ms": { "n": 100, "p50_us": 1000, "p99_us": 1000,
                          "max_us": 900,  "threshold_us": 2000, "gate": "pass" },
    "…": "…"
  }
}
```

## Options

| Flag | Default | Meaning |
|---|---|---|
| `files` | — | One or more `.jsonl` telemetry files |
| `--json` | off | Emit the machine-readable `Report` instead of the human table |
| `--gate` | off | Exit non-zero when any series misses its threshold |
| `--journal-commit-ms` | 2000 µs | Override the journal-commit p99 threshold |
| `--bulk-ack-ms` | 5000 µs | Override the bulk-ack p99 threshold |
| `--intent-commit-ms` | 10000 µs | Override the intent-commit p99 threshold |
| `--area-first-page-ms` | 50000 µs | Override the area first-page p99 threshold |

## Test data

`testdata/demo.jsonl` is a synthetic 30 s run whose four series all land
below their D16 targets, so the dashboard is exercisable without a cluster:

```sh
./p2-dashboard testdata/demo.jsonl          # prints the human table
./p2-dashboard --gate testdata/demo.jsonl   # exit 0
```

The unit tests (`cargo test --manifest-path p2-dashboard/Cargo.toml`) cover
the gate contract: conforming data passes, a regressed series fails, a single
outlier above the threshold does **not** fail (the gate reads p99, not max),
and a missing series fails by omission.

## What this does not claim

`journal_commit_ms` is a **server-internal** number (D16): the rig cannot
observe it over the wire, so the series is only as honest as the gateway that
reported it. The P2 build has no journal-latency wire message — the demo
runbook sources those samples from the `persistd` operator's log/metrics
pipeline and appends them to the JSONL stream before gating. If no journal
samples are present the gate fails with `missing_data`, which is the intended
posture.
