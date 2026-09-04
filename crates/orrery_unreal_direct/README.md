# orrery_unreal_direct — spike #1052, the non-`App` prong

**Research spike, not shipped code.** This crate and its C consumer are the
object an Unreal plugin would wrap **if the process carried no Bevy `App`**,
built and measured on Linux with clang, because the build host has no Unreal
Engine, no MSVC and no Windows machine. It is the second prong of the fork
D53 records (`docs/adr/0053-unreal-client-host-scope.md` §Options, "A fork H1
and H2 share"); spike #1043 (branch `spike/1043-staticlib-c-consumer`,
`0f48fa8`) is the first. Everything below is evidence for a decision the
owner has not taken. It settles neither G10.2 nor D52/D53 (both Proposed,
#1022), and #1042 rule 7 says the fork is the owner's.

## What it is

- **A `staticlib`** (`Cargo.toml`, `crate-type = ["staticlib", "rlib"]`)
  carrying `orrery_sim_host`'s generic `orrery_host_*` ABI
  (`crates/orrery_sim_host/src/abi.rs`) and one factory over a real ruleset
  (`src/skirmish.rs`: `orrery_games::Skirmish`, the same factory and the
  same two helpers spike #1043 exports, so the two prongs drive the same
  rules through the same entry points). **That is the whole Rust side.**
  There is no second handle: no `bevy_app::App`, no `bevy_time`, no
  `bevy_state`, no lightyear, no iroh, no tokio. `tests/c_consumer.rs`
  proves the absence with `nm` over the archive rather than asserting it.
- **A C consumer** (`examples/c/direct_consumer.c`, `include/
  orrery_unreal_direct.h`) compiled with `-std=c11 -Wall -Wextra -Werror`,
  that links the archive, creates the one handle, owns the fixed-step loop —
  and owns **everything the `App` prong got from Bevy and lightyear**: the
  D8 prediction ring, the input history, the correction intake with its
  rollback-or-snap decision, the restore, the replay with ring rewrite, and
  the residual the reconciliation monitor would consume. All of it through
  `orrery_host_snapshot` / `orrery_host_restore` / `orrery_host_install_state`
  / `orrery_host_submit_command` / `orrery_host_step` and nothing else.
  Lines 397–669 of that file (272 lines) are the driver; the rest is the
  #920-shaped harness, the JSON writer and the proof mode.
- **A test** (`tests/c_consumer.rs`, Linux-only by `cfg`) that builds the
  archive, asserts every exported symbol and the absence of every runtime
  crate, compiles the C program and runs it three ways. `cargo test -p
  orrery_unreal_direct` runs it; `./scripts/check.sh` reaches it through the
  root workspace.
- **A measurement** (`spike.sh`) that produces `direct-linux-<date>-n24-
  {predict,control}.json` in the `orrery-ipc-harness/1` schema and renders
  each with `scripts/ipc-report.py`.

## The thing the `App` prong was written not to have: the rollback driver

D53 §5: *"The rollback primitive is present; the rollback driver is not"* —
*"no code connects `orrery_predict` to `orrery_sim_host`"*. #1043 measured
its `App` **beside** the host and left the two unconnected (its README,
"Nothing connects the two handles"). This prong builds the driver, in C,
against the ABI as it stands, and the issue's third falsifier — *"the
prediction/rollback path cannot be driven at all through the existing
ABI"* — is the one thing here that is not a number. It is answered:

```
$ target/spike-1052/direct_consumer rollback
rollback depth=9 host_next_tick=60 identity_ok=1 identity_hashes_changed=0
identity_residual_mm=0 divergent_ok=1 divergent_hashes_changed=9
divergent_residual_mm=48 repeat_ok=1 repeat_hashes_changed=0
repeat_residual_mm=0 restore_failed=0 replay_step_failed=0 ...
```

Read: 24 craft fight for 60 ticks (damage routed by the adapter, cooldowns
running). Three corrections for the local craft arrive at **depth 9** —
D8's full window (`crates/orrery_predict/src/config.rs:62`):

1. **identity** — the predictor's own bytes for tick 51: restore the ring
   slot, install, replay 51..59 resubmitting the logged inputs. Every one of
   the 216 state hashes (24 entities × 9 ticks) equals the original run's.
   That is `restore`'s guarantee (`crates/orrery_sim_host/src/lib.rs:711-720`)
   holding through the C ABI, over a real ruleset, with adapter-routed inputs
   queued at the boundary (`lib.rs:194-200`).
2. **divergent** — the authority's bytes for tick 51 (the authority saw the
   local input one tick late, the physical shape of a mispredict): 9 hashes
   change — entity 1 at each replayed tick — and the local craft moves 48 mm.
