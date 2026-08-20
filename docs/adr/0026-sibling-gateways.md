# ADR-0026: Sibling gateways: ownership, reachability, and live shard handover

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D26

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **amends** two accepted records without changing
either one's decision text: [D11](0011-persistence.md)'s placement clause
("placement: rendezvous hashing over shard cells") is qualified by rule 1
below, and [D7](0007-authority-and-leases.md)'s "orphans are reassigned to the
nearest interacting peer" is qualified by rule 4. Both records carry the link.
It does **not** reopen shard→region placement, which
[docs/09](../09-services-and-ops.md) §3.3 settles as an offline ops migration;
everything here is intra-region.

## Context

Every deployment this repository has ever run has exactly one live gateway.
`docs/09` §3.2's "small production" is two `persistd` nodes, and the second one
is the first one's **journal chain follower** — the diagram routes player QUIC
to `PD1` only. So the questions a *second serving* gateway forces have never
been forced, and three of them have no answer anywhere in the accepted set.

### The ownership function says two things

`docs/08` §3.2 opens with

> Shard cells map to nodes by **rendezvous (HRW) hashing**:
> `owner(shard) = argmax_n weight_n · h(shard_id, node_id)`.

and its next paragraph opens with

> **Which shards a process owns is a deployment input, not an inference.**
> `persistd --shard` names them.

Both are implemented and they never meet. `RendezvousHasher::owner`
(`crates/orrery_persistd/src/placement.rs:66-76`) is real, correct, and reaches
production nowhere: its callers are `CellRuntime::placement_owner`
(`runtime.rs:1317-1319`), which has no callers of its own anywhere in the tree,
and the in-process test fixture `Cluster` (`cluster.rs:2420`, `:2651`), which
its own module doc calls "the library-side
harness the tests use to exercise placement and replication logic without a
real node-to-node transport" (`cluster.rs:4-8`). The binary computes nothing:
`resolve_shards` (`bin/persistd.rs:1765-1775`) takes the `--shard` list, sorts
it, validates it, and that is the shard set. Meanwhile the durable
`actor/{grid}/{shard}` row is what actually decides who may write — every
activation CASes it (`fence/mod.rs:176-186`, `:191-197`) and every checkpoint
re-reads it (§3.4 rule 1).

Redistribution across siblings asks "which gateway owns this cell" in every
one of its steps, so it cannot proceed while the answer is two functions that
disagree.

### There is no way to reach a sibling's peers

`GatewayConfig` (`crates/orrery_persist_client/src/gateway.rs:203-211`) holds
**one** `addr` and **one** expected `gateway` node id; the client "opens one
aeronet session to this endpoint". There is no gateway-to-gateway path of any
kind. The code names the gap where it bites:

> A successor must therefore be reachable on **this** gateway. A peer
> connected to a sibling gateway is not a candidate — redistribution across
> gateways needs a cluster-wide session directory, which is later work.
> (`gateway.rs:2669-2678`)

### And nothing covers a handover from an owner that is still alive

§3.4's fencing rule covers cold start, node replacement and follower
promotion. `activate_shards`' own doc says so: "the durable ownership
transition used for bootstrap, clean restart, and follower promotion"
(`fence/mod.rs:188-190`). Every one of those is a case in which the previous
owner is **gone** — and the recovery path is built on exactly that assumption.

**The concrete hazard.** A new owner's actor start-up restores every durable
lease row it finds (`actor.rs:1917-1944`) through `with_fresh_recovery_ttl`
(`actor.rs:1015-1030`), which re-arms a held row to `now_ms + LEASE_TTL_MS`
(10 s, `lease.rs:25`) on **its own** clock — `registrar_now_ms` is a
process-local `Instant` origin (`lease.rs:84-87`), so a durable expiry minted
in another process means nothing and a full fresh window is the only safe
reading. Now move shard `S` from A to B while A is alive:

1. B activates, restores peer P's row `Active`, and believes P holds it for
   10 s.
2. P's session is still to A. P heartbeats to A.
3. A's `owning_shard` (`runtime.rs:750-759`) filters `self.actors` for a shard
   that is a prefix of the cell; A no longer hosts `S`, so it returns `None`.
   P cannot renew.
4. Ten seconds later B sweeps the row expired and runs redistribution
   (`gateway.rs:2669-2716`) — over the peers connected to **B**, of which P is
   not one, and which for a freshly-activated shard is typically empty. The row
   parks.

