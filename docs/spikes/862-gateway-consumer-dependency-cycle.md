# #862 box 2 item 3 — persistd's gateway consumer, and the dependency cycle

**Status: spike, now settled. Proposes, decides nothing.** The working artifact
is [`862-gateway-consumer/`](862-gateway-consumer/); it runs green against a
real FoundationDB cluster and its central assertion has been mutation-checked.

> **Outcome, 2026-09-03.** The owner took **candidate B**. It shipped as
> `orrery_persistd::standing_feed::DcCooldownFeed`, with the key builder moved
> to `orrery_persistd::keyspace::cooldown_entry_key` (its five `d`-family
> siblings' module) and identity calling it from there — one definition of
> those bytes, which is what makes B safe. The `persistd` binary installs the
> feed whenever it has a cluster, and the enforcement the spike deliberately
> left unwired is wired: C5's posture is read at the Hello arm and at the top of
> each sweep *and* again per session, so an auto-suspend demotes mid-sweep.
> Candidate D remains the named successor; taking it deletes `DcCooldownFeed`
> and changes nothing else.

Written 2026-09-03, against `main` at `8c42868` (#958, which landed less than an
hour before this spike started and moved this territory).

---

## 1. The constraint, established

### What the gateway would need to consume

`orrery_persistd`'s gateway already has every part of a standing consumer
except the value that turns it on:

| Part | Where | State |
|---|---|---|
| the seam | `gateway.rs:491` `trait StandingInvalidationFeed` | exists |
| the handle | `gateway.rs:516` `SharedStandingInvalidationFeed` | exists |
| the config slot | `gateway.rs:3394` `GatewayConfig::standing_feed` | exists, `None` at `:3500` |
| Hello refusal | `gateway.rs:4651-4665` | exists |
| the sweep | `gateway.rs:6242`, `:6416` | exists |
| **any non-test assignment** | — | **absent** |

The only assignment in the tree is `tests/gateway_standing.rs:160`, and it
supplies a local `MutableFeed` test double. `bin/persistd.rs:2743` builds a
`GatewayConfig` with no such field.

The data it needs is D33 clause (e)'s `AccountInvalidation`
(`orrery_protocol/src/identity.rs:105`) — `{ account, effective_from_ms }`.
The producer is `orrery_identity::StandingInvalidationSource::current`
(`invalidation.rs:92`), which is a map over `AccountStore::cooldown_entries`
(`store.rs:320`), which over FDB is a range scan of the `dc` family
(`identity/src/fdb.rs:637-670`).

### What the crate layout forbids — and it is forbidden, not merely unwired

`orrery_identity` depends on `orrery_persistd`
(`crates/orrery_identity/Cargo.toml:30`, for `keyspace` and
`gateway::BindingAuthority`). persistd does not depend on identity. Adding the
edge is a hard cargo failure, not a lint:

```
error: cyclic package dependency: package `orrery_identity v0.1.0` depends on itself.
```

**Two refinements matter, and both were checked rather than assumed:**

1. **`optional = true` does not dodge it.** Cargo resolves the package graph
   before features, so the identical error appears. This is why the pattern the
   coordinator uses — `standing-feed = ["dep:orrery_identity", …]`
   (`coordinator/Cargo.toml:36`) — is structurally unavailable here. The
   coordinator sits *downstream* of identity; persistd sits *upstream*.
2. **The blocker is narrower than "persistd cannot see identity".** The seam at
   `gateway.rs:491` is a trait local to persistd, and `orrery_identity` could
   legally write `impl orrery_persistd::gateway::StandingInvalidationFeed for …`
   today: the edge already runs that way and the orphan rule is satisfied. What
   is impossible is not the *impl*, it is the *composition root*. persistd's
   root is `[[bin]] persistd`, a target **inside** `crates/orrery_persistd`
   (`Cargo.toml:82-85`), and a bin target sees only its own crate's
   dependencies. The coordinator's root, `orrery-coordinator.rs`, lives in a
   crate that is allowed to name identity. **That asymmetry is the entire
   problem.**

So: "forbidden by the dependency graph" is the composition root's access to
`orrery_identity`'s *types*. Everything else is merely not wired.

---

## 2. The candidates

### A. A third crate holding the shared standing contract

`orrery_standing`, depended on by both. Moves the `AccountInvalidation` feed
trait and the `dc` row types below both crates.

**Concrete form:** new workspace member; `gateway.rs:469-516` (`FeedFailure`,
the trait, the alias) and the `dc` row type move into it; persistd and identity
both depend on it; identity implements the shared trait; the impl is injected
at… persistd's bin, which still cannot name identity.

**Discarded.** It does not solve the problem. A shared *trait* was never the
obstacle — persistd already owns a perfectly good local one, and identity can
already implement it. The obstacle is the composition root, which a fourth
crate does not move. It also contradicts a decision already recorded twice:
both `server.rs:175-178` and `standing_feed.rs:10-16` state that the two
consumers' copies of this trait are *deliberately* separate so neither's
polling contract couples to the other's.

### B. Read the durable `dc` family directly — no new edge at all

The gateway range-scans `dc ‖ account:u64-be -> entered_at_ms:u64-be` through
the FDB handle persistd already owns, exactly as #958's reactor drains the `yd`
queue.

**Concrete form:** move `cooldown_entry_key` and the two range bounds from
`orrery_identity/src/fdb.rs:279-297` into `orrery_persistd::keyspace`; identity
calls persistd's builder instead of its own; a `DcCooldownFeed` in persistd
implements the existing trait; `bin/persistd.rs` constructs it behind
`--fdb-cluster-file` and a posture read. No crate gains a dependency. No edge
is added, inverted, or moved.

**The finding that makes this the natural shape:** every *other* `d`-family key
builder already lives in `orrery_persistd::keyspace` —
`account_range_start` (`da`, `keyspace.rs:1409`), `binding_range_start`/`_end`
(`db`, `:1421`/`:1429`), `binding_history_range_start`/`_end` (`dh`,
`:1437`/`:1450`). And `keyspace.rs:1429` already names `dc` in prose: "`b"dc"`
names nothing: `c` is the gap the discriminators `a < b < h` deliberately
leave." Identity's `cooldown_entry_key` is the **only** `d`-family key builder
in the tree outside that module, and it is private.

So this candidate does not introduce a second copy of the bytes — the thing
`orrery_identity/Cargo.toml:29` correctly says "D31 clause (b) cannot survive".
It *removes* the one that is already anomalous, by moving it to the module that
owns its five siblings.

D31's sole-**writer** rule is untouched: this is a read. The coordinator
already ships that posture and says so at `standing_feed.rs:20-24` — "a *read*
of a family this process never writes, which keeps D31's sole-writer rule
intact."

**Built and proved.** See §3.

### C. Invert the edge — persistd depends on identity

**Discarded, with the compiler as the reason.** It is not a trade to price; it
is the same cycle read backwards. `orrery_identity` needs persistd's
`keyspace` and `gateway::BindingAuthority`; removing that need means moving the
whole `d`-family keyspace and the binding-authority seam into identity or a
fourth crate, which is candidate A plus a much larger move. Against D15's spine
this also inverts rule 2's direction of travel for no gain the other candidates
do not already give.

### D. A seam in persistd that identity implements, injected at the composition root

The `PredictedBy` (#933) / hit-claim-publisher (#938) pattern: put the crossing
at the composition root rather than inverting a spine edge.

**Concrete form:** the seam already exists (`gateway.rs:491`) and identity can
already implement it. What must change is *where persistd's composition root
lives*: move `[[bin]] persistd` out of `crates/orrery_persistd` into a new
`crates/orrery_persistd_bin` (or `orrery_gatewayd`) that depends on both
persistd and identity. That crate is downstream of both, so the cycle vanishes
and the coordinator's exact pattern becomes available.

**This is the architecturally cleanest candidate and it is the expensive one.**
It is the honest general fix: it makes persistd's root behave like the
coordinator's, and it would serve every future crossing, not just this one.
Its price is a new workspace member carrying three `[[bin]]` targets
(`persistd`, `world-census`, `orrery-ramp`) and their `required-features`
matrices, plus every script, gate and workflow that names a persistd binary
path. `scripts/check.sh`'s lane table, `gate-status.sh`, `p3-siblings-gate.sh`
and `nightly.yml` all reference these binaries.

**Not discarded — deferred.** Nothing in candidate B forecloses it; see §4.

---

## 3. What was built, and what it proves

[`862-gateway-consumer/main.rs`](862-gateway-consumer/) implements candidate B's
feed — `impl orrery_persistd::gateway::StandingInvalidationFeed for
DcCooldownFeed` — importing nothing from `orrery_identity`, and has identity's
real `FdbAccountStore::observe_cooldown` write the row it reads.

Against the dev cluster:

```
PASS: a dc row written by orrery_identity was read by a persistd-only feed
      [AccountInvalidation { account: AccountId(604045312905969665), effective_from_ms: UnixMillis(1756000000000) }]
```

Mutation (range pointed at `db` instead of `dc`) fails it with `left: []`.

**What that establishes:**

- The row format is trivially readable from persistd: a ten-byte key and an
  eight-byte big-endian value, no postcard framing
  (`identity/src/fdb.rs:299-316`, `:590-593`).
- The feed body needs **zero** identity types. `AccountInvalidation`,
  `AccountId` and `UnixMillis` are all `orrery_protocol`, which persistd
  already depends on.
- The gateway's existing trait is satisfiable as written — no signature change,
  no `GatewayConfig` change, no new seam.

**What it deliberately does not establish, and must not be read as:** the spike
installs no feed and enforces nothing. It never touches `StrikesPosture`. A
gateway that consumes standing becomes an enforcement point, and #934 fixed a
real bug — `--strikes shadow` cutting live sessions — caused by an enforcement
arm that read no ramp posture. Promoting this spike means adding the posture
read that is deliberately absent from it, at both `gateway.rs:4651` (Hello) and
`gateway.rs:6416` (sweep).

---

## 4. Recommendation

**Take candidate B. Keep candidate D as the named successor.**

### What it adds

Three key-builder functions moved (not copied) into
`orrery_persistd::keyspace`, one `DcCooldownFeed` type in persistd, and one
construction in `bin/persistd.rs` behind `--fdb-cluster-file`, the `fdb`
feature, and a C5 posture read. No crate gains a dependency; the 13-workspace
layout is untouched; the root workspace gains no member. Build times are
unaffected — persistd already links `foundationdb` behind `fdb`, and identity
gets marginally *smaller*.

It also pays down a real inconsistency: the `d` family's key builders stop
being split across two crates.

### What it forecloses — priced honestly

**It hard-codes an in-process durable read as the gateway's transport for
standing.** That is a genuine cost and it is worth naming precisely, because it
is deeper than the coordinator's version of the same thing: the coordinator
reads through identity's typed `AccountStore`, so when identity's service half
lands, only `IdentityStandingFeed`'s body changes. A gateway reading raw
keyspace is coupled to the *row layout*, not to a typed API — so a `dc` layout
change would have to update two crates rather than one.

The mitigation is the key-builder move itself: after it, there is exactly one
definition of those bytes, in persistd, and identity calls it. A layout change
edits one function. Without the move this candidate would be genuinely unsafe,
and that is why the move is not optional decoration on it.

**It does not foreclose candidate D.** If the composition root is later moved
out of `crates/orrery_persistd`, `DcCooldownFeed` is deleted and replaced by
identity's typed adapter, and nothing else changes — the trait, the config slot,
the Hello arm and the sweep are all already correct and untouched by either
choice. That is the reason to prefer B now: it is the cheap move that the
expensive move does not have to undo.

### Cost of the alternative, for comparison

Candidate D is better architecture and roughly ten times the change. It is the
right thing to do when a *second* crossing needs persistd's root to see a
downstream crate — at which point it is amortised across two problems instead of
carrying this one alone.

---

## 5. Left to the owner, deliberately

- **The choice itself.** B and D are both defensible; B is recommended, not
  decided. D is not a fallback, it is a deferral.
- **The appeal/dwell interaction (D33 policy).** Untouched. Nothing here decides
  whether exoneration ends dwell early.
- **Whether the gateway should enforce standing at all**, and at what C5
  posture it should ship. This spike stops one step short of that on purpose.
- **Publication latency.** #958 made publication filing-driven for identity's
  reactor; a `dc`-reading gateway inherits whatever latency that path has, and
  whether one poll interval is acceptable at the gateway is a D33 question, not
  a wiring one.
