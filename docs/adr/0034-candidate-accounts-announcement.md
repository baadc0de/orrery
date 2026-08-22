# ADR-0034: Candidate accounts travel in the witness-epoch announcement

**Status:** Proposed · **Date:** 2026-08-21 · **Decision:** D34

This record **amends the accepted [D28](0028-witness-set-seeding.md)**. It does
not reopen D28's seeding authority, courier path, draw, or durable epoch-row
decisions. It implements accepted [D31](0031-id-account-subspace.md), resolved
question 3, by changing the signed announcement D28 clause (d) defines.

## Context

D28 announces `candidates: Vec<NodeId>`, but a gateway must resolve each node
to an account before it can exclude intent parties and enforce one slot per
account. D31 supplies the durable `id/` reverse index and fails closed on a
miss. That is safe but makes an empty or filling `id/` subspace demote honest
traffic wholesale to D29's provisional path.

The coordinator already verified every candidate's identity token and retained
its `AccountId` while building the pool. Carrying that resolved view adds no
authority: the same coordinator already signs the pool and chooses the set.
It does make the view signed and epoch-frozen, eliminating the gateway's live
lookup miss from the eligibility decision.

The compatibility premise was checked against the implementation rather than
inherited from D31. Before this amendment, `PROTOCOL_VERSION` is 2 and
`GatewayMsg::protocol_accepted` is exact equality. The caveat recorded on issue
#230 is now discharged: #235 retired the unversioned `GatewayMsg::Hello`, and
the gateway answers it with `HelloRefused` without installing a session.
Consequently every admitted gateway session passes the exact version check.

## Decision

### (a) The signed claims carry a positional account vector

`WitnessEpochClaimsV1` gains this field immediately after `candidates`:

```rust
candidate_accounts: Vec<AccountId>
```

For every well-formed version-3 announcement,
`candidate_accounts.len() == candidates.len()` and
`candidate_accounts[i]` is the account the coordinator's verified session
token bound to `candidates[i]` when the epoch was issued. The vector is inside
the existing `WITNESS_EPOCH_V1_DOMAIN || postcard(claims)` signature preimage.
Changing an account without re-signing is therefore a bad signature, exactly
like changing a candidate.

The vector is parallel rather than a map so there is one canonical order, no
duplicate-key interpretation, and no second NodeId encoding. This decision
allocates no storage key family or prefix byte.

### (b) The protocol version becomes 3, with no compatibility window

Adding a positional field changes postcard decoding. `PROTOCOL_VERSION` is
bumped from 2 to 3. A gateway accepts only an offered version equal to its own;
version 2 is not admitted alongside version 3. `GatewayMsg` and `GatewayReply`
gain no variants, so their pinned postcard discriminants do not move.

The `WitnessEpochClaimsV1` name and `WITNESS_EPOCH_V1_VERSION` remain V1. The
envelope's semantic and cryptographic construction is unchanged; the gateway
session protocol is what prevents an old positional decoder from receiving the
new body.

### (c) The account payload is bounded and measured

D28 already bounds `candidates` at `MAX_EPOCH_CANDIDATES = 32`. Therefore the
unencoded identifier payload is bounded at `32 × 8 = 256` bytes. The protocol
rejects a non-empty account vector that is not parallel to the candidates and
rejects more than 32 accounts.

The wire criterion is measured with signed postcard envelopes and realistic
ten-million-range account ids, not inferred from Rust field widths:

| candidate pool | full announcement | account-vector increment |
|---:|---:|---:|
| 5 | 471 B | 20 B |
| 7 | 607 B | 28 B |
| 8 | 643 B | 32 B |
| 16 | 931 B | 64 B |
| 24 | 1,219 B | 96 B |
| 32 | 1,507 B | 128 B |

The largest supported realistic announcement adds 128 encoded bytes, below
the 256-byte criterion, and the complete 1,507-byte envelope remains below
D28's 2,048-byte cap. Postcard varint-encodes `AccountId`; the 256-byte figure
is the fixed-width identifier budget, while the table is the actual wire
measurement acceptance relies on.

### (d) The announcement is live authority; `id/` is the cross-check

For an intent naming epoch `A`, the gateway derives candidate ownership from
`A.candidate_accounts`. A missing `id/` row does not remove that candidate and
does not demote the intent. This is the property D31 resolved question 3 chose.

If a durable `db || node` row disagrees, the gateway does **not** rewrite the
signed epoch and does not substitute the current durable account into it. The
announcement remains authoritative for that epoch; the durable row is current
identity state and may legitimately reflect a rebind after issuance. The
gateway records the mismatch by node, announced account, durable account,
grid, cell, epoch, and handle for audit. D31 clause (h)'s `dh` history then
answers whether the disagreement was a lawful rebind or whether the
coordinator announced a binding inconsistent with identity history.

A disagreement therefore affects later epochs through the coordinator's next
verified session view and is an audit/security signal for this epoch, not an
opportunity to shop between two eligibility sets. If history proves the
announced binding was already false at issuance, ordinary discrepancy and
standing policy applies to the coordinator/account; this amendment creates no
new strike or quarantine rule.

## Consequences

- D28 clause (g)'s parenthetical remains correct: turnover still judges an
  intent against the announced set for the handle it names. The new account
  vector makes that rule stronger by freezing the binding view alongside the
  NodeId view.
- D31 clause (f)'s live `id/` miss demotion is superseded for a version-3
  announcement carrying accounts. D31 clause (h)'s citation to D27 remains
  correct: `AttestRow.eligible` is still the draw audit's source, while `dh`
  cross-checks the announced account vector rather than replacing it.
- The `id/` subspace remains necessary for current binding lookup, history,
  rebind audit, and detecting coordinator/identity disagreement. This wire
  change removes a hot-path dependency; it does not remove durable identity.

## Alternatives considered

- **Let durable `id/` override the announcement.** Rejected: it restores the
  empty-subspace outage and lets a mid-epoch rebind change the eligibility set
  after attestations were collected.
- **Reject or demote immediately on disagreement.** Rejected: a current row
  can differ because of a legitimate post-issuance rebind. History, not the
  current row alone, establishes whether the coordinator was wrong.
- **Wait for another wire change.** Rejected by D31 resolved question 3. Exact
  version matching is already universal, so waiting does not make the bump
  cheaper and leaves enforcement dependent on a filling index.
