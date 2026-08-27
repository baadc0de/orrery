# Multiple humans in one campaign (#563)

Status: PROPOSAL. Nothing here is decided; accepting any of it is
owner-reserved. If adopted, the population and admission-contract changes in
section 4 deserve a short ADR (they change what a campaign session *is*);
the rest is implementation under existing decisions. Verified against `main`
at `a1d01b77`; line numbers cited below are from that commit.

## 0. Recommendation in one paragraph

The harness's single exterior slot is **incidental, not essential** (section
1 establishes this from the code). Generalize the harness to `E` exterior
slots behind one standing endpoint, seat and unseat humans **mid-run**, and
turn admission's per-campaign flock into a **slot free-list** whose leases
expire at the harness attempt boundary. Human slots extend the bot budget;
the friends campaign is configured so the full house is exactly the 8-peer
shard unit (`peers = 5`, `humans = 3`). Play campaigns run `seconds = 3600`;
measurement campaigns keep 900. Impairment is untouched: it is applied
per-packet in the host router to every leg, exterior legs included, and that
survives N humans without modification.

## 1. The crux: is the single exterior slot essential or incidental?

**Incidental.** The claim in `docs/plans/always-on-p1-swarm-host.md:5-6`
("The harness has exactly one external slot and ends after `seconds`") is
two claims glued together, and only the second is structural. "Ends after
`seconds`" is essential: the run loop is `for tick in 0..ticks`
(`gates/p1-swarm/src/swarm.rs:1088`), the report is a function of a bounded
run, and the always-on supervisor exists precisely because one process must
not host indefinitely (`scripts/p1-swarm-always-on.py:4-7`). "Exactly one
external slot" is a data-shape accident. The evidence, read at every layer:

- **The wire is already multi-peer.** Every frame names a swarm index
  (`gates/p1-swarm/src/exterior.rs:107-115`): uplink frames name the
  recipient's slot, downlink frames the sender's, exactly because "one
  connection carries traffic for many island-mates". The uplink sequence is
  per-connection, not per-recipient (`exterior.rs:121-123`) - which means N
  connections each carry their own independent sequence space with no
  interaction. Nothing in the frame grammar, the join handshake
  (`exterior.rs:344-365`), or the ack protocol assumes one exterior exists.
- **The router routes by slot index, uniformly.** An exterior uplink
  datagram enters `router.accept(tick, self.node, recipient_index, ...)`
  like any bot packet (`swarm.rs:656-661`), and delivery dispatches on
  `delivery.to == exterior.index` (`swarm.rs:1034-1041`). An uplink from
  human A addressed to human B's index already flows through the impaired
  router and out the deliver loop; with a slot map instead of an `Option`,
  human-vs-human traffic needs no new path.
- **Admission on the host side is per-connection and slot-agnostic.**
  `host_accept` takes the index as a parameter (`bridge.rs:266-271`), and a
  token-gated campaign skips the deterministic-key check entirely
  (`bridge.rs:283-288`): identity is the issuer-signed token bound to the
  dialler's transport key. The always-on harness is launched with
  `--issuer-key` and **no** `--require-session`
  (`scripts/p1-swarm-always-on.py:56-59`), so N humans' tokens already
  verify against the same policy today; only one of them gets accepted.
- **The witness ring already generalizes over the count.**
  `seed_witnesses` computes `count = bots + exteriors`
  (`swarm.rs:1229`), and a rendered client seats **unanchored** - no
  watcher is armed against it, nothing of it is judged
  (`swarm.rs:1267-1281`). Additional human slots are additional unanchored
  ring members; the bots' reciprocal sets are unchanged in kind.
- **The client is already slot-parametric.** Regolith takes its slot from
  the join reply (`clients/regolith/src/join.rs:44`), derives its entity as
  `slot + 1` (`clients/regolith/src/campaign.rs:407`) and its pose as
  `campaign_spawn_pose(slot, slot + 1)` (`campaign.rs:412`) - the exact
  mirror of the host's `spawn_pose(index, index + 1)` for the exterior
  (`swarm.rs:796-797`). Any slot value round-trips; nothing client-side
  assumes `slot == peers` beyond what admission handed it.

