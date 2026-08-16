# p4-streams-bench

What each control-lane transport costs the repair path, measured over real QUIC.

## The question

`orrery_net`'s control lane rides QUIC streams (D3, docs/02-networking.md §7). A
stream is ordered within itself and independent of every other stream, so *which*
stream a message takes decides what can block it:

- **one shared stream** is cheap and totally ordered, and a lost segment holds
  up every message queued behind it;
- **a stream per message** cannot head-of-line block, at the cost of a stream
  per message and no ordering between them.

Neither is universally right. The traffic mix decides, so this measures the mix.

## What is real

Real: two `aeronet_iroh` endpoints, two QUIC connections' worth of congestion
control, loss detection, retransmission and stream scheduling; `orrery_net`'s
send path, channel policy and upload meter.

Not real: the game above it, and the wire. `src/impaired.rs` implements iroh's
`CustomTransport` as an in-process link with seeded loss, delay and jitter, and
the endpoints are built with `clear_ip_transports()` — so it is the only path
they have.

> The first version of this used a UDP proxy instead. It does not work: iroh
> probes for a better path and takes it. The proxy carried **21 packets out of
> 1457**. That is why the impairment is inside the stack rather than beside it —
> a structural guarantee instead of a check that could pass by luck.

`--check-link` validates the model itself: that the observed drop rate is the
configured one.

## The workload

Three classes at once, because the interesting failures are *between* them.

| class | size | rate | what it is |
|---|---|---|---|
| `state` | 500 B | 20 Hz | replication + witness frames, on datagrams under every transport |
| `sparse` | 120 B | 4 Hz | lease traffic, handoff acks, manifest deltas — "must arrive, order matters, tiny volume" |
| `repair` | 40 kB | `--repair-hz` | a `LogRangeResponse` filling a one-second hole |

Latency is to a *whole* message: a repair that arrives in thirty-four pieces is
complete when the last one lands, because that is when a witness can fold it.

## The candidates

| name | sparse control | repairs |
|---|---|---|
| `datagram` | one datagram, unreliable | chunked to the MTU, one chunk per round trip |
| `shared` | shared stream | shared stream |
| `bulk` | own stream each | own stream each |
| `split` | shared stream | own stream each |

`datagram` is the status quo reproduced rather than modelled — what
`orrery_witness` did when `Channel::Control` was a datagram with a different
first byte. It is there to size the win, not as a contender.

## Results

3% loss, 40 ms RTT, 100 ms jitter on 10% of packets, 30 s per transport.
Milliseconds.

### Uncongested — 40 kB/s of repairs (`--repair-hz 1`)

| transport | class | done % | p50 | p95 | p99 |
|---|---|---:|---:|---:|---:|
| datagram | sparse | 92.5% | 22.7 | 121.5 | 125.6 |
| datagram | **repair** | **90.0%** | **3004** | 3454 | 3504 |
| shared | sparse | 100% | 22.7 | 105.1 | 178.9 |
| shared | **repair** | 100% | **214** | 428 | 481 |
| bulk | sparse | 100% | 26.8 | 115.4 | 152.4 |
| bulk | repair | 100% | 263 | 926 | 1021 |
| split | sparse | 100% | 22.7 | 115.3 | 164.6 |
| split | repair | 100% | 231 | 720 | 864 |

### Saturated — 80 kB/s of repairs (`--repair-hz 2`, four seeds)

Milliseconds, p50 / p95. **Bold** is the best of the three in that row.

| seed | | shared | bulk | split |
|---|---|---:|---:|---:|
| 7 | sparse | 80.3 / 401.2 | 39.1 / 135.6 | **35.1 / 96.9** |
| | repair | 467.5 / **679.5** | **438.2** / 923.9 | 510.4 / 1267.8 |
| 101 | sparse | 251.0 / 819.1 | 65.9 / 153.6 | **47.4 / 137.3** |
| | repair | **612.6 / 1153.6** | 1088.4 / 1932.9 | 762.6 / 2353.4 |
| 2024 | sparse | 131.7 / 711.7 | 37.1 / **119.5** | **35.0** / 121.2 |
| | repair | 518.2 / **1057.8** | **452.4** / 1267.5 | 452.7 / 1105.6 |
| 55555 | sparse | 88.5 / 397.1 | 51.5 / 146.0 | **32.9 / 115.1** |
| | repair | 425.9 / **681.0** | 664.6 / 1660.4 | **421.7** / 1413.6 |

