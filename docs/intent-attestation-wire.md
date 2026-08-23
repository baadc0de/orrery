# Orrery intent, attestation, and witness-epoch wire specification

**Status:** interoperability specification for `PROTOCOL_VERSION = 4`

**Normative decisions:** [D27](adr/0027-attestation-envelope.md),
[D28](adr/0028-witness-set-seeding.md), and
[D34](adr/0034-candidate-accounts-announcement.md)

**Test vectors:**
[`wire-vectors/intent-attestation-v1.json`](wire-vectors/intent-attestation-v1.json)

This document specifies enough of Orrery's critical-intent and witness-epoch
surface for an implementation that does not read the Rust source. Byte offsets
are zero-based. Hex strings encode bytes in transmission order, two lowercase
hex digits per byte. Ed25519 public keys are 32-byte compressed Edwards-Y
encodings; signatures are the 64-byte `R || S` encoding.

## Conformance warning: D27 is not implemented in this tree

The accepted D27 attestation layout and the current implementation disagree.
As of this document's publication:

- D27 specifies the 157-byte, `orrery/attestation/v1` preimage in §3 below.
- `orrery_protocol::persist` exports neither `ATTESTATION_PREIMAGE_TAG`,
  `ATTESTATION_PREIMAGE_LEN`, nor an attestation-preimage builder.
- `orrery_persistd::intent::check_attestations` verifies a witness signature
  over `Intent::signing_preimage()`, the issuer's bytes. This is the exact
  role-confusion D27 forbids.
- The shipped validator separately rejects the issuer as its own witness, but
  that identity check does not provide cryptographic domain separation for
  other keys.
- The `CellEpoch` source comment still calls the value "chosen peer-side";
  accepted D28 instead makes it the opaque handle from a coordinator-signed
  announcement. Its `u64` representation is unchanged, so this is a stale
  semantic comment rather than a byte-layout difference. This specification
  follows D28.

Therefore the D27 vector in the committed JSON is a **normative reference
vector generated from D27's field table, not a vector reproduced through a
shipped attestation API**. The JSON also records the current tree's legacy
attestation preimage and signature. A third-party witness should not treat the
legacy scheme as the protocol contract, and cannot interoperate safely with
the current gateway until the D27 verifier lands. D28's announcement,
commitment, reveal, and draw do agree with their implementation and their
vectors exercise the shipped APIs.

## 1. `Intent` on the postcard wire

The postcard-encoded `Intent` struct has these fields in declaration order:

| Order | Field | Type | Meaning |
|---:|---|---|---|
| 0 | `intent_id` | `u128` | Submitter-chosen idempotency key. |
| 1 | `issuer` | 32-byte `NodeId` | Ed25519 key that authorizes the intent. |
| 2 | `cell_epoch` | newtype `u64` | Opaque handle of the signed witness epoch. |
| 3 | `ops` | `Vec<IntentOp>` | Operations in execution order. |
| 4 | `attestations` | `Vec<Attestation>` | Witness/signature pairs. |
| 5 | `signature` | 64-byte Ed25519 signature | Issuer signature specified in §2. |

Each `IntentOp` is, in order, a postcard `u16 op` followed by postcard bytes
`args`. Each `Attestation` is a raw 32-byte `witness` followed by a raw 64-byte
`signature`. Postcard field encoding is specified in §6; it is distinct from
both cryptographic preimages below.

## 2. Issuer signing preimage

The issuer signs a bespoke encoding, **not postcard**:

```text
INTENT_PREIMAGE_TAG = ASCII "orrery/intent/v1"  // 16 bytes
```

| Offset | Width | Field | Encoding and commitment |
|---:|---:|---|---|
| 0 | 16 | domain tag | ASCII bytes verbatim. Separates this signature purpose and version. |
| 16 | 16 | `intent_id` | `u128`, fixed-width little-endian. Commits the idempotency key. |
| 32 | 32 | `issuer` | `NodeId` bytes verbatim. Commits the claimed signing identity. |
| 64 | 8 | `cell_epoch` | Inner `u64`, fixed-width little-endian. Commits the witness-epoch handle. |
| 72 | 4 | operation count | Number of operations as `u32`, fixed-width little-endian. |
| 76 | variable | operations | Repeated in order using the layout below. |

At cursor `p`, each operation is:

