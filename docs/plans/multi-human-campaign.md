# Multi-human campaign proposal (#563), amended by join-while-running (#681)

Status: the original proposal remains design history; #681 reverses its
lobby-only product decision. This document does not accept or amend an ADR and
does not authorize deployment.

## 1. Recommendation

Build one bounded, cohort-started campaign attempt with exactly eight seat ids:

```text
island_seats = 8
human_seats  = 4                 # slots 4, 5, 6, 7
bot_seats    = island_seats - human_seats = 4   # slots 0, 1, 2, 3
lobby        = 90 s from the first authenticated arrival, or until all 4 humans connect
run          = 3,600 s after Start
restart      = 5 s after the attempt exits
```

Humans may join one standing host whenever a human seat is unbound. The lobby
duration only forms the initial cohort: its clock starts after the first
authenticated arrival, a full cohort starts immediately, and an empty host
waits indefinitely. Each join receives a `StartV1` membership snapshot at the
host's current tick. Existing clients receive the same current-tick shape when
membership changes, so outbound replication and witness recipients follow the
live set in both directions.

An unbound slot has no pre-join simulation history. The joining client adopts
the manifest tick and its slot's deterministic spawn snapshot, signs an
epoch-zero anchor at that tick, and continues the same input-log chain locally.
Host bot witnesses arm from that claim and state. This uses the existing
`StartV1.tick` field and Meta lane; #681 changes semantics but adds no wire
field, frame, protocol crate type, ALPN, or ruleset behavior.

The eight-seat ceiling is a campaign invariant, not a claim that every attempt
contains eight live peers. D6 puts populations through eight in the full-mesh
regime and changes topology at nine (`docs/adr/0006-population-adaptive-topology.md:7-15`).
Issue #320's game goal is density, while the P4 detection leg is specifically an
eight-peer island (`docs/11-roadmap.md:851-859`). Four bots preserve a useful
crowd when two friends arrive; four humans make a full eight-peer island when a
party fills the lobby. The separate detection gate remains exactly eight peers;
a six-person play attempt is human-cohort evidence, not a substitute detection
run.

## 2. The single exterior slot is incidental, but multiplicity is not mechanical

The one-slot property is not a wire or QUIC requirement. It is an implementation
specialization repeated at four layers.

1. A frame already addresses a swarm index. Uplink frames name a recipient and
   downlink frames name a sender, precisely because one connection carries
   traffic for several island-mates (`gates/p1-swarm/src/exterior.rs:30-38`,
   `gates/p1-swarm/src/exterior.rs:107-117`). The datagram sequence is explicitly
   connection-local, not recipient-local (`gates/p1-swarm/src/exterior.rs:119-129`).
   Giving each human its own connection therefore gives each its own sequence
   without changing the frame grammar.
2. The bridge already describes itself as one real iroh connection "per external
   peer" and returns one independent `HostLink` with bounded queues
   (`gates/p1-swarm/src/bridge.rs:1-2`,
   `gates/p1-swarm/src/bridge.rs:261-272`,
   `gates/p1-swarm/src/bridge.rs:374-388`). An accept loop can create N such
   links. No socket-level multiplexing is needed.
3. The host calls `host_accept` exactly once, passes `config.peers` as the one
   accepted index, and then stores one link through `with_external`
   (`gates/p1-swarm/src/main.rs:564-636`). This is a startup choice, not a
   transport limit.
4. `Swarm` stores `exterior: Option<ExteriorSlot>`, derives total population by
   adding zero or one, and branches on that option throughout routing and reports
   (`gates/p1-swarm/src/swarm.rs:767-776`,
   `gates/p1-swarm/src/swarm.rs:779-843`,
   `gates/p1-swarm/src/swarm.rs:1031-1063`,
   `gates/p1-swarm/src/swarm.rs:1485-1499`). This is the deepest landed
   constraint: changing `Option` to a slot-indexed collection reaches formation,
   interest refresh, send collection, delivered-input lookup, delivery, witness
   assignment, liveness criteria and serialization.

The conclusion is therefore:

```text
essential to exterior protocol or island architecture: no
incidental one-slot specialization in the current harness: yes
cost of N slots: a coordinated host/client/admission/report change, not Vec<T>
```

