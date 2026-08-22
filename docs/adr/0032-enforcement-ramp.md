# ADR-0032: The enforcement ramp: shadow semantics per control, the flag inventory, promotion evidence, and auto-suspend

**Status:** Accepted · **Date:** 2026-08-21 · **Decision:** D32

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Amended 2026-08-21, while still Proposed ([#248]).** The merged first draft
of this record allocated the one-byte key family `b'y'` to `ramp/`, justified
by `'y'` appearing nowhere in `keyspace.rs` — a check of the code, not of the
accepted set. Accepted [ADR-0031]'s resolved question 4 had already allocated
`y` to `strike/`, and an accepted record does not yield to a proposed one, so
this record moved: `ramp/` spends **no family byte** and lives as the
`b"vr"`-discriminated sub-span of the registered `v` family. Clause (c)
carries the new allocation, the byte-budget arithmetic that forced it, and
the allocation rule whose absence produced the collision. Nothing else in the
record changed.

**Supersedes:** nothing. It **implements the policy half** of
[D17.3](0017-risks-and-open-questions.md) for epic #106 — #147 shipped the
K-of-N switch and refused to ship the ramp ("this code takes no position on
when a deployment flips it, on shadow-to-live ramping, or on verdict-rate
auto-suspend",
`crates/orrery_persistd/src/intent/mod.rs:742-744`) — and it **closes**
[D29](0029-low-population-path.md)'s open question on annulment-on-expiry
(`0029:775-780`). It also **corrects a factual pointer** in that open
question, which attributes rollout policy to #105; clause (a) owns the
correction and the erratum text applied alongside it. Nothing in any accepted
record's decision clauses changes.

**Siblings.** [#217] builds the attestation shadow arm and the deployment
switch this record specifies. [#221] produces the measurements clause (e)
consumes. [#222] proves, as a permanent gate leg, that shadow observes and
does not act. [#205] fixes the thresholds the ramp ramps toward — every
number this record defers, it defers there. [#224] owns the economy-wide
invariant auditor that clause (g) wires into the ramp as a gate. Where a
decision belongs to those records, it is listed under Open questions rather
than guessed.

## Context

Seven facts about the landed tree, each read before it was written here.

### 1. Every record defers the ramp, and two of the deferrals point at each other

[docs/11-roadmap.md:867](../11-roadmap.md) states the deliverable:
"*Enforcement switches:* write refusal/annulment, in-session authority
correction broadcast, strikes — **each independently feature-flagged, ramped
from shadow to live per D17.3**." #147 declined to build the ramp. D29 left
its annulment-on-expiry default unset "because enforcement rollout policy is
#105's and explicitly out of this record's scope"
(`0029:775-780`). #105's scope line reads: "*Enforcement rollout policy and
identity-service operations are tracked separately*" — it does not claim
them. So D29 points at #105, #105 points away, and the actual owner is epic
[#106], whose goal line is the ramp itself. Clause (a) resolves this rather
than letting a reader triangulate it.

### 2. No fleet can reach any enforcement arm

The deployed binary hardcodes the permissive validator:
`validator: Arc::new(BaselineIntentValidator::permissive())`
(`crates/orrery_persistd/src/bin/persistd.rs:2109`, inside `gateway_config`,
with no CLI flag reaching it). `BaselineIntentValidator::enforcing`
(`crates/orrery_persistd/src/intent/mod.rs:784-793`) has **no caller in the
binary** — the only non-test callers in the workspace are library tests and
`p5-dupe-gauntlet/src/main.rs:209`, a separate harness workspace. The
executor side is equally dark: `FdbIntentExecutor::recording_epochs`
(`crates/orrery_persistd/src/intent/fdb.rs:234-241`) is never called by the
binary, so the deployed executor holds no witness-epoch authority and writes
no `epoch/` or `attest/` rows at all — its own doc marks `None` as "the
enforcement-off build" (`intent/fdb.rs:128-131`). A deployment therefore has
no ramp lever of any kind; a recompile is the only switch, which is no
switch.

### 3. "Off" observes nothing, and nothing in the tree observes without acting

`AttestationEnforcement` has exactly two arms, `Off` and `Required`
(`intent/mod.rs:745-759`). `Off`'s doc says "'Off' means *the quorum* is off,
not that an attestation may be a forgery" (`:750-753`) — accurate, and also
an admission that `Off` is not shadow: `check_at` returns before resolving
any epoch (`intent/mod.rs:1216-1223`), so no quorum predicate is evaluated,
no would-be verdict is computed, and nothing is observed. `Required`
evaluates and refuses. There is no arm that evaluates, records, and admits —
which is what [D17.3](0017-risks-and-open-questions.md) requires to exist
before any control acts: "the strike pipeline must launch in shadow mode
(telemetry-only) first."

Quarantine full-validation landed the same month with the opposite problem:
it is **unconditional**. A quarantined session's attestation set is verified
up front purely on the token's standing bit
(`intent/mod.rs:996-1002`), with no flag at all. So of the roadmap's "each
independently feature-flagged" controls, the count of controls that are
feature-flagged today is zero.

### 4. The commit path re-proves the quorum independently of admission

Admission is not the only place a below-quorum intent dies.
`record_witness_epoch` (`crates/orrery_persistd/src/intent/fdb.rs:837-932`)
re-derives the eligible vector from the **durable** epoch row, re-derives
`required(I)` under the durable draw key, and **refuses at commit time** any
intent whose carried attestations do not contain it (`:902-919`) — the
protection that closes the D26 sibling-handover gap. Whenever the executor
holds an epoch authority it also writes the `attest/{intent_id}` row
(`keyspace.rs:920`) — the recorded eligible vector D27 clause (f) requires —
into the same transaction, unconditionally (`:924-930`); with no authority it
writes nothing (`NotApplicable`, `:847-850`).

This matters to shadow twice. First, a shadow mode that admits below-quorum
intents at admission will have them refused at commit unless the executor's
re-proof is mode-aware — shadow would act after failing to act, which is the
worst of both. Second, an `AttestRow` that looks enforced but was not is a
false audit trail, which is worse than none ([#217] flags exactly this).
Clause (d) decides both.

### 5. What the ramp protects against is calibrated in P4's prior art

P4 shipped witnessing in shadow and exited on a measured zero-false-positive
gate: the P1 swarm harness reports "0 false positives at 0.9999992
observation coverage, and the conviction and armed-honest controls both
clean" (AGENTS.md, gate-status summary). That is what a zero-FP claim looks
like when it is real — a count, a coverage denominator, and named controls —
and clause (e) demands the same shape per enforcement control rather than a
narrative.

### 6. The demotion trigger exists as prose and nowhere as a number

"A spike in deviation verdicts across unrelated accounts auto-suspends
enforcement for that rule version" ([docs/07:237](../07-witnessing.md)) names
the shape — account spread, not event volume; rule-version scope — and
[R-6](../11-roadmap.md) (`11-roadmap.md:904`) names the early warning
("discrepancy-report rate correlating with peer RTT/loss rather than
accounts"). Neither names a rate, a window, or a fallback state. An alarm
with no threshold cannot fire, and one that fires into an undefined state is
worse. Clause (f) supplies all three.

### 7. The auditor constraint is tracked nowhere

"Economy-wide invariant auditing cadence … Must be live before enforcement is
fully on — post-hoc-only correction is the documented GTA Online failure"
(`11-roadmap.md:917`, deadline "P5 exit") had no owner until [#224] was
filed, and no wiring into any ramp decision. Clause (g) decides whether the
auditor gates any control going live, and states the dependency in both
directions.

## Decision

### (a) Ownership: the ramp is epic #106's, recorded here; D29's pointer is corrected by erratum

> **Enforcement rollout policy — shadow semantics, flag inventory, promotion
> criteria, auto-suspend, and reversibility bounds for every enforcement
> control — is owned by epic [#106] and recorded in this document. Neither
> [#105] nor any child of [#105] owns it; D29's statement that "#105" owns it
> was a mis-citation, corrected by the erratum note applied to
> `0029:775-780` alongside this record.**

The fix is **both** of the forms the issue offers, each doing the job only it
can do. The clause above is the decision, reached the normal way; the erratum
is a bracketed annotation at the citation site so a reader of D29 alone is
not sent to an epic that disclaims the work. No accepted decision text is
rewritten — D29 declines to set the default, and still does; this record sets
it (clause h). This is the same treatment D28 gave `docs/09` §6: name the
divergence, correct the pointer, change no ruling.

### (b) Shadow is a mode with three obligations

> **For any enforcement control C, shadow mode means: (1) C's predicate is
> evaluated in full against real traffic, including every sub-predicate live
> mode would evaluate; (2) the action live mode would take is recorded, with
> identifiers fine enough to compute clause (e)'s promotion evidence without
> re-running traffic; (3) none of C's actions is taken. On an internal error
> during evaluation, shadow degrades to "record unevaluated" — never to an
> action.**

Three corollaries, each excluding something "shadow" is liable to drift into:

- **Shadow is not Off.** `Off` evaluates nothing (`intent/mod.rs:1216-1223`)
  and therefore calibrates nothing. A control in `Off` has no observation
  period and cannot be promoted from it.
- **Shadow is not live-with-a-log-line.** The suppressed actions are the
  point: refusals do not refuse, broadcasts do not send, strikes do not
  count, annulments do not annul. If any action escapes, the mode is live
  whatever the enum says, and [#222]'s gate leg exists to catch precisely
  that.
- **Observations reuse the existing vocabulary.** Would-be refusals carry the
  exact `RejectionCause` and its stable log label
  (`intent/mod.rs:500-520`) that `Required` would have returned — never a
  parallel taxonomy — so a shadow report joins against rejection logs without
  a translation table. Observations are telemetry only; nothing shadow
  computes reaches the wire, matching the doctrine that causes are logged
  rather than sent (`intent/mod.rs:328-332`).

> **A fixed set of checks is correctness, not enforcement, and never ramps,
> never suspends, and is never flag-gated: the intent envelope's signature
> and issuer binding, the `MAX_ATTESTATIONS` cap, the duplicate-attestation
> rule, the self-witness refusal, the D27 attestation preimage, D29 clause
> 4's provisional-input quarantine, and the FoundationDB transaction's
> authority over durable truth.**

This restates `intent/mod.rs:750-753`'s rule — "'Off' means *the quorum* is
off, not that an attestation may be a forgery" — as a property of the whole
inventory. These checks protect the cluster against malformed or forged
input; they punish nobody and calibrate nothing, so there is nothing to
ramp. A flag that could disable a signature check would be an
always-available denial-of-service lever pointed at the cluster by whoever
holds the operator row.

### (c) The flag inventory: five controls, two layers each

> **Each enforcement control has exactly one runtime lever: a durable
> operator-set posture row, `ramp/{control}`, polled by every `persistd`
> process on the existing 1 s maintenance sweep — never read on the hot
> path. Each also has exactly one startup default, a CLI argument, which
> seeds the row's absence and pins tests. The durable row overrides the CLI;
> auto-suspend writes the same row. Maximum time from an operator's decision
> to a control stopped in a running fleet: one poll interval plus apply,
> bounded at 2 s wall clock; intents already past validation complete under
> the prior mode (bounded above by D16's 10 ms commit p99).**

Why two layers. A CLI-only flag is not reversible on an incident timescale:
rolling a gateway restart drops sessions and takes minutes, while
auto-suspend needs to demote a misbehaving control fleet-wide in seconds. A
hot-path read is not affordable: the admission path performs no FDB round
trip by design (#147's acceptance evidence; D16's budget), so the posture
must be cached and refreshed asynchronously. The gateway already runs a 1 s
maintenance loop (`gateway.rs:4777`), and the epoch cache rides it
(`:4829-4831`, the D28 arrangement); posture polling joins that loop, and a
posture change lands on every process within one sweep period.

The rows are durable in FoundationDB, and that much is forced, not chosen:
auto-suspend (clause f) is a *persistd-written* posture change that every
process in the fleet must see within one poll interval and that must survive
every restart — a config file or CLI default cannot be written by a tripping
gateway, the coordinator holds no durable state by design ([ADR-0031]
Context), and FoundationDB is the only shared durable store in the system.
What is **not** forced is a family byte, and `ramp/` does not get one:

> **`ramp/{control}` is the sub-span `b"vr" ‖ control-name` inside the
> registered `v` family — no new family byte is spent. The full ramp scan is
> `[b"vr", b"vs")`. The existing `content/version` row is the bare one-byte
> key `[b'v']` (`keyspace.rs:458`), which sorts before every two-byte
> `v ‖ …` key (`[0x76] < [0x76, …]`, the same ordering argument [ADR-0031]
> clause (a) makes for its range bounds), so the landed key is untouched and
> no migration exists. The implementing change registers nothing new in
> `all_key_families_are_range_disjoint` (`keyspace.rs:2777`) — byte `v` is
> already in its table — and must instead add a sub-span assertion in the
> style of the `d`/`l` sub-discriminator tests: `[b'v']` < every `b"vr"` key
> < `[b'w']`.**

This paragraph replaces the first draft's allocation of family byte `b'y'`,
and the arithmetic is why the fix is a sub-span rather than another byte.
`registered_families()` (`keyspace.rs:2662`) holds eighteen one-byte
families — `a c d e f g i k l m n o p r s u v w` — and six more bytes are in
use as exclusive range ends (`b h j q t x`). Of the two lowercase bytes left,
accepted [ADR-0031] resolved question 4 allocates `y` to `strike/` and closes
its budget with `z` to `jarchive/`. The clean-byte budget is therefore
**zero**, and five absent-by-default singleton rows would be the worst
possible way to spend a byte even if one remained. [ADR-0031]'s Consequences
already name the fork for the next family — "adopt sub-discrimination as
this one does or open the question of a two-byte family space" — and this
record takes the first tine.

`v` is the right host, not merely an available one. Its one landed key kind,
`content/version`, is a deployment-plane singleton: written by the world
seeder at seed time (`docs/12-world-seeding.md` §9.3), read by `persistd`,
never written on any hot path. `ramp/` rows have the same shape — at most
five rows, written rarely by the operator plane and by auto-suspend, polled
by every process. [ADR-0031] clause (d)'s single-writer objection — the
reason `strike/` did not share `d` — is about transactional coupling: `db`
must be written with `da` atomically, and index staleness there is a
security property. No such coupling exists here: no transaction spans the
`v` sub-spans, no scan crosses them, and a posture row's staleness is
bounded by the poll interval regardless of who wrote the content row. The
discriminator is ASCII at a fixed offset, per the rule [ADR-0031] draws from
the `lease_key` finding — never an id's high byte.

**The allocation rule, so the next family is not answered ad hoc a third
time.** This collision happened because a proposed record checked the tree
and not the record set — and it is the second such check (D28 chose `e`/`f`
against "the fourteen in `keyspace.rs`"; it happened to be right). The rule:

> **The free list for a key-prefix allocation is the lowercase bytes minus
> `registered_families()` minus every byte an accepted record allocates or
> earmarks — the code alone is never sufficient, because a
> documented-but-unimplemented family is still allocated. That list is now
> empty. A new key kind therefore takes an ASCII sub-discriminator inside
> the existing family whose writer, retention and scan profile it matches,
> as this record does. A kind that genuinely cannot — because sharing would
> put a foreign writer inside a transactionally-coupled family ([ADR-0031]
> clause (d)) or because no family can host its scan shape — is grounds for
> a dedicated ADR that amends [ADR-0031]'s budget arithmetic and defines a
> multi-byte family scheme. It is never grounds for taking `y`, `z`, or a
> range-end byte in passing.**

The value:

```rust
struct RampPosture {
    mode: RampMode,          // Off | Shadow | Live
    source: PostureSource,   // Default | Operator | AutoSuspend
    set_at_ms: u64,
    reason: String,          // ≤ 256 B, free text, logged on apply
    incident_id: Option<[u8; 16]>, // set by AutoSuspend, cleared by Operator
}
```

No row for a control means "the CLI default stands". Writes are rare
(operator incidents, suspend trips), conflict-free (one row per control),
and out-of-band — how an operator authenticates a posture write is D12 ops
work and is this record's open question 1, not a silent assumption.

The inventory. Startup defaults are chosen to **preserve today's observable
behavior exactly** — the same property #147 demanded of its switch — so
landing the flags changes nothing until an operator does:

| Control | Acts by | Runtime row | CLI default | Default |
|---|---|---|---|---|
| C1 attestation quorum (#147) | refusing intents that fail D27 clause (d)'s predicate | `ramp/attestation_quorum` | `--attestation-enforcement off\|shadow\|required` | **off** |
| C2 quarantine full-validation (#149) | forcing quarantined sessions off the attestation shortcut, onto full validation | `ramp/quarantine_validation` | `--quarantine-validation off\|shadow\|live` | **live** |
| C3 write refusal/annulment (D10 item 3b) | refusing pending writes and annulling journaled effects on a guilty verdict | `ramp/write_annulment` | `--write-annulment off\|shadow\|live` | **off** |
| C4 authority-correction broadcast (D10 item 3a) | revoking the offender's leases and broadcasting the adjudicated state | `ramp/authority_correction` | `--authority-correction off\|shadow\|live` | **off** |
| C5 strikes (D10 item 3c) | filing verdict strikes; thresholds crossing into standing changes | `ramp/strikes` | `--strikes off\|shadow\|live` | **off** |

Notes the table cannot carry:

- **C2 defaults live because it is already live.** Its landed behaviour is
  keyed on identity's signed standing bit (`intent/mod.rs:996-1002`), and
  its false-positive surface is identity's threshold choice — [#205]'s
  decision, not the gateway's. Demoting C2 is the incident response to a
  mass-quarantine bug, not a calibration step.
- **C1's `required` arm is the existing `AttestationEnforcement::Required`;
  `off` is the existing `Off`.** Only `shadow` is new code, and it is
  [#217]'s to build. The CLI replaces the hardcoded constructor at
  `persistd.rs:2109`.
- **C3, C4 and C5 do not exist yet.** Their flags are specified now so their
  implementing issues (#220, #215, #219) land against a named contract
  instead of inventing five vocabularies. Until they exist, the rows simply
  gate nothing.
- **Dependencies between flags are stated, not enforced by the flags
  themselves.** C2's refusal power presupposes C1: with C1 `off`, no intent
  commits on an attestation shortcut, so C2 has nothing to deny — its shadow
  observation degrades to shortcut-incidence counting (clause d). C3, C4 and
  C5 consume adjudication verdicts and are independent of C1. Recommended
  promotion order and its reasons are clause (e)'s.

### (d) Shadow semantics per control

One table, then the four decisions the table cannot hold. "Recorded" always
means: would-be action, `RejectionCause` label where one exists, subject
account, cell-epoch handle or `RulesetId` where applicable, timestamp — the
dimensions clause (e) computes over.

| Control | Evaluated in shadow | Recorded | Suppressed |
|---|---|---|---|
| C1 attestation quorum | full D27 clause (d) predicate, **including D30's standing conjunct** | would-be `RejectionCause`; commit proceeds | the refusal |
| C2 quarantine validation | whether this quarantined session's intent would have been forced off the shortcut | shortcut-incidence per quarantined account | the forcing (session admitted via ordinary path) |
| C3 write refusal/annulment | refusal set (pending intents) + annulment set (journaled effects, inverse ops computed) | both sets, sized, per account/row | the refusal and the compensating writes |
| C4 authority-correction broadcast | the correction payload: leases revoked, state diff, recipients | payload digest, recipient count, byte size | the lease revocation and the broadcast |
| C5 strikes | the verdict→weight filing | the strike row, **written with `mode: shadow`** | the row's effect: never counted toward any threshold |

**C1 — the `AttestRow` question, decided.** A shadow-period commit writes
its `AttestRow` like any other, with one new field: `enforced: bool`. Shadow
commits write `enforced: false`; `required` commits write `enforced: true`;
`off` continues to write nothing (`NotApplicable`,
`intent/fdb.rs:847-850`). The alternative readings both fail: omitting the
row leaves shadow-period attested commits unauditable against D27 clause
(f), and writing it unmarked fabricates an audit trail that says the cluster
stood behind a quorum it deliberately waived. With the marker, an auditor
reads a coherent story: insufficient co-signatures, admitted by policy,
observed not trusted. Cost, priced: `AttestRow` is one handle (8 B), the
eligible vector (≤ `WITNESS_SET_TARGET_N = 7` NodeIds ≈ 231 B), and a
deadline (8 B) — ~250 B inside a transaction the intent already runs, added
to every shadow-period commit, against D16's 10 ms commit p99. Shadow is a
temporary posture paying a temporary tax.

**C1 — the executor's re-proof is mode-aware.** Under `shadow`,
`record_witness_epoch` adopts the durable draw key exactly as today (the
cache must converge regardless of mode, `intent/fdb.rs:899-901`), writes the
marked `AttestRow`, and **skips the required-subset re-proof** (`:902-919`).
That re-proof exists to stop a stale-key intent committing below quorum —
which shadow commits below quorum *on purpose*. Leaving it armed would make
shadow refuse at commit what it admitted at admission, violating clause (b)
from the far side. Under `required` and `off` the function behaves exactly
as today. Corollary: a deployment running C1 in anything but `off` must wire
the epoch authority into **both** the validator and the executor
(`recording_epochs`); the flag's implementation includes that wiring, which
is why the binary currently runs dark (Context §2).

**C2 — what shadow measures while C1 is off.** With no quorum active, no
intent anywhere commits on attestations, so "denied the shortcut" is vacuous
and C2-shadow honestly degrades to counting quarantined-session intents and
their outcomes. That is not nothing: it is the population that C2-live will
constrain, and its size is an input to [#205]'s threshold calibration. Once
C1 promotes, C2-shadow measures the real quantity — quarantined accounts
whose intents would have passed the quorum, i.e., the shortcut uses being
denied. C2 may not be promoted past its default before C1 does; there is
nothing to enforce behind it.

**C4 — shadow is pure computation, and that is a limitation, stated.** A
broadcast cannot be unsent, so C4-shadow builds the correction payload,
hashes it, counts recipients, and sends nothing — it can prove the payload
is well-formed and addressable, and nothing about how peers reconcile. The
reconciliation half is proven by [#222]'s enforcing-mode leg against harness
peers before any C4 promotion review, not by production shadow data. This is
the one control whose shadow period validates less than its live mode
exercises; the promotion review must say so rather than imply coverage.

**C5 — the strike shadow files rows, stamped.** Of the two candidate
shadows, "do not file at all" loses: docs/07:215 requires thresholds to be
"calibrated against the observed honest-population distribution", and an
empty ledger calibrates nothing — promotion day would arrive with no score
distribution to compare against. So shadow **files the row** with a mode
stamp, and the score sums only live-stamped weights:

```
S(t)      = Σ wᵢ · 2^(−Δtᵢ/14 d)        over rows with mode = live only
S_shadow  = Σ wᵢ · 2^(−Δtᵢ/14 d)        over all rows — reported, never enforced
```

Non-retroactivity is then exact rather than hoped-for. Decay alone would
mostly have done the job — a strike filed on day 0 of a 30-day shadow period
retains `2^(−30/14) ≈ 0.226` of its weight at promotion, `2^(−60/14) ≈ 0.051`
after 60 days — but 22.6% of a false strike is not zero of one, and "the
threshold moved" must never retroactively convict. The stamp costs one byte
of mode on a row family [#205] has not shaped yet; the reporter/subject
split survives intact because filing touches nobody — `EvidenceForged`
strikes the reporter in shadow exactly as it would live, in data, visible to
no one. While C5 is in shadow, **no standing ever changes**: witness
eligibility (D28 clause e), quarantine assignment, cooldown and ban are all
downstream of thresholds, and shadow rows do not cross thresholds by
construction. [#219]'s refusal mechanics activate only on C5's live rows.

### (e) Promotion evidence: the predicate

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

"Measurably negligible" resolves to **zero on the cohort**, and the coverage
term is what makes the zero mean something: a false-positive rate of 0 over a
cohort nobody watched is not evidence, it is blindness with a clean conscience.
[#221] reports both numbers; a rate without its denominator is not evidence,
which is the discipline P4's swarm leg already follows.

The terms, each made checkable on purpose:

- **H, the known-honest cohort**, has two halves. *Armed-honest*: operator-
  controlled accounts acting honestly under automation, the control P4's
  swarm harness already runs ("the conviction and armed-honest controls both
  clean"). *Natural*: accounts older than the 7-day probation
  ([docs/07:217](../07-witnessing.md)) with zero upheld adverse findings in
  the archive, sampled by a human into the cohort. Membership must be
  derivable from durable facts plus a recorded sample decision — never from
  "seemed fine".
- **Coverage ≥ 0.999** is the fraction of H's qualifying activity the shadow
  evaluation actually observed, P4's denominator discipline
  (0.9999992 there). A floor of three nines is set, not six, because
  attestation observations attach to every intent rather than to sampled
  dispute windows — hitting P4's figure is the expectation, clearing the
  floor is the gate. The floor is a dial with no derivation; it is stated so
  that lowering it later is a visible decision.
- **W ≥ 30 days** is two strike half-lives, so the calibration window
  observes decay behaviour rather than assuming it (clause d's arithmetic).
- **|H| ≥ 100** keeps the zero meaningful — `0/3` is not evidence. Dial,
  no derivation, same honesty rule D29 applied to `C = 8`.
- **The sensitivity leg** is [#222]'s gate leg for control C: a synthetic
  offender is refused by the enforcing process, committed by the shadow
  process, and observed with the matching cause label; and a control flipped
  back to shadow demonstrably stops acting. Production shadow data proves
  *specificity* (zero false positives); it cannot prove *sensitivity*,
  because production may contain no guilty traffic at all. Injected positives
  close that; a control that has never fired anywhere has not been shown to
  work.

**Recommended promotion order: C1 → C5 → C4 → C3, with C2 riding C1.**
Reasons: C1 is prerequisite infrastructure for C2's meaningful shadow and
gates nothing downstream; C5 before C4/C3 because verdict responses are
harmless while strikes count nothing, and C4/C3's evidence improves once
verdicts exist to respond to; C3 last because it moves durable value
backwards and is the one control clause (g) hard-gates on the auditor. The
order is a recommendation; the predicate is the requirement.

**The pre-live review gate** — #106's third acceptance line, made into an
artifact rather than an assertion. Each promotion ships a **dated promotion
note** in the promoting pull request, containing: the per-control row from
clause (c) restated as-flown; links to the [#221] report artifacts behind
each predicate term, with their coverage denominators; the [#222] gate-leg
report; a re-read of the relevant threat-model rows
([docs/07 §1–§3](../07-witnessing.md)) confirming the control addresses the
threat it claims; and, for C3, the auditor-liveness evidence of clause (g).
The reviewer is **the repository owner** — the acceptance authority
[DECISIONS.md](../DECISIONS.md) already establishes — and the implementing
agent's own summary is evidence, not review. No promotion merges without
that note; a later auditor reading the history can find, per control, who
accepted it and against which numbers.

### (f) Auto-suspend: the trigger, the fallback, the asymmetry

> **Each `persistd` process monitors its own shadow observations and live
> actions per control. When the trigger fires, the process writes
> `ramp/{control}` with `mode: shadow`, `source: autosuspend`, the incident
> id, and a reason — demoting the control fleet-wide within one poll
> interval. Auto-suspend may only demote. Returning a suspended control to
> live is always an operator act; no timer re-arms enforcement.**

The trigger, per control C (and per `RulesetId` v where the control is
verdict-driven — C3, C4, C5 — per [docs/07:237](../07-witnessing.md)'s "for
that rule version"; C1 and C2 are protocol-level and suspend globally):

```
suspend(C [, v]) ⟺ spread ≥ 8 distinct accounts
                  ∧ rate  ≥ max(10 × median₇d(C [, v]),  25 events/h)
   over a sliding 60-minute window
   spread  = distinct accounts with a would-have-acted (or acted) event
   rate    = events per hour, same window, same scope
   median₇d= trailing 7-day hourly median of the same counter
```

Each term earns its place:

- **Account spread before event volume.** A ruleset bug strikes everyone
  equally; an attacker concentrates. docs/07:237 says "across unrelated
  accounts", so the counter is cardinality, and one account flooding the
  path cannot trip it — floods are what per-account rate limits are for
  ([docs/07:236](../07-witnessing.md)).
- **Rate against the control's own baseline**, floored at an absolute
  minimum, because 10× a quiet Tuesday's median of zero is zero. The floor
  (25 events/h) and the spread bound (8) are dials with no derivation, in
  D29's `C = 8` tradition: they are set low enough that a genuine bug trips
  them within minutes and high enough that a quiet period's noise does not.
- **RTT/loss correlation is a required dimension, not a nice-to-have.** R-6
  names "discrepancy-report rate correlating with peer RTT/loss rather than
  accounts" as the early warning, so [#221] dimensions the counters by
  network-quality bucket and the monitor treats an RTT-correlated spike as a
  first-class trip reason — that shape is packet loss wearing a cheat
  costume, exactly the false positive this machinery exists to prevent.

**Fallback is shadow, never off, and never a promotion.** Falling to shadow
keeps observing — the incident itself is calibration data, and the alternative
(blindness during the exact period something went wrong) throws away the
evidence that explains it. Falling to off would also make auto-suspend a
denial-of-service lever against enforcement itself: induce spikes, blind the
cluster. The asymmetry is the point — automation may make the fleet safer
without asking, never less safe. The trip writes an incident id and reason;
the operator's return-to-live is an `Operator`-source posture write that
clears it, and clause (e)'s review gate governs the re-promotion like any
first promotion.

**Blast radius is global-per-control (or per-rule-version) and that is
accepted.** A single noisy gateway can demote a control for every gateway.
Falling to shadow is safe and observable, the incident trail explains the
trip, and per-gateway scoping would trade one noisy box for inconsistent
enforcement across siblings — D26's handover assumes peers judge alike.
Cross-process aggregation is deliberately deferred (open question 4).

### (g) The economy-wide invariant auditor gates the last control, not the first

> **"Enforcement fully on" is defined as C3 (write refusal/annulment on
> guilty verdicts) reaching live. The economy-wide invariant auditor ([#224])
> must be live before C3's promotion review may conclude — live meaning
> deployed, sweeping on its start cadence (daily full conservation sweep,
> hourly incremental over hot ledgers), and emitting findings into the audit
> pipeline. No other control waits for it.**

The roadmap sentence ("must be live before enforcement is fully on",
`11-roadmap.md:917`) is made checkable by defining the thing it quantifies
over. The GTA Online failure is post-hoc-only *correction*: value created by
dupes circulated economy-wide because nothing looked until complaints
arrived. Mapping that onto the inventory decides the scope of the gate:

- **C1 is pre-hoc.** A quorum refusal stops the bad commit; there is nothing
  for an economy-wide sweep to find later. Gating C1 on the auditor would
  delay the control that *prevents* the GTA failure for no safety gained.
- **C2 and C5 move no durable value.** Quarantine strictness and standing
  changes act on identities and validation paths; a wrong call there is a
  false-positive problem, governed by clause (e), not a conservation
  problem.
- **C4 reconciles session state to a verdict.** Wrong verdicts propagate and
  are wrong visibly; rollback repairs them locally; no ledger row moves.
- **C3 is the post-hoc control.** Annulment reaches backwards into journaled
  history, and its trigger is replay evidence only. The leaks the per-intent
  checks structurally cannot see — individually-valid steps composing a slow
  drain ([#224]) — are invisible to every other control in this record. Live
  annulment without a conservation sweep is the documented failure, verbatim.

Dependency wiring, both directions as [#224] asks: this record's C3 clause
depends on [#224]; [#224]'s gating question is answered here (yes, for C3
only) and its issue should link back on acceptance. The auditor's cadence,
ownership (P5 exit vs the P6 archive-tailer machinery) and time-to-detection
target remain [#224]'s to settle — clause (g) consumes "live and sweeping",
nothing more.

### (h) D29's annulment-on-expiry default: live from day one, and not a ramped control

> **Deadline expiry annuls, from the day the provisional path deploys, with
> no flag and no shadow arm. Annulment-on-expiry is not an enforcement
> control; it is the fail-closed half of D29 clause 9 and belongs beside the
> always-on set of clause (b). D17.3 does not reach it.**

D29's open question (`0029:775-780`) asked whether expiry should be
"shadow-mode-only during the enforcement ramp", noting annulment destroys
value without a strike. Three reasons close it the other way:

1. **What it punishes is nobody.** Expiry annuls as `Unadjudicable` and
   strikes no account (D29 clause 9a). D17.3's requirement is about the
   *strike pipeline* launching telemetry-only — a requirement this record
   honours through C5. A mechanism that convicts nobody has no false-positive
   cohort to protect, which is the whole content of a shadow period.
2. **Shadowing it converts bounded incidents into permanent ones.** The
   deadline is what bounds value-at-risk and what makes "outlast the replay
   queue" lose (D29 clause 9a-b). Hold-expired-instead-of-annulled means: a
   finalizer outage stops annulling, every affected account wedges at its
   outstanding cap (`PROVISIONAL_OUTSTANDING_CAP = 8`,
   `crates/orrery_protocol/src/persist.rs:655`) *forever* — further
   low-population intents refused indefinitely rather than for the 5-minute
   deadline (`persist.rs:637`) an incident lasts — and unfinalized rows
   accumulate without bound, since the sweep collects only
   `finality ∈ {Final, Annulled}` rows (D29 clause 9c). An incident becomes
   a permanent tax on exactly the players in empty cells.
3. **The punitive part of the provisional path is already ramped.** A spot
   replay that finds fabrication annuls *and strikes*; the annulment is D29
   machinery (always-on), the strike is C5 (shadow until promoted). During
   the ramp an honest player's exposure is therefore already protected where
   protection is owed — against false *convictions* — while the economy
   stays fenced by the fail-closed path.

One correction to D29's wording while closing it: the open question says
"the flag is named" — no such flag exists. Grepping `crates/orrery_persistd`
for annul/expiry/shadow surfaces comments, metrics (`intent_provisional_
annulled`, `intent_annulled_replays`, `bin/persistd.rs:547-549`) and tests;
there is no config field, CLI argument or posture row for expiry behaviour
anywhere in the tree. The sentence was prospective; this record resolves the
question by declining to create the flag. Expiry remains instrumented and
alarms as an incident per D29 clause 9(b) — a nonzero annulment rate is
either an attack or a ruleset bug, and both page somebody.

## Consequences

- **Five CLI arguments and no new keyspace family land in `persistd`.** The
  rows are `ramp/{control}` at `b"vr" ‖ control-name`, one row per control,
  absent-by-default, inside the already-registered `v` family. The
  disjointness test (`keyspace.rs:2777`) needs no new row; the implementing
  change adds the `v` sub-span assertion clause (c) requires, in the style
  of the `d`/`l` sub-discriminator tests.
- **The one-byte family budget is spent, and this record says so on the
  way past.** Accepted [ADR-0031] holds `y` for `strike/` and `z` for
  `jarchive/`; nothing clean remains. Clause (c)'s allocation rule is
  normative once this record is accepted: sub-discriminate inside a matching
  family, or write the ADR that opens the multi-byte space — and always
  check the accepted record set, not just `keyspace.rs`.
- **The binary stops being unable to enforce.** Wiring the flags replaces
  `persistd.rs:2109`'s hardcoded constructor and adds the `recording_epochs`
  call the executor never receives. After this record's implementation
  issues land, a deployment can observe (shadow) without recompiling — which
  is the first moment D17.3 is satisfiable in the field rather than in
  tests.
- **`AttestRow` gains a field, a durable value-shape change.** `enforced:
  bool` is **not** additive in the tolerant sense: postcard encodes
  **positionally**, and `from_bytes` errors on trailing bytes, so a reader
  built before the field fails outright on a row written after it — it does
  not decode-and-drop. An earlier revision of this bullet said the opposite;
  [#217] found it while implementing, and the corrected reasoning is what
  makes the change affordable.

  What makes it affordable is **retention, not compatibility**: `AttestRow`
  is swept with the intent row at `INTENT_ROW_RETENTION_MS` — one hour
  (`crates/orrery_persistd/src/intent/fdb.rs:86`, applied at `:927`) — so the
  mixed-shape window is bounded by that sweep rather than by reader
  tolerance. A deployment must therefore not straddle the change for longer
  than the retention horizon, and audit tooling must be updated in the same
  change or shadow-period audits misread rows written by the other side.
- **`strike/` gains a mandatory mode stamp before its first row is written.**
  [#205] shapes the family; this record constrains it: no strike row may
  exist without a mode, and the scorer filters on it. Retrofitting a mode
  onto a live ledger is exactly the retroactivity clause (d) refuses.
- **Auto-suspend is implementable the day telemetry exists.** The trigger
  consumes only per-control, per-cause, per-account-cardinality counters —
  [#221]'s deliverable, dimensioned by `RejectionCause::label` and
  `RulesetId`. No new metrics vocabulary is created by this record.
- **Two things remain operator-secret for now.** How an operator
  authenticates posture writes (any writer of the FDB row commands the
  fleet's enforcement posture), and how the posture row is retained —
  incident history wants the rows kept, the GC wants them swept. Both are
  D12 ops work, listed as open questions rather than assumed.
- **Nothing here enables anything.** Every default preserves current
  behaviour: C1 off, C2 live-as-landed, C3/C4/C5 nonexistent. Enabling
  requires an operator act, evidence under clause (e), and — for C3 — an
  auditor that does not exist yet.
- **AGENTS.md's decision-table row is owed by whoever holds that lane** —
  this record's working constraints put `AGENTS.md` outside its editable
  set, so the index update lives in [DECISIONS.md](../DECISIONS.md) only.

## Alternatives considered

- **CLI-only flags, no durable posture.** Simpler, no new family, no
  authentication question. Rejected: a redeploy is minutes and drops
  sessions; auto-suspend would have to restart fleets to demote a control,
  which turns "contain the blast radius" into "cause a bigger one".
- **A family byte of `ramp/`'s own — `y`, as the merged first draft said.**
  Rejected on [#248]: accepted [ADR-0031] resolved question 4 allocates `y`
  to `strike/`, and the draft's justification ("`'y'` appears nowhere in
  `keyspace.rs`") consulted the code while `strike/` is
  documented-but-unimplemented — exactly the guard-blindness [#226]
  describes from the other direction.
- **Take `z` instead.** The same mistake one byte later: [ADR-0031]'s
  accepted arithmetic closes only because `z` goes to `jarchive/`. Spending
  it here re-opens an accepted record's budget to shelve five rows.
- **Open the two-byte family space now.** The structural fix, and costed
  rather than dismissed: it touches every key builder in `keyspace.rs`, the
  disjointness guard's one-byte model, and — for any family it re-homes —
  the on-disk key format, the least reversible change class in the system
  ([#226]). Buying that to store at most five absent-by-default posture rows
  is backwards. The mechanism stays available to a future family that
  genuinely needs a range of its own, through the dedicated ADR clause (c)'s
  allocation rule names.
- **Keep posture out of FoundationDB entirely** — a config file, an
  environment variable, an ops-plane push. Rejected for the same reason
  CLI-only flags are: auto-suspend is a durable, fleet-visible write made by
  a `persistd` process itself, within seconds, surviving restarts. There is
  no other shared durable store — the coordinator holds none by design
  ([ADR-0031] Context) — so anything outside FDB reinvents replication for
  five rows.
- **Hot-path posture reads.** Always-current mode, no staleness. Rejected:
  puts an FDB round trip inside the admission path that #147's acceptance
  and D16's 10 ms p99 both forbid; the 1 s poll buys the same semantics for
  nothing per intent.
- **One global enforcement flag instead of five.** The roadmap says "each
  independently feature-flagged" (`11-roadmap.md:867`) and the controls fail
  differently: a quorum false-positive is an attestation-calibration
  problem, a strike false-positive is a thresholds problem, and suspending
  both because one misbehaves trades real protection for convenience.
- **Shadow files no strike rows at all.** Cheaper, no mode stamp, no privacy
  question. Rejected under clause (d): thresholds must be calibrated against
  an observed distribution (docs/07:215), and an empty ledger is not an
  observation.
- **Auto-suspend falls to `off`.** Strictly safer-looking. Rejected: it
  blinds the cluster during the incident, discards the calibration data the
  post-mortem needs, and makes the trigger a censorship lever — spike the
  deviation rate, turn enforcement off. Shadow keeps watching; only the
  operator may go darker than that.
- **Auto-re-arm after a quiet hour.** Tempting for unattended fleets.
  Rejected: the asymmetry (automation demotes, humans promote) is what makes
  an unattended trip safe. A control that returns itself to enforcement has
  re-entered clause (e)'s jurisdiction without clause (e)'s evidence.
- **Gate every control on the auditor, not just C3.** Maximally cautious
  reading of `11-roadmap.md:917`. Rejected: the GTA failure is specifically
  post-hoc *value* correction; gating pre-hoc admission (C1) on a post-hoc
  auditor delays the anti-GTA control itself, and the roadmap's own P5-exit
  deadline shows the intent was sequencing, not paralysis.
- **Make expiry-annulment shadowable anyway, default shadow.** The literal
  reading of D29's question. Rejected under clause (h): it creates the
  permanent-wedge failure mode, protects a cohort (struck accounts) that the
  mechanism never touches, and adds a flag whose only two positions are
  "fail closed" and "accumulate liability".
- **Resolve the D29↔#105 pointer contradiction by editing D29's open
  question in place.** Rejected: accepted records are amended by new records
  that name them, never quietly retouched (AGENTS.md ground rules). The
  bracketed erratum travels with this record's acceptance and changes no
  ruling.

## Open questions

1. **The posture row's writer authentication.** Anything that can write
   `ramp/strikes` can silence enforcement fleet-wide, so the row's trust
   level is "coordinator-key-equivalent" at least. Whether posture writes
   are direct FDB access by an ops tool (D12's operator plane) or a signed
   envelope verified by `persistd` is deployment work this record does not
   guess at.
2. **Posture-row retention.** Incident history argues for keeping superseded
   rows (who suspended what, when, why); the checkpoint pass sweeps
   everything with a deadline. Keeping an append-only shadow of posture
   changes in the journal archive (D11's event source) is the likely answer
   and is [#221]-adjacent tooling, not keyspace design.
3. **Whether C2's `off` arm should exist at all.** It is specified for
   symmetry, but its only use is responding to a mass-quarantine bug by
   treating quarantined sessions as `Good` on the intent path — and the same
   incident also makes those accounts witness-eligible again only if the
   *token* standing changes, which is identity's lever, not the gateway's.
   If [#205]/[#219] conclude partial demotion is incoherent, dropping the
   arm narrows the enum; the compiler will find every site either way.
4. **Cross-process aggregation for the auto-suspend trigger.** Local
   detection with global effect is simple and conservative, but a fleet-wide
   median computed by the coordinator (or the ops plane) would see spikes
   any single gateway cannot. Defer until [#221]'s per-gateway numbers show
   whether local windows are too noisy to be useful.
5. **Whether C4's shadow period should send corrections to opted-in
   internal testers.** It would validate reconciliation under production
   conditions years before public launch; it also means shadow acts on
   someone. Left to the C4 promotion review, with the note that clause (d)
   already routes that proof through [#222]'s harness leg instead.

[#106]: https://github.com/baadc0de/orrery/issues/106
[#105]: https://github.com/baadc0de/orrery/issues/105
[#205]: https://github.com/baadc0de/orrery/issues/205
[#215]: https://github.com/baadc0de/orrery/issues/215
[#217]: https://github.com/baadc0de/orrery/issues/217
[#219]: https://github.com/baadc0de/orrery/issues/219
[#220]: https://github.com/baadc0de/orrery/issues/220
[#221]: https://github.com/baadc0de/orrery/issues/221
[#222]: https://github.com/baadc0de/orrery/issues/222
[#224]: https://github.com/baadc0de/orrery/issues/224
[#226]: https://github.com/baadc0de/orrery/issues/226
[#248]: https://github.com/baadc0de/orrery/issues/248
[ADR-0031]: 0031-id-account-subspace.md
