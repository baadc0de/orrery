# A22 - A second engine in front of the same ruleset: sequencing Unreal, S5 and ECS

> Research node for the owner's stated direction: *"We will be moving from
> Regolith to first person 3D encounters, the next question is engine
> agnosticism... my next steps would be a Ruleset to bevy_ecs move, followed
> by an Unreal Engine integration plugin"*, refined in the same conversation
> to *"it can also not be a plugin, but a C++ bootstrap that uses Unreal
> (like a Game would) and integrates by APIs at runtime."*
>
> Repository facts verified at `0c01d4e` on 2026-08-30; every `path:line`
> below was read before being cited. **Propose, not decide** - section 7
> lists what stays with the owner. Nothing here amends D42 or any other ADR,
> and nothing here authorises an ECS adoption: D42 clause (d) trigger-gates
> that and no trigger has fired.

## 1. The decision that is already made

Engine agnosticism is not an open architectural question. **D42 clause (a)
is Accepted** and states the rule directly:

> canonical truth never lives in a Bevy application world. Application worlds
> hold only *mirrors* - presentation and replication components keyed by the
> `PersistId` component [...] **Any future host, on any engine, is a mirror
> consumer of engine-neutral canonical bytes.**

Unreal is therefore the already-sanctioned case, not a new one, and
`scripts/core-gates.sh` clause 1 enforces the boundary today by scanning
`GATED_CRATES=(orrery_core orrery_games orrery_conformance)` for Bevy.

What is missing is not a decision but a **seam**: D42 clause (b)(2)'s
`SimulationHost`, *"one kernel-owned driver owning tick advance, stable-id
lookup, command-in/event-out, and output collection - the loop all three of
today's hosts hand-roll"*. That is stage **S5** of A18's programme, whose
serial spine is four deep: **S1.a -> S4.1 -> S4.2 -> S5**.

## 2. The two goals are independent, and the stated order inverts them

| Goal | What it actually needs | Gate |
|---|---|---|
| A second engine in front of the ruleset | **S5**, the host seam | A18's serial spine |
| `bevy_ecs` behind the ruleset | **S7** | D42 (d) trigger: T1, T2 or T3 |

**An Unreal client does not need the ECS move.** S7 is a substrate swap
*behind* the seam; a second engine consumes the seam *in front* of it. D42
(d) admits a canonical `bevy_ecs::World` only when T1 (`CoreState`-as-one-
enum measurably stops scaling), T2 (the `BTreeMap` store dominating measured
tick cost) or T3 (A4's Tier-H gate bundle demonstrated at least as strong as
what it replaces) fires. None has. A18 records S7 as *"not specifiable
before entry"*.

Adding a second engine arguably **weakens** the ECS case rather than
strengthening it: it makes the engine-neutral byte boundary more
load-bearing, and T3 is a necessary precondition for any canonical byte to
move regardless.

## 3. First person is mostly skin, and the schema already anticipates it

The ruleset is already three-dimensional. `QPos` carries `z_m`
(`crates/orrery_games/src/regolith/state.rs:60`), and `Craft` carries both
`yaw_urad` and `pitch_urad`, the latter commented in place as *"Retained
schema field; input discipline locks it to zero"*
(`state.rs:208-210`).

So first-person encounters are principally a **skin and input** change plus
an encounter-design change, with one real ruleset change - unlocking pitch -
that costs a `REGOLITH_RULESET.version` bump and a digest change. The
project already bumps that freely (v16 -> v18 inside three days). No schema
migration is implied, because the field is already carried and replicated.

This is worth separating from the engine question entirely: it can ship on
the Bevy skin, on the existing playtest cadence, before any Unreal work
lands.

**Landed, #744.** Track C did exactly the above and nothing more. The lock
was one clause of the value-range invariant, `craft.pitch_urad != 0`; it is
now `craft.pitch_urad.abs() > PITCH_LIMIT_URAD`, against the ±π/2 limit the
step was already clamping to. `REGOLITH_RULESET.version` went 18 -> 19 and
the honest pilot now flies a small zero-mean elevation jitter, so the
four-platform matrix evaluates `sin`/`cos` off their exact points for the
first time. All four Regolith state-chain goldens moved; all eight Skirmish
chains did not, and neither did Regolith's `solo` *outcome* chain — `solo` is
the one-entity control, so no craft position reaches an event there. The
quoted comment above is therefore now historical: `state.rs` reads
*"Elevation, micro-radians, clamped to ±`PITCH_LIMIT_URAD`"*.

## 4. Plugin, bootstrap, or neither: what actually differs

The owner's refinement - a C++ bootstrap using Unreal as a game would,
rather than a redistributable plugin - changes less about the architecture
than it appears, because **Unreal is only ever a client here**. The
authoritative host stays `gates/p1-swarm`; no Unreal process holds canonical
state under D42 (a).

What must hold in either shape:

1. **Tick advance is a pure function of simulated time**, never of frame
   timing.
2. **The renderer never writes canonical state** - the owner's standing rule
   that the skin may interpolate but must assert nothing the ruleset has not.

