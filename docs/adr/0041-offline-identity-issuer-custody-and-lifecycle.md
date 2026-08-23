# ADR-0041: Offline invites are single-use capabilities, account allocation is singular, and issuer keys are portable secrets

**Status:** Proposed · **Date:** 2026-08-23 · **Decision:** D41

This record is non-normative until accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete accepted decision set. Acceptance is
reserved to the owner.

**Supersedes:** nothing. It proposes the custody and lifecycle policy that
[D31] explicitly excludes and that [D12]'s identity service now needs before an
invite-redemption endpoint is deployed. It relies on [D31]'s account store,
[D33]'s standing checks and invalidation posture, and the existing one-hour
session-token bound. It does not amend any accepted record while Proposed. The
accepted-record and expansion-document tensions that acceptance would require
the owner to resolve are named in clause (f).

Out of scope: passwords, email, OAuth, Steam, payment or the price of an
account; client packaging and download access; consent records; implementing
an HTTP listener, operator UI, HSM signer, secret store, FoundationDB schema,
rate limiter or client credential vault; changing the session-token wire
format; changing D33 standing; operating the TLS certificate; and deploying
anything to `orrery-hel1-1.distopik.com`. This record governs the invite and
issuer capabilities behind an eventual endpoint; it does not create that
endpoint or accept itself.

## Context

### 1. The landed implementation has two different secrets and no endpoint