So a live handover today loses every lease under the moving subtree *and*
spends ten seconds in a state where B's registrar asserts a holder that
provably cannot heartbeat. No `Expire` ever reaches P; P's client believes it
holds authority until its own lease clock lapses. Both halves of the invariant
this record has to state are violated.

## Decision

### 1. The durable `actor/` row is the single ownership rule; HRW is advisory

**The owner of a shard is the node named by its `actor/{grid}/{shard}` row, and
a process may serve only shards its `--shard` set names and whose row it has
won by CAS.** In full:

```
owner(g, s) = actor[g][s].owner   when actor[g][s].status ∈ {Active, Draining}
            = ⊥                   otherwise (no row, or Splitting)

serves(n, g, s)  ⟺  s ∈ shards(n)  ∧  owner(g, s) = n  ∧  status = Active
```

`shards(n)` is `--shard`, verbatim. Rendezvous hashing is **demoted from a
placement rule to a placement *planner***: a pure function an operator or a
future autoscaler uses to *propose* an assignment, never consulted on any
serving, routing, fencing or recovery path.

```
propose(g, s) = argmax_n  weight_n · h(g, s, n)
```

Three reasons, in the order they carry weight.

- **HRW is not the authority anywhere it matters, and already was not.** The
  fence row decides who may write; §3.5 already says relocate works by
  "overriding HRW via the `actor/` row", which concedes the point in the one
  case that moves a shard. A rule that every interesting case overrides is not
  the rule.
- **HRW without a membership service is a split-brain generator.** `argmax`
  over a node set is only single-valued if every node agrees on the set and the
  weights. Nothing in this system publishes that agreement — there is no
  membership protocol, and `Cluster::nodes` is a `Vec` a test constructs. Two
  nodes with different views compute different owners and both attempt to
  fence; the CAS turns that into a flapping outage rather than into corruption,
  which is better than the alternative and still an outage.
- **The saving is imaginary.** HRW's virtue is "no central assignment table",
  and the central table exists regardless: every activation reads
  `actor/{grid}/{shard}` for fencing, so removing HRW removes no read.

**This rule is provisional on there being no membership service, and that is
the fact most likely to overturn it.** Making HRW normative and deriving
`--shard` from it is the better design the moment the cluster can agree on its
own node set and weights, and it is the alternative this record would have
taken if one existed. Nothing here is hard to unwind: `propose` is already the
right function, `--shard` would become its output rather than an operator's
input, and no durable data depends on the switch because the `actor/` row
remains the fence either way. A reader who arrives holding a membership
protocol should re-open this rule before building anything on top of it.

**The hash takes the `GridId`.** If a planner is kept, `propose` mixes the
grid: `hash2(cell.to_bits(), node.id)` (`placement.rs:69`) does not, so two
grids' identically-numbered shards receive identical proposals and their
placement is perfectly correlated — the spread HRW exists for is defeated
exactly where nested grids make it matter. This is
[D22](0022-grid-id-in-the-storage-key.md)'s bug class (C-8) in the one family
that escaped the sweep, and `cluster.rs:2425-2427` already *documents* the
correct behaviour ("Placement is keyed by `(grid, cell)`") above a call that
discards the grid. Fixing it is cheap precisely because the function is now
advisory: no durable data depends on its output.

### 2. Reachability: peers multi-home; the session directory is control-plane only

**A peer holds one gateway session per gateway that owns a shard it is
interested in; a gateway never proxies another gateway's client traffic.**
`GatewayConfig` becomes a resolver over `(grid, shard) → gateway`, seeded from
the coordinator and repaired by the redirect in rule 5.

The proxy alternative is rejected on identity, not on cost.
`SessionTokenClaimsV1` binds `account` to `node`, "the iroh transport identity
this token authorizes" (`identity.rs:73-88`), and the gateway verifies that
binding against the QUIC peer of the connection the frame arrived on. A frame
proxied by gateway A to gateway B arrives with A as B's transport peer, so B
would have to accept A's *assertion* of who authored it. That converts a
per-connection cryptographic binding into a hop-by-hop trust relation between
processes — the one property the session token exists to provide, traded away
to save the client a second dial. It also puts a second process in the ack path
of a bulk write whose client-observed p99 budget is 5 ms (D11), and makes A an
availability dependency for shards A does not own.

**The cluster-wide session directory is still built, and it is not a data
path.** Each gateway publishes, for each peer with a live session,
`session/{grid}/{node} → (gateway_node_id, generation, expires_at)`, written on
session open and on the `retiring` transition already tracked at
`gateway.rs:2497` and `:2552-2553`, and TTL'd so a crashed gateway's rows lapse.
Its properties are chosen so that nothing correctness-bearing rests on it:

