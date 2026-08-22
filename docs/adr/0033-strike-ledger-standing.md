# ADR-0033: Strike ledger, exponential decay, and the quarantine → cooldown → ban standing machine

**Status:** Proposed · **Date:** 2026-08-21 · **Decision:** D33

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It makes concrete D10 item 5's account-attached,
14-day-decaying strikes and its quarantine → cooldown → ban sequence. It
depends on [D31](0031-id-account-subspace.md): an upheld verdict names an
account, and identity alone writes `id/`. It supplies the numbers to which
[D32](0032-enforcement-ramp.md)'s C5 ramp refers; it does **not** choose when
those numbers begin acting.

## Context

D10 decides the direction but deliberately leaves its thresholds open:
strikes attach to accounts, not rotating NodeIds; `Unadjudicable` is never a
strike; and the half-life is 14 days. D16 carries that half-life and no strike
threshold, duration, or retention row. `docs/07` contains a useful but
non-normative expansion-table default (`3 / 6 / 10`); it cannot settle a
missing D16 parameter, and its arithmetic has a practical cliff: two 3-point
findings reach 6 only when simultaneous.

The writer matters before the key byte does. D12 assigns identity the
reputation ledger and bans as a service responsibility, while `docs/08`
describes an adjudication-executor-written `strike/{account}/{versionstamp}`
row read by identity. These are compatible only if service ownership means
identity owns the *standing decision and token*, while the executor owns the
append-only adjudication fact. This record chooses that split.

D31's accepted resolved question 4 assigns `strike/` family byte `y`, with
sub-discriminator `ya`, precisely on the condition that the executor, not
identity, writes the strike rows. Its keyspace inspection is current:
`keyspace.rs` registers no `y` or `z` family, while `d` is the identity-only
family. Proposed D32 currently says `ramp/` uses `y`; that is a conflict
between proposals, not an accepted contradiction. This record follows D31 and
PR #225: D32 must move `ramp/` before either proposal is accepted.

## Decision

### (a) The adjudication executor writes immutable strike facts; identity writes standing

> **The adjudication executor is the sole writer of `strike/`; identity is its
> sole online scorer and the sole writer of the derived account standing and
> token. A verdict becomes one immutable strike fact in the executor's commit
> transaction. Identity never invents, edits, or deletes a strike.**

This preserves D31 clause (d)'s single writer for `d`: the executor never
writes the account row or a binding index, so `da`/`db` retain their required
atomicity. It also preserves D12's ownership boundary: identity decides who is
issued a token, who is quarantined, and whether an appeal removes a ban; the
executor merely records the self-verifying fact it adjudicated. The reversal
condition in D31 therefore does **not** obtain: identity is not the strike
writer, so `y`, not `d`, remains correct.

Only the D10/D29 mapping files a row:

| Adjudicated outcome | Subject | Weight |
|---|---|---:|
| `Deviation` / confirmed replay violation | disputed account | 3.0 |
| false attestation | attesting account | 3.0 |
| `EvidenceForged` | reporter (or D29 provisional submitter) | 3.0 |
| non-cooperation / log gap after the existing proof threshold | responsible account | 1.0 |
| reviewed timing-pattern finding | responsible account | 0.5 |
| `Unadjudicable` | nobody | 0 |

The last three weights retain the explicit D10 §5 evidence-quality ordering.
A transport-signature failure that cannot establish a named reporter remains
`Unadjudicable`, as the current adjudicator already documents; it cannot be
turned into an anonymous strike by this ledger.

### (b) `ya` is one append-only account ledger, retained for 90 days

> **A strike key is `ya || account_id:u64-be || versionstamp:[u8;10]`; its
> value is `StrikeRow { issued_at_ms, weight_milli, kind, evidence_ref,
> ruleset, mode, expires_at_ms }`. `mode` is `shadow` or `live`; the scorer
> counts only `live`. The executor writes with FoundationDB's
> versionstamped-key primitive, so an account's facts are commit-ordered,
> never clock-ordered. Every row expires hard at `issued_at_ms + 90 days`.**

`weight_milli` stores 3000, 1000, or 500, not a float; an appeal's
compensating fact uses the corresponding negative integer. `evidence_ref` is
the durable evidence/archive handle plus its digest; `ruleset` makes an
auto-suspend or appeal traceable to the replayed rule version. `issued_at_ms`
is evidence and drives expiry; the versionstamp is the ordering key. A sweep
may read the account-contiguous `ya` ranges and delete values whose carried
deadline has passed; it is an off-path maintenance operation, never a login or
intent-path scan.

