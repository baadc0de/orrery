# ADR-0053: The Bevy client is one host and the Unreal client is another; what the C ABI already carries, and what it does not

**Status:** Proposed · **Date:** 2026-09-03 · **Decision:** D53

This record is non-normative until accepted. See the [ADR
index](../DECISIONS.md) for precedence, scope, and the complete accepted
decision set. Acceptance is reserved to the owner.

> **This record cites a pull request that is not merged.** Its requirement
> basis — [G10], [G10.2], [G10.3], [game ADR-0002] clauses 1–2 — lives on the
> branch `docs/game-design-requirements`, open as **[#1021]** at the time of
> writing. **This record cannot be accepted before #1021 is.** Every
> `game/docs/...` link below is dead until that merge, deliberately: they are
> the citation, not a convenience. If #1021 lands changed, re-read clauses (a)
> and (c) against the merged G10 before accepting; if it is closed unmerged,
> this record is withdrawn rather than re-argued.

**Citation convention.** As with [D52]: an Orrery change made to satisfy a
`G`-numbered game requirement is an Orrery ADR citing the G number. This is the
second of the two records [game ADR-0002]'s "Consequences for Orrery" asks for.

**Supersedes on acceptance:** nothing, and it **amends no accepted record's
normative text** — [D4]'s included. It **scopes** [D4]: D4 chose a stack, and
this record records which host that stack binds. Every sentence of D4 stays
true of the host it was written about. [D42] clause (b)(2) already contemplated
more than one driver on one seam and named three, one of them Unreal
(`0042-canonical-simulation-architecture.md:253-259`); this record does not
amend that clause either, and clause (f) below restates what it forbids so that
acceptance cannot be read as loosening it.

**Out of scope, each with its own owner:**

- **Whether to build the Unreal client at all, and when.** [#744] is open and
  explicitly propose-only; [A22] §7 item 1 reserves it. This record describes a
  seam, not a schedule, and starts no work.
- **The season cook and its dual output** ([G10.4], [D21]) — a distribution
  decision, not a host decision, and a separate record.
- **The macro service** ([G4.20]), **the materialise/fold contract** ([G8]),
  **viewer-dependent replication** ([G7.8]) and **mothership-scale interest
  management** — the other four follow-ups [game ADR-0001] names.
- **Platforms**, which are [D52]'s. Clause (c) item 8 notes one place where the
  two records touch, and defers.
- **`pitch_urad`**, owner-reserved at [A22] §7 item 5; it changes canonical
  behaviour and the ruleset digest and belongs to no host.
- **ECS adoption.** [D42] clause (d) trigger-gates it; nothing here fires a
  trigger or asks for one.

## Context

### 1. [D4] named a stack, and was never asked which host runs it

[D4] adopts `aeronet` 0.21 with an iroh IO layer, `bevy_replicon` 0.42 and
`lightyear` 0.29, and says what each is for. It is a **dependency** decision.
It contains no sentence about *which client process* those crates live in,
because in August 2026 there was one answer and the question had no content.

That stack is genuinely live, not aspirational. `crates/orrery` depends on
`aeronet_io` and `aeronet_iroh` (`Cargo.toml:38-39`) and on `lightyear`
(`:45`); `orrery_predict` is described in its own manifest as "the only
[crate] whose internals name lightyear types" (`crates/orrery_predict/Cargo.toml:8-12`,
`crates/orrery_predict/src/lib.rs:1-31`); `bevy_replicon` is vendored under
`vendor/bevy_replicon` (root `Cargo.toml:104-108`); and the shipping playtest
client installs it — `clients/regolith/src/main.rs:3` imports
`orrery_predict::OrreryPredictPlugin`, and `:490` notes that the plugin
installs lightyear's state-backed resources.

**Every one of those seams is a `bevy_app::App` seam.** They are Bevy
`Plugin` implementations and nothing else: `OrreryPredictPlugin`
(`crates/orrery_predict/src/plugin.rs:27-28`, systems landing in `FixedLast`
and `PostUpdate` at `:56-57`), `OrreryReplicationBridgePlugin`
(`crates/orrery_predict/src/bridge.rs:47-48`), `OrreryNetPlugin`
(`crates/orrery_net/src/plugin.rs:83-84`, opening the iroh endpoint and
accepting sessions), `CoordinatorPlugin` (`crates/orrery_net/src/coordinator.rs:274-275`),
the `orrery_replicon` facade (`crates/orrery_replicon/src/lib.rs:1-8`), and the
authority, spatial, witness and persist-client plugins beside them.

### 2. The seam already anticipated a second host, and only the first exists

[D42] clause (b)(2) is explicit: *"The Bevy client, the future field host, and
any Unreal sidecar drive the same host API"*
(`0042-canonical-simulation-architecture.md:253-259`). One of those three
drivers exists. `orrery_field_host` is **not built** — `README.md:38,40`,
`0029-low-population-path.md:47` ("There is no `orrery_field_host` directory
under `crates/`"), `docs/10-crates.md:83` ("planned P6, not built"),
and [A9], which records that the seam landed as `orrery_sim_host` (#738) with
`EcsBackend` (#757) but that "no field host is built on the seam yet"
(`docs/plans/a9-engine-boundaries.md:67`) — and no Unreal driver exists either.

Note the word D42 used: **sidecar**. [game ADR-0002] clause 2 chooses
in-process instead and demotes the sidecar to a named fallback. That is a
change of shape at the same seam, and clause (e) is about whether the evidence
for it exists yet.

### 3. What `orrery_sim_host` actually exports, checked symbol by symbol

Thirteen generic `extern "C"` entry points, plus one factory each game supplies
itself. Creation is deliberately **not** a generic symbol: `export_handle`
(`crates/orrery_sim_host/src/abi.rs:258-261`) is a Rust-generic helper, and the
only concrete factory in the tree is `orrery_synthetic_host_create`
(`crates/orrery_sim_host/examples/synthetic_abi.rs:24-29`). The convention is
documented at `crates/orrery_sim_host/include/orrery_sim_host.h:12-13`.

| Concern | Symbol | Evidence |
|---|---|---|
| ABI version | `orrery_host_abi_version` | `src/abi.rs:361-364`, const `ABI_VERSION = 1` at `:55`, header `:45` |
| Ruleset identity | `orrery_host_ruleset_id` | `src/abi.rs:389-393`; `{u32 version; u8 digest[32]}`, header `:63-66` |
| Lifecycle (destroy) | `orrery_host_destroy` | `src/abi.rs:372-373`; accepted on a poisoned handle, `:377-379` |
| Clock read | `orrery_host_next_tick` | `src/abi.rs:413-417` |
| **Input submission** | `orrery_host_submit_command` | `src/abi.rs:434-439`; `[target PersistId u64 LE][canonical input bytes]`, `:427-428` |
| Mirror application | `orrery_host_install_state`, `orrery_host_remove_state` | `src/abi.rs:458-465`, `:482-486` |
| **Step** | `orrery_host_step` | `src/abi.rs:505-511`, `(host, ticks, out_first_tick, out_next_tick)` |
| Hash drain | `orrery_host_drain_state_hashes` | `src/abi.rs:527-533` |
| **Event read** | `orrery_host_drain_events` | `src/abi.rs:559-565`; `[source u64 LE][len u32 LE][event bytes]` |
| **Whole-population read** | `orrery_host_collect_states` | `src/abi.rs:587-593`; header `:138` says in terms "a renderer may read it after every frame" |
| Single-entity read | `orrery_host_state` | `src/abi.rs:606-612` |
| Rewind | `orrery_host_snapshot`, `orrery_host_restore` | `src/abi.rs:630-636`, `:649-654`; format at header `:154-167` |

Four properties of that surface matter more than the symbol list, because they
are the ones expensive to retrofit and all four are already paid for:

- **No allocator crosses the boundary.** There is no `orrery_host_free`. Every
  byte-returning call copies into caller storage (`copy_records`,
  `src/abi.rs:326-355`), and on `capacity < required` it writes `out_required`,
  **writes nothing and drains nothing** (`:344-347`, header `:30-35`).
- **Panics are contained.** `with_host` and `read_host` wrap in
  `catch_unwind` (`src/abi.rs:280-299`, `:301-317`); a panic in a mutating call
  poisons the handle (`:295`) and every later call but destroy returns
  `Poisoned` (`:287-289`). The rationale is stated at `:34-40`.
- **The host never reads a clock.** `src/lib.rs:6-9`, header `:113-116`. The
  accumulator lives in the caller — which is what makes an Unreal-owned process
  loop admissible at all, and what [A22] §4 recommends.
- **No `bevy_ecs` type escapes.** The crate depends on `bevy_ecs` but not on
  `bevy_app` outside dev-dependencies (`Cargo.toml:8-11`, `:20-23`), and
  `EcsBackend`'s world is stated to be unreachable from any `App`:
  *"No `bevy_app::App` touches it, no plugin registers against it, no renderer
  reads it… a `bevy_ecs::Entity` never escapes"* (`src/ecs.rs:48-56`).

There is a cross-language proof, and it is real: `tests/c_consumer.rs` builds
the cdylib, asserts every required symbol with `nm -D --defined-only`
(`:70-102`), compiles a 655-line C consumer under `-std=c11 -Wall -Wextra
-Werror` (`:105-125`), and runs five tests including a variable-rate C-side
accumulator with a forced hitch that must reproduce one Rust
`step(TickCount::new(120))` field-exactly and hash-for-hash (`:177-178`), a
snapshot/step/restore/replay identity (`:228-229`), the drain-nothing contract
(`:306-307`), and a panic crossing the boundary as `PANIC` then `POISONED`
then a clean destroy (`:378-379`).

### 4. [G10]'s own consequence bullet, checked against that surface

[G10]'s engineering consequences say: *"`orrery_sim_host` already exports step,
snapshot and restore across a C ABI; what is missing is the network client and
prediction loop behind the same kind of surface (today they are Bevy plugins in
the sidecar). The plugin needs: connect, submit input, step, read mirror
frames, read events, plus lifecycle."*

The **second** sentence is exactly right and is the load-bearing one. The
**first** understates what exists and the **third** over-counts what is
missing, and getting this wrong in either direction costs real scheduling:

| G10's item | State | Evidence |
|---|---|---|
| connect | **missing entirely** | no transport dependency (`Cargo.toml:8-11`); no `connect`, session, or socket symbol anywhere in the crate |
| submit input | exists | `orrery_host_submit_command`, `src/abi.rs:434-439` |
| step | exists | `orrery_host_step`, `src/abi.rs:505-511` |
| read mirror frames | exists **as canonical bytes**, not as frames | `orrery_host_collect_states` / `orrery_host_state`, `src/abi.rs:587-612` — see clause (d) |
| read events | exists | `orrery_host_drain_events`, `src/abi.rs:559-565` |
| lifecycle | exists, with a caveat | `orrery_host_destroy` `:372`; **creation is game-supplied by convention**, header `:12-13` |

So of G10's six, **one is absent, four are present, and one is present in a
different currency than the bullet implies.** What is genuinely missing beyond
`connect` is not in that list at all: it is the **prediction loop** — the thing
that decides *when* to snapshot, *when* to roll back, and *what to reconcile
against* — and everything the network client does around it.

### 5. The rollback primitive is present; the rollback driver is not

`HostSnapshot` is documented as *"the per-entity set D47 (b) names as the
rollback unit"*, with queued inputs carried inside it because the host itself
produces some of them (`crates/orrery_sim_host/src/lib.rs:184-200`), and
`restore` states the guarantee prediction needs: replaying the same
post-snapshot commands reproduces the same state hashes and output bytes
(`:711-722`). [D47]'s unit and [D8]'s loop are therefore *expressible* across
the ABI.

Nothing drives them. The reconciliation policy, the ring, the correction
intake, and the budget live in `orrery_predict` as a lightyear configuration
layer behind a Bevy plugin, and **no code connects `orrery_predict` to
`orrery_sim_host`.** The doc-comments assert the property; the wiring does not
exist.

## Decision

### (a) [D4] is scoped, not amended: one stack, one host, and a second host it does not reach

**The Bevy client is one host. The Unreal client is another.** [D4]'s
`aeronet` → `bevy_replicon` → `lightyear` stack is normative **for the host
that is a Bevy `App`** — the Regolith client, `orrery_sidecar`, and the future
field host. It is not normative for, and does not reach, a host that is not a
Bevy `App`.

This is a scoping, not a reversal, and three things follow from saying it that
way:

1. **[D4]'s risk paragraph still binds where it always did.** Its accepted
   risks — lightyear's API churn, single-maintainer bus factor — attach to the
   Bevy host and are unchanged. This record does not use a second host as an
   excuse to reopen them.
2. **A second host is not a licence to reimplement the stack's job.** Whatever
   drives an Unreal host must satisfy the same [D8] prediction contract, the
   same [D47] rollback unit and the same [D42] clause (a) authority rule, or it
   is a different architecture and needs its own record.
3. **[D4] is not deprecated by the existence of [G10].** The Bevy client is the
   client that ships today ([D52] §4), and nothing here schedules its
   retirement — a question [D52]'s last section records as open.

### (b) The seams that carry over, and they are more than the requirement claims

Named so that a future lane does not rebuild them: the thirteen generic
symbols of Context §3; the flat `[id u64 LE][len u32 LE][bytes]` framing
(`src/abi.rs:11-12`, header `:17-23`), which does not change when a game adds a
field to its state; caller-owned `(out, capacity, out_required)` copy-out with
no allocator handoff; `catch_unwind` plus handle poisoning; the two orthogonal
version axes (`orrery_host_abi_version` for the runtime surface,
`orrery_host_ruleset_id` for the rules identity — the split [A9] `:292` asked
for); the clock-free `step(ticks)` contract with the accumulator in the caller;
and snapshot/restore-with-queued-inputs as [D47]'s rollback unit expressed in
C.

Together these are a **complete single-process offline simulation surface**.
For a C++ caller that wants to install state, submit input, step, read state
and events, and rewind, nothing is missing.

### (c) What is missing, stated as work and not as aspiration

1. **Connect and session lifecycle.** No transport symbol exists and no
   transport dependency exists. Everything the peer link does — iroh endpoint,
   session accept, coordinator membership — is `OrreryNetPlugin` and
   `CoordinatorPlugin` (`crates/orrery_net/src/plugin.rs:83-84`,
   `coordinator.rs:274-275`).
2. **Authority correction intake and the rollback driver.** Context §5. The
   primitive exists; the policy does not cross.
3. **A spawn/despawn stream.** The ABI offers whole-population
   `collect_states` and per-entity `state`; there is no "these entities
   appeared, these left" event. A renderer that diffs the whole population
   every frame is a workable but different design, and it should be a chosen
   one.
4. **Interpolation and AOI/interest.** Both Bevy-plugin-shaped today
   (`orrery_spatial`'s visibility plugin; lightyear's interpolation inside
   `orrery_predict`). [G10.3] makes presentation smoothing Unreal's business,
   but *which entities a client is told about* is not presentation.
5. **The hit-claim path.** `orrery::hit` is a Bevy plugin surface
   (`crates/orrery/src/hit.rs:226`); [G10.3] requires the ruleset to adjudicate
   hit registration, so this crosses or the requirement is not met.
6. **A shippable library for a real ruleset.** `orrery_sim_host` declares no
   `[lib] crate-type`, so it is a plain **rlib**; the only `cdylib` in the
   crate is the `synthetic_abi` **example** (`Cargo.toml:16-18`). [G10.2] says
   "Rust **static library**", and no `staticlib` artifact is declared anywhere.
   This is small work and it is not done.
7. **A generated header.** `include/orrery_sim_host.h` is hand-written; a
   repository-wide search for `cbindgen` returns nothing. The Rust
   `ABI_VERSION` const (`src/abi.rs:55`) and the header `#define` (`:45`) are
   held in step by a comment and by `tests/c_consumer.rs` compiling against the
   header. That is a real mechanism, but it is one platform's mechanism —
   see item 8.
8. **Non-Linux proof of the C surface.** `tests/c_consumer.rs` hardcodes
   `libsynthetic_abi.so` (`:75`), shells out to `nm -D` (`:71`), and emits a
   GNU-ld `-Wl,-rpath` flag (`:107`); it carries no `#[cfg]` and no `#[ignore]`,
   so on Windows it fails rather than skips. [G10] calls per-platform toolchain
   ABI compatibility (MSVC on Windows, clang on Linux) *"a build-system task,
   not a design one"* — which this record accepts as a statement about
   difficulty, while recording that **it is unproven on the platform [G10.5]
   and [D52] make first.** Establishing it is cheap; assuming it is not free.

### (d) The mirror surface is a choice this record puts to the owner rather than makes

State crosses the existing ABI as **canonical bytes** — the same bytes the
kernel commits to. `src/abi.rs:9-32` records why, and the reasoning is sound
and should not be relitigated: a fixed projection struct puts a game's field
names in the header, a projection callback needs a C-to-Rust re-entry for no
gain, and a typed column schema drifts from `CoreCodec`. The consumer instead
writes one thing per state type — a C++ mirror of its own `CoreCodec::decode`.

The cost of that is recorded honestly in [#744]: *"The wire is not
self-describing… Any schema or layout change silently breaks the C++ decoder,
and nothing in CI would notice today."* [#744] calls the cross-language fixture
test its load-bearing lane for exactly this reason, and recommends deciding
between a committed fixture test and a versioned presentation DTO **before**
a client depends on raw offsets, because retrofitting the DTO afterwards is the
expensive order.

**A presentation DTO already exists, unused.** `crates/orrery_ipc` is an
engine-neutral codec — *"defines messages and their byte encoding, not a
transport"*, `orrery_protocol` its only dependency (`src/lib.rs:1-8`), with its
own `IPC_SCHEMA_VERSION` independent of `PROTOCOL_VERSION` (`:23`). Its
vocabulary is precisely the missing one: `EngineToSidecar` carrying
`EntityInput`/`InputBatch` (`:293`, `:304`, `:313`), and `SidecarToEngine`
carrying `FrameBatch` of `EntityFrame`/`QuantizedTransform` (`:378`, `:394`,
`:405`), `SpawnBatch` (`:416`), `DespawnBatch` (`:423`) and
`CorrectionBatch`/`CorrectionNotice` (`:435`, `:444`). Frames are produced
every tick by `crates/orrery/src/ipc.rs` (message at `:137`, written at
`:271-289`, plugin at `:324-332`) — and read back only by a test
(`crates/orrery_sidecar/tests/extract.rs:46`). **Nothing depends on
`orrery_ipc_transport` but its own bench binary; the frames are never put on a
socket in production.**

The options are in §"Options" below. This record recommends but does not
choose.

### (e) In-process versus sidecar is not yet settled by evidence, and this record will not pretend otherwise

[game ADR-0002] rejects the out-of-process sidecar as the primary path and
keeps it *"as the fallback if in-process latency or crash containment proves
unacceptable"*. The measurement that was supposed to decide this exists, and it
does not yet answer the question that was asked.

[#920] defines the threshold precisely: `ipc_added` at N = 24 at 60 Hz **on
Windows** over TCP loopback with `TCP_NODELAY`, the *worst reasonable*
candidate. Sidecar stands at p99 ≤ 1 ms and p99.9 ≤ 4 ms; sidecar is overturned
at p99 ≥ 16.7 ms (one tick — the input delay the wiring explicitly refuses to
spend, `maximum_input_delay_before_prediction: 0`) or p50 ≥ 1 ms; between them
is the owner's call. A scaling clause asks for p99 ≤ 2 ms at N = 128.

What is in the tree is **Linux only**, two runs of 36,000 samples over ten
minutes (`docs/data/sidecar-ipc-linux-2026-09-03-n24.json`, `…-n128.json`):

| Run | `ipc_added` p50 | p99 | p99.9 | max |
|---|---|---|---|---|
| N = 24 | 41.7 µs | 70.9 µs | 198 µs | 5.19 ms |
| N = 128 | 47.8 µs | 384 µs | 4.14 ms | 24.5 ms |

Those clear the stand band and the scaling clause comfortably — **and they are
informational by construction.** `nightly.yml`'s `sidecar-ipc-windows` job
records that `scripts/ipc-report.py` refuses a verdict for any platform but
Windows, and no Windows report exists under `docs/data/`.

Therefore: **the in-process decision of [G10.2] rests on [game ADR-0002]'s
judgement, not on the number [#920] was written to produce.** Both may be
right; a Windows run at N = 24 is one nightly job away from saying so. Clause
(e) does not overturn [G10.2] — it is not this record's to overturn — but a
record that cited [#920] as settled would be citing a measurement that has not
been taken on the platform it was defined for.

### (f) What an Unreal host may not do, restated so acceptance cannot loosen it

None of this is new; it is repeated because a second host is precisely when
these get quietly relaxed.

1. **No Unreal process holds canonical state** ([D42] clause (a); [#744]'s
   standing constraints). Unreal is a client and a mirror consumer.
2. **The skin may interpolate but must assert nothing the ruleset has not.**
   [G10.3] agrees on its own terms: Unreal replication,
   `CharacterMovementComponent` and Chaos are presentation.
3. **No `Ruleset` lives in a crate with Bevy in its graph** ([D42] clause (a),
   [D43] clause (e)(1)), enforced by name in `scripts/core-gates.sh`. An
   in-process host changes nothing about this.
4. **No `bevy_ecs` type crosses the C ABI or the IPC boundary** — held today
   (`crates/orrery_sim_host/src/ecs.rs:48-56`), and a condition of any
   extension made under clause (c).
5. **No ECS adoption is implied.** [D42] clause (d) is untouched.

## Options for the owner

**On the mirror surface (clause (d)) — the decision [#744] asks to be taken
before a client depends on byte offsets:**

- **M1 — Canonical bytes only, plus a committed cross-language fixture test.**
  Extend the existing ABI; keep `collect_states` as the mirror read. *For:* no
  second encoding to maintain, no drift between two descriptions of the same
  state, and the machinery already exists (`tests/c_consumer.rs` test 5 is
  exactly this shape). *Against:* every C++ consumer re-implements
  `CoreCodec::decode`, and the presentation layer reads canonical bytes it has
  no business depending on — the coupling [#744] calls out.
- **M2 — Canonical bytes for the simulation path, `orrery_ipc` frames for the
  presentation path, both behind the same C ABI. (Recommended.)** Prediction,
  adjudication and rewind keep canonical bytes; the renderer reads
  `FrameBatch`/`SpawnBatch`/`DespawnBatch`/`CorrectionBatch`. *For:* it is
  [#744]'s "versioned presentation DTO", it is already written and already
  Bevy-free, it carries its own independent schema version, and it supplies the
  spawn/despawn stream clause (c) item 3 says is missing. *Against:* two
  encodings of one world, so clause (f) item 2's boundary has to be enforced
  rather than assumed; and `orrery_ipc` has no production consumer today, so
  adopting it is also the act of first proving it.
- **M3 — A typed projection in the header.** Rejected in `src/abi.rs:16-18`
  when the retired `orrery_sim` did it: adding a game field changes the ABI.
  Listed to be rejected explicitly.

**On the host shape (clauses (a) and (e)):**

- **H1 — In-process now, as [G10.2] states**, with the network and prediction
  driver built behind the C ABI. *For:* it is the requirement; it removes a hop
  and a process to supervise. *Against:* the deciding Windows number does not
  exist (clause (e)), and it means building a non-`App` driver — see H3.
- **H2 — In-process, gated on the Windows [#920] run. (Recommended.)** Accept
  [G10.2] as the direction and let the one missing nightly report either
  confirm it cheaply or hand the owner the fallback with a graph rather than a
  sentence. The job exists; nothing has to be built to get the number.
- **H3 — Sidecar first, in-process later.** *For:* `orrery_sidecar` and
  `orrery_ipc_transport` exist, and crash containment is free. *Against:*
  [game ADR-0002] already rejected it as primary, and the Linux numbers do not
  argue for reopening that.

**A fork H1 and H2 share, and it should be decided consciously:** [G10.2] asks
for *"Bevy headless inside the game process"*. The seam that exists is
`bevy_ecs` **without** `bevy_app` (`Cargo.toml:8-11`), with a world explicitly
unreachable from any `App` (`src/ecs.rs:48-56`) — while the entire network and
prediction stack is `bevy_app`-shaped (Context §1). Satisfying G10.2 in full
therefore means either **running a real headless Bevy `App` inside the Unreal
process** beside the ABI handle (cheap in code, and it drags a plugin graph, a
schedule runner and lightyear into the shipped client process), or **building a
non-`App` network and prediction driver behind the ABI** (a clean process, and
it duplicates what [D4]'s stack does for the Bevy host). This record does not
choose. It is the largest single question the Unreal host poses, it is not
answered by [G10.2]'s wording, and it should not be answered by whichever lane
starts first.

## Consequences

- [D4] gains a scope it never had, and the Bevy host keeps every commitment D4
  made about it. Nothing in `docs/03-replication.md` or
  `docs/05-prediction-rollback.md` becomes false.
- [D42] clause (b)(2)'s "any Unreal sidecar" reads as one shape among the
  options in this record rather than as the settled one; the clause's normative
  content — one host API, storage behind the seam — is untouched and is what
  makes the alternatives expressible at all.
- Clause (c) is a work inventory, not a schedule. Accepting this record starts
  nothing; [#744] remains propose-only and [A22] §7 item 1 remains the owner's.
- If the owner takes M2, `orrery_ipc` acquires its first production consumer
  and its `IPC_SCHEMA_VERSION` (`src/lib.rs:23`) starts meaning something. If
  the owner takes M1, [#744]'s D.5 fixture lane becomes load-bearing and should
  be scheduled before D.2, as [#744] itself recommends.

## What this record could not establish

1. **The Windows `ipc_added` number.** Clause (e). The job exists
   (`nightly.yml`'s `sidecar-ipc-windows`), the threshold exists ([#920]), the
   report does not. Everything this record says about in-process versus sidecar
   is therefore an argument, not a measurement, and it is labelled as one.
2. **Whether `orrery_predict`'s rollback can in fact be driven from
   `snapshot`/`restore`.** `crates/orrery_sim_host/src/lib.rs:21` and `:184-200`
   assert the property and `tests/rewind.rs` exercises it in Rust, but no code
   connects the prediction layer to the host seam, so the claim is untested
   against the real reconciliation policy — the budget, the ring depth, the
   correction ordering. A spike would answer it; this record did not run one.
3. **Whether the C consumer test runs in CI at all, and where.** It carries no
   `#[cfg]` and no `#[ignore]`, and its Linux/ELF assumptions are structural
   (`:71`, `:75`, `:107`). Whether `./scripts/check.sh test` reaches it on a
   non-Linux runner was not established here, and clause (c) item 8 is written
   to be true either way.
4. **The Unreal-side cost of clause (c).** Every estimate in this record is
   about the Rust side of the boundary. What items 1–5 cost in C++ inside a UE
   5.8 plugin — a `UGameInstanceSubsystem` owning the accumulator beside the
   opaque handle, as [#744] D.3 proposes — is not measured anywhere, and the
   spike behind [#744] reached map load headless but **never displayed a
   frame** (its sandbox's Zen/DDC path was read-only). The cheapest thing that
   can still fail is therefore still unproven.
5. **Whether [G10.2]'s "Bevy headless inside the game process" was intended as
   a full `App` or as the `bevy_ecs` backend that exists.** The requirement
   text does not distinguish them and the distinction is load-bearing — it is
   the fork at the end of §"Options". This is the one question this record most
   wants the owner's answer to, and it is a question about [#1021]'s wording,
   which is why it must not be resolved before #1021 merges.

[#744]: https://github.com/baadc0de/orrery/issues/744
[#920]: https://github.com/baadc0de/orrery/issues/920
[#1021]: https://github.com/baadc0de/orrery/pull/1021
[D4]: 0004-bevy-netcode-stack.md
[D8]: 0008-prediction-rollback-interpolation.md
[D21]: 0021-ruleset-distribution.md
[D42]: 0042-canonical-simulation-architecture.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
[D47]: 0047-rollback-unit.md
[D52]: 0052-client-platform-scope.md
[A9]: ../plans/a9-engine-boundaries.md
[A22]: ../plans/a22-engine-agnostic-client.md
[G10]: ../../game/docs/00-requirements.md#g10--client-engine-and-content-pipeline
[G10.2]: ../../game/docs/00-requirements.md#g10--client-engine-and-content-pipeline
[G10.3]: ../../game/docs/00-requirements.md#g10--client-engine-and-content-pipeline
[G10.4]: ../../game/docs/00-requirements.md#g10--client-engine-and-content-pipeline
[G10.5]: ../../game/docs/00-requirements.md#g10--client-engine-and-content-pipeline
[G4.20]: ../../game/docs/00-requirements.md
[G7.8]: ../../game/docs/00-requirements.md
[G8]: ../../game/docs/00-requirements.md
[game ADR-0001]: ../../game/docs/adr/0001-requirements.md
[game ADR-0002]: ../../game/docs/adr/0002-client-engine.md