3. **repeat** — the same divergent bytes again: 0 hashes change, 0 mm. The
   ring now holds the corrected timeline, which is what the replay's ring
   rewrite is for.

The tick bridge that #1043's item 1 named as its unknown has nothing to
bridge here. The host never reads a clock (`crates/orrery_sim_host/src/lib.rs:6-9`)
and there is no second accumulator: `smoke` issues 120 ticks, the host
counts 120, **no priming update**, `drift_ticks_host_minus_issued = 0`. The
`App` prong needed `TimeUpdateStrategy::ManualDuration` plus one primed
update to get to 0 (its README, "Two accumulators, and Bevy discards the
first delta"). That obligation does not exist on this prong.

## Threads, size, link — load-independent facts

Each read from the OS (`/proc/self/task/*/comm`) or from the linker, never
from the library's claim.

| | `App` prong (#1043) | non-`App` prong (this) |
|---|---|---|
| threads at handle creation | **1 → 65** (Compute ×24, Async Compute ×4, IO ×4, `tokio-rt-worker` ×32) | **1 → 1**, and 1 at the end of the loop |
| release archive | 193.5 MB | **34.2 MB** (`liborrery_unreal_direct.a`, 34,227,444 bytes) |
| linked C executable | 73 MB | **10.3 MB** |
| `native-static-libs` | `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc` | same set |
| accumulators | two (C's and Bevy's `Time<Fixed>`), primed to agree | one |
| iroh endpoint opened in `Startup` | yes, connected or not | no endpoint exists |
| `bevy_ecs` in the archive | yes (multi-threaded executor) | yes, **single-threaded**: `bevy_ecs` is `default-features = false` (`Cargo.toml:85`), and the only `bevy_tasks` code `nm` finds is `single_threaded_task_pool` plus the unused `ComputeTaskPool::get` statics |

The `bevy_ecs` row matters for D53 §"could not establish" item 5 — whether
G10.2's "Bevy headless" meant a full `App` or *"the `bevy_ecs` backend that
exists"*. Two facts about that backend, read from the tree:
`orrery_sim_host::ecs::EcsBackend` requires `CoreState: Sectioned`
(`crates/orrery_sim_host/src/ecs.rs:91-96`), and only `RegolithState`
implements it (`crates/orrery_games/src/regolith/state.rs:493`) — **Skirmish
cannot be hosted on it today**, so this prong runs on the `Executor`
backend. And with `bevy_ecs` single-threaded, the ECS backend would add no
threads either; the question of which backend is a storage question
(`ecs.rs:49-56`), not a runtime one.

## The number: predicted-tick latency at N = 24, measured as #920 and #1043

Method matched to `crates/orrery_ipc_transport/src/bench.rs` and to #1043's
consumer column for column: `CLOCK_MONOTONIC`, the same `sleep_until_ns`
pacing (sleep in 1 ms quanta, spin the last 1.5 ms), N = 24 at 60 Hz, 600
warmup ticks, 36,000 sampled ticks, one local input per tick with `t0` at
the hand-over, 23 honest pilots submitted through the ABI each tick
(`orrery_games::skirmish::pilot::honest_orders`), damage routed through the
adapter, nearest-rank percentiles. `ipc_added` is emitted under that key so
`scripts/ipc-report.py` renders it, and it is **defined exactly as #1043
defines `inproc_added`** — `t_decode − t0`, submit + step + collect + decode
— so the two prongs compare like with like. What this prong adds is reported
**beside** it, never inside it:

| column | what it is |
|---|---|
| `baselines_ns.snapshot` | the ring write at every tick boundary (`orrery_host_snapshot` into the slot) — the per-frame cost the ring adds |
| `baselines_ns.authority_step` | a **stand-in authority** stepped on this thread to manufacture correction bytes; in production the authority is a remote peer and this column does not exist on the game thread |
| `rollback_ns.depth_k`, k = 1..9 | one correction every 12 ticks (5 Hz), depth cycling 1..9: restore + install + k-tick replay with ring rewrite |
| `rollback_ns.restore`, `.replay`, `.residual_mm` | the split, and the residual the monitor would take (`crates/orrery_predict/src/monitor.rs:59-70`, `eps_pos_mm`) |
| `rollback_ns.events_reemitted_by_replay` | events the replay emits a second time — a de-duplication obligation for presentation that the driver has to carry (lightyear's rollback carries it for Bevy components; here nothing does it for you) |
| `drops.snapshot_failed`, `restore_failed`, `replay_step_failed` | every one a counter the loop increments |

### The number: deferred

**No latency number is published from this pass.** The box was running an
Unreal Engine installed-engine build (`BuildGraph -target="Make Installed
Build Linux"`, Linux + Win64, Development + Shipping) for the whole of it;
`/proc/loadavg` on 32 cores read **24.7** when work started, **20–30** during
every build and run, and **12.5–13.0** at its lowest during the dry run. The
brief's condition for a number is low single digits, and a number with a
caveat gets quoted without it.

What was done instead: the harness was exercised end to end — `spike.sh`
built the release archive, linked the consumer with clang, ran two 600-tick
dry runs (`predict` and `control`) and rendered both through
`scripts/ipc-report.py` with every column present, `ipc_added` equal to the
sum of its parts, zero drops, zero overruns, 99 corrections all planned as
rollback, **1 → 1 → 1 threads**. Those dry-run figures are not evidence and
are not committed. The two 10-minute runs are the second pass, on a quiet
box:

```sh
crates/orrery_unreal_direct/spike.sh            # date label defaults to today; reports to docs/data/
```

Each report carries `loadavg_start`/`loadavg_end`, so it identifies itself
either way.

## What this prong loses — the real content of the fork

The `App` prong ships a plugin graph; this prong ships none. What that
graph was doing, with the code that did it, and where each piece now has to
live:

### Network — nothing in-process can carry a packet

`OrreryNetPlugin` (`crates/orrery_net/src/plugin.rs:83-123`) is the *whole*
network stack, and every piece of it is a Bevy system or resource:

| what | where | on this prong |
|---|---|---|
| iroh endpoint, opened in `Startup` | `plugin.rs:101,126-129` | **absent**; nothing opens a socket |
| peer connect/disconnect tracking (`PeerRegistry`) | `plugin.rs:175-206` | absent |
| relay-path telemetry | `plugin.rs:217-225` | absent |
| coordinator client and island membership | `plugin.rs:98-100,119-121`, `coordinator.rs`, `island.rs` | absent |
| the peer packet lane (datagrams = state, streams = control) | `plugin.rs:109-111`, `peer_link.rs` | absent |
| upload budget and meter | `plugin.rs:93-94`, `budget.rs` | absent |
| session admission | `plugin.rs:133-135` | absent |

So **a non-`App` process has no transport**. The consumer's choices are
(a) run the net `App` on a worker thread — the threads come back, but the
game thread never calls `App::update()` and the host keeps one clock; that
is a *third* shape D53's fork does not name; (b) reimplement on iroh/quinn
directly — tokio's 32 workers come back and D3's channel policy is
rewritten in C++; (c) the sidecar (#920, D53 H3). None of the three is
measured here, and the choice is the network's, not the host's.

### Prediction — what `OrreryPredictPlugin` did, piece by piece

| what | where | on this prong |
|---|---|---|
| lightyear `ClientPlugins` with D16's five numbers | `crates/orrery_predict/src/wiring.rs:8-14` | absent. lightyear rolls back **Bevy components**, and per its own docs supplies *"prediction mechanics only"* — no per-entity authority, no rollback signal (`wiring.rs:34-53`). It never touched the host's canonical state; the driver that would connect it does not exist (D53 §5) |
| the tick bridge, advanced every `FixedLast` | `plugin.rs:52-57,73-75`, `tick.rs` | **not needed**: one clock, one `u64` tick; nothing to bridge |
| correction intake and the rollback-or-snap plan | `correction.rs:71-85,100-120` | **reimplemented**: `apply_correction`, the depth test against the window, snap past it (`direct_consumer.c:559-586`) |
| the ring, the input history | lightyear's `ConfirmedHistory`, `InputConfig::packet_redundancy` (`config.rs:47`) | **reimplemented**: `ring_slot`, `ring_snapshot`, `submit_logged` — a 10-slot ring of `HostSnapshot` bytes plus the logged commands, 3.6 KB per slot at N = 24 (`snapshot_bytes_max`) |
| restore, replay, ring rewrite | (nothing did this; D53 §5) | **built**: the replay loop in `apply_correction` |
| the residual for the monitor | `wiring.rs:170-174` (`feed_residuals`), `monitor.rs` | **the residual is computed** (`residual_mm`, integer mm on the lattice as `monitor.rs:13-18` requires); the EWMA, bands, sustain and confidence grading of `ReconciliationMonitor` are **not reimplemented** |
| the rollback budget ladder | `budget.rs:26-59,97-124` | **not reimplemented**; the cost it would plan against is measured (`rollback_ns.depth_k`) so its `step_cost` EWMA has a number to seed from |
| interpolation from two authoritative snapshots with `RenderedInterpBasis` | `wiring.rs:177-200`, `AppInterpolationBasisExt` | **not reimplemented**; under G11.4 this is presentation and would be written on the Unreal side against `install_state`'s observed ticks |
| `PredictConfig::validate`'s coupling invariants | `config.rs:72-90`, `plugin.rs:29-37` | not applicable — no lightyear knobs to couple |
| a second event stream on replay | lightyear's rollback replays Bevy systems and their events | **counted, not solved**: `events_reemitted_by_replay` (34 over 45 replayed ticks in `smoke`); presentation must de-duplicate by `(source, tick)` |

The honest total: **272 lines of C** (ring, history, intake, restore,
replay, residual) bought a working, hash-exact rollback through the
canonical state — the state the hit ledger and the witness reports are
about. What the `App` prong has instead is lightyear's rollback of Bevy
components, which does not reach that state, plus the same 272 lines still
to write. The monitor's statistics, the budget ladder and interpolation are
unwritten on this prong and *present but unconnected to the host* on the
other.

## Falsifiers, as #1052 wrote them

1. *Reimplementation cost exceeds the `App` prong's coexistence cost.*
   Prediction side: 272 lines, measured above, versus 64 threads, a second
   primed clock, and a bridge that is still unwritten. **Not falsified** for
   prediction. Network side: **not measured** — the transport is absent
   here, and its cost is (a)/(b)/(c) above.
2. *Scheduling the loop from the game thread cannot hold the tick rate.*
   Dry run under load 13: `frame_total` p99 243 µs with the stand-in
   authority on the same thread, 0 overruns of 36,000. The quiet-box number
   is deferred, but a 16.7 ms tick has 60× headroom on the loaded figure.
   **Not falsified** on the evidence here; not proven on a quiet box.
3. *The prediction/rollback path cannot be driven through the existing
   ABI.* **Falsified**: the `rollback` mode above, hash for hash, and the
   test `rollback_through_the_abi_is_hash_exact_and_the_ring_follows_the_correction`.

## What could not be established without the engine

- Whether the archive links under UE's MSVC flags; the Windows library set;
  `catch_unwind` across an MSVC boundary. Same as #1043; Linux/clang only.
- `inproc_added` versus `ipc_added` **on the same Windows box**. The Windows
  sidecar leg is still red in the nightly.
- What a real transport costs beside the host on the game thread. There is
  none here; that is the prong's cost, stated, not measured.
- A frame on screen.

## Reproduce

Toolchain: the pinned `rust-toolchain.toml` (1.96.0) — on this box the
rustup shims must precede system rust on `PATH` or the pin is silently
ignored; clang 22 (or `CC=cc` for gcc); Linux x86_64, 32 cores.

```sh
# the three C-driven tests plus the crate's unit tests (debug profile)
cargo test -p orrery_unreal_direct

# the measurement: release staticlib, C consumer, two 10-minute runs,
# reports in docs/data/, rendered with scripts/ipc-report.py
crates/orrery_unreal_direct/spike.sh                  # date label defaults to today
crates/orrery_unreal_direct/spike.sh dry 600 /tmp/x   # a 10-second dry run, reports elsewhere

# one run by hand, after spike.sh has built target/spike-1052/direct_consumer
target/spike-1052/direct_consumer bench --entities 24 --ticks 36000 --warmup 600 \
    --correction-every 12 --report out.json
python3 scripts/ipc-report.py out.json
target/spike-1052/direct_consumer rollback     # the hash-for-hash proof at depth 9
target/spike-1052/direct_consumer smoke        # 120 ticks, one line, thread counts
```
