# The attempt and accounting contract for multi-human campaigns

Status: contract, normative for the campaign accounting path. Piece 1 of #563's
adopted decomposition (#572). It does not accept an ADR, amend an accepted
record, or authorize deployment, and it needs no ruleset bump:
`REGOLITH_RULESET.version` stays at 16, because nothing here changes a rule the
client executes or the witness re-executes.

Executable half: `scripts/p4-attempt-accounting.py`, whose `--self-test` runs in
`scripts/check.sh`'s `gates` lane per commit. This document is the statement; the
script is what fails when the statement is broken.

## 1. Why this lands before the hours it accounts for

Today one attempt means one human, so an attempt's hours are unambiguous. Three
sites make that true, and each becomes wrong in the same direction with N humans:

- The host writes one figure for the whole cohort,
  `player_hours = total_peers * seconds / 3600`
  (`gates/p1-swarm/src/swarm.rs:1499`).
- Assembly copies the raw report and attaches one client session to it, without
  touching `player_hours` (`scripts/p4-campaign-session.sh`, `cmd_assemble`).
- The ledger banks `.player_hours` and never compares it with the signed
  `banked_minutes` next to it (`scripts/p4-ledger.sh`, `cmd_append`).

So a present one-human, eight-bot 900-second campaign can bank
`9 * 900 / 3600 = 2.25` actor-hours against a human who signed for less than
0.25 of them. That is already a defect. With four humans it is the *same* cohort
total banked once per participant, and #240's entire discipline is an auditable
denominator: "≥ 500 honest player-hours" is a claim about a denominator, and a
numerator over an inflated denominator measures nothing.

Retrofitting this after multi-human play works would make the first banked
cohort hours the test case. This contract therefore lands first, with fixtures,
while zero cohort hours exist.

## 2. `AttemptReport`

The attempt-level evidence document the host emits. It is `SwarmReport`
(`gates/p1-swarm/src/swarm.rs:284`) plus four fields, and nothing is removed:

```text
AttemptReport {
    ..SwarmReport,                 # identity, witnessing, coverage, gaps, …
    attempt_id: UUIDv7,
    bots: B,                       # bot seats, i.e. slots [0, B)
    valid_attempt_seconds: u64,    # seconds the attempt actually ran to a clean end
    completed: bool,
    exteriors: [ExteriorEntry],
    per_link_impairment: [LinkEntry],
}

ExteriorEntry {
    slot: u16,                     # >= B; the seat namespace is stable per attempt
    session_id: UUIDv7 | null,     # the coordinator-issued invite id seated there
    node: hex64,                   # QUIC-authenticated NodeId admitted at that seat
    connected_ticks: u64,          # this seat's own span, not the attempt's
    frames: { uplink, downlink, downlink_dropped },
    close: goodbye | attempt_end | disconnected | queue_overflow | never_connected,
}

LinkEntry { from_slot, to_slot, lane, delivered, dropped, delayed, bytes }
```

`identity.target` in an `AttemptReport` is the **host** target and is preserved
verbatim as `attempt.host_target` on every derived row. See §5.

**Compatibility.** `exteriors` is the contract, and three spellings are read:
`exteriors`; `external` as a **list**, which is what `gates/p1-swarm` emits
since #571 landed as #579 and made `SwarmReport.external` a
`Vec<ExteriorReport>` ordered by swarm slot; and `external` as a single
**object**, the pre-#579 spelling, kept readable so an archived report still
derives. A report carrying both field names must name the same slots in both, or
it is two accounts of one attempt and is refused.

**What the host records — updated by #960.** This section previously read "what
the host records, and what it does not", and the *does not* was the whole of
#960: `SwarmReport` carried none of `attempt_id`, `bots`,
`valid_attempt_seconds`, `completed` or `per_link_impairment`, and
`ExteriorReport` carried no `session_id` and no `close`. Nothing converted a
`SwarmReport` into an `AttemptReport`, so a complete signed human row was refused
with `the attempt report carries no UUIDv7 attempt_id to bind rows to` and no
human hour could bank. The host now emits all of them:

* `attempt_id` — the `--attempt-id` the operator names the generation by, which
  previously reached only the start manifest, the active-seats file and the
  reservation journal. `scripts/p1-swarm-always-on.py`, the only production
  minter, now mints a **UUIDv7** rather than `attempt-<time_ns>-<n>`; the
  directory *is* the id, and every other consumer matches it by equality.
* `bots`, `valid_attempt_seconds`, `completed` — the last two read from the run's
  own tick count against its budget, so a short attempt says so.
* `per_link_impairment` — per directed link, from counters the router now keeps
  beside its swarm-wide ones. **Datagram lane only**: the loss draw is made in
  `Router::schedule`, which the reliable lane never reaches, so a stream entry
  would carry `dropped == 0` by construction and folding it into §6.1's binomial
  band would report a leg as clean for carrying reliable traffic.
* `ExteriorReport.session_id` — the coordinator-issued invite id seated at that
  seat, so the preferred binding of §4.1 is now available on a real report and
  not only the node-bound fallback. `null` on a seat bound without admission.
* `ExteriorReport.close` — in this section's vocabulary, decided by the host
  because it is the only party that knows: a seat the bridge never reported
  connected is `never_connected`; a seat the downlink pump dropped frames for is
  `queue_overflow` and outranks a clean goodbye, because a leg can say goodbye
  politely and still have been fed a backlog the impairment profile did not
  cause; then `goodbye`, then `attempt_end` for a seat still connected when the
  clock stopped, then `disconnected`.

The node-bound path is retained unchanged, because a seat bound outside
admission still carries no invite id: the binding is by seated invite id when a
report carries one and by admitted node when it does not, and when a seat carries
an id the row that lands on it by node must name that same id or the host's copy
and the client's copy disagree and the row is refused. `connected_ticks` is real
per seat, so §4's non-constant denominator is bounded by a number the host
measured.

**The seam itself is now tested.** `gates/p1-swarm/tests/attempt_report_seam.rs`
runs the real binary as a reservation-backed host, seats two real remote
processes over QUIC, and puts the report the host actually wrote through
`scripts/p4-campaign-session.sh assemble` and `scripts/p4-ledger.sh append`
unedited. The fixtures in §7 remain a check on *this* contract; that test is the
check that the host's output is a thing this contract can read at all, which is
the one nothing covered and the reason the suite stayed green with an empty
ledger.

## 3. The contribution arithmetic

One ledger input per actor. Not one per attempt, and never the attempt's total
copied onto a participant. **§9 restates the unit these figures are lengths of:
a seat interval, on both sides.**

```text
bot contribution     player_hours = B * valid_attempt_seconds / 3600
human contribution   player_hours = signed_row.banked_minutes / 60      (one per signed interval)
attempt total        = bot contribution + sum(human contributions)
```

Worked, for the proposed campaign profile `B = 4`, one hour, two humans who bank
50 and 42 minutes:

```text
bot hours   = 4 * 3600 / 3600      = 4.000
human hours = 50/60 + 42/60        = 0.8333 + 0.7000 = 1.5333
attempt     = 4.000 + 1.5333       = 5.5333 player-hours, over 3 ledger inputs
```

The defect this replaces would have banked `6 * 3600 / 3600 = 6.000` **on each
of the two human rows**, for a claimed 16 player-hours from one hour of play.

**Exactly once** is the property, and it is a bijection in both directions:

- every signed human interval in the attempt produces exactly one input;
- every input binds to exactly one exterior entry;
- no two inputs bind to the same `(attempt_id, slot)`;
- no session id appears in two client rows, and no session id is seated at two
  slots.

A schema-shaped test — "the report has an `exteriors` array of the right type" —
proves none of this. The fixtures in §7 test the bijection.

## 4. Binding

Every derived row carries, and is refused without, a `binding` that matches an
exterior entry of the attempt it claims:

```text
binding = { attempt_id, slot, session_id, node, connected_ticks, close }
```

with all four of these holding:

1. `session_id` is seated in `attempt.exteriors` — an unseated row is not a
   participant of this attempt. On a report whose seats carry no invite id, which
   is every report `gates/p1-swarm` writes (§2), the seat is instead the one
   whose admitted `node` this row is signed by, and a row whose node the attempt
   admitted nowhere is refused the same way. Where a seat *does* carry an id, it
   wins: a row landing on it by node while naming a different id is the host's
   copy and the client's copy disagreeing, and is refused rather than bound.
2. `slot >= B` — a human row bound to a bot seat is refused even though the slot
   number is valid.
3. `row.measurement_node == exterior.node`, and the row's Ed25519 signature
   verifies for that node under
   `scripts/verify-campaign-measurement.py`. The session token authenticates the
   connected NodeId but does not by itself reserve a seat
   (`gates/p1-swarm/src/exterior.rs`, the judge's token check), so the match
   against the seat is what makes the row this seat's row. #579 added the other
   half of that: the node must name **exactly one** seat of the attempt, because
   a seat map listing one node twice is ambiguous and a row bound into it is
   bound to nobody in particular. Both `scripts/p4-campaign-session.sh` and
   `scripts/p4-ledger.sh` check it.
4. `close ∈ {goodbye, attempt_end, disconnected}`. A `queue_overflow` close
   means the host counted downlink frames it could not deliver on that leg, so
   that human's observed link is the pump's backlog rather than the declared
   profile; a `never_connected` seat banks nothing.

### The non-constant denominator, stated rather than assumed

A human seated for part of an attempt makes the per-attempt denominator
non-constant. That is not a rounding concern; it is the ordinary case when
players may join or leave throughout a one-hour attempt. **Per-session
connected spans are the answer**, and this contract makes them a refusal rather
than a comment:

```text
connected_minutes = connected_ticks * (seconds / ticks) / 60
refuse unless banked_minutes <= connected_minutes + one tick
```

The one-tick tolerance absorbs the boundary rounding between the client's wall
clock and the host's tick clock, and nothing else. The bot contribution uses the
attempt-wide `valid_attempt_seconds` because bot seats are occupied for the whole
attempt by construction; human contributions never do.

A disconnect therefore costs its own interval and nothing else: the other humans
and the bots keep their contributions, because their state, traffic and evidence
remain measured. The attempt-wide clauses in §6 are the ones that take everybody
down together.

## 5. Mixed-platform target rules

`scripts/p4-campaign-session.sh` currently requires the signed client's
`platform_triple` to equal the host report's `identity.target`, and refuses
otherwise. On a Linux host that makes an honest Windows or macOS human row
unassemblable — the operator must lie about one side or discard the hour. The
rules:

- A human row's `identity.target` is **that participant's signed
  `platform_triple`**. The row is a measurement of that client on that platform.
- The bot contribution's `identity.target` is the host target.
- `attempt.host_target` retains the host's own triple verbatim on every row, bot
  and human, so the host's platform is never lost.
- The ledger's existing constraint `session.platform_triple == identity.target`
  (`validate_session_record`) is **retained, not loosened**. Setting
  `identity.target` from the signed row is what makes an honest cross-platform
  row satisfy it. A row stamped with the host's triple over a foreign
  `platform_triple` still fails that check, and must.

This also keeps `p4-ledger.sh total`'s platform fold honest: a Windows human's
half hour lands in the `windows` bucket, which is what "across all three
platforms" in #240 is counted from.

## 6. Refusals

### Retained, unchanged, and enumerated because they must not be loosened

These are `scripts/p4-ledger.sh`'s, and the ledger keeps checking every one
against the file it is handed. The contract restates them so a refusal can name
the *attempt* rather than a derived row, and its fixtures assert them through
the real `append`.

| Refusal | What it protects |
|---|---|
| `witnessing != true` | an unwitnessed hour measured no false-positive rate |
| `total_false_positives != 0` | one signal against an honest peer is no hours, not fewer |
| `observation_coverage < 0.95` | a blind witness also reports zero findings |
| `deferral_ledger_balances != true` | coverage is only a lower bound if the deferral arithmetic closes |
| `identity.impairment.loss` outside `[0.03, 0.05]` | the criterion's hours are hours under injected impairment |
| `jitter_ticks == 0` or `jitter_rate == 0` | a clean link is a fine run and not one of these 500 |
| `player_hours <= 0` | a run that accumulated nothing |
| malformed or incomplete `session` row (`validate_session_record`) | a row missing a field is not evidence |
| `session.platform_triple != identity.target` | §5; the honest fix is the target, not the check |
| `session.pipeline_digest != pipeline_id(commit)` | hours are only comparable within a pipeline |
| `impairment_mismatch` disagreeing with the row's own observed/configured numbers | a post-hoc edit in either direction |
| signature not verifying for `.external.node` | binds every client-owned field to the admitted key |
| human report without a UUIDv7 `identity.human_session_id` | a display name cannot become the distinguishing field |
| `run_key` dedup on `identity` | a re-dispatched or restored report adds no second hour |
| `measurement_key` collapse (pipeline, actor, seed, impairment, target, and `human_session_id` for humans) | a re-measurement is provenance, not another measured hour |

Two consequences worth naming. `run_key` hashes the whole `identity`, and derived
rows add `attempt_id` (and `slot`, for humans) to it, so two attempts keep
distinct provenance lines. `measurement_key` reads `human_session_id` for a
human, so two visits by one person are two measurements; **and since #1048 it
reads `attempt_id` for a bot contribution that carries one**. A bot leg with no
attempt — every CI leg — still collapses, because a deterministic re-run of one
seed re-measures one simulated hour. See §9.

### Added by this contract

| Refusal | What it prevents |
|---|---|
| no UUIDv7 `attempt_id` | a row with nothing to bind to |
| `completed == false`, or `valid_attempt_seconds <= 0` | a partial attempt keeps its rows for diagnosis and banks none |
| a session id in two client rows | one interval banked twice |
| a session id at two slots | the same, from the host's side |
| two rows binding one slot | one seat carrying two intervals |
| a row whose session id is not seated | an interval attributed to the wrong attempt |
| a human row bound to a slot `< B` | a human interval charged to a bot seat |
| `measurement_node != exterior.node` | a row bound to the wrong participant |
| `banked_minutes > connected_minutes + one tick` | a claim longer than the seat it was played in |
| `close ∈ {queue_overflow, never_connected}` | a leg whose evidence is the backlog, or no leg at all |
| missing `per_link_impairment` | a cohort aggregate cannot verify one human's leg |
| a slot's links carrying `< 1000` packets | a band computed from too small a sample |
| a slot's links with `dropped == 0` or `delayed == 0` | a leg that ran clean inside an impaired attempt |
| a slot's observed loss outside the band in §6.1 | impairment configured but not applied on that leg |
| anything written before every row validates | a refusal that leaves bankable-looking inputs behind |

### 6.1 The per-leg impairment band

#240 requires impairment *verified applied*, not merely configured, and a cohort
attempt has one leg per human. The router draws per packet, so a slot's directed
links are a binomial sample of the configured loss `p`. The acceptance band is
three standard deviations of that binomial — plain arithmetic, chosen in advance,
not a tolerance widened until a run passes:

```text
n     = sum over the slot's directed links of (delivered + dropped)
sigma = sqrt(p * (1 - p) / n)
band  = [max(0, p - 3*sigma), p + 3*sigma]
```

At `p = 0.03` and the 1,000-packet floor that is `0.03 ± 0.0162`, i.e.
`[1.38%, 4.62%]`. The floor exists because the band is meaningless at small `n`:
at `n = 20` it reaches past 14% and would admit a leg that dropped nothing.

This is deliberately *not* a requirement that observed loss equal exactly 3.000%,
and it does not describe the 100 ms spike as both p50 and p99: the router injects
a 100 ms delay into 10% of packets. The coordinator advertised it as both
percentiles anyway until #1030, which flagged every honest session of the first
real cohort; it now advertises the quantiles that two-point distribution
actually has (p50 0, p99 100), and the jitter halves of the flag compare as a
*floor* rather than a target — the measurement composes the injected spike with
the path the volunteer plays over, and delays add rather than cancel, so only a
shortfall is evidence. Loss remains two-sided. An honestly flagged mismatching
row is flagged evidence, not a refusal.

## 7. Fixtures

`scripts/p4-attempt-accounting.py --self-test` runs 51 named fixtures per commit
(29 at #572; #576 added five for the host's own array spelling and the
node-bound seat; #1048 added three for the seat-interval unit — see §9).
The two failure modes this contract exists to prevent are named directly:

**Duplicated cohort hours**

- `human_row_banks_its_own_interval_not_the_cohort_total` — each human row's
  `player_hours` equals its own `banked_minutes / 60`, and is asserted *not* to
  equal `total_peers * seconds / 3600`. Both halves are needed: the second is
  what fails when the arithmetic reverts to today's.
- `attempt_total_is_bot_plus_signed_intervals`
- `one_interval_may_not_be_banked_twice`
- `one_session_may_not_occupy_two_seats`
- `derived_rows_bank_through_the_real_ledger` — the derived files go through
  `p4-ledger.sh append` unmodified, and the banked hours must sum to the derived
  total with three distinct `run_key`s and three distinct `measurement_key`s.
- `reappending_an_attempt_banks_no_second_cohort_hours`
- `two_generations_bank_two_bot_intervals` — a second generation of the same
  cohort must be a second *distinct* bot measurement, and collapsing the two
  must be shown to raise the reported human mix. Both halves are needed: the
  second is what fails when the fixture stops measuring §9.2's defect.
- `a_restart_does_not_double_bank_a_seat_interval`
- `one_seat_interval_may_not_bank_twice_across_a_restart` — the same interval
  re-derived at another commit, where `run_key` dedup cannot see it.

`scripts/p4-ledger.sh --self-test` carries the matching three at the append
seam: `two_generations_are_two_bot_measurements`,
`a_restart_banks_no_second_copy_of_one_generation`, and
`a_deterministic_rerun_is_still_one_measurement`, the last being the collapse
§9.2 must *preserve* for every attempt-less CI leg.

**Cross-platform host/client rows**

- `mixed_platform_rows_carry_their_own_target` — a Windows client on a Linux
  host is stamped with its own triple, the bot contribution keeps the host's,
  and `attempt.host_target` survives on every row.
- `cross_platform_host_client_row_is_refused` — the same row re-stamped with the
  host's Linux triple is refused by the real ledger.
- the `windows: 0.5 distinct hours` assertion inside
  `derived_rows_bank_through_the_real_ledger`, so the hour reaches the platform
  bucket #240 counts.

The rest cover the binding (`row_bound_to_the_wrong_node_is_refused`,
`row_with_no_exterior_is_refused`, `human_row_bound_to_a_bot_seat_is_refused`,
`binding_names_the_seated_slot`), the non-constant denominator
(`interval_may_not_exceed_its_seats_connected_span`,
`a_disconnect_costs_only_its_own_interval`,
`a_queue_overflow_leg_banks_nothing`), the retained attempt-wide clauses, the
per-leg impairment band, and the one-slot compatibility spelling.

## 8. What this contract does not do

- It does not implement multi-exterior routing. `Option<ExteriorSlot>` and the
  routes that branch on it are #571.
- It does not change admission, the flock, or the reservation allocator (#573).
- It does not itself modify `scripts/p4-campaign-session.sh` or
  `scripts/p4-ledger.sh`. That is #563's piece 7 (#576), which consumes this
  contract rather than restating it, and which has since landed: `assemble`
  derives through `p4-attempt-accounting.py derive` instead of copying the
  attempt's own `player_hours` onto one client row, and the ledger checks the
  binding, cross-checks `player_hours == banked_minutes / 60`, and refuses a
  second claim on a seat across appends. The fixtures here still drive `derive`
  and the real `append` directly, which is what keeps them a check on *this*
  contract rather than on the seam above it.
- It does not itself carry the `binding` into the ledger *line* — #576 does.
  `p4-ledger.sh` copies a named field list into each line, and it now includes
  `attempt_id`, `slot`, `binding`, `attempt`, `contribution` and
  `link_impairment`, so reconciling which seat of which attempt an hour came
  from is an audit of the ledger rather than of a directory beside it.
- It changes no ruleset. `REGOLITH_RULESET.version` is 16 and stays there.
## 9. The unit is a seat interval (#1048)

Added by #1048, which asked for continuous banking: players come and go against
a world that is always running, so **the banking unit is a seat interval that
banks on departure**, not an attempt that banks at its end.

Most of that was already true, and this section says which part was not.

### 9.1 What a ledger input is

One input is one seat's occupancy of one slot over one wall interval.

```text
human seat interval   (attempt_id, slot, human_session_id)
                      bracket [connected_since, connected_until)
                      banks when the seat is released
bot seat interval     B of them, slots [0, B), each covering the generation's
                      own wall span; bundled into one input
```

The human half is §3 and §4 restated: a departed seat keeps its own entry
(`Swarm::departed_exteriors`, `gates/p1-swarm/src/swarm.rs`), with its own
bracket, its own close reason and its own `session_id`, so one player leaving
mid-attempt costs the others nothing and a rejoin is a *second* interval on the
same slot (#1028). The 2026-09-04 evidence exercises exactly that: slot 6 holds
two intervals under two coordinator-issued ids and banks two rows.

The attempt is therefore the **containing window**, not the unit. Since #1040
the bracket opens at connection accept, so a seat's interval is contained in
its bracket by construction, and the bracket in the generation.

### 9.2 The bot contribution, and the criterion it was quietly bending

`player_hours = B * valid_attempt_seconds / 3600` is unchanged as a *length*: it
is already the length of B seat intervals, and bot seats are occupied for the
whole generation by construction, which is what entitles them to the
attempt-wide figure a human contribution may never use.

What was wrong was its **identity**. `p4-ledger.sh`'s `total` folds the ≥25%
human-mix line over `distinct`, i.e. over `measurement_key`, and that key read
pipeline, actor, seed, impairment and target — not the attempt. A standing host
passes no `--seed` (`scripts/p1-swarm-always-on.py`, `Supervisor.command`), so
every generation is identical under that key and **all of them collapsed into
one distinct bot measurement**, while every human visit stayed distinct under
`human_session_id`.

The consequence, measured against the 2026-09-04 ledger: a second generation of
the same five bots banks its 1.25 provenance hours and adds *zero* distinct bot
hours. The reported mix stays 27% where the truth is 2.5 bot against 0.483
human — 16%, under the floor. The denominator stops growing with wall time
while the numerator keeps growing, so **the floor is cleared by running longer
rather than by anyone playing more**. That is live at `seconds = 900`, and
raising `seconds` does not repair it; it only changes the constant.

The property the mix needs is *chop-invariance*: cutting the same wall time into
more generations must not change `human_hours / total_hours`. Human hours are
chop-invariant already. Bot hours become so once the attempt is in the key.

**Stated in the direction that needs saying out loud.** This makes distinct bot
hours larger. The ≥25% floor becomes *harder*, and the raw 500-hour figure rises
faster with bot time. It adds no human hour and banks no interval that was not
measured; it stops discarding bot-hours that were separately measured over
disjoint wall time. Whether the 500 should be a human-weighted figure is a
separate decision this record does not make.

### 9.3 `run_key` across a host restart

`run_key` is `sha256(canonical identity)[:16]`, so a seat interval's provenance
key is what its identity names.

* **Human.** `(attempt_id, slot, human_session_id)`. `human_session_id` is the
  coordinator-issued invite id, a UUIDv7 minted once per admitted interval, and
  the ledger already refuses a human row without one. The host does not mint it,
  so a host restart cannot reproduce it; two intervals minted in one millisecond
  collide at 2⁻⁷⁴.
* **Bot.** `attempt_id`. `scripts/p1-swarm-always-on.py`'s `mint_attempt_id` is
  a UUIDv7 whose 74 random bits come from `secrets`, and the id is materialised
  as a directory created with `mkdir(mode=0o750)` — **no `exist_ok`**, so a
  repeated id raises rather than reuses. A restart mints a fresh one per child
  spawn.

Dedup is therefore two-layered and both layers are tested: `run_key` refuses an
identical replay of a report, and `refuse_a_second_claim_on_one_seat` refuses
the same seat or the same session re-derived at another commit, where the run
key differs and the first layer cannot see it.

### 9.4 Where the criterion is evaluated

Per generation report, over that generation's own window — **and deliberately
not over a rolling one**. The attempt-wide clauses are not additive over a
sliding window: `observation_coverage` is a ratio over judged and shown ticks
the report does not retain per interval, `deferral_ledger_balances` is a closure
property of the whole run, `total_false_positives` is a count over the run, and
§6.1's band needs `n ≥ 1000` packets on *that slot's* links, which the report
only totals per run. A rolling window would have to re-derive all four from
numbers no report decomposes. Since a seat interval is contained in exactly one
generation, judging it by that generation's evidence is both well defined and
the smallest window whose evidence exists.

### 9.5 Host restart: open intervals close honestly and bank nothing

An interval open when a host dies is **lost, deliberately**. Surviving it would
mean attaching a seat's open bracket to a *later* generation's evidence, and
that evidence — coverage, false positives, per-leg impairment — was measured by
the process that died. A generation that exits without its tick budget writes
`completed: false` and the derivation refuses every row of it, bot included; a
generation that dies without writing a report banks nothing at all. Both are
the under-counting direction, which is the recoverable one.

The remaining exposure is therefore the *blast radius*: a generation is banked
at its end, so a crash costs the intervals it contained. That is bounded by
`seconds` and is the reason `seconds` cannot simply be raised to hours without
also moving the emission point. Moving it is not done here — see §9.7.

### 9.6 `impairment_mismatch`'s shakedown clause

It still means what it meant, and the unit change does not touch it.
`cmd_shakedown` samples *rows* carrying `session.shakedown.phase == "unbanked"`,
and a row is a seat interval already; under continuous banking the sample is a
set of seat intervals rather than a set of attempts, which is more
representative rather than less. It never sampled an attempt, so there is
nothing for "no attempt to sample" to break.

What is worth recording beside it: all four signed rows of 2026-09-04 carry
`impairment_mismatch: true`, so a shakedown sampled from that cohort would have
FAILed the clause. That was #1030 — the coordinator advertised p50 = p99 =
100 ms for a two-point distribution whose true p50 is 0, and the client compared
its own measured jitter against it as a target rather than a floor. The
advertisement and the comparison are both fixed; the clause is correct and is
retained unchanged.

### 9.7 What #1048 does not do, and what remains an owner decision

* **A seat still banks when the generation ends, not the instant it departs.**
  The interval is *recorded* on departure — that is `departed_exteriors` — but
  it is *emitted* when the host writes its report, which is at process exit.
  Making it bank on departure needs the host to write a report while still
  running, and `Swarm::report` consumes `self` today. That is a host change, not
  an accounting one, and it carries a second question this record will not
  answer by implication: if a generation banks in pieces, an attempt-wide
  refusal raised in a later piece can no longer void an earlier one that has
  already banked. Every attempt-wide clause in §6 changes meaning at that point.
* **`seconds` and `lobby_seconds` are untouched.** They are campaign config, and
  the mix arithmetic above is now chop-invariant, so the length of a generation
  is a blast-radius and latency decision rather than an accounting one.

