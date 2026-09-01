# Seam-existence staleness sweep — the #738 seam and everything landed on top of it, applied below the ADR layer

**Documentation-only lane.** Follow-up to
[plan-staleness-sweep-2026-09-01.md](plan-staleness-sweep-2026-09-01.md)
(#854), which flagged three seam-existence claims as OUT OF SCOPE for its
trigger set and left them uncorrected. This sweep corrected those three,
then searched independently for the same class of claim: anything asserting
the `SimulationHost` seam does not exist, that no host implementation
exists, that nothing has been migrated to an ECS, or that the differential
harness is unbuilt. Branch: `docs/seam-exists-staleness`. Nothing here
amends an Accepted ADR — amending one is owner-reserved
([AGENTS.md](../../AGENTS.md)); `check.sh` is exempt (prose; AGENTS.md, "The
push is the gate"); `./scripts/lane-diff-audit.sh` passes on this diff.

The landings this sweep tested claims against:

- **#738 (2026-08-30)** — the seam: `crates/orrery_sim_host`, `SimulationHost`
  at `src/lib.rs:248`, the existing `Ruleset` hosted through an adapter.
- **#748/#749 (2026-08-31)** — the F-4 differential harness, all four A10
  §4.1 classes (`crates/orrery_games/src/diff.rs:1`); #749 also added
  `iroh-base`, `bytes`, `postcard` and `serde` to `orrery_games`' manifest.
- **#757 (2026-08-31)** — the host substrate behind the seam: `EcsBackend`
  (`crates/orrery_sim_host/src/ecs.rs:653`), accepted wherever
  `SimulationHost::on_backend` accepts the executor (`ecs.rs:6-7`), at
  four-class F-4 parity.
- **#771 (2026-08-31)** — Tier H recorded as landed and enforced
  mutation-style (battery: `scripts/core-gates.sh` section 6;
  `DECLARED_HOST_CRATES=(orrery_sim_host)`).
- **#855 (2026-09-01)** — S7.4 lane one: `regolith.world` migrated to
  ruleset-owned components and systems at four-class parity
  (`crates/orrery_games/src/regolith/world_ecs.rs:1-13`); `bevy_ecs` now a
  taken dependency of `orrery_games` (`crates/orrery_games/Cargo.toml:40`);
  `orrery_core` gained `canonical_step_with`
  (`crates/orrery_core/src/executor.rs:480`), which delegates only the rules
  body to the callback and keeps RNG derivation, neighbour framing,
  quantization, hashing and materialization attribution in core
  (`executor.rs:472-479`).

Every fact above was verified on this tree before any correction was
written. Editing discipline is #854's: the old claim is kept as a dated
statement of what was true when written, the new fact is added after it,
and no document's argument or reasoning history is rewritten.

---

## 1. The three flagged starting points, quoted and corrected

- **a9:67 (E-15 evidence row)** — "No field host exists; the `SimulationHost`
  seam is recommendation, not code." The seam half is falsified by #738;
  the field-host half **stands** (see §4 — the field host is a role A1 §5.5
  defines as "not a crate", still filled by stand-ins; the seam's only
  dependent crate is `gates/migration-bench`). Corrected in place: the
  original claim is kept and dated, the landing and the surviving half are
  stated.
- **a9:263 (§4.3)** — "the host the sidecar would wrap — the `SimulationHost`
  seam — **does not exist yet** (E-15); the sidecar is therefore two
  absences deep: no host seam, no Unreal consumer." Falsified by #738. The
  sidecar is one absence deep now: the host seam exists, the Unreal
  consumer does not. The recommendation sentence after it is argument and
  is untouched.
- **a22:119 pre-edit (§5, now :127)** — "**This constrains S5's API shape,
  and S5 has not been built.**" Falsified by #738 — and by the document's
  own §6 landed note, which already records "S5 (#738) was designed
  against" the landed C ABI. Corrected: "not been built" is dated as
  history; the C-ABI constraint itself is untouched, since it binds any
  future C++ crossing of the now-existing seam.

## 2. Corrected (subordinate documents; 4 files, 11 claims)

| File | Line (pre-edit) | What it said | What is true now |
|---|---|---|---|
| [a9-engine-boundaries.md](../plans/a9-engine-boundaries.md) | :67 (E-15 row) | "No field host exists; the `SimulationHost` seam is recommendation, not code" | Seam landed 2026-08-30 (#738, `src/lib.rs:248`); `EcsBackend` behind it (#757, `src/ecs.rs:653`); field-host half stands — no field host is built on the seam, one dependent crate (`gates/migration-bench`). The cell's #414 clause also carried a stale status ("still open") — see §6. |
| same | :263 (§4.3) | "the `SimulationHost` seam — **does not exist yet** (E-15); the sidecar is therefore two absences deep" | Landed #738/#757; the sidecar is one absence deep (no Unreal consumer). Recommendation sentence untouched. |
| same | :343 (§5 component 1) | "the `SimulationHost` seam (must exist first — E-15)" | It does — landed 2026-08-30, #738. The §5 "specified, not implemented" banner is untouched: the *Unreal observer* is still not implemented. |
| [a22-engine-agnostic-client.md](../plans/a22-engine-agnostic-client.md) | :36-40 (§1) | "What is missing is not a decision but a **seam** … stage **S5**" | Kept as written; appended: S5 landed 2026-08-30 (#738), so the missing artifact is the seam's consumer (track D), not the seam. |
| same | :127 (§5) | "S5 has not been built" | Kept as written; appended: landed 2026-08-30 (#738) as `crates/orrery_sim_host`; the C-ABI constraint binds any future C++ crossing. |
| [docs/10-crates.md](../10-crates.md) | :60 (graph label) | "Bevy-free today — orrery_games permitted bevy_ecs, not taken" | `bevy_ecs` taken 2026-09-01 (#855), `Cargo.toml:40`; label now says the group is Bevy-free except that one taken, app-free dependency. |
| same | :126 (spine paragraph) | "it has taken no Bevy dependency: the manifest declares `orrery_core`, `orrery_protocol` and `orrery_compose`, so the permission lives in the records and the gate, not yet in the graph" | Overtaken by #855: the dependency is taken, for ruleset-owned components and systems driving the migrated `regolith.world` sections inside `EcsBackend`'s dedicated world; app-free still. The 2026-08-31 acceptance and gate permission stand; the "not taken" clause is dated as history. |
| same | :156 (dependency table row) | Bevy column "**none** (permitted — D42 (a) amended; not taken)"; other-deps column "libm, rand_chacha 0.9, blake3" | Bevy column: `bevy_ecs`, permitted and taken 2026-09-01 (#855). Other-deps column was also stale independent of Bevy — `iroh-base`, `bytes`, `postcard`, `serde` arrived with #749 (verified by git log -S on the manifest) and `rand_core` earlier; the cell is brought current to the manifest. |
| same | :620 (persistence paragraph) | "`orrery_games` depends on `orrery_core`, `orrery_protocol` and `orrery_compose` and takes no Bevy dependency" | First half dated as history (#855 took it); the escape-check sentence stands unchanged — `orrery_games` still has not joined `DECLARED_HOST_CRATES` (it owns no canonical `World`; the dedicated one remains the host's). |
| same | :639 (ruleset-crate paragraph) | "with no Bevy and no tokio — Bevy-free today" | Kept and dated: #855 joined `bevy_ecs` (components and systems, no Bevy app, still no tokio). |
| [a18-ruleset-ecs-implementation-programme.md](../plans/a18-ruleset-ecs-implementation-programme.md) | :504-506 (§5 S7 standing note) | "the dependency is not yet taken - the manifest declares no Bevy" | This standing note was **written by #854** and was true then; #855 overtook it the same day. Dated as history and appended: the migration (`regolith.world` at four-class parity, `world_ecs.rs`) and the invariants (`canonical_step_with`, `executor.rs:480`; `BEVY_PERMITTED_CRATES` still carries the crate; `DECLARED_HOST_CRATES` still names only `orrery_sim_host`). |

## 3. Owner-reserved — nothing new; #854's list stands

No seam-, host-, migration- or harness-falsified claim was found inside an
Accepted ADR beyond what #854 §2 already lists for the owner: D42 clause
(d)'s body and title, `docs/DECISIONS.md:57`'s D42 index row, D43 clause
(e)'s heading and honest-accounting paragraph, D44 (b) and D47 (b)
conditional wording. The ADRs that mention the seam
(D42 (b)(2), D43, D44, D48) do so in amendment or forward-conditional text
that the landings satisfy rather than falsify. Nothing here touched any
file under `docs/adr/`.

## 4. Deliberately left — the "field host" family, history, and argument

The load-bearing distinction of this sweep: **the seam exists; the field
host does not.** A1 §5.5 defines the field host as a *role*, "not a crate",
filled today by stand-ins (the p1-swarm bot, the regolith local session);
#738 landed the seam library, not a field deployment of it. Claims in that
family therefore remain true and are left:

- **[a3-simulation-host-comparison.md](../plans/a3-simulation-host-comparison.md)
  G14 (:56) and §7 (:457)** — "There is no field host" / "The field host
  does not exist (G14)". Still true. §7 additionally remains the section
  #745's audit comment named for the owner's accepting edit — #854 §3 left
  it for that reason and this sweep does not reverse that call.
- **[a2-kernel-game-module-ownership.md:82](../plans/a2-kernel-game-module-ownership.md)**
  — "harness code standing in for the field host that does not exist yet
  (A1 §5.5)". Still true on the same distinction.
- **[a12-exchange-systems-shakedown.md:205](../plans/a12-exchange-systems-shakedown.md)**
  — G14 restatement plus "the seam *would* at least make…" (conditional);
  also inside the incident record #854 §3 already declines to track
  amendments into.
- **[docs/07-witnessing.md:203](../07-witnessing.md)** — "With no field
  host, the intent commits *flagged provisional*". Still true.

Also left, by #854's standing classifications:

- **a18 §2's audit table** (:80-94, including the PR-15/PR-16 "host seam —
  Not started" row) — dated at `f82ee980` by its own convention; #854 §5
  left it and this sweep does not redo that.
- **[a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md)** — the
  2026-08-25 planning record (its PR-15 "no ECS" row describes what that PR
  was to do); left by #854 §3.
- **a22:136-137** — "the argument for running the spike before S5 is
  designed" — the document's argument; the spike's answer is recorded in
  the §6 landed note, so the argument is history but not falsified.
- **a18 §5's S5/S6 spec sections** (:445-485) and the §6 graph (:527-550) —
  plan prose and a plan diagram with no false existence claim; the S5 spec
  is what #738 was built against, and the graph's corrected S7 label
  ("entry sanctioned 2026-08-30; #757 landed") remains true.
- **docs/spikes/\*** — dated snapshots by their own convention (including
  the two that quote the pre-#855 "not taken" census text).
- **[docs/10-crates.md:42](../10-crates.md)** and **[docs/06-verifiable-core.md:3](../06-verifiable-core.md)**
  — "no Bevy dependency" claims about a *sketch* ruleset crate and about
  `orrery_core` respectively; both still true.

## 5. Checked and excluded (false positives)

- **a10 §3's other fixture rows** (F-2/F-3/F-12) — stale for reasons outside
  this sweep's four claim classes; #854 §5 flagged them for a future sweep
  and that flag stands. (See §6 for one new observation on F-2.)
- **docs/14-capacity.md** — no Bevy-status or seam-existence claims; its
  `orrery_sim_host`/`ecs.rs` references were already written against the
  current facts (#854 §4 verified, re-verified here).
- **The `NeighborFrame` producer family** (collision-under-own-state:72,
  a6:279), the ack channel (a19:166), the mechanic trigger (a15:493), the
  lightyear R-1 trigger (docs/11:65), "links that do not exist" (a13:212,
  a14:738) — different subjects, none falsified by the seam landings.
- **a9:24, a9:406** — "zero Unreal code in this tree" (still true) and the
  #414 census-drift row (D-2; issue-status wording, not a seam claim).
- **docs/11-roadmap.md, docs/00-overview.md, docs/12-world-seeding.md,
  docs/13-chain-replication.md** — no seam-existence, host-existence,
  migration or harness-unbuilt claims (12-world-seeding's "S5" is its own
  stage, unrelated).

## 6. Classification uncertainty, stated

- **a9:67's #414 clause.** The E-15 cell asserts #414 is "still open";
  `gh issue view 414` reports CLOSED, and a18:80 (dated table) agrees. The
  issue-status claim is census-drift class, not one of this sweep's four
  classes, but it sits inside the very sentence the task directed this
  sweep to correct, so the cell was corrected on both counts rather than
  leaving a fresh falsehood beside the new text. Flagged here in case the
  owner prefers the #414 status reverted to a flag-only note.
- **10-crates:156's other-deps cell** was refreshed beyond the Bevy cell
  (iroh-base/bytes/postcard/serde/rand_core). Those additions are
  #749-caused drift — inside this sweep's trigger set (the harness
  landings) — but strictly the cell is a census drift, not a false
  *claim*; reverted in one edit if the owner disagrees.
- **a10 F-2 (outcome-chain goldens).** `diff.rs:296-298` defines committed
  D-1 and D-2 golden chains, which looks like F-2's criterion existing
  inside the F-4 harness home. This sweep did **not** verify the tables are
  populated and committed, so F-2's "proposed" row was left exactly as
  #854 left it; the next fixture-row sweep should check it first.
- **"Field host" readings.** a12:205 and docs/07:203 were classified
  still-true on A1 §5.5's role definition. If the owner reads "field host"
  as "any host implementation behind the seam" (in which case #738/#757
  falsify those two), both are one-edit reversals.

## 7. Verification

- `./scripts/lane-diff-audit.sh` — run on this diff; passes.
- `check.sh` — exempt: documentation-only (AGENTS.md, "The push is the
  gate").
- The diff touches three plan documents, one numbered expansion document,
  and this summary; no ADR, no code, no gate, no golden, and no spike other
  than this file.
