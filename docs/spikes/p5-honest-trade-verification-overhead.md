# P5 honest trade verification-overhead measurement

**Method: pre-built attestations.** This measurement isolates gateway-side
**verification overhead**; it is not an end-to-end attestation-overhead number.
An end-to-end result would additionally include witness discovery, proposal
request/response latency, witness execution, witness signing, retries, and
quorum collection. Every measured attested intent nevertheless carried exactly
the default required K = 3 cryptographically valid signatures from distinct,
announced, non-party accounts.

## Result

On 2026-08-23, the attested population's honest trade commit p99 was
**10.044 ms** over 10,000 committed samples. The paired unattested control was
**8.085 ms** p99 over 10,000 committed samples, a **+1.959 ms** p99 delta.
The attested result therefore **missed** D16's strict `< 10 ms` budget by
**0.044 ms**. This is a finding, not a harness failure, and no threshold was
retuned.

The population was large enough to put 100 observations in its upper one
percent. All 20,000 measured acknowledgements were commits, all 20,000 durable
receipts were read back, and the harness pre-verified 30,000 witness signatures.
Each sample used a fresh item and a fresh asset/balance row; there was no
same-balance hotspot masquerading as attestation cost.

| population | n | p50 | p90 | p99 | mean | max |
|---|---:|---:|---:|---:|---:|---:|
| unattested control (`shadow`) | 10,000 | 6.842 ms | 7.013 ms | 8.085 ms | 6.906 ms | 37.495 ms |
| exactly-K attested (`required`) | 10,000 | 6.843 ms | 7.012 ms | **10.044 ms** | 6.941 ms | 37.511 ms |

The two populations ran as 16 concurrent pairs: every worker submitted one
control and one attested trade concurrently to separate gateway processes
against the same private FoundationDB instance. That keeps temporal conditions
paired without asserting an exact wall-clock completion window.

## Stage attribution

The existing `IntentTrace` counters show the direct gateway admission cost:
mean `admit_us` rose from **48 µs** to **148 µs**, a **+100 µs** verification
delta for the three ed25519 signatures plus membership and required-subset
checks. Mean client round trip rose only 35 µs (6.906 to 6.941 ms); other
executor/FDB means moved slightly in both directions.

| mean stage | control | attested | delta |
|---|---:|---:|---:|
| admission | 48 µs | 148 µs | **+100 µs** |
| spawn wait | 1 µs | 1 µs | 0 µs |
| executor | 6.739 ms | 6.625 ms | -0.114 ms |
| FDB commit | 6.244 ms | 6.118 ms | -0.126 ms |
| server total | 6.791 ms | 6.777 ms | -0.014 ms |

With `ORRERY_INTENT_SLOW_US=10000`, 72/10,000 control intents and 99/10,000
attested intents crossed the 10 ms server cut. Their FDB commit spans remained
the dominant tail term. The +1.959 ms p99 delta must therefore not be relabeled
as 1.959 ms of signature work: the directly attributable verification term is
the +100 µs admission mean, while process scheduling and FDB tail placement
decide which nearest-rank observation becomes p99.

## Rig and evidence

- Source: `5bb8049660dff45b1eb532e611d70f3dc5c6d74b` plus the uncommitted #153 changes.
- Host: `fortyninety`, AMD Ryzen 9 9950X3D (16 cores / 32 threads), Linux
  7.1.6-1-cachyos x86_64.
- FoundationDB: native `/usr/bin/fdbserver`, 7.3.77, one private single-memory
  instance on ephemeral port 50207 with its own data directory. The shared
  development cluster on port 4500 was neither leased nor touched.
- Gateway and client: release build, direct loopback iroh transport, 16
  concurrent submissions per population, 10,000 samples per population.
- Machine-readable report:
  [`docs/data/p5-honest-trade-verification-overhead-2026-08-23.json`](../data/p5-honest-trade-verification-overhead-2026-08-23.json).

Reproduce with:

```sh
SAMPLES=10000 CONCURRENCY=16 scripts/p5-honest-trade-measure.sh
```

The script chooses a non-default ephemeral FDB port, creates a dedicated data
directory, launches opposed shadow/required gateways, and stops only the
instance identified by that directory.

## Scope protection

The measurement is an additive `measure` subcommand and a sibling script. The
live `run` gauntlet, `ramp` command, their assertions, fixed ids, output schemas,
and reported numbers are unchanged. `gates/p2-load` and its historical
`intent_commit_ms` series are also unchanged; the two new populations have
distinct series names.