The existing runbook's statement that making one process host indefinitely
would contradict the harness interface is true of that interface
(`docs/plans/always-on-p1-swarm-host.md:3-9`); it is not evidence that one
external connection is fundamental. This proposal changes the finite attempt
interface additively and keeps the supervisor as the lifetime boundary.

## 3. Population and slot model

### 3.1 Do not overload `--peers`

Today `--peers` is the number of bots created by `Swarm::new`; the external
craft is appended afterward (`gates/p1-swarm/src/main.rs:215-222`,
`gates/p1-swarm/src/swarm.rs:723-776`,
`gates/p1-swarm/src/swarm.rs:790-820`). Preserve that meaning for existing gates.
Add an additive campaign form:

```text
p1-swarm --peers 4 --external-slots 4 --lobby-seconds 90 --seconds 3600 ...
```

`--external-peer` remains the compatibility spelling of
`--external-slots 1`. More than one exterior slot is valid only in campaign
mode. The supervisor derives and asserts:

```text
0 < external_slots <= island_seats
peers + external_slots == island_seats
island_seats == 8                    # first campaign profile
```

The last equality is configuration policy, not a new architectural constant.
D6 already permits smaller and larger islands with different topology
(`docs/adr/0006-population-adaptive-topology.md:9-15`). If a later campaign
wants nine or more seats, it must first implement and measure the interest-mesh
behavior rather than running the current full-mesh harness past the boundary.

### 3.2 Stable seat ids, variable active membership

Every attempt has the same seat namespace. Bots occupy `[0, B)`, human seats
occupy `[B, B + H)`, and entity derivation remains `PersistId(slot + 1)`, as it
is today (`gates/p1-swarm/src/swarm.rs:796-801`,
`clients/regolith/src/campaign.rs:650-653`). Spawn pose must use the configured
`island_seats`, never `slot + 1`. The client currently does use `slot + 1`
(`clients/regolith/src/campaign.rs:407-421`), which happens to agree for the sole
last slot and becomes wrong for the first of several humans.

An active manifest is required because a later human is not a deterministic bot
key and a human currently sends state only to lower-numbered slots
(`clients/regolith/src/campaign.rs:861-897`). `StartV1` is proposed as:

```text
StartV1 {
    attempt_id: UUIDv7,
    seed: u64,
    tick: 0,
    island_seats: u16,              # 8
    active: [{ slot, node, entity }],
    witness_recipients: [slot],     # this subject's frozen ring set
    duration_ticks: u64,
}
```

The host computes one active ring over actual bot and connected-human members,
using the existing bounded width of seven
(`gates/p1-swarm/src/swarm.rs:1229-1239`). Each client broadcasts replication to
`active - self`, not `0..self.slot`, and sends claims/frames only to the supplied
witness recipients. The host verifies every supplied anchor against the seat,
NodeId and entity before tick zero, preserving the current anchor check boundary
(`gates/p1-swarm/src/bridge.rs:319-347`).

No accepted production witness rule is weakened. D10 requires coordinator-seeded
cell-epoch sets for consequential attested intents (`docs/adr/0010-witnessing.md:7-13`),
while this P4 campaign remains shadow measurement. The harness's deterministic
ring is measurement scaffolding today (`gates/p1-swarm/src/swarm.rs:1229-1239`).
If this host is ever promoted from P4 scaffolding into a production coordinator,
it must consume the D28 announcement instead of choosing the ring itself; D28
reserves witness-set choice to the coordinator
(`docs/adr/0028-witness-set-seeding.md:115-135`).

## 4. Admission, allocation and roster

### 4.1 Replace a session-long flock with two locks

The present `flock` is held from mint through the whole session and the second
request gets `campaign_busy` (`scripts/admission.py:179-201`,
`scripts/admission.py:247-251`). Its original job was both mutual exclusion and
one-slot capacity. Split those concerns:

```text
host lease:        one supervisor/attempt generation owns campaign + UDP port
reservation lock:  short exclusive transaction around the seat free-list
```

The supervisor holds the host lease for its process lifetime. A second
supervisor must fail before binding or publishing `listening.txt`; this retains
the protection against two harnesses fighting over one campaign. Admission
takes the reservation lock only long enough to read the current attempt id,
reserve a seat and persist the reservation. It never holds that lock while a
human plays.