Both are satisfiable whether Unreal's loop or our own owns the process. The
precedent is already in-tree: with a connected exterior, `Swarm::run` paces
itself against a fixed `tick_duration` and deliberately does not outrun the
peer (`gates/p1-swarm/src/swarm.rs`, the `real_time` branch). An Unreal
client runs that same accumulate-and-step inside whichever loop owns the
process.

**Recommendation, for the owner's judgement:** prefer *"our game that happens
to use Unreal"* over a redistributable plugin - there is no third-party
audience to owe API stability to - but default to **Unreal owning the process
loop**, with a fixed-step driver inside its tick, unless the spike in section
6 finds a blocker. Taking `main()` means reproducing `FEngineLoop`'s init and
tick and inheriting packaging, platform back-ends and content-cooking paths
that assume the engine's own entry point: a permanent tax to buy a property
an accumulator already provides.

## 5. The load-bearing consequence, and it is timing-sensitive

**This constrains S5's API shape, and S5 has not been built.**

If the seam is ever crossed from C++, `SimulationHost` must be expressible as
a **C ABI**: an opaque handle, an explicit `step(ticks)`, command-in and
event-out as flat buffers, no Rust-only types in the signature, and no
callbacks holding Rust lifetimes. D42 (b)(2) specifies the seam's
*responsibilities* but does not pin its ABI, so this is compatible with the
record rather than an amendment to it.

Designing S5 that way costs almost nothing now and is expensive to retrofit.
**That is the argument for running the spike before S5 is designed**, so its
signature answers to a real C++ call site instead of a guess.

## 6. Proposed tracks

| Track | Work | When | Touches Regolith |
|---|---|---|---|
| **A. Unreal spike** | Section 6.1 below | now, in parallel | no |
| **B. Serial spine** | S1.a -> S4.1 -> S4.2 -> S5 | playtest-quiet windows | yes - S4's entry is *regolith quiet* |
| **C. First person** | unlock `pitch_urad`, encounter tuning, camera and input on the Bevy skin | between playtests, versioned normally | yes, ruleset bump |
| **D. Unreal client** | the real thing, consuming S5 | after S5 | no |
| **E. `bevy_ecs` (S7)** | only on a fired D42 (d) trigger | last, or never | - |

### 6.1 What the spike must answer

Three questions, in order, each cheap and each able to invalidate the next:

1. **Can Unreal render live craft from `orrery_protocol` bytes with no Rust
   linked at all?** Proves the asset pipeline, the netcode and the feel.
   A deliberately dumb client: interpolate, assert nothing.
2. **With a fixed-step accumulator inside Unreal's tick, does the client
   reproduce host arithmetic exactly?** This is the question the bootstrap
   idea is really about. Only if this fails is owning `main()` worth its
   cost - and then the reason is known, with an artifact.
3. **What must the C ABI look like for prediction?** Feeds S5's signature
   directly, which is why this runs first.

> **Answered and landed, 2026-08-31 (#725, #744).** All three: decode with no
> Rust linked (`ldd` showed only libc and friends); the fixed-step accumulator
> reproduces host arithmetic **field-exactly** across fast, jittered and a
> forced 250 ms hitch (120 ticks, craft 7, `(76097, -22824, 5756)` mm); and the
> C ABI landed as `crates/orrery_sim/include/orrery_sim.h` (#727), which S5
> (#738) was designed against. **Section 4's recommendation is therefore
> checkable rather than an opinion: let Unreal own the loop.** A bootstrap
> owning `main()` buys lifecycle control, not deterministic arithmetic.
>
> Track D then took it further: **D.1** rendered the first frame, and **D.2**
> rendered four craft decoded live from a running `gates/p1-swarm`, with a
> receiver that refuses unanchored, superseded and malformed patches. Both are
> wireframe under Mesa llvmpipe; **appearance and performance remain
> unverified.**

> **Correction to the wire cost, 2026-08-31 (D.2).** This plan and the spike
> report both describe the State datagram as singly tagged. **The live
> exterior wire is double-tagged**: `[TAG_STATE][TAG_STATE][TAG_REPLICATION*]`,
> because the peer stack wraps the replication envelope's own tagged message in
> an outer channel byte. `fixture_gen` calls `encode_replication` directly and
> so emits the single-tagged form; the first live decode failed outright with
> `packet 0: not replication traffic`.
>
> The lesson is larger than the byte. A C++ decoder built and tested against a
> fixture was **wrong about the real wire, and nothing caught it** - which is
> precisely the hazard the "wire is not self-describing" cost names. **A fixture
> that does not travel the live encoding path is not a fixture for the live
> path**, and that argues for the cross-language fixture test earlier than its
> position in track D suggests.

## 7. Reserved to the owner

1. **Whether to do this at all**, and in what order against A18's programme.
2. **Plugin versus bootstrap versus Unreal-owned loop** - section 4
   recommends, and the spike is what makes the recommendation checkable.
3. **The scheduling collision.** S4's entry condition is *regolith quiet*,
   and a live playtest cadence is its opposite. A18 already reserves
   *"whether S1.a lands before #329's quiet point"* as item 9, calling it
   *"the single decision that shapes this epic"*; a second engine is now a
   third pull on the same four trees.
4. **Any ECS adoption** - D42 (d), unchanged and untouched here.
5. **Unlocking `pitch_urad`**, which changes canonical behaviour and the
   ruleset digest.
