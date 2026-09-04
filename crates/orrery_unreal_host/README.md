# orrery_unreal_host — spike #1043, the engine-independent half

**Research spike, not shipped code.** This crate and its C consumer are the
object an Unreal plugin would later wrap, built and measured on Linux with
clang, because the build host has no Unreal Engine, no MSVC and no Windows
machine (#1043, owner's comment of 2026-09-04). Everything below is evidence
for a decision the owner has not taken. It settles neither G10.2 nor D52/D53
(both Proposed, #1022).

## What it is

- **A `staticlib`** (`Cargo.toml`, `crate-type = ["staticlib", "rlib"]`)
  carrying `orrery_sim_host`'s generic `orrery_host_*` ABI
  (`crates/orrery_sim_host/src/abi.rs`), a real ruleset behind one factory
  (`src/skirmish.rs`: `orrery_games::Skirmish`, not the synthetic one), and
  a headless `bevy_app::App` behind a second opaque handle (`src/app.rs`):
  `MinimalPlugins` + `StatesPlugin` + `OrreryNetPlugin` (relays disabled) +
  `OrreryPredictPlugin` at D16's defaults. This is the **`App` prong** of
  D53's fork (`docs/adr/0053-unreal-client-host-scope.md`, §Options, "A fork
  H1 and H2 share"): a full `App` beside the ABI handle, not a non-`App`
  driver behind it (#1052 is that prong; it is not here).
- **A C consumer** (`examples/c/spike_consumer.c`, `include/
  orrery_unreal_host.h`) compiled with `-std=c11 -Wall -Wextra -Werror`, that
  links the archive, creates both handles, owns the fixed-step loop, calls
  `App::update()` once per tick from its own thread, drives the host through
  the generic entry points only, decodes the canonical bytes with a C mirror
  of `Craft::decode` (`crates/orrery_games/src/skirmish/state.rs:104-127`)
  and writes the mirror the way an actor transform write would.
- **A test** (`tests/c_consumer.rs`, Linux-only by `cfg`) that builds the
  archive, asserts every exported symbol with `nm`, compiles the C program
  and runs it four ways. `cargo test -p orrery_unreal_host` runs it;
  `./scripts/check.sh` reaches it through the root workspace.
- **A measurement** (`spike.sh`) that produces `docs/data/inproc-linux-
  <date>-n24-{manual,auto,no-app}.json` in the `orrery-ipc-harness/1` schema
  and renders each with `scripts/ipc-report.py`.

The two handles are **not connected**. D53 §5 records that the rollback
driver between `orrery_predict` and `orrery_sim_host` does not exist; this
spike does not build it. What is measured is what the `App` prong costs on
the game thread beside the host path, which is the coexistence question
#1043 asks and the one that can be partly answered from C.

## The number: predicted-tick latency at N = 24, measured as #920 measures `ipc_added`

Method matched to `crates/orrery_ipc_transport/src/bench.rs`:

| #920 (sidecar)                                                    | here (in-process)                                                        |
|-------------------------------------------------------------------|--------------------------------------------------------------------------|
| one system-wide monotonic clock, `CLOCK_MONOTONIC` (`lib.rs:311`) | same clock, read from C                                                  |
| pacing `sleep_until_ns` (`lib.rs:389-401`): sleep in 1 ms quanta, spin the last 1.5 ms | same routine in C                                       |
| N = 24, 60 Hz, 600 warmup ticks, 36,000 sampled ticks             | same                                                                     |
| one input per tick from the game thread (`bench.rs:317-333`)      | one `Thrust` order for the local craft, `t0` at the hand-over            |
| `hop_in = t1 − t0` (transport, decode)                            | the `orrery_host_submit_command` call: command decode + queue           |
| `extract = t2 − t1` (step + extraction)                           | `orrery_host_step(1)`: the tick, its state hashes                        |
| `encode = t3 − t2`                                                | `orrery_host_collect_states`: canonical bytes copied into caller storage |
| `hop_out = t4′ − t3`                                              | one clock read — there is no hop; the column records its absence         |
| `decode_out = t_decode − t4′`                                     | C-side decode of all 24 records                                          |
| `phase = t_apply − t4′`, reported separately (`bench.rs:627`)     | apply to the mirror actors; **no tick wait exists in-process**, so the column collapses to the apply cost |
| `ipc_added = t_decode − t0` (`bench.rs:629`)                      | `inproc_added = t_decode − t0`, emitted under the key `ipc_added` so the report script renders it |
| nearest-rank percentiles (`bench.rs:400-405`)                     | same formula                                                             |
| real per-frame work, not ping-pong (#920 lie 5)                   | 23 honest pilots' orders submitted through the ABI each tick (the P4 pilot, `orrery_games::skirmish::pilot::honest_orders`), damage routed through the adapter, events and hashes drained |

Where it necessarily differs: there is no second process, so `hop_null`,
the sidecar's own report, `TCP_NODELAY` and `timeBeginPeriod` have no
meaning (the schema fields are written `false`, factually); and `phase` is
not a wait because the mirror is applied in the frame that produced it.

### The number: deferred

**No latency number is published from this pass.** Every run attempted on
2026-09-04 was taken on a box at 1-minute load 40–70 on 32 cores (seven
orphaned spinning shells, since killed, and an Unreal Engine source build
still running at the time of writing). #920's method is only worth matching
if the conditions are comparable, and a number with a caveat gets quoted
without it. The harness is complete and was exercised end to end — three
600-tick dry runs rendered through `scripts/ipc-report.py` with every
column present and `ipc_added` equal to the sum of its parts — but those
figures are not evidence and are not committed. The three 10-minute runs
(`spike.sh`) are the second pass, on a quiet box; each report carries
`loadavg_start`/`loadavg_end` so it identifies itself either way.

What the harness reports when it runs, so the second pass has nothing to
design: `phases_ns` (the eight #920 columns), `baselines_ns.app_update`
(`App::update()` on the game thread — the column #1043's 1 ms coexistence
falsifier is drawn on), `baselines_ns.remote_inputs`, `drains`,
`frame_total`, `drops` (`input_dropped`, `step_failed`, `app_update_failed`,
`decode_failures`, `tick_overruns` — every one a counter the loop
increments), `coexistence` and `timeline` (below), and `notes`.

## What the number means, and what it does not

- It is a **Linux staticlib-plus-C-consumer baseline**: the predicted-tick
  cost of the host path when the caller is in the same process and the
  mirror is written in the same frame. On this box, beside #920's Linux
  sidecar number (`docs/data/sidecar-ipc-linux-2026-09-03-n24.json`:
  `ipc_added` p50 41.7 µs, p99 70.9 µs, p99.9 198.3 µs), it is the other
  half of the comparison D53 H2 wants on one graph — **on Linux, which is
  informational under #920's own rule** (`scripts/ipc-report.py:204-213`).
- It is **not** the in-process Unreal number G10.2 turns on. That number is
  taken inside a UE 5.8 process on Windows with real actor mirror work, UE's
  task graph beside Bevy's pools, and `timeBeginPeriod` both ways. Nothing
  here was linked by MSVC, nothing here ran beside Unreal's scheduler, and
  no frame was displayed (#744 D.1 stays unproven).
- It settles **neither G10.2 nor D52/D53**. D53 is Proposed (#1022); the
  fork between a full `App` and a non-`App` driver is the owner's (#1042
  rule 7). This is evidence on the `App` prong only; the non-`App` prong has
  its own spike (#1052) and none of its numbers.
- Its **falsifiers read as follows** against #1043's list: the staticlib
  linked (on Linux/clang; the MSVC question is untouched); `App::update()`
  p99 on the game thread is the coexistence number the owner asked for, and
  the 1 ms line #1043 draws is drawn on it below; nothing deadlocked; no
  panic crossed the boundary except as a code.

## Coexistence, as far as C can see it

These are load-independent facts, observed on this box from the C driver,
each attributed to the actor that produced it.

- **What a headless `App` spawns into a foreign process** (read from
  `/proc/self/task/*/comm`, not from either engine's claim): the C process
  went from **1 thread to 65** at `orrery_app_create`, on 32 cores —
  `Compute Task Pool` ×24, `Async Compute Task Pool` ×4, `IO Task Pool` ×4
  (bevy_tasks' default split, `bevy_app` 0.19.1 `task_pool_plugin.rs`),
  and **`tokio-rt-worker` ×32** from the multi-thread runtime
  `aeronet_tokio_runtime` builds for the iroh endpoint
  (`vendor/aeronet_tokio_runtime/src/lib.rs:110-113`, `new_multi_thread()`
  with the default worker count = cores). One more thread appeared during
  the loop in one run (66 at end). Inside a UE process these sit beside
  UE's own task-graph workers; the two pools are sized independently and
  neither knows about the other. That oversubscription is #1043's
  coexistence falsifier, and it is a configuration question (both pool
  sizes are settable) rather than a structural one — but nothing in the
  tree sets them, and this spike did not.
- **Bevy's scheduler ran from a foreign main loop without incident.**
  `App::update()` was called from the C thread that created the `App`
  36,000+ times per dry run with `MinimalPlugins`' multi-threaded executor
  dispatching systems onto the compute pool; no deadlock, no hang, no
  `check`-class failure, `app_update_failed = 0`.
- **Update from a thread that did not create the `App` works.** `threadhop`
  creates the `App` on the main thread and updates it three times from a
  `pthread`: `on_creating_thread=0 update=0,0,0 back_on_creator=0
  fixed_steps=4 destroy=0`. No `NonSend` panic fired for this plugin set.
  Unreal's game thread need not be the thread that loaded the module.
- **A system panic stays behind the boundary, as a code.** `panic`: the
  probe system panics on `Compute Task Pool (18)`; Bevy's executor re-raises
  it on the calling thread (`bevy_ecs` 0.19.1
  `schedule/executor/multi_threaded.rs:305-308`); `orrery_app_update`
  returns `7` (`PANIC`), the next call `6` (`POISONED`), destroy `0`. Same
  contract as the host handle (`crates/orrery_sim_host/src/abi.rs:34-40`).
  The `Poisoned`/`Panic` codes were the only way any panic was seen.
- **The archive links on Linux/clang with the C runtime alone**:
  `rustc --print native-static-libs` = `-lgcc_s -lutil -lrt -lpthread -lm
  -ldl -lc`. `liborrery_unreal_host.a` is **193.5 MB** in release (it
  carries lightyear, iroh, tokio, bevy_ecs and the games); the linked C
  executable is 73 MB. The Windows library set is not established.
- **Idle cost** is measured (`idle_cpu_pct_without_app` vs
  `idle_cpu_pct_with_app`, process CPU over a 60 Hz paced window doing only
  `App::update()`), but the values from the loaded box are not reported.

## Where the `App` prong looked structurally off

1. **Two accumulators, and Bevy discards the first delta.** The host never
   reads a clock (`crates/orrery_sim_host/src/lib.rs:6-9`); Bevy's
   `TimePlugin` reads the wall clock, and lightyear increments
   `LocalTimeline` from `Time<Fixed>` in `FixedFirst`
   (`crates/orrery_predict/src/plugin.rs:52-57`). So the `App` prong has
   **two tick counters by construction** — the foreign accumulator's and
   Bevy's — and #1043's stated unknown ("whether lightyear's tick bridge
   survives an externally-owned accumulator") is the question of whether
   they stay equal. Measured:
   - Under `TimeUpdateStrategy::ManualDuration(fixed_step)` — the mechanism
     by which a foreign accumulator owns Bevy's clock — 120 `update()`s
     produce **119** fixed steps and lightyear tick **119**
     (`src/app.rs`, `a_manual_app_runs_exactly_one_fixed_step_per_update`;
     the C smoke line asserts the same). Bevy's first update is its
     zero-delta startup frame. A driver that does not prime one update at
     creation carries a **permanent one-tick offset** between the host tick
     it stepped and the tick lightyear stamps. With one priming update
     (the bench's idle window does this), the drift over the measured loop
     is **0** (`timeline.drift_ticks_lightyear_minus_host`).
   - Under `Automatic` (Bevy on the wall clock), `Time<Fixed>` steps 0, 1
     or 2 times per `update()` and lightyear's tick follows wall time, not
     the caller's count; the two agree on average and disagree per frame.
     The number for the disagreement over 36,000 frames is in the deferred
     run's `timeline` block.
   So on this prong a UE `UGameInstanceSubsystem` owning the accumulator
   (#725's recommendation) must also own Bevy's clock through
   `ManualDuration`, and prime it — or lightyear stamps a different tick
   than the host executed. That is the sharp form of D53 §"could not
   establish" item 2, and it is a driver obligation the non-`App` prong
   would not have.
2. **The `App` prong ships a second scheduler and a second runtime.** 64
   threads, a plugin graph, and an iroh endpoint opened in `Startup`
   (`crates/orrery_net/src/plugin.rs:101,126-129`) arrive with the handle
   whether or not the game is connected. That is exactly what D53's fork
   text predicts ("drags a plugin graph, a schedule runner and lightyear
   into the shipped client process"); this spike puts thread counts on it.
3. **`OrreryPredictPlugin` needs `StatesPlugin`, which `MinimalPlugins`
   lacks** — the same line the sidecar and the regolith client both carry
   (`crates/orrery_sidecar/src/lib.rs:258-262`,
   `clients/regolith/src/main.rs:497-501`). Not wrong, but the "MinimalPlugins
   + net + predict" composition in the issue text is one plugin short.
4. **Nothing connects the two handles.** The host is stepped by C; the
   `App` predicts nothing, because the rollback driver D53 §5 names does
   not exist. `inproc_added` therefore measures the host path with the
   `App` *beside* it, not *through* it. A driver that routes the host's
   snapshot/restore through lightyear's rollback would put `App::update()`
   on the input path, and the number would change. That driver is
   #1043's item 1 as written, and it is not built here.
5. **What a UE plugin would wrap is already the shape it needs**: one
   header, one archive, `orrery_app_create/update/destroy` beside
   `orrery_host_*`, no allocator crossing, no Bevy type in the header.
   Wrapping it in a `UGameInstanceSubsystem` is a `.Build.cs` and a C++
   file, not a redesign.

## What could not be established without the engine

- Whether the archive links under UE's MSVC flags (`/MD`, `bUseUnity`, PCH),
  whether duplicate symbols appear against UE's bundled zlib/openssl-class
  libraries, and which Windows system libraries the link needs. The Linux
  answer to the last is recorded in the reports' notes
  (`native-static-libs`, from `rustc --print native-static-libs`); it says
  nothing about `ws2_32`/`bcrypt`/`ntdll`/`userenv`.
- Whether `catch_unwind` and the `Poisoned` contract behave the same across
  an MSVC-built boundary. Proven here on Linux only.
- What Bevy's 64 threads do beside Unreal's task graph on the same cores.
  Here they sat beside one C thread.
- `inproc_added` versus `ipc_added` **on the same Windows box** — the
  comparison #1043's third falsifier is about. The Windows sidecar leg is
  still red in the nightly (#1043 Settles).
- A frame on screen. No Unreal, no pixels.

## Reproduce

Toolchain: the pinned `rust-toolchain.toml` (1.96.0) — on this box the
rustup shims must precede system rust on `PATH` or the pin is silently
ignored; clang 22.1.8 (or `CC=cc` for gcc); Linux x86_64, 32 cores.

```sh
# the four C-driven tests plus the crate's unit tests (debug profile)
cargo test -p orrery_unreal_host

# the measurement: release staticlib, C consumer, three 10-minute runs,
# reports in docs/data/, rendered with scripts/ipc-report.py
crates/orrery_unreal_host/spike.sh            # date label defaults to today
crates/orrery_unreal_host/spike.sh 2026-09-04 600   # a 10-second dry run

# one run by hand, after spike.sh has built target/spike-1043/spike_consumer
target/spike-1043/spike_consumer bench --entities 24 --ticks 36000 --warmup 600 \
    --clock manual --report out.json
python3 scripts/ipc-report.py out.json
target/spike-1043/spike_consumer panic       # PANIC then POISONED then destroy OK
target/spike-1043/spike_consumer threadhop   # update from a thread that did not create the App
```
