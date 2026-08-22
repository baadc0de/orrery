# ADR-0027: The attestation envelope — witness preimage, role separation, and where the required-K draw is made

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D27

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **implements** [D10](0010-witnessing.md) item 4's
attested-intent clause, which no accepted record has ever made concrete, and it
**contradicts two sentences** of [docs/07 §4](../07-witnessing.md) — the
`Attestation { intent_hash, epoch_id, tick, sig }` shape at line 173 and the
`HMAC(epoch_seed, intent_id)` derivation at lines 176 and 180. D10's decision
text stays accepted word for word: K-of-N, a deterministic per-intent required
subset, party exclusion and commit-then-reveal all survive. What changes is
*which key* the per-intent draw uses and *who holds it*. Two rows are proposed
for [D16](0016-parameter-reference.md)'s table.
[D12](0012-backend-services.md)'s service inventory is unchanged, and — as in
[D24](0024-island-drain.md) — that is half the point of the record: **no
coordinator→gateway edge is added.**

Out of scope, owned elsewhere: the announcement envelope, the candidate pool
and the eligibility filter (#143, which also owns the `epoch/{cell_id}` row);
the low-population and provisional paths (#144); enforcement rollout; and any
code (#147, #148, #154).

## Context

### What exists in the tree

`Attestation` is two fields and nothing else:

```rust
pub struct Attestation {
    pub witness: NodeId,
    pub signature: Signature,
}
```

(`crates/orrery_protocol/src/persist.rs:268-275`). The one verification
anywhere in the tree checks that signature against `Intent::signing_preimage()`
— **the identical bytes the issuer signs** (`crates/orrery_persistd/src/intent/mod.rs:332-348`;
preimage at `crates/orrery_protocol/src/persist.rs:316-330`, domain tag
`INTENT_PREIMAGE_TAG = b"orrery/intent/v1"` at `:302`). That preimage is
deliberately attestation-*excluding* so co-signatures can be appended without
invalidating the issuer (`:304-310`), which is correct for the issuer and is
exactly what leaves the witness signature undomained.

The consequence is a role confusion, not a subtlety: **an issuer's own
signature is a byte-valid `Attestation` naming the issuer**, and a signature
solicited for one role verifies in the other. The precheck's own doc says the
threshold is missing (`intent/mod.rs:239-243`: "Attestations are not
*required* (P5 owes the K-of-N threshold), but a present one must be real") and
says why nothing can be checked (`:263-265`: "`cell_epoch` is carried, not
checked: nothing here knows which witness set it names"). The witness crate
agrees from the other side: "Attestation co-signing (docs/07 §4) is P5, not
P4" (`crates/orrery_witness/src/lib.rs:60-63`), and `WitnessSet` is a bare
`Vec<NodeId>` left empty until coordinator seeding exists
(`crates/orrery_witness/src/plugin.rs:290-308`).

### The contradiction that makes docs/07 §4 unimplementable as written

Three statements, each true of the accepted set, and no two of which can hold
together:

1. **The gateway derives the required subset.** "the gateway derives the
   **K required co-signers** — a deterministic per-intent subset of the
   announced set, `HMAC(epoch_seed, intent_id)`" (`docs/07-witnessing.md:180`,
   restated in the §4.2 diagram at `:176`).
2. **The seed is coordinator-held and secret until epoch end.** "the
   coordinator computes `seed = HMAC-SHA256(k_epoch, cell_id ‖ epoch)` from a
   coordinator-held **epoch seed key**" (`:159`); "During the epoch the seed
   stays secret" (`:160`).
3. **The gateway has no connection to the coordinator, by design.** "there is
   no coordinator→gateway connection anywhere in this crate: adding gateways
   does not add coordinator fan-out, and a gateway needs only the coordinator's
   *public* key to check the claim"
   (`crates/orrery_coordinator/src/interest.rs:10-13`), and on the receiving
   side "the gateway needs no connection to the coordinator — the peer is the
   courier, exactly as it is for its identity token"
   (`crates/orrery_persistd/src/gateway.rs:713-719`).

A **secret cannot be couriered by an untrusted peer.** So (1) ∧ (2) ∧ (3) is
unsatisfiable, and P5 cannot begin until one of them gives.

### The premise that is actually wrong

It is (2)'s scope, not (1) or (3). `docs/07` §4 uses one phrase — "the epoch
seed" — for two keys that serve two different mechanisms with two different
adversaries and two different holders:

| Mechanism | What secrecy buys | Who must know the key |
|---|---|---|
| **Set selection** — the Fisher–Yates shuffle that picks N of the candidates (`:159`) | a peer cannot grind its NodeId into the announced set | the **coordinator**, which computes the shuffle |
| **Required-subset draw** — which K of the announced N must have signed (`:180`) | a submitter cannot grind `intent_id` until the required slots land on its colluders | **whoever checks the intent** — and nobody else |

Only the second is a gateway concern, and the gateway does not need the
*coordinator's* key to run it — it needs *a* key that the submitter does not
have. Note what the §4.2 flow already establishes: the submitter "broadcasts
the proposal to the epoch's full announced set (minus parties) … and submits
the lot" (`:180`). **The submitter never learns the required subset and never
needs to.** The draw is a verifier-side filter, evaluated once, after the
attestations are already fixed. A secret that only the verifier consumes has no
business travelling.

`intent_id` is a submitter-chosen `u128`
(`crates/orrery_protocol/src/persist.rs:285-286`), so a *public* draw function
is grindable at a cost of roughly `C(N,K)/C(c,K)` hash evaluations — about 35
tries at N=7, K=3, c=3. That is why some secret is load-bearing. It is not why
it has to be the coordinator's.

## Decision

### (a) What a witness signs

> **A witness signs a distinct, domain-separated, fixed-length 157-byte
> preimage that binds its attestation to one intent, one issuer signature, one
> cell-epoch and one witness identity; a witness never signs
> `Intent::signing_preimage()`, and an ed25519 signature that verifies as an
> issuer signature can never verify as an attestation.**

```
ATTESTATION_PREIMAGE_TAG = b"orrery/attestation/v1"      // 21 bytes
```

serialized in exactly this order, all integers little-endian, no length
prefixes anywhere because every field is fixed-width:

| Offset | Width | Field | Value | Encoding |
|---|---|---|---|---|
| 0 | 21 | tag | `ATTESTATION_PREIMAGE_TAG` | ASCII bytes, verbatim |
| 21 | 32 | `intent_hash` | `blake3(intent.signing_preimage())` | 32-byte blake3 digest |
| 53 | 8 | `cell_epoch` | `intent.cell_epoch.0` | `u64` little-endian |
| 61 | 64 | `issuer_sig` | `intent.signature` | ed25519 `R ‖ S` |
| 125 | 32 | `witness` | `attestation.witness` | NodeId = ed25519 public key |
| | **157** | | | fixed length, always |

```rust
// described, not implemented — #147/#154 own the code and the vectors.
fn attestation_preimage(intent: &Intent, witness: NodeId) -> [u8; 157]
```

`blake3` is chosen over SHA-256 because it is already a workspace dependency
(`crates/orrery_persistd/Cargo.toml:74`) and is already the hash the
`epoch/{cell_id}` row commits with (`docs/08-persistence.md:3236`); this record
adds no new cryptographic dependency to the tree.

Field by field, each earns its place:

- **`intent_hash`, not the intent preimage.** The issuer preimage is
  variable-length (`persist.rs:317`, `ops_len`); hashing it first makes the
  attestation preimage fixed-length, which is what removes every
  length-prefix-ambiguity question an implementer would otherwise have to
  answer. It transitively covers `intent_id`, `issuer`, `cell_epoch` and every
  op.
- **`issuer_sig`.** This is the answer to question (a)'s second half: **yes,
  the attestation commits to the issuer's signature.** An attestation therefore
  cannot be lifted onto a re-signed intent that hashes the same — a different
  issuer key, or a re-signature under a rotated key, produces different bytes
  at offset 61 and the attestation stops verifying. It also makes issuer and
  witness preimages *structurally* non-interchangeable independently of the
  tag: an issuer preimage cannot contain the signature over itself.
- **`cell_epoch`, redundantly.** It is already inside `intent_hash`. It is
  repeated at a constant offset so the attestation's epoch binding does not
  depend on the field list of `orrery/intent/v1` — a future
  `orrery/intent/v2` that reorders or drops fields must not silently unbind
  every attestation ever made. Eight bytes is the right price for that.
- **`witness`.** An attestation is not transferable between witnesses, so a
  co-signature harvested from one set member cannot be re-attributed to
  another, and the gateway's non-party check (`witness ∉ parties`) is checking
  something the signature itself asserts.

### (b) Role separation is enforced three independent ways

> **Issuer and witness signatures are separated by the domain tag, by the
> structural impossibility of an issuer preimage containing its own signature,
> and by the witness's own NodeId appearing in the bytes it signs; any one of
> the three is sufficient, and a verifier must not rely on only one.**

`b"orrery/intent/v1"` and `b"orrery/attestation/v1"` are both fixed constants
at offset 0 and neither is a prefix of the other, so the two signed-byte sets
are disjoint. The gateway additionally rejects `attestation.witness ==
intent.issuer` and any witness in the party set (`docs/07:158`), as §4.2
already requires.

### (c) The wire verdict: `PROTOCOL_VERSION` stays at 1, preimage-only

> **`Attestation` gains no field. `PROTOCOL_VERSION` is not bumped
> (`crates/orrery_protocol/src/protocol.rs:13`). Every binding this record
> specifies rides in the preimage, is recoverable by the verifier from the
> `Intent` the attestation travels inside, and costs zero wire bytes.**

Every field of `docs/07:173`'s proposed `Attestation { intent_hash, epoch_id,
tick, sig }` except `tick` is already derivable from the enclosing `Intent`.
Carrying them would add bytes a verifier must then check for *agreement* with
the intent — a new rejection cause, a new test matrix, and no new capability.

`tick` is the one genuinely new datum, and this record declines it, for three
reasons stated so the next person does not have to re-litigate them:

1. It is load-bearing for no check specified here. Epoch binding is
   `cell_epoch`; intent binding is `intent_hash ‖ issuer_sig`; role binding is
   the tag and `witness`.
2. It would be an **unverifiable self-report**. `Intent` carries no tick
   (`persist.rs:284-297`), so the gateway has no independent tick to compare
   against; the strongest check available would be "inside the epoch's tick
   range", which `cell_epoch` already asserts more directly.
3. A bump would not even buy what it looks like it buys. Version enforcement is
   opt-in today — "the unversioned `GatewayMsg::Hello` is still accepted
   unchecked, so enforcement is opt-in until that variant is removed"
   (`protocol.rs:9-12`).

**What a `PROTOCOL_VERSION − 1` peer's attestation means** — the question the
N/N−1 rolling window (`protocol.rs:3-5`) forces, and it has a clean answer
because the struct did not change: a peer running the old *semantics* emits an
`Attestation` whose signature is over `Intent::signing_preimage()`. Under (a)
that signature simply fails to verify against the attestation preimage. So:

> **A signature made under the old semantics is not an attestation. It is
> counted toward no required slot and it is not, on its own, grounds to reject
> the intent: an intent whose attestations all fail this way is an intent with
> zero valid attestations, which is the low-population/provisional case (#144),
> not a forgery.**

That preserves the rolling-upgrade window without ever counting an undomained
signature, and it means the migration needs no flag day. It does mean an
attestation forged by replaying an issuer signature now fails as
`REASON_BAD_SIGNATURE`-class rather than being silently accepted — the current
behaviour at `intent/mod.rs:340-346` is the bug this closes.

### (d) The required-K draw moves to the verifier and uses the verifier's own key

> **The per-intent required subset is drawn with a 32-byte `draw_key` generated
> by the persistence cluster, held only inside the persistence cluster, and
> never sent to any peer or to the coordinator; the coordinator's `k_epoch`
> stays coordinator-held, seeds only the set-selection shuffle, and is never
> sent to a gateway. No coordinator→gateway connection is created by this
> record, and `crates/orrery_coordinator/src/interest.rs:10-13` remains true
> word for word.**

That is the sentence a reviewer looking for the trust boundary should point at.
Answering the epic's question in one line: **the only process that may hold an
unrevealed draw key is a `persistd` gateway (and the `epoch/{cell_id}` row it
writes it to); the only process that may hold an unrevealed `k_epoch` is
`orrery_coordinator`; neither key ever crosses to the other, and no peer ever
holds either.**

**The derivation.** For an intent `I` in cell-epoch `e` of cell `c`, with `A`
the coordinator-signed announcement for `(c, e)`:

```
selected(A) = [w_1 … w_N]            announced set, in announced order, N ≥ 5
P(I)        = parties(I)             accounts and every NodeId bound to them (docs/07:158)
E(I)        = [w ∈ selected(A) : w ∉ P(I)]      eligible, announced order preserved
d           = draw_key(c, e)         32 bytes, cluster-held, secret until epoch end

if |E(I)| < N_floor (= 5):   no draw is made; §4.5 low-population path (#144) owns it.

r_i         = blake3::keyed_hash(d, DRAW_TAG ‖ intent_id.to_le_bytes() ‖ E(I)[i])
                                     for i in 0 … |E(I)|−1
required(I) = the K = 3 members of E(I) with the smallest r_i, compared as
              big-endian 32-byte integers, ties broken by NodeId bytewise ascending
```

with `DRAW_TAG = b"orrery/attestation-draw/v1"`. `blake3::keyed_hash` is a MAC,
so this is the same construction `docs/07:180` asked for with a different
primitive and a different key holder. K = 3 and N ≥ 5 are D16's, unchanged.

**The admission predicate**, which is the whole of what a gateway must
implement:

```
admit(I) ⟺ verify_issuer(I)
         ∧ |I.attestations| ≤ MAX_ATTESTATIONS                     (intent/mod.rs:152, = 16)
         ∧ no witness repeats                                      (intent/mod.rs:336-339)
         ∧ ∀ a ∈ I.attestations:
               a.witness ∈ E(I)
             ∧ verify(a.witness, attestation_preimage(I, a.witness), a.signature)
         ∧ required(I) ⊆ { a.witness : a ∈ I.attestations }
```

The first three conjuncts are what the tree already does; only the preimage the
fourth verifies against, and the fifth conjunct entirely, are new.

**Why the gateway is the right holder.** Not because it is convenient — because
giving it this key grants it no capability it lacks. The gateway is already the
sole writer of durable truth (D11; `docs/08` §7), already the party that
"verifies signatures + attestations … then executes a FoundationDB
serializable optimistic transaction". A compromised gateway does not need to
bias a draw; it can commit whatever it likes. So the draw secret is placed with
the party whose compromise already ends the game, rather than with parties
whose compromise *is* the threat model. Contrast the alternative the current
text implies — couriering a live secret through the peers it is meant to
defend against — which is not a weaker version of this, it is the opposite of
it.

**Key lifecycle, and the ordering rule that makes the audit non-vacuous:**

```
on first announcement seen for (c, e):
    d      ← 32 bytes from the OS CSPRNG
    commit ← blake3(DRAW_COMMIT_TAG ‖ c ‖ e ‖ d)
    write epoch/{c}.draw_commit = commit          ← must be durable BEFORE any
                                                    intent in (c, e) is admitted
during e:      d lives in gateway memory and in the epoch/{c} row; the cluster's
               trust boundary is the disclosure boundary — no peer holds an FDB
               handle, so "secret" means "not exported", not "not stored"
at epoch end:  publish d into epoch/{c}.draw_key_revealed
```

> **No intent may be admitted under `(c, e)` until that cell-epoch's draw
> commitment is durable.** Without this clause the gateway could choose `d`
> after seeing which attestations arrived, and every retrospective audit of the
> draw would be theatre.

Storing `d` in the row rather than only in memory is deliberate and is what
makes the scheme survive [D26](0026-sibling-gateways.md): a sibling that takes
over the shard mid-epoch **reads** the draw key rather than minting a new one,
so a handover does not silently re-roll every outstanding required subset.

**Collusion arithmetic is preserved exactly.** §4.4's number is unchanged
because the shape of the draw is unchanged — a hidden uniform K-subset of the
eligible set:

```
P(all K required slots land on colluders) = C(c, K) / C(N, K)
                                          = C(3,3) / C(7,3) = 1/35 per attempt
```

and every failed attempt still leaves a refusal record from an honest required
witness. The three cheap moves §4.4 abolishes stay abolished: attestation
shopping (the submitter still cannot choose which K count), `intent_id`
grinding (the draw key is secret during the epoch, so offline grinding has
nothing to grind against), and reseed grinding (untouched — it is #143's).

### (e) When the announcement is unavailable

The seed-unavailability case in the epic's framing **disappears**: under (d)
the gateway generates its own draw key, so there is no seed it can fail to
obtain. What can be missing is the coordinator's *announcement*, which is the
netsplit case `docs/07:233` describes and `docs/09-services-and-ops.md:15`
prices ("No *new* islands, merges, promotions, or witness epochs; running
islands unaffected").

The announcement reaches the gateway **couriered**, exactly as an interest
grant does: the submitter attaches the coordinator-signed
`WitnessSetAnnouncement` (#143 owns its envelope) to its intent, or the gateway
already holds it from an earlier courier, and it is verified against the
coordinator's public keys the way `CoordinatorHandoutAuthority` verifies
handouts (`crates/orrery_persistd/src/gateway.rs:713-724`).

> **A gateway that holds no valid announcement for an intent's cell-epoch
> derives no required subset and admits no attestation toward K. Behaviour, in
> three cases and with no "TBD" among them:**
>
> 1. **Announcement for a *superseded* epoch, within one epoch length of its
>    end** (the `docs/07:233` grace, 30 s): the gateway derives `required(I)`
>    against the last announcement it holds, using that epoch's draw key, and
>    admits normally. `cell_epoch` in the preimage means a stale attestation is
>    still bound to the epoch it was made in; it is accepted late, never
>    re-dated.
> 2. **Announcement stale beyond the grace, or for a different epoch than the
>    intent names:** `E(I)` is undefined, `required(I)` is undefined, and the
>    intent takes the §4.5 provisional path (#144) — commit flagged, finalized
>    by cluster spot replay.
> 3. **Never any announcement for the cell** (a cell that has never had an
>    epoch): identical to case 2. A gateway must not fall back to a self-chosen
>    or NodeId-ordered set. `WitnessSet`'s fallback
>    (`crates/orrery_witness/src/plugin.rs:296-303`) is explicitly justified by
>    "shadow mode is what makes an interim witness set safe; the moment reports
>    carry consequences, this must come from the coordinator" — attestation is
>    where reports start carrying consequences.

In all three cases the failure mode is *provisional commit*, never *refusal*
and never *silent full admission*. That keeps D12's netsplit posture intact:
"P2P sim continues without the cluster (intents queue, durable commits pause);
no cluster = degraded, not dead."

> *Erratum (2026-08-22,
> [ADR-0037](0037-unavailable-witness-epoch.md)):* cases 2 and 3 and “never
> refusal” contradict D29 clause 2, which became Accepted in the same commit as
> this record. D37 proposes that `UnknownEpoch` and `EpochStale` refuse with
> bounded cures; case 1 and “never silent full admission” remain unchanged.
> This annotation is not normative unless D37 is accepted.

### (f) Retroactive verifiability: what must be published, exhaustively

> **After epoch end, a third party can recompute every required subset of a
> cell-epoch from published data alone if and only if all five of the following
> are available; the fifth is new in this record and the audit is vacuous
> without it.**

1. the coordinator-signed announcement for `(c, e)` — candidates, `selected` in
   announced order, tick range, `seed_key_commitment` (`docs/07:161`, #143);
2. `k_epoch`, revealed at epoch end (`docs/07:160`) — needed to check the
   *selection*, not the draw;
3. `draw_commit`, published at epoch start (this record);
4. `draw_key`, published at epoch end (this record) — checked against 3;
5. **per intent, the eligible vector `E(I)` the gateway actually derived over**,
   recorded alongside the committed intent.

Item 5 is the one that is easy to miss and fatal to omit. `E(I)` depends on
party exclusion, which matches "on **accounts and every NodeId bound to them**"
(`docs/07:158`) — bindings that live in `orrery_identity` and *change over
time*. Recomputing `E(I)` a week later from current bindings can silently
produce a different eligible list and therefore a different `required(I)`, and
the auditor would conclude the gateway cheated when it did not. So:

> **The gateway records the eligible vector it derived over, in announced
> order, with the committed intent. An audit reads the recorded `E(I)`; it does
> not reconstruct historical account↔NodeId bindings.**

**Accepted, with the limit stated.** The storage cost is real — a NodeId vector
per committed intent, kept for as long as the intent is auditable — and it was
weighed against what it buys and accepted deliberately.

What it buys is the only audit that can exist today. What it does *not* buy is
worth being exact about, because a later reader will otherwise over-trust it:
the audit proves **"given the eligibility list you recorded, did you draw the
required subset correctly"**, not **"was that eligibility list honest"**. A
gateway that lied about `E(I)` would pass. That is acceptable here only because
the gateway is already the sole writer of durable truth (D11) — its compromise
ends the game by other means, so this adds no attack surface. It does bound the
claim, and the bound belongs in the record rather than in a reviewer's memory.

The upgrade path, if an audit that does not trust the gateway is ever wanted, is
an append-only account↔NodeId binding history in `orrery_identity` so that
`E(I)` becomes reconstructible from first principles. That is a materially
larger system and is not proposed here.

### (g) Parameters

Two rows are proposed for [D16](0016-parameter-reference.md), both promotions
of numbers `docs/07` already states and D16 does not carry:

| Parameter | Default | Source |
|---|---|---|
| Witness epoch length | 30 s | `docs/07:156` (`witness_epoch_secs`) |
| Witness co-sign budget | 150 ms | `docs/07:174`, `:180` |

The stale-epoch grace is deliberately *not* a third row: `docs/07:233` defines
it as "one epoch length", and expressing it as a derived quantity rather than a
tunable is what stops the two from drifting apart. K = 3 of N ≥ 5
(`docs/adr/0016-parameter-reference.md:18`) and the 10 s epoch reseed minimum
(`:23`) are unchanged and are not restated here.

## Consequences

- **`docs/07-witnessing.md` §4 must be reconciled when this record is
  accepted, and is not edited here.** Two sentences become wrong:
  `:173`'s `Attestation { intent_hash, epoch_id, tick, sig }` (the struct gains
  nothing; the binding is preimage-only, and there is no tick), and
  `:176`/`:180`'s `HMAC(epoch_seed, intent_id)` (the draw key is not the epoch
  seed and does not come from the coordinator). `:159`–`:160` stay correct for
  what they actually govern — set selection — once "the epoch seed" is read as
  naming `k_epoch` specifically. Accepted ADRs are normative over expansion
  docs (`AGENTS.md`, "Ground rules"), so until §4 is rewritten this record
  governs.
- **The gateway acquires a secret it did not have, and that is an operational
  cost even though it is not a security regression.** One 32-byte key per live
  cell-epoch, appearing in gateway memory, in core dumps, and in an FDB row —
  a thing to rotate, to exclude from support bundles, and to think about when
  writing a debug endpoint. Nothing in the tree operates a secret at this
  cardinality today.
- **A residual the record does not close: a gateway colluding with a submitter
  that pre-registers `intent_id`s before epoch start can grind `d`.** Over
  intent ids fixed *after* `d` the draw is uniform regardless of `d`, so the
  attack needs the ids in advance and needs the gateway. It is dismissed on the
  same ground as everything else a compromised gateway can do: that gateway can
  simply commit the fraud directly, so the draw is not the weak link.
- **What is lost: public verifiability during the epoch.** As with the
  coordinator's seed, the draw is auditable only *after* the reveal. A VRF
  would give per-intent public verifiability with no reveal delay — `docs/07:160`
  already notes it as future work — and this record keeps that door open by
  changing only the key and its holder, not the shape of the draw.
- **What is deferred: peer-side audit of the draw.** `draw_commit` and
  `draw_key` live in an FDB row no peer can read. A cluster-side auditor
  (D12's telemetry/audit pipeline) can verify everything in (f); a *peer*
  auditing its own epoch cannot, because no peer-visible message carries the
  commitment. Naming the message that carries it is an open question below.
- **`intent/mod.rs:332-348` becomes wrong rather than incomplete.** Today it
  verifies attestations against the issuer preimage, which under (b) is
  precisely the signature confusion this record forbids. #147 replaces the
  preimage; the surrounding `DuplicateAttestation` / `BadAttestation` causes and
  the `MAX_ATTESTATIONS` cap survive unchanged.
- **Nothing is required of the submitter.** It still broadcasts to the full
  announced set minus parties and submits everything that returns inside the
  co-sign budget (`docs/07:180`). No pre-commit round trip to the gateway, no
  extra RTT, no change to the p99 < 10 ms intent-commit budget (D16) beyond the
  K signature verifications the gateway already performs for present
  attestations.
- **A citation in D24 has drifted.** `docs/adr/0024-island-drain.md:27-28` and
  `:117-118` cite the courier sentence at `gateway.rs:619-622`; it is now at
  `:717-719`. The quoted text is verbatim correct and the argument is
  unaffected. Noted, not fixed — D24 is accepted and is not this record's to
  edit.

## Alternatives considered

- **Give the gateway the coordinator's `k_epoch` over a new
  coordinator→gateway connection.** The literal reading of `docs/07` §4.
  Rejected on the grounds D24 gave and on one more that D24 did not need.
  D24's grounds transfer intact: it adds an edge D12's inventory does not have
  (`0012-backend-services.md:9-16`), and it makes a live path depend on
  coordinator availability, contradicting `docs/09:15`'s "running islands
  unaffected" — under this variant a coordinator outage would stop *durable
  commits*, not merely new epochs, which is a strictly worse blast radius than
  the drain case D24 declined. The additional ground is that a control edge is
  a *secret-distribution* channel here, not merely a control channel: the key
  would have to be re-delivered to every gateway on every reseed, at a 10 s
  floor (D16), for every live cell — coordinator fan-out proportional to
  gateways × cells, which is exactly the property `interest.rs:10-12` says the
  courier model exists to avoid.
- **Courier the seed through the peer, like an interest grant.** Rejected as
  incoherent rather than merely bad: an interest grant is *public and signed*,
  and its security rests on the signature. A seed's security rests on secrecy,
  and the courier is the adversary. Handing the epoch seed to a peer hands
  every submitter the ability to compute `required(I)` before choosing
  `intent_id`, which is exactly the grinding attack §4.4 claims is abolished.
- **Derive the subset from something already public — the seed-key commitment,
  the epoch number, the announcement hash.** The most tempting option, because
  it needs no secret anywhere. Rejected on arithmetic: `intent_id` is a
  submitter-chosen `u128` (`persist.rs:285-286`), so the submitter grinds it
  offline until `required(I)` is its three colluders, at an expected
  `C(7,3)/C(3,3) = 35` hashes. Constraining `intent_id` does not help — any
  derivation the submitter can evaluate, the submitter can grind, whatever
  input it is fed.
- **Commit-reveal with retrospective enforcement only** — admit on "any K", and
  after the reveal check that the required K were among them, annulling if not.
  Rejected because it inverts D10's whole posture. The GTA Online lesson
  `docs/07:249` names is "validate before, not after"; this variant restores
  attestation shopping at commit time and buys back only a compensation path.
  Provisional commit already exists for the cases that genuinely cannot be
  decided up front (§4.5, #144), and it is bounded to those cases on purpose.
- **A gateway-issued per-intent challenge nonce**, drawn fresh and handed to
  the submitter before it solicits attestations. Genuinely secure and genuinely
  simpler to audit — the nonce can be public, because it is unpredictable at
  the time the intent is fixed. Rejected on latency and on statefulness: it
  adds a gateway round trip *before* the 150 ms co-sign window, i.e. onto the
  critical path of every critical write, and it makes the gateway hold
  per-outstanding-intent state that must survive a D26 shard handover. The
  draw key buys the same unpredictability with one 32-byte value per
  cell-epoch and zero added round trips.
- **Drop the required-subset idea: require K = N − t attestations.** No draw,
  no secret, nothing to grind — the attacker would need most of the set. It is
  the cleanest cryptographic answer and it loses on availability. Interest sets
  churn violently (`docs/07:232` cites 68% membership turnover per second in
  Donnybrook's regime), and 5-of-7 required means any three unreachable
  witnesses block every durable write in the cell until the next reseed, which
  is floored at 10 s (D16). K=3-of-N with a hidden draw keeps the same
  collusion cost curve at a fraction of the availability cost.
- **Put `tick` on the wire anyway**, matching `docs/07:173`. Rejected under (c):
  the gateway has no independent tick for an intent, so it would be an
  unverifiable self-report, and adding it costs a postcard-positional wire
  break (`persist.rs:269-275`) plus a `PROTOCOL_VERSION` bump for a field no
  check reads. If adjudication later shows it needs the witness's own judged
  tick, it returns as `orrery/attestation/v2` with a bump — this record does
  not pre-authorize that.
- **Sign the intent preimage but with a per-role key.** A witness could hold a
  second keypair used only for attestations, leaving the preimage untouched.
  Rejected: it moves the problem into key management (a second key to
  distribute, bind to an account, and rotate) to avoid a 21-byte constant, and
  it leaves the attestation still unbound to the epoch and to the issuer's
  signature, which is most of what (a) is for.

## Open questions

1. **Which peer-visible message carries `draw_commit`.** Peer-side audit of the
   draw needs it on a channel a peer can read, and no such message is named
   here. Candidates: an epoch advisory on the gateway session, or an echo on
   the intent ack. It is a wire addition either way, so it wants its own
   decision rather than a clause in this one.
2. **`CellEpoch` is a bare `u64` with no cell term** (`persist.rs:88-99`:
   "Wire-identical to `Epoch`: both are a newtype over one u64"), while
   `docs/07:156` specifies `EpochId { cell: CellId, epoch: u32 }`. The draw is
   keyed by `(c, e)`, so a gateway serving more than one cell cannot resolve
   *which* announcement an intent names from the intent alone — it must infer
   `c` from the intent's subject entities, which is `Ruleset`-dependent. This
   record's preimage uses the `u64` as it stands and is unaffected; whether
   `CellEpoch` widens, or #143's announcement carries the binding, is
   unresolved.
3. **Is the draw key per cell-epoch or per shard-epoch?** Per cell-epoch is
   specified above and is the tighter blast radius. Per shard-epoch is one key
   for many cells and much less state to carry across a D26 handover. No
   measurement exists either way.
4. **Does the grace in (e) case 1 also need the *draw* key of the superseded
   epoch to still be unrevealed?** If epoch `e`'s key is published at `e`'s end
   while `e`-attested intents are still arriving inside the 30 s grace, a
   submitter that reads the reveal could grind `intent_id` against the now-public
   key. The obvious fix — delay the reveal by one grace window — trades audit
   latency for it, and is not decided here.
5. **Whether the party set used for `E(I)` is asserted by the announcement or
   computed by the gateway.** (f) requires the gateway to record `E(I)` either
   way; who *derives* it is #143's boundary, not this record's.
