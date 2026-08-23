# ADR-0040: Visibility is layered by consequence — heuristics shape rate, shared facts may gate membership, and secrets stay cluster-side until reveal

**Status:** Proposed · **Date:** 2026-08-23 · **Decision:** D40

This record is non-normative until accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete accepted decision set. Acceptance is
reserved to the owner.

**Supersedes:** nothing. It proposes a missing authority boundary across
[D5]'s cell AOI, [D8]'s interest/proxy model, [D9]/[D10]'s replay and witness
paths, [D12]'s coordinator manifests, and [D29]'s cluster spot-replay path. It
does not amend any accepted record while Proposed. The accepted records and
expansion rules that acceptance would put in tension are named explicitly in
clause (h), for the owner to resolve.

Out of scope: choosing aimed fire or adding line-of-sight lock break to
Regolith ([#351], [#352], [#353]); selecting an occlusion data structure or
geometry format; implementing a static PVS, dynamic-occluder broad phase,
delivery receipt, stealth mechanic, relay policy, manifest redaction, witness
protocol, or new `Ruleset` API; changing D16 parameters; preventing a client
from displaying state it legitimately received; and claiming that
zero-knowledge proofs make continuous 20–60 Hz hidden simulation practical.
This record fixes the proposed layering and safety obligations, not any one
game's visibility mechanic.

## Context

### 1. Three different questions currently share one word

The tree has three spatial-query layers, with three different failure
directions:

| layer | current mechanism | consequence of a wrong answer |
|---|---|---|
| storage/partition | D5 `CellId` hierarchy and contiguous cell ranges | extra or missed load/routing work |
| replication/presentation | 27-cell AOI, per-client visibility, bounded high-rate set, 1–4 Hz proxies | bandwidth or presentation fidelity |
| ruleset/adjudication | reads through `StateView::neighbor` | a gameplay outcome and its replay evidence |

[docs/01 §9] already confines octrees, k-d trees, BVHs and similar structures
to optional **per-cell in-memory query structures**; they are not partition
units. [docs/03 §3] then says, in words verified in the current file, “Cell
visibility is the coarse filter only,” and makes fine-grained interest a
second stage. In the landed code, `visibility.rs` derives each `ClientAoi`
from the coordinator's island manifest and sets replicon visibility by cell;
`interest.rs` ranks in-AOI entities by float squared distance, retains at most
the configured high-rate cap (24 by default) with a 15% margin, and assigns
every other candidate a 1–4 Hz proxy.

The design and implementation are not identical. [docs/03 §4.1] specifies a
multi-factor score with distance, interaction and aim plus gameplay pins; the
current `interest.rs` explicitly says “Selection is purely distance-based.”
Therefore [#354]'s statement that scoring is *already* multi-factor survives
as an expansion-document design claim, not as a claim about landed behavior.
This proposal relies only on the shared property: replication scoring is
outside the adjudicated step and can safely add traffic or alter rate while it
does not make gameplay truth.

The ruleset side has the opposite contract. `StateView::neighbor` takes
`&mut self`, appends each successful first read to `self.reads`, and documents
why: “A view that let neighbours be read without recording would produce
windows that cannot be replayed.” [#353]'s deferred LoS mechanic is therefore
not a reusable replication query. Ruleset geometry must be integer-exact,
ruleset-owned and bounded in declared reads; a conservative float-space PVS
used only to schedule replication has neither that authority nor that proof
burden.

### 2. The accepted baseline is AOI membership with a proxy floor

[D5] makes a peer's own cell plus its 3×3×3 neighborhood — 27 interest cells —
the replication interest group. [D8] says remote entities outside the
high-rate set are extrapolated at 1–4 Hz. [docs/03 §4] sharpens the two sets:
the high-rate cap is 24, every other in-AOI entity is a proxy, and current
interaction partners are pinned high-rate because hit validation depends on
them. [docs/03 §5.2] further says low-priority entities send eventually while
the link budget covers the sum of floor rates.

That baseline has an important fail direction. A false positive in visibility
spends bytes. A false negative removes state an observer may need to predict,
present or validate an effect. [D10] makes the reconciliation-error monitor and
continuous witness re-execution inputs to discrepancy escalation, so starving
an honest observer can manufacture the very disagreement the trust path is
supposed to detect.

The no-LoS combat posture in [#352] illustrates the distinction but does not
become an architectural dependency of this record: if a ruleset permits an
occluded entity to affect an observer, occlusion cannot remove that entity
from the observer's replication membership. It may lower an unpinned entity's
rate no further than the accepted proxy floor. If a future ruleset makes LoS
an exact precondition to effect, the premise changes and membership culling
can become safe — but then visibility itself is gameplay-load-bearing and
must move onto adjudicable facts.

### 3. The three regimes are per state category, not per game

The research in [#354] ends with three regimes separated by **who may compute
visibility and against what evidence**:

| regime | meaning of visibility | possible authority |
|---|---|---|
| 1 — attention | heuristic fidelity preference | each receiver may request; each sender schedules |
| 2 — shared geometry | deterministic consequence of mutually known state | both peers compute the same predicate; sender gates delivery |
| 3 — stealth/scanning | gameplay result derived from a secret or contested fact | ruleset decides detection; a secret-holding cluster reveal gate enforces withholding |

These regimes compose. One entity may carry public motion under regime 1,
LoS-gated weapon state under regime 2, and a hidden sensor signature under
regime 3. Calling an entire game “FPS” or “stealth” would give one mechanism
authority over unrelated state and obscure which facts a witness must learn.

The authority split is also temporal. A replication heuristic can answer
“how often should I send what membership already permits?” without changing
truth. A shared-geometry predicate can answer “is membership presently
required?” only if both sides can reproduce the same conservative result from
logged shared inputs. A secret detection result is itself gameplay: the
ruleset must emit it as a logged fact, and the replication layer may only obey
that fact after it exists.

### 4. The governing invariant turns hiding into a bounded state transition

For subject entity `e`, observer `o` and tick `t`, define

```text
affects(e,o,t) = e may cause a ruleset-visible effect on o at t
                 from the state and permissions held before t

member(e,o,t)  = o has an installed replica sufficient to consume,
                 present and validate every such effect

reveal(e,o,r)  = a Ruleset-produced, hash-chained event at tick r that
                 authorizes disclosure and pins e into o's membership
```

The proposed invariant is:

> **Any entity must be replicated to anyone it can currently affect; hiding
> is permitted exactly where affecting first requires a logged reveal.**

In symbols:

```text
affects(e,o,t)                         => member(e,o,t)
not member(e,o,t) and affects(e,o,f)   => exists r < f: reveal(e,o,r)
                                           and member(e,o,f)
```

“Currently affect” includes direct damage, collision, targeting, trading,
ownership and any delivered event whose correct presentation or validation
depends on `e`; it is not limited to Euclidean range. A reveal that arrives
after the effect is not a reveal precondition. A local rendering decision is
not logged. A sender's bare assertion that it revealed is not an installed
replica.

The invariant gives two plain bounds. Let `A_o(t)` be all AOI entities and
`I_o(t)` the influence set — entities that can affect `o` without first
crossing a reveal transition. Then

```text
I_o(t) subset_of M_o(t)
max_hidden_o(t) = |A_o(t)| - |I_o(t)|

wire_floor_o >= sum floor_bytes(e) * floor_hz(e), for e in I_o(t)
```

No occlusion algorithm can lawfully hide more than the first line permits or
promise bandwidth below the second. Worked bound: if an observer's 27 cells
contain 80 entities and 30 can affect it without reveal, at most `80 - 30 =
50` may be membership-hidden; all 30 stay replicated even if every one is
behind geometry. If each required proxy snapshot were measured at 120 bytes
and its accepted floor were 1 Hz, those 30 alone impose `30 × 120 × 1 = 3,600
B/s = 28.8 kb/s` before QUIC framing, witness traffic or high-rate pins. The
120-byte premise is an example to show the arithmetic, not a measurement or a
D16 default.

Reveal also has a time bound. If `L_log`, `L_send`, and `L_install` are the
measured worst-case time to commit the reveal to the evidence stream, deliver
the required baseline, and install it, then an effect may occur no earlier
than

```text
L_reveal = L_log + L_send + L_install
effect_tick >= reveal_tick + ceil(60 * L_reveal_seconds)
```

At a hypothetical measured `20 + 45 + 17 = 82 ms`, the guard is
`ceil(60 × .082) = 5 ticks`; choosing three ticks because the ordinary send
interval is 50 ms would be unsafe. Games may instead make reveal instantaneous
in simulation and delay effect until the baseline acknowledgement; either
shape pays the same causal bound. The numbers must come from the implementing
harness, not this proposal.

### 5. “Witnessed-yet-hidden” is an impossibility frontier, not a backlog item

[D9] streams an authoritative core entity's signed log to its cell-epoch
witness set, and [D10] requires those witnesses continuously to re-execute the
stream. [D28] announces the candidate and selected sets; [D34] additionally
places the candidates' account bindings inside the signed announcement. A
peer witness that can validate hidden position or detection inputs necessarily
learns them. A peer denied those inputs cannot validate them in real time.

[#354]'s “20 Hz” description of that stream does not survive as one current
tree fact. D9 and [docs/02 §7] still say one frame per 20 Hz send, but the
landed `orrery_witness::frame_interval_ticks` and [docs/03 §5.3a] derive one
frame per ten ticks — 6 Hz — at D16's mesh defaults, while the promoted
one-link case may tighten back to 20 Hz. The ≤7-link fan-out does survive.
This accepted-ADR/implementation cadence drift needs its own reconciliation;
the witnessed-yet-hidden problem is unchanged at either cadence.

The resulting frontier is honest and unresolved:

```text
secrecy + peer validation  => delay validation until reveal; lose real-time
secrecy + real-time        => use a trusted secret-holder; lose peer validation
peer validation + real-time => disclose to witnesses; lose secrecy from peers
```

There is no fourth row in this proposal. In particular, behavioral telemetry
can suspect that a player acted on secret state but cannot prove what a client
knew or displayed. That matches [docs/07 §8]'s accepted limitation: “ESP
within the interest set is not stopped.” Receipt is the enforceable boundary;
knowledge is not.

The existing escape seam is cluster replay, but its scope must not be
overstated. [D29] sends **low-population reversible intents** through a
quarantined provisional commit and finalizes every one by cluster spot replay.
Its clause 7 reuses `AdjudicationExecutor`; [docs/06 §10] budgets a 180-tick
single-entity replay at `< 5 ms`, hence about `1 / .005 = 200` windows/s/core.
Using that path for hidden continuous-state windows is a **forward
extrapolation**. D29 does not authorize it, does not make provisional value
spendable before finalization, and does not turn cluster replay into real-time
peer witnessing.

The extrapolated escape trades the second property: keep secret state in the
cluster, reveal per observer, and let cluster infrastructure replay hidden
windows. Its price is an infrastructure referee on the hot path for that
state category, plus a new workload whose arrival rate is

```text
replay_cores >= hidden_entities * windows_per_entity_per_second
               * replay_seconds_per_window / target_utilization
```

For example, 2,000 hidden entities finalized once per 3-second window produce
`2,000 / 3 = 667 windows/s`; at the `<5 ms` **budget ceiling** and 70%
utilization that is at least `667 × .005 / .70 = 4.77`, hence 5 replay cores,
before redundancy and bursts. This is sizing arithmetic, not measured
capacity. It makes the trust and operations price visible; it does not solve
trustless witnessed-yet-hidden.

### 6. Metadata is a separate visibility frontier

Withholding entity components does not withhold membership metadata. The
current `IslandManifest` contains every peer's `NodeId` and occupied cells and
is broadcast to every peer it names; `visibility.rs` reads exactly those
`PeerEntry.cells` to derive `ClientAoi`. Independently, D28's signed witness
announcement names `candidates` and `selected`, and D34 carries the parallel
candidate-account vector. Thus each is an oracle before geometry runs:

```text
redacted entity state + named (NodeId, cells) manifest  => presence + coarse location
redacted entity state + named witness membership       => presence + validation role
```

A hidden subject appearing under a stable identity in either channel is not
secret merely because replicon withheld its transform. A direct interest-mesh
connection can add a transport-graph presence oracle; [D10] already accepts
peer IP exposure and offers relay-only privacy mode at a latency cost. Closing
one channel while leaving another open is a game-policy choice about presence
leakage, not full secrecy.

Manifest redaction, pseudonyms, cell coarsening, witness-role substitution and
relay pinning are all **forward extrapolations**. They would affect topology,
reachability, audit and identity semantics and are deliberately not selected
here. Any implementation proposal must inventory metadata channels separately
from component replication and state, for each channel, the identity and
location granularity an observer learns.

## Proposed decision

### (a) One authority matrix applies per state category

> **Every replicated state category is classified as `Attention`,
> `SharedVisibility`, or `SecretVisibility`. The classification fixes who may
> compute visibility, which evidence the computation consumes, and whether it
> may change rate or membership. A game may use all three classifications at
> once; no game-wide visibility mode exists.**

```text
Attention:
    receiver may request priority; sender owns scheduling and budget
    unaudited heuristic may change rate, never required membership

SharedVisibility:
    Ruleset owns effect semantics
    peers reproduce one conservative predicate from shared logged inputs
    sender owns transmission; predicate may remove membership only under (b)

SecretVisibility:
    Ruleset owns detection/reveal
    cluster owns unrevealed state and per-observer disclosure
    peers neither hold the secret nor decide its membership gate
```

A field host in D6's promoted regime is infrastructure and may execute the
sender/oracle roles for categories assigned to it. That does not let a mesh
peer call itself a field host or make a unilateral predicate authoritative.

### (b) The affect-or-reveal invariant is the membership gate

> **For every `(entity, observer, tick)`, `affects => member`. Membership may
> be absent only when every path by which the entity could affect the observer
> first crosses a logged `Reveal` transition, and the effect is causally held
> until the observer has installed the revealed baseline.**

Rate reduction is allowed only above the minimum needed to consume and
validate possible effects. Current interaction partners bypass heuristic
scoring and remain pinned high-rate as [docs/03 §4.1] specifies. Witness logs,
claims and gap repair are a separate lane and no presentation-visibility
factor may shed them.

An implementation must expose `|A_o|`, `|I_o|`, `|M_o|`, hidden membership,
required floor bytes/s, reveal-to-baseline latency and effects held for reveal.
The permanent gate must fail on either counterexample:

```text
exists e,o,t: affects(e,o,t) and not member(e,o,t)
exists reveal(e,o,r), effect(e,o,f): f <= r or baseline_not_installed(e,o,f)
```

### (c) Attention heuristics remain outside adjudication and fail toward bytes

> **An `Attention` query may use approximate or asymmetric inputs and may be
> computed independently per peer. Its only authority is rate and priority;
> uncertainty, missing inputs and disagreement choose the more-visible rate,
> never membership removal.**

Static occlusion as a score multiplier is admissible in this regime only if it
clamps at the accepted proxy floor and pins bypass it. Dynamic-occluder
raycasts are not prohibited, but they must justify their cost against the same
safe failure direction. None of these queries enters `StateView::reads`, and
none may be cited as proof of a gameplay LoS result.

### (d) Shared visibility must be symmetric, conservative and retrospectively reproducible

> **A `SharedVisibility` membership predicate is legal only when both peers
> compute the same result from versioned geometry and logged shared state, its
> uncertain cases resolve to visible, and the sender records enough policy
> version and input references for retrospective replay. The Ruleset, not the
> replication query, remains authority over whether LoS or another shared fact
> actually gates an effect.**

Sender-side culling is the proposed transmission shape: a receiver cannot ask
an honest subject to leak more than the sender permits. This does **not** prove
delivery. Replaying `(subject position, observer position, geometry version,
visibility-policy version, tick)` can prove what the policy required, but a
victim cannot prove packet non-receipt from silence. Any effect-validity rule
that requires a recent signed baseline acknowledgement is a separate future
proposal; this record neither assumes it nor permits lack of such a proof to
be described as solved.

Latency grace must be derived, not guessed:

```text
visible_if PVS(subject_cell, expanded(observer_bounds, v_max * L_lookahead))
L_lookahead >= measured position age + one-way delivery tail + install tail
```

All boundary, stale-version and missing-table cases return visible. Exact
ruleset LoS may be narrower; a replication PVS must never be reused as its
gameplay proof.

### (e) Secret detection is a ruleset fact; unrevealed state is cluster-owned

> **For `SecretVisibility`, detection is an adjudicated Ruleset output such as
> `Reveal { observer, subject, tick, category }`; replication only consumes
> that output. State intended to remain secret from peers is held by cluster
> infrastructure until reveal. An untrusted peer may not be both holder of the
> secret and authority over withholding it.**

This is a **forward extrapolation** from [docs/06 §9]'s cluster-side
secret-randomness rule and D10's “server-side-secret state … revealed late”
mitigation. It adds no such event or gateway service today. Effects from hidden
state are refused or delayed until clause (b)'s reveal/baseline barrier clears;
“stealth plus unseen effect” is prohibited for the same observer and state
category.

### (f) Witnessed-yet-hidden remains an explicit trust-tier choice

> **No implementation may claim secrecy, real-time peer validation and peer
> witness access to the hidden state simultaneously. A state category chooses
> two and reports which property it gives up. Cluster validation is the
> proposed escape for real-time secret state, and it is labeled infrastructure
> trust, never peer witnessing.**

Extending D29's executor to hidden windows requires a separate accepted record
that specifies scheduling, evidence custody, replay cadence, overload behavior
and whether failure delays an effect or annuls a consequence. Until then, the
path is an extrapolation and `SecretVisibility` has no implementable continuous
validation mechanism under this proposal alone.

### (g) Metadata leakage is measured independently of geometry

> **Every visibility policy carries a channel inventory covering entity
> components, island manifests, witness announcements, connection topology,
> reliable control events and durable receipts. For each observer and channel
> it states whether identity, presence, coarse cell, exact position, timing or
> role leaks. “Hidden” without that inventory is not a valid security claim.**

The framework exposes leakage choices; it does not silently redact topology
or witness records whose existing consumers require them. A game's accepted
leak policy must say which of these are intentionally public. Changes to a
manifest, announcement or transport route require their own owner-approved
record because those are authority and reachability surfaces, not cosmetic
serialization.

### (h) Accepted records and expansion rules the owner must resolve

This proposal is intentionally not self-accepting. Acceptance would require
the owner to decide each tension below and then amend or supersede the named
record explicitly:

- **D8 and [docs/03 §4–§5].** D8 says remote out-of-set entities are 1–4 Hz
  proxies; docs/03 says cell visibility bounds what may be received and every
  remaining in-AOI entity is proxied. Regimes 2 and 3 propose membership-drop
  for some in-AOI state. The owner must decide whether the proxy guarantee is
  narrowed to the influence set and revealed state, or whether membership-drop
  is rejected.
- **D9 and D10.** D9 streams authoritative core logs to peer witnesses and D10
  requires continuous peer re-execution. Secret state cannot obey that rule
  while remaining secret from those peers. The owner must choose cluster
  validation, delayed peer validation after reveal, or disclosure to witnesses
  per state category. Separately, D9/docs/02's 20 Hz frame statement and the
  landed/docs/03 6 Hz mesh default need reconciliation; this proposal does not
  choose which text is amended.
- **D29.** Its spot-replay path is accepted only for low-population reversible
  intents, with quarantined value and 100% finalization. The owner must decide
  whether a new hidden-window workload may reuse its executor; this record
  cannot widen D29 by analogy.
- **D6 and D12.** Visibility-sensitive promotion, a cluster reveal gate, and
  manifest redaction are not in their accepted topology/service duties. The
  owner must decide whether secrecy is a promotion criterion and which service
  owns secret state and disclosure.
- **D28/D34 and [docs/07 §4.1].** Announced witness candidates, selected nodes
  and account bindings are presence oracles. The owner must choose whether
  secret categories use peer witnesses and leak that metadata, use a redacted
  or substitute announcement, or leave the peer-witness path.
- **[docs/07 §4.5 and §8].** Section 8's ESP limitation remains accurate, but
  §4.5 is stale against accepted D29: it still lists a P5 field-host fallback,
  says provisional value is optimistically spendable and permits sampling.
  D29 instead makes provisional commit the sole P5 fallback, quarantines its
  value, and fixes finalization at 100%. The owner must update docs/07 before
  using it as the specification for any cluster-validation extension.

## Consequences if accepted

- Visibility gains one governing safety test across all regimes. Geometry,
  stealth and replication mechanisms can vary without changing
  `affects => member` or the reveal barrier.
- The safe bandwidth claim becomes a lower bound rather than “occlusion saves
  bandwidth.” Required influence-set traffic is irreducible; only the
  complement can be hidden, and every byte claim must show both sets.
- Regime 1 can evolve independently of the verifiable core because it fails
  toward extra bytes. Regime 2 becomes an audit surface with policy versions,
  logged inputs and a still-open delivery-duty problem. Regime 3 introduces an
  infrastructure trust tier and cannot be implemented merely by adding a
  replicon visibility bit.
- A mechanic may not combine hidden membership with an effect that arrives
  before reveal. Existing no-LoS effects therefore force membership for their
  possible targets even if presentation occlusion scores them low.
- Peer manifests and witness announcements become part of every secrecy
  review. Hiding transforms while broadcasting a stable `NodeId`, cells and
  witness role is reported as coarse presence leakage, not as stealth.
- No code changes follow from a Proposed document. Acceptance still leaves
  separate design and measurement work for PVS construction, integer ruleset
  broad phases, reveal events, delivery evidence, cluster replay scheduling,
  metadata policy and topology changes.

## Alternatives considered

- **One visibility oracle for rendering, replication and rules.** Rejected:
  approximate replication queries are safe only because their errors do not
  decide truth, while ruleset queries must be logged and replayable. Sharing a
  data structure may be possible; sharing authority is not.
- **Occlusion always changes rate, never membership.** Retained as regime 1's
  current safe posture and rejected as the universal rule. Once shared LoS is
  an exact effect precondition, conservative membership-drop can satisfy the
  invariant and is the only branch that withholds state from an ESP client.
- **Any symmetric peer may cull membership.** Rejected: symmetry proves a
  policy result only from shared inputs. It neither proves delivery nor keeps
  contested secret inputs secret.
- **Let the authoritative peer hold and withhold its own stealth state.**
  Rejected: the party with the incentive to cheat would control both the
  secret and the evidence of correct withholding, while witnesses would need
  disclosure to validate it.
- **Treat cluster spot replay as a solved stealth backend.** Rejected: D29's
  accepted scope is reversible intents, not continuous hidden state; the
  extension has an explicit workload and trust price and still lacks overload
  and effect-delay semantics.
- **Hide components and ignore metadata.** Rejected: the current manifest and
  witness announcement independently reveal stable membership and cells or
  roles. Geometry cannot repair a protocol-level oracle.
- **Require zero-knowledge proof for all hidden simulation.** Rejected as an
  architecture choice. The research found no demonstrated continuous-state,
  adversarial peer-witness system at Orrery's 20–60 Hz rates. Discrete,
  turn-scale proofs remain future per-mechanic research, not a dependency.

## Open questions reserved to the owner

1. Whether D8's proxy floor is narrowed for shared/secret categories or remains
   universal, which would reject membership hiding inside the AOI.
2. Whether the delivery-duty problem in regime 2 warrants a signed-baseline
   receipt protocol, and how refusal-to-ack avoids becoming invulnerability.
3. Whether secret state is held in persistd, a field host, or a new service;
   this proposal names the authority class and deliberately does not invent a
   sixth D12 service.
4. Which pair of secrecy, peer validation and real-time each initial secret
   category chooses, and whether delayed validation is acceptable gameplay.
5. Which metadata channels remain intentionally public and which accepted
   topology/witness records, if any, are superseded to close them.

[D5]: 0005-spatial-model.md
[D6]: 0006-population-adaptive-topology.md
[D8]: 0008-prediction-rollback-interpolation.md
[D9]: 0009-verifiable-core.md
[D10]: 0010-witnessing.md
[D12]: 0012-backend-services.md
[D28]: 0028-witness-set-seeding.md
[D29]: 0029-low-population-path.md
[D34]: 0034-candidate-accounts-announcement.md
[docs/01 §9]: ../01-spatial-model.md
[docs/03 §3]: ../03-replication.md
[docs/03 §4]: ../03-replication.md
[docs/03 §4.1]: ../03-replication.md
[docs/03 §4–§5]: ../03-replication.md
[docs/03 §5.2]: ../03-replication.md
[docs/03 §5.3a]: ../03-replication.md
[docs/02 §7]: ../02-networking.md
[docs/06 §9]: ../06-verifiable-core.md
[docs/06 §10]: ../06-verifiable-core.md
[docs/07 §4.1]: ../07-witnessing.md
[docs/07 §4.5 and §8]: ../07-witnessing.md
[docs/07 §8]: ../07-witnessing.md
[#351]: https://github.com/baadc0de/orrery/issues/351
[#352]: https://github.com/baadc0de/orrery/issues/352
[#353]: https://github.com/baadc0de/orrery/issues/353
[#354]: https://github.com/baadc0de/orrery/issues/354
