# A20 - Scaling entity update frequency with bandwidth, instead of dropping overage

> Design research for the owner's question, verbatim: "map if scaling entity
> update frequency with bandwidth is feasible instead of dropping overage."
> Context: the replication meter sheds FIFO when a peer exceeds its 1 Mbps
> upload budget (`crates/orrery_net/src/peer_link.rs:248-258`), and shedding
> has already cost correctness once - #621's 84 false discrepancy signals
> against an honest peer trace back to dropped packets becoming gaps. Prior
> art: `docs/plans/a19-replication-delta-keyframes.md` (built: #664, #667,
> #671, #683) and docs/03-replication.md sections 5 and 9.3. Repository facts
> verified at `f6cb0b2a` on 2026-08-29; every `path:line` below was read
> before being cited. **Propose, not decide** - section 10 lists what stays
> with the owner. Nothing here amends docs/03-replication.md or any ADR.

## Verdict up front

**Yes - per-entity, per-link rate scaling is expressible on this wire, and
it is cheap precisely because of the shape A19 built.** Every delta at HEAD
references the sender's last 1 Hz keyframe rather than the previous send
(`gates/p1-swarm/src/bot.rs:1550-1576`), so deltas are mutually superseding:
a receiver cannot distinguish a delta the sender chose not to build from a
delta the network lost, and both cost exactly one 50 ms update. Scaling an
entity's rate down for a link is therefore *choosing which deltas not to
build this tick* - zero new wire format, zero metadata bytes, no
`PROTOCOL_VERSION` bump, no `client_rev` invalidation, and the keyframe
lattice is already the guaranteed floor: 1 Hz per entity per interested
link, which is what keeps replicas inside the 2 s TTL
(`REPLICA_TTL_TICKS = 120`, `bot.rs:441`) and keeps every future delta
anchorable. The owner's feared failure - "a deprioritised entity's keyframe
expires and you have converted a bandwidth problem into an unanchored-delta
problem" - cannot occur under the scheme in section 4, because keyframes are
outside the scaled class by construction and a receiver's anchor is replaced
only by a newer keyframe, never by time (`bot.rs:514-560`; `kf_age` is a
`u16` of ticks, so an anchor stays referenceable for 18 minutes).

**And the honest half of the answer: it saves zero kbps today.** The
witnessed impaired hour peaks at 829 kbps against 1000 with **zero packets
shed** (#683) - the meter is not dropping any overage, because there is no
overage. Rate scaling is not a bandwidth optimization; it is an
*overload-behaviour* mechanism, and at HEAD the overload regime is not
exercised by any gate. So the recommendation is staged, evidence first:

1. **Lane 1 (measure):** a `--budget-kbps` override on the gate, and the
   witnessed impaired hour re-run at 900/700/500 kbps, to reproduce the
   FIFO failure signature (#621's lineage: shed keyframes -> unanchored
   deltas -> re-anchors -> false signals) deterministically, before any
   machinery is built against it. If the signature does not reproduce, this
   node's case collapses and that is the finding.
2. **Lane 2 (build now, small):** anchor-last shed order in the existing
   backstop - within `Lane::Replication`, shed deltas before keyframes.
   This is the cheapest change that removes the catastrophic mode (anchors
   dropped under pressure) and it stands on its own even if the governor is
   never built.
3. **Lanes 3-5 (build only on evidence):** the delta governor - a sender-
   side per-send-tick byte slice that skips lowest-priority deltas instead
   of offering them to the meter - conditional on lane 1 showing that FIFO
   plus lane 2 still fails clauses under pressure.

**The strongest argument against building the governor** is stated here
rather than buried: it is machinery that is dead code at today's load,
activates only in overload, and untested-until-needed overload paths are
exactly where the #621 class of bug lives. Lane 1 exists to convert that
argument into a measurement: either the pressured hour shows FIFO failing
clauses (build the governor, and gate it with the pressured leg so it is
*not* dead code), or it shows FIFO plus anchor-ordering holding (keep
shedding, file the governor as declined, and this node's answer to the
owner is "feasible but not worth building yet, here is the run that says
so"). A well-evidenced no is an acceptable outcome of this programme.

## 1. The wire at HEAD, and the arithmetic that reproduces 829

Worst peer, witnessed impaired leg (`--min-cells 1`, all 32 peers in one
another's interest sets - `scripts/p1-swarm-gate.sh:171-174`), one authored
craft each, 20 Hz sends (`send_hz: 20`, `gates/p1-swarm/src/swarm.rs:120`)
of 60 Hz ticks, keyframes at 1 Hz staggered by `PersistId`
(`DEFAULT_KEYFRAME_EVERY_SENDS = 20`, `bot.rs:104`).

Average wire sizes, derived from #671's measured hour rather than asserted:
383,801 kB of replication wire, split 8.6% keyframe / 91.4% delta bytes
over 177,887 keyframes and 2,916,285 deltas, gives

```text
W_k = 0.086 x 383,801,000 / 177,887   = ~186 B wire per keyframe
W_d = 0.914 x 383,801,000 / 2,916,285 = ~120 B wire per delta
      (both include the 60 B datagram overhead the meter charges,
       crates/orrery_net/src/budget.rs:48,60-63)
```

Per second, per recipient, one craft: 1 keyframe + up to 19 deltas. Times
31 recipients:

```text
replication offered = 31 x (186 x 1 + 120 x 19) x 8 bit
                    = 31 x 2,466 B x 8 = 611.6 kbps
measured witnessed peak (#683)         = 829 kbps
residual = 217 kbps = witness lane (~180 kbps per the harness's own
           header figures, gates/p1-swarm/src/main.rs:144-150) plus
           control and #683's rollback re-sends
```

The model reproduces the replication share; the 217 kbps split between
witness, control and re-send traffic is inferred, not measured - lane 1
prints the split so nothing downstream leans on the inference. Two floors
bound everything below:

- **All-datagram-overhead floor at full cadence:** 60 B x 20 Hz x 31 =
  298 kbps (A19 section 7). This floor applies only to messages actually
  sent - a skipped delta skips its 60 B too, which is why rate scaling
  reaches under this floor while payload compression cannot.
- **Keyframe-only floor:** 31 x 186 x 8 = **46.1 kbps**. The replication
  lane is compressible from 611.6 down to 46.1 kbps before any anchor,
  any TTL, or any liveness property is touched. That 13x range is the
  headroom rate scaling has and shedding does not.

## 2. Question 1: is per-entity rate scaling expressible here?

Expressible, and the proof is structural rather than hopeful. The three
facts that make it so, each at HEAD:

1. **Deltas supersede each other.** Every delta patches the sender's last
   keyframe, never the preceding delta (`bot.rs:1550-1576`; the client
   mirrors it, `clients/regolith/src/campaign.rs:467-533`). Skipping any
   subset of an entity's deltas on any subset of links leaves every
   remaining delta exactly as decodable as before. The receiver cannot
   tell policy from loss; loss is already handled.
2. **Anchors do not expire by time.** A receiver replaces its per-entity
   anchor only when a newer keyframe arrives (`bot.rs:514-560`), and
   `kf_age` is a `u16` of ticks - 65,535 ticks = 18 minutes of reach,
   against a 1 s keyframe cadence. Deprioritising deltas ages nothing.
3. **Keyframes are already the liveness heartbeat.** `LastSeen` expires a
   replica after 120 ticks (`bot.rs:441,467`); 1 Hz keyframes refresh it
   with 2x margin. So the floor rate is not a policy choice this node
   invents - it is the cadence A19 already pays for, unconditionally.

Therefore the scheme's one hard rule: **keyframes are never scaled, never
skipped, and (lane 2) shed last.** The scalable class is deltas only, from
19/s per entity per link down to 0. Unchanged-state elision (`bot.rs:1550`:
a byte-identical state with an unchanged cell emits nothing this interval)
is the existing precedent: the criterion already accepts a sender that
offers less than
the maximum cadence when offering less loses nothing.

One receiver-side staleness note, stated rather than hidden: a delta
carries the committed cell only when it changed (`bot.rs:1549-1560`), so a
link whose deltas are being skipped learns of a cell change at worst one
keyframe interval late - the same bound a lost keyframe already imposes
(A19 section 5.5). Audience membership is computed sender-side from the
roster (`bot.rs:1471-1493`), so this staleness affects the receiver's
local bookkeeping and rendering only, never who gets sent what.

## 3. Question 2: the priority function, its inputs, and its cost

Priority classes, highest first. Every input is audited against H2
(ADR-0050 clause (d): "hearsay never gates membership or rate, in either
direction", `docs/adr/0050-knowledge-tiers.md:194-200` - Proposed, but
this node treats it as binding anyway):

| class | what | input | authority of the input |
|---|---|---|---|
| P0, never scaled | keyframes | sender's own clock | sender-authored |
| P0, never scaled | immediate keyframe + first deltas to a just-added audience link | roster diff (`added`, `bot.rs:1507-1547`) | coordinator manifest via `IslandMembership` |
| P0, never scaled | all deltas to links in the author's witness set | `set_witness_set` (`gates/p1-swarm/src/swarm.rs:1739`; ADR-0028 seeding) | witness-set record |
| P1..Pn, scaled farthest-first | remaining deltas, ordered by Chebyshev cell distance between the entity's committed cell and the recipient's committed cell, ties by (`PersistId`, `NodeId`) | the same roster entry the audience filter already reads (`entry.cells`, `bot.rs:1479`) and the D2 single-writer committed cell | simulation authority (D2) |

No hearsay, no summary-tier product, no receiver feedback, no
non-authoritative signal anywhere in the table - H2 holds by input audit,
not by promise. The richer inputs the owner's question gestures at
(relative velocity, combat participation, "can the observer act on it")
are all *available* authoritatively (they are fields of authored canonical
state, `crates/orrery_games/src/regolith/state.rs`), and all deferred:
distance-only first, because lane 5's measurement will show whether a
second term earns its complexity, and because #653's swept-margin AOI work
- if the owner adopts it - changes what "near" means and should land
before any speed-dependent priority term is tuned against it.

Cost to compute, honestly. There are no per-entity timers and no new
per-link protocol state. Per send tick (every 3rd of 60 Hz), the sender
already builds the per-entity audience map by iterating entities x roster
members (`bot.rs:1471-1493`). The governor adds: one integer cell-distance
per (entity, link) candidate pair, one sort of the candidate list, one
prefix-sum against the byte slice. Gate scale: 1 entity x 31 links = 31
pairs. A future 24-entity author in a 32-peer island: 744 pairs, sorted 20
times a second - on the order of 10^5 comparisons/s, noise against a 60 Hz
simulation. The only retained state is per-(entity, link) last-delivered
tick, wanted anyway for the degradation-honesty counter in section 7.

## 4. Question 3: the interaction with adjudication - the hard constraint

What each peer can still prove when entity A reaches peer X at 20 Hz and
peer Y at 5 Hz:

- **Both hold identical bytes for everything they receive.** Encode-once
  survives: the governor selects *recipients per message*, never message
  content, so keyframe and delta bytes remain shared across all links
  (`bot.rs:1519-1536` clones one payload per recipient today; the governor
  only shortens the recipient list on delta sends).
- **Hash equality is cadence-independent.** Every delivered message
  reconstructs the author's exact canonical bytes (A19's exactness law,
  pinned by `a_delta_patch_reconstructs_the_authors_canonical_bytes_exactly`
  in `crates/orrery_protocol`), so any delivered sample still satisfies
  `state_hash` equality against a claim regardless of how many samples
  its neighbours got.
- **Adjudication itself never reads replication cadence.** `verify_bundle`
  is a pure function of witness-lane `LogFrame`/`StateClaim` bytes
  (`crates/orrery_core/src/replay.rs`); the witness lane has its own
  cadence, its own budget share, and is unsheddable
  (`budget.rs:432-434`).

**The one genuinely cadence-sensitive consumer, and A19's stale finding
corrected:** A19 section 11 recorded that `Witness::observe` had no
production caller. That is no longer true at HEAD - the gate's receive
seam feeds every replication-delivered reconstruction into the witness's
sample store (`gates/p1-swarm/src/bot.rs:657-668`), which is exactly the
store a blind watch re-anchors from (`try_reanchor`,
`crates/orrery_witness/src/witness.rs`). A witness that receives an
author's entity at 1 Hz has one twentieth of the re-anchor points, and
re-anchor starvation is the mechanism behind #621's coverage hole and
false signals. **Hence the P0 rule above: links to peers in the author's
witness set are never scaled.** That exemption costs, at gate scale,
`MAX_WITNESS_LINKS = 7` (`crates/orrery_witness/src/plugin.rs:152`) of 31
links kept at full delta rate - priced into section 6's floor.

**Clause audit.** Every clause of the P1/P4 criterion was read at
`swarm.rs:2262-2420` against the question "does this silently depend on
uniform cadence": peak upload, shed count, undecodable inbound, at least
one replica held, false positives (witnessed), observation coverage >=
95% (witnessed), external-peer connect/participate/keep-up, and
gaps-seen-under-loss. None reads per-link or per-entity cadence; the two
that are cadence-*adjacent* are coverage (via the observe store - handled
by the witness-link exemption) and the replica-held guard (handled by the
keyframe floor). One clause interaction must be named as a hazard rather
than a dependency: **the shed clause cannot see a governor.** Deltas the
governor skips are never offered to the meter, so `total_shed` stays 0
no matter how aggressively the governor degrades - a governor that
skipped everything would go green on today's criterion. Section 7's
degradation-honesty counter exists for exactly this, and lane 3's
mutation checks break the governor in that direction on purpose.

Determinism: canonical simulation reads inputs and neighbor frames, never
replicas; replicas at the receiver feed interest selection and the skin
(receiver-side scaffolding, `crates/orrery_spatial/src/interest.rs`) and
the witness observe store (verification-side). Varying who-received-what
therefore cannot make simulation state receiver-dependent. Ruleset and
wire versioning: no wire change of any kind, so no `PROTOCOL_VERSION`
move and no published-binary or campaign `client_rev` invalidation; a
governed sender interoperates with an ungoverned receiver and vice versa,
because skipped deltas are indistinguishable from lost ones.

## 5. Question 4: does it actually beat shedding? The arithmetic

Define f = offered replication / affordable replication. At HEAD, f < 1
on every leg and the two schemes are byte-identical (the governor's
acceptance includes `an_unpressured_governed_run_is_wire_identical`).
The comparison lives entirely in f > 1:

**FIFO (HEAD behaviour).** The meter admits in arrival order until the
1 s window fills, then sheds everything sheddable for the rest of the
window (`peer_link.rs:248-258`). Overage fraction o = 1 - 1/f of each
second goes dark *for all entities on all links at once*, keyframes
included - shedding is time-correlated, not priority-correlated. Lost
keyframes strand deltas: #683 measured, at 3% link loss alone, 12,315
discarded keyframes producing the bulk of 116,295 unanchored deltas over
the witnessed hour, roughly 8.5 stranded deltas per lost keyframe. Scale
that mechanism by shedding: at f = 1.19 (offered 829 vs an affordable
700), o = 0.156, so ~4.8 keyframes/s shed per peer -> ~17,300/hour ->
projected ~150,000 additional unanchored deltas, on top of loss - and
when effective keyframe delivery falls below 0.5 Hz (f > 2), replicas
start expiring against the 2 s TTL and entities vanish from peers
entirely. The measured precedent for the end state is #621: 15,001 shed,
coverage 86.5%, 84 false accusations of an honest peer. These
projections are derivations from #683's measured ratios, not
measurements; lane 1 replaces them with numbers.

> **Measured, 2026-08-30 (#687, delivered by #698).** The pressured sweep
> against `main` *including* #688's anchors-shed-last:
>
> | Budget | Peak | Shed kf/delta | Clause failures |
> |---|---:|---:|---|
> | 1,000 | 829.784 kbps | 0 / 0 | none |
> | 900 | 829.784 kbps | 0 / 65 | none |
> | 700 | 699.824 kbps | 303 / 5,295 | shed; boundary thrash |
> | 500 | 531.048 kbps | 1,789 / 29,049 | shed; 89 false positives; coverage 94.2% |
>
> Two of this section's inputs did not survive it:
>
> * **The 8.5 stranded-deltas-per-lost-keyframe ratio above does not hold
>   across the curve** — 14.38 at 700, 8.79 at 500 — and at 900 kbps
>   unanchored deltas rose 8,551 with **zero** shed keyframes. It is an
>   aggregate coincidence, not a causal pairing, so the ~150,000 projection
>   built on it does not follow.
> * **The swept margin costs nothing**, measured −4.328 kbps, not the
>   +829 kbps increment quoted as the pressure that makes f > 1 likely.
>
> FIFO survives 900 kbps and the real swept-margin load with #688 alone,
> failing only under synthetic pressure at ~1.19x offered peak. As of
> 2026-08-30 the measured peak is 806–808 kbps against a 1,000 kbps budget
> with the swept margin live. Section 5's own case for declining lane 3 on
> exactly this evidence therefore stands, and #687's lane recommended
> declining it. **Whether to decline remains the owner's call.**

**Governor.** Skips lowest-priority deltas first, per tick. At the same
f = 1.19: skip 129 kbps of deltas = ~134 deltas/s = ~4.3/s/link spread
over the 24 non-witness links - the farthest entities drop from 20 Hz
toward the floor while near, witness-watched, and newly-visible entities
stay at 20 Hz. Keyframes shed: 0. Additional unanchored deltas: 0. TTL
breaches: 0. The degradation range before the governor exhausts its pool:

```text
scalable pool  = 24 links x 19 deltas/s x 120 B x 8 = 437.8 kbps
floor          = keyframes 46.1 + witness-link deltas 127.7 = 173.8 kbps
absorption     = 611.6 / 173.8 = ~3.5x today's replication load
                 with zero anchor loss; beyond 3.5x, lane 2's
                 anchor-last backstop takes over and anchors are
                 still the last thing dropped
```

**The cost side of the ledger, per the brief's own test.** Metadata: 0
bytes - there is no wire change, so the "3 kbps of metadata to save 20"
failure mode is structurally absent. Compute: section 3's sort, ~10^5
ops/s worst case. Complexity: one new module plus one integration seam in
`broadcast_state`, against FIFO's zero. That complexity is the real price,
and it is why the recommendation is conditional on lane 1: **if the
pressured hour shows FIFO+anchor-ordering holding every clause, the
governor loses this comparison and should not be built** - simplicity is
worth more than an absorption range nothing exercises.

## 6. Question 5: peak versus mean

The budget clause grades the worst 1-second window
(`worst_peak_upload_bits`, `swarm.rs:2272-2280`; sampled once per
simulated second from a 1 s sliding meter, `swarm.rs:1533-1537`,
`budget.rs:153-167`). A scheme that lowers mean but not peak has not
helped; here is what each piece does to peak specifically:

- **The governor bounds offered replication per 50 ms send tick** to
  slice/20 bytes. Any 1 s window sums 20 slices, so windowed replication
  is <= slice *by construction*, not by headroom. Keyframe cost per tick
  is already flattened by the `PersistId` stagger (#671's "size fallback
  waits for the staggered slot" pinned exactly this peak concern).
- **What it cannot bound:** the unsheddable lanes. Witness and control
  go out and are charged even over budget (`peer_link.rs:259-265`), so
  peak_total <= slice + witness + control. The 829 figure contains ~217
  kbps of that traffic; a governor slice must be set net of it (lane 3
  reserves headroom rather than assuming the lanes are free). FIFO
  shedding has exactly the same blind spot, so neither scheme wins or
  loses here.
- **Shedding also caps the peak** - that is what the meter does - so the
  peak clause alone will never distinguish the schemes. What
  distinguishes them under pressure is every *other* clause: shed count,
  unanchored deltas, coverage, false positives. That is why lane 5's
  settling table reports all of them, not the peak alone.

## 7. The projected budget, and how to measure it

Projection against the 829 kbps witnessed figure, derived in sections 1
and 5, measured by lanes 1 and 5:

| configuration | projected peak | shed | keyframes lost to policy | new unanchored |
|---|---|---|---|---|
| HEAD, budget 1000 | 829 (measured) | 0 | 0 | 0 |
| HEAD FIFO, budget 700 | ~700 (meter-capped) | ~17,000/hr | ~17,300/hr | ~150,000/hr projected |
| + lane 2 anchor-last, budget 700 | ~700 | ~17,000/hr (deltas only) | 0 until deltas exhausted | ~0 from shedding |
| + lane 3 governor, budget 700 | <= 483 + lanes ~217 = ~700 by construction | 0 | 0 | 0 |
| governor, budget 1000 (today's load) | 829, wire-identical | 0 | 0 | 0 |

The instrument is the existing gate: `gates/p1-swarm --peers 32 --seconds
3600` is simulated time and runs the witnessed hour in about ten wall
minutes (`scripts/p1-swarm-gate.sh:23-26`), deterministically from the
seed, and `--delta-stats` (#664) already emits changed-byte histograms.
Additions are counters and flags only, no criterion moves:

- lane 1: `--budget-kbps` override; report the witness/control/re-send
  split of the 217 kbps residual; run the 900/700/500 sweep on the
  witnessed impaired hour twice per seed (determinism is the control).
- lane 3/5: `deltas_skipped_by_governor`, and the **degradation-honesty
  counter**: per-(entity, link) maximum inter-delivery gap in ticks. Its
  invariant - the gap never exceeds the keyframe interval plus jitter
  allowance - is what makes "the governor cannot go green by silence"
  checkable, and it is the counter the shed clause cannot substitute for
  (section 4). Whether it becomes a criterion clause is owner-reserved;
  it ships as a reported number either way.

## 8. What this must not touch

1. **Canonical encoding, claim preimages, `verify_bundle`** - consumed,
   never defined (A19 section 6 item 1 carries over verbatim).
2. **Lane charging.** Governed sends are normal `Channel::State`
   datagrams; `lane_of`'s untagged-defaults-to-replication rule
   (`budget.rs:456-477`) is untouched, and lane 2's shed-order change
   must keep the existing pin that a caller can never make traffic
   cheaper by mis-tagging.
3. **H2.** Section 3's input table is the compliance argument; any later
   priority term must extend that table before it extends the code.
4. **The criterion.** No allowance, threshold, or clause moves in any
   lane. Lane 1's pressured runs are *additional* measurement legs, not
   replacements, and a disappointing number is a finding against this
   node.
5. **The meter as final authority.** The governor sits before the meter
   and the backstop stays armed behind it, unchanged in authority -
   "senders enforce their own upload budget regardless of requests"
   (docs/03-replication.md section 4, quoted at `budget.rs:33-36`).

## 9. Decomposition

Five lanes, house format, ordered 1 -> 2 -> (owner decision) -> 3 -> 4/5,
with 4 parallel to 5. Lanes 3-5 are conditional on lane 1's evidence and
the owner's go. Two lanes are currently active in `gates/p1-swarm`,
`clients/regolith`, `crates/orrery_protocol` and `crates/orrery_coordinator`;
lanes below must not start in a workspace while another lane holds it.

### Lane 1 - reproduce the overload signature before building against it
(type:measurement) Files (exclusive): `gates/p1-swarm/src/main.rs`,
`gates/p1-swarm/src/swarm.rs`. Blocked on nothing; gates all others.
Acceptance: a `--budget-kbps` override reaching the `UploadBudget`
resource; the witnessed impaired hour swept at 900/700/500 kbps, twice
per seed, with the FIFO signature (shed, shed keyframes, unanchored by
cause, coverage, false positives) and the residual-lane split in the JSON
report. MUTATION CHECK - guarded stage: the override reaching the meter.
Break: parse the flag but leave `UploadBudget` at 1 Mbps. Expect: the
sweep legs report identical peaks and the named pressure check fails.

### Lane 2 - anchors are shed last (type:task)
Files (exclusive): `crates/orrery_net/src/peer_link.rs`,
`crates/orrery_net/src/budget.rs`. Blocked on lane 1 (its runs are the
before-numbers). Within `Lane::Replication`, classify by the wire sub-tag
(`TAG_REPLICATION_DELTA`, `crates/orrery_protocol/src/channels.rs:132`)
and admit the tick's batch unsheddable-first, anchors next, deltas last.
Acceptance: `a_keyframe_is_shed_only_after_every_delta` and the existing
mis-tagging pin stays green. MUTATION CHECK - guarded stage: the
classifier feeding shed order. Break: classify delta-tagged payloads as
anchors. Expect: the shed-order property fails by name; lane accounting
tests stay green.

### Lane 3 - the delta governor (type:task, owner-gated on lane 1)
Files (exclusive): `gates/p1-swarm/src/bot.rs`, new
`gates/p1-swarm/src/governor.rs`. Sections 3-6. Acceptance:
`a_governed_send_never_skips_a_keyframe`,
`a_witnessing_links_deltas_are_never_governed`,
`the_farthest_entitys_deltas_are_skipped_first`,
`an_unpressured_governed_run_is_wire_identical`, plus the
degradation-honesty counter. MUTATION CHECK - guarded stage: the priority
ordering. Break: skip nearest / witness-link deltas first. Expect: the
witness-link and ordering legs fail by name while the slice bound stays
green. Second break: admit keyframes to the governable pool. Expect: the
keyframe leg fails and the honesty counter exceeds the keyframe interval.

### Lane 4 - client parity (type:task) Files (exclusive):
`clients/regolith/src/campaign.rs`, its tests. Blocked on lane 3; the
client already mirrors the keyframe/delta sender, so the governor seam
lands beside it, pinned byte-identical to the gate bot's for the same
state and roster.

### Lane 5 - the settling measurement (type:measurement)
No new files - lane 1's flags and lane 3's counters. Blocked on 3. The
pressured witnessed hour, FIFO+lane-2 versus governed, as section 7's
table with measured numbers; per-entity gap distributions; **no
allowance moves**. If the governed run does not beat the lane-2 run on
the non-peak clauses, that is a finding for keeping shedding, reported
to the owner as such.

## 10. Owner-reserved decisions

1. **Whether lanes 3-5 are built at all**, after lane 1's evidence is on
   the table. This node supplies the decision inputs (sections 5 and 7);
   the go/no-go is a judgement about spending complexity on an overload
   regime, and it is the owner's.
2. **Whether the degradation-honesty gap counter becomes a criterion
   clause** or stays a reported number. Evidence for the choice: lane
   5's distributions.
3. **The governor's slice constant** (proposed: net of measured witness
   and control shares with explicit headroom, from lane 1's split -
   not a round number).
4. **Sequencing against #653's AOI decision**, which changes audience
   sizes and therefore offered load; and against the two lanes currently
   holding `gates/p1-swarm` and `clients/regolith`.

## 11. Findings, and corrections to the brief that commissioned this

- **A19's "no production caller for `Witness::observe`" is stale.** The
  gate's receive seam now feeds it at `bot.rs:657-668`; replication
  cadence toward witnesses is adjudication-relevant at HEAD. This is the
  single most load-bearing correction in this node - without it, the
  witness-link exemption would look optional.
- **#672's unanchored count needs its provenance kept straight**: the
  issue measured 20,614 (0.74%) pre-classification; 116,295 is #683's
  re-measured, classified figure after the sender-accounting fix, of
  which 103,099 are keyframes genuinely lost to the 3% loss profile.
  Both numbers are real; they are not the same measurement.
- **"Peak" means the worst 1-second sliding window sampled at 1 Hz of
  simulated time** (`swarm.rs:1533-1537`, `budget.rs:153-167`), not an
  instantaneous burst - which is why per-tick slice bounding (section 6)
  bounds it exactly.
- **The H-rules' home is ADR-0050** (carrying A13/A14); A16 applies H2
  to the contact-arrow product but does not define it.
- **#653 is open and owner-reserved**, with measurement merged (#675)
  but no AOI geometry changed at HEAD; "in flight" overstates it.
- **Not verified:** the 217 kbps residual split (inferred, lane 1
  measures it); every f > 1 number in section 5's FIFO column
  (derived from #683's measured ratios, lane 1 measures them); and
  populations above 32.