| Relative offset | Width | Field | Encoding |
|---:|---:|---|---|
| `p + 0` | 2 | `op` | `u16`, fixed-width little-endian. |
| `p + 2` | 4 | argument length | Byte length as `u32`, fixed-width little-endian. |
| `p + 6` | argument length | `args` | Opaque bytes verbatim. |

Advance `p` by `6 + argument length` for each operation. The preimage ends
after the final argument byte. It includes neither `attestations` nor the
issuer `signature`; witnesses may be appended after the issuer signs without
changing these bytes.

To verify, reconstruct the bytes exactly and run Ed25519 verification using
`intent.issuer` and `intent.signature`. Reject an operation count or argument
length that cannot be represented as `u32`; the Rust builder's casts assume
the protocol's earlier size bounds have already enforced this.

## 3. Witness attestation preimage (D27)

A witness signs this bespoke fixed-width encoding, also **not postcard**:

```text
ATTESTATION_PREIMAGE_TAG = ASCII "orrery/attestation/v1"  // 21 bytes
ATTESTATION_PREIMAGE_LEN = 157
```

| Offset | Width | Field | Encoding and commitment |
|---:|---:|---|---|
| 0 | 21 | domain tag | ASCII bytes verbatim. Separates witness attestations from every other signature purpose. |
| 21 | 32 | `intent_hash` | BLAKE3-256 of the complete §2 issuer preimage. Transitively commits `intent_id`, issuer, epoch handle, operation ids, lengths, and bytes. |
| 53 | 8 | `cell_epoch` | Inner `u64`, fixed-width little-endian. Repeats the epoch binding at a stable offset. |
| 61 | 64 | `issuer_sig` | `intent.signature` as Ed25519 `R || S`. Prevents lifting the attestation onto a differently signed copy of the same intent preimage. |
| 125 | 32 | `witness` | Attesting witness's `NodeId` bytes. Prevents re-attribution to another witness. |
|  | **157** |  | Exactly 157 bytes; no count or length prefix appears. |

Verify the issuer signature first. Then reconstruct this preimage with the
`witness` named by the `Attestation` and verify `attestation.signature` under
that same witness key.

### Why issuer and witness signatures are non-interchangeable

The roles are separated three ways:

1. Their first bytes are different complete domain tags:
   `orrery/intent/v1` versus `orrery/attestation/v1`; neither tag is a prefix
   of the other.
2. The witness message contains the issuer's already-created signature. The
   issuer message cannot contain its own signature without a circular
   definition.
3. The witness message contains the witness public key; changing the claimed
   witness changes the signed bytes.

Ed25519 verification is over the exact message, so a signature valid for one
preimage does not authenticate the other. A verifier must still enforce party
exclusion independently: a party can deliberately make a valid witness-role
signature with its own key, and domain separation alone does not decide
eligibility.

## 4. `WitnessEpochV1` announcement

The coordinator signs the following claims in declaration order and places
the signature after them in a postcard struct:

| Order | Claim | Postcard type | Constraint / meaning |
|---:|---|---|---|
| 0 | `version` | `u8` | Exactly `1`. |
| 1 | `grid` | newtype `u32` | Grid containing the cell. |
| 2 | `cell` | `u64` serializer | Non-zero raw `CellId` bits. |
| 3 | `epoch` | `u32` | Counter monotone per `(grid, cell)`. |
| 4 | `handle` | `u64` | Globally unique handle; `(incarnation << 48) | counter`, with the counter masked to 48 bits. |
| 5 | `epoch_ms` | `u64` | Duration, not a timestamp; `1..=300000`. |
| 6 | `accept_grace_ms` | `u64` | Duration, not a timestamp; `1..=300000`. |
| 7 | `candidates` | `Vec<NodeId>` | `1..=32`, strictly ascending by the 32 public-key bytes. |
| 8 | `selected` | `Vec<NodeId>` | `1..=7`, no repeats, every member in `candidates`; order is draw order. |
| 9 | `seed_commitment` | `[u8; 32]` | Commitment specified in §5. |
| 10 | `prev_seed_key` | `Option<[u8; 32]>` | `None` only for the cell's first epoch; otherwise reveals the preceding epoch key. |
| 11 | `issuer_key_id` | newtype `u32` | Selects a configured coordinator verification key. |

The exact signing message is:

```text
ASCII "orrery/witness-epoch/v1" || postcard(WitnessEpochClaimsV1)
```

`WitnessEpochV1` itself is:

```text
postcard(WitnessEpochClaimsV1) || coordinator_signature[64]
```

