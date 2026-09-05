# ADR-0021: `Ruleset` distribution stays link-time, and the harness API freezes

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D21

This decision is normative. See the [ADR index](../DECISIONS.md) for
precedence, scope, and the complete decision set.

**Supersedes:** nothing. It closes the D17.6 open question *"`Ruleset`
distribution to cluster — games recompile `persistd`: acceptable?"*, whose
stated deadline is **P2 exit**, and which is therefore due:
[D19](0019-indexed-waldb-journal.md) landed the backend that made the P2 gate
hold.

## Context

`orrery_persistd` ships as a **library harness**, not as a service binary: a
game links its `Ruleset` and builds the `persistd` it deploys
([D12](0012-backend-services.md), [docs/09](../09-services-and-ops.md) §1,
[docs/10](../10-crates.md) §11). Three things in the cluster call into game
rules — the intent validator (`Ruleset::validate_intent` behind
`intent::IntentValidator`), the adjudication executor
(`AdjudicationExecutor::register`, which holds `RETAINED_BUILDS` version-keyed
workers), and parked-cell catch-up when it arrives — and all three need the
rules *in process*.

> **Correction, 2026-08-30 (#629).** The parenthetical above describes an
> unbuilt member. `Ruleset::validate_intent` is not, and has never been, a
> member of the trait. The trait is `crates/orrery_core/src/ruleset.rs:267-368`
> and its surface is `CoreState`, `CoreInput`, `CoreEvent`, `id`,
> `max_neighbor_reads`, `max_neighbor_staleness_ticks`, `step`, `materialize`,
> `classify_component` and `invariants` — the last six defaulted. The only
> `validate_intent` occurrences in first-party Rust are two doc comments
> recording its absence (`crates/orrery_core/src/lib.rs:66`,
> `crates/orrery_core/src/ruleset.rs:9`), both attributing it to the docs/03
> §3 sketch rather than to the trait.
>
> This corrects the *Context*, not the Decision: the argument that the
> adjudication cluster needs the rules in process rests on the executor and
> on parked-cell catch-up, and stands without this example. Recorded as an
> addition so the original text stays readable; the normative text is
> unchanged, which is the owner's to edit.
>
> **Citation refreshed, 2026-09-05 (#626 stage S0).** The correction's finding
> stands — `validate_intent` is still absent, still only the two doc comments,
> now at `crates/orrery_core/src/lib.rs:68` and
> `crates/orrery_core/src/ruleset.rs:9`. Its `path:line` had already drifted:
> the trait is `crates/orrery_core/src/ruleset.rs:294-412`, and its surface
> gained an eleventh member, `OVERFLOW_IS_CANONICAL` (D43 (f)(3) as amended,
> built in this stage), which is *undefaulted* — the only one besides the three
> associated types, `id` and `step`. The line numbers are re-stated rather than
> silently corrected in place, because a citation that is quietly rewritten
> stops recording when it was true.

The consequence a game team lives with is that **every `Ruleset` change is a
persistd redeploy**, and the game repo owns the deployed artifact. The
question the roadmap left open was whether that is acceptable for 1.0, or
whether the cluster should load rules some other way — the named alternative
being a WASM-sandboxed `Ruleset`.

The roadmap's own proposed path was to keep link-time composition and to
freeze the harness API at P2 exit, "the cheap moment to decide". Nothing has
since produced a concrete demand for the alternative: no third-party rules are
loaded anywhere in the tree, `orrery_games` is linked like any other consumer,
and the P2 gate builds its `persistd` from the workspace.

## Decision

**1. Link-time composition is the answer for 1.0.** A cluster runs rules it was
compiled with. WASM sandboxing is not adopted, and no dynamic `Ruleset` loading
path is built.

The reasoning, stated so the reversal has something to argue against:

- **Determinism is the product.** D9 scopes determinism to a `Ruleset` executed
  by a fixed-tick executor over `rand_chacha` and `libm`, and the cross-platform
  matrix in CI verifies bit-identical replays across four targets. A WASM
  boundary adds a second execution environment whose float, ordering and trap
  semantics have to be shown identical to the native one — on every platform,
  for every host build — before a single verdict from it can be trusted.
  Adjudication is where a `Ruleset` decides whether a player is cheating; a
  sandbox that is *nearly* deterministic is worse than no sandbox.
- **Adjudication performance is on the enforcement path.** The executor
  re-executes windows of up to 180 ticks per bundle and holds three concurrent
  ruleset builds. A sandbox tax there is paid per bundle, per build.
- **The scenario it serves has no launch title.** Loading untrusted third-party
  rules into the cluster is a modding feature. Every game in the roadmap
  compiles its own binary already, because it also compiles its own client.

**2. The harness API is frozen at this record.** Frozen means: a breaking change
to the surfaces below requires an ADR that names this one, not a patch release.
Additive change — new methods, new types, new default-carrying config fields —
is not breaking and needs no record.

The frozen surface is `orrery_persistd`'s public exports, and specifically the
seams a game composes:

| Seam | Surface |
|---|---|
| Runtime construction | `CellRuntime`, `RuntimeConfig`, `JournalConfig` |
| Rules on the intent path | `intent::IntentValidator`, `intent::IntentExecutor` |
| Rules on the evidence path | `AdjudicationExecutor::register`, `RETAINED_BUILDS` |
| Durable tier | `checkpoint::CheckpointStore`, `checkpoint::ColdCellReader`, `checkpoint::CheckpointConfig` |
| Routing and admission | `cluster::Router`, `cluster::Cluster`, the `gateway` exports |
| Leases and fences | `lease::LeaseStore`, `fence::FenceStore` |
| Journal | `journal::Journal`'s public methods, `journal::JournalConfig` |

**3. The freeze starts *after* D20.** [D20](0020-journal-retention.md) changed
`CheckpointTarget::checkpoint` and `CellRuntime::checkpoint_shard` to return the
watermark they wrote, and added retention methods to `Journal` and a field to
`CheckpointConfig`. Those land inside the freeze boundary rather than against
it, which is the practical reason this record is written now and not before it.

**4. What reopens the question.** Concrete demand, in one of these forms: a
title that must run rules it does not compile; an operational requirement to
ship a rules hotfix without a persistd redeploy that a rolling deploy cannot
meet; or a `RETAINED_BUILDS` horizon that proves too short in practice because
redeploys are too expensive to be frequent. Absent one of those, this is
settled for 1.0.

## Consequences

- **The game repo owns the persistd artifact**, and a rules change is a cluster
  deploy. Rolling deploys keep old builds alive for the adjudication retention
  horizon (three builds, D12); evidence older than that resolves as
  `Unadjudicable` — never a strike (D10).
- **`orrery_persistd` has a compatibility obligation it did not have
  yesterday.** Refactors that reach its public surface now cost an ADR. That is
  the intended price: the harness is an API for other people's code.
- **The WASM path stays available and stays unbuilt.** Nothing in the design
  forecloses it — the seam a sandboxed ruleset would implement is
  `orrery_core::Ruleset`, which is already the only thing the cluster calls.
  Reopening it means paying the determinism proof, not rearchitecting.

## Alternatives considered

- **WASM-sandboxed `Ruleset`.** Rejected for 1.0, on determinism and
  adjudication cost, for a scenario no launch title needs. Reconsidered only
  against the concrete demands listed above.
- **Freeze later, at P5 exit.** Rejected: the roadmap's argument holds — the
  cheap moment to freeze an API is before it has external consumers, and P2
  exit is the last such moment. Deferring buys nothing except more surface to
  freeze.
- **Do not freeze at all.** Rejected: "games link their own binary" is only a
  workable distribution model if the thing they link is stable. An unversioned
  harness makes every Orrery release a game-side port.
