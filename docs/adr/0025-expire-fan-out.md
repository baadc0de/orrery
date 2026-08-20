# ADR-0025: `Expire` fan-out — recipient set, non-holder addressing, amplification bound

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D25

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It fixes three things
[D7](0007-authority-and-leases.md) left underspecified — who a non-holder
`Expire` goes to, what a non-holder does with it, and how much of it there may
be — and adds two rows to [D16](0016-parameter-reference.md)'s table. D7's
decision text stays accepted in full; one row of
[docs/04 §3](../04-authority.md)'s wire table gains a precise recipient
expression, and §4.3's parenthetical "(also to cell subscribers)" gains a
definition.

## Context

The wire table says `Expire` goes "registrar → holder + cell subscribers"
(docs/04 §3, line 124). That second term names a group with no registry
anywhere in the system. [D5](0005-spatial-model.md)'s interest group is
evaluated by senders, not maintained by anyone:

> In mesh and interest-mesh regimes there is no global room registry. Room
> membership is a pure function of replicated positions, so each sender
> evaluates it locally per outgoing link.
> — [docs/03 §3](../03-replication.md), line 113

The registrar is not a sender of replicated positions and holds none, so it
structurally cannot evaluate that predicate. What it *does* hold is the
coordinator interest handout: `InterestAuthority::allows(peer, grid, cell,
now_ms)` (`crates/orrery_persistd/src/gateway.rs:596-603`), backed by
`snapshot_for(peer)` — a **peer-keyed** map
(`gateway.rs:723`, `HashMap<NodeId, CoordinatorInterestSnapshot>`). There is no
`peers_covering(cell)` and no reverse index of any kind. The only way a gateway
answers "who covers this cell" is to iterate its own session registry and
call `allows` per peer, which is exactly what `Redistributor::candidates`
already does (`gateway.rs:2715-2743`).

Two further gaps. `Expire`'s addressing rule is written for the loser:

> An `Expire` addresses the loser by the token it *still believes it has
> installed* — parking has already bumped the row's own `lease_id` past it
> — and its `disposition` tells the loser where authority actually went […]
> It carries no `seq`, so the loser **keeps the pair it last knew** rather than
> resetting the entity's row.
> — [docs/04 §3](../04-authority.md), lines 177-181

That does not typecheck for a peer that never held the lease: there is no token
it believes it has installed. And nothing bounds the message count. Fan-out
multiplies messages per expiry by the size of the recipient set, and the one
rate limit near this path — D7 §10's claim bucket, 20/s sustained, burst 64
(docs/04 §10, line 553) — is an **ingress** control on peer→registrar `Claim`
traffic. It constrains registrar egress by exactly nothing. D6's ≤ 1 Mbps
per-peer upload budget and ≈ 35 Mbps hot-cell egress ceiling
([ADR-0006](0006-population-adaptive-topology.md), line 15) are the *field
host's* numbers, not a gateway's control-plane egress.

### The asymmetry that makes the bound tractable

The two dispositions are not symmetric, and this is the whole design.

- **`Reassigned{to}`** — INV-4 already converges every subscriber without any
  fan-out: "A peer applies received state for an entity only if its pair is ≥
  the highest pair seen" (docs/04 §1, line 65), and the successor's grant
  bumped the pair (INV-2, line 62). The successor's very first replicated
  envelope carries the higher pair and every observer repoints on it. Fan-out
  here buys **latency only**, bounded by one send interval (20 Hz, D16).
- **`Parked`** and **`Free`** — no successor stream ever arrives. Nothing
  raises the pair, nothing repoints anything, and every observer extrapolates a
  proxy of an entity that no node writes. Fan-out is the **only** mechanism.

Fan out only the dispositions that have no self-healing path, and the worst
case collapses — for reasons made precise in the bound below, not merely
reduced.

## Decision

**1. The recipient set is the successor candidate set, plus the holder.** For
an entity `e` committed to `(grid, cell)` on gateway `G` at instant `t`:

```
A(G, grid, cell, t) = { p ∈ Sessions(G, t) : InterestAuthority::allows(p, grid, cell, t) }

recipients(e) = { holder(e) } ∪ ( A(G, grid, cell(e), t)  if disposition ∈ {Parked, Free}
                                  ∅                        otherwise )
```

