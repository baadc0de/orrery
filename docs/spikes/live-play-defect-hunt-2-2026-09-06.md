# Live-play defect hunt 2, 2026-09-06 — the join, the reconnect, and the service behind them

The first hunt (`live-play-defect-hunt-2026-09-06.md`) found fourteen defects in
the *run*: what a long session does to a bound seat. All fourteen are filed and
the session-fatal ones are merged. This lane took the territory it did not
reach — **how a seat gets in, what happens when it is refused or raced, and
what a session does when its link goes away and comes back** — plus the
FoundationDB leg `check.sh` does not cover.

**Fourteen defects found and filed: #1144–#1157.** Branch
`docs/live-play-defect-hunt-2`. Nothing under `game/` or `crates/orrery_games`
was touched, the live host was not touched, and nothing was deployed.

## What was found, ranked by what it would cost a session

| # | Defect | Cost |
|---|---|---|
| [#1144](https://github.com/baadc0de/orrery/issues/1144) | A silent dialler wedges the host's join accept loop forever; the next tester's *dial* then fails as if the host were down | **Session-fatal.** One stalled connection closes the door on everyone, permanently on a standing host |
| [#1146](https://github.com/baadc0de/orrery/issues/1146) | A `409`'d concurrent join releases the *running* session's campaign flock, so a third join starts a second harness on the same UDP port | **Session-fatal.** Two joins a second apart corrupt a live session |
| [#1147](https://github.com/baadc0de/orrery/issues/1147) | A reconnect inside the seat-release window is refused with a reservation the retry never refreshes, and nothing retries. **Measured: 12.1 s of hard lockout** | High. The client tells the tester to rejoin *now*, and now is the one time it cannot work |
| [#1145](https://github.com/baadc0de/orrery/issues/1145) | Quitting after a link drop freezes the client for up to ten minutes: the exit path posts every increment synchronously on the main thread at 45 s each | High. Reads as a crash, in exactly the case the retry path exists for |
| [#1148](https://github.com/baadc0de/orrery/issues/1148) | A corrupt `uploads.json` silently becomes an empty one and the next write commits the amnesia, orphaning every queued body | High for evidence. A silent total-loss arm |
| [#1150](https://github.com/baadc0de/orrery/issues/1150) | An unreadable `slots.json` reads as *no reservations*, and the next join overwrites the journal with one row | High. Takes out a cohort, not a seat |
| [#1151](https://github.com/baadc0de/orrery/issues/1151) | One malformed line in `joins.jsonl` silently `404`s every session admitted after it — and a full disk is how it gets there | High for evidence. Also cohort-wide |
| [#1149](https://github.com/baadc0de/orrery/issues/1149) | The upload path classifies no HTTP status and retires nothing: a permanently-refused body is re-POSTed at every launch forever | Medium. A slow bleed that compounds with #1145 |
| [#1156](https://github.com/baadc0de/orrery/issues/1156) | The corrected `StartV1` roster is sent **once** to seats mid-handshake, where the live path sends five — leaving a frozen wrong witness ring | Medium. #1130's failure mode, reached by a different door |
| [#1152](https://github.com/baadc0de/orrery/issues/1152) | An uncaught `OSError` in the join path drops the connection with no HTTP response, *after* the reservation is committed | Medium. Produces "it's broken" and nothing else |
| [#1153](https://github.com/baadc0de/orrery/issues/1153) | The upload store assumes a serial server: fixed `.tmp` name, no lock, and `Content-Length` buffered before it is capped | Medium. Banks a wrong artifact rather than failing |
| [#1154](https://github.com/baadc0de/orrery/issues/1154) | `released_sessions` is never pruned and is re-fsynced in full on every publication, from inside the tick loop | Low. Duration-only bleed on a standing host |
| [#1157](https://github.com/baadc0de/orrery/issues/1157) | The witness join anchor is written with two discarded results: a failed write leaves the client flying a seat the host refused | Low frequency, #1128's symptom. The only discarded results in an otherwise uniformly propagating function |
| [#1155](https://github.com/baadc0de/orrery/issues/1155) | The legacy harness `ssh` command line is unquoted | Low. Config-sourced footgun |

**If only one thing is fixed before a tester flies, fix #1144.** It costs the
session for everybody and it needs no attacker — a stale client, a NAT rebind
or a port scanner all produce it.

## The pattern, stated once

The first hunt's two patterns were *a second implementation that drifted* and
*a bound invalidated by a later landing*. This lane's territory has two of its
own.

**The first: everything on the join path is serial, and nothing on it has a
timeout it did not have to be given twice.** `read_message` is bounded at ten
seconds on both reads (`bridge.rs:189-206`); `accept_bi` immediately above it
is not bounded at all (`bridge.rs:532-535`). The connection-wide idle timeout
that would have covered it cannot, and the file explains why in its own comment
(`bridge.rs:66-72`) — iroh's keep-alive stays enabled, so a *silent* connection
is never an *idle* one. All three accept sites then hold exactly one handshake
in flight, one of them (`main.rs:1540`, the live-host posture) with no timeout
at all. Six lines of asymmetry, and the whole door.

**The second: the service fails open on its own journal and closed on
everybody else's.** `_published_standing_host_membership` returns `None` on a
malformed feed and the phase becomes `restarting`, pinned by a test
(`admission.py:452-483`, `:1666`). `_read_slots` — admission's *own*
authoritative reservation journal — returns `[]` on any `OSError`, with no log
line (`admission.py:419-425`), and the next join then durably overwrites the
file with a single row. Same file, same author, opposite posture, and only the
careful one has a test.

And the first hunt's own first pattern — *a second implementation that drifted
from the first* — recurs once here, in the most self-documenting way available:
`republish_start_roster` (`main.rs:1160-1163`) says in its doc comment that it
uses the "same lane and same JSON as `Swarm::publish_live_manifests_for`", and
copies everything about that publisher except the one property it exists for.
`publish_live_manifests_for` sends five copies a second apart because
"a joiner that is still finishing its handshake when the first copy goes out"
needs a second chance (`swarm.rs:361-365`); the correction sends one, to seats
that are by construction still finishing their handshakes. That is #1156.

Two smaller recurrences of the first hunt's own patterns:

- **A guard that reads the shared dict rather than its own object.** The `409`
  path pops `self.locks[ident]` in a `finally` without checking whose lock it
  is (`admission.py:799-803`) — the same "keyed by the wrong thing" shape as
  #1118's acknowledgement.
- **A lease refreshed on one branch of two.** The reissue returns the same
  session on every `existing` hit (`admission.py:725`) but refreshes
  `expires_at` only inside `reclaimable` (`admission.py:735`), so the retry the
  client explicitly advises is served a dead row.

## What was run, not read

Reasoning from source found the wrong answer twice in this lane. Both are
recorded below. Everything with a number in it was measured.

- **The silent-dialler wedge, against the real `bridge` over loopback iroh.**
  A dialler completing the QUIC handshake on `EXTERIOR_ALPN` and opening no
  stream: `host_prepare` still pending after **40 seconds**, connection still
  open. This is #1144.
- **The starvation, with `main.rs`'s serial accept loop reproduced verbatim.**
  One valid seat admitted before the stall; a second, perfectly well-formed
  seat behind it could not even complete its **dial** — `endpoint.accept()` is
  never polled, so the failure presents as an unreachable host, not a refusal.
- **A hard-killed seat that comes straight back**, four real processes: a
  reservation-backed standing host, a seated external peer, a `SIGKILL`, an
  immediate relaunch and a post-release relaunch. Immediate rejoin refused
  `reservation_slot_occupied` and **exited in 0.5 s**; seat released at
  **t+12.1 s**; rejoin after release reseated cleanly. This is the measurement
  in #1147, and it is retained as
  `gates/p1-swarm/tests/reconnect_blackout.rs`.
- **The FoundationDB leg**, which `check.sh` does not cover, against a real
  single-node cluster on port 4533. **Green: 1143 tests, no skips**, against a
  floor of 541 — `checkpoint_restore` 22, `fence_split` 14, `lease_fdb` 7,
  `intent_commit` 13, `area_load` 14, `persistd_binary` 20, `fdb_gates` 10,
  `shard_handover_fdb` 1, `intent_witness_epoch` 10,
  `standing_invalidation_chain` 4, `ramp_posture_authenticated` 4,
  `ramp_posture_history` 3. Cluster stopped and its directory removed.
- **Two proofs-of-concept against `admission.py`'s own test fixture**, both
  subclassing `AdmissionTests` with no repo file touched. All fifty existing
  tests pass; both PoCs fail. One reproduces the stale reissued lease on both a
  `pending` and an `active` seat (#1147); one reproduces the flock release,
  ending with `session child alive: True` and `a third join could take the
  flock while the session runs: True` (#1146).

Nothing graphical was run and no window could have appeared: no Bevy client
binary was launched, and every measurement above is a library, a headless
binary, or a Python fixture.

## Checked and found clean

Worth as much as the findings — this is what the next hunt does not need to
redo.

**Slot allocation and admission's transaction discipline**

- **Two concurrent reservations cannot be handed the same slot.**
  `free_slots[0]` is chosen ascending under `flock(LOCK_EX)` held across the
  whole read-modify-write (`admission.py:679` through `:715`), and separate
  `open()` calls give separate open file descriptions, so the lock genuinely
  serialises threads inside the one process.
- `slots.json` is written `os.replace` + parent `fsync`
  (`admission.py:914-919`), so the host can never read a torn journal.
- **Clock units agree end to end.** Admission writes wall-clock *seconds*
  (`admission.py:709`); the host compares `row.expires_at <= now_ms / 1_000`
  against `SystemTime`/`UNIX_EPOCH` millis (`exterior.rs:927`, `bridge.rs:569`),
  and `unix_seconds()` carries the same contract in its doc comment
  (`main.rs:690-697`). `released_at` is `SeatReclaim::lost_at()` seconds
  matched against `int(time.time())`. No unit mismatch, no monotonic/wall
  mixing.
- **`slots.json` does not grow.** `_current_slots` (`admission.py:516-525`)
  filters to the current `attempt_id` and persists only that, so dead
  generations are dropped on the next join. Bounded by `humans`.
- **The #1016 double-booking mechanism holds.** `held_slots = active | pending`
  (`admission.py:287-289`) keeps a lobby row alive past its lease;
  `hold_pending` publishes before the host answers (`main.rs:754`,
  `swarm.rs:270-279`); `abandon_live_join` gives the hold back on every
  non-bind exit (`main.rs:780-789`); `record_live_binding` clears the release
  in the same publication as the bind (`main.rs:828-846`). The only hole is the
  lease *value* (#1147), not the membership logic.
- **`window_closed` / `window_ends_at`.** Admission does not read them and does
  not need to: `p1-swarm-always-on.py:302-308` unlinks `attempt.json` the
  moment `window_closed` turns true, and the generation hand-off order keeps
  admission in `restarting` throughout the gap. #1053's seam is sound.
- **`--require-session`, `--require-client-rev`, issuer keys.** Both mandatory
  flags are unconditionally present on the legacy line (`admission.py:783`) and
  pinned by `test_the_harness_is_pinned_to_exactly_the_admitted_session_id`
  (`:1113`). `--require-client-rev` is omitted when falsy on *both* sides
  (`admission.py:784`, `p1-swarm-always-on.py:157`) and admission's own check
  is skipped on the same condition (`:662`) — a consistent opt-in, not a silent
  asymmetry. Supervisor args go through `Popen` as a list with no shell.
- **No bare `except:` and no silent `pass`** in `admission.py`. The single
  `pass` (`:113`) is a documented `BrokenPipeError` in the upload probe.

**Reconnection and evidence under a flapping link**

- **No evidence accumulates while disconnected, and nothing is silently
  buffered.** `advance` returns `TickReport::default()` before touching the
  accumulator unless `JoinState::Joined` (`campaign.rs:1493`), and losing the
  link sets `Closed` on the tick it is noticed (`campaign.rs:1718`). There is
  no buffer, bounded or otherwise, holding disconnected evidence.
- **No double-banking is reachable.** A run-time departure is
  `SeatReclaim::Spent`, so `spent()` is true and `_current_slots` *prunes* the
  row (`admission.py:517-522`); the relaunch mints a fresh session and cannot
  collide. See the corrected lead below — this was the lane's first and wrongest
  conclusion.
- **No spin in the retry path.** `retry_pending_uploads` is a one-shot
  background thread called once at startup (`admission.rs:1785-1803`,
  `main.rs:192`) doing repair → sweep → flush and exiting; `flush_pending`
  (`admission.rs:1556-1562`) makes one attempt per body with no loop and no
  timer. The absence of a backoff is correct, not an omission.
- **#1118's fix is complete and correctly one-shot.** The acknowledgement is
  keyed on `pending.upload_key` (`admission.rs:1492`); the repair targets
  exactly the bare-session-id entries with increment siblings
  (`admission.rs:1828-1845`), skips bodies no longer on disk (`:1846-1849`), and
  is latched by `increment_acks_repaired` (`:1855`, `serde(default)` at
  `:1225`).
- **#1119's fix is complete on both halves.** Client `upload_path`
  (`admission.rs:1287-1293`) and service `_store_upload` suffix
  (`admission.py:969`) agree exactly including the increment-zero unsuffixed
  case; the service validates `increment_of(row) == increment`
  (`admission.py:962`); an identical-bytes re-send is genuinely free
  (`admission.py:977` conflicts only on *differing* bytes). Deployable in
  either order, as documented.
- `durable_write` / `atomic_bytes` on both sides — tmp + fsync + rename +
  parent-directory fsync (`admission.rs:1891-1905`, `admission.py:915-919`).
  `with_extension("tmp")` on `upload-<id>.increment-N.json` yields a unique,
  non-colliding temp name. (The *service* side's fixed `.tmp` is #1153; the
  client's is fine.)
- `build_upload_body` correctly refuses mixed session ids and mixed increment
  indices (`admission.rs:1717-1731`), mirrored server-side.

**Anything sized by the bot count that a human seat's index exceeds** — the
#1132 shape the brief flagged as likely to bite again:

- `clients/regolith`'s admission, campaign and lobby paths have **no
  fixed-size array or `Vec` indexed by seat at all**. `PersistId::new(slot+1)`,
  `Archetype::for_slot`, `campaign_spawn_pose(slot, island_seats)` and every
  replica map are `BTreeMap`s or pure functions. `CampaignConfig::island_seats`
  (`campaign.rs:213-218`) guards with `.max(slot + 1)`, so an under-published
  `humans` cannot size the pose below the seat's own index.
- `gates/p1-swarm` has two bot-count-sized vectors, `samples` and
  `applied_interest_crossings` (`swarm.rs:2289`, `:2305`). The second is
  indexed by `index_of[node]` (`swarm.rs:2641-2644`), which *does* carry
  exterior seats — but the only producer of `HostInterestCrossing` iterates
  `self.bots` (`swarm.rs:3396-3410`), which is `0..config.peers`. **The panic
  is real and remains latent, exactly as #1132 says.** It is not newly
  reachable after #1130/#1131 landed. Re-confirmed, not re-filed.

**Duplicates of the first hunt, confirmed still open, not re-filed** — the
quadratic per-increment telemetry re-upload is #1125; the client's fixed
`telemetry_start` (`admission.rs:536-540`) is never advanced, and the new
detail worth adding to it is that a paused campaign is a *global* consequence:
`free_bytes()` under `MINT_FLOOR_BYTES` pauses all admissions
(`admission.py:665-669`), so one long session's redundant telemetry can close
the campaign to everybody.

## Corrected below: two wrong leads, recorded because they looked right

**The first, and it was nearly filed as the highest-severity find of the
lane.** The chain looked airtight: admission finds a reservation by transport
node and not by session (`admission.py:694`), and returns
`existing["session_id"]` unconditionally (`admission.py:725`); the client has
no in-process rejoin, so a reconnect is a relaunch; a relaunch restarts
`emitted_increments` at zero (`session.rs:516`); and `upload_key` is
`(session_id, increment_index)` (`admission.rs:1247-1252`). Same session, same
key, same filename — a second stint that overwrites the first stint's body on
disk and is then `409`'d forever.

It does not happen, and the reason is one line nobody in the chain had opened.
`_current_slots` (`admission.py:517-522`) filters out every row for which
`membership.spent(session_id, now)` holds, and a *run-time* departure is
`SeatReclaim::Spent` (`swarm.rs:3247`), which never reaches `released_at` and
so is never `reclaimable`. The row is pruned before `existing` is ever
searched. The relaunch mints a fresh session and there is no collision.

What survives is much smaller and is #1147: inside the ~12 s before the host
publishes the release, the row is *not* yet spent, so the same session does
come back — with a lease nothing refreshed. That is a refusal, not a
double-bank.

**The second.** The `assert_eq!` in `LiveMembership::release_seat`
(`swarm.rs:325-330`) panics the host process if a release ever names a session
other than the binding's. It looked reachable via a fast rejoin racing a
pending release. It is not: `reserve_live_join` refuses on
`active.contains_key(&slot)` (`swarm.rs:270`) before any rebind can happen, so
the release always runs first. Left alone.

The lesson is the standing one, and this lane needed it twice: read the cited
line before repeating a claim, including one you reasoned your own way to.

## Where a third hunt should look

- **The rendered client against a real host.** Both hunts have now said this.
  Everything measured across twenty-six findings used a headless runner, a
  stand-in, or a library. #1145 in particular is a *window* freezing, and
  nobody has watched it happen.
- **Concurrency in `admission.py`.** `grep -n "Thread\|concurrent"` finds the
  reaper, the `ThreadingHTTPServer` construction and one probe thread — and no
  test. The server is genuinely threaded and #1146 and #1153 are both what that
  costs. A concurrency fixture would likely pay again.
- **The host with more than one human actually leaving and arriving during a
  run.** This lane measured one seat's departure and return. Two seats
  contending for the release window, and a seat departing while another is
  mid-handshake, are untouched.
- **`check.sh` lints one workspace of thirteen** (#1140), and both
  `gates/p1-swarm` and `clients/regolith` — where twenty of the twenty-six
  findings live — are among the twelve it does not. That is not a coincidence
  worth ignoring.
