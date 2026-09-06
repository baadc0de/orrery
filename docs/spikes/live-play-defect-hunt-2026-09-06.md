# Live-play defect hunt, 2026-09-06 — what a long session breaks that a fixture cannot

Nobody has flown `playtest-2026-09-04` yet, and a tester is worth one or two
shots a day. This lane spent its time looking for the class of defect #1112
belongs to: **an invariant that holds in a short fixture and fails in a long
session.** Branch `docs/live-play-defect-hunt`. Nothing under `game/` was
touched; the live host was not touched and nothing was deployed.

**Fourteen defects found and filed.** Three small ones fixed here; the rest are
reported, not repaired, because finding was the deliverable.

## What was found, ranked by what it would cost a session

| # | Defect | Cost |
|---|---|---|
| [#1128](https://github.com/baadc0de/orrery/issues/1128) | The external peer's island roster freezes at its spawn cell: a human seat stops replicating ~20 s in and never resumes | **Session-fatal.** The player is a ghost to everyone else after twenty seconds |
| [#1129](https://github.com/baadc0de/orrery/issues/1129) | Two human seats are mutually invisible for the whole session; every Meta frame is dropped | **Session-fatal.** Two testers in one campaign cannot find each other |
| [#1119](https://github.com/baadc0de/orrery/issues/1119) | A session longer than five minutes banks only its first five: the admission service `409`s every increment after the first | **Highest for evidence.** 55 of every 60 minutes never reaches the service |
| [#1121](https://github.com/baadc0de/orrery/issues/1121) | With nothing selected, the client locks — and can fire — at the headless pilot's tick-scheduled targets, including a real neighbouring seat | **Highest for feel.** Present from the first tick, re-arms on every miss |
| [#1118](https://github.com/baadc0de/orrery/issues/1118) | Every banked increment after a seat's first is never acknowledged: silent re-upload forever, and lost evidence when increment zero fails | High. **Fixed here** |
| [#1130](https://github.com/baadc0de/orrery/issues/1130) | The external peer witnesses by NodeId order while the host arms by slot order: at 32 peers five of seven armed watches are dark, and coverage still reads 100% | High. The banked hour is not witnessed and the report says it is |
| [#1131](https://github.com/baadc0de/orrery/issues/1131) | The bridge frame cap is half what the witness control lane may emit, and both ends fail silently while `connected` stays true | High if reached; blocked behind #1130 today |
| [#1125](https://github.com/baadc0de/orrery/issues/1125) | Every increment re-uploads the whole session's telemetry: quadratic bytes, `413` past about ninety minutes | Medium. Masked by #1119 today |
| [#1123](https://github.com/baadc0de/orrery/issues/1123) | The jitter sample vector grows unbounded and is sorted twice a second | Medium. A hitch that arrives partway through and worsens |
| [#1120](https://github.com/baadc0de/orrery/issues/1120) | Sixteen minutes of outward flight latches `arithmetic_overflowed` and freezes the trail | Medium. Permanent corruption of canonical state |
| [#1132](https://github.com/baadc0de/orrery/issues/1132) | The swept interest margin never fires for a human seat — the one participant the flag was bought for | Medium. Being hit by a craft the client was not told about |
| [#1124](https://github.com/baadc0de/orrery/issues/1124) | The rock/pickup/bloom domain has no stage-1 false-positive coverage, and the fixture that would give it violates the invariant on every tick | Medium. A hole, not a break |
| [#1133](https://github.com/baadc0de/orrery/issues/1133) | Three router counters incremented and read by nothing, including the head-of-line tax that would explain a failed drain-horizon clause | Low. Dead telemetry reads as evidence |
| [#1126](https://github.com/baadc0de/orrery/issues/1126) | `afk_seconds` reports the trailing idle streak, not the total | Low. A wrong number in the artifact a playtest exists to produce |

**If only one thing is fixed before a tester flies, fix #1128 and #1129.**
Everything else costs evidence or polish; those two cost the session itself.

## The pattern, stated once

Two patterns, and neither is subtle.

**The first: a second implementation that drifted from the first.**
`gates/p1-swarm/src/peer_runner.rs` is the external-peer path, and it is a
re-implementation of what a bot does in-process. Four of the fourteen findings
(#1128, #1129, #1130, #1132) are places where it has drifted — a roster never
refreshed, a filter sized on the bot count, a witness set never configured, a
`let _ =` that discards the crossing the flag exists to produce. Every one is
invisible in-process, and the tree's only real-peer fixture runs for **eight
seconds** (`gates/p1-swarm/tests/external_join.rs:161`), which is under the
sixteen seconds it takes a craft to leave the cell it spawned in.

The external peer is the tester. Everything about it is under-exercised.

**The second: a bound whose justification was true when written and was
invalidated by a later landing nobody reconciled it with.**

- The upload map was re-keyed by upload key in #1048; the acknowledgement write
  was not (#1118).
- `p4-ledger.sh` was taught about increments in full; `scripts/admission.py`
  was not — the word does not appear in it once (#1119).
- The trail's `i16` metre had "more than a tenfold range margin" against a
  1 km island; #955's tether then made outward flight unbounded by design
  (#1120).
- `Order::Grab` and `pitch_urad` were gated out of the human intent path with
  comments explaining exactly why an unbidden bot target must not pass
  through; the arm immediately below them passes one through (#1121).

- The bridge's frame cap "exists to bound a hostile or desynced length field,
  not to police the senders, who never approach it"; the witness control lane's
  repair budget is about twice it (#1131).

The rest are duration alone: an unbounded accumulator (#1123), a quadratic
re-read (#1125), a coverage hole nothing crosses in fifteen seconds (#1124),
and three counters wired to nothing (#1133).

## What was run, not read

Reasoning from source found the wrong answer at least once in this lane (see
"corrected below"), so the findings above that carry numbers carry measured
ones.

- **`crates/orrery_games` scenario play, 60 simulated minutes × 4 scenarios**
  (216,000 ticks each, release build, ~6.6 s wall). This is what surfaced
  #1120. The whole shipped corpus is 180/180/600/600 ticks, and
  `WORLD_SCENARIO` is 900 — **the longest fixture in the tree is fifteen
  seconds of play.**
- **The world module, 108,000 ticks (30 simulated minutes)**, with every
  stage-1 flag classified by the state that raised it. This is what surfaced
  #1124.
- **A single Interceptor at full outward thrust, up to three simulated hours**,
  stepping the real ruleset one tick at a time. Latched at 974.2 s. This is
  the measurement behind #1120.
- **The upload acknowledgement path, against a live multi-shot HTTP service**
  — two increments of one session, both 204'd, the second never marked
  acknowledged. This reproduced #1118 before it was fixed and is now a
  regression test.
- **`Admission._store_upload` driven directly** with three increment bodies of
  one session id: increment 0 stored, 1 and 2 refused `409`. This is #1119.
- **Every `[[bin]]` in the workspace actually started** — looking for another
  #1105. `orrery-sidecar` with `--serve`/`--stand-in-remote`, `orrery-observer`
  against that live sidecar for 120 frames, `orrery-coordinator`, `persistd`,
  and the five smaller tools to argument parsing. All reach their first tick.
- **The `IntentPipeline` driven with `Controls::default()`** across the pilot's
  four-scenario schedule, printing the `Lock` target each seat emits. This is
  #1121.
- **The p1-swarm gate, for about 46 minutes of wall time**, including an
  8-peer and a 32-peer witnessed impaired **hour** each
  (`--seconds 3600 --witness --impaired --enforce`, 4.5 and 12 min wall), a
  swept-margin hour with the live-host flags, and — the one that mattered — a
  **two-process external join duration series** at 8 / 20 / 40 / 60 / 120 and
  **900** simulated seconds, measuring uplink frames per second in each window.
  That series is #1128, #1129 and #1130.

Nothing graphical was run and no window could have appeared: the Bevy client
binary was built but never launched, and every measurement above is a library
or a headless binary.

## Checked and found clean

Worth as much as the findings — this is what the next hunt does not need to
redo.

**Duration and accumulation**

- The four player scenarios at 216,000 ticks each raise **zero** stage-1 flags.
  Solo, duel, island and island-lossy are clean over an hour.
- The world module over 30 simulated minutes raises **no** rock, pickup or
  bloom violation of any kind other than the fixture's own `next_bloom_tick`
  (#1124). The module itself looks sound over duration.
- `Executor::insert_observed` / `SimulationHost::install_state_observed` — the
  #1112 seam. Every live caller outside `campaign.rs` (which another lane owns)
  stamps the observation tick. The only unstamped `install_state` calls are in
  tests and in deterministic setup, which is what the doc comment says it is
  for (`crates/orrery_sim_host/src/lib.rs:951-957`).
- Regolith's `u16` fields — `expires_at`, `ttl_remaining`, `claimed_at`,
  `cooldown`, `respawn_in`, `lock_progress`, `cover_claim_cooldown`. Every one
  is pickup- or craft-relative, not an absolute tick, so none wraps at the
  18-minute mark a `u16` tick counter would.
- `hearsay.rs` / `contact_arrows.rs` staleness — the closest analogue to #1112.
  Both stamp and age against the *host's* clock, `expire()` bounds the map by
  seat count, and `fact_age_ticks: u16` cannot wrap against a 900-tick horizon.
- Entity lifecycle across the client: `sync_rendered_state`,
  `ensure_local_body`, `ensure_rock_bodies`, `sync_ship_labels` each despawn on
  state absence; no per-entity map survives a despawn. Overlay pools are fixed
  size.
- `orrery_witness`'s `prune_if_due`/`prune` — retention is amortised and every
  accumulating map is bounded.

**Silent failure**

- `campaign.rs:2205-2333` `persist_and_queue`, the #1051 disposition seam:
  every arm logs, including the double-`None` case. No silent disposition
  remains.
- `clients/regolith/src/lib.rs:3201-3225` — the former `if let` with no `else`
  now has an explicit `None => error!` arm.
- `orrery_persist_client`'s `uplink.rs`, `plugin.rs`, `feed.rs` — no `let _ =`
  or `.ok()` swallowing an I/O or network error.
- `net.rs` — sequences are `u64`, `MAX_FRAME_BYTES` is checked on both read
  paths, `drain_downlink` drains fully every call, and both reader and writer
  log and clear `connected` on failure. #947's silent break is genuinely fixed.
- `durable_write` / `sync_parent_directory` — tmp + rename + fsync, with the
  Windows carve-out correctly best-effort.

**Shipped binaries** — none is another #1105. See the run list above.

**p1-swarm, over an hour of simulated time each**

- Router impairment determinism (`router.rs:190-215`, `:320-336`) — packet-identity
  keyed, `occurrences` pruned per tick. No duration dependence.
- `UploadMeter.per_peer` — `forget_departed_links` is registered in both
  `orrery_net/src/plugin.rs:111` and the gate at `bot.rs:903`. Pruned.
- `RateMeter::advance` (`budget.rs:229-259`) — bucket ring, no wrap at any
  realistic session length.
- `replica_seen_at` / `replica_authorities` TTL prune (`bot.rs:1154-1168`) and
  `demoted_at` horizon prune (`bot.rs:1526-1540`) — both bounded.
- `deferred_live_manifests` (`swarm.rs:2958-2970`) — `publishes_left` cannot
  underflow; `retain` drops at zero. `armed_external_watches` pruned on seat
  release (`swarm.rs:3047`).
- Cell-edge grid agreement across an external run — `cell_edge_m_for_session`
  (`main.rs:604-609`) forces the campaign 512 m edge on both sides. No mismatch.
- The 8-peer and 32-peer witnessed impaired hours: `stranded_in_flight 0`,
  deferral ledger balances, chain gaps flat at 11.4/s from 60 s through 3600 s.
  The 32-peer hour's two failures are the documented island-formation band, not
  duration.
- Real-time metronome drift (`swarm.rs:3134-3139`, no catch-up) — 905.8 s wall
  for a 900 s run, 0.64%. Not worth fixing.

## Corrected below: a wrong lead, recorded because it looked right

Seeing honest bot pilots reach 72 km from a 1 km island boundary, this lane
first concluded the tether was broken. It is not. `apply_tether`
(`crates/orrery_games/src/regolith/craft.rs:452-472`) acts per axis, and at the
moment of the reading the craft was 32 km out on **y**, where its outward speed
was 21 m/s — below the design escape speed of 33.3 m/s. The three-digit speeds
in the same sample were on x and z, where the craft was *inside* the boundary
and correctly untethered. The tether is doing exactly what
`TETHER_ESCAPE_SPEED_MMS` says it should.

The real finding was the opposite of the first one: the tether works, and
because it works — because "leaving stays possible; it stops being free"
(`crates/orrery_games/src/regolith/mod.rs:176-178`) — the world is unbounded,
and it is the trail's `i16` metre that no longer has the margin its comment
claims. That is #1120.

The lesson is the standing one: read the cited line before repeating a claim,
including one's own.

## Fixed in this lane

Small and safe only; the hunt did not stop for them.

- `clients/regolith/src/admission.rs` — #1118. The acknowledgement is written
  under `pending.upload_key` rather than `pending.session_id`, with an `else`
  arm that says so if the entry is ever absent. New regression test
  `a_banked_increment_records_its_own_acknowledgement`, plus the multi-shot
  `upload_test_service` harness it needs.
- `clients/regolith/src/grab.rs` — the out-of-reach pickup caption carried a
  U+00B7 middle dot. This client loads no font asset, so it drew as a box, in
  the common "in view, out of reach" state. Now ASCII, with the same
  `is_ascii()` assertion `anchor.rs` and `legend.rs` each keep for their own
  lines.
- `clients/regolith/src/hud.rs` — `{:#x}` after a literal `#` rendered
  `#0xa1000015b50002` where the arms above print `#3`. Now `{:x}`.

Left deliberately unfixed: `clients/regolith/src/lib.rs:1879`, where a click
that misses unconditionally clears the player's lock. It is a real defect and
it is what re-arms #1121, but whether a miss should deselect is a design call,
not a lane's.

## Where a next hunt should look

- **The fixtures, not the code.** Two numbers explain most of this page: the
  longest ruleset scenario is **900 ticks (15 s)** and the only real-peer
  fixture is **8 simulated seconds**. Six of the fourteen findings needed
  nothing more sophisticated than running past those. A single long-run
  scenario in the battery would have caught #1120 and #1124 outright; a
  thirty-second external join would have caught #1128.
- **Two humans, for longer than a second.** #1129 was sitting behind a
  one-second fixture that asserted only seat bookkeeping.
- `crates/orrery_net`'s own duration behaviour was only sampled through the
  gate, not hunted directly.
- The rendered client against a real host. Everything here used the headless
  runner or a stand-in, which is precisely why #1131 could not be observed
  firing.