`Sessions(G, t)` is what `PeerRegistry::live_peer_leases` returns
(`gateway.rs:2601-2621`): peers with a live authenticated session and a current
generation on **this** gateway. The predicate is `InterestAuthority::allows`
called through the same seam a live `Claim` and a successor nomination pass —
never reimplemented beside it. So `A` minus the previous holder *is*
`Redistributor::candidates(grid, cell, previous_holder, now)`, which the
registrar has already materialised by the time it knows the disposition.
Fan-out therefore costs no enumeration it was not already paying.

**2. `A` is a strict subset of D5's interest set, and the exclusions are
named.** Let `I(cell, t)` be D5's true interest set — every peer whose 27-cell
neighbourhood contains `cell`. Then `A ⊆ I`, and `A` excludes:

- **peers on sibling gateways.** "A peer connected to a sibling gateway is not
  a candidate — redistribution across gateways needs a cluster-wide session
  directory, which is later work" (`gateway.rs:2676-2678`). The same sentence
  is true of fan-out, for the same reason.
- **peers whose grant lapsed while still rendering.** A coordinator grant
  carries a lifetime, not a deadline, and the gateway stamps its own expiry on
  acceptance (docs/04 §2); a peer between refreshes covers the cell visually
  and is invisible to `allows`.
- **peers with no gateway session at all** — pure mesh peers replicating from
  their island and never talking to this registrar. They are in `I` and
  unreachable here by construction.

**3. Widening happens at `Sessions`, and nowhere else.** `A` is defined as a
predicate over an addressable session set. A later cluster-wide session
directory replaces `Sessions(G, t)` with a directory-backed union across
gateways; the predicate, the recipient expression, the message, the client rule
and the bound's *shape* are unchanged, and only the constants in rule 8 are
re-derived. Nothing in this record depends on whether that directory is built.

**4. Non-holder copies reuse `LeaseMsg::Expire` verbatim; the client branches
on holdership.** No new wire variant. `LeaseMsg::Expire`
(`crates/orrery_protocol/src/authority.rs:280-291`) already carries everything a
non-holder needs — `entity`, `last_holder`, `reason`, `disposition` — and its
holder-specific field, `lease_id`, becomes *informational* for a recipient that
never installed it. This is chosen because today's clients already do the right
thing with an unexpected copy: `orrery_authority`
(`crates/orrery_authority/src/lib.rs:495-500`) drops any `Expire` whose entity
is absent from `state.leases` or whose `lease_id` does not match the installed
one. A non-holder copy sent to an unupgraded client is a no-op, so the rollout
is one-sided and needs no negotiation — which is precisely the property a new
variant would destroy.

**5. What a non-holder does, in one sentence.** A non-holder `Expire` changes
exactly one thing — the recipient's belief about who holds the entity, set to
`to` for `Reassigned{to}` and to *no holder* for `Parked` and `Free` — and must
change **nothing** else: not `SeqPair`, not any fence, not
`LocallyAuthoritative`, and it raises no `AuthorityEvent::Lost`. The rule is
written for all three dispositions even though rule 7 only ever sends two, so
that a widened fan-out is a change to who is sent what and not to what a
recipient does with it.

**6. A non-holder does nothing with `seq`, and this is not a formality.**
`Expire` carries no `seq` field, and a recipient must not synthesise, reset, or
zero one. INV-2 (docs/04 §1, line 62) is the first reason. The operative one is
the hazard docs/04 §3 (lines 181-190) already records for the holder and which
applies verbatim, and worse, to a non-holder: a peer holding no fence resolves a
late fencing `BulkNack` against the sequence pair alone, and a row left at
`(0, 0)` is superseded by *every* row — including one the registrar has already
moved past — which is how a duplicated NACK repoints `Authority.holder` at a
stale peer with no `Lost` event to show for it. A non-holder holds no fence *by
definition*, so it is permanently in that state; the pair it already learned
from replication is its only defence and the advisory must not touch it.