Ninety days matches D31's binding-history horizon rather than creating a
second dispute horizon:

```
remaining(90 d) = 2^(-90 / 14) = 0.01160... ≈ 1.2 %.
```

Thus deletion loses at most 0.0348 points from a 3-point fact, far below the
0.5-point smallest live weight and the proposed 3-point quarantine threshold.
The retained evidence/archive, not an indefinitely retained score row, is the
appeal source.

The cost is bounded and visible. The key is 20 B. A conservative encoded
value budget is 104 B (timestamps/deadline 16, weight/kind/mode 4, evidence
handle+digest 48, ruleset handle 32, encoding slack 4), or **124 B logical per
row before FoundationDB replication/overhead**. At 10^7 accounts:

```
3 retained rows/account × 10^7 accounts × 124 B = 3.72 GB logical
8 retained rows/account × 10^7 accounts × 124 B = 9.92 GB logical.
```

The first is the sizing target, not an assertion that every account is
convicted. The second is the explicit operational alarm point: a 90-day
average above eight filed rows per account is either a ruleset incident or an
attack distribution and pages the C5 operator. It is deliberately not a
silent filing cap: dropping proved violations to save storage would make the
ledger cease to be an audit record.

### (c) Decay is evaluated at read time, continuously in wall-clock time

> **Identity computes score when it needs a standing, by summing the retained
> live ledger rows at its read instant; no periodic decay write and no
> timer-driven score sweep exists.**

For `t` and each live row `i` issued at `t_i`, with weight `w_i` in points:

```
S(t) = Σ_i w_i · 2^(-(t - t_i) / (14 days)),       t ≥ t_i
```

The implementation uses fixed-point milli-points and a conservative
round-up at a threshold comparison; it must not make a player safer because a
platform rounded a decay factor down differently. The stored row is never
mutated merely because time passed. This makes the result independent of sweep
cadence and means an outage cannot freeze a score high or low.

Useful anchors for a 3.0-point finding are:

```
age       0 d       7 d       14 d      28 d      90 d
weight    3.000     2.121     1.500     0.750     0.0348
```

D32 owns whether `mode = shadow` facts are collected and when C5 is promoted.
This record consumes its required contract exactly:

```
S_live(t)   = Σ_{mode = live}   w_i · 2^(-age_i / 14 d)
S_shadow(t) = Σ_{all rows}      w_i · 2^(-age_i / 14 d)  // telemetry only
```

No shadow fact changes standing, even after promotion. That is a ledger rule,
not a second shadow policy.

### (d) Proposed standing thresholds: one proof quarantines; a recent pattern escalates

> **`Q`, `C`, `B`, the minimum cooldown and the probation window are
> deployment configuration, not constants of this record. The defaults are
> `Q = 3.0`, `C = 5.0`, `B = 7.0`, a 14-day minimum cooldown and a 7-day
> probation. A deployment may set others; it may not set incoherent ones, so
> the four invariants below are validated at startup and a violation refuses
> to start rather than warning.**

Decided by the repo owner on 2026-08-22: thresholds are configurable with this
package as the default, rather than a value fixed by the record. The reasoning
is that these are policy dials — this record says so itself, two paragraphs
down — and a dial whose only value is written into a normative ADR forces an
ADR amendment to retune a number that P4's calibration campaign (#240) exists
to inform. The record therefore owns the *shape* and the *invariants*; the
deployment owns the *values*.

What configuration may not do, checked at startup:

```
(i)   Q <= w_max                 a single proved major violation must quarantine
(ii)  Q < C < B                  strictly ordered, or a state is unreachable
(iii) B <= n_intended * w_max    ban must be reachable by the number of major
                                 findings the operator intends, given decay
(iv)  cooldown_min > 0           a cooldown that can be left instantly is not one
```

`w_max` is 3.0 under clause (a)'s weight table. Invariant (iii) is not
theoretical: the `3 / 6 / 10` package discussed below fails it for
`n_intended = 3`, because three 3-point facts sum to 9 and never reach 10 —
so it silently means "three major findings are insufficient" rather than "a
higher bar". A configuration that cannot reach its own terminal state is the
kind of error a startup check should catch, not an operator should discover
from an absence of bans.

