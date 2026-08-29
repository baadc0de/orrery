# #699 swept-margin audience experiment

Date: 2026-08-29
Branch point: `d2084c4` (`fix/per-link-impairment-rng`, #700)

## Question

Does `--swept-interest-margin` fail P1's boundary-thrash clause because the
flipping peer receives an extra or earlier replica and adjudicates differently,
independently of impairment RNG coupling?

The full-hour command is the one from #699, with and without only the swept
flag. No threshold, allowance, hysteresis parameter, roster order, audience
order or send order was changed.

## Correction to the carried clue

On #700's per-link impairment baseline, the single flipper is stable index 13,
`PersistId(14)`, not index 12 / `PersistId(13)`:

```text
baseline: 819648 bit/s, 0 boundary flips
swept:    819648 bit/s, 1 boundary flip
          index 13 / PersistId(14)
```

The index-12 identity in #699's reopening comment came from the pre-#700
impairment realisation. It was carried forward without re-identifying the
flipper after #700 moved the impaired baseline.

The accompanying “its own interest never widened” clue therefore does not
apply to the current flipper either. The receiver-side capture for index 13 is:

```text
next tick       0  60  120  180  240  300  360  420  480  540  600
baseline cells 27  27   27   27   27   27   27   27   27   27   27
swept cells    27  27   27   27   27   27   27   27   36   27   27
```

## First replica divergence

The arrival histories are identical through receive tick 480. The refresh for
next tick 480 widens the swept receiver from 27 to 36 cells. At receive tick
483 the swept leg gets an added-audience burst that the baseline does not get:

```text
replica entities: 5, 9, 12, 13, 16, 17, 20, 24
wire form:        cached keyframe for every entity
state ticks:      438, 450, 459, 462, 471, 474, 483, 435
```

The first record in delivery order is the extra `PersistId(5)` keyframe at
receive tick 483, carrying state tick 438. This is an **extra arrival**, not a
late baseline arrival: the baseline has no `PersistId(5)` arrival in this
audience interval, while the swept leg continues with its tick-486 delta.

Thus the set/timing half of the proposed hypothesis is true, but that is not
the mechanism which first changes canonical behaviour.

## Causal chain

1. The tick-483 burst includes an extra `PersistId(13)` keyframe and subsequent
   deltas on the directed `PersistId(13) -> index 13` link.
2. `Router::rng` is per directed link, not per lane or logical packet. State
   datagrams and reliable control messages on this link consume the same
   sequential `ChaCha8Rng` stream.
3. The first non-replica trace divergence is consequently a **delay**, not an
   adjudication of the added replica: a `ShotResolved { target: PersistId(13),
   result: OutOfArc }` from `PersistId(13)` reaches the baseline at tick 773
   and the swept leg at tick 776. The three-tick difference is one configured
   reliable-stream retransmission interval. Both messages use the same link as
   the extra `PersistId(13)` state traffic.
4. Canonical histories then separate. At tick 791 the legs apply the same
   `Damage` input but already have different state hashes and positions. The
   feedback loop changes later authored inputs and replicas; at tick 20,589
   only the swept leg nominates and resolves a collision with `PersistId(18)`.
5. The late decisive reversal is a `CollisionResolved` from `PersistId(12)`
   received only by the swept leg at tick 182,190. Just before it, tick 182,164
   has velocity `(3.533, 0, -6.785)` m/s; applying it yields
   `(7.250, 0, 25.150)` m/s. The baseline receives no corresponding collision
   and continues on a different trajectory.
6. The swept craft commits cell `(27, 0, 3) -> (27, 0, 4)` at tick 183,184,
   then returns to `(27, 0, 3)` at tick 184,647. The baseline traverses a
   different cell sequence and records no return.

The early same-link timing change is upstream of the first canonical state
divergence, the unique tick-20,589 collision, the tick-182,190 reversal, and the
eventual return. No hysteresis decision is implicated.

## Verdict and recommendation

The proposed **RNG-free audience-adjudication hypothesis is refuted**. The
audience does produce the predicted extra arrival, but the first behavioural
effect is same-link state/control impairment-stream coupling. #700 removed
cross-link coupling only; it did not make an A/B invariant to packet insertion
within one link.

Do not change the 10% hysteresis margin or the zero-return criterion. Before
wiring the swept margin into a host, make impairment fate stable for logical
packets common to both A/B legs. Merely splitting the RNG per lane is partial:
it stops state traffic shifting control, but an inserted state packet still
shifts every later state packet on that lane. Prefer a deterministic fate draw
keyed by run seed, directed link, lane and stable logical-packet identity, while
retaining reliable-stream ordering and head-of-line accounting. Then re-run
the exact full-hour pair. This is a harness-model change and remains an owner
decision; this experiment lands no such fix.

## Diagnostic

Set `P1_SWARM_AUDIENCE_TRACE_SEAT=<stable index>` to emit receiver-side
coverage counts, decoded replica arrivals, delivered inputs, collision
nominations/outcomes, and cell commitments. The probe is opt-in and observes
only data already selected or installed; it never rewrites a roster, audience
or send queue.

## Flags-off identity guard

Because the coverage probe is called from the audience path, the normalised
full-hour flags-off report was compared with an archive of the branch point.
Both legs used the exact command above without `--swept-interest-margin` and
without any diagnostic environment variable. After removing only
`identity.commit` and `started_at_unix_secs`, both complete JSON reports have
this SHA-256:

```text
82f2341f27037d712cd7dd0c78ad672bf3b9f6f800d019d586ce6742010d5122
```

Both report 819,648 bit/s, zero boundary flips and every P1 clause holding.