For the same reason the holder pointer needs its own ordering token. `lease_id`
is monotone per row (the registrar increments `row.lease_id` on every acquire),
so **a non-holder applies an `Expire` only when its `lease_id` exceeds the
highest it has already applied for that entity**, and drops it otherwise. That
is one `u64` of local high-water state per observed entity, evicted with the
cell subscription that produced it, and no wire change. Without it a
re-delivered advisory after a reconnect repoints a peer at a holder the
registrar has already replaced — the same failure, arriving through the new
door.

**7. `Reassigned` is holder-only.** Per the asymmetry above: INV-4 converges
observers on the successor's first envelope, so the fan-out copy is redundant
within one send interval. This is the clause that makes §8's arithmetic work.

## The bound

**8. Three limits, all per gateway.**

- **Per expiry:** at most `EXPIRE_FANOUT_MAX_RECIPIENTS = 128` non-holder
  copies — D6's per-cell player ceiling. A cell with more admitted sessions
  than that is already past D6's ceiling; the excess is dropped, in `NodeId`
  order so a replay is reproducible.
- **Per recipient:** a token bucket of **32 advisories/s sustained, burst 64**,
  deliberately the same *shape* as §10's claim bucket so the ingress and egress
  limits on this path read alike.
- **Per pass:** the addressable set is enumerated **once per `(grid, cell)`**,
  never once per entity, and reused for every lease in that cell — which for
  the redistribution path means reusing the `candidates` call that already
  happened (rule 1).

**9. When a limit is exceeded, the advisory is dropped, never queued.**
Counted as `expire_fanout_dropped` in `AuthorityMetrics`. Dropping is safe
because the advisory is an optimisation, not a correctness mechanism: a
recipient that loses it falls back to exactly the pre-D25 behaviour — the
entity stops being written, its proxy extrapolates and decays, and any peer
that actually cares issues a `Claim` and receives the authoritative
`Deny{Parked}` (docs/04 §3, line 121). An advisory is also dropped rather than
queued when its connection's lease lane is at `MAX_QUEUED_LEASE_OPS_PER_CONN`
(1 024, `gateway.rs:388`): `Grant`, `Deny` and `HeartbeatAck` are correctness
traffic and must never queue behind a hint.

### The arithmetic, with the field-host case substituted in

Messages per expiry:

```
M(e) = 1 + [disposition(e) ∈ {Parked, Free}] · min( |A(G, grid, cell(e), t)| , R )      R = 128
```

Burst over one `cleanup_peer_session` pass for a lost peer `P` holding `L`
leases (`gateway.rs:4909-4968` parks each lease, then hands each to
`Redistributor::redistribute`):

```
F(P) = Σ    min( |A(G, grid, cell(e))| , R )
     e ∈ parked(P)
```

Naive ceiling. `L ≤ MAX_PEER_LIVE_LEASES = 256` (`gateway.rs:397`) and
`|A| ≤ 128` at D6's ceiling, so

```
F ≤ 256 × 128 = 32 768 messages ≈ 32 768 × 64 B ≈ 2.10 MB
```

in one unpaced pass, on the reliable control stream, against no limit that
exists today. (`Expire` with a `Parked` disposition postcard-encodes to ≤ 56 B
— 1 tag + ≤ 10 entity + ≤ 10 `lease_id` + 33 `Option<NodeId>` + 1 reason +
1 disposition — so 64 B carries the frame.) That is the number this record
exists to remove.

**Why rule 7 collapses it.** `Redistributor::redistribute` returns `Parked`
in exactly three cases (`gateway.rs:3040-3078`):

1. the row is `STRONG_HELD` or `PLAYER_BOUND` — early return, never offered;
2. `candidates.is_empty()`;
3. the policy declined, or the handoff failed.

Case 2 is the load-bearing one. `candidates` **is** `A` minus the previous
holder, so case 2 says `|A \ {P}| = 0` — and the fan-out term for that entity
is therefore `0` by construction. *A lease parks for want of a successor only
when there is nobody to tell.* The two factors are not merely anti-correlated;
one is the other's emptiness test. So:

```
F(P) = Σ min(|A|, R)  +  Σ min(|A|, R)         [ case-2 leases contribute 0 ]
       strong(P)         declined(P)
```