Do not make admission's Python memory authoritative. It already documents its
roster as in-memory labels rather than identity or addressing
(`scripts/admission.py:59-76`). The standing host owns the attempt and knows
connection liveness, so it must own a small durable reservation journal beneath
its existing attempt directory. Admission invokes one idempotent host-side
`reserve` operation over the existing SSH control path; the operation locks,
writes by atomic replace, fsyncs and replies with the generation and seat. This
also prevents two admission workers from handing out the same slot.

### 4.2 Reservation state machine

```text
EMPTY
  | reserve(sid, node), earliest slot first
  v
RESERVED -- 45 s without authenticated dial --> EMPTY
  | QUIC NodeId + token + sid match reservation
  v
CONNECTED -- explicit goodbye ---------------------> EMPTY
  | transport reports close; two-second grace       --> EMPTY
  | StartV1
  v
ACTIVE -- explicit goodbye / close grace --> EMPTY
  | another reservation may bind the same slot
  v
ACTIVE under a new session
  | attempt ends
  v
EMPTY in the next generation
```

The session token currently authenticates the connected NodeId but does not
reserve a particular seat; the host's judge verifies the token against the
transport identity (`gates/p1-swarm/src/exterior.rs:537-548`). Therefore the
host must additionally match `(attempt_id, slot, session_id, NodeId)` against
the reservation. Arrival order must never assign seats: two clients can dial in
the opposite order from two HTTP replies.

`POST join` is idempotent for an active `(campaign, NodeId)` reservation. A
repeat before `StartV1` returns the same session id and slot and may mint a fresh
token for the already-recorded account; it must not append a second invite-ledger
allocation. A different NodeId cannot claim that reservation. After `StartV1`,
new requests remain admissible while any human slot is unbound. Each new
session begins a distinct authority and witness chain at the current host tick;
it does not splice or continue the departed session's chain. Existing clients
remove the departing entity when the live manifest omits it and may install the
new session's later keyframe under that stable seat entity.

### 4.3 Roster is a complete seat map

The current roster contains generated bot labels plus one human at `c.peers`
(`scripts/admission.py:137-157`) and disappears wholesale when the session ends
(`scripts/admission.py:292-311`). Return all eight seats instead:

```json
{
  "attempt_id": "...",
  "phase": "lobby",
  "starts_in_s": 37,
  "roster": [
    {"slot":0,"kind":"bot","state":"active","nickname":"shakedown-1"},
    {"slot":4,"kind":"human","state":"connected","nickname":"ada"},
    {"slot":5,"kind":"human","state":"reserved","nickname":"lin"},
    {"slot":6,"kind":"human","state":"empty","nickname":null},
    {"slot":7,"kind":"human","state":"vacant","nickname":null}
  ]
}
```

An empty or post-disconnect seat has no label. Nicknames remain decoration, not
identity, matching the endpoint's current boundary
(`scripts/admission.py:253-265`). `GET /v1/campaigns` reports `open` while any
seat is reservable, `lobby` while a cohort is forming, `full` when no seat is
free, `running` after `StartV1`, and `restarting` between attempts. The current
binary `busy` state is insufficient because it conflates a live host with no
capacity (`scripts/admission.py:111-123`).

## 5. Attempt and session length

Fifteen minutes is an automation interval, not an acceptable social session.
The existing supervisor restarts five seconds after every finite child and
deletes the listening record between children
(`scripts/p1-swarm-always-on.py:62-83`); every restart therefore drops all
connections. Propose a one-hour active attempt plus the 90-second lobby.

One hour has three useful properties:

```text
maximum interruption frequency       = once per 3,600 active seconds
four humans, full clean attempt       = 4 human-hours
four bots, full clean attempt         = 4 bot-hours
total valid cohort evidence           = 8 player-hours/attempt
```

The token hard maximum is also one hour (`docs/adr/0016-parameter-reference.md:34-42`),
but token expiry does not terminate an already authenticated QUIC connection;
the verifier is called at join (`gates/p1-swarm/src/exterior.rs:491-548`). A
token minted during the lobby may expire before the attempt ends without making
the live connection unauthenticated. The absence of post-start rejoin is what
makes that safe for v1. A future rejoin design must implement the accepted
half-TTL refresh posture rather than stretching token lifetime.

