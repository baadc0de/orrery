# A17 - The invariant auditor ordering knot: dissolved, with residue

> Research for #224 and #245, the oldest live blocker pair on the board:
> "the economy-wide invariant auditor is due at P5 exit, the archive it
> reads exists only from P6, and nobody owns it." Repository facts verified
> at `d88b1fb3` on 2026-08-28; every `path:line` below was read in this
> worktree before being cited, because line numbers in this corpus drift
> constantly (the auditor row itself has moved three times: `:917` ->
> `:1248` -> `:1279` -> `:1282`). Nothing here amends an accepted record -
> **propose, not decide.** Section 7 lists what stays with the owner;
> section 10 lists what could not be verified.
>
> Series placement, argued: this is an A-series node (next free number,
> a17) rather than a named node like `campaign-admission-service.md`,
> because the named nodes design shippable things and this node is
> verdict-first research on ordering and governance - the a12/a16 shape
> ("shakedown", "design research"), not the build-plan shape. It designs
> nothing new; it establishes what the records already entail and what
> residue is real.

## 1. Verdict up front

**The circular dependency does not exist at HEAD, and on the records it
never did.** The claimed knot rested on reading a *decision* deadline as a
*delivery* deadline. The auditor row sits in the roadmap's open-questions
table under the column header `| Question | Proposed resolution path |
Decision by |` (`docs/11-roadmap.md:1278`); its "P5 exit" is the date by
which the auditing-cadence *decision* is owed (`docs/11-roadmap.md:1282`).
That decision has been made by an Accepted record: ADR-0032 clause (g)
(`docs/adr/0032-enforcement-ramp.md:570-577`) pins auditor liveness to one
event - C3's promotion review - and states "No other control waits for
it." C3 lands in P6 beside the archive tailer the full sweep needs, and
the roadmap says so in P6's own deliverables (`docs/11-roadmap.md:897-910`).
At no point is anything required to read an archive that does not exist.

**None of the three epics is blocked by this.** #106's ramp promotes C1,
C2, C4 and C5 without the auditor (`0032:583-597`); #107 is *upstream* of
the auditor's full sweep, not behind it - the dependency arrow points from
the sweep to #107's tailer, never back; #108 (field-host promotion, chaos
suite) has no dependency on the auditor in any record I could find. The
framing "none of them move while the ordering is circular" is wrong at
HEAD, and was already wrong when #245's first grooming comment landed.

