# #106 — rollout telemetry: what a shadow-to-live promotion decision can actually read today

**Status:** read-and-report only. This does not amend an ADR, change production
code, or touch any frozen crate. Every claim cites the tree as of this branch.

## Verdict

**No.** An operator today cannot distinguish "this control would have refused
40 honest players" from "this control would have refused 40 cheats" from any
surface the deployed binaries produce. The distinction is not a missing metric
on an existing report — it is a *classification* (cohort membership or triage)
that nothing in production computes, joins, or stores. The machinery built to
answer it — `RampMeter`, `HonestCohort`, `scripts/ramp-report.py` — is landed,
self-tested, and wired to nothing: the one component that computes D32 clause
(e)'s `fp_count(H, C, W)` is constructed only by tests, and the only committed
artifact carrying a nonzero cohort is harness traffic.

The split for whoever picks this up:

- **Already emitted, nobody reads it:** the per-event shadow telemetry on
  `orrery::ramp::shadow` (would-be cause, subject account, timestamp) for C1
  and C4; the durable `attest/` rows with `enforced: false`; the report script
  and its clause-(e) predicate.
- **Needs a code change:** meter wiring in the deployed binary; any production
  source for `HonestCohort` membership; a coverage denominator at the default
  log level; export of the C4/C5 counters; C2's degraded count; the
  auto-suspend trigger's inputs (rate window, RTT/loss bucket); a production
  artifact runner.

## 1. The promotion bar D32 demands, quoted exactly

D32 clause (e) ([docs/adr/0032-enforcement-ramp.md:434](../adr/0032-enforcement-ramp.md)):

