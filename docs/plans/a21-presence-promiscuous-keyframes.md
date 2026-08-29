# A21 - Promiscuous keyframes on peer presence: no, and here is the arithmetic

> Research node for #662, the owner's request verbatim: *"see if promiscuous
> keyframes on peer presence could be a thing."* Scheduled after the A19
> lanes and after #653's outcome, and argued against both: A19's
> keyframe/delta wire is merged and measured (#671, #683), and the owner's
> #653 decision - swept interest margin plus immediate crossing propagation -
> is merged as primitives (#692) with its host wiring still to come.
> Repository facts verified at `624c7782` on 2026-08-29; every `path:line`
> below was read before being cited. **Propose, not decide** - section 10
> lists what stays with the owner. Nothing here amends docs/03-replication.md
> or any ADR.

## Verdict up front

**No - broadening keyframe emission beyond the join rule buys nothing that
can be named, at a price charged in exactly the currency the budget grades.**
The profitable form of a presence-driven keyframe already exists at HEAD:
when a peer joins an entity's audience, the sender queues its cached keyframe
to that link before any delta is eligible there
(`gates/p1-swarm/src/bot.rs:1538-1547`, pinned by
`a_newly_interested_peer_receives_a_keyframe_before_any_delta`, #671's
mutation transcript). Section 3 walks every remaining presence transition -
departure, a third party's join, audience churn, own cell crossing - and
finds each one either undeliverable by definition or a re-send of bytes the
recipient already holds ~98% of the time. Section 4 shows the one population
that looks like an argument in favour - #683's 103,099 missing-newer
stranded deltas - is caused by keyframe *loss*, which is uncorrelated with
presence events, so presence-driven keyframes fire at the wrong moments to
repair it; the knob that actually repairs it (keyframe double-send, A19
section 5.5) costs a flat 46 kbps and removes ~97% of the population, and
even that is repairing a staleness no criterion clause registers.

The cost side is not symmetric noise. A keyframe is the expensive message
(~186 B wire vs ~120 B, derived from #671's measured hour in A20 section 1),
presence events are bursty and correlated (a squadron crossing a boundary is
one event repeated per craft), the budget grades the worst 1-second window
(`gates/p1-swarm/src/swarm.rs:2272-2280` region; A20 section 11), and #692
has just projected the swept margin's own increment at up to 829 kbps against
171 kbps of headroom. A broadcast-on-presence policy concentrates its spend
into precisely the windows the swept margin already contends for (section 2's
table), and its emission timing toward links whose knowledge did not change
is an H5-adjacent disclosure the unicast join rule does not have (section 5).

**The one deliverable this question does surface:** #692's crossing event and
the existing join rule compose into event-timed presence keyframes *for
free* - provided the host wiring feeds crossings into the same roster diff
`broadcast_state` reads (`bot.rs:1507-1509`). That composition is currently
unwired and unpinned, and it is the entire legitimate content of
"promiscuous keyframes on peer presence." Section 8 files it as a pin on the
wiring lane, plus one measurement leg that can falsify this node's churn
arithmetic if the owner wants a number instead of a derivation.

## 1. The policy space, named

"Presence" here means changes to an entity's **audience**: the set of peers
whose granted interest cells contain the entity's committed cell. At HEAD the
sender computes it per send tick from the island roster
(`bot.rs:1470-1483`: `entry.cells.contains(&cell)` over
`IslandMembership.peers`), diffs it against the previous tick
(`added`, `bot.rs:1507-1509`), and:

- **P-join (HEAD, merged in #671).** A keyframe goes to `added` links
  immediately: on a keyframe-due tick they are simply part of the full
  audience send (`bot.rs:1511-1535`); on a delta tick they get the cached
  keyframe payload and are excluded from that tick's delta audience so the
  anchor always precedes any delta on the link (`bot.rs:1538-1547`).
  Receiver side, a delta decodes only against the exact keyframe tick it
  references (`bot.rs:551-560`) - older anchor is `MissingNewerKeyframe`,
  newer is `SupersededKeyframe` - so a mis-ordered arrival is an accounted
  error, never a wrong state.
- **P-fresh.** Like P-join, but re-encode the current state for the joiner
  instead of sending the cached (up to ~1 s old) keyframe.
- **P-crossing.** On the entity's own committed-cell crossing, emit a fresh
  keyframe to the whole audience.
- **P-broadcast.** On *any* change of the audience set (join or leave, own
  or provoked by another peer's movement), emit a keyframe to the whole
  audience - the maximal reading of "promiscuous," and the one #662's
  "possibly broadcast rather than unicast" names.

All four are sender-local policies over the existing
`TAG_REPLICATION`/`TAG_REPLICATION_COMPRESSED` messages: **none needs a wire
change, a `PROTOCOL_VERSION` move, or a ruleset version bump.** The receiver
already accepts a keyframe whenever it arrives and replaces its anchor
unconditionally (`bot.rs:519-527`). The question is purely whether the extra
sends buy anything.

## 2. Does it save or cost? The arithmetic

Wire prices from #671's measured hour (derivation in A20 section 1):
keyframe ~186 B, delta ~120 B, both including the 60 B datagram overhead the
meter charges (`crates/orrery_net/src/budget.rs:48,60-63`). Steady-state
keyframe spend per sender, one entity, full 31-link audience:

```text
31 links x 186 B x 8 bit x 1 Hz = 46.1 kbps    (A20's keyframe-only floor)
```

Incremental cost of each policy, per presence event, against that baseline:

| policy | bytes per event | steady-state addition | peak behaviour |
|---|---|---|---|
| P-join (HEAD) | 186 B x k joiners, unicast | ~0 (events are rare per link) | k x 1.5 kbit in the event's window |
| P-fresh | same as P-join | same | same, plus one extra encode |
| P-crossing | 31 x 186 B = 5.77 kB broadcast | +43 to +75 kbps per v18-ceiling sender | every crossing lands whole in one window |
| P-broadcast | 5.77 kB per event, k events per correlated crossing | load-dependent, unbounded by own motion | k x 46.1 kbit in one window |

The P-crossing steady-state row: a craft at the v18 ceiling (480 m/s)
crosses committed-cell boundaries at `v/edge` = 480/512 = 0.94/s per aligned
axis at the campaign edge (`CAMPAIGN_CELL_EDGE_M = 512.0`,
`crates/orrery_games/src/regolith/mod.rs:219`), up to
`(|vx|+|vy|+|vz|)/edge <= 1.62/s` diagonally - so 0.94-1.62 broadcast
keyframes/s, +43 to +75 kbps, **roughly doubling the keyframe lane for
exactly the fast movers the swept margin is already paying for**. At the
gate's 128 m edge (`DEFAULT_CELL_EDGE_M`,
`crates/orrery_protocol/src/cell.rs:58`) the same craft crosses 3.75/s and
the addition is +172 kbps - the entire measured headroom.

The peak column is the load-bearing one. Three facts compound:

1. **The budget grades the worst 1-second window**, sampled from a sliding
   meter (A20 section 11's verified reading), not the mean.
2. **Presence events are correlated.** k craft crossing a boundary together
   is k audience-change events in the same window. Under P-broadcast that is
   `k x 46.1` kbit; under P-join it is `k x 1.5` kbit - a factor of 31, or
   `31/k` if a coalescing rule collapses the burst to one broadcast.
3. **#692 makes those windows the contended ones.** Crossings now propagate
   immediately (`crates/orrery_coordinator/src/interest.rs:126`,
   `apply_crossing`), and the swept margin's own cost is projected at up to
   829 kbps of increment against 171 kbps of headroom (#692's description).
   A presence-driven broadcast spends its bytes in the same second the
   doubled interest set does.

So the cost is real, peak-shaped, and lands on the worst moment. What stands
against it:

## 3. What a presence-driven keyframe could buy, transition by transition

- **A peer joining the audience.** Fully served by P-join at HEAD: cached
  keyframe first, deltas decodable immediately after (they reference that
  same cached anchor, `bot.rs:1548-1566`). Residual latency once the roster
  knows: one send tick, <= 50 ms. The freshness gap P-fresh would close is
  <= 1 s of state age *inside a decodable stream that corrects it with the
  next delta 50 ms later* - the delta patches the anchor to the current
  canonical bytes (`bot.rs:1548-1566`), so the joiner renders current state
  after keyframe + first delta regardless of the anchor's age. P-fresh buys
  nothing measurable.
- **A peer leaving the audience.** A keyframe to a link that no longer
  contains you is undeliverable value by definition; the receiver's replica
  simply ages out (`REPLICA_TTL_TICKS = 120`, `bot.rs:441,461-470`).
- **Churn: leave then rejoin.** The receiver's anchor is replaced only by a
  newer keyframe, never by time (`bot.rs:519-527`; the anchors map has no
  TTL), and on rejoin P-join re-sends the current cached keyframe anyway.
  A churning peer is never left without an anchor. Cost of churn under
  P-join: one redundant ~186 B unicast per rejoin. Under P-broadcast: 5.77 kB
  to everyone per flap.
- **A third party's presence change.** Existing audience members' anchor
  state is untouched by someone else joining or leaving. A broadcast
  keyframe to them re-sends bytes they hold - unless they are inside a
  missing-newer window, which is section 4's question.
- **Own cell crossing.** The crossing changes the *audience* (who can see
  you), and P-join already handles every link that change adds. The
  crossing does not invalidate any existing link's anchor: deltas carry the
  committed cell when it changed (`bot.rs:1549-1560`), and the keyframe's
  cell field is bookkeeping, not an anchor precondition.

Every transition is either already served, undeliverable, or redundant. The
claim "every delta supersedes the last, so a missed delta costs nothing"
(A19 section 5.5) was tested rather than assumed: it is pinned by
`a_shed_or_lost_delta_is_fully_superseded_by_the_next` (#671's mutation
transcript shows it failing when deltas chain), and #671 measured
`deltas_unanchored: 0` across 2.9 M deltas on the clean hour. The churn
cases above are covered by the receiver-side exact-tick anchor check plus
the no-TTL anchor map - both read at source.

## 4. The missing-newer population, honestly

The strongest prima facie argument for promiscuity: #683's witnessed
impaired hour counted **116,295 `deltas_unanchored`** - 284 no-anchor,
**103,099 missing-newer**, 12,912 superseded - where missing-newer means a
receiver holding an anchor whose successor keyframe was lost
(`bot.rs:551-556` is the check; the counter classification landed in #683).
Would presence-driven keyframes repair it?

**No, because the trigger is uncorrelated with the wound.** Missing-newer
windows are opened by the 3% loss profile eating a keyframe; presence events
are opened by geometry. At ~3-4% keyframe loss per link-second and a mean
residual of ~0.5 s to the next staggered keyframe, the instantaneous
fraction of links inside a stale window is ~1.7-2%. A P-broadcast event
therefore spends 31 x 186 B to early-close an *expected* ~0.55 links'
windows by ~0.25 s each - about 42 kB per second-of-staleness saved, and
only when a presence event happens to occur at all.

Two alternatives dominate it, both already named:

- **Keyframe double-send** (A19 section 5.5's knob): +46.1 kbps flat per
  sender, cuts the per-link stranding probability from p to ~p^2
  (0.03 -> 0.0009), removing ~97% of the missing-newer population,
  uniformly in time rather than only at presence events.
- **Receiver keyframe request (NACK)** on the first missing-newer delta:
  near-perfect repair for ~one message per loss event, but it is receiver
  feedback - the apparatus A19 section 4(a) deliberately declined on this
  interim wire, and the acked-baseline end-state owns it.

And the premise deserves its own audit: **the population currently harms no
clause.** The same hour that counted 103,099 missing-newer deltas held every
criterion clause - 829 kbps peak, 0 shed, 0 false positives, ~100% coverage
(#683). Each stranded delta is a <= 1 s stale window already bounded and
masked (A19 section 5.5), on a wire whose replicas feed interest selection,
the skin, and the witness observe store - not simulation. Spending 46 kbps
to shrink it is a latency purchase, and section 10 leaves it with the owner
priced exactly so.

## 5. Hearsay, reveal gates, and what the timing says

Input side, H2 (`docs/adr/0050-knowledge-tiers.md:194-200`, Proposed,
treated as binding per A20's precedent): every input a presence-driven
cadence would read is authoritative - the island roster is the
coordinator's manifest, crossings arrive as signed
`InterestCellCrossing`/grant exchanges
(`crates/orrery_protocol/src/coord.rs:1003`,
`crates/orrery_coordinator/src/interest.rs:126-150`). No hearsay gates any
rate in any policy in section 1. H2 is satisfiable on inputs.

Output side is where P-broadcast fails. A keyframe emitted to *all* links
because *one* link's membership changed is a timing signal to 30
uninvolved receivers that someone entered or left the author's visibility
set - who-can-see-whom, leaked past whatever H5 reveal gating
(`0050-knowledge-tiers.md:220-225`) the hearsay tier will enforce, on a
channel no reveal gate inspects. P-join has no such emission: the only
party whose traffic changes is the joiner, and the fact it learns - its own
membership - it already holds authoritatively in its own grant. Two honest
qualifiers: #535 records that the island manifest *already* broadcasts every
peer's occupied cells island-wide, so P-broadcast would today leak less than
the roster does - but #535 exists precisely because that disclosure is
unreviewed, and a new mechanism should not mint a second instance of it
while the first is under review. And ADR-0050 is Proposed, so this is an
argument of consistency, not compliance.

## 6. Witnessing

The gate feeds every replication-delivered reconstruction into the witness
sample store (`bot.rs:657-668` - the seam A20 section 4 corrected A19's
stale "no production caller" finding with; re-verified at HEAD), so
replication cadence toward witness links is adjudication-relevant. Presence
policies change that cadence only additively (more keyframes, never fewer),
so no policy here starves re-anchoring - but none helps it either: witness
links are seeded from the witness-set record
(A20 section 3's P0 table), are not presence-churning, and A20's governor
already exempts them from any rate reduction. Witnessing is a null
interaction: it neither argues for nor against, and the doubled-cost /
no-benefit conclusion stands without it.

## 7. What #653's outcome actually changes

#692 merged the swept margin (`swept_neighbors27`,
`crates/orrery_protocol/src/cell.rs:345`) and the immediate crossing event
(`apply_crossing`, `crates/orrery_coordinator/src/interest.rs:126`) as
primitives; host and client wiring is a declared follow-up. Two consequences
for this node:

1. **The latency motivation in #662 dissolves.** The issue's scenario - "a
   peer can enter interest, receive nothing but deltas anchored to a
   keyframe it never saw, and be reduced to waiting up to a second" - cannot
   occur at HEAD even before #692: the sender withholds deltas from `added`
   links until the cached keyframe is queued (`bot.rs:1538-1547`), and the
   receiver's exact-tick check turns any mis-ordered delta into a counted
   error rather than a wrong render (`bot.rs:551-560`). The residual delay
   is the *roster's* 1 Hz refresh - which is exactly what the crossing
   event fixes at the interest layer. Once the wiring feeds crossings into
   `IslandMembership` before `broadcast_state` reads it, the join keyframe
   fires within one send tick of the actual crossing, with zero new
   mechanism. Presence-driven keyframes in the only form that pays are an
   emergent property of two merged designs - *if* the composition is wired
   and pinned, which is issue A in section 8.
2. **The budget position hardens the "no."** The swept margin roughly
   doubles the interest set (27 -> 54 cells, #692) with an upper-bound
   increment of 829 kbps against 171 kbps of headroom, and #687 is measuring
   the real figure now. More cells means more boundary surface, means more
   presence events; immediate propagation means they arrive in the same
   window they occur. Every kbps a presence-broadcast policy adds lands on
   the most contended second of a budget that is plausibly already over.

## 8. Decomposition

Two issues, house format, both propose-only until the owner judges this
node. Neither may start while #687 (A20 lane 1) holds `gates/p1-swarm`.

### Issue A - pin the crossing -> join-keyframe composition (type:task)

Blocked on the #692 host-wiring lane (whichever issue carries
`IslandMembership` updates from `InterestCellCrossing`); it is a pin on that
lane's work, not new machinery. Why: section 7 item 1 - the event-timed
join keyframe is the whole yield of #662, and it exists only if the crossing
event updates the roster the sender diffs. Nothing currently asserts the
composition end to end.

Acceptance criterion: a named leg,
`a_crossing_driven_roster_add_gets_its_keyframe_on_the_next_send_tick` -
drive a craft across a cell boundary mid-keyframe-interval, deliver the
crossing event, and assert the newly-added link receives the cached keyframe
within one send tick of the roster update and before any delta, without
waiting for the 1 Hz bulk refresh.

MUTATION CHECK - guarded stage: the roster update path the crossing takes.
Break: apply crossings only at the bulk refresh. Expect: the named leg fails
(keyframe arrives up to ~1 s late); the existing
`a_newly_interested_peer_receives_a_keyframe_before_any_delta` stays green,
proving the two pins guard different things.

### Issue B - measure presence churn before believing section 2 (type:measurement)

Blocked on `gates/p1-swarm` being free. Why: every churn figure in section 2
is a derivation (crossing rates from geometry, correlation asserted from the
squadron argument), and this repo's standing rule is that machinery -
including a *decision against* machinery - should rest on measurement where
one is cheap. This leg is the falsifier: if measured churn is wildly above
the derivation, the cost table is wrong in the direction that makes the "no"
stronger; if presence events turn out rare and uncorrelated, the P-broadcast
cost case weakens and the owner should see that.

Acceptance criterion: a `--presence-stats` flag emitting, for the roaming
legs (not `--min-cells 1`, whose presence sets are static by construction),
per-entity audience-change event rates, joiner-cluster-size distribution per
1 s window, and the instantaneous stranded-anchor fraction (links currently
inside a missing-newer window), into the JSON report; deterministic per
seed, run twice as the control. No criterion, allowance, or threshold moves.

MUTATION CHECK - guarded stage: the audience diff feeding the counters.
Break: count audience size instead of audience *changes*. Expect: the
cluster-size distribution degenerates to a constant and the named
determinism check over a boundary-crossing scenario fails; unrelated legs
stay green.

## 9. What this must not touch

1. Canonical encoding, claim preimages, `verify_bundle` - consumed, never
   defined (A19 section 6 item 1, carried verbatim).
2. The join rule itself and its pins - this node's "no" leaves
   `bot.rs:1507-1547` exactly as merged.
3. Lane charging - any keyframe emitted by any future policy is a normal
   `Channel::State` datagram; `lane_of`'s untagged-defaults-to-replication
   rule stands.
4. H2 - section 5's input audit is the compliance argument for anything
   presence-driven; extend the audit before extending any code.
5. The criterion - Issue B adds counters and legs, never moves an
   allowance; a surprising churn number is a finding, not a knob.

## 10. Owner-reserved decisions

1. **Judging this node's "no"** - declining P-fresh, P-crossing, and
   P-broadcast is a recommendation with arithmetic attached, not a
   decision.
2. **Whether Issue A folds into the #692 wiring lane or stands alone**, and
   both issues' sequencing against #687-#691, which hold the harness.
3. **Whether to buy the missing-newer reduction anyway**: keyframe
   double-send at +46.1 kbps per sender removes ~97% of a population that
   currently fails nothing (section 4). Priced; not recommended while #687
   is still establishing where the budget actually stands.
4. **Whether Issue B runs at all**, given the derivation already answers
   the question to this node's satisfaction and the harness is contended.
5. **The #535 manifest review** - section 5 hands it one more datum (a
   presence-broadcast policy would leak less than the manifest already
   does); what that means for the manifest is that review's question, not
   this node's.

## 11. Findings, and corrections to the brief that commissioned this

- **#662's motivating failure mode does not exist at HEAD.** The
  join-then-undecodable-deltas scenario is structurally prevented by the
  sender's `added`-exclusion rule and the receiver's exact-tick anchor
  check (section 7 item 1). The issue predates #671's merge; the residual
  is roster latency, owned by #692's wiring.
- **The "immediate keyframe" on join is the cached one**, up to ~1 s old,
  not a fresh encode (`bot.rs:1544-1547`) - immaterial to decodability
  (the next delta corrects to current state), but a brief that reasons
  about it as "current state on join" would be subtly wrong.
- **Direction nit in #662 and the brief:** the merged rule keys on a peer
  joining the *entity's audience* (the peer's granted cells now contain
  the entity's cell), not on "a peer joining your interest set." Same
  event viewed from opposite ends at gate scale, different sets in
  general.
- **Keyframe share: the measured figures are 5.7% of messages and 8.6% of
  bytes** (#671: 177,887 keyframes / 2,916,285 deltas, 8.6% of bytes).
  The brief's ranges (5.5-5.8% / 8.2-8.6%) could not be sourced to #671
  or #683's text; the endpoints are unverified.
- **The 298 kbps datagram floor is per message actually sent** - A20
  section 1 already sharpened A19's phrasing: elision and rate scaling
  reach under it, payload compression cannot. Presence policies only add
  messages, so the floor argues against them either way.
- **#683 and #692 are PRs, #687 is an issue** - `gh issue view` fails on
  the first two. Trivial, recorded so the next reader does not re-derive
  it.
- **Not verified:** presence-churn rates and joiner correlation (Issue B
  is the falsifier); the per-link stranding fraction (derived from #683's
  loss ratios, same caveat A20 carries); and everything downstream of
  #692's 829 kbps upper bound, which #687 is replacing with a measurement
  now.
