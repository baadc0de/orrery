# p2-dashboard

The **latency gate** for the P2 persistence MVP
([docs/11-roadmap.md](../docs/11-roadmap.md) §P2). It consumes the JSONL
telemetry emitted by `p2-load --json` and reports the four D16 latency series
against the demo-criterion targets verbatim
([ADR-0016](../docs/adr/0016-parameter-reference.md)):

| series               | D16 target (p99, in-region) |
|----------------------|-----------------------------|
| `journal_commit_ms`  | < 2 ms (server-internal)    |
| `bulk_ack_ms`        | < 5 ms (client-observed)    |
| `intent_commit_ms`   | < 10 ms                     |
| `area_first_page_ms` | < 50 ms                     |

A fifth series, `gateway_bulk_server_ms`, is the server-side half of
`bulk_ack_ms`: persistd measures receipt-through-send-call and appends it to
the same artifact, so a bulk-ack regression can be attributed to server work
or to the wire without re-running. D16 sets no target for it, so it is folded
and reported with `"gate": "not_gated"` and never contributes to the verdict —
present or absent.

Five more, `client_bulk_{queue,send,wire,dispatch}_ms` and
`client_quic_rtt_ms`, are the client side of the same attribution: the rig's
own scheduler wait, its send path, the socket-write-to-reply span, its ack
handling, and QUIC's own path RTT. `bulk_ack_ms` is `send + wire` exactly, so
the five say which side of the socket a bulk-ack tail is on. Ungated, for the
same reason and with the same consequence: present or absent, they never
change the verdict. Their names live in `orrery_persist_client::latency`
rather than `orrery_protocol::metrics` only because that crate was frozen when
they were added; `CLIENT_UNGATED_SERIES` documents the move.

The series names, the histogram boundaries and the bucket-reconstruction rule
are **one definition**, `orrery_protocol::metrics`, re-exported through
`orrery_persist_client::latency` and shared with `p2-load` and persistd. That
is what makes a percentile reported here the same number the rig measured.

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
  sample; or `{"type":"sample_batch","series":"<name>","value_us":<u64>,"count":<u64>}`
  for `count` identical samples. The latter is used by persistd journal group
  commit telemetry, so its counts remain part of the same run artifact.
  Samples are bucketed into the **bounded-memory D16 histogram** from
  `orrery_persist_client::latency` — the same recorder the rig uses live — so
  percentiles resolve within one bucket width of the true value in constant
  memory (a 30-minute soak at 10k entities × 4 Hz is ~72M samples; nothing is
  materialized into a `Vec` and sorted, on either side of the wire).
- `{"type":"run_footer","note":"..."}` — end-of-run marker; counted, not gated.

Per series the report carries `n`, `p50_us`, `p99_us`, `max_us`, the threshold
it was gated against (`null` for an ungated series), and a per-series verdict
(`pass` / `fail` / `missing_data` / `not_gated`). A **gated** series with no
samples fails the gate — the D16 demo criterion requires all four measured,
and an empty series cannot pass by omission.

A `sample` or `sample_batch` record naming a series outside the contract above
is counted in the report's `unknown_series` field, and the distinct names are
listed in `unknown_series_names`. It does not gate — a producer that grows a new series should not fail a nightly run —
but a *typo* in one of the gated names now shows up there instead of vanishing
into the fold.

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
  "records": 406,
  "malformed": 0,
  "unknown_series": 0,
  "unknown_series_names": [],
  "gate": "pass",
  "run": { "gateway": "ea4a…", "entities": 10000, "cells": 128, "sessions": 6,
           "diff_hz": 2.0, "intent_mix": {"trade": 0.02, "craft": 0.01},
           "duration_secs": 30 },
  "series": {
    "bulk_ack_ms":      { "n": 100, "p50_us": 1500, "p99_us": 3000,
                          "max_us": 3000, "threshold_us": 5000, "gate": "pass" },
    "journal_commit_ms": { "n": 100, "p50_us": 1000, "p99_us": 1000,
                          "max_us": 900,  "threshold_us": 2000, "gate": "pass" },
    "gateway_bulk_server_ms": { "n": 100, "p50_us": 500, "p99_us": 3000,
                          "max_us": 3000, "threshold_us": null, "gate": "not_gated" },
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

`testdata/demo.jsonl` is a synthetic 30 s run whose four gated series all land
below their D16 targets, plus a `gateway_bulk_server_ms` block so the ungated
path is exercised too. The dashboard is runnable on it without a cluster:

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
observe it over the wire. Persistd emits compact `sample_batch` records into
the run artifact, which this dashboard folds exactly as repeated samples. If
no journal samples are present the gate fails with `missing_data`.