- **Advisory, never authorizing.** It answers "which gateway can currently
  reach node P", and nothing else. No admission, eligibility or fencing
  decision reads it. A stale row costs a wasted control message.
- **Eventually consistent.** It is read to *address* a peer, never to decide
  whether a peer exists; the authority on that is the session itself, on the
  gateway that terminates it.
- **It carries registrar control frames only.** A gateway that must deliver an
  `Expire` to a peer it does not hold a session for looks the peer up and sends
  the *notification* to the owning gateway over an internal control transport,
  which re-emits it on the peer's own session. Nothing client-authored travels
  that link, so no identity is re-terminated. This is the primitive the
  `Expire` fan-out decision needs, and its recipient set may widen under this
  record without it being rewritten.

### 3. Live handover is a drain, and the drain is what makes recovery correct

**A live shard handover A→B divests every lease under the moving subtree,
through the holders' own sessions on A, before the ownership CAS — so B
restores no held row and the previous owner's sessions are gone by
construction.**

The point is worth stating the other way round, because it is the whole
design: `with_fresh_recovery_ttl` is **correct exactly when the previous
owner's sessions are gone**, which is why it is right for crash, restart and
promotion and wrong for a live move. The handover's job is to make its
precondition true, not to change the function.

A new fence status `Draining { successor }` joins `Active` and `Splitting`
(`fence/mod.rs:35-41`).

```mermaid
sequenceDiagram
    participant P as Peers on A (holders under S)
    participant A as Gateway A (owner, epoch e)
    participant F as FoundationDB (actor/{g}/{S})
    participant B as Gateway B (--shard includes S, inactive)
    A->>F: CAS (A,e,Active) → (A,e,Draining{B})
    Note over A: still the single writer; only admission closes
    A->>P: Deny{Draining} to new Claims under S
    loop every live lease row under S
        A->>P: Expire{Revoked, Reassigned{to} or Parked} on the holder's own session
    end
    Note over A,P: unanswered divests hit handoff_deadline_ms (300 ms)<br/>and park unconditionally — the drain is bounded
    A->>F: Checkpoint(PreHandover); stop accepting diffs for S (NACK epoch)
    A->>F: CAS (A,e,Draining{B}) → (B,e+1,Active)
    Note over A: from here A's epoch is stale: late checkpoints conflict (§3.4.1)
    B->>F: load checkpoint, replay tail (§3.4.2–3)
    Note over B: every restored row is parked; no held row exists to re-arm
    B->>B: open mailbox, ready(g, S, e+1)
    P->>A: next diff/claim under S
    A-->>P: WrongOwner{grid, shard, owner: B}
    P->>B: dial B, re-Claim; parked rows unpark per §7 / strong grace
```

Numbered, with the obligation each step discharges:

1. **Mark.** A CASes `actor/{g}/{S}` from `(A, e, Active)` to
   `(A, e, Draining{B})`. Status only — A remains owner, epoch, and single
   writer. A losing CAS aborts the handover with nothing changed.
2. **Close admission.** A denies new `Claim`s for cells under `S`, keeps
   serving diffs and heartbeats. The write path stays live for the whole drain,
   so the drain is invisible to gameplay for the same reason §3.5's NACK window
   is.
3. **Divest.** For each live row under `S`, A runs the ordinary cooperative
   path and emits `Expire` **on the holder's own connection**. The holder is by
   construction connected to A, so this needs neither the directory nor a
   cross-gateway grant. Weak rows may be reassigned to an eligible peer still
   on A; everything else parks with `own_seq` intact and §4.3's grace re-armed.
4. **Bound it.** A holder that does not answer within `handoff_deadline_ms`
   (300 ms, `gateway.rs:2161`) is revoked unconditionally and its row parks. A
   drain of `k` rows therefore completes within one deadline, not `k` of them.
5. **Quiesce-flush.** `Checkpoint(PreHandover)` — §3.5's `PreSplit` cause under
   a second name — then stop accepting diffs for `S`. NACKed diffs are dropped,
   not retried, exactly as §3.5 already specifies.
6. **Hand over.** A CASes `(A, e, Draining{B})` → `(B, e+1, Active)`. From this
   instant `owning_shard` returning `None` on A is *correct* rather than a
   trap, because step 3 left nobody heartbeating to A for `S`.