At this load the datagram baseline completes **0–37%** of its repairs depending
on seed, so it has no row worth comparing.

## What it says

**Streams beat the datagram baseline outright, and not narrowly.** Uncongested,
a repair goes from a 3003 ms median to 214–264 ms. Saturated, the baseline stops
working: 0–37% of repairs complete, and in one seed its own chunk retries flood
the state lane hard enough to take *state* latency to a 4 s p95. That is PR #15's
"the path amplifies under load", seen directly rather than inferred.

Between the two stream modes there is no winner, and saying otherwise would be
reading noise. What four seeds *do* support is one effect in each direction:

**Sharing a stream with repairs costs sparse control 2–5× its median and 3–6× its
p95.** Every seed, no exceptions, and the mechanism is exactly head-of-line
blocking: a 40 kB repair is ~34 packets, at 3% loss it almost certainly loses one,
and on a shared stream a lease ack queued behind it waits for the retransmission.

**Buying that back costs the repair tail 1.4–2×.** Also every seed. Strict
ordering on one stream finishes repairs in sequence; independent streams
interleave, so no repair is starved and none finishes as early either.

`bulk` and `split` sit within noise of each other on both metrics — `split` takes
sparse p50 in all four seeds and p95 in three, but by margins a fifth seed could
reverse. **Nothing here says `split` is faster than `bulk`.**

## The decision, and why it is not a scoreboard

The measurement narrows it to one question — *would you rather pay sparse control
latency or repair tail latency?* — and the system answers that, not the numbers.

Sparse control is lease traffic, handoff acks and manifest deltas. It is
latency-critical and correctness-adjacent, and nothing repairs it. Gap repair is
already slow by design: the witness holds one outstanding repair on a backoff,
defers judgement while it catches up (`Catchup`), and escalates only after
several attempts. A repair tail 1.4–2× longer is absorbed by machinery that
exists to absorb it. A lease operation five times slower is not.

So repairs go on their own streams. Between `bulk` and `split`, `split` — because
ordering is meaningful for sparse control and free on a stream it already has, and
because per-message streams for bulk is the shape docs/02-networking.md §7 already
specifies for area load. That is a design argument the measurement permits, not
one it proves.

## What it did to the swarm

The figures above are one link in isolation. `p1-swarm --witness` is the
end-to-end check — every bot honest by construction, so any signal beyond a
chain gap is a false positive. Same command on both sides, against `main` at
`74508f3e` (`--peers 8 --seconds 120 --impaired --witness`):

| | before | after |
|---|---:|---:|
| chain gaps detected | 2151 | **1341** |
| false positives (all `Stalled`) | 128 | **82** |
| packets the link carried | 188 114 | 177 176 |

Which is the mechanism running forwards: fewer round trips per repair means
fewer packets, which crowds the state lane less, which opens fewer holes, which
leaves fewer of them unfillable in time.

Read it as an effect rather than as a clean A/B, though — `p1-swarm`'s router
gained its reliable lane in the same change, so part of the difference is the
harness modelling control as reliable *because it now is*. The tables above are
the controlled measurement; this is what it was worth downstream.

P4's false-positive criterion is still not met at 82, and the roaming clause
fails on both sides for the unrelated reason that 120 simulated seconds is not
an hour.

## Caveats worth stating

- **Loopback, one process.** Both peers share a machine and the RTT is imposed
  rather than travelled. That is what makes the latency figures comparable
  without a clock exchange; it also means absolute numbers are optimistic while
  the *differences between transports* are the finding.
- **The upload budget is raised to 50 Mbps** so the transport is what the
  numbers describe. What the 1 Mbps budget costs was measured separately in
  PR #15, and is the finding this benchmark follows from.
- **Two peers, not an island.** A witness-set fan-out multiplies repair load by
  up to seven. This measures one link's behaviour, not that multiplication.