> **A control C is promoted from shadow to live only when the predicate below
> holds, computed by [#221]'s report from the run's own artifacts — never
> re-derived — and reviewed under the gate at the end of this clause.**

```
promote(C) ⟺ production leg
             ∧ sensitivity leg
             ∧ review gate
             ∧ (C = C3 ⟹ auditor live, clause (g))

production leg ⟺ W ≥ 30 days of continuous production traffic
               ∧ fp_count(H, C, W) = 0
               ∧ coverage(H, C, W) ≥ 0.999
               ∧ |H| ≥ 100

fp_count(H, C, W) = |{o ∈ obs(C, W) : o.subject ∈ H ∧ o.would_act}|
coverage(H, C, W) = observed qualifying H activity / total qualifying H activity
```

The evidence the record itself demands before a control goes live
([0032:497-509](../adr/0032-enforcement-ramp.md)) — the pre-live review gate,
each promotion shipping a **dated promotion note** in the promoting PR
containing:

- the per-control row from clause (c) restated as-flown;
- **links to the [#221] report artifacts behind each predicate term, with
  their coverage denominators**;
- the [#222] gate-leg report (the sensitivity leg: a synthetic offender is
  refused by the enforcing process, committed by the shadow process, with the
  matching cause label);
- a re-read of the threat-model rows (docs/07 §1–§3) confirming the control
  addresses the threat it claims;
- for C3, the auditor-liveness evidence of clause (g).

Two properties of the predicate drive everything below. First, "measurably
negligible" resolves to **zero on the cohort**, and the coverage term is what
makes the zero mean something: "a false-positive rate of 0 over a cohort
nobody watched is not evidence, it is blindness with a clean conscience"
([0032:453-457](../adr/0032-enforcement-ramp.md)). Second, production shadow
data proves *specificity only* — "production may contain no guilty traffic at
all"; sensitivity is proven by [#222]'s injected positives
([0032:480-487](../adr/0032-enforcement-ramp.md)).

## 2. What shadow mode emits today

The stable target is defined once:
`pub const SHADOW_TARGET: &str = "orrery::ramp::shadow"`
([crates/orrery_persistd/src/intent/shadow.rs:55](../../crates/orrery_persistd/src/intent/shadow.rs)).
Every emit site in the tree, per control:

| Control | Emit site | Fields carried | Reachable from a deployed binary? |
|---|---|---|---|
| C1 attestation quorum | `shadow::emit`, [intent/shadow.rs:364-395](../../crates/orrery_persistd/src/intent/shadow.rs), called from `observe` at [intent/mod.rs:1952-1963](../../crates/orrery_persistd/src/intent/mod.rs) | `control`, `intent_id`, `issuer`, `account` (Option), `cell_epoch`, `would_act`, `verdict` (cause label), `observed_at_ms` | Yes — `--attestation-enforcement shadow` ([bin/persistd.rs:2489-2493](../../crates/orrery_persistd/src/bin/persistd.rs)) |
| C4 authority correction | [gateway.rs:8421-8433](../../crates/orrery_persistd/src/gateway.rs) | `control`, `subject`, `entity`, `leases`, `recipients`, `over_limit`, `payload_bytes`, `payload_digest`, `action = "would_revoke_and_broadcast"` | Yes — `--authority-correction shadow` ([bin/persistd.rs:187-188](../../crates/orrery_persistd/src/bin/persistd.rs)), durable-row poller at [persistd.rs:1322-1326](../../crates/orrery_persistd/src/bin/persistd.rs) |
| C5 strikes (gateway) | `would_move_standing` [gateway.rs:4296-4304](../../crates/orrery_persistd/src/gateway.rs); `would_refuse_hello` [gateway.rs:4356-4365](../../crates/orrery_persistd/src/gateway.rs); `would_terminate_session` [gateway.rs:5997-6005](../../crates/orrery_persistd/src/gateway.rs) | `control`, `node`/`issuer`, `account`, `action`, `observed_at_ms` | No — `standing_feed` defaults `None` and the publisher "does not exist yet" ([gateway.rs:3206-3213](../../crates/orrery_persistd/src/gateway.rs)); persistd has no strikes wiring at all |
| C5 strikes (coordinator) | `would_refuse_hello`, [orrery_coordinator/src/server.rs:1256-1265](../../crates/orrery_coordinator/src/server.rs) | `control`, `issuer`, `account`, `action`, `observed_at_ms` | No — `standing_feed: None` default ([server.rs:479](../../crates/orrery_coordinator/src/server.rs)); the binary has no flag to change posture or wire a feed |
| C2 quarantine validation | none | — | No. "C2-shadow degrades to counting quarantined-session intents — a count this tree does not emit on any target" ([intent/ramp.rs:716-721](../../crates/orrery_persistd/src/intent/ramp.rs)) |
| C3 write annulment | none | — | Control does not exist ([intent/ramp.rs:723-726](../../crates/orrery_persistd/src/intent/ramp.rs); D32 clause (c) note 3) |

What one observation records, and what it does not. The C1 event carries the
exact `RejectionCause` label `Required` would have returned — the borrowed
vocabulary D32 clause (b) mandates
([intent/shadow.rs:28-33](../../crates/orrery_persistd/src/intent/shadow.rs)),
the submitting account, and the intent id, which "joins to `intent/{intent_id}`
and — under shadow — to the `attest/{intent_id}` row the commit wrote with
`enforced: false`" ([shadow.rs:163-165](../../crates/orrery_persistd/src/intent/shadow.rs);
durable shape proven at
[tests/intent_witness_epoch.rs:958](../../crates/orrery_persistd/tests/intent_witness_epoch.rs)).
It does **not** carry the full predicate input — no attestation vector, no
eligible vector, no draw key — but those are recoverable from the durable rows
the intent id names, so the join is possible without re-running traffic, which
is what clause (b) obligation (2) requires.

**Is it enough to compute a false-positive rate?** Structurally yes; in
production, no. Three gaps sit between the events and clause (e)'s ratio:

1. **No meter is wired.** `RampMeter`
   ([intent/ramp.rs:334-504](../../crates/orrery_persistd/src/intent/ramp.rs))
   computes the entire cohort block — `fp_count`, `coverage` with its
   numerator and denominator, `|H|` by halves, `accounts_would_act` spread,
   `by_cause` — from two counting points: `record_qualifying` at the first
   statement of `check_at` ([intent/mod.rs:1799-1801](../../crates/orrery_persistd/src/intent/mod.rs))
   and `record` at the shadow arm. The deployed binary constructs
   `BaselineIntentValidator::shadow(...)` — observer `None`
   ([bin/persistd.rs:2489-2493](../../crates/orrery_persistd/src/bin/persistd.rs)) —
   and `shadow_observing` ([intent/mod.rs:1143-1153](../../crates/orrery_persistd/src/intent/mod.rs))
   has **no caller outside tests** ([intent/mod.rs:3541, 3803, 3821, 3888,
   4019-4022](../../crates/orrery_persistd/src/intent/mod.rs)).
2. **The log-only denominator is not trustworthy.** Would-act events are
   `info`; everything else is `debug`
   ([shadow.rs:345-361](../../crates/orrery_persistd/src/intent/shadow.rs)),
   and persistd's default filter is `info`
   ([bin/persistd.rs:869-888](../../crates/orrery_persistd/src/bin/persistd.rs),
   plain-fmt stderr subscriber at [:894-899](../../crates/orrery_persistd/src/bin/persistd.rs)).
   The module doc itself warns: "a denominator assembled by counting the log
   lines a level filter chose to keep is not a denominator"
   ([shadow.rs:359-361](../../crates/orrery_persistd/src/intent/shadow.rs)).
   At the default level an operator sees the numerator events and nothing to
   divide them by.
3. **The counters that do exist are exported nowhere.** `GatewayStandingMetrics`
   counts `shadow_hello_would_refuse` / `shadow_sessions_would_terminate`
   ([gateway.rs:2878-2884, snapshot at :2963-2966](../../crates/orrery_persistd/src/gateway.rs))
   and `AuthorityCorrectionMetrics` counts C4's evaluated/shadow-suppressed
   split ([gateway.rs:2888-2913, accessor :5526](../../crates/orrery_persistd/src/gateway.rs))
   — but the `--metrics-jsonl` drain chain
   ([bin/persistd.rs:410-434](../../crates/orrery_persistd/src/bin/persistd.rs))
   writes neither. The coordinator's `shadow_hellos_would_refuse`
   ([server.rs:731, snapshot :913, :1024-1030](../../crates/orrery_coordinator/src/server.rs))
   is snapshotted and its shutdown log prints three other fields
   ([bin/orrery-coordinator.rs:177-183](../../crates/orrery_coordinator/src/bin/orrery-coordinator.rs)).
   A counted-and-dropped counter is the exact "documented alarm that is not
   exported is a paragraph" defect persistd's own reporter names
   ([bin/persistd.rs:522-526](../../crates/orrery_persistd/src/bin/persistd.rs)).

The decision template itself is landed and rigorous: the committed artifact
[docs/data/ramp-shadow-2026-08-22.json](../data/ramp-shadow-2026-08-22.json)
carries `provenance.traffic: "harness"` and says so in its own note, and
`scripts/ramp-report.py` renders clause (e)'s predicate against the floors
(W ≥ 30, fp = 0, coverage ≥ 0.999, |H| ≥ 100), refuses a nonzero
`accounts_truncated`, and mutation-checks the 0-of-0 vs 0-of-10000
distinction. The [#222] gate leg
([scripts/ramp-shadow-gate.sh](../../scripts/ramp-shadow-gate.sh)) proves
shadow-observes and shadow-does-not-act against two opposed gateway processes.
Both read the **harness's** gateway binary, whose JSON logs the gauntlet parses
([gates/p5-dupe-gauntlet/src/main.rs:1593-1634](../../gates/p5-dupe-gauntlet/src/main.rs));
production persistd's subscriber is plain-fmt, not JSON, so that reader does
not transfer unchanged.

## 3. The decisive question: 40 honest or 40 cheats?

**No.** The telemetry records *who* would have been refused and *why*; nothing
records or computes *whether they were honest*. Honesty is not an event
property, it is a set membership — and the set has no production source:

- `HonestCohort` "Membership is an **input**. Nothing in this module infers
  it: D32 requires it be 'derivable from durable facts plus a recorded sample
  decision — never from "seemed fine"'"
  ([intent/ramp.rs:222-225](../../crates/orrery_persistd/src/intent/ramp.rs)).
  Every constructor outside the type itself is a test
  ([intent/ramp.rs:766-772, :963-970](../../crates/orrery_persistd/src/intent/ramp.rs);
  [intent/mod.rs:3911, :3972, :4027, :4079](../../crates/orrery_persistd/src/intent/mod.rs)).
  There is no loader, no CLI, no durable row, no identity join, no harness
  hand-off.
- Even with the meter wired, `snapshot(&cohort)` needs the cohort handed in
  ([intent/ramp.rs:415](../../crates/orrery_persistd/src/intent/ramp.rs)) — an
  operator act (arming accounts) plus a human sampling decision for the
  natural half, per clause (e)'s definition of H
  ([0032:461-468](../adr/0032-enforcement-ramp.md)).
- For the witness pipeline the equivalent classification is #240's triage:
  "every discrepancy report triaged to honest-or-real. An untriaged report is
  not a zero." That campaign is **open** — the 500 player-hours have not run.

What an operator *could* do today, by hand: run C1 in shadow, grep stderr for
`orrery::ramp::shadow` `would_act = true` events (visible at the default info
level), extract the `account` field, and join against a personally maintained
list of known-good accounts. Possible in principle, done by nothing, and with
no denominator even at `debug` level unless the level is raised — which
returns to the filtered-log caveat above. A decision procedure that lives in
an operator's head and a text editor is not clause (e)'s "computed by [#221]'s
report from the run's own artifacts — never re-derived".

So the two scenarios are genuinely indistinguishable in the produced evidence:
forty would-act events from forty distinct accounts read identically whether
the accounts are the operator's own armed bots or a cheat ring. The counters
that would separate them — `fp_count` over H, and the fleet `spread` that
clauses (f)'s trigger reads — exist in code and in no deployment.

## 4. What is already measured nearby

**The witness discrepancy pipeline (P4's shadow telemetry).** The report path
exports an exhaustive outcome split through `--metrics-jsonl`: `verdicts`,
`adjudicated`, `confirms`, `exonerates`, `evidence_forged`, `unadjudicable`,
`refused_no_adjudicator`, `refused_rate_limited`
([bin/persistd.rs:755-771](../../crates/orrery_persistd/src/bin/persistd.rs),
`GatewayReportMetrics`). This measures what adjudication *did* — not whether
its subjects were honest. The honest/real split is a human triage layer, and
it is #240's campaign, which has not run.

**#240's honest-cohort campaign vs the ramp's shadow observation.** Not the
same measurement wearing different names — the same *discipline* applied to
different predicates. #240 measures the witnessing detection bands: discrepancy
reports raised against honest play under injected impairment, per session-hour,
across three platforms. The ramp measures per-control would-be enforcement
actions over intents and verdicts, per clause (e)'s predicate. #240's own body
draws the same line: "An honest-cohort denominator is exactly what #221
proposes to measure for the *enforcement* ramp; the same discipline applies
here and the two should share machinery rather than each inventing it." D32
Context §5 ([0032:115-123](../adr/0032-enforcement-ramp.md)) cites P4's
"0 false positives at 0.9999992 observation coverage" as the *shape* clause
(e) demands per control — "a count, a coverage denominator, and named
controls" — not as transferable evidence. A zero from #240 would not satisfy
`fp_count(H, C1, W) = 0`: different predicate, different traffic, different
window. What *is* reusable is the cohort machinery — #240's armed-honest
bot/human mix is exactly `HonestCohort`'s two halves — and nothing wires the
campaign's accounts into it. Also relevant: #240 is the P4 exit gate and is
still open, so the zero it would produce does not exist yet either.

**The invariant auditor (#224 cadence settled; [#848] spike; auditor landed).**
`crates/orrery_persistd/src/audit.rs` (hourly incremental) and
`audit/conservation.rs` (daily full) exist and emit `AuditFinding`s carrying
`account`, `item`, `asset`, `receipt_intent_id`
([audit.rs:161-175](../../crates/orrery_persistd/src/audit.rs)), with the rule
"Findings are **reports, never actions**" ([audit.rs:158-160](../../crates/orrery_persistd/src/audit.rs)).
For promotion evidence its role is narrower than it looks: clause (g) makes it
a **liveness gate for C3 only**
([0032:570-605](../adr/0032-enforcement-ramp.md)) — "deployed, sweeping, and
emitting" — not a false-positive instrument. A conservation finding is a fact
about the economy, not a label on a would-be refusal. It could retroactively
corroborate that a control's targets were genuinely stealing, but nothing
today joins auditor findings to ramp observations, and no control's promotion
predicate reads them.

## 5. What a promotion decision needs, and where each piece would come from

The operator decision, per clause (e), is: read the [#221] artifact for the
control, check `fp_count = 0` over H with coverage ≥ 0.999 and |H| ≥ 100 over
W ≥ 30 days of production traffic, check spread for clause (f) context, attach
[#222]'s sensitivity-leg report and the threat-model re-read, and (for C3) the
auditor-liveness evidence. Everything in that sentence except the [#222] leg
and the prose is missing in production. Separated by what exists:

**Already emitted, nobody reads it:**

1. **Per-event C1 shadow observations** — cause label, subject account,
   intent id, timestamp on `orrery::ramp::shadow`
   ([intent/shadow.rs:364-395](../../crates/orrery_persistd/src/intent/shadow.rs)).
   Sufficient to reconstruct a cohort join after the fact, given a cohort.
2. **C4's would-be correction payloads** — digest, recipients, byte size
   ([gateway.rs:8421-8433](../../crates/orrery_persistd/src/gateway.rs)),
   exactly clause (d)'s recorded set for C4.
3. **Durable inertness evidence** — shadow-period `attest/` rows stamped
   `enforced: false` (D32 clause (d); proven
   [tests/intent_witness_epoch.rs:958](../../crates/orrery_persistd/tests/intent_witness_epoch.rs)).
4. **The report template** — `scripts/ramp-report.py` + the
   `orrery.ramp.report/1` schema, floors and guards included. The decision
   format exists; only the production input does not.
5. **In-process C4/C5 counters** — `AuthorityCorrectionSnapshot`,
   `GatewayStandingMetrics`, the coordinator's shadow counters. Counted
   unconditionally; exported by nothing (§2, gap 3).

**Needs a code change:**

1. **Wire the meter into the deployed shadow constructor** —
   `BaselineIntentValidator::shadow_observing(...)` with a `RampMeter` at
   [bin/persistd.rs:2489](../../crates/orrery_persistd/src/bin/persistd.rs),
   plus a periodic `RampSnapshot` export (a `--metrics-jsonl` record or a
   written artifact). This is the single change that turns the emitted stream
   into clause (e)'s numbers, because the coverage denominator's counting
   point ([intent/mod.rs:1799-1801](../../crates/orrery_persistd/src/intent/mod.rs))
   only fires through an observer.
2. **A production source for H.** Where the armed and natural sets are written,
   recorded, and auditable — clause (e) requires "durable facts plus a
   recorded sample decision". Options are a decision for the implementing
   issue; the type and the join already exist.
3. **A denominator at the default log level** — either the meter (preferred,
   per 1) or an explicit warning that log-derived denominators are level-filter
   dependent. The current info/debug split
   ([shadow.rs:345-361](../../crates/orrery_persistd/src/intent/shadow.rs))
   makes the log-only path structurally unable to produce one.
4. **C2's degraded observation** — the quarantined-session intent count clause
   (d) accepts while C1 is off; nothing emits it
   ([intent/ramp.rs:716-721](../../crates/orrery_persistd/src/intent/ramp.rs)).
5. **Auto-suspend's inputs** — the rate term needs a sliding 60-minute window
   and a trailing 7-day hourly median "this artifact does not carry"
   ([scripts/ramp-report.py](../../scripts/ramp-report.py), spread-term note);
   the RTT/loss dimension R-6 requires "cannot be measured from what is
   emitted today" ([intent/ramp.rs:69-76](../../crates/orrery_persistd/src/intent/ramp.rs));
   and no monitor calls `AttestationPosture::auto_suspend()` in any binary —
   the demotion write exists
   ([intent/mod.rs:969-1020](../../crates/orrery_persistd/src/intent/mod.rs))
   and has no production caller.
6. **A production artifact runner** — clause (e) says the predicate is
   "computed by [#221]'s report from the run's own artifacts"; today the only
   artifact is written by an ignored test over harness traffic
   ([intent/ramp.rs:78-88](../../crates/orrery_persistd/src/intent/ramp.rs)).
7. **C5's shadow period needs its consumers wired before it can observe at
   all** — `standing_feed` has no publisher and no binary flag on either
   process (§2 table), so C5's observation period cannot begin.

Items 1–3 are the promotion predicate's spine; without them the observation
period produces numerator-shaped logs. Items 4–7 block specific controls or
the auto-suspend half of the ramp, not the first C1 promotion.

**No dashboard is proposed here.** The gap is one wired counter, one cohort
source, and one export — the reading surface (`ramp-report.py`) and the gate
(`ramp-shadow-gate.sh`) already exist and already enforce the record's floors.