7. **Open.** B loads and replays (§3.4 steps 2–3), restores rows — all parked —
   opens the mailbox, announces `ready(g, S, e+1)`.
8. **Redirect.** A peer's next write under `S` is answered `WrongOwner{grid,
   shard, owner}`; it re-resolves, dials B, and re-claims. Parked rows resume
   under the ordinary unpark rules; a `PLAYER_BOUND` or strong-held row is
   reclaimable only by the identity that held it.

The row is never retired. Only its owner and epoch move.

### 4. A successor is never selected on a sibling gateway

**Candidacy stays "a live authenticated session on the gateway that owns the
entity's shard", and `docs/04` §4.3's narrowing of D7 is blessed rather than
lifted.** Three reasons:

- **A grant needs liveness the granting gateway can observe.** §4.3 already
  requires "never an unreachable holder" — a grant whose successor's session
  dies before the push lands is unwound and the entity parks. A cross-gateway
  successor's session liveness is a fact only the sibling holds, so the granting
  registrar would be asserting a holder it cannot watch.
- **Heartbeats have to reach the actor, and they follow the session.** If P is
  on B and the row's actor is on A, renewing requires forwarding P's heartbeat
  frames A-ward — which is the client-traffic proxy rule 2 rejects, arriving
  through the back door.
- **The single-writer counter stays countable.** `observe_fencing_rejection`
  (`gateway.rs:1029-1043`) is per-process by construction and only counts a
  rejection whose live row names a different holder with an unexpired lease.
  With the narrowing, each process's count is sound on its own and the cluster
  figure is the sum. Lifting it would make a cross-process invariant that no
  single process can evaluate, and the metric would have to be re-derived from
  a join nobody performs.

**And under rule 2 the narrowing costs almost nothing**, which is why blessing
it is not a resignation. A peer interested in cells under `S` holds a session
to `S`'s gateway, because that is where its own writes must go. So "has a
session on this gateway" and "is interested in this cell" converge on the same
peer set, and `InterestAuthority::allows` — the predicate candidacy already
shares with live claims — is doing the narrowing anyway. The residual is the
peer interested in `S` that has not *yet* dialled `S`'s owner; it parks, and
unparks on its first claim.

**But that mitigation is not in force yet, and until it is, this rule is a
capability loss.** The convergence argument above assumes multi-homing — a peer
holding a session to every gateway owning a shard it cares about. This record
does not build that: the `GatewayConfig` resolver and the coordinator's
`(grid, shard) → gateway` publication are named as consequences and left to
later P3 work. Until they land, a two-gateway deployment parks every entity
whose only eligible successor is connected to the sibling, where a one-gateway
deployment would have reassigned it. That is strictly worse, for exactly the
configuration this record exists to enable. It is therefore a sequencing
constraint and not merely a backlog item: **multi-homing lands before a second
gateway carries live players, not after.**

### 5. The handover invariant, in checkable terms

**I1 — no overlapping live ownership.** At every instant, for every grid `g`,
the set `{ (n, s) : serves(n, g, s) }` contains no two entries whose shard cells
overlap (`s₁.is_prefix_of(s₂)` or the converse, including equality). Checkable
without a global pause, because it is a property of one durable row per shard:
every transition is a CAS on `actor/{g}/{s}`, `Draining` is not `Active`, and
`(B, e+1, Active)` is reachable only from `(A, e, Draining{B})`. The harness
assertion is the union of every node's `shard_cells()` filtered to rows naming
that node `Active`, tested pairwise for prefix containment — the same shape as
`fence::validate_activation_set`'s non-overlap check, taken cluster-wide.

**I2 — no holder loses the ability to heartbeat without an `Expire`.** For
every lease row live at step 1, an `Expire` frame was written to its holder's
session **before** the step 6 CAS. Two counters, both zero-valued:
`leases_live_at_drain_start − expires_delivered_before_cas == 0`, and
`heartbeats_rejected_wrong_owner == 0` across the handover window. The second
is the useful one in a fault injection, because it fails on exactly the bug the
Context describes: it counts renewals that arrived at a process no longer
hosting the shard.

Both are properties of a *planned* handover. A crash mid-drain degrades to the
existing path — the `Draining` row's owner is dead, `activate_shards` fences it
away, and the sessions really are gone, so `with_fresh_recovery_ttl` is back on
its precondition.

## Consequences

- **`docs/08` §3.2 states one ownership rule** and the HRW paragraph is
  rewritten as the planner. The `CellId::ROOT` fallback text and its measured
  numbers — 96 % of a 7.81 ms ack in `router_apply`, 8 921 of 10 000 leases
  withdrawn, against 1.03 ms and 174 on a deployed shard set; 386 ms activation
  and 63 ms recovery for 128 shards, 503 ms to readiness — are unchanged and
  still true: they were always about `--shard`, which is now the only rule
  there is.
- **`docs/04` §4.3's "on this gateway" narrowing is normative**, not an
  implementation note. That file is not edited here (another lane owns it); the
  edit is owed as a follow-up and named in this record's PR.
- **`RendezvousHasher` keeps its tests and loses its callers.** `runtime.rs:1318`
  and `Cluster`'s routing (`cluster.rs:2420`, `:2437`, `:2523-2525`, `:2651`)
  are harness paths and stay harness paths, but they must stop being read as a
  statement about production placement, and `cluster.rs:2425-2427`'s comment
  needs the grid actually threaded or the claim dropped.
- **Three additions are owed to the wire and the fence.** `FenceStatus::Draining
  { successor }`; a `WrongOwner { grid, shard, owner }` rejection, which is the
  encoding this record was flagged as possibly narrowing; and
  `CheckpointCause::PreHandover`. None is a protocol *break*: a peer that does
  not understand the redirect falls back to the reconnect it already performs.
- **`--shard` sets across siblings must not overlap**, and that is now an
  operator-checkable property of the deployment rather than an emergent one.
  `fence::validate_activation_set` enforces it within one process; across
  processes the CAS enforces it after the fact, at the cost of a failed
  activation. A `persistd` pre-flight that reads the sibling rows first would
  turn that into a startup error, and is worth having.
- **The session directory is a new FDB row family** and therefore carries the
  `GridId` discriminator, per D22.
- **Multi-homing changes the client's dial story**, which is a `orrery_persist_client`
  change (`GatewayConfig` → a resolver) and a coordinator responsibility for
  publishing `(grid, shard) → gateway`. Both are P3 work this record does not do.
- **Two sibling gateways do not change the durability posture.** Chain
  replication remains per-node (D11, [D23](0023-follower-journal-retention.md));
  a sibling is not a follower and a follower is not a sibling, and a deployment
  that wants both runs both relationships.

## Alternatives considered

- **Make HRW normative and derive `--shard` from it.** The honest version of
  option (a), and it is the one this record would have taken if a membership
  service existed. Rejected: it does not, HRW's argmax is single-valued only
  under one, and building a membership protocol to avoid reading a row that is
  read anyway inverts the cost.
- **Keep both rules, with HRW as the default and `--shard` as an override.**
  Rejected as the status quo with a coat of paint. "Exactly one normative rule"
  is the requirement precisely because a reader cannot currently answer "who
  owns this shard" from the specification, and a default-plus-override still
  has two answers whenever they disagree — which is the only interesting case.
- **Gateway-to-gateway proxying of client traffic.** Rejected above on
  identity: it re-terminates the per-connection binding
  `SessionTokenClaimsV1` establishes, and every mitigation for that (signed
  forwarding envelopes, mutual gateway attestation) is a second authentication
  system to avoid a second dial.
- **Cross-gateway successor grants with heartbeat forwarding.** The
  generous reading of (d). Rejected: it needs the proxy, and it makes the
  single-writer metric a cross-process join.
- **Handover by TTL: stop serving `S` on A and let B's fresh-TTL restore ride
  it out.** The zero-work option, and it is the bug. Ten seconds of a registrar
  asserting a holder that cannot renew, no `Expire` anywhere, and a
  redistribution over an empty candidate set. Rejected as the thing this record
  exists to prevent.
- **Transfer the registrar's clock along with the shard** — carry a wall-clock
  or logical expiry in the durable row so B honours A's remaining TTL. Rejected:
  `registrar_now_ms` is deliberately never wall-clock or peer-supplied
  (`lease.rs:79-87`), and honouring the remaining TTL would keep the row *held*
  by a peer that still cannot reach B. It solves the arithmetic and not the
  reachability, which is the actual problem.
- **Migrate sessions, not just shards** — hand A's QUIC sessions to B. Rejected:
  a session is an iroh connection to A's endpoint with A's transport identity;
  "migrating" it is a redirect with extra steps, which is step 8.
- **Defer the whole question until a second gateway is actually deployed.**
  Rejected because the deferral is not free: `docs/08` §3.2 currently ships two
  contradictory ownership rules, and every reader of it — human or agent —
  pays for that today.