The cost is measurement granularity and failure exposure. A crash can lose up
to one hour of an attempt-level host report instead of 15 minutes, and a pipeline
change resets comparable P4 evidence because the ledger hashes the witness,
core, game and swarm trees (`scripts/p4-campaign-session.sh:104-127`). Preserve
partial client records for diagnosis, but bank only rows whose host evidence and
client interval validate. Do not advertise uninterrupted play: the one-hour
boundary remains visible in the lobby and in-game countdown.

## 6. Impairment remains per logical link and becomes independently reproducible

All exterior traffic already enters the same in-process router as bot traffic
(`gates/p1-swarm/src/exterior.rs:10-15`,
`gates/p1-swarm/src/swarm.rs:612-687`). The router applies the declared profile
to every packet it carries (`gates/p1-swarm/src/router.rs:45-61`,
`gates/p1-swarm/src/router.rs:75-99`). Multiple humans therefore do not bypass
impairment.

Do change the RNG partition. Today one router-global RNG consumes draws in
traffic arrival order (`gates/p1-swarm/src/router.rs:151-175`,
`gates/p1-swarm/src/router.rs:179-193`). Adding a human changes later loss and
jitter decisions for every existing peer, which makes a two-human failure
irreproducible in a one-human reduction. Derive one stream per directed logical
link and lane:

```text
link_seed = blake3(attempt_seed || from_slot || to_slot || lane)
fate      = link_rng[link_seed].next(packet_ordinal)
```

The profile is per directed peer pair, not a shared netem bucket and not one
profile per QUIC connection. A human-to-human datagram crosses one exterior
uplink, one router decision and one exterior downlink; it receives exactly one
logical impairment decision.

Report counters by `(from_slot, to_slot, lane)`, then reduce them per human:

```text
loss_h = dropped_h / (dropped_h + delivered_h)
jitter_applied_h = delayed_h > 0
profile_exercised_h = dropped_h > 0 && delayed_h > 0 && packets_h >= floor
```

Issue #240 requires applied impairment per session, not merely configuration.
The current client already measures uplink loss from router acknowledgements and
downlink loss from tick gaps (`clients/regolith/src/campaign.rs:15-37`), and its
session row keeps configured and observed values separately
(`clients/regolith/src/session.rs:105-155`). Require both host per-link counters
and the signed client observation. Do not require stochastic loss to equal
exactly `3.000%`, and do not describe 100 ms spikes as both p50 and p99: the
router injects a 100 ms delay into 10% of packets
(`gates/p1-swarm/src/router.rs:75-99`). The configuration schema should say
`loss_pct=3`, `jitter_spike_ms=100`, `jitter_rate_pct=10`; acceptance should use
a predeclared statistical band and a minimum sample count.

## 7. Evidence and banking must be fixed before multi-human hours count

One attempt produces shared host evidence plus separate participant intervals:

```text
AttemptReport {
    ordinary witness and impairment aggregate,
    bots: B,
    exteriors: [{slot, session_id, node, connected_ticks, frames, close}],
    per_link_impairment: [...]
}
```

Assembly emits one ledger input per actor contribution:

```text
bot contribution   = B * valid_attempt_seconds / 3,600
human contribution = signed_session.banked_minutes / 60
attempt total       = bot contribution + sum(human contributions)
```

For `B=4`, two humans who bank 50 and 42 minutes in a valid one-hour attempt
produce:

```text
bot hours   = 4 * 3,600 / 3,600 = 4.000
human hours = 50/60 + 42/60     = 1.533
total       = 5.533 player-hours
```

Do not copy the attempt's total hours into every human row. The current host
sets `player_hours = total_peers * configured_seconds / 3,600`
(`gates/p1-swarm/src/swarm.rs:1464-1465`,
`gates/p1-swarm/src/swarm.rs:1499-1499`), while the assembler copies the raw
report and merely attaches one client session (`scripts/p4-campaign-session.sh:197-202`).
The ledger then banks `.player_hours` without checking it against the signed
`banked_minutes` (`scripts/p4-ledger.sh:529-562`,
`scripts/p4-ledger.sh:589-630`,
`scripts/p4-ledger.sh:681-701`). Thus a present one-human, eight-bot 15-minute
campaign can claim `9 * 900 / 3,600 = 2.25` actor-hours even if that human's
signed row banks fewer than 0.25 hours. This is an existing accounting defect;
N humans would multiply it. Fixing it is a prerequisite, not follow-up polish.

