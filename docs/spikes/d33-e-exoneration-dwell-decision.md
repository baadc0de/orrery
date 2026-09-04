# Decision memo — D33 clause (e): does exoneration end a standing cooldown's dwell early?

**Propose-only. Nothing is decided and nothing is amended.** This is the
artifact for [#1013](https://github.com/baadc0de/orrery/issues/1013), an
owner decision split out of #862 and left standing. It exists so the owner is
not asked for a verdict on ergonomics with nothing in front of them: every
claim about current behaviour below is cited to a line read on this tree
(`origin/main` at `cf4b431`, 2026-09-04), the numbers are the configured
ones, and the attack and fairness cases are worked rather than asserted.
Amending an Accepted ADR is owner-reserved
([DECISIONS.md](../DECISIONS.md)); §7 drafts the clause (e) text each option
would need, and §8 gives a recommendation. Until the owner picks one, D33 is
exactly what it was and `uphold_appeal` keeps having no caller.

Documentation-only lane: `check.sh` is exempt per
[AGENTS.md](../../AGENTS.md) ("a documentation chore: prose, a plan, an ADR
update"); no Rust changes here. The issue cites `adjudication.rs:782` and
`cooldown.rs:95/:104`; on this tree `uphold_appeal` is at
`adjudication.rs:805` (the file grew) and `cooldown.rs` lives in
`orrery_identity`, not `orrery_persistd`. This memo re-cites at today's
numbers.

---

## 1. The question

`AdjudicationExecutor::uphold_appeal`
(`crates/orrery_persistd/src/adjudication.rs:805-835`) appends D33's
compensating `Appeal` row. Score recomputation then drops the account's live
score; if it drops below `C`, the *score* no longer says cooldown. But #884
gave `cooldown_min_ms` a durable dwell (`crates/orrery_identity/src/cooldown.rs:95`)
precisely so that a score falling below `C` does **not** release an account
until the full dwell has run from the durable `dc` entry. Decay was the case
in hand; exoneration was not considered. The question:

> After an upheld appeal makes `S_live < C`, is the account released at its
> next observation, or does the `cooldown_min_ms` floor still run from
> `entered_at_ms`?

Two readings, from the issue:

- **Dwell is a consequence of the strike.** Remove the strike, remove the
  dwell. Otherwise an exonerated account serves a penalty for something that
  did not happen.
- **Dwell is a cooling-off period, not a penalty.** Exoneration does not
  obviously shorten it, and early release is a lever an attacker might aim
  at.

D33 clauses (d)–(f) do not settle it (§2.4).

## 2. What the mechanism does today, exactly

### 2.1 The dwell rule

Every real mint goes through `CooldownStanding` / `ComputedStanding::standing`
(`crates/orrery_identity/src/bin/orrery-identity.rs:52-60` builds it;
`cooldown.rs:41-48` and `:124-131` are the two `StandingSource` impls), and
both call `apply_dwell` (`cooldown.rs:55-111`):

```
observation = scorer.observe(account)               // read-only: rows -> S_live, level, newest_live_strike_ms
if level in {Cooldown, Banned}:                      // cooldown.rs:64-80
    store.observe_cooldown(account, now, newest_live_strike_ms)   // create or restart the dc row
    return Err(Cooldown | Banned)
entry = store.cooldown_entry(account)               // cooldown.rs:84-90
if entry is None: return level                       // never entered: no dc row, no dwell
if now - entry.entered_at_ms < cooldown_min_ms:      // cooldown.rs:95-97  <- the floor
    return Err(Cooldown)
if !store.clear_cooldown_if(account, entry):         // cooldown.rs:102-104 compare-and-clear
    return Err(Cooldown)                             //   a concurrent restart wins
return level                                         // Good or Quarantined
```

Three properties follow directly from the code:

1. **The floor is blind to why the score fell.** `apply_dwell` sees only
   `now`, `entered_at_ms` and `level` (`cooldown.rs:95`). Decay and
   exoneration are indistinguishable at that line. So **today, with no code
   change, an upheld appeal does not shorten a standing dwell**: the account
   is refused until `entered_at_ms + cooldown_min_ms`, then released at its
   next mint. The issue's question is therefore "should this change", not
   "which of two existing behaviours is right".
2. **Restart is strike-driven and appeal-immune.** `observe_cooldown`
   restarts `entered_at_ms` only when `newest_live_strike_ms > entered_at_ms`
   (`crates/orrery_identity/src/fdb.rs:560-568`), and
   `newest_live_strike_ms` is computed over rows with `weight_milli > 0`
   (`crates/orrery_identity/src/standing.rs:354-361`), so an `Appeal` row
   (negative weight, `adjudication.rs:824`) can never start or restart a
   dwell. The comment at `standing.rs:316-318` says so by design.
3. **The entry survives an appeal that lands the score between `C` and
   `B`.** `cooldown.rs:61-63` records the entry for `Banned` as well as
   `Cooldown` "so an upheld appeal later dropping the score below B but not
   below C" preserves the original start. #884 thought about appeals at the
   *ban* boundary and chose continuity of the dwell clock; it did not write
   down what happens when the appeal takes the score below `C`.

### 2.2 Release is on the mint path only

The filing reactor (`crates/orrery_identity/src/filing.rs:40-45`, `:277-290`)
calls exactly one mutation, `observe_cooldown`, and "holds no release path at
all". The invalidation feed publishes `dc` membership
(`crates/orrery_identity/src/invalidation.rs:29-33`) and consumers never
remove an applied entry on absence (`:46-51`); recovery runs through minting,
and the gateway's watermark rule admits any token issued at or after
`effective_from_ms` — "a lifted cooldown or an upheld appeal"
(`crates/orrery_persistd/src/gateway.rs:531-537`,
`crates/orrery_protocol/src/identity.rs:96-103`). So whatever the owner
decides, the *moment* of release is the account's next mint attempt after the
rule is satisfied; nothing pushes a release.

### 2.3 What an upheld appeal writes

`uphold_appeal` (`adjudication.rs:805-835`):

- refuses an `Appeal` row or a non-positive weight as the thing appealed
  (`:811-815`);
- refuses when strike filing is off — `strike_filer` is `None` unless
  `persistd` runs with `--strikes shadow|live` and an FDB cluster file
  (`:816-820`; `crates/orrery_persistd/src/bin/persistd.rs:2897-2909`,
  `:3021-3027`);
- stamps `issued_at_ms = filer.clock()` (the executor's wall clock), weight
  `-appealed.weight_milli`, the original's `evidence_ref`, `ruleset` and
  `mode`, and a fresh 90-day expiry (`:821-830`);
- files through `StrikeLedger::file` with no episode key, so it is
  deduplicated by `(evidence digest, Appeal)` (`:831-834`;
  `adjudication.rs:1339-1344`), and — because `file` writes a `yd` filing
  notice for every row in the same transaction (`:1365-1379`) — it queues a
  re-evaluation that the filing reactor will run. That re-evaluation can only
  create/restart a `dc` entry when the account is still at or above `C`
  (`filing.rs:254-257`, `:277-290`); it cannot release.

The scorer treats the row as any other: `score_rows`
(`standing.rs:242-262`) decays it from **its own** `issued_at_ms`, rounds a
negative contribution away from zero (`:251-255`), and counts it only if
`mode == Live`.

### 2.4 What D33 says, and where it stops

- Clause (d) (`0033-strike-ledger-standing.md:165-170`) makes the minimum
  cooldown a deployment dial with invariant (iv) `cooldown_min > 0` — "a
  cooldown that can be left instantly is not one" (`:187`). It says nothing
  about appeals.
- Clause (e) (`:234-240`): "cooldown reverses only after both `S < Q` and
  fourteen consecutive days since its most recent entry". Its rationale
  (`:242-246`) is explicitly about *decay*: "the 14-day floor therefore
  prevents a precisely timed low-weight sequence from clearing on the first
  possible read and provides a full half-life of observation."
- Clause (e) on appeals (`:248-252`): the compensating fact is appended;
  "Identity then clears the derived ban/cooldown state only when
  **recomputation** says it may." Written 2026-08-21, eleven days before the
  dwell existed (#884 merged 2026-09-01), when recomputation *was* the whole
  release rule. Read literally today it can mean either "the score
  recomputation" (exoneration releases) or "the whole release rule, dwell
  included" (it does not). That ambiguity is the issue.
- Clause (h) (`:326-331`): cooldown admits nothing. Relevant to what
  "observation" can mean during a dwell — see §4.2.

Two rationales for the floor coexist in clause (e): an **anti-gaming** one
(a timed low-weight sequence must not clear on the first read) and an
**observation** one (a full half-life). §4 tests each against exoneration.

## 3. What is reachable today

`uphold_appeal` has **no production caller**. The only references in the
workspace are its definition and three unit tests in the same file
(`adjudication.rs:1982`, `:2014`, `:2018`; workspace-wide search over `*.rs`,
`*.md`, `*.toml`). `StrikeKind::Appeal` is constructed nowhere else. #877
recorded the same fact ("constructed nowhere in the workspace") and closed
with the row type, the sign-aware rounding and offence-time attribution
landed, and the producer still unwired. `persistd` "has no scrape or admin
surface" (`persistd.rs:29`, `:1286`), and `orrery-identity` exposes only the
mint and reactor paths. There is no endpoint, CLI or operator tool through
which anyone — owner included — can file an appeal today.

A caller would have to supply three things `uphold_appeal` cannot derive:

1. **The `StrikeRow` being reversed** — read from the account's `ya` span.
2. **The subject `NodeId`.** A `StrikeRow` does not carry it
   (`adjudication.rs:125-142`); the episode index stores only an expiry
   (`:1353-1357`). It must be recovered from the retained evidence bundle,
   which D33 clause (b) names as "the appeal source" (`:105-108`).
3. **The same `OffenceTime`** the original was filed under
   (`:801-804`), so a NodeId rebound since the offence cannot redirect the
   credit to a new owner. With `OffenceTime::Unknown` the ledger refuses any
   node with more than one owner in its history (`:349-366`). The verdict
   path today files with `OffenceTime::Unknown` unless the caller had an
   authenticated instant; the appeal caller inherits that limitation.

And it would find this state: the original row still present and immutable;
possibly a `dc` row with `entered_at_ms` at the crossing (stamped by the
reactor, `filing.rs:47-52`) or at the first refused mint; the account refused
at every mint until `entered_at_ms + cooldown_min_ms`. After the appeal, under
today's code, **the same refusal until the same instant** (§2.1 item 1).

## 4. The numbers

### 4.1 Configured values

Every number below is the shipped default; the repository sets **no
override anywhere** — `ORRERY_IDENTITY_COOLDOWN_MIN_MS` / `--cooldown-min-ms`
(`orrery-identity.rs:326-328`) appear in no script, unit file, Terraform
variable or doc (`infra/` is S3/IAM/OIDC only). Production runs
`DEFAULT_STANDING_THRESHOLDS` (`standing.rs:51-58`).

| Dial | Value | Where |
|---|---:|---|
| `Q` / `C` / `B` | 3.0 / 5.0 / 7.0 points | `standing.rs:52-54` |
| half-life | 14 d | `adjudication.rs:38` |
| **`cooldown_min_ms`** | **14 d** | `standing.rs:56` |
| probation | 7 d | `standing.rs:57` |
| strike retention (appeal window) | 90 d | `adjudication.rs:40`; D33 (b) |
| max session-token TTL | 1 h | `orrery_protocol::MAX_SESSION_TOKEN_TTL_MS` |
| major / non-coop / timing weight | 3.0 / 1.0 / 0.5 | `adjudication.rs:47-53` |

Invariant (iv) forbids setting the dwell to zero (`standing.rs:172-176`), so
"turn the dial down" is available only as far as one millisecond and shortens
*earned* cooldowns equally. It is not a substitute for a rule.

The issue's framing ("ten minutes into a one-hour cooldown") understates the
stake by three orders of magnitude. **The default dwell is fourteen days.**
`docs/07-witnessing.md:212` still says cooldown is "typically 24–72 h
wall-clock (ops dial)" under the superseded `3/6/10` table; D33 clause (d)
and the code say 14 d. That expansion row is stale and §9 lists it.

### 4.2 Timelines a cooldown actually has

With `S(t) = Σ w_i · 2^(-(t - t_i)/14 d)` and `d(a,b) = 14·log2(a/b)`:

```
entry pattern                           S at entry   S<C after   S<Q after   released today (floor)
two majors, 8.19 d apart (edge of C)     5.000        0 d          10.32 d     14 d after entry
two majors, same day                     6.000        3.68 d       14.0 d      14 d
three majors, same day (Banned)          9.000        11.87 d      22.19 d     14 d  (via Cooldown at 5.08 d)
one major + two non-coop, same day       5.000        0 d          10.32 d     14 d
```

Under today's rule the floor binds in every row of that table: no cooldown
entered at the default thresholds ends by decay before day 14. So the dwell
is not a corner-case backstop; it *is* the cooldown duration for every
account that does not re-offend.

What "a full half-life of observation" (D33 `:245`) can observe during those
14 days: not the account's play, because clause (h) admits nothing. What it
can observe is **the ledger** — verdicts for offences committed *before* the
refusal that are still in flight (spot replay is asynchronous and sampled,
`docs/07-witnessing.md` §4.5; the verdict-to-row path is `adjudication.rs:
657-752`). A late verdict restarts the dwell (`fdb.rs:562-568`). That
observation argument survives exoneration unchanged: a released-early account
that picks up a late live strike with `S ≥ C` is refused again at its next
observation with a fresh entry, and with `S < C` the late strike alone would
not have cooled it down anyway.

### 4.3 How long an appeal takes

**Not establishable from the tree.** There is no appeal path, so there is no
measured or specified latency, no queue, no SLA. What the records fix:

- The reviewer is a human reading retained evidence (D33 `:239-240`; `docs/07
  :213`); for this project the acceptance authority is the repository owner
  (D32 `:564`). Realistically that is a person with one to two looks per day
  at best, so **days, not minutes**, and not "hours" reliably.
- The window is bounded above by the 90-day retention: after that the row
  and its evidence are gone and there is nothing to appeal.
- The 14-day dwell is therefore the same order of magnitude as a plausible
  review latency. If review takes 1 d the exonerated account serves 13 d it
  did not owe; at 3 d, 11 d; at 7 d, 7 d. Only if review routinely takes
  longer than the dwell does the fairness case collapse — and a review that
  slow makes the strike effectively unappealable during the cooldown, which
  is a different problem.

The issue's hypothetical "appeals resolve in hours and cooldowns last
minutes" is inverted here: cooldowns last two weeks and appeals will take
days. **The fairness cost is real and is measured in days per wrongful
cooldown.**

### 4.4 How many accounts this touches

Also not establishable: C5 launches in shadow and goes live only when the
false-positive rate on known-honest cohorts is measured negligible
(`docs/07-witnessing.md:215`; D32). If that gate is honoured, wrongful
cooldowns are rare by construction and this rule decides *per-incident*
fairness, not a fleet-wide cost. That cuts both ways: the attack surface in
§5 is equally per-incident.

## 5. The attack case, worked

The second reading says early release is "a lever an attacker might aim at".
To convert an upheld appeal into early release an attacker must obtain an
upheld appeal. Enumerating what that takes on this tree:

1. **Reach the executor's method.** There is no RPC, CLI or admin surface
   (`persistd.rs:29`, `:1286`; §3). Whatever caller lands must be an
   operator-plane action; D32 clause (i) already sets the pattern for such
   actions — Ed25519-signed by a key in `--operator-key`, verified by the
   reader (`0032-enforcement-ramp.md:710-727`). An appeal producer that
   does less than that would be a defect of the producer, not of this rule.
2. **Or compromise the reviewer** — the owner, or a future delegated
   reviewer — into upholding a false appeal, or forge exculpatory evidence
   that survives replay (D10: evidence is self-verifying; the ledger row
   carries the evidence digest).

Either way, the attacker controls **the appeal authority**. Now compare what
that authority already grants without any early-release rule:

| Prize | Already reachable through a compromised appeal? | Size |
|---|---|---|
| Remove 3.0 points from the ledger permanently | yes — that is what the row does (`:824`) | resets escalation toward ban (three majors within 22.19 d) |
| Lift a ban | yes — D33 (e) makes ban "appealable only by human review"; `cooldown.rs:61-63` already routes it through the same dwell | the terminal standing |
| Stack negative credit via late appeals | yes — §9 item 3, an adjacent defect | up to −1.5 points per 14-day-late appeal |
| **End a cooldown's dwell early** | **only if this decision says yes** | **≤ 14 d of admission on one account** |

The marginal lever is the last row: at most `cooldown_min_ms − (t_appeal −
entered_at_ms)` days of play, on an account whose ledger the attacker already
rewrites at will. Two bounds make it small:

- **The fresh-account bound.** An attacker who wants to play during a
  cooldown buys another account. It is admitted immediately and plays on a
  7-day probation flag (`standing.rs:57`; D33 erratum `:304-313`) that costs
  witness eligibility, not admission. So the value of early release to an
  attacker is bounded by the cost of one account acquisition (D41 invites),
  which is the Sybil price the whole standing machine already relies on
  (`docs/07-witnessing.md:20`).
- **The re-offence bound.** A released account that earns a new live strike
  with `S ≥ C` is refused again with a fresh 14-day entry
  (`fdb.rs:562-568`); with `S < C` it is quarantined, not free. Early release
  never buys a clean slate, only a shorter refusal.

One design of early release *does* reopen a real hole, and it must be named
so it is not chosen by accident. If the rule were "any `Appeal` row newer
than the entry waives the floor, subject to `S_live(now) < C`" (naive
Option 2, §6), then an account with three majors (S = 9.0), one of them
reversed, would sit at 6.0 — a cooldown **earned by the two remaining
strikes** — and be released by decay at 3.68 d instead of 14 d. That is
exactly the "decay alone releases" defect #884 closed, reopened for anyone
holding one upheld appeal among several strikes. Option 3 closes it by asking
whether the cooldown would have happened without the reversed row.

**Where the evidence points.** With the appeal authority as the only entry
point, the attack case is *conditional on a compromise that already hands
over larger prizes*, and its residual value is bounded by one account
acquisition. The fairness case is unconditional: every wrongful cooldown
costs 14 d minus review latency, in days. The evidence favours releasing a
wrongful cooldown — **strongly**, provided the rule cannot discount an
*earned* one. The unquantified worry does not outweigh the quantified cost.

## 6. Options

Notation: `rows` is the account's live `ya` span at `now`; `E =
entry.entered_at_ms`; `C` and `dwell = cooldown_min_ms` from the thresholds;
`S(rows, t) = score_rows(rows, t).live_milli`.

### Option 1 — status quo: exoneration never shortens the dwell

```
release iff  S(rows, now) < C  and  now - E >= dwell
```

No code change. Clause (e) is amended only to say so explicitly.

- **Cost:** every wrongful cooldown runs its full 14 d regardless of how fast
  it is overturned (§4.3). The account is told "the strike should not have
  been filed" and refused for another `dwell − (t_appeal − E)`.
- **Benefit:** the mutation boundary in `cooldown.rs` stays as small as
  #884 left it; the floor is a pure function of two timestamps; defence in
  depth against a bad appeal path is maximal (the compromised reviewer still
  cannot shorten a refusal).
- **What it asserts:** that dwell is a cooling-off period the account owes
  for having *been* at `C`, independently of whether it should have been.
  Clause (h) makes that hard to defend: nothing is observed during a
  cooldown except the ledger, and the ledger keeps working after release
  (§4.2).

### Option 2 — any exoneration waives the floor (naive)

```
appealed = { a in rows : a.kind == Appeal and a.issued_at_ms > E }
release iff  S(rows, now) < C  and  (now - E >= dwell  or  appealed != ∅)
```

- **Cost:** reopens the decay hole for partially-reversed ledgers (§5): an
  earned cooldown with one reversed strike among several ends by decay with
  no floor. Not recommended in this form.
- **Benefit:** simplest fair rule; one extra predicate on the observation.

### Option 3 — exoneration voids a *wrongful* cooldown; an *earned* one keeps its dwell

The floor is a consequence of the crossing. If the crossing would not have
happened without the reversed row(s), the cooldown was wrongful and the
floor is void. If the remaining rows crossed `C` on their own, the cooldown
was earned and the floor stands.

```
reversed(rows) = { p in rows : p.weight_milli > 0
                   and exists a in rows : a.kind == Appeal
                                      and a.evidence_ref.digest == p.evidence_ref.digest
                                      and a.weight_milli == -p.weight_milli }
remaining      = rows \ reversed(rows) \ { a : a.kind == Appeal }

wrongful       = S(remaining, E) < C          // scored AT THE ENTRY INSTANT, not now
release iff      S(rows, now) < C
             and ( now - E >= dwell  or  wrongful )
```

Worked on §4.2's rows (appeal upheld at day 3, major = 3.0):

```
two majors 8.19 d apart, one reversed:  S(remaining, E) = 3.0 or 2.0  < 5  -> wrongful -> released at day 3 (Quarantined or Good)
two majors same day, one reversed:      S(remaining, E) = 3.0         < 5  -> wrongful -> released at day 3 (Quarantined)
three majors same day, one reversed:    S(remaining, E) = 6.0         >= 5 -> earned   -> floor stands, day 14
three majors, two reversed:             S(remaining, E) = 3.0         < 5  -> wrongful -> released when S(rows,now) < C
```

- **Cost:** the scorer must expose one more read-only derivation
  (`remaining` and `S(remaining, at)`), and `apply_dwell` must score at `E`
  rather than only at `now`. Roughly thirty lines and three tests; the
  compare-and-clear at `cooldown.rs:102` is reused unchanged, so a concurrent
  restart still wins. Matching an `Appeal` to its original by `(digest,
  -weight)` is the same identity the ledger deduplicates on
  (`adjudication.rs:831-834`).
- **Benefit:** fair in exactly the case the first reading names, closed in
  exactly the case #884 exists for. An attacker with one upheld appeal gains
  nothing unless *that* strike was the one that crossed `C` — in which case
  the cooldown was indeed wrongful and the release is the correct outcome
  whoever asked for it.
- **Edge cases, decided by the pseudocode:** an appeal issued *before* `E`
  (rollout: entry stamped at first observation after #884 shipped) is
  handled identically — `remaining` excludes the pair, `S(remaining, E)` is
  what the entry was actually earned on. A strike newer than the appeal that
  keeps `S(rows, now) ≥ C` is refused at `cooldown.rs:64-80` before any of
  this runs, and it restarted `E`. A shadow original and its shadow appeal
  never enter `S` at all.

## 7. The clause (e) text each option needs

Amending D33 is owner-reserved. Against
`docs/adr/0033-strike-ledger-standing.md:248-252` as it stands:

> An upheld appeal appends an executor-authorized compensating fact referencing
> the appealed evidence, with the original weight negated for scoring; it never
> rewrites history. Identity then clears the derived ban/cooldown state only
> when recomputation says it may. A rejected appeal changes nothing. A ban is
> therefore terminal against time, not against demonstrated error.

**Option 1** appends one sentence:

> Recomputation includes clause (d)'s minimum cooldown: an upheld appeal
> lowers the score but does not shorten a dwell already entered, which runs
> from its most recent entry regardless of why the score later fell.

**Option 3** replaces the second sentence:

> Identity then clears the derived ban/cooldown state only when recomputation
> says it may. Recomputation distinguishes a wrongful cooldown from an earned
> one: if the remaining live rows, scored at the cooldown's most recent entry
> instant, were below `C`, the entry was a consequence of the reversed
> finding and clause (d)'s minimum cooldown is void; otherwise the floor
> stands and the appeal lowers the score only. A ban's entry is treated the
> same way through the cooldown it decays into.

**Option 2** would replace it with "an upheld appeal that leaves `S < C`
ends the minimum cooldown", and §5 says why that should not be written.

## 8. Recommendation

**Option 3.** The reasons, in the order they weigh:

1. **The floor's own rationale does not reach exoneration.** D33 `:242-246`
   justifies the floor against "a precisely timed low-weight sequence" — an
   attacker timing *decay*. An appeal cannot be timed by the appellant, and
   clause (h) leaves nothing to observe during a dwell except the ledger,
   which keeps working after release (§4.2).
2. **The fairness cost is quantified and large relative to the alternative
   harm.** 14 d default, no override deployed, invariant (iv) forbids zero;
   review will take days; the account serves the difference (§4.3).
3. **The attack lever is conditional and bounded.** It requires the appeal
   authority, which already rewrites the ledger and lifts bans; its residual
   value is one account acquisition (§5). Option 3 removes even that
   residual for earned cooldowns.
4. **Option 3 is the reading under which #884's decision and the appeal
   clause are both true.** Decay alone never releases; an appeal releases
   only what it disproves.

What acceptance would authorise, and what it would not:

- Authorises: the scorer derivation and the `apply_dwell` change in §6,
  three tests (wrongful two-major, earned three-major, appeal-before-entry),
  the clause (e) amendment in §7, and a D16 note that the minimum cooldown is
  "void on a wrongful entry".
- Does not authorise: wiring `uphold_appeal` to a caller. That is #877's
  producer, and it needs its own trust decision (operator-signed, per D32
  clause (i)) and its own evidence path for the `NodeId` and `OffenceTime`
  (§3). This memo only fixes what that caller's effect on a dwell would be.

If the owner prefers **Option 1**, the code needs nothing and clause (e)
needs the one sentence in §7 — but the record should then say plainly that
an exonerated account serves the remainder of its dwell, so the producer's
author does not rediscover this question.

## 9. Adjacent findings, not decided here

Found while reading; each is a separate issue and none is changed by this
memo.

1. **Release threshold: `S < C` in code, `S < Q` in the record.** Clause (e)
   (`:238`) says cooldown reverses only after `S < Q`; `apply_dwell` releases
   into `Quarantined` at `S < C` (`cooldown.rs:84-90`, `:106-110`), and
   `score_decay_behavior_is_unchanged_after_dwell_passes` asserts
   `Quarantined` at day 14 (`cooldown.rs:272-286`). At the default floor the
   two agree for every entry at or below `S = 6.0` (`S < Q` arrives by
   14.0 d), and diverge above it: at `S = 6.9` the code releases at 14 d
   (`S ≈ 3.45`) where the record would hold until 16.82 d.
2. **Ban is not terminal in code.** Clause (e) `:239-240`: "ban never
   reverses by decay". `classify` is score-only (`standing.rs:196-206`); a
   9.0 ban decays to `Cooldown` at 5.08 d and is released through the same
   dwell at 14 d. No durable ban flag exists. If the owner meant it, it needs
   a `db`-style row and a rule; #877's "ban ... clearing on recomputation"
   assumed one.
3. **A late appeal over-credits.** The `Appeal` row decays from its own
   `issued_at_ms` (`adjudication.rs:823`, `standing.rs:247-250`), while the
   original decays from the earlier filing instant, so the pair nets
   `w·(2^(-(t-t₀)/14) − 2^(-(t-t_a)/14)) < 0`. Upheld 14 d after filing, the
   net is −1.5 points at the moment of upholding, decaying thereafter. That
   masks half a major finding and is a small lever for the appeal authority
   in its own right. Fix: score the appeal from the original's `issued_at_ms`
   (carry it on the row, or resolve the pair by digest at read time). Option 3
   is unaffected because it removes the pair rather than summing it.
4. **`docs/07-witnessing.md:210-213` is stale**: `3/6/10`, "24–72 h" and
   "guest" contradict D33 clauses (d), (e) and (h).

## 10. What could not be established from the code

- Appeal review latency (§4.3): no path, no queue, no SLA. The estimate
  "days" is an inference from who the reviewer is, not a measurement.
- The wrongful-cooldown rate (§4.4): C5 has not gone live; no false-positive
  measurement exists yet in the tree.
- Whether `OffenceTime::KnownMs` will be available to an appeal caller: the
  verdict path files `Unknown` unless a caller authenticates an instant, and
  the appeal must reuse the original's (§3). If it was `Unknown` and the node
  has changed hands since, the appeal is refused by construction.
- Account acquisition cost in money or effort (§5's fresh-account bound):
  D41 makes invites single-use capabilities; the price of one is operational,
  not in the tree.

[#1013]: https://github.com/baadc0de/orrery/issues/1013
[#862]: https://github.com/baadc0de/orrery/issues/862
[#877]: https://github.com/baadc0de/orrery/issues/877
[#884]: https://github.com/baadc0de/orrery/pull/884