There is no outer version, length, field-name, or domain-tag byte in the
postcard envelope. Transport framing must provide the envelope length.

### Offline verification

A recipient needs only the encoded envelope and a configured map from
`IssuerKeyId` to coordinator Ed25519 public key. It does not contact the
coordinator. Apply these checks in order:

1. Reject an envelope longer than 2,048 bytes. Postcard-decode exactly one
   `WitnessEpochV1`, reject trailing bytes, and require claims version 1.
2. Resolve `issuer_key_id`; reject an unknown id.
3. Re-encode the claims canonically, prepend the domain, and verify the
   signature with the resolved key.
4. Enforce the candidate and selected-set constraints in the table above.
5. Require both durations to be in `1..=300000` milliseconds.

Coverage, supersession, and reveal-chain checks require a holder's state and
are outside this purely offline signature-and-shape check.

## 5. Commitment, draw, and chained reveal

All derivation bindings use fixed-width **big-endian** values, unlike the
little-endian intent/attestation fields and unlike postcard varints:

```text
binding = grid:u32_be || cell_bits:u64_be || epoch:u32_be  // 16 bytes

commitment = BLAKE3-256(
    ASCII "orrery/witness-epoch-commit/v1" || binding || seed_key[32]
)

draw_seed = HMAC-SHA256(
    key = seed_key[32],
    message = ASCII "orrery/witness-epoch-seed/v1" || binding
)
```

To reproduce `selected`:

1. Sort candidates by their raw 32-byte public keys ascending and remove
   duplicates.
2. Initialize the `rand_chacha` 0.9 `ChaCha20Rng` with `draw_seed`.
3. Run downward Fisher-Yates for `i = len-1` through `1`. For each step let
   `bound = i + 1`; repeatedly consume `next_u32()` until
   `value < u32::MAX - (u32::MAX mod bound)`, then swap `i` with
   `value mod bound`.
4. Keep the first `min(7, len)` entries in their shuffled order.

The explicit rejection sampler is part of the wire algorithm. Substituting a
language's standard uniform distribution or shuffle is not interoperable,
even if it is unbiased, because its byte consumption may differ.

For epoch `e`, announcement `A_e.seed_commitment` commits `seed_key_e` but
does not expose it. The next announcement for the same `(grid, cell)`,
`A_(e+1)`, carries `prev_seed_key = Some(seed_key_e)`. A holder checks that key
against `A_e.seed_commitment`, then recomputes `draw_seed_e` and the selection
from `A_e.candidates`. Thus a coordinator cannot publish a usable successor
without opening its predecessor. The first announcement has `None` and has no
predecessor to open.

## 6. Postcard 1.1.3 rules that affect this surface

Orrery's current lockfile resolves postcard 1.1.3. Implement these rules
exactly:

- Struct fields are concatenated in declaration order. Field names and struct
  names are absent.
- `u8` is one raw byte. Unsigned integers wider than 8 bits use unsigned
  LEB128: least-significant seven-bit group first, with bit 7 set when another
  byte follows. Newtypes serialize exactly as their inner integer.
- Signed integers wider than 8 bits use ZigZag followed by the unsigned
  varint. No signed integer occurs in `WitnessEpochClaimsV1`, but this matters
  to the wider protocol.
- Sequence and byte-string lengths are unsigned varints. `Vec<NodeId>` is a
  length followed by raw 32-byte keys. A fixed array or tuple has no length
  prefix, so `[u8; 32]`, `NodeId`, and the signature's 64-byte tuple are raw
  bytes.
- `Option<T>` is `00` for `None`, or `01 || postcard(T)` for `Some(T)`.
- An enum variant is keyed by its zero-based **declaration-order index** as an
  unsigned `u32` varint, followed by its fields. Appending a variant preserves
  all existing keys; inserting, deleting, or reordering variants changes keys
  and is wire-breaking. An old decoder still cannot accept an appended variant
  it actually receives.
- Floating-point values, when present elsewhere, are IEEE bits in
  little-endian order. They do not occur on this surface.
- A decoder for a bounded envelope must reject trailing bytes; successful
  prefix decoding is not acceptance.

Do not apply postcard's varints to the §2 or §3 signing preimages. Conversely,
do not encode announcement claims as fixed-width integers: their signature is
over postcard bytes.

## 7. Protocol version 4