What actually pins the count to one, exhaustively:

| Where | What | Kind of change |
|---|---|---|
| `swarm.rs:521` | `exterior: Option<ExteriorSlot>` | `BTreeMap<usize, ExteriorSlot>` |
| `swarm.rs:790` | `with_external` seats before `run` only | seat/unseat during run |
| `main.rs:607-618` | one `host_accept` before the swarm starts | standing accept loop |
| `swarm.rs:329-330, 1486` | `external: Option<ExteriorReport>` | `Vec<ExteriorReport>` |
| `admission.py:200, 224` | flock + `"slot": c.peers` constant | free-list allocation |
| `swarm.rs:874-877, 917-929` | `if let Some(exterior)` in island/roster | loops over the map |

Every row is a representation change, not a protocol change. Cost estimate
for the pure N-slots refactor (rows 1, 4, 6 and the `Option` call sites -
`node_of` at `swarm.rs:832`, `total_peers` at `swarm.rs:841`,
`collect_delivered_inputs` at `swarm.rs:1006-1011`, `deliver` at
`swarm.rs:1033-1055`, `report` clauses at `swarm.rs:1786-1816`): mechanical,
one to two days including tests. The two subtle pieces are the accept loop
and mid-run seating (section 3) and the admission allocator (section 4).

## 2. Population: where human slots come from

Human slots **extend** the bot budget; they do not come out of it. Reasons:

- Bot identities, poses and the witness ring are fixed at launch
  (`swarm.rs:731-746`: every bot gets `count: config.peers`). Carving humans
  out of `--peers` would mean the bot population depends on who showed up,
  which breaks the determinism the seeded run depends on and makes every
  attempt's denominator a different shape.
- The exterior derivation is already "one past the bots and static":
  slot `k` derives pose from `(k, k + 1)` on both sides independent of how
  many other exteriors exist (`swarm.rs:796-797`,
  `clients/regolith/src/campaign.rs:412`). Extension is free; carving is not.

**The 8-peer ceiling is respected by configuration, not exceeded by
design.** #240's detection scenario is an 8-peer island and #320 calls that
the natural shard unit. So the friends campaign is sized so the *full house*
is the shard unit:

    peers = 5 bots, humans = 3    -> population 5..8 as friends come and go
    peers = 6 bots, humans = 2    -> population 6..8 (duo variant)

A partially occupied island runs *below* 8, which is inside the measured
envelope (the criterion holds at 8, 16 and 32 peers,
`gates/p1-swarm/src/main.rs:101-103`; fewer peers is strictly lighter on
every budget in that table). What the design refuses is population past 8 in
one island: four or more humans is two campaigns, not a bigger island. The
config gains one key:

    [regolith-friends]
    peers = 5
    humans = 3          ; exterior slot capacity; default 1 = today's shape
    seconds = 3600
    loss_pct = 3
    jitter_ms = 100
    always_on = yes

and the harness gains `--external-slots 3`. Exterior slot domain:
`peers .. peers + humans - 1`, entities `peers + 1 .. peers + humans`.

Consequence for the measurement: fewer bots means fewer host-side witnesses.
The ring gives each subject `min(MAX_WITNESS_LINKS, count - 1)` watchers
(`swarm.rs:1233-1240`); at 5 bots + 3 unanchored humans every *bot* is still
watched by 4-7 ring members of which only bots watch (humans neither watch
nor are watched host-side, `swarm.rs:1288-1305`). Bot-vs-bot false-positive
coverage therefore thins relative to an 8-bot island. That is the real price
of the mix and it is #240's stated intent ("bot + human mix"); the session
record must state the mix per attempt so the denominator stays auditable.

## 3. The harness: N seats behind one standing endpoint, seated mid-run

### 3.1 Why mid-run seating rather than a pre-run join window