Mixed-platform cohorts expose a second existing mismatch. Assembly currently
requires the signed client platform to equal the host report target
(`scripts/p4-campaign-session.sh:183-188`). A Linux host therefore cannot
assemble a Windows or macOS human row without lying about one side. Preserve
`host_target` in attempt evidence, but set each emitted human report's
measurement target from that participant's signed `platform_triple`; the bot
contribution uses `host_target`. The ledger's human measurement key already
includes `human_session_id`, so independent humans remain distinct
(`scripts/p4-ledger.sh:655-664`).

An exterior disconnect invalidates only that human contribution unless it also
causes an ordinary attempt-wide witness criterion to fail. Other connected
humans and bots may bank because their state, traffic and evidence remain
measured. A host crash, false positive, coverage below the existing 95% floor,
unbalanced deferral ledger, or unexercised impairment invalidates every
contribution based on that attempt; those are properties of the shared evidence
(`scripts/p4-ledger.sh:606-630`).

## 8. Failure modes and recovery

| Failure | Required result |
|---|---|
| Two admission requests race for the last seat | Host-side reservation transaction gives one seat; loser gets `409 campaign_full`. |
| Admission dies after mint but before reserve reply | Retry by NodeId returns the persisted reservation and same session; no second ledger allocation. |
| Reservation never dials | Host expires it after 45 s and returns the slot to the lobby free-list. |
| Human sends explicit goodbye | Host releases and atomically republishes the seat immediately; admission may reassign it. |
| QUIC reports the transport closed | Host preserves the binding for a two-second grace, then releases it. No application-frame silence timer exists. |
| Human sends to an empty/vacant slot | Host drops and counts `inactive_recipient`; it never indexes a bot vector with that slot. |
| One exterior queue fills | Count a failure for that exterior; do not block the synchronous tick or other connections. Existing one-slot behavior already counts full downlink queues (`gates/p1-swarm/src/swarm.rs:565-580`). |
| One pump dies | Mark that link dead only. Do not clear a shared liveness flag or end other links. |
| Host dies during lobby | All reservations belong to the old attempt id and are invalid after restart; clients return to admission. |
| Host dies during run | Preserve partial attempt and client rows for diagnosis; bank none without a final valid host report. |
| Admission restarts during run | Reconstruct phase and roster from the host journal; Python memory is not the source of truth. |
| Second supervisor starts | Campaign host lease or UDP bind refuses it before it can publish a generation. |
| Stale `listening.txt` survives | Generation mismatch makes every reservation/join fail closed; supervisor still removes the file before spawn (`scripts/p1-swarm-always-on.py:70-76`). |
| Lobby has one human | Start with four bots plus that human after 90 s; the feature does not make solo play unavailable. |
| Lobby is empty | Host waits without starting the expensive real-time run; first reservation starts the lobby clock. |

## 9. What breaks first

At the proposed eight-seat ceiling, topology and peer upload limits bind the
design before a need for more campaign humans does. D6 changes topology at nine
and caps each peer at 1 Mbps (`docs/adr/0006-population-adaptive-topology.md:9-15`),
so this proposal refuses a ninth seat rather than extrapolating the full mesh.

The conservative exterior-tunnel bandwidth bound for `H` humans in an
eight-peer full mesh is:

```text
human uplinks into host       <= H * 1 Mbps
host downlinks to humans      <= H * (8 - 1) * 1 Mbps
gross exterior tunnel bound  <= 8H Mbps

H = 4 => <= 32 Mbps
```

This is deliberately pessimistic: the 1 Mbps number is each sender's whole
upload budget, counted once again for every human recipient. It is still small
relative to a gigabit campaign host, but CPU cost includes QUIC encryption and
four async pump pairs and must be measured rather than inferred.

The sharpest static RAM bound is less friendly. Each exterior has 4,096-frame
bounded uplink and downlink queues (`gates/p1-swarm/src/exterior.rs:263-305`),
and a frame may carry 64 KiB (`gates/p1-swarm/src/exterior.rs:60-68`). Ignoring
object overhead:

```text
per exterior worst queued payload = 2 * 4,096 * 65,536 = 536,870,912 B = 512 MiB
four exteriors worst payload       = 4 * 512 MiB       = 2,048 MiB
```

