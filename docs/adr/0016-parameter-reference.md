# ADR-0016: Parameter reference (defaults)

**Status:** Accepted; extended by [ADR-0020](0020-journal-retention.md),
enforced by [ADR-0023](0023-follower-journal-retention.md), proposed extension in
[ADR-0025](0025-expire-fan-out.md) ·
**Date:** 2026-08-11 · **Decision:** D16

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

| Parameter | Default | Parameter | Default |
|---|---|---|---|
| Sim tick | 60 Hz | Lease TTL / heartbeat | 10 s / 2.5 s |
| Send rate | 20 Hz (≤30) | Journal fsync group | ~2 ms |
| Rollback window | 9 ticks (150 ms) | Checkpoint cadence | 20 s jittered |
| Interp buffer | 100 ms | Bulk ack p99 (client / journal) | < 5 ms / < 2 ms |
| High-rate interest set | 24 entities | Intent commit p99 | < 10 ms |
| Proxy rate | 1–4 Hz | Area first-page-in | < 50 ms |
| Hit rewind cap | 200 ms | Witness quorum | K=3 of N≥5 |
| Cell edge (interest) | 128 m | Strike half-life | 14 days |
| Shard cell | 8×8×8 interest cells | Peer upload budget | ≤1 Mbps |
| Hysteresis margin | 10% cell edge | Mesh→promotion threshold | >32 sustained |
| ε_pos / ε_vel / window | 1 cm / 1 cm·s⁻¹ / 250 ms | Adjudication window max | 3 s (180 ticks) |
| Epoch reseed min interval | 10 s | Ruleset builds retained (adjudication) | 3 |
| Hot-cell egress (promoted) | ≤ 35 Mbps | Witness-log fan-out | witness set only (≤ 7 links) |
| Journal retention | on (D20) | Journal open (index rebuild) | < 2 000 ms (D20) |
| Drain grace | 10 s (D24) | — | — |
| `Expire` fan-out dispositions | `Parked`/`Free` only (D25) | `Expire` fan-out bucket (per recipient) | 32/s, burst 64 (D25) |

The last row is added by [D20](0020-journal-retention.md). *Journal retention*
is whether a node releases journal segments its checkpoints have made
redundant; with it off, journal disk and the index rebuilt from it at every
open grow with total uptime rather than with the checkpoint cadence. *Journal
open* is the budget for that rebuild on a node within its retention floor —
measured at 3.94 µs and ~95 bytes of index per record, so 2 000 ms is roughly
508 000 records. Both parameters are **enforced** as of
[D23](0023-follower-journal-retention.md): the P2 kill-9 gate fails unless
retention released on both nodes during the run and every node's reported
`journal_open_ms` is inside this budget. Retention is unchanged as a default
(on) and unchanged as a switch (`persistd --no-journal-retention`); what
changed is that a run in which it did nothing no longer passes.

The *drain grace* row is set by [D24](0024-island-drain.md). It is the horizon the coordinator stamps
into `CoordMsg::Drain`'s `deadline`, and it is set equal to the lease TTL in
the same table rather than tuned: a shorter grace names an instant before the
registrar's expiry sweep can observe anything, and a longer one names an
instant after that sweep has already parked every row — so `10 s` is the only
value that adds no third timer to the two this system already has.

The `Expire` fan-out rows are added by [D25](0025-expire-fan-out.md), and are
*proposed* rather than accepted until that record is. *Fan-out dispositions* is
which expiry outcomes are copied to non-holders: `Reassigned` is excluded
because [INV-4](../04-authority.md) converges observers on the successor's
first envelope without any message, while a parked entity has no successor
stream and the advisory is the only mechanism. *Fan-out bucket* is the
per-recipient egress limit on those copies, shaped after D7 §10's claim bucket
so the ingress and egress limits on the same path read alike; D25 also caps one
expiry at **128** non-holder recipients, D6's per-cell player ceiling. Both
limits **drop** rather than queue — the advisory is best-effort by
construction, and `Deny{Parked}` on a subsequent claim is the authoritative
answer.