For a field host — "a field host is just a holder with many leases"
(docs/04 §4.3, line 365) — the leases are world entities: props, loot, NPCs. A
player's character is `PLAYER_BOUND` to that player's own peer, not to the
host, so `strong(P) ≈ 0`; and the field host's cell is populated by definition,
which is what makes redistribution succeed and `declined(P) ≈ 0` too. **The
field-host disconnect, the worst case in the system, fans out approximately
nothing.** Its 256 leases reassign, each `Expire` goes to its holder alone, and
the < 10 s player-facing figure §4.3 already claims is unchanged.

The pessimistic residual — a host whose entire working set is strong-owned,
`strong(P) = L` — is what the two rate limits catch. With the per-recipient
bucket, one pass delivers at most `burst = 64` advisories to any single peer:

```
F_actual ≤ 64 · |∪A| ≤ 64 × 128 = 8 192 messages ≈ 524 KB per gateway per pass
                                                 ≈ 4 KB to any one peer
```

and the sustained rate a peer can be made to absorb is

```
32 msg/s × 64 B × 8 = 16.4 kbit/s  =  1.6 % of D6's 1 Mbps per-peer budget
```

with the gateway's aggregate at the ceiling `128 × 16.4 kbit/s ≈ 2.1 Mbit/s`,
6 % of D6's 35 Mbps hot-cell figure. Both are comfortably inside budgets that
already exist, and neither grows with `L`.

CPU, which is the limit that bites before bandwidth does. `candidates` locks
every registry entry's mutex (`gateway.rs:2601-2621`), so one enumeration is
`O(|Sessions|) ≤ MAX_PEER_REGISTRY_ENTRIES = 4 096` (`gateway.rs:395`). Per
entity that would be `256 × 4 096 = 1 048 576` lock-and-check operations in one
pass. Per `(grid, cell)` it is bounded by the cells one peer's grant may cover,
`MAX_INTEREST_GRANT_CELLS = 64` (`crates/orrery_protocol/src/coord.rs:157`):

```
64 × 4 096 = 262 144      — 4× less, and in the redistribution path 0 extra,
                            because the same call already produced `candidates`
```

## Consequences

- **The wire format does not change**, and neither does the client's
  compatibility story: an unupgraded peer drops a non-holder copy at
  `lib.rs:495-500` and is exactly as correct as it is today. Upgrade order is
  free in both directions.
- **`Expire` acquires two readings of one field.** To the holder, `lease_id` is
  the fencing token being revoked; to everyone else it is an ordering token for
  an advisory. The reading is decided by whether the recipient has that entity
  in `state.leases`, which is a local test with no wire ambiguity — but it does
  mean the field's *documented* meaning is now recipient-dependent, and docs/04
  §3 says so.
- **Parked entities converge; reassigned ones converge no faster than they do
  today.** An observer learns of a reassignment on the successor's first
  envelope (≤ one send interval at 20 Hz, D16) rather than immediately. That is
  the price of rule 7 and it is paid in the case that heals itself.
- **The advisory is best-effort and must be treated as such by everything
  downstream.** No client behaviour may become *correct only if* the advisory
  arrives; `Deny{Parked}` on a claim remains the authoritative answer. A gate
  that asserted delivery would be asserting something the bound explicitly
  permits the gateway to drop.
- **Non-holder receipt costs clients one `u64` per observed entity.** Bounded
  by the interest set (24 high-rate plus proxies, D16) and evicted with the
  subscription. A client that skips the high-water check is not safe; it is
  vulnerable to the repointing hazard of rule 6 through reconnect
  re-delivery.
- **Cross-gateway observers are not served, and this is visible.** A peer on a
  sibling gateway watching a parked entity gets no advisory and falls back to
  proxy decay plus `Deny{Parked}`. The record names this rather than hiding it,
  and rule 3 is where a session directory would fix it.
- **Two new counters.** `expire_fanout_sent` and `expire_fanout_dropped`, both
  absolute totals, on the gateway's `AuthorityMetrics`. A drop count that
  tracks a cell's population is a cell past D6's ceiling; a drop count that
  tracks *one* peer is a lane at its queue depth.

## Alternatives rejected