That theoretical state already exhausts a 2 GB box. It is not expected steady
state, but it identifies the first catastrophic ceiling: queue memory under
stalled or hostile connections, not simulation state. The implementation should
replace the per-connection frame-count-only bound with both a smaller frame cap
and a shared byte budget, disconnecting the offender when either is exhausted.
A proposed starting budget is 16 MiB per exterior plus 64 MiB shared across all
exteriors; measure p99 occupancy during a four-human hour before accepting it.

The brief's statement that `hel1` is the 2 GB Hetzner box conflicts with the
repository's corrected machine record. The campaign design identifies
`orrery-hel1-1` as 16 cores and about 62 GB, and identifies the different
`ubuntu-2gb-hel1-1` as the 1.9 GB relay
(`docs/plans/campaign-admission-service.md:1108-1128`). The in-tree capacity
document also records 62 GB for the measured box (`docs/14-capacity.md:34-44`).
I could not verify which machine the newly deployed unit actually runs on, so
the design is safe for the 2 GB premise anyway: eight total seats, four humans,
and byte-bounded queues. On a real 2 GB host, queue occupancy and one-core QUIC
CPU are the expected first limits; on the documented 62 GB host, measurement is
still required but neither is plausibly close at four humans.

## 10. Implementation decomposition

The estimates are review-sized source deltas, not commitments. Tests are
included. Pieces 1 and 2 define contracts; pieces 3-7 implement them.

| Piece | Size | Nature | Depends on | Deliverable |
|---|---:|---|---|---|
| 1. Attempt and accounting schema | 250-400 lines | Subtle | none | Specify `AttemptReport`, `ExteriorReport[]`, per-link impairment, bot/human contribution arithmetic, mixed-platform target rules and ledger refusals. Add fixtures which catch duplicated cohort hours and cross-platform host/client rows. |
| 2. Exterior protocol v4 | 350-550 lines | Subtle | 1 | Add reservation-bound join, `StartV1` active manifest, per-client witness recipients and explicit lobby rejection reasons. Keep frame grammar unchanged. Cross-process tests must join clients in reverse reservation order. |
| 3. Multi-exterior swarm core | 700-1,100 lines | Most subtle | 2 | Replace `Option<ExteriorSlot>` with slot-indexed exteriors; audit every route named in section 2; form over active membership; verify N anchors; isolate liveness/backpressure; serialize N exterior reports. Include 1, 2 and 4 exterior tests plus one vacant slot. |
| 4. Per-link impairment and byte budgets | 300-500 lines | Subtle | 3 | Partition RNG by directed link/lane, add per-link counters, enforce per-link and shared queued-byte caps, and prove one human's traffic does not perturb another link's fate sequence. |
| 5. Admission reservation allocator | 450-700 lines | Subtle state/ops | 2 | Host-owned generation journal and atomic `reserve`; ascending free-list; idempotent retry; TTL/grace; split host lease from transaction lock; complete seat-state roster; listing phases. Mutation-test duplicate allocation and stale generation. |
| 6. Regolith multi-member client | 400-650 lines | Subtle | 2 | Consume `StartV1`, spawn with fixed `island_seats`, send to active slots, send witness frames to assigned recipients, show lobby/countdown/seat state, and fail closed on manifest mismatch. No ruleset or golden change should be needed. |
| 7. Assembly and ledger repair | 300-500 lines | Subtle evidence | 1, 3, 6 | Emit one bot contribution plus one report per signed human interval; bind each row to matching exterior sid/node/slot; use signed human platform; cross-check `player_hours == banked_minutes/60`; retain all present ledger refusals. |
| 8. Supervisor/config/deploy | 180-300 lines | Mechanical after 2-7 | 3, 5, 7 | Add `external_slots`, lobby and one-hour settings; preserve attempt directories; expose phase/generation; preflight four clients; update runbook and service. Deploy only after a local four-process proof. |

Critical path:

```text
1 accounting contract -----> 7 assembly/ledger ----+
       |                                           |
       +-> 2 protocol -> 3 swarm -> 4 impairment --+-> 8 deployment
                         |                          |
                         +-> 5 admission -----------+
                         +-> 6 client --------------+
```