The recommended package is arithmetic, not a claim that the values were
measured:

| Boundary | Proposed score | What it means |
|---|---:|---|
| quarantine `Q` | 3.0 | one proved major violation immediately loses shortcut and witness privilege |
| cooldown `C` | 5.0 | two major violations within 8.19 days suspend durable participation |
| ban `B` | 7.0 | three major violations within 22.19 days require human review to return |

For two 3-point facts separated by `d`, escalation reaches cooldown when:

```
3 + 3·2^(-d/14) ≥ 5
2^(-d/14) ≥ 2/3
d ≤ 14·log2(3/2) = 8.19 days.
```

For three 3-point facts, two now and one `d` days earlier, ban is reached
when `6 + 3·2^(-d/14) ≥ 7`, hence `d ≤ 14·log2(3) = 22.19 days`. The spacing
is intentional: a single proof is enough to protect other players; repeated
recent proof is what removes the account from durable play; a pattern, not
one old incident, is what reaches a terminal standing.

The alternative inherited from `docs/07` is `Q/C/B = 3/6/10`. It is more
forgiving but has two cliffs: two 3-point facts only equal 6 at exactly the
same instant, and even three equal 9, never ban. It therefore means “three
major findings are insufficient absent a fourth or lesser facts,” not merely
“a higher threshold.” **`3/5/7` is the default** for exactly that reason: it
supplies a two-recent-proof and three-recent-proof machine while retaining the
14-day half-life as the forgiveness mechanism, and it satisfies invariant (iii)
where `3/6/10` does not. These are policy dials with no fabricated empirical
provenance — the values that replace them should come from P4's calibration
campaign (#240), which is what measuring a real false-positive rate is for.

### (e) State transitions, reversals, and appeals

> **Standing is the maximum applicable state, evaluated after every live
> filing and whenever identity mints or refreshes a token: `Good` when
> `S < Q`; `Quarantined` when `Q ≤ S < C`; `Cooldown` when `C ≤ S < B`; and
> `Banned` when `S ≥ B`. Escalation is immediate. Quarantine reverses when
> read-time decay makes `S < Q`; cooldown reverses only after both `S < Q` and
> fourteen consecutive days since its most recent entry; ban never reverses by
> decay and is appealable only by human review of retained evidence.**

Cooldown's two conditions are not duplicate timers. At entry score 5.0, decay
needs `14·log2(5/3) = 10.32 d` to fall below 3; the 14-day floor therefore
prevents a precisely timed low-weight sequence from clearing on the first
possible read and provides a full half-life of observation. A new live strike
re-enters/restarts cooldown if the resulting score remains at least 5.0.

An upheld appeal appends an executor-authorized compensating fact referencing
the appealed evidence, with the original weight negated for scoring; it never
rewrites history. Identity then clears the derived ban/cooldown state only
when recomputation says it may. A rejected appeal changes nothing. A ban is
therefore terminal against time, not against demonstrated error.

Effects are deliberately narrower than new wire states:

| State | Token / session result | Enforcement effect |
|---|---|---|
| Good | `SessionStanding::Good` | ordinary path |
| Quarantined | existing `SessionStanding::Quarantined` | no witness eligibility; D10 full validation |
| Cooldown | identity refuses a session token | may remain in a non-durable guest experience only if the game offers one |
| Banned | identity refuses a session token | no connection; appeal endpoint only |

`SessionStanding` therefore stays two-valued and token V1 is unchanged.
Cooldown and ban are admission decisions, not claims a connected peer must
interpret. On a change to cooldown or ban, identity publishes an account
generation invalidation and gateways terminate matching sessions; the target
is one posture poll plus apply (≤2 s, D32's existing fleet bound). A
quarantine takes effect no later than token refresh; if the client ignores its
half-TTL refresh, V1's existing one-hour maximum token TTL bounds the lag.

### (f) Reading is off the intent hot path; a standing miss fails closed

> **The gateway and coordinator read only identity's signed token standing on
> their hot paths; they never query `ya`. Identity point/range-reads the
> account's retained `ya` span when issuing or refreshing a token, computes
> `S(t)`, and caches the resulting standing only until the next strike
> invalidation or token refresh. A missing or unreadable ledger is never
> interpreted as `Good`: identity refuses to mint or refresh the token.**

This is the same direction as D31 clause (f), with a stricter consequence.
For D31, an unresolved NodeId is excluded from a witness set and the intent
can take D29's provisional path. Here an unresolved standing could turn a
banned account into `Good`; quarantining it would still turn a cooldown/ban
miss into an admission. The safe answer is no new signed assertion. Existing
tokens remain bounded by their signed V1 TTL, and the invalidation path
normally makes the bound seconds rather than an hour.

There are two non-equivalent absences and neither is silently forgiven:

```
no `da` account row                 => authentication fails; no token
`da` exists, but `ya` read fails    => standing is unknown; no token refresh
empty successfully read `ya` span   => S = 0; Good
```

This leaves D28's “score below the witness-eligibility threshold” row
approximated: a score of 0.5 or 1.0 is intentionally still `Good` on the
existing two-arm token. Sending a continuous or bucketed score would be a new
signed wire contract and expands the false-positive surface; it is not needed
for the standing machine. The 7-day probation remains an identity admission
rule, not a new token field, until the D31/D28 account-age wire decision is
made.

### (g) Proposed D16 amendment

On acceptance, D16 gains these rows; it does not gain a second half-life:

| Parameter | Default | Parameter | Default |
|---|---|---|---|
| Strike half-life | 14 days (existing) | Strike-ledger retention | 90 days |
| Quarantine score `Q` | 3.0 | Cooldown score `C` | 5.0 |
| Ban score `B` | 7.0 | Minimum cooldown | 14 days |
| Account probation | 7 days | Strike storage alarm | 8 retained rows/account/90 d |

## Consequences

- `y` remains available for the executor-written strike ledger exactly as D31
  reserved it; `d` remains identity-only. The future keyspace test must add
  `y` as a registered family and prove `[y, z)` is disjoint.
- A 90-day ledger has a finite, priced retention horizon consistent with
  D31's 1.2%-remaining binding-history rationale. Expiry changes a score by
  less than the smallest live fact at its boundary.
- A new FDB read does not enter the intent p99. Login/refresh pays a bounded
  account-range scan; D12's stated ~33 auth/s at 10k CCU is the relevant path,
  not D16's <10 ms intent commit budget.
- D32 C5 still owns `off`/`shadow`/`live`, telemetry, promotion evidence, and
  auto-suspend. This record owns the meaning of a live fact and every standing
  transition downstream of it. Neither record silently enables C5.
- The exact `ramp/` family byte in proposed D32 must be reconciled before
  acceptance. It cannot also be `y`.

## Alternatives considered

- **Identity writes `ya`.** Rejected. It would make `d` viable under D31's
  stated reversal condition, but it adds a verdict-delivery protocol and lets
  identity recast adjudication facts. The executor already has the verdict and
  `docs/08` already gives it the append direction.
- **One materialized, periodically decayed score.** Rejected. It makes score
  a function of sweep uptime and creates write churn proportional to every
  struck account instead of every verdict. Read-time exponential decay is the
  exact function D10 specifies.
- **Keep `3 / 6 / 10`.** Presented for owner selection, not silently adopted.
  Its threshold arithmetic means common two- and three-major-verdict patterns
  do not escalate; `3 / 5 / 7` is recommended for that reason.
- **Automatic decay unbans.** Rejected. A ban is the point at which a pattern
  merits review; time cannot validate evidence or undo a malicious sequence.
- **Fail open or issue a `Good` token on a ledger miss.** Rejected. The party
  able to make a lookup unavailable would select the branch that admits a ban.
  That is D31(f)'s attacker-controlled-unknown problem with a worse outcome.

## Owner decisions requested

1. **Threshold package:** accept recommended `3 / 5 / 7`, or choose the
   conservative legacy-shaped `3 / 6 / 10`. Recommendation: `3 / 5 / 7` for
   the explicit 8.19-day and 22.19-day escalation windows above.
2. **Guest experience during cooldown:** this record only permits it as a
   game-level, non-durable mode. Whether any game exposes one is product
   policy; token refusal and durable-write denial do not depend on that choice.

[#205]: https://github.com/baadc0de/orrery/issues/205
[D10]: 0010-witnessing.md
[D12]: 0012-backend-services.md
[D16]: 0016-parameter-reference.md
[D31]: 0031-id-account-subspace.md
[D32]: 0032-enforcement-ramp.md