PR [#357] landed an offline mint command and a redemption library, not an
offline token issuer. `orrery-invite` loads a caller-selected local TSV,
generates 32 random bytes, appends the resulting allocation, saves the whole
file, and prints `account=` and `invite_code=` to standard output. Its module
documentation states that the binary has “no network or issuer key input”
(`crates/orrery_identity/src/bin/orrery-invite.rs:1,18-24`). The mint command
therefore handles an **invite capability**, never the Ed25519 **issuer signing
key**.

The clear code has the exact shape

```text
"orrery-invite-v1-" || hex(random[32])
```

and the ledger stores

```text
SHA-256("orrery/invite-code/v1\0" || clear_code)
|| AccountId:u64
|| volunteer_label:plaintext
```

These are the current prefix, header and hash preimage, not the prose from the
PR (`crates/orrery_identity/src/invite.rs:19-21,88-97,316-328`). The hash is
domain-separated and compared without a data-dependent early exit.
The 64 hexadecimal characters encode 256 random bits; the known prefix adds no
entropy. For `m` independently generated codes, the birthday bound is

```text
P(any duplicate) <= m(m - 1) / 2^257
m = 1,000,000     => P <= 4.32e-66
```

and `q` guesses against `m` live codes have union-bound success at most
`q*m/2^256`. This makes the hash suitable as a lookup capability under the
generator's assumption; it does not make the volunteer label secret and it
does not make an online endpoint exempt from request-rate limits.

`redeem_invite` hashes the presented code, finds an account, creates that
account only if it is absent, binds the presented `NodeId`, and calls the
existing `IdentityService::issue`
(`crates/orrery_identity/src/invite.rs:224-248`). There is no network listener
in [#357], no production key provisioning, no invite expiry, no
consumed/revoked bit, no operator identity, and no ledger mutation during
redemption. A successful code therefore remains valid in the input ledger.

This contradicts two operational claims in [#345]. Its proposed mint CLI was
said to hold the issuer signing key; the landed CLI explicitly does not. Its
long-TTL campaign-token option treated the cap as configurable; the landed
issuer refuses more than `3,600,000 ms` and the verifier independently rejects
it. The same issue's “silent re-redemption” branch does describe current
behavior: because redemption records no consumption, anyone retaining the code
may present it again.

Two more [#345] statements are now stale rather than policy inputs:
`orrery_identity` exists, and `AccountId` is a `u64` newtype rather than a bare
alias. Its proposed HTTP endpoint and client-side code vault remain absent.
[#357] identifies the same citation drift and accurately limits itself to the
CLI/library half; the current files do not contradict its implementation
summary.

### 2. Hash-only storage makes loss and support asymmetric

After the command returns, the implementation has only two cleartext copies it
can know about: the in-process `String` and the line written to stdout. The
saved ledger cannot reconstruct the code from its hash. Consequently:

```text
lost clear code + intact ledger       => no recovery of that code
supplied clear code + intact ledger   => verify and identify its account
ledger alone                          => label/account inventory, no login
clear code alone + live ledger row    => account-binding capability today
```

“Print once” is not “leave no copies.” Terminal scrollback, shell-session
capture, CI logs, screen sharing, clipboard history and an operator's message
archive can all become code custodians. Conversely, deleting every cleartext
copy is irreversible by design. Support can verify a code the volunteer still
has, or revoke/replace an allocation once lifecycle state exists; it cannot
read the current file and resend the original code.

The flat file has two further lifecycle properties not named in [#345].
`InviteLedger::save` rewrites the path with `fs::write`; it does not perform an
atomic replace, fsync protocol or backup
(`crates/orrery_identity/src/invite.rs:82-99`). The CLI also takes no file lock
around its load/mint/save sequence
(`crates/orrery_identity/src/bin/orrery-invite.rs:18-24`). Two processes that
load the same maximum account `n` can each allocate `n+1`, print two distinct
codes, and race whole-file saves; the last save can erase the other row,
leaving one printed code permanently invalid. These are reasons to treat the
file as a mint artifact, not as the live endpoint's authoritative database.

### 3. Sequential allocation is locally deterministic and globally colliding

`InviteLedger::next_account` returns `1` for an empty ledger and otherwise
`max(account) + 1`. Parsing rejects duplicate code hashes but permits duplicate
account ids (`crates/orrery_identity/src/invite.rs:107-169`). There is no
operator namespace, allocation lease or shared counter.

For two independent operators whose ledgers both begin empty and who each mint
`k` codes:

```text
operator A accounts = {1, 2, ..., k}
operator B accounts = {1, 2, ..., k}
account collisions  = k                    probability = 1
code collisions     <= k(2k - 1) / 2^257  approximately zero
```

The account collision is more dangerous than a clean `AccountExists` refusal.
Redemption first reads the account; if it already exists, it skips
`create_account` and proceeds to bind the new node to that existing account.
Thus the second operator's code can merge two volunteers into one durable
account (`crates/orrery_identity/src/invite.rs:237-245`). Up to D31's accepted
eight-node binding cap, the merged parties share the account's standing,
probation age, binding-rate budget and every account-keyed asset or ledger
fact. At the cap, later redemption refuses after the collision has already
consumed account and support state.

Replacing the sequence with uniform random `u64` ids would change deterministic
collision into birthday risk, not create authority:

```text
P(collision among n random u64 ids) <= n(n - 1) / 2^65
n = 10,000,000                      => P <= 2.71e-6
```

That may be acceptable for correlation identifiers; it is not a substitute
for an authoritative allocator whose collision merges security principals.

### 4. Redeemed, revoked and expired do not exist in V1

`account_for_code` has two results: matching account or no match. Redemption
does not remove the row, append a tombstone or compare an expiry. The same code
may therefore bind multiple NodeIds and mint multiple sessions. Deleting its
line manually makes later presentations look like every never-issued code; it
also destroys the label/account audit fact and can be undone accidentally by
restoring an old live backup (`crates/orrery_identity/src/invite.rs:147-155,224-248`).

The create, bind and issue steps are not one transaction. A failure can leave
an account created but unbound, or a node bound but no token returned. Retrying
works in some of those states because account creation is conditional and an
already-held binding is idempotent, but no atomic consumed transition proves
that exactly one node won the code. Two concurrent redeemers can race for the
same capability; the store serializes account and binding rows, not invite use.

Revoking an invite and revoking a session are different acts:

```text
revoke unredeemed code   => refuse future first redemption
consume redeemed code   => refuse code replay; current token remains valid
unbind NodeId            => refresh later refuses; current token may remain
retire issuer key        => every token under that key becomes UnknownIssuer
```

No current invite operation performs any of those transitions.

### 5. Issuer-key compromise crosses every account boundary

`IssuerSigningKey` holds an Ed25519 secret in process memory. `IssuerKeyring`
publishes every held public key, selects one active key for signing, and
supports add, activate and retire
(`crates/orrery_identity/src/issuer.rs:29-63,126-217`). The verifier selects
solely by `IssuerKeyId`, checks the signature, connected `NodeId`, maximum TTL
and time; it does not read the account store or recompute standing
(`crates/orrery_protocol/src/identity.rs:382-419`). A stolen active or
still-published secret can therefore sign chosen account, node, standing and
probation claims across the token field space. It is not limited to codes the
attacker stole or accounts the issuer previously created.

For one compromised trusted key and `V` verifiers carrying its public half:

```text
affected verifiers                    = V
account ids the signature can name    = 2^64
maximum forged-token lifetime         = 3,600,000 ms
per-account containment from the key  = 0
```

Stopping the process does not revoke signatures. If the public key remains
trusted, a token minted immediately before containment can verify for one
hour. Removing the public key fleet-wide stops forged and legitimate tokens
under it together. The ordinary three-step rotation intentionally dual-accepts
old and new keys for at least one token lifetime; that is correct for planned
rotation and wrong for a known-compromised outgoing key.

The TLS private key has a different blast radius: it authenticates the HTTPS
endpoint, while the issuer key authenticates session claims to every gateway
and coordinator. They must not be the same key, share a file, or acquire the
same access policy by convenience.

### 6. The first intended host is already a migration

The owner-approved initial endpoint host is
`orrery-hel1-1.distopik.com`, scheduled for decommissioning in September 2026.
A live TLS probe on 2026-08-23 returned a Let's Encrypt certificate valid from
2026-08-13 through **2026-11-11 08:38:34 UTC**. The certificate therefore
outlives the machine by at least the interval from its September teardown to
11 November; certificate validity is not evidence that the host, its disk or
its token-signing key remains available.

```text
openssl s_client -connect orrery-hel1-1.distopik.com:443 \
  -servername orrery-hel1-1.distopik.com </dev/null 2>/dev/null \
  | openssl x509 -noout -issuer -dates
issuer=C=US, O=Let's Encrypt, CN=YE1
notBefore=Aug 13 08:38:35 2026 GMT
notAfter=Nov 11 08:38:34 2026 GMT
```

An issuer key generated only on that host creates two bad migration choices:
copy an unplanned raw secret during teardown, or rotate under deadline and
keep the old public key accepted while its only recoverable secret may be on a
machine being destroyed. Key portability, recovery escrow and successor-host
rehearsal are deployment prerequisites, not post-decommission cleanup. DNS and
TLS can move to a successor while the Ed25519 issuer identity stays stable;
conflating those lifecycles turns a host replacement into a fleet-wide login
rotation.

### 7. Session freshness has a countable steady-state price

The landed default and hard maximum TTL are both one hour. `IssuedSession`
sets refresh to `issued_at + ttl/2`, and `refresh` repeats the normal issuance
checks: TTL, account existence, current binding, standing, then signature.
These are the current service paths
(`crates/orrery_identity/src/service.rs:175-188,296-417`), while the verifier's
independent cap is `3,600,000 ms`
(`crates/orrery_protocol/src/identity.rs:39-42,409-417`). At steady concurrent
population `C` and TTL `T` seconds, half-TTL refresh costs

```text
refreshes/hour = 7200 C / T
refresh QPS    = 2 C / T

T = 3600 s, C = 10,000  => 20,000 refreshes/hour = 5.56/s
T =  900 s, C = 10,000  => 80,000 refreshes/hour = 22.22/s
```

Each refresh performs at least the account read, binding read, standing lookup
and Ed25519 signature in the current service path. Shortening TTL reduces the
maximum stale-token and planned-rotation window linearly and increases this
load inversely. Re-presenting an invite is not a cheaper refresh: it adds a
capability lookup and binding path and, under V1, keeps a bearer account-
binding credential alive for the whole campaign.

## Proposed decision

### (a) Invite-code custody: one delivery, no operator recovery copy

The owner has three viable custody policies:

| Option | Recovery and support | Exposure and operational cost |
|---|---|---|
| Operator archives clear codes | support can resend | every archive, backup and operator becomes a bearer-capability custodian |
| Volunteer alone retains the code | no operator recovery; replace on loss | smallest operator breach surface; client storage remains a risk |
| Split/encrypt operator escrow | controlled recovery | key-management ceremony exceeds the value of a replaceable invite |

> **Recommendation for owner acceptance: the operator retains no cleartext
> invite after one end-to-end encrypted delivery. The volunteer may retain it
> only until the first successful redemption, then the client deletes it. The
> canonical operator record retains the code hash, account, non-secret support
> label, issue time, expiry and terminal status; stdout, CI logs and shared
> shell recording are prohibited mint channels. A lost unredeemed code is
> revoked and replaced, never recovered.**

The label is personal support metadata even though it is not a credential. It
is access-limited to issuer operators and erased on the campaign's support-
retention schedule; the hash and terminal state may remain after label erasure
without restoring the code. At `100 B` of compact hash/account/status metadata,
one million terminal rows are approximately `100 MB` logical before database
overhead — cheap enough that preventing reuse need not depend on retaining a
volunteer's name forever.

The current CLI is admissible only on an operator-controlled interactive
terminal with capture disabled, restrictive file permissions and an immediate
verified backup/import of the hash ledger. Its local TSV is a transport
artifact. It is not the live redemption authority and is never copied to a
second independently writable issuer.

### (b) Account allocation: one authority, with offline blocks only if allocated centrally

The allocation options are:

| Option | Benefit | Cost / collision behavior |
|---|---|---|
| Independent sequential ledgers | fully offline and simple | overlapping positions collide with probability 1 and may merge accounts |
| Random `u64` ids | no coordinator round trip | non-zero birthday risk and no proof that two operators chose distinct ids |
| One transactional allocator | exact uniqueness and audit | online dependency at allocation time |
| Centrally leased disjoint blocks | offline minting after lease | block inventory, expiry and unused-range recovery |

> **Recommendation for owner acceptance: identity is the sole account-
> allocation authority. Ordinary minting obtains each `AccountId` from one
> transactional allocator. An operator may mint offline only from a durable,
> centrally issued non-overlapping block naming that operator and allocation
> epoch. Until either mechanism exists, exactly one canonical ledger and one
> minting operator may allocate accounts; concurrent CLI minting, even against
> the same path, is prohibited. Redemption fails closed if an invite's account
> already exists without the same invite-allocation identity. It never adopts
> the existing account.**

The authoritative record must distinguish “this retry created the account”
from “some other allocation already owns this number.” `account exists` alone
cannot do that. The permanent regression gate must mint concurrently from two
authorized operators and prove disjoint accounts, then inject a duplicate
allocation and prove that no node is bound.

### (c) Lifecycle: single-use, never reusable, atomically consumed

The lifecycle options are:

| Option | Benefit | Cost / abuse bound |
|---|---|---|
| Multi-use bearer code | easy device recovery and refresh | anyone holding it can bind nodes until the eight-node/rate caps intervene |
| Delete row on first use | later lookup refuses | loses audit/reason, races backup restore, cannot distinguish consumed from unknown |
| Durable terminal state | explicit single-use, revocation and support | requires authoritative online state and a transaction boundary |

> **Recommendation for owner acceptance: every invite is single-use and moves
> monotonically `Issued -> Consumed` or `Issued -> Revoked`; `Consumed` and
> `Revoked` never return to `Issued`. The hash is never allocated again. First
> account creation, first NodeId binding and `Consumed` must commit atomically,
> or all abort. A token-signing failure after that commit is retried through
> authenticated session issuance, never by reopening the invite.**

`Consumed` records the winning NodeId and commit time; `Revoked` records time
and operator/reason code. Terminal hash/status/account facts remain for the
lifetime of the `orrery-invite-v1-` acceptance version, so an old backup cannot
make a code live and support can distinguish “already used” from “never
issued.” Expiry is another terminal refusal, not reuse permission. Once the
whole prefix/version is permanently disabled, terminal hashes may be deleted
under a separately recorded audit-retention policy.

Revocation before redemption prevents account acquisition. It does not kill a
token already issued. Post-redemption containment uses binding removal,
account standing/invalidation, or emergency issuer-key removal according to
the compromised object; the endpoint must not report invite revocation as
session revocation.

### (d) Issuer-key custody: portable escrow, least privilege, two rotation modes

The realistic custody options under the landed in-process signer are:

| Option | Availability | Compromise surface |
|---|---|---|
| Raw host-local file, no escrow | simplest startup | host loss loses identity; root compromise steals it |
| Encrypted portable secret, released to service at boot | migratable and recoverable | service memory and host root can still steal the live key |
| External HSM/KMS signer | secret need not enter process | new signer API, provider dependency, latency and outage mode |

> **Recommendation for owner acceptance: generate the Ed25519 issuer key off
> the serving host. Keep one encrypted canonical escrow controlled by the owner
> and one independently recoverable, access-audited copy held by a named
> recovery custodian. On the active host, only the identity service UID and
> root may reach the decrypted runtime credential; invite minters, TLS
> automation, CI, gateway and coordinator operators may not. Never bake the
> key into an image, repository, shell history or backup of the invite ledger.**

With the current in-process signer, host root compromise is issuer-key
compromise; file modes do not change that threat count. An HSM/KMS may later
narrow extraction risk, but this proposal does not pretend the landed API uses
one.

Planned rotation remains three-step:

```text
publish incoming key -> verify fleet dual-accepts -> activate incoming
-> wait >= 3,600,000 ms after last old signature -> retire outgoing
```

Emergency compromise is deliberately different:

```text
stop old signing -> publish/activate uncompromised key
-> remove compromised public key from every verifier immediately
-> terminate/re-authenticate sessions signed by it
```

Emergency removal trades a bounded login outage for containment and does not
wait one client-release cycle. Before first deployment, restore the escrow onto
a disposable successor, reproduce the same public key, sign a token, verify it
through the existing verifier, and destroy the test copy. Before the September
host teardown, bring up the successor and complete either that stable-key move
or a planned rotation. The Let's Encrypt expiry date is irrelevant to this
deadline.

### (e) Campaign sessions: one hour, half-TTL refresh, no invite replay

The session options are:

| Option | Freshness / containment | Cost |
|---|---|---|
| One-hour token, refresh at 30 min | existing maximum; stale claims bounded to 1 h | `2C/3600` refreshes/s |
| Shorter token, refresh at half TTL | tighter standing/key window | load rises as `2C/T` |
| Multi-day campaign token | fewer refreshes | violates the landed one-hour cap and multiplies key/standing staleness |
| Re-redeem invite on launch | simple with current V1 | keeps a multi-use account-binding bearer capability |

> **Recommendation for owner acceptance: the campaign uses the landed
> `3,600,000 ms` session TTL and refreshes at `1,800,000 ms`. Refresh requires
> a currently valid session token plus proof of the bound NodeId and repeats
> account, binding and standing checks. It never accepts an invite code. No
> campaign override may exceed the verifier cap.**

At 10,000 continuously connected accounts the steady refresh rate is 5.56/s,
beside [docs/09 §11]'s separate 33 auth/s patch-day planning line. The endpoint
must measure issue/refresh QPS and latency, standing-read failures, signature
failures and refreshes attempted after binding removal. Established sessions
retain [docs/09 §8]'s existing expansion rule for outage grace; new login and
refresh still fail when identity is unavailable. A shorter TTL remains an
owner-selectable future
campaign policy only after measuring that path and recording the changed
freshness/load arithmetic.

### (f) Accepted records and expansion rules the owner must resolve

This proposal does not silently edit architectural history. Acceptance requires
the owner to resolve these tensions explicitly:

- **D31 clauses (a) and (d), as amended by D36.** D31 gives `d` only its named
  identity sub-spans and makes identity their sole writer. Atomic allocation
  and invite consumption need durable allocator/allocation/status records in
  the same account-creation transaction. The owner must either amend D31 with
  additive identity-owned sub-spans or select another transactional store that
  still proves atomicity; a local TSV cannot do so.
- **D31's out-of-scope boundary.** D31 explicitly leaves login, credentials,
  payment and account cost undecided
  (`docs/adr/0031-id-account-subspace.md:21-26`). Accepting D41 would decide
  only the operator-issued invite credential and account-allocation path. The
  owner must confirm that narrow incursion without rewriting D31's accepted
  text; payment, pricing and general login remain open.
- **D12 and [docs/09 §1–§2].** The accepted service is Identity, while the
  expansion describes stateless replicas backed by one home FDB. A host-local
  live invite ledger and host-bound key make the approved first endpoint a
  stateful singleton. The owner must choose portable shared lifecycle state and
  replicated service operation, or explicitly accept a temporary single-
  endpoint availability exception with a dated migration gate.
- **[docs/09 §8].** Its planned key rotation says dual-accept for one client-
  release cycle. D41 recommends at least one maximum token lifetime for planned
  session-key rotation and immediate distrust for compromise. The owner must
  clarify that the existing sentence governs planned service-NodeId rotation,
  not emergency continued trust in a stolen identity issuer key.
- **D16.** The one-hour cap is landed code and expansion text but is absent
  from D16's parameter table. The owner must decide whether acceptance adds the
  session TTL and half-TTL refresh rows to D16 or deliberately leaves them as a
  protocol constant plus operations rule.

## Consequences if accepted

- A leaked unredeemed invite buys at most one first binding; after a committed
  redemption it buys zero. A stolen issuer key remains fleet-wide and is never
  described as equivalent to a stolen invite or TLS key.
- The current local TSV and `redeem_invite` are insufficient for deployment:
  authoritative allocation, lifecycle state, atomic consumption, authenticated
  refresh, endpoint throttling and secret loading remain implementation work.
  **None of that is implemented by this Proposed record.**
- Support cannot recover or resend a lost clear code. It can revoke and replace
  an unredeemed allocation, explain a consumed/revoked result from terminal
  metadata, and remove labels without permitting reuse.
- Multiple operators may work only through one allocator or centrally leased
  disjoint blocks. Independent ledgers cease to be a supported scaling model.
- Planned host replacement need not rotate the issuer identity, because escrow
  portability is proved before deployment. Planned key rotation preserves old
  sessions for one hour; emergency removal intentionally invalidates them.
- One-hour/half-TTL refresh costs `2C/3600` requests/s and bounds ordinary
  key/standing staleness to one hour. Shorter TTL remains a quantified trade,
  not an intuition.
- Acceptance creates a deployment gate: no public redemption endpoint may
  start while any recommendation in clauses (a)–(e) lacks an implemented,
  mutation-checked control or an owner-recorded temporary exception.

## Alternatives considered

- **Treat the hash-only TSV as sufficient custody policy.** Rejected as a
  recommendation: it answers what is stored, not who may mint, how stdout is
  delivered, what happens on loss, or how live/revoked/consumed state survives
  concurrency and restore.
- **Archive every clear code for support.** Rejected: replacement is cheap and
  an archive converts every support backup into the complete bearer-capability
  set. Hash-only storage deliberately bought the opposite property.
- **Let each operator own an `AccountId` sequence.** Rejected: two empty
  ledgers collide at every position, and current redemption can merge rather
  than refuse the principals.
- **Use random `u64` account ids and accept the birthday bound.** Rejected for
  authority. It changes the probability but supplies no allocation provenance
  and leaves collision handling load-bearing.
- **Keep codes multi-use as refresh credentials.** Rejected: the code has no
  device binding, expiry or revocation state today and can add nodes up to the
  account cap. The signed node-bound token is the narrower refresh credential.
- **Delete consumed rows.** Rejected: absence refuses but does not prove
  consumption, permits old-backup resurrection, and gives support no answer.
- **Rotate a compromised key through ordinary dual-accept.** Rejected: every
  minute of dual acceptance is a minute every verifier accepts attacker
  signatures. Availability cannot be bought by preserving known-bad trust.
- **Generate a fresh issuer identity on every host.** Rejected: host migration
  becomes fleet trust rollout, and destroying a host can destroy the only key
  before its last sessions expire.
- **Use the TLS private key as the session issuer key.** Rejected: different
  protocols, audiences and rotation events require distinct keys and blast
  radii.
- **Adopt [#345]'s long-lived campaign token.** Rejected by the proposed policy
  and impossible through the landed public API: both issuer and verifier cap
  TTL at one hour.

## Open questions reserved to the owner

1. Whether to accept the recommendations in clauses (a)–(e), each of which is
   still non-normative while this record is Proposed.
2. Whether authoritative offline minting is needed at all; if yes, who may
   lease blocks, their size, expiry and recovery rule. If no, the transactional
   allocator is the smaller mechanism.
3. Which D31-compatible key sub-spans or alternate transactionally consistent
   store hold allocation and lifecycle state, and how D38's at-rest envelope
   work sequences with them.
4. The support-label retention period and the operator/recovery-custodian
   identities. The proposal fixes separation of duties, not people's names.
5. Whether the first deployment must wait for replicated identity service or
   receives a dated single-host exception; in either case, the successor-host
   restore rehearsal precedes deployment on the September-retiring host.
6. Whether a later HSM/KMS signer justifies changing the landed in-process
   signing API after measured latency and outage behavior exist.

[D12]: 0012-backend-services.md
[D31]: 0031-id-account-subspace.md
[D33]: 0033-strike-ledger-standing.md
[D38]: 0038-at-rest-schema-versioning.md
[docs/09 §1–§2]: ../09-services-and-ops.md
[docs/09 §8]: ../09-services-and-ops.md
[docs/09 §11]: ../09-services-and-ops.md
[#345]: https://github.com/baadc0de/orrery/issues/345
[#357]: https://github.com/baadc0de/orrery/pull/357