Pieces 1, 2, 3 and 7 should not be split across owners without a written wire
fixture: their shared invariants are `(attempt, slot, sid, node)` binding and
exactly-once hour attribution. Pieces 4, 5 and 6 can proceed in parallel once
the v4 fixture is frozen. Piece 8 is mechanical and last.

The first deployable cut is not "Vec exterior compiles." It is:

```text
two independently minted humans reserve distinct slots
both receive the same StartV1 active manifest
both exchange state and a contested delivered input under impairment
one disconnect does not end the other's link
the attempt emits 4 bot hours plus exactly each signed human interval
the old one-exterior gate still passes through the compatibility spelling
```

## 11. ADR posture

No accepted ADR needs amendment for the bounded first cut. It stays inside D6's
full-mesh regime, preserves D3's iroh connection and lane semantics
(`docs/adr/0003-transport.md:7-12`), preserves D9's per-entity signed-log model
(`docs/adr/0009-verifiable-core.md:7-13`), and does not touch D21's frozen
`orrery_persistd` public surface (`docs/adr/0021-ruleset-distribution.md:61-77`).
The exterior v4 protocol and campaign attempt schema should be documented and
versioned, but an additive harness protocol is not in D21's freeze.

Recommend a new ADR only if the owner wants one of these broader claims to
become architecture rather than campaign policy: eight seats permanently fixed
for all games; lobby-only membership as a product rule; or the P1 harness
becoming a production island host. This proposal makes none of those decisions.

## 12. Reversal implemented by #681

The owner chose drop-in/drop-out play over the lobby-shaped intermediate. The
guarded proof is now an ignored multi-process integration test which joins a
process after the run starts, observes its host-authored binding, closes it,
observes the atomic release, and binds that same slot under a second session.
The client transport fixture separately proves a tick-900 join authors a
tick-900 signed anchor and continues sending witness claims from it.

## 13. Findings and items not verified

### Verified findings

- The single exterior slot is incidental to the wire and architecture but
  structural across the present harness implementation; section 2 cites the
  frame, accept, storage, routing, witness and report sites.
- Current human accounting can attribute whole-cohort configured hours to one
  signed human row instead of that row's banked interval
  (`gates/p1-swarm/src/swarm.rs:1499`,
  `scripts/p4-campaign-session.sh:197-202`,
  `scripts/p4-ledger.sh:529-562`). This is a pre-existing bug and a blocker for
  trustworthy multi-human banking.
- Current assembly requires client and Linux host target triples to match,
  blocking honest Windows/macOS campaign assembly on a Linux host
  (`scripts/p4-campaign-session.sh:183-188`). This is a pre-existing bug.
- Current client population and fan-out arithmetic assumes it is the one last
  exterior slot (`clients/regolith/src/campaign.rs:407-421`,
  `clients/regolith/src/campaign.rs:861-897`). The client must change with the
  host; admission alone cannot solve #563.
- The impairment model is global in configuration and RNG today, although every
  exterior packet does traverse it (`gates/p1-swarm/src/router.rs:151-193`,
  `gates/p1-swarm/src/swarm.rs:612-687`). Per-link RNG and counters are required
  for reproducible, per-session evidence.
- The brief's 2 GB `hel1` claim conflicts with the corrected in-repository host
  inventory (`docs/plans/campaign-admission-service.md:1108-1128`).

### Could not verify

- Which physical machine runs the newly deployed always-on unit. The tree has a
  service and runbook but no deployment-state record; the issue history names
  two easily confused hosts. Capacity conclusions are therefore stated for
  both 2 GB and 62 GB.
- Current live RSS, CPU, queue occupancy, packet rate or four-connection QUIC
  cost. The campaign design itself says impaired-witness harness RSS/CPU was not
  measured (`docs/plans/campaign-admission-service.md:1162-1164`). Piece 8 must
  measure these before raising the human cap.
- Whether friends require more than four simultaneous humans. Four is a proposed
  first cap derived from the eight-seat measurement island, not usage evidence.
- Whether one hour is the preferred social session. It is a proposed compromise
  and should be a campaign setting; the correctness design does not depend on
  exactly 3,600 seconds.
- A statistically justified acceptance band and minimum packet count for a
  measured 3% loss process. The current tree records configured and observed
  values but supplies no sampling-confidence policy. Piece 1 must choose and
  mutation-test one before banked hours rely on it.