- **Take docs/04 §3's "cell subscribers" literally.** Unimplementable at the
  registrar: the group has no registry (docs/03, line 113), the registrar sends
  no replicated positions, and the map it does hold is peer-keyed
  (`gateway.rs:723`). Any implementation would be a *different* set wearing the
  same name, which is worse than naming the difference.
- **Build a reverse `peers_covering(cell)` index in `InterestAuthority`.** It
  would make enumeration `O(|A|)` instead of `O(|Sessions|)` — real, but it buys
  a factor the per-cell rule (rule 8) already recovers, and it adds a second
  structure that must be kept coherent with grant expiry, whose read path is
  currently the *only* place expiry is enforced (`allows` checks
  `valid_until_ms` inline, `gateway.rs:610`). A stale reverse index would leak
  advisories to peers whose interest lapsed — a fresh correctness surface in
  exchange for a constant.
- **A new `LeaseMsg::Observe`/`ExpireNotice` variant for non-holders.** Costs a
  protocol version bump and a two-sided rollout for a message whose entire
  value proposition is that unupgraded peers are unharmed by it. The only thing
  it buys is that `lease_id` would not have two readings — paid for with the
  one property (silent, one-sided rollout) that makes the feature shippable.
- **Fan out every disposition, including `Reassigned`.** Restores the
  `L × |A| = 32 768` burst in the exact scenario the system is designed
  around, to save one send interval in the case INV-4 already handles.
- **Queue advisories instead of dropping them.** Turns a bounded burst into
  unbounded memory plus head-of-line delay for `Grant`/`Deny` on the same lane
  (`MAX_QUEUED_LEASE_OPS_PER_CONN`, `gateway.rs:388`), so a fan-out storm would
  degrade arbitration — the one thing on this path that is not best-effort.
- **Bound fan-out with D7 §10's claim bucket.** It is a peer-side ingress
  limit on `Claim`; it never sees a registrar egress message and cannot
  constrain one. Naming it as the bound would be a citation, not a limit.

## Accepted with these caveats

Accepted 2026-08-20 with three limits stated rather than resolved, so that a
later reader knows which parts rest on measurement that has not been taken.

- **The two constants are derived, not measured**, and the record says so
  below. They are safe in the direction that matters — both are ceilings, and
  the structural collapse in §"The bound" is what makes the expected cost
  near-zero regardless of where the ceilings sit — but neither number should
  be quoted as an observed figure until the `|A|` distribution is measured
  under the P2 workload.
- **One open question is now closed by [D26](0026-sibling-gateways.md)**, which
  was accepted the same day. This record confines all future widening to the
  `Sessions(G, t)` term, and D26 rule 2 builds the cluster-wide session
  directory as a control-plane index that changes exactly that term and not the
  `InterestAuthority::allows` predicate, the message, the client rule, or the
  bound's shape. The seam holds as designed; no amendment is owed here when the
  directory lands.
- **The `p3-island` observability consequence is a leg, not a blanket.**
  Retiring `p3-island/src/main.rs:32-36`'s "parking is not observable from any
  peer" is reachable on the **strong-claim leg**
  (`P3_VICTIM_CLAIM_KIND=strong`), where `STRONG_HELD` parks before candidates
  are computed (`gateway.rs:3041-3050`) and the survivors are a live audience.
  On the weak leg the victim's entities reassign, which this record keeps
  holder-only because INV-4 already converges observers. Any issue asserting
  that limitation is retired must name the strong leg, or it asserts something
  unreachable by construction.

## Open questions

- **The two constants are unmeasured.** 128 recipients and 32/s are derived
  from D6's population ceiling and D7 §10's bucket shape, not from a run. The
  quantity worth measuring is the realised `|A|` distribution per cell under
  the P2 workload; if it sits far below 128 the per-expiry cap is inert and
  only the per-recipient bucket does work.
- **`Free` is folded in with `Parked` on the argument that neither produces a
  successor stream.** That is right today, when `Free` is not produced by
  `redistribute`. A future disposition that leaves an entity claimable *and*
  immediately claimed by someone would belong on the `Reassigned` side.
- **Whether the high-water map should be shared with the `PersistId`
  component's own eviction** rather than kept beside it in
  `orrery_authority` — an implementation question, not a protocol one.
