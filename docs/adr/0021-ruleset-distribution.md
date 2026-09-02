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

## Proposed amendment, 2026-09-02 (#880): the retention horizon is one build, not three

> **Status: Proposed. Nothing below is normative until the owner accepts it.**
> The accepted text above stands unchanged, and
> `RETAINED_BUILDS`(`crates/orrery_persistd/src/adjudication.rs:35`) still reads
> `3`. This record exists because #880 built the registration seam and, in
> building it, established that the horizon D12 and the Consequences bullet
> above promise cannot be reached by the architecture as it stands.
>
> ### The contradiction, stated against the code
>
> The Consequences bullet above says *"rolling deploys keep old builds alive for
> the adjudication retention horizon (three builds, D12)"*. Three things have to
> be true for that sentence to describe a running cluster. Only the first is.
>
> 1. **The executor can hold three builds.** True.
>    `AdjudicationExecutor::register` pushes onto a `VecDeque` and retires the
>    oldest past the bound (`adjudication.rs:418-428`); `retained()` reports
>    what is held (`adjudication.rs:431-433`). The data structure is honest.
>
> 2. **A process can *register* three builds.** False as composed. A build is
>    registered by naming a concrete Rust type at link time —
>    `executor.register(|| orrery_conformance::Reference)`
>    (`crates/orrery_persistd/src/bin/persistd.rs:2737`) is the only call in a
>    deployable artifact, and it is one call. One binary links one version of
>    one rules crate, so one process registers **one** build. Reaching three
>    means linking three renamed crate versions (`orrery_games_v1`,
>    `_v2`, `_v3`) into a single artifact — three copies of the rules and their
>    transitive graph, coexisting, each with a distinct `RulesetId` — which is a
>    packaging decision D21 never took and nothing in the tree prepares.
>
> 3. **A report pinned to an old build reaches the process that still holds
>    it.** False. `gateway_report` adjudicates in-process against the executor
>    installed at `bin/persistd.rs:1436`; there is no routing tier that reads a
>    report's `RulesetId` and forwards it to a peer holding an older build. A
>    rolling deploy keeps old *processes* alive, which is what D21 says, but the
>    report goes to whichever gateway the witness is connected to, not to the
>    one that can judge it.
>
> So the deployed behaviour of "three retained builds" is: during a rollout,
> evidence pinned to the superseded build lands on a new-build process, misses
> its registry, and returns `Unadjudicable(UnknownRuleset)` — never a strike
> (D10). The horizon is 1, and the extra 2 are an unimplemented promise, not a
> latent capacity.
>
> ### Proposed decision
>
> **`RETAINED_BUILDS` becomes 1, and the rollout gap becomes a stated limit
> rather than a silent one.** The architecture is one process, one registered
> build; the constant should say so. Evidence pinned to a superseded build is
> `Unadjudicable` **by name** for the duration of a rollout — a bounded,
> documented, never-a-strike outcome (D10), visible in the existing
> `refused_no_adjudicator`/verdict counters rather than hidden behind a number
> that reads like slack the cluster does not have.
>
> The two ways to buy D21's literal promise were weighed and are rejected as
> disproportionate:
>
> - **Link three renamed crate versions.** Buys real multi-build adjudication,
>   and costs a permanent packaging obligation on every game team: three rules
>   graphs in one artifact, each pinned, each compiled, for a window measured in
>   minutes per deploy.
> - **Route reports across processes by `RulesetId`.** Buys the same, and costs
>   a new inter-gateway routing tier on the evidence path — new failure modes on
>   the one path whose entire value is that its answers are reproducible.
>
> Both spend a standing architectural cost to close a transient rollout window
> in which the correct answer is already safe.
>
> ### What acceptance would change
>
> - `RETAINED_BUILDS: usize = 1` (`adjudication.rs:35`), and its doc comment,
>   which currently explains the retirement of a *fourth* registration.
> - The Consequences bullet above: *"three builds, D12"* becomes one build, with
>   the rollout window named.
> - D12's three-retained-builds clause, which is the other half of the same
>   promise and cannot be amended from here.
> - Not the seam: `register`, `retained` and their signatures are frozen surface
>   and are unchanged by this. The bound is a number the seam enforces, not part
>   of the seam.
>
> ### What this amendment does *not* decide
>
> Where a production `Ruleset` registration lives — a persistd Cargo feature or
> a binary in the game crate — is deferred past the content freeze by the
> owner's 2026-09-02 note on #880, and is untouched here.