**What is real is residue, and it is small and enumerable** (section 6):
one previously-unfiled P6 deliverable (the daily full sweep, specifiable
today - filed as #615 with this node), two decisions parked with #224
until C3's review enters scheduling range (production cadence,
time-to-detection target), one owner-reserved interpretation (which half
of clause (g)'s cadence parenthetical binds), and four stale citations
inside two Accepted ADRs.

## 2. The knot as claimed, and the three legs it stood on

#245's syllogism: (L1) the auditor is due live at P5 exit; (L2) it is a
journal-archive consumer and the archive exists only where the P6 tailer
has put it; (L3) therefore a P5 exit criterion depends on a P6
deliverable, and the ramp it gates stalls. Each leg, checked:

**L2 is true and load-bearing.** The tailer is a P6 crate deliverable
(`docs/11-roadmap.md:890`), made load-bearing by D20: "The journal is
bounded by a retention floor" (`docs/adr/0020-journal-retention.md:145`),
so a released record's history exists only where the tailer has put it
(`docs/11-roadmap.md:895`). History past the release floor is gone, not
slow.

**L1 is false.** "P5 exit" is in the `Decision by` column
(`docs/11-roadmap.md:1276-1282`). The roadmap asks for the cadence
decision by P5 exit, not for a live auditor. No P5 deliverable names the
auditor: the P5 Deliverables list (`docs/11-roadmap.md:874-880`) covers
co-signatures, the trade flow, enforcement switches, and schema
versioning - no auditor. The only other P5 anchor is a line in the
*proposed, not accepted* Beyond-P6 section (`docs/11-roadmap.md:1095`),
which itself already carries the correction (`:1098`: the full-history
sweep "structurally cannot predate P6's archive tailer").

**L3 is therefore false as stated, and its true core is already written
down.** The promotion predicate conditions on the auditor for exactly one
control:

```
promote(C) <=> production leg
           AND sensitivity leg
           AND review gate
           AND (C = C3 => auditor live, clause (g))
```

(`0032:442`, transcribed to ASCII). The composed consequence - clause (g)
defines "live" to include the daily full sweep; the full sweep needs
history; D20 bounded the journal; hence C3's promotion review cannot
conclude before the P6 tailer - is stated verbatim in the roadmap's P6
deliverables (`docs/11-roadmap.md:897-910`, added by #309): "Therefore
C3's promotion review cannot conclude before this tailer is live, and the
ramp cannot be driven fully on within P5."

So the "P5 exit criterion depending on a P6 deliverable" was two accepted
records composing into a *P6* obligation, discovered mid-composition and
mistaken for a contradiction.

## 3. How it dissolved: the timeline, dated

The dissolution happened incrementally across four merged changes, which
is why the issues still read as open blockers - each fix landed after the
issue text froze:

| Date (2026) | Event | Effect on the knot |
|---|---|---|
| (pre-08-23) | #242 -> D32 Accepted (`docs/DECISIONS.md:49`) | L1's decision made; gate scoped to C3 only, both directions wired (`0032:600-605`) |
| 08-23 06:06 | #309 merged | composed consequence written into P6 deliverables (`docs/11-roadmap.md:897-910`) |
| 08-23 12:36 | #330 closed via PR #343 | incremental half shipped: `crates/orrery_persistd/src/audit.rs`, wired into the daemon (`crates/orrery_persistd/src/bin/persistd.rs:1249-1253`), on by default at hourly cadence (`persistd.rs:270`, `default_value_t = 3_600_000`) |
| 08-25 09:21 | PR #430 merged | open-questions row split by prerequisite (`docs/11-roadmap.md:1282`) |

#224 and #245 were opened 22 minutes apart, before every row of this
table, and their titles still assert the pre-table state.

## 4. What the auditor must prove, and what each half can prove

The per-intent path validates one transaction against its own
preconditions; the auditor exists for what that structurally cannot see -
a drain that is individually valid at every step (the documented GTA
Online post-hoc-correction failure, cited at `docs/11-roadmap.md:872` and
`0032:581-583`). The two halves prove different properties:

- **Hourly incremental over hot ledgers** (shipped, #330): reads *current*
  state - `ledger/bal/{account}/{asset}` (LE i128,
  `crates/orrery_persistd/src/keyspace.rs:1663`), `ledger/item/{item_uid}`
  ("the single-ownership row is the anti-dupe invariant",
  `keyspace.rs:1679`), and the versionstamp-ordered `ledger/receipt` walk
  with a durable cursor at `ledger/audit-cursor` (`keyspace.rs:1802`).
  Catches: duplicate ownership rows, balance moves without receipts,
  structural violations in current state. Cannot catch: a leak seeded
  before its cursor window, or value parked in cold ledgers and drained
  through history it never walks.
- **Daily full conservation sweep** (unbuilt, needs P6's tailer; now
  #615): global conservation over all history. Catches the slow composed
  leak. This is the half clause (g) exists for, and it gates only C3 -
  the one control that reaches backwards into journaled history
  (`0032:594-598`) - which is exactly the control that needs it.

Measured figures from #330's harness
(`docs/data/hot-ledger-sweep-ttd-2026-08-23.json`): 12 planted
violations, 0 missed; sweep interval 150 ms; TTD min/median/p95/max =
7/83/156/156 ms; pass duration <= 2 ms. The model is TTD ~= U(0, I) + D
for interval I and pass duration D - the measured median 83 ms is ~I/2 as
predicted. At the recorded start cadence I = 1 h: expected TTD ~= 30 min,
worst case ~= 60 min + D. **The missing number is the target, not the
machinery**: nothing anywhere states the time-to-detection the cadence
must meet, and clause (g) explicitly leaves that to #224
(`0032:602-605`).

## 5. The ways out, weighed - and which one the tree already took

For the record, the four candidate exits from the knot-as-claimed, and
what each does to the guarantee P5 makes (P5's goal: "cluster as sole
writer of value", `docs/11-roadmap.md:872`):

| Exit | Cost to schedule | Cost to guarantee | Status |
|---|---|---|---|
| (a) Full auditor liveness is a P6 obligation; P5 owes decisions only | none - C3 is P6 anyway | none: every control the auditor does not gate is pre-hoc or moves no durable value (`0032:583-597`); the post-hoc control and its gate travel together | **taken** (D32 (g) + #309 + #430) |
| (b) Build a minimal archive slice in P5 for the full sweep | a new P5 write path, duplicating #107's tailer | none, but buys nothing: the sweep's only consumer (C3's review) is P6 | rejected - spends P5 effort to pull forward a gate with no P5 consumer |
| (c) Auditor reads what already exists (hot ledger rows; the read-only precedent is `orrery_persistd`'s own census posture) | small | partial: current-state invariants only (section 4) | **taken for the incremental half** (#330) |
| (d) Ramp proceeds ungated, risk stated | none | C3 live without a conservation sweep is the GTA failure verbatim (`0032:594-598`) | rejected by clause (g), and D32's own alternatives table rejects the inverse over-gating too (`0032:759-762`) |

The tree took (a)+(c), which is the only pair that costs neither the
schedule nor the guarantee. No further exit is needed.

## 6. The residue: what is actually left, and where each piece goes

1. **The daily full sweep had no issue.** It is specifiable: a read-only
   consumer of #107's archive, checking global conservation per asset and
   per-item ownership continuity across history, findings into the same
   pipeline `audit.rs` already emits to, explicitly not an enforcement
   actor, and producing the measured full-pass scan cost #224's cadence
   decision is waiting on. It belongs on the P6 milestone riding epic
   #107, beside the tailer it depends on - filing it there is sequencing,
   not a record change. **Filed as #615 with this node.**
2. **Production cadence and the time-to-detection target** stay with
   #224, due when C3's promotion review enters scheduling range - not
   before, because deciding earlier forfeits the production-scale scan
   costs only P6's archive can yield (the 2 ms pass over a seeded test
   ledger extrapolates to nothing).
3. **Auditor packaging** (workspace crate vs standalone tool, a D15
   question) is already parked at P8 entry (`docs/11-roadmap.md:1231`).
   Leave it there.
4. **Epic home.** "Has no owner" (#224) means *no epic home*, and D32
   looked straight at the question and declined it: clause (a) assigns
   the *ramp* to #106 (`0032:147`), while clause (g)'s close says
   cadence, ownership and the TTD target "remain #224's to settle"
   (`0032:602-605`). Recommendation, as sequencing: the full sweep rides
   **#107** (its only hard dependency and its natural conflict surface);
   the already-shipped incremental stays where it lives, in
   `orrery_persistd`; #224 itself remains the decision ledger for cadence
   and target until C3's review. No new epic - the work splits cleanly
   into an existing one plus a parked decision.

What unblocks the epics: nothing needed to. #106 was never blocked (four
of five controls promote auditor-free); #107 blocks the sweep, not vice
versa; #108 was never involved.

## 7. Owner-reserved items, separated cleanly

These are interpretations or edits of Accepted records and are **not**
proposed here, only listed:

1. **Whether clause (g)'s "start cadence (daily full conservation sweep,
   hourly incremental over hot ledgers)" binds both halves.** The
   parenthetical reads as a conjunction; #309 wrote the P6 consequence on
   that reading. If the owner ever narrowed it to the incremental alone,
   C3 would no longer be behind P6 - see section 9 for why that is the
   strongest threat to this node's verdict. Moot unless C3 is pulled
   forward of P6, which nothing on the board proposes.
2. **Four stale `11-roadmap.md:917` citations inside Accepted ADRs**:
   `0032:140`, `0032:580`, `0032:760`, `0029:480`. The row is at `:1282`
   as of `d88b1fb3` and has moved three times since `:917`; if these are
   fixed, an anchor (the row's bolded question text) survives where a
   line number will not.
3. **The Beyond-P6 wording at `docs/11-roadmap.md:1095`** ("P5
   deliverables owned by no epic ... their construction stays P5 as the
   roadmap says") - the P5 Deliverables list never names the auditor, so
   "as the roadmap says" is a composed reading; but that section is
   marked proposed-not-accepted in its entirety, so amending it is the
   owner's either way.

## 8. Recommended dispositions for the issue pair

- **#245**: refuted as stated, and its own thread already shows the
  refutation plus the row-split fix (#430). The derived constraint it
  uncovered (C3's review behind the P6 tailer) is recorded in the
  roadmap's P6 deliverables. Nothing remains in it that #224 or #615 does
  not carry. Recommend: close as completed-by-#430, pointing here.
- **#224**: retitle/rescope to what it still owns - the cadence and TTD
  decisions parked until C3's review, plus the epic-home recommendation
  above if the owner takes it. It should not stay titled as gating "the
  P5 enforcement ramp": it gates C3 only, and C3 is P6. Keeping it open
  is right; keeping its title is what made it re-surface as urgent three
  separate times.

## 9. Strongest argument against

The case that this node's verdict - "dissolved; park the decisions" - is
wrong:

**The dissolution is one interpretation deep.** Every load-bearing step
rests on reading clause (g)'s cadence parenthetical as binding *both*
halves. That reading is nowhere decided; `0032:602-605` explicitly hands
the residue to #224, and #245's final owner-question about it is
unanswered. If a future lane - under schedule pressure to demo
"enforcement fully on" - reads "live and sweeping" as satisfied by the
incremental alone (which *is* deployed by default, sweeping hourly, and
emitting findings: `persistd.rs:1249-1253`, `audit.rs:171`), clause (g)'s
letter is arguably met and C3 can be promoted inside P5 with no full
sweep ever built. The residual leak class - seeded before the cursor
window, or drained through cold ledgers - is then live exactly when
annulment goes live, which is the GTA failure the gate is named for. A
node that says "nothing is urgent" makes that misreading *easier*,
because it removes the alarm that kept anyone looking. The
counter-counter is that promotion requires clause (e)'s review gate with
evidence in front of a human - but four stale citations sitting in two
Accepted ADRs are standing evidence that this corpus is read less
carefully than its governance assumes.

**And the measurement the parking argument leans on is toy-scale.** The
"decide cadence later, from data" recommendation cites 12 samples at a
150 ms interval over a seeded test ledger with 2 ms passes. If production
full-sweep passes cost hours, "daily full sweep" may be infeasible as
written and the cadence decision becomes an architecture question, not a
dial - and C3's promotion review is the latest possible moment to
discover that. If the owner weighs this heavier than the schedule
argument, the right move is the opposite of parking: answer the
both-halves question now (it costs one sentence in an erratum) and attach
a scan-cost spike to #615 rather than leaving it a bare deliverable.

## 10. What could not be verified

- **That the incremental sweep runs anywhere but tests and defaults.** It
  is wired and on by default in the daemon, but I found no evidence of a
  production deployment sweeping today; clause (g) consumes "deployed,
  sweeping" at C3's review time, and code-merged is not that.
- **Production archive scan cost** - no archive exists to measure; the
  full sweep's feasibility at "daily" is asserted by the roadmap sketch,
  not by any figure in the tree.
- **The owner's reading of clause (g)'s parenthetical** (section 7 item
  1) - deliberately unresolved in every record that touches it.
- **The audit pipeline's downstream consumer** - `audit.rs` emits
  findings (log target `orrery_audit`); what reviews them, and whether
  C3's promotion-evidence format under clause (e) ingests them, is not
  yet built and could not be checked.
- **Whether #108 has an indirect dependency on the auditor** through the
  chaos suite - none is recorded; absence of a record is all I can
  attest.

## Cross-references

#224, #245 (the pair) - #330 / PR #343 (incremental half) - PR #430 (row
split) - #309 (P6 consequence) - #615 (the full sweep, filed with this
node) - epics #106 / #107 / #108. Records:
[D32](../adr/0032-enforcement-ramp.md) clause (g),
[D20](../adr/0020-journal-retention.md),
[docs/11-roadmap.md](../11-roadmap.md) (P5, P6, and the open-questions
row).
