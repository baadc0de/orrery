# Spike #1045 — moving interiors: what ran, and the numbers

**Research spike, not shipped code** (#1042 rule 6). Nothing here merges;
nothing here accepts, amends or pre-empts D52/D53 (both Proposed, #1022).
It settles evidence for **G4.1** (ship-to-space and mothership-to-ship legs)
and **G11**'s two nesting levels; it measures **G10.3** ("CMC is
presentation") as a count. Every number below carries the actor that
produced it (#1042 rule 3).

Built on **spike #1052's non-`App` prong** (`crates/orrery_unreal_direct`,
PR #1066): the generic `orrery_host_*` ABI, one factory, no Bevy `App`, and
that prong's 272-line rollback driver lifted into a header both the C
consumer and the Unreal module include. Why that prong and not #1043's
`App`: the question here is rollback over *canonical* state across a frame
change, and only the non-`App` prong has a driver that reaches canonical
state (`crates/orrery_unreal_direct/README.md:44-77`; #1043's README, "The
two handles are **not connected**"). The `App` prong's lightyear rollback
rolls back Bevy components, which this spike's Unreal side does not have.

## What was built

| piece | where | what |
|---|---|---|
| a throwaway nested-frame ruleset | `crates/orrery_unreal_interiors/src/rules.rs` | station (fixture), ship, mech, avatar; every body steps in its **frame's** integer mm lattice; `Enter`/`Leave` are the §13.3 crossings as one exact transform (teleport-class = the zero-relative-velocity case) |
| the staticlib + factory | `crates/orrery_unreal_interiors/src/host.rs`, `include/` | `orrery_interiors_host_create`, the scene population, beside the generic ABI — `liborrery_unreal_interiors.a`, 32.2 MB release |
| the shared C header | `crates/orrery_unreal_interiors/examples/c/interiors_shared.h` | codec mirror, the scripted scenes (where every command and every frame change is, by tick), #1052's ring/rollback driver, a stand-in authority |
| the C consumer | `crates/orrery_unreal_interiors/examples/c/interiors_consumer.c` | `trace` (a scene alone, hash chain out), `rollback` (client + authority in lockstep, one correction spanning every frame change, hash for hash) |
| the test | `crates/orrery_unreal_interiors/tests/c_consumer.rs` | builds the archive, compiles the consumer (`-std=c11 -Werror`), compiles the header as C++20, runs smoke/trace/rollback; `cargo test -p orrery_unreal_interiors` |
| the runnable map | `unreal/MovingInteriors` (UE 5.8, Linux) | `AInteriorsScenario` placed in `/Game/Maps/MovingInteriors`: builds station, ship, mech, avatar from the ruleset's own population, steps the host once per frame at a fixed 60 Hz, mirrors in per-grid local frames, walks the scene, writes CSV + JSON |
| the presentation variants | `InteriorsScenario.cpp`, `InteriorsCharacter.cpp` | `mirror` (b, the control: a scene component in the frame's local space), `cmc` (a: CMC on a moving deck, mirror pose written before it ticks), `cmc_nobase` (a, based movement disabled), `cmc_drive` (a, CMC driven by input and speed, pose never written) |
| the interior modes | same | `resident` (interior attached from BeginPlay), `spawn` (200 attached components created at boarding, destroyed on leaving), `stream` (a sub-level streamed at boarding, `bShouldBlockOnLoad`) |

Not built: any `FrameChange` protocol record; any change to `orrery_games`;
interest management; five levels. **This ruleset cannot claim replay
closure** — there is no `FrameChange` record binding the log to the new
basis (`crates/orrery_core/src/lib.rs:68-73` lists it as not yet here). What
it can claim is that a frame change stepped, hashed, snapshotted and restored
by `orrery_sim_host` unchanged is hash-exact under D47's per-entity set, and
that is the measurement.

## The scenes (60 Hz; `interiors_shared.h`, `interiors_script_at`)

- **rest / straight / roll** (36,000 ticks each): avatar walks 10 m up the
  station bay, boards the docked ship at tick 250 (teleport-class), the ship
  undocks at 300 and cruises from 301 — at rest, at 50 m/s, or at **500 m/s
  rolling 1454 µrad/tick (4.998°/s)**, D5's own worked number — while the
  avatar walks a 20 m × 3 m corridor loop at 2.4 m/s in ship coordinates.
- **mech** (36,000): as roll, and at tick 1550 the avatar mounts a mech that
  walks a 10 m × 2 m loop and turns at every corner, walking a 1 m square in
  its cockpit; dismounts at 35,400. **Avatar in mech in ship: the second
  nesting level.**
- **transitions** (24 cycles × 600 ticks = 14,400): per cycle board the
  docked ship, undock and cruise 50 m/s, EVA (continuous-class, velocity
  kept), board under way (continuous-class), dock, disembark — **144 frame
  changes, 24 of each kind**.

The station sits 100 km from the world origin (`STATION_X_MM`) so LWC's
contribution is visible; by the end of the roll scene the ship is 400 km out.

## Rollback across the frame change — the number

Method (`interiors_consumer.c`, `rollback`): client and a stand-in authority
run the same script in lockstep. For every frame change at tick `Tc`, one
correction is arranged with `Ta = Tc − (i mod 9)`, `now = Ta + 9` (D8's full
window), so the replay window spans `Tc` at every offset. Three shapes:
**identity** (the client's own bytes), **ship** (the authority applied a
`Cruise` on the ship at `Ta − 1` the client never saw: the frame the avatar
crosses into is not the one it predicted), **avatar** (a different `Move` on
the crossing entity at `Ta − 1`). The correction carries the authority's
bytes for all four bodies at `Ta`; the driver restores the ring slot,
installs, replays the client's logged inputs with ring rewrite; then every
`(entity, tick)` hash in `[Ta, now)` is compared with the authority's, and
again for up to 30 ticks after. Every correction is tagged with its
arrangement; nothing else corrects the client. Control corrections (shape
ship, depth 5) every 120 ticks away from any transition are the
non-spanning baseline.

**Result: 0 mismatches.** 144 corrections spanning a frame change (plus 47
controls) in the transitions scene, 2–4 spanning plus ~296 controls in each
of roll, straight and mech (the mount and dismount rows are the second
nesting level) — hash for hash, in the window and after, at depth 9, and the
divergent shapes are not vacuous: they change 63–120 hashes per correction
against the abandoned timeline and move the avatar up to 1.4 m. The falsifier
"D47's per-entity snapshot set does not capture the frame relation" is **not
hit**: `frame` in the crossing entity's own state plus the frame body's own
snapshot is enough for the host's restore to reproduce the crossing, because
the crossing reads the frame through `StateView::neighbor` from the
tick-start snapshot that the restore also reinstates.

D8 budget actually spent: restore + install + 9-tick replay of 4 bodies =
**19.6 µs p50, 23.3 µs p99, 28.4 µs max** (C, release, Linux, loadavg 0.7).
Inside the Unreal process, the same driver on the same corrections: see the
table below.

What rollback across a frame change costs the *presentation*: the replay
re-emits the `FrameChanged` event the abandoned timeline emitted (144 of 144
in the transitions scene; `events_reemitted_by_replay`). A presentation layer
that re-parented an actor on the first emission must key the second by
`(entity, tick)` or it re-parents twice. That obligation is #1052's
`events_reemitted_by_replay` finding, now with a concrete event.

The same host, the same script, in the Unreal process and in the C
consumer, produce the **same hash chain** for every scene (`chain = C run`
column): the ruleset stepped inside UE 5.8 is bit-identical to the ruleset
stepped from C.

## Tables — generated by `summarize.py` from the run outputs

Conditions: Linux, UE 5.8.2 editor binary in `-game`, `-UseFixedTimeStep
-FPS=60` (one 1/60 s engine frame per ruleset tick, Chaos stepping with
the frame), **`-NullRHI`** for every row below unless the row says
`offscreen` — no rendering, no GPU, no shader compilation. `frame_ms` is the
wall time between consecutive scenario ticks (`FPlatformTime::Seconds`), the
whole engine frame. Box: 32 cores, loadavg ≤ 1.2 during the runs.

### Drift (mm), per variant x scene — Unreal, per-grid local frame

`direct` is the relative transform the mirror holds minus the ruleset's local pose; `reproj` is the frame's Unreal world transform inverted over the mirror's world location, minus the same pose (where LWC enters); `cmc` is the capsule's frame-local position after CharacterMovementComponent's own update, minus the pose it was given.

| scene | variant | direct p50 / p99 / max | reproj p50 / p99 / max | cmc p50 / p99 / max | ticks | chain = C run |
|---|---|---|---|---|---|---|
| rest | mirror | 0 / 0 / 0 | 0 / 0 / 0 | — | 36000 | yes |
| rest | cmc | 0 / 0 / 0 | 0 / 0 / 0 | 24 / 322 / 636 | 36000 | yes |
| rest | cmc_nobase | 0 / 0 / 0 | 0 / 0 / 0 | 24 / 322 / 636 | 36000 | yes |
| rest | cmc_drive | 0 / 0 / 0 | 5.24e+03 / 5.28e+03 / 5.28e+03 | 5.24e+03 / 5.24e+03 / 5.24e+03 | 36000 | yes |
| straight | mirror | 0 / 0 / 0 | 0 / 0 / 0 | — | 36000 | yes |
| straight | cmc | 0 / 0 / 0 | 0 / 0 / 0 | 833 / 880 / 978 | 36000 | yes |
| straight | cmc_nobase | 0 / 0 / 0 | 0 / 0 / 0 | 24 / 322 / 636 | 36000 | yes |
| straight | cmc_drive | 0 / 0 / 0 | 5.3e+03 / 5.35e+03 / 5.35e+03 | 5.24e+03 / 5.24e+03 / 5.24e+03 | 36000 | yes |
| roll | mirror | 0 / 0 / 0 | 0 / 0 / 0 | — | 36000 | yes |
| roll | cmc | 0 / 0 / 0 | 0 / 0 / 0 | 8.33e+03 / 8.45e+03 / 8.48e+03 | 36000 | yes |
| roll | cmc_nobase | 0 / 0 / 0 | 0 / 0 / 0 | 22.8 / 242 / 723 | 36000 | yes |
| roll | cmc_drive | 0 / 0 / 0 | 1.34e+08 / 2.81e+08 / 2.84e+08 | 1.34e+08 / 2.81e+08 / 2.84e+08 | 36000 | yes |
| mech | mirror | 0 / 0 / 0 | 0 / 0 / 0 | — | 36000 | yes |
| mech | cmc | 0 / 0 / 0 | 0 / 0 / 0 | 8.33e+03 / 8.47e+03 / 8.5e+03 | 36000 | yes |
| mech | cmc_nobase | 0 / 0 / 0 | 0 / 0 / 0 | 22.8 / 342 / 789 | 36000 | yes |
| mech | cmc_drive | 0 / 0 / 0 | 1.23e+04 / 3.57e+05 / 3.36e+06 | 1.47e+04 / 3.57e+05 / 3.36e+06 | 36000 | yes |
| transitions | mirror | 0 / 0 / 0 | 0 / 0 / 0 | — | 14400 | NO (db14b958a4c0ee4f vs 9f3277ab8bf0bb4d) |
| transitions | cmc | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 449 / 1.03e+03 | 14400 | NO (db14b958a4c0ee4f vs 9f3277ab8bf0bb4d) |

### CMC verdict as a number — assertions per 36,000 ticks

An assertion is a tick on which the capsule, after CMC's own update, sits more than 1 mm from the pose the mirror wrote (variants cmc, cmc_nobase) or from the ruleset's pose (cmc_drive, where the mirror never writes the capsule).

| scene | variant | assertions | vertical-only | horizontal | with based-movement delta | ticks walking / falling / flying | base as expected |
|---|---|---|---|---|---|---|---|
| rest | cmc | 35845 / 36000 | 34977 | 868 | 0 | 36000 / 0 / 0 | 35440 |
| rest | cmc_nobase | 35845 / 36000 | 34977 | 868 | 0 | 36000 / 0 / 0 | 35440 |
| rest | cmc_drive | 36000 / 36000 | 1 | 35999 | 0 | 36000 / 0 / 0 | 33110 |
| straight | cmc | 36000 / 36000 | 301 | 35699 | 35699 | 36000 / 0 / 0 | 35130 |
| straight | cmc_nobase | 35845 / 36000 | 34977 | 868 | 0 | 36000 / 0 / 0 | 35440 |
| straight | cmc_drive | 36000 / 36000 | 1 | 35999 | 35699 | 36000 / 0 / 0 | 35750 |
| roll | cmc | 36000 / 36000 | 301 | 35699 | 35699 | 36000 / 0 / 0 | 35750 |
| roll | cmc_nobase | 35847 / 36000 | 34991 | 856 | 0 | 36000 / 0 / 0 | 35440 |
| roll | cmc_drive | 36000 / 36000 | 0 | 36000 | 1588 | 1888 / 34112 / 0 | 1638 |
| mech | cmc | 35909 / 36000 | 301 | 35608 | 35608 | 35908 / 92 / 0 | 1807 |
| mech | cmc_nobase | 35636 / 36000 | 31658 | 3978 | 0 | 35908 / 92 / 0 | 19695 |
| mech | cmc_drive | 36000 / 36000 | 0 | 36000 | 35295 | 35595 / 405 / 0 | 1732 |
| transitions | cmc | 784 / 14400 | 279 | 505 | 501 | 783 / 12177 / 1440 | 1644 |

### Hitches — frames over 16.7 ms within ±120 ticks of each transition (Unreal, NullRHI unless stated)

| scene | variant | interior | transition | n | hitches | with GC | with spawn/destroy | max frame ms in window | frame that stepped the transition, ms | steady p50 / p99 / max ms | first frame ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| mech | cmc | resident | board_docked | 1 | 0 | 0 | 0 | 1.31 | 0.20 | 1.00 / 1.35 / 16.87 | 341 |
| mech | cmc | resident | undock | 1 | 0 | 0 | 0 | 1.31 | 0.21 | 1.00 / 1.35 / 16.87 | 341 |
| mech | cmc | resident | mount | 1 | 0 | 0 | 0 | 1.53 | 0.96 | 1.00 / 1.35 / 16.87 | 341 |
| mech | cmc | resident | dismount | 1 | 0 | 0 | 0 | 1.45 | 0.99 | 1.00 / 1.35 / 16.87 | 341 |
| mech | cmc_drive | resident | board_docked | 1 | 0 | 0 | 0 | 1.80 | 0.19 | 0.99 / 1.34 / 17.61 | 335 |
| mech | cmc_drive | resident | undock | 1 | 0 | 0 | 0 | 1.80 | 0.19 | 0.99 / 1.34 / 17.61 | 335 |
| mech | cmc_drive | resident | mount | 1 | 0 | 0 | 0 | 1.26 | 1.03 | 0.99 / 1.34 / 17.61 | 335 |
| mech | cmc_drive | resident | dismount | 1 | 0 | 0 | 0 | 1.32 | 0.98 | 0.99 / 1.34 / 17.61 | 335 |
| mech | cmc_nobase | resident | board_docked | 1 | 0 | 0 | 0 | 1.48 | 0.21 | 1.00 / 1.36 / 17.04 | 344 |
| mech | cmc_nobase | resident | undock | 1 | 0 | 0 | 0 | 1.48 | 0.20 | 1.00 / 1.36 / 17.04 | 344 |
| mech | cmc_nobase | resident | mount | 1 | 0 | 0 | 0 | 1.35 | 1.19 | 1.00 / 1.36 / 17.04 | 344 |
| mech | cmc_nobase | resident | dismount | 1 | 0 | 0 | 0 | 1.43 | 1.01 | 1.00 / 1.36 / 17.04 | 344 |
| mech | mirror | resident | board_docked | 1 | 0 | 0 | 0 | 1.41 | 0.18 | 0.94 / 1.25 / 16.04 | 338 |
| mech | mirror | resident | undock | 1 | 0 | 0 | 0 | 1.41 | 0.16 | 0.94 / 1.25 / 16.04 | 338 |
| mech | mirror | resident | mount | 1 | 0 | 0 | 0 | 1.19 | 0.88 | 0.94 / 1.25 / 16.04 | 338 |
| mech | mirror | resident | dismount | 1 | 0 | 0 | 0 | 1.54 | 0.96 | 0.94 / 1.25 / 16.04 | 338 |
| rest | cmc | resident | board_docked | 1 | 0 | 0 | 0 | 0.42 | 0.22 | 0.21 / 0.29 / 16.39 | 341 |
| rest | cmc | resident | undock | 1 | 0 | 0 | 0 | 0.42 | 0.20 | 0.21 / 0.29 / 16.39 | 341 |
| rest | cmc_drive | resident | board_docked | 1 | 0 | 0 | 0 | 0.25 | 0.20 | 0.19 / 0.29 / 16.07 | 336 |
| rest | cmc_drive | resident | undock | 1 | 0 | 0 | 0 | 0.25 | 0.20 | 0.19 / 0.29 / 16.07 | 336 |
| rest | cmc_nobase | resident | board_docked | 1 | 0 | 0 | 0 | 0.29 | 0.20 | 0.21 / 0.32 / 16.52 | 344 |
| rest | cmc_nobase | resident | undock | 1 | 0 | 0 | 0 | 0.29 | 0.21 | 0.21 / 0.32 / 16.52 | 344 |
| rest | mirror | resident | board_docked | 1 | 0 | 0 | 0 | 0.37 | 0.20 | 0.14 / 0.21 / 16.73 | 355 |
| rest | mirror | resident | undock | 1 | 0 | 0 | 0 | 0.37 | 0.18 | 0.14 / 0.21 / 16.73 | 355 |
| roll | cmc | resident | board_docked | 1 | 0 | 0 | 0 | 1.34 | 0.20 | 0.99 / 1.33 / 16.70 | 334 |
| roll | cmc | resident | undock | 1 | 0 | 0 | 0 | 1.34 | 0.21 | 0.99 / 1.33 / 16.70 | 334 |
| roll | cmc_drive | resident | board_docked | 1 | 0 | 0 | 0 | 1.29 | 0.19 | 0.97 / 1.32 / 17.69 | 335 |
| roll | cmc_drive | resident | undock | 1 | 0 | 0 | 0 | 1.29 | 0.20 | 0.97 / 1.32 / 17.69 | 335 |
| roll | cmc_nobase | resident | board_docked | 1 | 0 | 0 | 0 | 1.72 | 0.20 | 1.00 / 1.37 / 18.46 | 336 |
| roll | cmc_nobase | resident | undock | 1 | 0 | 0 | 0 | 1.72 | 0.21 | 1.00 / 1.37 / 18.46 | 336 |
| roll | mirror | resident | board_docked | 1 | 0 | 0 | 0 | 1.31 | 0.17 | 0.94 / 1.25 / 17.48 | 334 |
| roll | mirror | resident | undock | 1 | 0 | 0 | 0 | 1.49 | 0.15 | 0.94 / 1.25 / 17.48 | 334 |
| straight | cmc | resident | board_docked | 1 | 0 | 0 | 0 | 1.00 | 0.21 | 0.83 / 1.05 / 16.83 | 335 |
| straight | cmc | resident | undock | 1 | 0 | 0 | 0 | 1.00 | 0.21 | 0.83 / 1.05 / 16.83 | 335 |
| straight | cmc_drive | resident | board_docked | 1 | 0 | 0 | 0 | 1.34 | 0.19 | 0.83 / 1.08 / 18.40 | 337 |
| straight | cmc_drive | resident | undock | 1 | 0 | 0 | 0 | 1.34 | 0.19 | 0.83 / 1.08 / 18.40 | 337 |
| straight | cmc_nobase | resident | board_docked | 1 | 0 | 0 | 0 | 1.59 | 0.20 | 0.83 / 1.08 / 16.74 | 348 |
| straight | cmc_nobase | resident | undock | 1 | 0 | 0 | 0 | 1.59 | 0.20 | 0.83 / 1.08 / 16.74 | 348 |
| straight | mirror | resident | board_docked | 1 | 0 | 0 | 0 | 1.05 | 0.21 | 0.76 / 0.97 / 16.85 | 337 |
| straight | mirror | resident | undock | 1 | 0 | 0 | 0 | 1.05 | 0.15 | 0.76 / 0.97 / 16.85 | 337 |
| transitions | cmc | resident | board_docked | 24 | 0 | 0 | 0 | 16.42 | 1.03 | 0.90 / 1.36 / 16.46 | 344 |
| transitions | cmc | resident | undock | 24 | 0 | 0 | 0 | 14.83 | 1.00 | 0.90 / 1.36 / 16.46 | 344 |
| transitions | cmc | resident | eva | 24 | 0 | 0 | 0 | 2.29 | 1.01 | 0.90 / 1.36 / 16.46 | 344 |
| transitions | cmc | resident | board_underway | 24 | 0 | 0 | 0 | 2.29 | 1.58 | 0.90 / 1.36 / 16.46 | 344 |
| transitions | cmc | resident | dock | 24 | 0 | 0 | 0 | 2.29 | 0.32 | 0.90 / 1.36 / 16.46 | 344 |
| transitions | cmc | resident | disembark | 24 | 0 | 0 | 0 | 2.12 | 0.35 | 0.90 / 1.36 / 16.46 | 344 |
| transitions | mirror | resident | board_docked | 24 | 0 | 0 | 0 | 14.67 | 1.19 | 0.84 / 1.30 / 14.67 | 339 |
| transitions | mirror | resident | undock | 24 | 0 | 0 | 0 | 14.67 | 1.01 | 0.84 / 1.30 / 14.67 | 339 |
| transitions | mirror | resident | eva | 24 | 0 | 0 | 0 | 1.80 | 0.87 | 0.84 / 1.30 / 14.67 | 339 |
| transitions | mirror | resident | board_underway | 24 | 0 | 0 | 0 | 1.80 | 1.24 | 0.84 / 1.30 / 14.67 | 339 |
| transitions | mirror | resident | dock | 24 | 0 | 0 | 0 | 1.80 | 0.27 | 0.84 / 1.30 / 14.67 | 339 |
| transitions | mirror | resident | disembark | 24 | 0 | 0 | 0 | 1.80 | 0.20 | 0.84 / 1.30 / 14.67 | 339 |
| transitions | mirror | spawn | board_docked | 24 | 0 | 0 | 0 | 15.09 | 4.99 | 0.19 / 1.40 / 15.69 | 332 |
| transitions | mirror | spawn | undock | 24 | 0 | 0 | 0 | 15.09 | 0.99 | 0.19 / 1.40 / 15.69 | 332 |
| transitions | mirror | spawn | eva | 24 | 0 | 0 | 0 | 5.26 | 2.23 | 0.19 / 1.40 / 15.69 | 332 |
| transitions | mirror | spawn | board_underway | 24 | 0 | 0 | 0 | 5.26 | 5.26 | 0.19 / 1.40 / 15.69 | 332 |
| transitions | mirror | spawn | dock | 24 | 0 | 0 | 0 | 5.26 | 0.28 | 0.19 / 1.40 / 15.69 | 332 |
| transitions | mirror | spawn | disembark | 24 | 0 | 0 | 0 | 5.26 | 2.40 | 0.19 / 1.40 / 15.69 | 332 |
| transitions | mirror | stream | board_docked | 24 | 0 | 0 | 0 | 8.45 | 8.45 | 0.17 / 0.85 / 18.82 | 337 |
| transitions | mirror | stream | undock | 24 | 9 | 9 | 0 | 18.82 | 0.25 | 0.17 / 0.85 / 18.82 | 337 |
| transitions | mirror | stream | eva | 24 | 9 | 9 | 0 | 18.82 | 1.26 | 0.17 / 0.85 / 18.82 | 337 |
| transitions | mirror | stream | board_underway | 24 | 12 | 12 | 0 | 18.82 | 7.75 | 0.17 / 0.85 / 18.82 | 337 |
| transitions | mirror | stream | dock | 24 | 12 | 12 | 0 | 18.82 | 0.21 | 0.17 / 0.85 / 18.82 | 337 |
| transitions | mirror | stream | disembark | 24 | 12 | 12 | 0 | 18.82 | 1.02 | 0.17 / 0.85 / 18.82 | 337 |

Rendered (`-RenderOffScreen`, Vulkan on the RTX 4090, no window; `unreal-offscreen/`, PSO cache warm, no screenshots):

| scene | variant | interior | cycles | transition | n | hitches | with GC | max frame ms in window | transition frame ms (max) | steady p50 / p99 / max ms | first frame ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| transitions | mirror | spawn | 5 | board_docked | 5 | 0 | 0 | 8.23 | 8.23 | 1.30 / 4.53 / 41.82 | 617 |
| transitions | mirror | spawn | 5 | undock | 5 | 0 | 0 | 8.23 | 2.28 | 1.30 / 4.53 / 41.82 | 617 |
| transitions | mirror | spawn | 5 | eva | 5 | 4 | 0 | 41.82 | 3.02 | 1.30 / 4.53 / 41.82 | 617 |
| transitions | mirror | spawn | 5 | board_underway | 5 | 4 | 0 | 41.82 | 41.68 | 1.30 / 4.53 / 41.82 | 617 |
| transitions | mirror | spawn | 5 | dock | 5 | 4 | 0 | 41.82 | 1.78 | 1.30 / 4.53 / 41.82 | 617 |
| transitions | mirror | spawn | 5 | disembark | 5 | 4 | 0 | 41.82 | 3.44 | 1.30 / 4.53 / 41.82 | 617 |

### Rollback in the Unreal process (transitions scene, one correction per frame change, shape `ship`)

| variant | interior | corrections | hash mismatches in window | FrameChanged re-emitted by replay | avatar frame differs after correction | presentation residual max mm | restore+install+replay ns p50 / p99 / max |
|---|---|---|---|---|---|---|---|
| cmc | resident | 144 | 0 | 144 | 0 | 568 | 28994 / 36258 / 55052 |
| mirror | resident | 144 | 0 | 144 | 0 | 568 | 28022 / 37459 / 39604 |
| mirror | spawn | 144 | 0 | 144 | 0 | 568 | 27672 / 33703 / 73197 |
| mirror | stream | 144 | 0 | 144 | 0 | 568 | 28974 / 38111 / 38441 |

### Rollback across the frame change — C consumer against the stand-in authority, hash for hash

| scene | transitions | corrections | spanning a frame change | mismatches in window | mismatches after | rollback / snap | events re-emitted | total ns p50 / p99 / max |
|---|---|---|---|---|---|---|---|---|
| mech | 4 | 4 | 4 | 0 | 0 | 4 / 0 | 4 | 34635 / 39393 / 39393 |
| mech | 4 | 4 | 4 | 0 | 0 | 4 / 0 | 4 | 27001 / 41717 / 41717 |
| mech | 4 | 300 | 4 | 0 | 0 | 300 / 0 | 4 | 19436 / 21630 / 22772 |
| roll | 2 | 300 | 2 | 0 | 0 | 300 / 0 | 2 | 19597 / 21330 / 23023 |
| straight | 2 | 300 | 2 | 0 | 0 | 300 / 0 | 2 | 19156 / 21179 / 21360 |
| transitions | 144 | 191 | 144 | 0 | 0 | 191 / 0 | 144 | 19577 / 23324 / 28393 |

Per transition kind and correction shape (transitions scene, 24 cycles):

| transition | shape | n | mismatches in window | after | hashes changed vs abandoned timeline | residual mm max | ns p50 / p99 |
|---|---|---|---|---|---|---|---|
| board_docked | identity | 9 | 0 | 0 | 0 | 0 | 19626 / 20869 |
| board_docked | ship | 7 | 0 | 0 | 105 | 451 | 19656 / 19927 |
| board_docked | avatar | 8 | 0 | 0 | 72 | 81 | 19566 / 21921 |
| undock | identity | 9 | 0 | 0 | 0 | 0 | 19706 / 20508 |
| undock | ship | 7 | 0 | 0 | 63 | 0 | 19627 / 19817 |
| undock | avatar | 8 | 0 | 0 | 72 | 40 | 19857 / 20959 |
| eva | identity | 9 | 0 | 0 | 0 | 0 | 19627 / 20338 |
| eva | ship | 7 | 0 | 0 | 91 | 493 | 19607 / 19867 |
| eva | avatar | 8 | 0 | 0 | 72 | 40 | 19486 / 19817 |
| board_underway | identity | 9 | 0 | 0 | 0 | 0 | 19576 / 21891 |
| board_underway | ship | 8 | 0 | 0 | 120 | 488 | 19506 / 20177 |
| board_underway | avatar | 7 | 0 | 0 | 63 | 1435 | 19396 / 20709 |
| dock | identity | 9 | 0 | 0 | 0 | 0 | 19827 / 28393 |
| dock | ship | 8 | 0 | 0 | 72 | 0 | 19847 / 21009 |
| dock | avatar | 7 | 0 | 0 | 63 | 40 | 19737 / 19867 |
| disembark | identity | 9 | 0 | 0 | 0 | 0 | 19596 / 20077 |
| disembark | ship | 8 | 0 | 0 | 104 | 569 | 19457 / 20018 |
| disembark | avatar | 7 | 0 | 0 | 63 | 39 | 19417 / 20037 |
| control | ship | 47 | 0 | 0 | 423 | 0 | 19176 / 23324 |

Mech scene (second nesting level; `rollback-mech.json` plus one run per divergent shape):

| transition | shape | n | mismatches in window | after | hashes changed | residual mm max |
|---|---|---|---|---|---|---|
| board_docked | identity | 1 | 0 | 0 | 0 | 0 |
| undock | identity | 1 | 0 | 0 | 0 | 0 |
| mount | identity | 1 | 0 | 0 | 0 | 0 |
| dismount | identity | 1 | 0 | 0 | 0 | 0 |
| control | ship | 296 | 0 | 0 | 2664 | 0 |
| board_docked | ship | 1 | 0 | 0 | 18 | 50 |
| undock | ship | 1 | 0 | 0 | 9 | 0 |
| mount | ship | 1 | 0 | 0 | 9 | 0 |
| dismount | ship | 1 | 0 | 0 | 9 | 0 |
| board_docked | avatar | 1 | 0 | 0 | 9 | 40 |
| undock | avatar | 1 | 0 | 0 | 9 | 40 |
| mount | avatar | 1 | 0 | 0 | 9 | 20 |
| dismount | avatar | 1 | 0 | 0 | 9 | 20 |

### Every frame over 16.7 ms in every Unreal run, attributed (from the per-tick CSVs; first frame excluded)

| run | ticks | frames > 16.7 ms | of which in a frame that ran garbage collection | max ms |
|---|---|---|---|---|
| mech-cmc-resident | 36000 | 2 | 2 | 16.87 |
| mech-cmc_drive-resident | 36000 | 3 | 3 | 17.61 |
| mech-cmc_nobase-resident | 36000 | 1 | 1 | 17.04 |
| mech-mirror-resident | 36000 | 0 | 0 | 16.04 |
| rest-cmc-resident | 36000 | 0 | 0 | 16.39 |
| rest-cmc_drive-resident | 36000 | 0 | 0 | 16.07 |
| rest-cmc_nobase-resident | 36000 | 0 | 0 | 16.52 |
| rest-mirror-resident | 36000 | 1 | 1 | 16.73 |
| roll-cmc-resident | 36000 | 1 | 1 | 16.70 |
| roll-cmc_drive-resident | 36000 | 4 | 4 | 17.69 |
| roll-cmc_nobase-resident | 36000 | 1 | 1 | 18.46 |
| roll-mirror-resident | 36000 | 3 | 3 | 17.48 |
| straight-cmc-resident | 36000 | 1 | 1 | 16.83 |
| straight-cmc_drive-resident | 36000 | 1 | 1 | 18.40 |
| straight-cmc_nobase-resident | 36000 | 1 | 1 | 16.74 |
| straight-mirror-resident | 36000 | 1 | 1 | 16.85 |
| transitions-cmc-resident | 14400 | 0 | 0 | 16.46 |
| transitions-mirror-resident | 14400 | 0 | 0 | 14.67 |
| transitions-mirror-spawn | 14400 | 0 | 0 | 15.69 |
| transitions-mirror-stream | 14400 | 12 | 12 | 18.82 |

## Readings

### 1. Drift — the per-grid local-frame mirror holds; the headline is zero

**Variant (b), the control** (`mirror`): the relative transform the mirror
holds is the ruleset's own local pose, so `direct` drift is **0.000 mm at
p50, p99 and max** over 36,000 ticks in every scene — at rest, at 50 m/s,
and at **500 m/s with a 4.998°/s roll (D5's worked number), 10 minutes, 300
km travelled, the ship 400 km from the world origin at the end**. Measured,
not extrapolated: `Root->GetRelativeLocation() − target`, per tick.

The reprojection (`reproj`: the frame's Unreal world transform inverted over
the mirror's world location) is also **0.000 mm to six decimals** at every
tick: UE 5.8's LWC transforms are double, and the ulp of a double at the
farthest world position reached (4.0 × 10⁷ cm) is **1.9 × 10⁻⁸ mm**
(`ulp_double_mm` in every summary; computed with `nextafter` on the actual
number). Reported beside it because #1045 asked: the ulp of a **float** at
the same position is **10 mm at 100 km and 40 mm at 400 km** — a
single-precision reprojection would fail the 250 mm falsifier's spirit
within the walk; the double one does not enter at the sixth decimal. The
D5 "~7.6 µm-ulp regime" claim is about f32 *local* coordinates; the mirror's
local numbers here are the ruleset's integers, so their error is exactly
zero and the ulp question moves entirely to the world-space reprojection,
where it is 1.9 × 10⁻⁸ mm.

The falsifier "drift at 500 m/s + roll ≥ 250 mm p99 in both variants" is
**not hit**: variant (b) is at 0; variant (a) with based movement disabled
(`cmc_nobase`) is at **242 mm p99 in the roll scene**, and every millimetre
of it is attributed below to CMC's own floor offset and its collision
resolution, not to the frame.

### 2. Hitches — no leg needs a loading screen, on this evidence

Headless (`-NullRHI`), 24 of each transition in the transitions scene, ±2 s
window: **0 frames over 16.7 ms around any board, undock, EVA, board-under-
way, dock or disembark** with the interior resident (`resident`) or spawned
at boarding as 200 attached components (`spawn`). The frame that stepped a
boarding was **≤ 1.24 ms** resident and **≤ 5.26 ms** with the 200-component
spawn (measured, NullRHI: engine + physics + the plugin, no renderer).
Every frame over 16.7 ms in every headless run — 0 to 3 per 36,000-tick
run, 12 in the `stream` run — fell in a frame that ran garbage collection
(`gc` column, `FCoreUObjectDelegates::GetPostGarbageCollect`), UE's
periodic purge every ~61 s; none coincides with a transition except in
`stream`, where unloading the sub-level at EVA triggers the purge two ticks
later (16.7–18.8 ms, 12 of 24 EVAs).

Rendered (`-RenderOffScreen`, Vulkan, RTX 4090, 5 cycles, screenshots on):
the **first** boarding frame took **10.5 s** — `LogPSOHitching: 50 PSO
creation hitches, 0 precached`, no `VulkanPSO.cache` on disk: first-time
pipeline-state creation for the interior's meshes, the renderer meeting new
geometry, not the frame change. Every later transition frame was 12–20 ms,
and each of those is the screenshot readback the harness requested on that
frame (the hitches sit exactly on screenshot ticks). The same run without
screenshots and with the PSO cache warm (the rendered table below): **no
PSO hitch at all, the first boarding frame under 16.7 ms**, steady p50 1.3
ms / p99 4.5 ms rendered, and **4 frames over 16.7 ms in 3,000 ticks** — a
25–28 ms pair at ticks 1051–1052 (no transition, no GC, no spawn:
unattributed) and a 42 ms pair at 1061–1062, the frames after the second
cycle's board-under-way spawned its 200 interior components (the other
four spawn-boardings in the run stayed under 16.7 ms). Rendered, the
spawn-at-boarding shape can cost one 40 ms frame; the resident shape was
not rendered-measured.

Whether a loading screen is *needed*: on this architecture the ship's
interior is **actors attached to the moving ship**, resident or spawned in
one frame; there is no level boundary to hide. The `stream` mode is the
falsifier made concrete and it fails for a reason the hitch numbers do not
show: a streamed sub-level is **world-fixed** — `LoadLevelInstance` takes a
location and the level's actors stay there, so an interior streamed at
boarding is 833 mm behind the ship one tick later at 50 m/s. Streaming can
bring a docked ship's interior in (7–8 ms blocking load for a 200-actor
level, `transition_frame_ms` in the stream row) but cannot be the interior
of a ship under way. G4.1's "minimal or no loading screens" holds for
mothership→ship and ship→space **as long as interiors are attached
hierarchies**, with the one-time PSO cost paid at cook/precache time rather
than at the first boarding.

### 3. Rollback across the frame change — hash-exact, 0 mismatches

See the C tables: **144 corrections spanning a frame change, 6 kinds × 24,
3 shapes, every depth 1–9 → 0 hash mismatches in the window and 0 in the
30 ticks after**, against a stand-in authority whose divergent shapes changed 9–18 of the
36 `(entity, tick)` hashes in each window (the `hashes changed` column is
the sum per row) and moved the avatar by up to 1.4 m. The second nesting level: mount and
dismount under all three shapes, 0 mismatches. Inside the Unreal process,
the same driver on the same arrangement (shape ship, 144 corrections) — 0
mismatches, restore+install+replay **28 µs p50 / 35–38 µs p99** in-engine
(19.6 / 23.3 µs in C; both measured, Linux, informational against D8's
≈ 1 ms budget). The presentation residual the correction left for the
mirror to absorb: up to **568 mm**, the avatar's frame never differed
before and after (the corrected timeline crossed at the same tick), and
every one of the 144 replays re-emitted its `FrameChanged` event.

What this does and does not say: D47's per-entity set carries the frame
relation *for this ruleset*, because the relation is a `frame` field on the
crossing entity and the crossing is computed from the frame body's
tick-start state, which the snapshot also restores. It does not close
replay adjudication over the basis change — that is the `FrameChange`
record's job and it is still deferred — but it moves nothing onto the
slice's critical path: no protocol record was needed for prediction to be
correct across the crossing.

### 4. The CharacterMovementComponent verdict — as a number

CMC-as-presentation, with the mirror writing the ruleset's pose into the
capsule before CMC ticks, **cannot be made to assert nothing**; the count
is per 36,000 ticks:

- **`cmc` (based movement on)**: 36,000 of 36,000 ticks assert under way,
  every one carrying a based-movement delta of **exactly the ship's per-tick
  displacement** — 833 mm at 50 m/s, 8,333 mm at 500 m/s (`cmc p50` in the
  drift table). `UpdateBasedMovement` (`CharacterMovementComponent.cpp:2555-2601`)
  composes the base's delta since `SaveBaseLocation` onto the capsule, and
  the mirror had already placed the capsule in the base's new frame. Based
  movement and a written pose double-count by construction.
- **`cmc_nobase` (based movement disabled by overriding
  `UpdateBasedMovement`)**: **35,845 / 36,000** assert, of which **34,977
  are vertical-only** — the constant **24 mm** by which CMC's floor logic
  holds the capsule above the deck (`MAX_FLOOR_DIST = 2.4f`, `CharacterMovementComponent.cpp:99`), 21.5–22.8 mm on the
  rolled deck — and **856–868 are horizontal**, all in the ten ticks per
  loop where the walked corridor crosses an interior fitting at (3 m, 4.5 m):
  CMC resolves a penetration against a box **the ruleset does not have**
  (it has no collision at all), pushing the capsule up to 321 mm. Same
  numbers at rest, at 50 m/s and at 500 m/s + roll: with the capsule
  re-oriented to the frame and gravity set to ship-down each tick, CMC
  walks all 36,000 ticks through full rolls, and its contribution is
  frame-independent. **Assertions come from the floor offset and from
  collision, never from the moving frame.**
- **`cmc_drive` (CMC drives from the ruleset's velocity, pose never
  written — the ordinary way to use CMC)**: one frame of input latency (40
  mm) at rest, then **5.24 m** behind after the capsule spent 131 ticks
  blocked on the same fitting the ruleset walked through; **at 500 m/s +
  roll it lost the floor at tick 1,888 (roll 132°) and fell for the
  remaining 34,112 ticks** — a self-driven capsule stays world-upright, and
  CMC's custom gravity (`SetGravityDirection`) needs the capsule aligned to
  it, which only the pose-writing variants do; in the mech scene it ends
  3.4 km away. CMC as a *simulation* diverges from the ruleset immediately
  and unboundedly; it cannot be the thing that moves the avatar.
- **Second nesting level (avatar in mech in ship)**: `cmc_nobase` walks it
  (35,908 of 36,000 ticks walking, 92 falling at the mount tick; 31,658
  vertical-only assertions, 3,978 horizontal — the mech platform's edge and
  the ship deck's under it) and reports the expected base only 19,695
  ticks: CMC's floor trace picks the ship deck through the 20 cm platform
  edge as the mech turns. Based movement on two levels (`cmc`) double-counts
  the ship *and* the mech: 35,608 horizontal assertions with a based delta.
  The falsifier "CMC holds only one level deep" is not what the data says;
  what it says is that **based movement is wrong at any depth when the
  pose is written, and floor logic is indifferent to depth**.
- **After EVA and re-boarding under way** (`transitions/cmc`): CMC stayed
  in `MOVE_Falling` for the rest of the scene except the flying phases
  (12,177 of 14,400 ticks). Not attributed: candidates are the
  Flying→Walking mode switch with a 50 m/s residual `Velocity`, and the
  per-tick teleport zeroing the falling displacement. Left as an admitted
  unknown.

**Verdict.** CMC can be presentation only if (1) based movement is off, (2)
its collision is against geometry the ruleset also has, and (3) the 24 mm
floor offset is accepted as a presentation constant — at which point what
remains of CMC is a capsule, a floor trace and an animation-friendly
velocity, none of which it needs `UCharacterMovementComponent` for. The
control — a scene component in the frame's local space, transform written
from the mirror — is at 0 mm in every scene and at both nesting levels, has
no mode to fall into, and re-parents in one attach call at the crossing.
**Variant (b) is what the slice should ship for nested avatars**; the
presentation features lost are CMC's floor conforming (step-up, slope
snapping), its capsule-vs-world penetration resolution and its network
prediction hooks — the last of which G11.4 already rules out, since
prediction runs in the ruleset. Features that had to be disabled to reach
the `cmc_nobase` numbers: `UpdateBasedMovement` (overridden to a no-op),
`bEnablePhysicsInteraction=false`, `bRunPhysicsWithNoController=true`,
`SetGravityDirection` to the frame's down every tick, `MOVE_Flying` in the
universe frame, and a per-tick `TeleportPhysics` write of location *and*
rotation.

### 5. Which prong — #1052's non-`App` prong

Because the question is rollback over canonical state across a frame
change, and only that prong reaches canonical state with a driver
(`crates/orrery_unreal_direct/README.md:44-77`). The 272-line driver was
lifted, not rewritten (`interiors_shared.h`, section 3, with the only
change being that a correction may carry several entities' bytes), and it
is the same code in the C consumer and inside UE 5.8. The cost of that
choice is #1052's: no transport in the process. The `App` prong's plugin
graph was not needed for anything this spike measured.

### Other findings

- **The ruleset stepped inside Unreal is bit-identical to the ruleset
  stepped from C**: the FNV chain over every state hash of every tick
  matches for all five scenes (`chain = C run` column; the two transitions
  rows in the table say NO because those runs were corrected onto the
  authority's timeline — the same run with `-InteriorsRollback=0` matches,
  `results/unreal-norollback/`).
- The host costs **5–8 µs p50 per tick** inside the engine frame at four
  bodies (`host_us`), against #1069's 177 µs `App::update` — a different
  ruleset and a different question, quoted only for scale.
- A teleport-class crossing is exact to one quantum: rotating a millimetre
  lattice is not a lattice, so `to_local ∘ to_parent` round-trips within 1
  mm (`rules.rs` test), deterministically.
- Room-to-room sub-grid transition inside the fixture: not reached.
- Surface landing: not attempted.

### Could not establish

- Windows: nothing here was linked by MSVC or run on Windows; #920's bands
  do not apply to any number above.
- A rendered hitch number for the CMC variants; the rendered row is the
  mirror variant.
- Why CMC stays in `MOVE_Falling` after the first EVA/re-board cycle.
- Unreal Insights attribution: the GC and PSO attributions come from the
  engine's own delegates and log, not from a trace.

## Reproduce

```sh
# Rust side: tests (debug), and the release archive + C consumer + traces + rollback reports
cargo test -p orrery_unreal_interiors
crates/orrery_unreal_interiors/spike.sh            # -> docs/spikes/1045-moving-interiors/results/

# one rollback run by hand
target/spike-1045/interiors_consumer rollback transitions --report /tmp/r.json
target/spike-1045/interiors_consumer rollback mech --control-every 0

# Unreal side (Linux box, UE 5.8 at ~/UnrealEngine/5.8)
docs/spikes/1045-moving-interiors/run.sh build     # staticlib (release) + MovingInteriorsEditor
docs/spikes/1045-moving-interiors/run.sh maps      # authors /Game/Maps/MovingInteriors and ShipInterior, headless
docs/spikes/1045-moving-interiors/run.sh all       # the matrix above, -NullRHI, ~30 min
docs/spikes/1045-moving-interiors/run.sh scene roll cmc resident 36000
RHI=offscreen docs/spikes/1045-moving-interiors/run.sh scene transitions mirror spawn 0 -InteriorsShots=1
docs/spikes/1045-moving-interiors/summarize.py     # the tables
```

The map can also be opened in the editor (`UnrealEditor
MovingInteriors.uproject`) and played; the scenario actor in the level reads
its scene/variant from its properties when no command line overrides them.