The alternative - accept up to E dials during a fixed window, then form the
island and run - is less code, but it makes entry latency equal to the
attempt period. At `seconds = 900` that is a tolerable "wait up to 15
minutes"; at the `seconds = 3600` a play session wants, a friend who
arrives late waits up to an hour. Lengthening sessions and window-joining
are jointly unacceptable, and lengthening is the more valuable of the two
for play (section 5). So: seat at any time during the run.

The good news is that the hard half of mid-run join already exists in
spirit: the roster is re-published every simulated second
(`swarm.rs:901-941`), the late-join criterion is a first-class check
(`swarm.rs:1319-1343`), and a rendered human seats unanchored with no
witness re-arming (`swarm.rs:1267-1281`). What is genuinely new:

- **A standing accept task.** Today `main.rs:607-618` awaits exactly one
  `host_accept` and then starts the swarm. Instead: bind, write the
  listening record, spawn an accept loop on the tokio runtime that runs for
  the whole attempt, and hand seated links to the swarm thread over an
  mpsc. The swarm drains it at the top of each tick:

      // swarm thread, once per tick before tick_once:
      while let Ok(seat) = seat_rx.try_recv() {
          self.seat_external(seat.slot, seat.node, seat.anchor, seat.link);
      }

- **`seat_external` = `with_external` + live linking.** `with_external`
  (`swarm.rs:790-822`) only fills the struct; links between bots are made
  once, in `form_island` (`swarm.rs:880-893`), and `refresh_rosters` only
  calls `set_island`, never `bot.link` (`swarm.rs:931-941`; the links
  are made once at `swarm.rs:889-891`). Seating during the run must therefore also do, per bot: `bot.link(node, 1_200)`. Unseat
  reverses it: on `connected == false` (already polled per slot via the
  pump's flag, `bridge.rs` HostLink), remove the `ExteriorSlot`, drop it
  from `index_of`, mark the seat free for the accept loop, and keep its
  `ExteriorReport` row with its connected span. The craft simply stops
  being replicated - the same thing a mid-run bot stall already looks like
  to the island (`swarm.rs:972-978`).
- **Real-time pacing becomes unconditional in `--external-peer` mode.**
  Today `real_time = self.exterior.is_some()` (`swarm.rs:1084`), decided
  once before the loop. A standing host that starts with zero humans would
  sprint the attempt at bot speed and be mid-run or finished when the first
  human dials. The attempt must tick at wall clock from tick zero whenever
  it can accept exteriors. Cost: an empty attempt burns `seconds` of
  real time doing a bot-only sim it used to finish in seconds - which is
  exactly what a standing world is, and hel1's CPU covers it (section 6).
- **Slot identity on the wire.** `JoinReply::Accept` carries the index
  (`exterior.rs:553-561`) and the dialling side refuses a mismatch with
  what it derived (`bridge.rs:469-472`). With N slots there are two
  allocators in the system - admission's free-list and the host's seats -
  and they must not disagree, or the roster's `slot -> nickname` map and
  the client's `entity = slot + 1` split. **Admission is the allocator of
  record**: bump the join handshake to version 4 with an optional `slot`
  field in the identity tail (`exterior.rs:345-352` documents the pattern the
  v3 tail set); the client echoes the slot admission granted; the host
  validates range (`peers <= slot < peers + E`), vacancy, and the token,
  and refuses "slot taken - rejoin" otherwise. A no-show's slot stays
  empty until its lease expires (section 4). This also keeps rejoin
  trivially correct: same lease, same slot, same entity.

### 3.2 What does N slots cost inside the run?

Per seated human, per tick: one `try_recv` drain of the uplink queue
(`swarm.rs:964-968` generalizes to a loop over the map), one router
`accept` per uplink frame, one `deliver_from` per due delivery. All O(1)
per packet, identical to today's single-exterior cost times E. Memory per
seat: one `ExteriorSlot` + two bounded queues of depth 4096
(`exterior.rs:305`) - frames are small (MTU-ish datagrams, the 64 KiB
cap at `exterior.rs:68` is a bound on hostility, not a working size), so
queue worst case is ~4096 x ~1.3 KB = ~5 MB per direction per seat if fully
backed up, in practice near zero. Two pump tasks per connection on the
existing runtime. Nothing here moves the needle against the bot cost.

One real scaling wart: `refresh_rosters` prints a `replica_scope_capture`
line per bot per exterior per simulated second (`swarm.rs:918-928`). At 3
humans x 5 bots x 3600 s that is 54 000 log lines per attempt. Gate it
behind the existing debug env var or drop it to one exterior.

## 4. Admission: the flock becomes a free-list

### 4.1 What the flock becomes

The flock exists to stop two *harnesses* fighting over one campaign
(`admission.py:197-200` refuses; `admission.py:248` documents that the
child/reaper holds it). With a standing supervisor-owned host there is
exactly one harness per campaign by construction
(`scripts/p1-swarm-always-on.py:62-84`), so the process-exclusion job is
gone on the always-on path. What remains is a smaller job: **serializing
slot allocation**. Keep the flock, shrink its hold to the allocation
critical section, and keep the whole current behavior unchanged for
non-always-on campaigns (the per-join SSH-spawned harness at
`admission.py:225-246` genuinely still needs exclusion; do not touch it).

### 4.2 The allocator

Per always-on campaign, admission keeps a lease table (in memory, mirrored
to `<state>/<campaign>/slots.json` so a service restart does not orphan
seats):

    lease := {slot, session_id, account, node, nickname, expires_at}
    domain := {peers, ..., peers + humans - 1}

    join(campaign, request):                     # guards as admission.py:180-193
      with flock:                                # held microseconds, not seconds
        drop leases with expires_at <= now
        if a live lease has this node: return it # rejoin: same slot, same token flow
        free := domain - {l.slot for live leases}
        if free is empty:
            raise Refusal(409, "campaign_full",
                          "All player slots are taken - try again soon.",
                          retry_after_s = seconds_until_earliest_expiry)
        slot := min(free)
        mint + sign exactly as today (admission.py:203-206)
        record lease; append to joins.jsonl
      return join reply with "slot": slot        # instead of the constant c.peers

`GET /v1/campaigns` replaces the binary `busy` (`admission.py:119`) with
occupancy: `"state": "open", "slots_free": len(free), "slots": humans`.
`campaign_busy` survives only on the legacy path.

### 4.3 Lease lifetime: the attempt boundary, not join + seconds

Today `_release_always_on` sleeps `c.seconds` from *join time*
(`admission.py:292-298`), while the supervisor restarts on its own cadence.
The two clocks drift: a join minted late in an attempt holds the campaign
locked across the restart, refusing a rejoin while nobody is playing (see
Findings, F1). The free-list fixes this by construction: the supervisor
writes `attempt.json` (`{"started": epoch_s, "seconds": N}`) beside
`listening.txt` at child spawn (`p1-swarm-always-on.py:70-76` is the spot),
admission reads it over the same SSH path it already uses for the listening
record (`admission.py:285-290`), and every lease minted during an attempt
gets `expires_at = started + seconds + restart_delay`. A restart therefore
expires the whole table at once, everyone rejoins against the fresh attempt,
and rejoin-before-expiry (disconnect, crash, alt-F4) returns the same slot
with no operator action. A leaked slot - client got a token, never dialled -
costs one empty seat for at most one attempt. The roster
(`admission.py:255-272`) already treats missing rows as a real answer, so an
empty seat needs nothing: rows exist only for leased slots, keyed by the
leased slot number, several humans instead of one
(`session_roster`, `admission.py:137-156`, gains a human row per lease).

## 5. Session length and what it costs the measurement

900 s is right for a measured run and wrong for friends: the always-on
supervisor kills the child at `seconds` and every in-progress session with
it, explicitly and by design (`always-on-p1-swarm-host.md:39-42`). Fifteen
minutes is inside one good fight. Proposal: **play campaigns set
`seconds = 3600`**; the criterion campaigns keep their own config. Costs,
stated:

- **Bankability granularity.** Hours bank per completed attempt; a host
  crash at minute 59 loses four times more than at 900 s. Mitigation is not
  seamless handover (rejected in `always-on-p1-swarm-host.md:41-42` and
  still rejected here): record per-slot connected spans in the report
  (section 3.1 unseat keeps the row), and let the banking policy count
  spans from a *completed* attempt. The partial-attempt rule is unchanged.
- **Impairment evidence cadence.** #240 requires impairment verified
  applied per session. The report already serializes the configured profile
  (`router.rs:47-62`, `swarm.rs:1456`) and the router's drop counters
  (`swarm.rs:432`); a 3600 s attempt simply has one evidence bundle per
  hour instead of four. Per-slot uplink delivered/dropped counts fall out
  of the ack path for free (`swarm.rs:662-672`) and belong in the per-slot
  `ExteriorReport` so a human session's impairment is verified for *that
  leg*, not inferred from the aggregate.
- **Attempt report size.** One raw.json per hour instead of four; strictly
  fewer bytes per day.

Impairment itself is unaffected by N humans: it is one seeded model applied
per-packet to every leg in the host router - exterior uplink included, "the
whole point of the bridge" (`swarm.rs:615-620`) - so each human experiences
independent per-packet 3% loss and jitter draws from the shared profile. It
is per-peer in effect, shared in configuration. Nothing to change.

## 6. What breaks first on hel1

The standing pieces live on a 1 vCPU / 1.9 GB / 38 GB box
(`docs/plans/campaign-admission-service.md:789-798`; which box currently
runs the harness itself is Unverified U2 - the arithmetic below assumes the
worst case, everything on the 2 GB box).

- **Bandwidth: never the ceiling.** Worst measured peak upload per peer is
  ~921 kbps at 32 peers under the criterion profile (`main.rs:148-152`);
  at population 8 with interest gating it is far lower. Three human legs at
  a generous 1 Mbps each way is ~6 Mbps on a box with effectively
  unmetered bandwidth (`campaign-admission-service.md:790-792`).
- **CPU: fine at population 8, and the reason to stop there.** The dev
  measurement is 32 peers x 300 simulated seconds in ~10 wall seconds
  (`main.rs:98-99`), i.e. ~960 peer-sim-seconds per wall second on dev
  hardware. Real-time population 8 needs 8. Even granting the 1 vCPU box a
  10x handicap versus the dev machine, that is ~96 available against 8
  needed - 12x headroom. The number that does *not* have headroom is the
  budget table itself: 32-peer islands ran at 973 kbps against the 1 Mbps
  allowance (`main.rs:136-141`). Population is capped by the shard unit
  long before hel1's CPU is the binding constraint.
- **Memory: the first thing to actually measure.** The harness is one
  process holding `peers` headless Bevy apps plus the in-process
  adjudication cluster; its RSS on the box is not recorded anywhere I can
  cite (U3). 1.9 GB minus OS, sshd, admission (if co-located) leaves
  roughly 1.2-1.5 GB for the harness. If 8 in-process peers fit today at
  900 s (they demonstrably run today), 5 bots + 3 seats fits with margin -
  but a 3600 s attempt gives any per-tick leak four times longer to
  compound. First deployment step: read `/proc/<pid>/status` VmRSS at
  minute 5 and minute 55 of one long attempt before inviting anyone.
- **Disk: bounded but needs a retention rule.** `attempt-*/raw.json`
  accrues one directory per attempt forever
  (`p1-swarm-always-on.py:70-76`); at 3600 s that is 24/day against 38 GB
  with a 10 GB mint floor (`admission.py:33`, checked at
  `admission.py:187-190` - note the floor gates *admission's* disk, which
  only helps if admission and the harness share the box). Add
  `tmpfiles.d`-style pruning of attempts older than N days to the runbook.

Failure modes, named: (a) host restart drops all humans at once - expected,
explicit, leases expire together, everyone rejoins (section 4.3); (b) one
human's queue backs up - bounded queues drop and count
(`swarm.rs:571-580`), the report's `downlink_dropped` clause names the leg;
(c) admission dies mid-attempt - `slots.json` restores leases, tokens
already minted keep verifying (the host checks the issuer, not admission's
memory, `p1-swarm-always-on.py:59`); (d) two clients present the same slot
- host seats the first, refuses the second by vacancy check (section 3.1);
(e) slot leak - self-heals at the attempt boundary, worst case one empty
seat for one attempt.

## 7. Decomposition

Ordered; each piece lands and tests alone. Sizes are working-days-shaped.

1. **Harness plural slots (mechanical, ~1-2 d).** `Option<ExteriorSlot>`
   -> `BTreeMap<usize, ExteriorSlot>`; loop the `Option` call sites
   (`swarm.rs:521, 832, 841, 874, 904, 917, 965, 1006, 1033, 1084, 1229,
   1262, 1486`); `ExteriorReport` -> `Vec` with per-slot connected span
   and uplink delivered/dropped; report clauses per slot
   (`swarm.rs:1786-1816`). Still one seat filled pre-run; behavior
   identical at E=1. No wire change. Blocks 2 and 3.
2. **Standing accept loop + mid-run seat/unseat (subtle, ~3-5 d).**
   `--external-slots E`; accept loop task; seat channel drained per tick;
   `seat_external` with live `bot.link`; unseat on `connected == false`
   freeing the seat; unconditional real-time pacing in `--external-peer`
   mode; join handshake v4 carrying the granted slot; client drops the
   `assigned == derived` insistence in favor of adopting the reply
   (`bridge.rs:469-472`, `clients/regolith/src/join.rs`). This is the
   piece with the design risk; everything else is bookkeeping around it.
3. **Witness/report generalization (moderate, ~1 d, with 2).** Ring count
   over the seat map at seed time; humans stay unanchored; a human seated
   mid-run joins the ring *not at all* for that attempt (unanchored and
   unwatched, the existing rendered-client posture, `swarm.rs:1267-1305`)
   - state that in the report rather than pretending.
4. **Admission free-list (mechanical-plus, ~1-2 d, parallel with 1-3).**
   `humans` config key; lease table + `slots.json`; `campaign_full`;
   rejoin-returns-same-slot; occupancy in the listing; multi-human
   `session_roster`; `attempt.json` written by the supervisor and read for
   `expires_at`; retire the fixed `_release_always_on` sleep. Pure Python
   with the existing in-file unittest pattern (`admission.py:395+`).
5. **Config and ops (small, ~0.5 d, last).** Friends campaign stanza
   (`peers = 5, humans = 3, seconds = 3600`); runbook amendments to
   `always-on-p1-swarm-host.md` (the "exactly one external slot" sentence
   becomes "E slots, one attempt"); attempt-directory retention; the RSS
   check from section 6 as a deploy step.

Dependency graph: 1 -> 2 -> 5; 3 rides with 1-2; 4 is independent until 5
wires the config through. The owner can hand 1+3, 2, and 4 to different
implementers with only the v4 handshake shape (slot field) agreed between
2 and 4 up front.

## 8. Strongest argument against

**This design converts a measurement harness into a game server, and the
join-window alternative was most of the value for a tenth of the risk.**
Concretely: piece 2 makes the swarm's membership dynamic. Every invariant
in the run loop was written under "membership fixed at island formation" -
the witness ring is seeded once (`swarm.rs:1225-1310`), pacing is decided
once (`swarm.rs:1084`), links are formed once (`swarm.rs:880-893`), and the
report's clauses assume the exterior existed for the run
(`swarm.rs:1786-1816`). Mid-run seating touches all four, and the bug class
it opens (a seat wired into routing but not into the roster, or vice versa)
is exactly the "joined-but-deaf slot" the bridge module documents spending
an afternoon on (`bridge.rs:31-32`, `bridge.rs:104`). A pre-run join window
at `seconds = 900` - "everyone presses join within the same minute, next
round starts at most 15 minutes later" - needs only pieces 1, 3, 4, keeps
membership static, and for a coordinated group of friends on a voice call
the entry-latency argument in 3.1 is weaker than I have made it: friends
*do* coordinate joins. The counter-counter: the window design hard-couples
session length to entry latency forever, so the moment sessions lengthen
(and section 5 argues they must), it stops being viable; better to pay the
dynamic-membership cost once, now, with the full house capped at 8 keeping
the blast radius small. But if the owner weighs this-week delivery above
mid-fight join, the window variant is the honest fallback and pieces 1, 3,
4 are common to both.

A second, sharper objection: **mid-run seating makes the per-attempt
denominator non-constant**, and #240's whole discipline is an auditable
denominator. The answer is per-slot connected spans in the report (pieces
1-2) rather than per-attempt population - but that is a change to what a
"session" means in the session record, and it is the reason section 0 says
this deserves an ADR rather than landing as plumbing.

## 9. Findings while reading (no code changed)

- **F1:** `_release_always_on` frees the campaign a fixed `c.seconds` after
  *join time* (`admission.py:223, 292-298`), while the supervisor restarts
  on its own attempt cadence (`p1-swarm-always-on.py:68-83`). A client who
  joins mid-attempt and is dropped by the restart is refused rejoin with
  `campaign_busy` until the stale timer expires - a lockout with nobody
  playing, up to `c.seconds` long. Section 4.3's attempt-boundary leases
  subsume this; worth knowing it exists today.
- **F2:** `replica_scope_capture` logging in `refresh_rosters`
  (`swarm.rs:918-928`) is unconditional and scales as bots x exteriors x
  seconds; at the proposed play shape it is 54k lines/attempt. Gate it.
- **F3:** the always-on harness accepts any valid issuer token with no
  session pinning (`p1-swarm-always-on.py:56-59` passes no
  `--require-session`), so "one admission at a time" is enforced only by
  admission's flock, not by the host. Anyone holding an unexpired token
  (1 h TTL, `admission.py:224`) from a *previous* window can race the
  intended client for the single seat after a restart. The free-list plus
  slot-in-handshake (sections 3.1, 4.2) closes this; today it is a
  benign-population assumption worth stating.

## 10. What I could not verify

- **U1:** the live `/etc/orrery/campaigns.conf` values (peers, seconds,
  loss, jitter of the deployed campaign) - the file lives on the box, not
  in the tree. Assumed `peers = 8, seconds = 900, loss_pct = 3,
  jitter_ms = 100` from the test fixtures (`admission.py:395`,
  `p1-swarm-always-on.py:105`) and the issue text.
- **U2:** which machine runs the always-on harness today. The runbook
  installs it on "the campaign host" and admission reads
  `/var/lib/orrery-p1-swarm` over SSH (`admission.py:214-217`);
  `campaign-admission-service.md:789-798` names only the relay box as
  permanent. Section 6 assumes the 2 GB box as worst case.
- **U3:** the harness's actual RSS and per-attempt raw.json size on the
  box; no figure exists in the tree to cite. Section 6 makes this the
  first deploy check rather than guessing.
- **U4:** whether Regolith's client tolerates a `JoinReply::Accept` index
  other than the `--slot` it was launched with without the piece-2 change
  - `bridge.rs:469-472` refuses a mismatch for the *runner*; the client's
  own accept path (`clients/regolith/src/net.rs:306-310`) appears to adopt
  the reply, but I did not trace every consumer of `config.slot`.
- **U5:** the deployment status line in the brief ("deployed and live
  today") versus the issue text ("#560 merged today and is not yet
  deployed", written earlier). I verified only that the code and runbook
  are at `main`; the box state is invisible from here. Nothing in the
  design depends on which is true, only the rollout order does.
