# Decision memo — #1092: bot hours bank at generation end, and should keep doing so

**Recommendation: option 3, accept the exposure.** Build nothing.

This is the artifact for [#1092](https://github.com/baadc0de/orrery/issues/1092),
deferred out of [#1048](https://github.com/baadc0de/orrery/issues/1048)
(PR [#1086](https://github.com/baadc0de/orrery/pull/1086)). It is a decision
rather than an omission: the exposure was measured, the re-run it is weighed
against was measured, and the two options that would remove it were costed by
reading the lines that would have to change.

Every claim about current behaviour is cited to a line read on this tree
(`fix/1092-bot-hour-exposure` off `6fe6559`, 2026-09-05). Documentation-only
lane: `check.sh` is exempt per [AGENTS.md](../../AGENTS.md).

---

## 1. The question

The bot half of a campaign attempt banks one number, once:

```
player_hours = B * valid_attempt_seconds / 3600
```

derived at `scripts/p4-attempt-accounting.py:944` from the host's finished
report, and emitted as exactly one ledger input
(`scripts/p4-attempt-accounting.py:979-980`). The human half became a *seat
increment* in #1048 and banks while the seat still flies. So an attempt-wide
refusal still voids every bot hour in the attempt, and bot hours are the
majority of the flat 500 (owner decision, 2026-09-04).

Three options were named: (1) periodic checkpoints from the host, (2) a report
shape emittable more than once, (3) accept the exposure.

---

## 2. How much is actually at risk: **zero hours, to date**

### 2.1 The ledger that exists

There is one real campaign ledger and it holds one attempt. Preserved at
`scratchpad/work/hours.jsonl` (the file #1050 re-derived byte-identically),
five rows, attempt `01a0687f-df41-7c78-9e62-cc6e09501d67`:

| | hours |
|---|---|
| bot (`5 * 900 / 3600`) | **1.250000** |
| human, four signed intervals | 0.483324 |
| **total** | **1.733324** |

Its manifest records `"refused_seats": []` and one 168 ms clamp. Its report
clears every attempt-wide clause with room:

| clause (`scripts/p4-attempt-accounting.py:413-443`) | floor | observed |
|---|---|---|
| `witnessing` | `true` | `true` |
| `total_false_positives` | `0` | `0` |
| `observation_coverage` | `≥ 0.95` | `0.9999816` |
| `deferral_ledger_balances` | `true` | `true` |
| `identity.impairment.loss` | `[0.03, 0.05]` | `0.03` |
| jitter injected | `> 0` | yes |
| `completed` | not `false` | `true` |

**Bot hours voided by an attempt-wide refusal, to date: 0.00 of 1.25 banked
(0%).** There has never been one.

### 2.2 The nearest thing to a counter-example, and why it no longer counts

#971 refused seven consecutive honest attempts, and each refusal took the bot
contribution with it, because assembly stages into a private directory and
writes nothing unless every row binds
(`scripts/p4-campaign-session.sh:127-131`). The seven refusals are preserved in
`scratchpad/971/before/{1,2,11,12,13,14,15}.log`, all identical in form:

```
p4-attempt-accounting: refusing to derive: session … banks 1.00028 min but slot 4
was connected for 0.9961 min (host tick count at the nominal rate); an interval
cannot exceed its seat's connected span
```

At 4 bots and 30–190 s apiece that is roughly **0.5 bot-hours** — and none of it
was campaign time; they were local verification attempts.

It is not evidence for options 1 or 2, for two reasons:

* the defect was fixed at the source (#971: the wall bracket replaced the
  nominal-rate tick count, `scripts/p4-attempt-accounting.py:452-478`);
* the *blast radius* was fixed independently by #1032 — a seat that over-claims
  is now clamped, or refused on its own, and lands in `refused_seats`
  (`scripts/p4-attempt-accounting.py:1081`, `:1133`, `:1232-1236`) while the
  attempt assembles without it. A bad seat has not been able to void bot hours
  since.

So the only measured loss of bot hours came from a class that is now, by
construction, per-seat. What remains attempt-wide is §2.1's seven clauses,
which have never fired on a real attempt.

### 2.3 What a checkpoint could even save

Split §2.1's seven clauses by *when they become knowable*:

| clause | knowable at | checkpoints buy |
|---|---|---|
| `witnessing` false | tick 0 (`--witness` absent) | **nothing** — every checkpoint fails identically |
| loss outside `[0.03, 0.05]` | tick 0 (config) | **nothing** |
| no jitter injected | tick 0 (config) | **nothing** |
| `total_false_positives != 0` | mid-run | the prefix before the signal |
| `observation_coverage < 0.95` | mid-run | the prefix |
| `deferral_ledger_balances` false | run close (a closure property, `docs/plans/multi-human-attempt-accounting.md:486-494`) | the prefix |
| `completed == false` | crash | the prefix |

Three of the seven are configuration, wrong from tick 0, and no emission
cadence recovers a single hour from them. That halves the already-empty
exposure before any code is written.

---

## 3. What a re-run costs: **~20 wall seconds for 1.25 player-hours**

The decisive asymmetry is in one line:

```rust
let real_time = !self.exteriors.is_empty();   // gates/p1-swarm/src/swarm.rs:3106
```

A run paces itself to the wall clock **only when an external seat is
connected** — because a human plays in real time and the host may not outrun
them. A bot-only run keeps its faster-than-real-time pacing, and the criterion's
hours are *simulated* hours, not wall hours (`scripts/p1-swarm-gate.sh:21-26`).

Measured on this box, the campaign's own generation shape
(`p1-swarm --peers 5 --seconds 900 --min-cells 1 --impaired --witness
--stamp-wall-clock`, release build), three runs:

| run | wall | `valid_attempt_seconds` | `completed` | coverage | false positives | bot hours |
|---|---|---|---|---|---|---|
| 1 | **22 s** | 900 | `true` | 0.9999815 | 0 | 1.25 |
| 2 | **19 s** | 900 | `true` | 0.9999815 | 0 | 1.25 |
| 3 | **19 s** | 900 | `true` | 0.9999815 | 0 | 1.25 |

Plainly:

```
re-run cost   ≈ 20 s of one core
recovers      = 1.25 bot player-hours
speed-up      ≈ 900 / 20 = 45x real time
```

**The bot half is re-runnable without a volunteer.** That is the whole of the
asymmetry #1048 was reasoning about, stated the other way round: the human hour
that #1048 rescued was unrepeatable — a person's evening, and #1051 showed two
macOS testers losing real ones. A voided bot generation costs twenty seconds of
a machine that is otherwise idle between generations.

A worked upper bound on the exposure that is being accepted: the largest single
loss possible is one generation's bot contribution, `B * seconds / 3600` =
`5 * 900 / 3600` = **1.25 h**, recovered for 20 s. Against a 500-hour target
that is 0.25% of the total, restorable at roughly 225 bot-hours per wall hour
of re-running on one core.

---

## 4. What options 1 and 2 would concretely take

The issue names `Swarm::report` consuming `self` as the obstacle. It is *an*
obstacle, and the smallest one. Read end to end, the change is in five places
and two of them are contract, not mechanism.

### 4.1 The host (option 2 — a report emittable more than once)

`fn report(mut self, ticks: u64, late_join: Option<LateJoinReport>) -> SwarmReport`
(`gates/p1-swarm/src/swarm.rs:3487`) has to become `&mut self`. The body is
overwhelmingly counter reads, so the blockers are enumerable:

* `core::mem::take(&mut self.docket)` (`:3488`) — the docket is only *read*
  afterwards (`docket.first_conviction(bot.node)`, `:3549`), so this is a borrow;
* four destructive `Option::take()`s, each feeding a `report` that consumes its
  receiver: `shot_interest_stats` (`:3637`), `presence_stats` (`:3639`),
  `delivery_gaps` (`:3686`), `interest_margin_stats` (`:3690`), against
  `fn report(self, end_tick: u64)` (`:1336`) and `fn report(self)` (`:1401`);
* `let bot = &mut self.bots[index]` (`:3538`) is benign — `Bot::replicas`
  (`gates/p1-swarm/src/bot.rs:2571`) and `Bot::tracked` (`:2579`) take `&mut`
  only for the Bevy query borrow and mutate nothing observable;
* the call site `self.report(ticks, late_join)` (`:3170`) is the tail of `run`;
  a periodic call goes into the tick loop beside the metronome (`:3110-3136`);
* `main.rs:2084-2087` writes one `--json` path — checkpoints need one path per
  checkpoint, and the supervisor's attempt directory layout
  (`scripts/p1-swarm-always-on.py`, `Supervisor.command`) has to carry them.

That much is a day's mechanical work. The next two are not mechanical.

* **`completed` is `false` for every checkpoint.** `let completed = ticks ==
  budget_ticks` (`:3529`), and `check_retained_attempt_clauses` refuses outright
  on `completed == false` (`scripts/p4-attempt-accounting.py:439-443`), with the
  words "a partial attempt preserves its rows for diagnosis and banks none of
  them". Checkpointing means reinterpreting that clause — a §6 change, and it is
  the clause that currently makes a crashed generation bank nothing.
* **`player_hours` in the report is computed from the budget, not the run.**
  `player_hours: self.total_peers() as f64 * self.config.seconds as f64 / 3_600.0`
  (`:3803`) uses `config.seconds`, so a prefix report would state the *whole*
  attempt's hours in its own `player_hours` field. Banking does not read that
  field — accounting derives from `valid_attempt_seconds`
  (`scripts/p4-attempt-accounting.py:944`) — so nothing mis-banks today, but a
  checkpointed report would carry a field contradicting itself. (Worth a
  one-line follow-up regardless of this decision; the doc comment at `:1030-1035`
  already says it is not the banked figure.)

### 4.2 The ledger (needed by option 1 *and* option 2)

Option 1 — periodic checkpoints — does not avoid any of this. Whatever the host
emits, the ledger has an explicit refusal in the way:

```jq
| if $actor == "bot" then
    (if ([ $rows[] | select((.actor // "bot") == "bot") ] | length) > 0
     then "attempt \($attempt) has already banked its bot contribution"
     else "" end)
```

`scripts/p4-ledger.sh:1532-1535`. **One attempt may bank exactly one bot row,
by name.** Removing that means giving the bot side the span-partition machinery
the human side got in #1048: overlap refusal plus a sum clause holding
Σ spans ≤ B × the attempt's span, with self-test fixtures for each — the human
equivalents are `scripts/p4-ledger.sh:1112-1133`.

And the span has nowhere to live. Both keys read the increment out of
`.session`:

* `run_key` — `.identity + (if .session.increment then {increment: {since_tick,
  until_tick}} else {} end)` (`scripts/p4-ledger.sh:1674-1678`);
* `measurement_key` — the same clause (`:1745-1749`).

A bot row has no `.session` at all: `bot_report.pop("session", None)`
(`scripts/p4-attempt-accounting.py:946`). So two bot checkpoints of one attempt
would hash to an **identical** `run_key` — same seed, impairment, target,
actor, attempt, no session, no span — and the second would be silently skipped
as an already-banked run. This is exactly the defect PR #1086 hit on the human
side and had to fix. A bot span therefore needs a new home outside `.session`
plus the same clause added to both key expressions, plus the schema validation
at `scripts/p4-ledger.sh:1272-1282`, plus a bot-side derivation loop replacing
the single-input block at `scripts/p4-attempt-accounting.py:943-980`.

### 4.3 The bill

Five files, three of them refusals with self-test fixtures
(`p4-ledger.sh`, `p4-attempt-accounting.py`, `p4-campaign-session.sh`), one §6
contract change (`completed`), and one host change to the harness that produces
every gate number. Against a measured exposure of zero hours and a measured
recovery cost of twenty seconds.

---

## 5. The argument that decides it, and it is not "bots are cheap"

Cheapness is the smaller half. The larger half is what the change would *do* to
the evidence.

#1048 accepted a real cost on the human side and recorded it
(`docs/plans/multi-human-attempt-accounting.md:605-616`): **an attempt-wide
refusal can no longer void a banked increment.** A false positive raised at
minute 14 used to void minutes 0–13 and cannot now. That was the right trade
for humans, because the alternative was a volunteer permanently losing an hour
they actually flew.

Extending it to bots buys the same property and pays a worse price. Bot hours
are the *majority* of the flat 500, and the only thing that makes them worth
counting is that each generation was witnessed end to end with zero false
positives against honest peers and ≥95% observation coverage. Banking a prefix
of a generation later found to have raised a false positive is banking hours
whose honesty evidence subsequently failed — and doing it to the half of the
total that carries the most weight, in exchange for something re-derivable in
twenty seconds.

Stated as the trade:

```
option 1/2:  keep <= 1.25 h per refusal, at the cost of hours whose
             attempt-wide evidence later failed, on the majority half
option 3:    lose <= 1.25 h per refusal, re-run it in ~20 s, and keep
             "every banked bot hour belongs to a generation that passed
              every attempt-wide clause" as an invariant
```

The invariant is worth more than 1.25 h. And it is *free* to keep, because the
thing it costs is re-runnable without a person.

---

## 6. Recommendation

**Option 3. Accept the exposure. Close #1092 with this memo as its rationale.**

Conditions under which this should be revisited, so the decision is falsifiable
rather than permanent:

1. **A refusal actually fires on a real generation.** Today it never has. If the
   campaign ledger accumulates attempt-wide refusals at any material rate, the
   arithmetic in §3 changes and this should be re-derived, not re-argued.
2. **`seconds` is raised past the point where a re-run stops being cheap.**
   §9.8 leaves `seconds` as a blast-radius/latency decision. At 900 s the blast
   radius is 1.25 h and 20 s of re-run. At, say, 8 hours it would be 40 h and
   ~10 minutes — still cheap, but the ratio is the thing to watch, and it is
   `seconds / 45` on this box.
3. **The bot half stops being re-runnable without a volunteer.** If a future
   campaign shape forces `real_time` on for bot-only runs — i.e. if
   `gates/p1-swarm/src/swarm.rs:3106` stops being the discriminator — the whole
   asymmetry above evaporates and option 2 becomes the right answer.

### One follow-up worth filing independently of this decision

`SwarmReport::player_hours` (`gates/p1-swarm/src/swarm.rs:3803`) is computed
from `config.seconds` while `valid_attempt_seconds` (`:3528`) is computed from
the ticks actually stepped. A short attempt therefore reports the *budget's*
player-hours in a field its own doc comment already flags as not the banked
figure (`:1028-1035`). Nothing banks from it, so this is a diagnostics defect
rather than an accounting one — but it is one line, and it is the field a reader
reaches for first.

---

## 7. Provenance of the numbers

| number | source |
|---|---|
| 1.733324 h banked, 1.25 bot / 0.483324 human, 5 rows, 1 attempt | `scratchpad/work/hours.jsonl` and `manifest.json`; matches PR #1050's re-derivation |
| coverage 0.9999816, 0 false positives, `completed: true`, loss 0.03 | `scratchpad/work/raw.json` (attempt `01a0687f-df41`) |
| 7 refused attempts, ~0.5 bot-hours, all per-seat span refusals | `scratchpad/971/before/*.log`, and #971's own table |
| 22 / 19 / 19 wall seconds for a 900 s, 5-peer, impaired, witnessed bot-only generation | run on this box, 2026-09-05, release build of `gates/p1-swarm` at `6fe6559` |
| every code claim | file:line, read on this tree |
