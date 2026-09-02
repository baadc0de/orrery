# Draft amendment — D32 open question 1: authenticating a `ramp/{control}` write

**Propose-only. Nothing is amended.** Amending an Accepted ADR is
owner-reserved ([AGENTS.md](../../AGENTS.md), [DECISIONS.md](../DECISIONS.md));
this document drafts the amendment text and the case for it so the owner can
accept or reject it in one reading. Until the owner accepts §6's diff, the
record is exactly what it was and this file changes nothing.

**No writer was built.** [#863](https://github.com/baadc0de/orrery/issues/863)
and [#875](https://github.com/baadc0de/orrery/issues/875) both require open
question 1 to be answered *in the record* before the writer lands, so the
artifact behind this document is a standalone spike outside the workspace —
nothing in the tree can depend on it, and
[`FdbRampPostureStore`](../../crates/orrery_persistd/src/intent/ramp.rs#L176)
still has `from_context` and `read` and no write.

**Measured:** 2026-09-02 on this checkout, Rust 1.96.0, against a real
single-node FoundationDB 7.3.43 cluster — not a harness file lever, and not an
in-memory fake. Every figure below is the spike's own output.

Debt of record: [D32](../adr/0032-enforcement-ramp.md) open question 1
(`0032-enforcement-ramp.md:777-782`), with open question 3 (`:788-795`) reached
as a consequence rather than by choice — see §5.

---

## 1. The question, and why it is not an implementer's choice

The record states the stake plainly (`0032-enforcement-ramp.md:777-782`):

> **The posture row's writer authentication.** Anything that can write
> `ramp/strikes` can silence enforcement fleet-wide, so the row's trust level
> is "coordinator-key-equivalent" at least. Whether posture writes are direct
> FDB access by an ops tool (D12's operator plane) or a signed envelope
> verified by `persistd` is deployment work this record does not guess at.

Two things follow that the phrasing makes easy to miss.

First, **the choice is a trust-boundary choice, not an ergonomics choice.** The
two candidates the record names do not differ in convenience; they differ in
*what the possession of the FoundationDB cluster file entitles you to*. Under
one, it entitles you to command the enforcement posture of every gateway in the
fleet. Under the other, it does not.

Second, **the row is the only lever, so its authentication is the only gate.**
Clause (c) is normative (`:225-234`): "Each enforcement control has exactly one
runtime lever: a durable operator-set posture row, `ramp/{control}`". There is
no second path to demote a control in a running fleet, so whatever authenticates
this row is the whole of the authorisation story for runtime enforcement
control.

### 1.1 What the tree actually holds today, verified

| Claim | Verified on this checkout |
|---|---|
| `ramp_key` exists | [`keyspace.rs:548`](../../crates/orrery_persistd/src/keyspace.rs#L548), `b"vr" ‖ control` |
| Row value is `postcard(RampPosture)` | [`ramp.rs:134`](../../crates/orrery_persistd/src/intent/ramp.rs#L134), decoded at [`:191`](../../crates/orrery_persistd/src/intent/ramp.rs#L191) |
| Nothing writes a `ramp/` row | `git grep ramp_key` → 3 sites: the definition, a keyspace ordering test, and `FdbRampPostureStore::read` |
| `PostureSource::AutoSuspend` is never constructed | confirmed, zero sites; all three `PostureSource::Operator` constructions are test fixtures ([`persistd.rs:3654`](../../crates/orrery_persistd/src/bin/persistd.rs#L3654), [`:4375`](../../crates/orrery_persistd/src/bin/persistd.rs#L4375), [`gateway.rs:9420`](../../crates/orrery_persistd/src/gateway.rs#L9420)) |
| The gate says so itself | [`ramp-shadow-gate.sh:197`](../../scripts/ramp-shadow-gate.sh#L197): "the durable `ramp/{control}` row it specifies is not in the tree yet" |

So the spike is measuring against a real absence, not a suspected one.

---

## 2. The three candidate mechanisms

The record names two. The spike runs three, because the second splits into two
mechanisms with materially different trust boundaries, and the split is the
whole answer.

| | Where the check runs | Authority to command the fleet | Row is self-describing |
|---|---|---|---|
| **M1** direct FDB write by an ops tool | nowhere | possession of the cluster file | no |
| **M2** signed envelope, verified at **write** time by a privileged writer | in the writer service | possession of the cluster file **or** an operator key | no |
| **M3** signed row, verified at **read** time by every poller | in every `persistd` | an operator key only | yes |

M1 and M2 are the record's two candidates. The observation this spike
contributes is that **M2 does not do what its description implies.** A signed
envelope verified by a writer authenticates *the API*; it leaves the stored row
a plain, unauthenticated byte string, so anyone who can reach FoundationDB
directly bypasses the check entirely and the fleet cannot tell the difference.
M2's guarantee holds exactly as long as nobody goes around it, which is not a
guarantee.

M3 moves the verification to the consumer. The signature travels *in the row*,
and every poller checks it before the mode may take effect. Because the check is
on the read side, a raw FoundationDB write is not a bypass — it is a row that
every gateway refuses.

---

## 3. What the spike demonstrated

21 checks, all passing, against a live cluster. The interesting ones:

### 3.1 M1 — the audit gap is structural, not a missing feature

```
PASS  a cluster-file holder sets ramp/strikes and it reads back
PASS  the row claims source=Operator and names no operator — the audit gap
PASS  the same unauthenticated authority silences C5 fleet-wide
```

`PostureSource::Operator` is a *class*, not an identity
([`ramp.rs:123-130`](../../crates/orrery_persistd/src/intent/ramp.rs#L123)).
Under M1 the incident review can establish that someone silenced enforcement and
cannot establish who. This is not fixable by adding a name field: an
unauthenticated writer can write any name.

### 3.2 M2 — measured bypass, not asserted

```
PASS  a correctly signed envelope is accepted by the writer
PASS  an envelope signed by a non-operator key is refused
PASS  M2's guarantee is bypassed entirely by writing the row directly —
      it authenticates the API, not the row
```

The third line is the finding. The spike verifies the envelope exactly as an
M2 writer would, then writes the plain row directly and observes the mode
change land anyway.

### 3.3 M3 — FoundationDB access stops being fleet authority

```
PASS  a raw cluster-file write is REFUSED by the poller — FDB access is no
      longer authority over fleet enforcement
PASS  a row signed by an unknown key is refused
PASS  flipping the mode after signing is refused
PASS  a row signed for C4 cannot be moved to C5's key — the control is bound
PASS  a correctly signed operator row is admitted
```

The fourth line is why the preimage binds the control name. Without it, a
legitimately signed `ramp/authority_correction = off` row could be copied to
`ramp/strikes` by anyone with write access — a valid signature authorising a
posture nobody authorised.

The preimage, in full, so the amendment is checkable rather than gestural:

```
preimage = blake3(
    "orrery/d32/ramp-posture/v1\0"
  ‖ u32le(len(control)) ‖ control
  ‖ u8(mode)            // Off=0 Shadow=1 Live=2
  ‖ u8(source)          // Default=0 Operator=1 AutoSuspend=2
  ‖ u64le(set_at_ms)
  ‖ u32le(len(reason))  ‖ reason
  ‖ opt(incident_id)    // 0x00 | 0x01 ‖ 16 B
  ‖ opt(expires_at_ms)  // 0x00 | 0x01 ‖ 8 B
)
signature = Ed25519(operator_key, preimage)
```

Domain separation is the leading constant; control binding is the second field.
Both are load-bearing and both are exercised by a failing-if-removed check.

### 3.4 Clause (f)'s asymmetry becomes a predicate instead of a convention

Clause (f) says (`:552-554`) "automation may make the fleet safer without
asking, never less safe". Today that is a property of how `auto_suspend` happens
to be written. Under M3 it becomes a rule the *verifier* enforces, which means
it holds against a buggy or compromised auto-suspend as well as a correct one:

```
PASS  an unsigned AutoSuspend row demoting live->shadow is admitted
PASS  an unsigned AutoSuspend row that would PROMOTE is refused
```

An `AutoSuspend`-source row needs no operator signature — a tripping gateway has
no operator key and must not — but it is admitted **only** if it strictly
lowers the acting rank. A trip can never promote. That is the asymmetry, written
as an admission predicate rather than trusted to a call site.

### 3.5 Clause (c)'s 2 s bound, measured against a real store

Five rounds, operator write → running poller applies, one-second poll interval:

| Round | Decision → effect |
|---|---|
| 0 | 8.6 ms |
| 1 | 998.9 ms |
| 2 | 1000.5 ms |
| 3 | 999.8 ms |
| 4 | 999.9 ms |

Worst observed **1.0005 s**, against clause (c)'s 2 s bound. The distribution is
exactly what the design predicts — uniform over the poll interval, plus an FDB
read — and round 0's 8.6 ms is the case where the write landed just before a
tick. Signature verification does not move the number: it is one Ed25519 verify
per control per second, off the hot path by construction.

This is the first time the 2 s bound has been measured against FoundationDB
rather than against the gate harness's `--posture-file` lever, which is what
#875's acceptance evidence asks for.

### 3.6 A migration hazard the spike found by accident

```
PASS  HAZARD: postcard IS prefix-tolerant — an M3 row decodes cleanly in the
      *landed* reader, which silently ignores the signature and applies the
      mode. A rolling upgrade therefore has un-upgraded processes obeying rows
      they never authenticated.
```

This was written as the opposite assertion and the spike falsified it. Appending
the authenticator fields to `RampPosture` produces bytes that today's
`FdbRampPostureStore::read` decodes **successfully**, dropping the trailing
fields on the floor. During a rolling upgrade that means old gateways apply
signed rows without checking the signature — the mechanism appears to be
deployed while half the fleet is still M1.

Row sizes measured: plain **16 B**, signed **113 B** (+97 B). The size is
irrelevant at five rows; the silent-decode is not.

**Consequence for the amendment:** M3 cannot be introduced by appending fields.
It needs a distinguishing tag the old reader rejects — the simplest being a
schema-version discriminant at the front of the value, which D38
(at-rest schema versioning) already establishes the idiom for. §6's diff says
so normatively rather than leaving it to the implementing lane to discover in
production.

---

## 4. Recommendation

> **Adopt M3: the operator's signature is stored in the `ramp/{control}` row
> and verified by every `persistd` before the posture may take effect.**

The reasoning, in the order it actually decides:

1. **The record already sets the bar and M3 is the only candidate that clears
   it.** "The row's trust level is coordinator-key-equivalent at least"
   (`:778-779`). A coordinator key is verified *by the consumer* — that is what
   makes `--coordinator-key` mean anything at
   `persistd`. M1 and M2 both make the trust level "cluster-file-equivalent",
   which is strictly weaker than the record's own floor, because the cluster
   file is held by every process that stores anything.
2. **It is the only candidate under which a compromised ops tool is not a
   fleet-wide enforcement compromise.** Under M1/M2 the blast radius of the
   operator plane includes silencing every control on every gateway. Under M3 it
   includes nothing an operator key does not also authorise.
3. **It reuses an idiom the tree already has.** `--coordinator-key` is a
   verifying-key set on the CLI, verified at the consumer. `--operator-key` is
   the same shape, same convention, and the same clap surface — which is also
   why it composes with the pending `env`-feature retrofit rather than
   designing against it.
4. **It makes clause (f)'s asymmetry mechanical** (§3.4), which matters more
   than it sounds: auto-suspend is the one writer that is *not* a person, and
   "never less safe" currently rests on that writer being correct.

What M3 costs, priced honestly: one Ed25519 verify per control per poll (five
per second per process, off the hot path); 97 B per row; an operator key set to
distribute and rotate; and a schema-tagged migration (§3.6) rather than a
field append. The key-custody question is real and is **D41's** lane
(offline identity issuer custody and lifecycle), not this record's — this
amendment names the dependency rather than inventing a custody scheme.

### 4.1 What this does not decide

Open question 2 (posture-row retention) is untouched. The spike stores one row
per control and overwrites it; whether superseded rows are retained as incident
history is still `#221`-adjacent tooling, exactly as the record says (`:783-787`).

---

## 5. The de-hardening hazard — #875's sharp case, addressed

#875 states it directly: C2 is already live and unconditional, so
`ramp/quarantine_validation = off` is a **de-hardening** lever, and clause (f)
says automation may make the fleet safer without asking, **never less safe**.
"Do not design a lever that only weakens."

### 5.1 The distinction that makes this tractable

The trap is treating `off` as uniformly "safer". It is not, and the record's own
default table (`:271-277`) says why: **C2 is the only control whose D32 default
is `live`.** So define de-hardening against the record's table rather than
against intuition:

```
rank(Off) = 0,  rank(Shadow) = 1,  rank(Live) = 2

de_hardening(control, mode)  ⟺  rank(mode) < rank(d32_default(control))

d32_default(quarantine_validation) = Live      // C2, already live
d32_default(_)                     = Off       // C1, C3, C4, C5
```

Measured:

```
PASS  C5 off is not de-hardening: D32's own default for C5 is off
PASS  C2 shadow IS de-hardening: D32's default for C2 is live
```

For C1/C4/C5 nothing can go below the default, so *every* write is a promotion
and the "lever that only weakens" problem does not arise. For C2 alone, the
lever points downward from shipped behaviour. One control, one rule.

### 5.2 Two constraints, both demonstrated

**(a) C2's `off` arm is not built.** This reaches open question 3 (`:788-795`)
and answers it in the negative. The record already suspects the arm is
incoherent: demoting C2 treats quarantined sessions as `Good` on the intent
path, while witness eligibility only returns if the *token* standing changes,
which is identity's lever. An arm whose only effect is to weaken one half of a
two-sided property is not a lever, it is a hole. The spike refuses it at the
verifier, so a correctly signed row still cannot select it:

```
PASS  C2's off arm does not exist — a correctly signed row still cannot
      select it (OQ3)
```

The remaining C2 demotion is `live → shadow`, which keeps observing. That is the
same fallback clause (f) already mandates for auto-suspend — "fallback is
shadow, never off" (`:552`) — applied to the operator's lever for consistency.

**(b) A de-hardening write must expire.** This is the part that stops the lever
being a one-way ratchet. A demotion below the D32 default carries a mandatory
`expires_at_ms`; past it, every poller reverts to the CLI startup default
without anyone having to remember:

```
PASS  a de-hardening write with no expiry is refused — it cannot become permanent
PASS  the same write with a one-hour expiry is admitted
PASS  past its expiry the poller refuses it and reverts to the CLI default
```

The failure this prevents is specific and ordinary: an incident demotes C2 at
03:00, the incident is resolved, and the row stays. Nothing alerts, because a
posture row is *supposed* to sit there. The fleet then runs permanently below
its shipped hardening because of a Tuesday. An expiring de-hardening write makes
the demotion as temporary as the incident, and re-extending it is a deliberate,
signed, logged act.

Note the asymmetry is preserved exactly: promotions need no expiry, demotions
below the default do. Automation still may not promote at all (§3.4). So the
lever weakens only temporarily, only with an operator signature, and only as far
as shadow — while it can harden freely and permanently.

---

## 6. The proposed amendment text

Against `docs/adr/0032-enforcement-ramp.md`. Open question 1 is replaced by a
clause; open question 3 is closed.

### 6.1 Replace open question 1 (`:777-782`) with clause (h)

> ### (h) Posture writes are authenticated at the reader, by an operator signature in the row
>
> **A `ramp/{control}` row whose `source` is `Operator` takes effect only if it
> carries an Ed25519 signature by a key in the process's `--operator-key` set,
> over the domain-separated preimage below, and every `persistd` verifies it on
> the poll before applying the mode. Possession of the FoundationDB cluster file
> is therefore not authority over fleet enforcement posture; an unsigned or
> badly-signed row is refused and the control falls to `shadow` rather than
> retaining the unverified mode. A row whose `source` is `AutoSuspend` needs no
> signature and is admitted only if it strictly lowers the acting rank — clause
> (f)'s asymmetry, enforced by the verifier rather than by its writer.**
>
> ```
> preimage = blake3("orrery/d32/ramp-posture/v1\0"
>                 ‖ u32le(len(control)) ‖ control
>                 ‖ u8(mode) ‖ u8(source) ‖ u64le(set_at_ms)
>                 ‖ u32le(len(reason)) ‖ reason
>                 ‖ opt(incident_id) ‖ opt(expires_at_ms))
> ```
>
> The control name is inside the preimage so a signature for one control cannot
> be replayed at another's key. `--operator-key` follows `--coordinator-key`'s
> shape and convention exactly: a verifying-key set on the CLI, checked at the
> consumer. Operator key custody, issuance and rotation are [D41](../adr/0041-offline-identity-issuer-custody-and-lifecycle.md)'s, not this
> record's.
>
> **The row gains a schema discriminant rather than appended fields.** Measured
> 2026-09-02: appending the authenticator to `RampPosture` yields bytes the
> pre-amendment reader decodes *successfully*, silently discarding the
> signature — so a rolling upgrade would leave un-upgraded processes obeying
> unauthenticated rows while appearing to have deployed the mechanism. The
> implementing change therefore tags the value per [D38](../adr/0038-at-rest-schema-versioning.md) so an old reader
> refuses a new row instead of half-reading it.
>
> **A write that leaves a control below its clause (c) default carries a
> mandatory `expires_at_ms`, after which every poller reverts to the startup
> default.** Promotions carry none. This applies to exactly one control today —
> C2, the only one whose default is `live` — and it exists so that an incident
> demotion cannot outlive its incident by inattention.

### 6.2 Close open question 3 (`:788-795`)

> ~~3. **Whether C2's `off` arm should exist at all.**~~ **Closed by clause
> (h): it does not.** C2's only durable demotion is `live → shadow`, which
> keeps observing, matching clause (f)'s "fallback is shadow, never off". The
> arm's only use was to treat quarantined sessions as `Good` on the intent
> path while witness eligibility stayed unchanged — half of a two-sided
> property, which is a hole rather than a lever. The enum narrows; the
> compiler finds every site.

### 6.3 Consequence for #863 / #875 acceptance

Both issues carry "D32 open question 1 is answered in the record before any
writer lands" as unchecked acceptance evidence. Accepting §6.1 discharges it and
unblocks the writer. Nothing else in either issue is unblocked by this document,
and no writer is landed by it.

---

## 7. The artifact, and how to re-run it

The spike lives at [`d32-oq1-spike/`](d32-oq1-spike/) — `main.rs` plus an inert
`Cargo.toml.txt`. Its README says why the manifest is inert: `check.sh
--self-test` refuses a discovered workspace that no lane visits
(`scripts/check.sh:714-718`), and a propose-only spike should neither buy an
exemption from that rule nor hide a directory deeper to evade it.

It needs the FoundationDB client 7.3.x (the headers are a *compile* input —
`foundationdb-gen` does `include_bytes!` on
`/usr/include/foundationdb/fdb.options`) and a running cluster; a single-node
in-memory one is enough. It writes and clears only `ramp/strikes` and
`ramp/quarantine_validation`, and exits non-zero if any check fails.
