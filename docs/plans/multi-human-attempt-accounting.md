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

**What the host records, and what it does not.** `ExteriorReport` after #579
carries `index`, `node`, `connected_ticks`, the frame counters, `said_goodbye`,
`connected` and `witness_anchored` — and **no `session_id`**. So `exteriors`'s
`session_id` is this contract's field and not yet a host-emitted one, and the
seat identity a real report supplies is the QUIC-authenticated `node`, which the
client signs into its own row and which #579 also made unique per seat. The
binding is therefore by seated invite id when a report carries one and by
admitted node when it does not; when a seat carries an id, the row that lands on
it by node must name that same id, or the host's copy and the client's copy
disagree and the row is refused. `connected_ticks` is likewise real per seat
now, so §4's non-constant denominator is bounded by a number the host measured.

## 3. The contribution arithmetic

One ledger input per actor. Not one per attempt, and never the attempt's total
copied onto a participant.

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
non-constant. That is not a rounding concern; it is the ordinary case for a
one-hour attempt with a 90-second lobby and no post-start rejoin. **Per-slot
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
distinct provenance lines. `measurement_key` does *not* read those fields, so two
attempts of the same deterministic bot cohort still collapse to one measured
bot-hour — deliberately, and unchanged.

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
a 100 ms delay into 10% of packets. The client-side `impairment_mismatch` flag
keeps its existing exact-inequality meaning and is retained unchanged — an
honestly flagged mismatching row is flagged evidence, not a refusal.

## 7. Fixtures

`scripts/p4-attempt-accounting.py --self-test` runs 34 named fixtures per commit
(29 at #572; #576 added five for the host's own array spelling and the
node-bound seat).
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