`PROTOCOL_VERSION` is `4`. A gateway accepts an offered version exactly when it
equals its own — D29 clause 5 closed the `{V, V−1}` rolling window for all
traffic, so there is no predecessor to accept and no cluster-first deployment
order to observe. Any other value must be refused.

Version 3 added the signed `candidate_accounts` vector to
`WitnessEpochClaimsV1`, parallel to `candidates` (D34). A version-2 decoder
does not know that positional field, so this is a protocol break even though
the surrounding `GatewayMsg::WitnessEpoch` discriminant remains unchanged.

Version 4 appends the signed `on_probation` boolean to
`SessionTokenClaimsV1`, which rides `GatewayMsg::VersionedHello` (D28 clause
(e), D33 clause (d)). Same rule, same reason: a version-3 decoder reads seven
claim fields where there are now eight, and postcard's body carries no names to
skip past, so the eighth byte displaces the signature rather than trailing it.

The *token* carries its own version byte as its first claim field, and a
verifier accepts both `1` (the pre-probation body) and `2` (the current one).
That is a different axis from this one and not a second `{V, V−1}` window: it
exists because identity and the gateways are separate services with no
handshake between them, so a fleet rollout has an interval in which one mints
the old shape and the other reads the new one. A claims-version-1 token
authenticates its session normally and is read as `on_probation = true` —
unknown age, therefore not witness-eligible. Outside that fleet-internal
window, a client offering protocol version 3 is refused at the handshake and
never presents a token at all.

The check binds every session, because `GatewayMsg::VersionedHello` is the only
bootstrap a gateway admits. The older unversioned `GatewayMsg::Hello` is retired
and refused with `GatewayReply::HelloRefused` rather than dropped, so version
enforcement is universal rather than opt-in. The version does **not**
mean that D27 changed the postcard `Attestation` shape: D27 intentionally adds
no fields and specifies only different signed bytes. As the conformance warning
states, those signed bytes have not landed in the inspected implementation.

## 8. Test vectors and regeneration

The committed JSON contains full keys, preimages, signatures, claims, and
postcard envelopes. These are test-only keys and must never be used in a
deployment.

| Vector | Fixed secret | Principal check values |
|---|---|---|
| Intent | `11` repeated 32 bytes | Public key `d04ab232…78737`; 99-byte issuer preimage; signature `87bfa2e9…9ebd03`. |
| D27 attestation reference | `22` repeated 32 bytes | Public key `a09aa5f4…55a4f0`; 157-byte preimage; intent hash `2c0f06cd…18d6d2`; signature `343225c7…15b70b`. |
| Witness epoch signer | `33` repeated 32 bytes | Public key `17cb79fb…8080ce`; issuer key id 42. |
| Epoch 0 draw | seed key `44` repeated 32 bytes | commitment `268bf993…6f2b0`; selected set begins `af06a3e3`, `d62f016a`, `2df04125`; epoch 1 reveals this key. |
| Epoch 1 draw | seed key `55` repeated 32 bytes | commitment `51753ad4…b6248f`; selected set begins `4a72e403`, `d62f016a`, `12a41592`. |

Ellipses in this human table are display-only; the JSON values are complete.
The candidate test keys, both epoch envelopes, and every secret input are also
published there.

Regenerate from the repository root:

```sh
docs/wire-vectors/generate.sh
```

Pass another output path to compare without replacing the committed vector:

```sh
docs/wire-vectors/generate.sh /tmp/orrery-wire-vectors.json
cmp docs/wire-vectors/intent-attestation-v1.json /tmp/orrery-wire-vectors.json
```

The launcher creates a temporary Cargo package and points it at this
worktree's `crates/orrery_protocol`; it does not modify that crate or the root
workspace. The generator:

- calls `Intent::signing_preimage`, `Intent::sign`, and
  `Intent::verify_issuer`;
- calls the shipped witness-epoch binding, commitment, seed, draw, signing,
  encoding, offline verification, reveal verification, and draw-audit APIs;
- asserts that the published announcement preimage produces the same signature
  as `WitnessEpochV1::sign`;
- locally implements only D27's missing 157-byte field table, labels the result
  as normative-only, and proves its signature does not verify over the issuer
  preimage; and
- emits the legacy witness-over-issuer-preimage signature to make the current
  implementation disagreement independently observable.

Regeneration must be byte-identical when the wire contract has not changed.
Do not update the JSON merely to make a changed implementation pass. An
intentional preimage or announcement change requires a new domain/version and
an explicit protocol-compatibility decision before regenerating its vectors.
