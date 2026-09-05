# #898 step 3 — two sidecars, one Unreal observer, and the kill

The remainder of [#898] after [#1100] closed the rest of it: **step 3's
renderer, and the kill-the-observer half of [A9] P-4.**

Machine: Linux x86_64, 16 physical / 32 logical cores, 46 GB. UE 5.8.2 at
`~/UnrealEngine/5.8`, its bundled clang 20.1.8 and .NET. Every editor run is
headless (`-NullRHI`) with `DISPLAY` and `WAYLAND_DISPLAY` unset. **No window
was opened at any point.**

## What was built

| Piece | Where | What it is |
|---|---|---|
| The observer's rules | `crates/orrery_ipc_transport/src/observer.rs` | `ObserverView` + `ObserverLink`: overwrite semantics, membership from the complete extraction, identity by `PersistId`. Bevy-free. |
| The serving half | `crates/orrery_sidecar/src/serve.rs` | `IpcServer` + `publish_ipc_frames`: puts the extractor's `IpcOutbound` batches on a real socket, and cannot be made to disturb the simulation. |
| The reference observer | `crates/orrery_sidecar/src/bin/orrery-observer.rs` | A headless renderer with the drawing removed. The process `tests/observer_kill.rs` kills. |
| The engine ABI | `crates/orrery_unreal_observer/` | `staticlib` + one hand-written C header + a C consumer. What Unreal links. |
| The Unreal observer | `unreal/OrreryObserver/` | A `Runtime` module, one actor, one capsule per stable id, wireframe, on a flat plane. |

Before this, `orrery_ipc`'s frames were produced every tick and read back only
by a test — [ADR-0053] §"Options" records it exactly: *"the frames are never
put on a socket in production"* (`0053-unreal-client-host-scope.md:370-371`).
They are now, and an engine renders from them.

## The kill — [A9] P-4

Two `orrery-sidecar` processes, each serving on an OS-chosen port; one Unreal
editor observing both; `SIGKILL` to the editor mid-run; the sidecars sampled
before and after by a *third* process, so the instrument is not the thing
being measured.

```
sidecar seed=21 pid=2704182 addr=127.0.0.1:43875
sidecar seed=22 pid=2704255 addr=127.0.0.1:34805
== observer (to be killed) against 127.0.0.1:43875 127.0.0.1:34805
before the kill: sidecar ticks 415 and 404
== SIGKILL 2704330
editor is gone: yes
after the kill:  sidecar ticks 600 and 589
PASS: sidecar A advanced 415 -> 600 with its observer dead
PASS: sidecar B advanced 404 -> 589 with its observer dead
```

Banked at `results/kill-2026-09-05.txt`; reproduce with `run.sh kill`.

**loadavg 182 at the start of that run** — `check.sh` had just finished and the
box was still draining. That is a bad condition for a timing measurement and
this is not one: it is a liveness-and-continuity check, and a loaded box makes
it harder to pass rather than easier. Both sidecars still advanced 185
canonical ticks each with their renderer killed.

The same property is checked in `cargo test` on every commit, and more
sharply, by `crates/orrery_sidecar/tests/observer_kill.rs`: it spawns a real
`orrery-observer` process, `SIGKILL`s it, and asserts the ruleset's own
`StepTrace` continues **one tick and one millimetre at a time with no gap and
no repeat** across the kill — not merely that the sidecar is still alive.
`Synthetic::step` advances by exactly one per tick
(`crates/orrery_synthetic/src/lib.rs:83`), so a stall, a dropped tick or a
replayed one all fail the assertion.

## What the observer rendered

`run.sh observe 36000`, two sidecars, `-NullRHI`, `-UseFixedTimeStep -FPS=60`:

```
ticks=36000
entities_seen=108000
predicted_seen=72000
interpolated_seen=36000
bracketed_seen=36000
capsules=3
```

Three capsules from two sidecars: sidecar A's predicted entity, A's
interpolated one, and B's predicted one. **Every one of the 36,000
interpolated samples carried a real bracket** (`basis_from != basis_to`)
rather than an exact tick — the value-and-basis pair `orrery_predict`
co-produced, carried unchanged through the codec, the socket, the C ABI and
into C++.

### The interpolated capsule is a stand-in, and this is the sentence that says so

`--stand-in-remote` presents an entity through lightyear's `Interpolated`
class with a real `ConfirmedHistory`, and everything downstream of the
snapshots is real: the sampling, the blend, the exported basis, the
extraction. **The peer is not.** The snapshots are written by the sidecar's
own `feed_stand_in` rather than delivered by replication from another node.
So this establishes the *presentation and extraction path* for the
interpolated class end to end; it does **not** establish that a replicated
peer produces an `Interpolated` copy over the facade's link. Nothing in the
tree establishes that yet, and this demo must not be described as two peers.

## Numbers

### The Unreal-side cost of the crossing

[ADR-0053]'s "what this record could not establish" item 4 says every estimate
in it is about the Rust side of the boundary
(`0053-unreal-client-host-scope.md:645-690`). This is the other side: poll +
copy-out + capsule move, timed on the Unreal game thread with
`FPlatformTime::Seconds()`, inside the Unreal process.

| | ns |
|---|---|
| p50 | **1,323** |
| p99 | 5,069 |
| p99.9 | 7,825 |
| max | 45,385 |

35,940 samples (60 warmup ticks excluded — they pay once for spawning the
capsules), 2 links, 3 entities, `-NullRHI`. **loadavg 0.82 at the start,
0.93 at the end**, no other lane on the box.

**Read this narrowly.** The editor free-ran: 36,000 ticks in ~4.2 s, so it
polled at ~8,500 Hz against sidecars presenting at 120 Hz, and each link
applied only 542 messages over the run. Most ticks therefore measured the
copy-out and the actor moves with no new message to decode. It is the cost of
*asking*, at three entities — the right number for "what does an idle frame of
this crossing cost the game thread", and the wrong number for "what does a
frame's worth of decoding cost". A 60 Hz-paced N=24 run is named below as
undone.

### The archive

`liborrery_unreal_observer.a`, release: **28,813,108 bytes**. For scale, from
the two spikes beside it: #1043's `App`-prong archive is 193.5 MB
(`crates/orrery_unreal_host/README.md:151-155`) and #1045's is 32.2 MB
(`docs/spikes/1045-moving-interiors/README.md:25`). An observer is the
cheapest thing on this seam because it carries a socket and a codec and
nothing else — `tests/c_consumer.rs` checks that with `nm`: no symbol from
`bevy_ecs`, `bevy_app`, `lightyear`, `iroh`, `tokio` or `aeronet`.

`native-static-libs` for it: `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`.

### Build cost

| Step | Wall |
|---|---|
| `cargo build --release -p orrery_unreal_observer` | in a 2m36s workspace release build |
| UnrealBuildTool, cold (no makefile), 7 actions | **24.9 s** |
| UnrealBuildTool, one `.cpp` changed | 5.6 s |
| `run.sh map` (headless `-run=pythonscript`) | ~12 s, 0 errors |
| `run.sh observe 36000` end to end | ~9 s |

Spike #1045's 10.5 s of first-boarding PSO hitching does not appear here
because nothing was rendered; a `RHI=offscreen` run would meet it.

## Two defects this work found

**1. The shipped `orrery-sidecar` binary panicked at startup, and always had.**
`sidecar()` calls `app.finish()`; `ScheduleRunnerPlugin`'s runner re-runs
`finish()` on any app whose plugin state is not `Cleaned`; and
`RepliconSharedPlugin::finish` *removes* the `ProtocolHasher` it consumes
(`vendor/bevy_replicon/src/shared.rs:124-127`), so the second pass panics on
the `expect` there. Every test drives the app with `update()` and never met
it. The binary #871 was filed to create — because "a seam reachable only from
`cargo test` is the defect" — could not run one tick. Fixed by advancing to
`Cleaned` in the builder.

**2. A second dial queued behind the first instead of replacing it.** The
first serving thread did both `accept` and the writing, so a dial while an
observer was connected sat in the listen backlog until that observer *died* —
and an observer that hangs rather than dies would have held the listener for
as long as it lived. Found by the first Unreal kill run: the probe meant to
read the sidecar's tick connected, was accepted by the kernel, and waited
forever. Only a second *live* observer could tell the intended behaviour from
the implemented one. Split into an acceptor and a writer, and pinned by
`a_second_dial_replaces_the_first_rather_than_queueing_behind_it`.

## An observation the observer made visible

Extraction runs in `Update`, so the **presentation rate is the app's frame
rate, not its tick rate**. Free-running under `MinimalPlugins`, one sidecar
emitted **1,725 batches/s at ~248 % CPU** against a 64 Hz canonical tick —
about 27 complete extractions per tick, all of them on the link. Capping the
runner at `PRESENTATION_HZ = 120` brings that to **119 batches/s at ~32.9 %
CPU**. (Both figures are rates and CPU shares taken while the box was busy
with this lane's own builds; they are indicative of the ratio, and are not
offered as a latency measurement.)

The cap is the right lever rather than de-duplicating by tick: consecutive
batches at one tick are not redundant, because an interpolated entity's alpha
advances between them, and that motion is the whole reason the interpolated
class is presented at frame rate.

## What this does and does not say about [ADR-0053]

The record is Proposed and **acceptance is owner-reserved; nothing here edits
it or asks for it to be accepted.** Three of its sentences are now touched by
running code:

- Clause (d)'s **M2** — "canonical bytes for the simulation path,
  `orrery_ipc` frames for the presentation path" — notes that *"`orrery_ipc`
  has no production consumer today, so adopting it is also the act of first
  proving it"* (`:526-527`). This is that act, on the presentation half: a
  shipped sidecar puts frames on a socket and an Unreal module renders them.
- Clause (c) item 3, the missing **spawn/despawn stream**, is supplied: the
  schema carries the batches, and the observer's membership comes from the
  complete extraction beside them.
- Clause (c) item 5's prediction that wrapping the archive is *"a `.Build.cs`
  and a C++ file, not a redesign"* (`:210-214`) holds a second time —
  `OrreryObserver.Build.cs` is 45 lines and follows #1045's exactly.

Clause (f) items 1 and 2 are held by the **shape of the surface**, not by
convention: `orrery_unreal_observer` exports no send, submit or input symbol,
so a module holding the handle cannot produce a canonical fact with it. The
actor moves capsules by assignment and runs no physics, no
`CharacterMovementComponent` and no Unreal replication.

**Clause (e) is not addressed here** and this spike takes no position on the
in-process-versus-sidecar fork. It is also, on its face, factually stale — it
says no Windows report exists and one landed on 2026-09-04
(`docs/data/sidecar-ipc-windows-2026-09-04-n24.json`), tracked as [#1101].

## Deliberately not done

- **A 60 Hz-paced observer run at N=24.** The cost figure above is an idle-ish
  frame at three entities. The number that would sit beside #920's is a paced
  run at the tested baseline population, and it needs the observer to sleep to
  a deadline rather than free-run.
- **Any Windows measurement.** Neither box is Windows. The Linux figures are
  not substitutes and are not offered as any.
- **A rendered frame.** Every run was `-NullRHI`. `RHI=offscreen` is wired in
  `run.sh` and untried; #1045's 10.5 s cold-PSO figure is what it would meet
  first.
- **A real replicated peer** behind the interpolated capsule — see the
  stand-in warning above.
- **Fan-out to several observers.** One at a time, by design; a second dial
  replaces the first.
- **Input.** The link is one-directional and stays that way here; #898 step 4
  landed the verdict path separately, over iroh, not over this.

## Reproducing

```sh
docs/spikes/898-unreal-observer/run.sh build        # cargo staticlib, then UnrealBuildTool
docs/spikes/898-unreal-observer/run.sh map          # author the map, headless
docs/spikes/898-unreal-observer/run.sh observe 36000
docs/spikes/898-unreal-observer/run.sh kill         # the A9 P-4 demonstration

cargo test -p orrery_sidecar --test observer_kill   # the same property, every commit
cargo test -p orrery_unreal_observer                # the C boundary
```

And without Unreal at all, two sidecars and the reference observer:

```sh
cargo build --release -p orrery_sidecar
target/release/orrery-sidecar --serve 127.0.0.1:0 --seed 21 --entity 1 --stand-in-remote 42 &
target/release/orrery-sidecar --serve 127.0.0.1:0 --seed 22 --entity 7 &
target/release/orrery-observer --addr 127.0.0.1:PORT_A --addr 127.0.0.1:PORT_B --frames 120
```

[#898]: https://github.com/baadc0de/orrery/issues/898
[#1100]: https://github.com/baadc0de/orrery/pull/1100
[#1101]: https://github.com/baadc0de/orrery/issues/1101
[A9]: ../../plans/a9-engine-boundaries.md
[ADR-0053]: ../../adr/0053-unreal-client-host-scope.md
