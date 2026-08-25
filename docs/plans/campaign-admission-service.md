# The campaign admission service (#476 design)

**Verdict: one Python file behind nginx, whose only jobs are to run the
binaries that already exist and to keep an append-only record of what it ran.**
The service lists campaigns, admits one volunteer at a time under a nickname,
mints the session id and account through `orrery-invite mint` (subprocess, so
the ledger's uniqueness lock is reused, not reimplemented), signs the token
through `orrery-invite session-token` (subprocess, so `SessionTokenV1` has
exactly one implementation), launches `p1-swarm --external-peer` over SSH on
the campaign's own — possibly ephemeral — server, pinned to the admitted
session id, and hands the client a `CampaignJoinFileV1`-shaped JSON over the
wire. On close the client uploads its `campaign-records.jsonl` row and
telemetry to the service, which files them beside the host's own report and
**banks nothing** — `scripts/p4-campaign-session.sh assemble` and
`p4-ledger.sh append` remain the only gate, run off-host by the operator, so
the two-copy reconciliation survives untouched (§7). The service and every
piece of durable state live on the permanent relay box; only the harness runs
on disposable campaign servers (§10). The issuer key sits beside the service
on the relay, rotated fresh per campaign; what that trades away is stated
without softening in §4.

The volunteer's whole procedure becomes: start the game, click a campaign,
type a nickname, play, quit. Zero pasted blobs, zero terminal, zero files
handed back. That is the owner's stated criterion and every trade-off below is
resolved toward it.

This is a design document. It changes no code and amends no accepted record by
itself; the D41 amendment it requires is drafted as a **proposal** in §11.
Every code citation was read from the working checkout at `54a8ee81` on
2026-08-25. One brief-supplied claim did not survive verification and several
are unevidenced in the tree; §14 logs them.

An analogy that holds up: the service is a **box office**, not a bank. It
writes your nickname on a numbered ticket (mint), stamps the ticket so the
door will honor it (session-token), and opens the door for one patron at a
time (the harness pins one session id). The vault — the hours ledger — is in
another building, and the box office cannot deposit into it; it can only file
receipts that the accountant later reconciles against the door's own log.

## 1. What the tree actually holds (verified)

Everything the service needs already exists as an offline binary or a script;
the service is glue, which is why a Python file is genuinely sufficient rather
than optimistically sufficient.

- **Minting.** `orrery-invite mint --ledger <tsv> --label <label>` allocates
  the next `AccountId`, a fresh invite code, and a **pre-minted UUIDv7 session
  id**, under an exclusive flock with atomic replace
  (`crates/orrery_identity/src/invite.rs:133-171`, `update_locked`;
  `bin/orrery-invite.rs:68-77` prints `account=`, `invite_code=`,
  `session_id=`). The ledger refuses duplicate session ids at mint and at
  parse (`invite.rs:271-276,438-444`). Labels must be nonempty single-line
  text without tabs (`invite.rs:425-427`).
- **Signing.** `orrery-invite session-token --issuer-credential <path>
  --account <n> --node <hex>` builds `SessionTokenClaimsV1::new(…)` with
  `SessionStanding::Good`, `on_probation: true`, TTL capped at
  `MAX_SESSION_TOKEN_TTL_MS = 3_600_000` (`bin/orrery-invite.rs:85-108`;
  `crates/orrery_protocol/src/identity.rs:42,117-158`), signs it through
  `IssuerKeyring`, and prints `issuer_key_id=`, `issuer_public_key=`, and
  `session_token=<hex>`. The token binds `claims.node` — the client's
  transport identity — and the verifier refuses any other dialler
  (`identity.rs:389-419`).
- **Issuer key lifecycle.** `orrery-issuer-key generate/escrow/restore/load`
  (`crates/orrery_identity/src/bin/orrery-issuer-key.rs:24-107`) produces a
  plain runtime credential, mode `0600` on Unix, refused inside git work
  trees, with passphrase-encrypted `age` escrow for portability
  (`crates/orrery_identity/src/issuer_key_lifecycle.rs:1-28,110-121`).
- **The host's door.** `p1-swarm --external-peer` judges a join with
  `Admission { require_client_rev, require_session, issuer }`
  (`gates/p1-swarm/src/exterior.rs:469-548`): `require_session` is a single
  `Option<String>` matched exactly, and the token is verified **for the
  dialler's transport identity** at join time. `--require-session` and
  `--issuer-key <id>:<pubkey hex>` are per-process flags
  (`gates/p1-swarm/src/main.rs:378-385,562-568`). The host writes
  `--listening-file` as one line, `<node id hex> <ip:port>`
  (`main.rs:363-366`), and its report carries a **singular** `.external`
  block (`scripts/p4-campaign-session.sh:141-145` requires
  `.external != null`).
- **Direct reachability.** `bridge::bind` sets `RelayMode::Disabled`
  (`gates/p1-swarm/src/bridge.rs:63-76`) — the brief's citation of line 71 is
  current. The campaign server must expose its UDP port publicly; the client
  mirrors the choice (`clients/regolith/src/net.rs:426-437`).
- **The host identity is fixed and public.** `bot::host_key()` derives from a
  hardcoded seed (`gates/p1-swarm/src/bot.rs:1379-1384`), so `--host-node` is
  a constant the service can serve, and it authenticates nothing (#474).
- **The client's transport key is derivable from the slot.** `slot_secret`
  (`clients/regolith/src/net.rs:418-424`) — the brief's citation holds — is a
  pure function of the public slot number. This is #409, open. §3 makes the
  wire protocol indifferent to it.
- **The client records itself.** At shutdown the client appends one
  `SessionRecord` row to `campaign-records.jsonl` beside its telemetry
  (`clients/regolith/src/lib.rs:936`,
  `clients/regolith/src/campaign.rs:784-806`); a session that never joined
  writes **no row** (`campaign.rs:779-782`). `assemble` then requires exactly
  one row naming the pinned session id, the mismatch-flag arithmetic to hold,
  a human actor, the host's platform triple, and a witnessed external run
  (`scripts/p4-campaign-session.sh:127-192`).
- **The join file is in flight, not landed.** PR #475 (`feat/473-join-file`)
  is **OPEN with auto-merge enabled**, not merged: the working tree's
  `clients/regolith/src/main.rs` still takes only the four flags, and
  `CampaignJoinFileV1` appears nowhere under `crates/orrery_protocol/src/`.
  The brief's "just landed (#475)" is ahead of the tree; §14 logs it. This
  design assumes #475 merges first and treats its
  `CampaignJoinFileV1 { host_node, slot, session_id, session_token }` JSON
  shape (read from the PR diff) as the vocabulary the service speaks.

**The single constraint that shapes everything:** the harness pins **one**
session id per process and hosts **one** external participant per run. So a
"campaign" the service lists is not a lobby — it is a hosting configuration
that admits one volunteer at a time, each admission being one harness run.
That bound comes from the harness's shape, not from any box's resources (§10).
Widening the harness to a set of admitted sessions is real Rust work and is
named in §12 as future, not assumed.

## 2. The service, concretely

One file, `/opt/orrery/admission.py` on the **permanent relay box**
(`ubuntu-2gb-hel1-1`, §10 — verified on the box: systemd, with the running
`iroh-relay.service` unit as the pattern to follow, and Python 3.14.4), in
Python 3 standard library only (`http.server.ThreadingHTTPServer` on
`127.0.0.1:8323`, `subprocess`, `json`, `configparser`, `fcntl.flock`,
`os.statvfs`), behind nginx doing TLS and body-size limits. nginx owns
public TCP 443 and terminates both hostnames with certbot-managed
certificates (owner decision, §10.1); §10.1 also carries the one silent
configuration trap in the relay move.
The harness does **not** run here: the service launches it over SSH on the
campaign's own server (§3.2 step 10). No framework, no database, no queue, no
second service. Every piece of durable state is a file on this one permanent
box:

```text
/opt/orrery/admission.py            the service (this design's whole scope)
/etc/orrery/campaigns.conf          operator control file (§2.1) — the only file a human edits
/var/lib/orrery-admission/
  issuer.cred                       runtime signing credential, 0600 (§4)
  campaign_ssh_key                  service-owned ssh identity, 0600, authorized on campaign servers (§10)
  <campaign-id>/ledger.tsv          orrery-invite ledger: accounts, nicknames-as-labels, session ids
  <campaign-id>/joins.jsonl         append-only admission log (§6)
  sessions/<session-id>/
    raw.json                        the host's report — pulled back from the campaign server at session end (§10.2)
    listening.txt                   host node + socket — pulled at session start (§3.2 step 11)
    client-records.jsonl            the client's uploaded rows (§7)
    telemetry.jsonl                 the client's uploaded telemetry stream (§7; deleted after the operator's pull, §10.3)
```

The service never parses or produces a `SessionTokenV1`, never computes a
UUIDv7, and never allocates an `AccountId` itself: those live in
`orrery-invite`, invoked as a subprocess, so there is exactly one
implementation of each and the Python file cannot drift from the Rust wire
format. Rejected alternative: reimplementing postcard + Ed25519 signing in
Python — it *is* the second token format the epic forbids, even when the bytes
initially agree. Rejected alternative: a small new Rust signing daemon — a
second service where a subprocess call suffices.

nginx config, in full (the service is HTTP-only on loopback; nginx owns TLS):

```nginx
# nginx owns public TCP 443 and terminates TLS for both hostnames with
# certbot-managed certificates — dns-01 for both, since 80 stays the
# relay's and 443 is nginx's own (§10.1, owner decision).
server {
    listen 443 ssl;
    server_name campaigns.distopik.com;
    location /v1/ {
        proxy_pass http://127.0.0.1:8323;
        client_max_body_size 64m;      # telemetry upload bound (§7, §10.3)
        proxy_read_timeout 60s;        # join blocks while the harness binds (§3.2)
    }
}
server {
    listen 443 ssl;
    server_name iroh-relay.distopik.com;
    location / {
        proxy_pass https://127.0.0.1:8543;   # the relay's now-private TLS listener (§10.1)
        proxy_ssl_server_name on;
        proxy_http_version 1.1;              # the relay path upgrades to websocket
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 1h;               # relay connections are long-lived
    }
}
```

### 2.1 The operator control file (owner requirement)

The owner requires control through a simple file on the admission box that
lists campaigns
and marks them open or closed. That file is `/etc/orrery/campaigns.conf`, INI
format, parsed with Python's stdlib `configparser`. INI over JSON or TSV
deliberately: no quoting, no commas, no significant tabs — the three ways an
operator editing over SSH at 23:00 gets a file subtly wrong — and a section
header is hard to mangle invisibly. A worked example with two campaigns:

```ini
# /etc/orrery/campaigns.conf — the operator edits this file and nothing else.
# open = yes admits volunteers; anything else (or a missing key) is closed.

[shakedown-3pct]
title       = Shakedown: 3% loss, 100ms jitter
open        = yes
host        = 203.0.113.7
peers       = 8
seconds     = 3600
loss_pct    = 3
jitter_ms   = 100
client_rev  = 54a8ee81

[clean-link]
title       = Clean link baseline
open        = no
host        = 203.0.113.7
peers       = 8
seconds     = 1800
loss_pct    = 0
jitter_ms   = 0
client_rev  = 54a8ee81
```

The section name is the campaign id (URL-safe: `[a-z0-9-]{1,64}`, refused
otherwise at parse). `host` is the campaign server — the ephemeral box the
service launches the harness on over SSH (§10); campaign stand-up fills it
in, and a join against a dead or wrong `host` fails at §3.2 step 11 as
`host_failed`. `peers`, `seconds`, `loss_pct`, `jitter_ms` become the
harness invocation and the client's `--expect-*` configuration; `client_rev`
becomes `--require-client-rev`. Omitting `client_rev` omits the pin — legal,
but the listing marks the campaign `unpinned` so the operator can see it.

**Read discipline: re-read on every request.** At one join per hour-long
session the file is parsed a handful of times a day; caching it buys nothing
and costs the operator a restart. Closing a campaign is therefore: edit the
file, save — the next `GET /v1/campaigns` shows it closed and the next join
refuses. No signal, no reload endpoint, nothing to restart mid-playtest.

Defined behavior for the three failure shapes:

- **Missing file** → the campaign list is empty (mirrors
  `InviteLedger::load`'s absent-file-is-empty-ledger posture,
  `invite.rs:110-118`). The listing response carries
  `"operator_note": "campaigns.conf is absent"` so an empty list is
  distinguishable from a closed playtest.
- **Malformed file** → the service keeps serving the **last successful
  parse** (held in memory) and refuses nothing that was open under it; the
  listing carries `"operator_note": "campaigns.conf failed to parse: <error>;
  serving the previous version"`. A typo must not eject volunteers mid-join.
  If the service has never parsed it successfully, the list is empty with the
  error in the note.
- **Edited mid-request** → each request reads the file once, start to finish;
  a torn in-place write at worst fails that one parse and falls into the
  malformed case above. Operators using rename-into-place editors (vim's
  default) never present a torn file at all.

**Closing an in-progress session:** flipping `open = no` stops **new
admissions only**. A session already running belongs to its harness process,
which terminates itself at `--seconds` regardless; the volunteer keeps the
hour they were promised. An operator who must end a session *now* stops the
harness (`systemctl kill orrery-harness@<session-id>` or plain `kill`) — the
client sees the link close, writes its row, and the hour simply may not
assemble. The control file controls admission; it is not a process manager,
and giving it kill semantics would make a typo lethal.

## 3. The admission wire protocol

Three endpoints, versioned under `/v1/`, JSON both ways, every error a JSON
body `{"error": "<machine tag>", "detail": "<sentence for the volunteer>"}`
with a conventional status code. The client renders `detail` verbatim in the
UI — the service is the one party that knows *why* a join failed, so it writes
the sentence (the same posture as `Admission::judge`, whose refusal strings
are "worded for the volunteer who will read it", exterior.rs:490-492).

### 3.1 `GET /v1/campaigns`

```json
{
  "campaigns": [
    {
      "id": "shakedown-3pct",
      "title": "Shakedown: 3% loss, 100ms jitter",
      "state": "open",
      "peers": 8,
      "seconds": 3600,
      "loss_pct": 3,
      "jitter_ms": 100,
      "client_rev": "54a8ee81"
    },
    { "id": "clean-link", "title": "Clean link baseline", "state": "closed",
      "peers": 8, "seconds": 1800, "loss_pct": 0, "jitter_ms": 0,
      "client_rev": "54a8ee81" }
  ],
  "operator_note": null
}
```

`state` is `open`, `closed`, or `busy` (a session is in progress; §3.2).
When the mint floor is holding admissions (§10.3), every otherwise-open
campaign lists as `"state": "paused"` and the top-level `operator_note` says
why — `"admissions paused: 7.9 GB free is below MINT_FLOOR_BYTES"` — so an
operator reading the listing (or `journalctl`, which gets the same line at
WARNING) sees a service that is refusing on purpose, not one that has
crashed.
Closed and busy campaigns are listed, not hidden: a volunteer staring at an
empty list files a bug report, one staring at `busy — try again in ~40 min`
waits. The listing also carries `"server_rev"` so a client can warn about a
pin mismatch *before* the volunteer types a nickname rather than after.

### 3.2 `POST /v1/campaigns/{id}/join`

Request — the client sends its transport identity explicitly:

```json
{
  "nickname": "ada",
  "node": "b7e2…64 hex chars…9c1",
  "client_rev": "54a8ee81"
}
```

`node` is the client's transport public key as hex, exactly what
`--print-slot-key` prints today (`clients/regolith/src/main.rs:22-34`). The
service signs **the presented node** and never derives it from the slot.
Today the client computes that key from `slot_secret(peers)` — publicly
derivable, #409 — so the field adds no secrecy yet; it exists so that when
#409 lands and the client holds a private transport key, **nothing on this
wire changes**: the client presents a different public key and the same
request admits it. A protocol that derived the node server-side from the slot
would work today and break on exactly the day the security fix arrives.

The handler, in order (pseudocode; each refusal names its step):

```text
1  campaign = parse(campaigns.conf)[id]          else 404 unknown_campaign
2  campaign.open                                 else 403 campaign_closed
3  statvfs(/var/lib/orrery-admission).free >= MINT_FLOOR_BYTES
                                                 else 503 admissions_paused
      (the owner's 10 GB mint floor, §10.3 — read fresh per request, never
       cached, measured on the upload store's filesystem, not "/")
4  nickname matches ^[^\t\r\n]{1,32}$ non-empty  else 422 bad_nickname
5  node is 64 lowercase hex chars                else 422 bad_node
6  acquire flock on <campaign>/lock, non-blocking else 409 campaign_busy
      (a live harness pid ⇒ busy; include retry_after_s from --seconds)
7  orrery-invite mint --ledger <campaign>/ledger.tsv --label <nickname>
      → account, session_id     (invite_code discarded; §6)  fail ⇒ 500
8  orrery-invite session-token --issuer-credential issuer.cred
      --account <account> --node <node>
      → session_token, issuer_key_id, issuer_public_key       fail ⇒ 500
9  append {when, campaign, nickname, account, session_id, node}
      to <campaign>/joins.jsonl                               fail ⇒ 500
10 ssh -i campaign_ssh_key orrery@<campaign.host> p1-swarm --external-peer
      --peers P --seconds S --min-cells 1
      --impaired --witness --stamp-wall-clock
      --json /var/tmp/orrery/<sid>/raw.json
      --listening-file /var/tmp/orrery/<sid>/listening.txt
      --require-client-rev <campaign.client_rev>
      --require-session <sid>
      --issuer-key <issuer_key_id>:<issuer_public_key>
      (the harness runs on the campaign's own server, §10 — the relay never
       simulates; the ssh child is held open for the whole session)
11 poll ≤30 s for the remote listening file over ssh; copy it to
      sessions/<sid>/listening.txt              else kill, 503 host_failed
12 respond 200; a reaper thread waits on the ssh child and, the moment the
      harness exits, pulls raw.json → sessions/<sid>/raw.json (§10.2)
```

Response — the `join` object **is** a `CampaignJoinFileV1`, byte-compatible
with what `--join <path>` reads (PR #475), so the client can write it to disk
and the two paths stay one code path:

```json
{
  "join": {
    "host_node": "f2a1…64 hex…",
    "slot": 8,
    "session_id": "01917f0e-2b9a-7c4d-8f21-6a0b3c9d1e2f",
    "session_token": "0801…hex SessionTokenV1…"
  },
  "host_direct": "95.216.0.1:52011",
  "account": 17,
  "expires_in_s": 3600,
  "configured": { "loss_pct": 3, "jitter_p50_ms": 100, "jitter_p99_ms": 100 }
}
```

`slot` is the campaign's `peers` value — the external slot index the runbook
already uses (`p4-campaign-session.sh:66-69`: `--peers 8 … --slot 8`).
`host_direct` comes from `listening.txt`; the client passes it as
`--host-direct` does today, because relays are disabled (§1) and discovery
must not be a silent dependency. `configured` feeds the client's
`ConfiguredImpairment` (`clients/regolith/src/campaign.rs:97-99`) so the
mismatch flag compares against what the *host* declares, ending the last
copy-paste (the `--expect-*` flags).

Every failure a volunteer can actually hit, and what they see:

| Case | Status | `error` | The volunteer sees |
|---|---|---|---|
| campaign id gone from campaigns.conf | 404 | `unknown_campaign` | "That campaign has ended — refresh the list." |
| operator closed it | 403 | `campaign_closed` | "This campaign is closed; pick another." |
| relay disk below the mint floor | 503 | `admissions_paused` | "Campaigns are temporarily unavailable while the operator makes room — nothing you did was wrong. Try again later." (§10.3; the listing shows `paused` before they ever click) |
| someone else is playing | 409 | `campaign_busy` | "In use — try again in about N minutes." (`retry_after_s`) |
| empty/tab-bearing/33-char nickname | 422 | `bad_nickname` | "Nicknames are 1–32 characters, no tabs or newlines." |
| malformed node key | 422 | `bad_node` | "This build sent a bad transport key — reinstall the client." |
| wrong build | 403 | `client_rev_mismatch` | "This campaign needs build 54a8ee81 — download the current build." (checked at step 1 against the request's `client_rev`, so the refusal happens *before* the harness ever binds; the harness re-checks at join anyway, exterior.rs:520-526) |
| harness would not start, ssh to campaign.host failed, or no listening file | 503 | `host_failed` | "The host could not start your session — tell the operator, nothing you did was wrong." |
| mint/sign subprocess failed | 500 | `admission_failed` | same operator-facing sentence; detail logged server-side, never the credential path |
| service down / nginx 502 | — | — | the client's unreachable screen (§8) |

Timeout note: the join response takes as long as the harness takes to bind
(observed locally: seconds; bounded at 30 s by step 11). The client shows
"starting your session…" for the duration; nginx's `proxy_read_timeout 60s`
covers it.

### 3.3 `POST /v1/sessions/{session_id}/upload`

Multipart or two sequential PUT-like posts would both work; simplest is one
JSON body:

```json
{
  "records": [ { "session_id": "01917f0e-…", "actor": "human", "…": "…" } ],
  "telemetry_jsonl": "<the client's session.jsonl, as text>"
}
```

Handler:

```text
1  session_id appears in some <campaign>/joins.jsonl   else 404 unknown_session
2  body ≤ 64 MiB (nginx enforces; service re-checks)   else 413 too_large
3  every row in records has .session_id == session_id  else 422 wrong_session
4  write sessions/<sid>/client-records.jsonl and telemetry.jsonl
      (write-once: an existing file is replaced only if byte-identical,
       otherwise 409 conflict — a second, different upload is evidence of a
       problem, not something to silently last-write-win)
5  fsync both files and their directory, then respond 204 — the client's
      local copy stays authoritative until this acknowledgement lands (§7),
      which is what makes host teardown unable to destroy the only copy (§10)
```

There is deliberately **no free-space check on this endpoint**: the mint
floor (§10.3) refuses new *admissions* so that uploads from sessions already
admitted always have room to land — refusing evidence the campaign already
paid a volunteer's hour for would be the worst possible failure. If the disk
nonetheless reaches zero despite the floor's headroom, step 5's fsync fails,
the service answers 500, and the client's local copy (§7) remains
authoritative for a later retry — delayed, never lost.

Step 3 refuses early what `assemble` would refuse later
(`p4-campaign-session.sh:148-152` requires exactly one row for the session),
so the volunteer's client learns about a mix-up while the volunteer is still
at the keyboard. Step 4's fixed server-side filenames mean **the client names
nothing on the relay's disk** — no path traversal surface, and an upload can
never touch `raw.json`, which arrives only through the reaper's pull
(§10.2). The upload endpoint accepts
data; it confers no validity on it (§7).

Idempotency: the client retries the identical body freely (same bytes → 204
again). What is deliberately absent: authentication on the upload. The
session id — 74 bits of mint entropy (`invite.rs:352-356`) — is the
capability, exactly as it is for the join the harness already accepted. An
attacker who has it can file a bogus client row; §7 shows why that banks
nothing.

## 4. The issuer key on the relay box

The runtime credential lives at `/var/lib/orrery-admission/issuer.cred` on
the permanent relay box, mode `0600`, owner `orrery-admission` (the service
UID), generated on the operator's machine at each campaign stand-up and
copied over — the box is permanent (§10) but this key deliberately is not
(below). The lifecycle tooling already enforces the mode and refuses
repository paths (`issuer_key_lifecycle.rs:110-121`).

The trade, stated once: whoever compromises the relay box reads this file
and can sign arbitrary `SessionTokenClaimsV1` — any account, any node, any
standing — for one hour per token, and the same box stores **both halves of
the reconciliation** once the reaper pulls `raw.json` beside the uploaded
client rows (§10.2), so full fabrication of banked hours is within that
attacker's reach until the operator's pull. The owner's observation is
correct that this is not a new species of risk: the dev box where minting
and accumulation happen is exactly as decisive if compromised, so the paper
property D41 protected was always conditional on *some* machine's integrity.
Two distinctions worth naming rather than blurring: the relay is publicly
reachable by design while the dev box accepts no inbound connections — same
worst case, different odds of getting there — and this particular box is
also load-bearing for NAT traversal for every player (§10), so a compromise
or outage here is not contained to campaigns.

**Cheap mitigation, recommended: a campaign-scoped key, and it needs no new
machinery.** `IssuerKeyId` is a plain `u32` selecting the verifier's trusted
key (`identity.rs:70`, `identity.rs:389-404`), and the harness trusts exactly
the one key its `--issuer-key <id>:<pub>` names (`main.rs:441-442`,
`exterior.rs:539-546` builds the verifier from `[issuer]` alone). So: generate
a **fresh key with its own id** (say `--key-id 476`) for campaign
admission; only the harness runs the service launches trust it; the dev
box's issuer key never touches the relay and nothing else anywhere trusts
key 476. Scoping
is by *who trusts the key*, not by a token field — fully expressible with the
current types, zero token-format change. A relay compromise then forges
campaign admissions, which an attacker controlling the admission box could
stage anyway; it forges nothing signed by the real issuer identity.
Emergency containment is D41 (d)'s existing move: stop trusting key 476,
which ends admissions and nothing else.

Rotate the key per campaign even though its box is now permanent. A fresh
key id per stand-up means the harness runs of campaign N+1 trust only key
N+1, so a key stolen during one campaign is worthless for the next without a
fresh compromise — rotation converts "compromised until noticed" into
"compromised until the campaign ends". The types already permit it: a key id
is minted per `orrery-issuer-key generate --key-id <n>`
(`orrery-issuer-key.rs:26-33,80-83`), each harness run trusts only the
single key its `--issuer-key` names, and every token dies in an hour —
nothing outlives the campaign that would need the old key. Keep **no escrow
for this key**: a key cheaper to mint than to archive should be minted, not
archived. D41 clause (d)'s escrow ceremony remains the rule for any
long-lived issuer identity the fleet later trusts; this key is deliberately
not that identity.

## 5. What a nickname is

**A label, deliberately not an identity.** It is stored as the
`volunteer_label` field of the campaign's invite ledger (the mint's `--label`,
§3.2 step 7) and echoed into `joins.jsonl`; it appears in the operator's
support view and nowhere in any signed or banked artifact. Constraints are the
ledger's own: nonempty, ≤32 chars by service policy, no tab/CR/LF
(`invite.rs:425-427` refuses the rest at mint).

Nicknames **collide freely**. Two volunteers named `ada` are two rows with
two accounts; the operator disambiguates by session id and join time, which
`joins.jsonl` gives them. The rejected alternative — a first-claim
`nickname → AccountId` mapping so a returning `ada` keeps one account — was
rejected because with no credential behind it, the mapping is a *pretense* of
continuity: anyone typing `ada` inherits `ada`'s account, standing, and
probation age, which is precisely the account-merging failure D41 §3 documents
for colliding allocations, recreated on purpose. Fake continuity is worse
than honest anonymity. When a real client credential exists (post-#409, or a
successor decision), a durable nickname→account claim becomes an owner
decision worth making; §12 lists it.

Relation to `AccountId`: every admission mints a **fresh** account
(`next_account`, `invite.rs:323-335`) in that campaign's ledger, so an
account is "one volunteer-campaign admission", nothing more. `on_probation`
is `true` in every token the CLI signs (`bin/orrery-invite.rs:96-100`), so a
fresh account forfeits nothing a nickname-stable account would have earned —
probation never ages out inside a one-hour session anyway.

## 6. Session id provenance

**The service mints at admission time — but through the existing pre-minting
machinery, not beside it.** §3.2 step 7 runs `orrery-invite mint`, which
allocates the UUIDv7 under the ledger's flock and its refuse-duplicates
constraint (`invite.rs:271-276`). This is the middle path between the two
options the epic poses, and it is chosen over both:

- *Pure service-side minting* (Python `uuid7()`) re-derives in Python what
  `invite.rs:361-378` already implements and tests against
  `p4-ledger.sh`'s regex — a second implementation of a wire shape, rejected
  on the same grounds as a second token format.
- *Pre-minted pool from an operator-run ledger* keeps D41's offline ceremony
  and with it a copy step: someone must carry minted ids to the service and
  keep the pool topped up, and an empty pool at 21:00 on a playtest night is
  exactly the volunteer-hostile failure the owner overturned the papers over.

The mint also emits an invite code; **the service discards it unread**. That
is safe (the ledger stores only its hash; a code nobody ever saw is a code
nobody can present) and it is the honest expression of the owner's decision:
nickname admission means no invite gate, so the capability goes unissued
while the parts of the mint that still matter — account allocation, session
id uniqueness, the label — keep their one implementation. The ledger rows
also give the operator the same support surface invites had: who joined,
when, as which account, under which session id.

`joins.jsonl` (step 9) is the service's own append-only copy — one line per
admission — and is what the upload endpoint checks membership against. It is
recording, not authority: the authoritative copy of "which session this host
ran" is the `--require-session` pin inside the harness process and its
`raw.json`.

## 7. The telemetry upload, and why the two-copy reconciliation survives

What the client sends, and when: on shutdown, after `finish_record` writes the
banking row (`campaign.rs:784-806`), the client POSTs §3.3's body — its row(s)
for this session plus the telemetry stream — to the URL it was handed at join
(the join response's origin; no separate configuration). The upload is a
**courier for the client's copy**, nothing else.

The reconciliation invariant, restated so the preservation argument has a
fixed target: a row banks only when *the host's* report (`raw.json`, written
by the harness process that pinned `--require-session <sid>` at launch) and
*the client's* row agree on the session id, and `assemble` refuses otherwise
(`p4-campaign-session.sh:148-152` — exactly one client row for the pinned id;
`:141-145` — the host actually hosted a witnessed external run). Two
independently produced copies, one arbiter.

How this design preserves it, mechanically:

- **The host's copy is never uploaded and never writable by the client.** The
  harness writes `raw.json` on the campaign server, and the reaper — the
  service's own ssh child, not any client-facing path — pulls it to
  `sessions/<sid>/raw.json` on the relay (§10.2); the upload endpoint writes
  only the two fixed client-side filenames (§3.3 step 4) and refuses
  everything else by construction — there is no parameter that names a path.
- **The service's join record is a third witness, not a substitute.** An
  uploaded row is accepted only for a session id the service itself admitted
  (§3.3 step 1) and only naming that id in every row (step 3). But acceptance
  confers nothing: `assemble` still runs against `raw.json`, not against
  `joins.jsonl`.
- **Banking stays off both boxes.** The operator rsyncs
  `/var/lib/orrery-admission/sessions/` from the relay to their own machine
  and runs `assemble` + `p4-ledger.sh append` there, exactly as the
  "deliberately manual" comment in `p4-campaign-session.sh:14-25`
  anticipated — this *is* the named "upload from the client" replacement,
  pointed at the relay instead of S3, and it changes nothing upstream of the
  hand-off. A client's unilateral
  claim cannot become a banked row because no path exists from the upload
  directory into the ledger that does not pass through `assemble`'s refusals.

Failure cases:

- **Upload fails (service down, network drop).** The row is already durable
  in the client's local `campaign-records.jsonl` (written before the upload
  is attempted). The client keeps a sidecar `uploads.json` marking which
  session ids were acknowledged with 204; on next boot it retries every
  unacknowledged row. The sidecar — not a mutation of the records file —
  because the records file is append-only evidence and rewriting it to add
  bookkeeping would hand a future auditor a file the client habitually
  rewrites.
- **Client crashes before writing the row.** No row exists anywhere; the
  session banks nothing. That is today's semantics (`campaign.rs:779-782`:
  never-joined or unfinished sessions produce no row) and it is correct — a
  crashed session measured nothing bankable, and the host's `raw.json` alone
  cannot assemble (no client row ⇒ `assemble` counts zero rows and refuses).
- **Client crashes after writing, before uploading.** The retry-on-next-boot
  path covers it, as long as the volunteer ever starts the game again. If
  they never do, the hour is lost with them; the operator can see the
  admitted-but-never-uploaded session in `joins.jsonl` and ask. Accepted as
  the cost of having no operator round trip.
- **A second, different upload for the same session** (retry gone wrong, or
  tampering): 409 (§3.3 step 4). First-write-wins with byte-identity retries
  keeps replays cheap and rewrites loud.

## 8. The client's boot flow

Verified: the client is Bevy `0.19` (`clients/regolith/Cargo.toml:12`), and
Bevy 0.19's **default** features include `bevy_ui_widgets` (bevy 0.19.1
`Cargo.toml`: `default = ["2d","3d","ui","audio"]`, and `ui` lists
`bevy_ui_widgets`), re-exported as `bevy::ui_widgets`
(`bevy_internal-0.19.1/src/lib.rs:104-105`). The crate ships headless-widget
behaviors — `Button` with an `Activate` observer event, editable text via
`bevy_text::EditableText` + `EditableTextInputPlugin`, `scrollarea`, `radio`
(`bevy_ui_widgets-0.19.1/src/{button,text_input,scrollarea,radio}.rs`) — with
styling left to the app, which matches the HUD's existing hand-styled
`Node`/`Text`/`TextFont` idiom (`clients/regolith/src/hud.rs:274-302`). The
crate warns it is experimental with a moving API; acceptable for a first-party
client that pins `0.19`, and the fallback (hand-rolled `Interaction`-driven
buttons like every pre-widget Bevy app) costs a screen, not a design. So
"simple idiomatic Bevy widgets" is implementable exactly as the owner asked,
with no new dependency.

**Where the client finds the service (owner requirement): baked in, so a bare
double-click works.** The admission URL defaults to
`https://campaigns.distopik.com` — a dedicated alias for the permanent relay
box, plain 443, no port; nginx terminates both hostnames on that socket
(owner decision, §10.1) — as a compile-time constant beside `BUILD_REV`; a volunteer who
runs the binary with **no arguments at all** lands on the campaign list. Two
indirections are deliberate in that choice. First, an **alias, not the
relay's own name**: `iroh-relay.distopik.com` names a NAT-traversal service
that must never have its fate coupled to admission's — if admission ever
moves off the box, the alias moves and the relay's name stays put. Second,
the **HTTPS URL, not any host NodeId**: campaign servers are ephemeral
(§10), and each one's NodeId and socket arrive per-join in the response
(`join.host_node`, `host_direct`, §3.2) — the campaign listing is the
indirection that lets campaign servers come and go without touching a
volunteer's install. Baking a campaign server's name or NodeId would mean
rebuilding every volunteer's client per campaign — rejected. The default is
overridable as `--admission-url <url>` and `ORRERY_ADMISSION_URL`, flag over
env over baked default — the same explicit, tested precedence posture #475
established for the token — so a staging service is one env var away.

Flow, as states of a `JoinGate` resource driving one UI root:

```text
FetchingCampaigns ──ok──▶ Browsing ──click──▶ NicknameEntry ──Join──▶ Admitting
      │ error                    ▲                                        │
      ▼                          └────────────── error (dialog) ◀─────────┤
  Unreachable ──Retry──▶ FetchingCampaigns              ok: write join file,
      │                                                 build CampaignConfig,
      └─"play offline with a join file (--join)"        enter today's session
```

- **FetchingCampaigns / Admitting:** the HTTP calls run on
  `bevy_tasks::IoTaskPool` (or a plain `std::thread` with a channel — the
  client already carries tokio for iroh, `Cargo.toml:25`); a system polls the
  task each frame. Never block a Bevy system on a socket.
- **Browsing:** a `scrollarea` of rows, one `ui_widgets::Button` per open
  campaign showing title, impairment profile, and state; `busy` and `closed`
  rows render dimmed with the reason, not hidden (§3.1).
- **NicknameEntry:** one `EditableText` field pre-focused, the §3.2 nickname
  rule validated live (length, no tabs), and a Join button that disables
  while invalid. The consent notice (`CONSENT_NOTICE`, shown today on
  stderr, `main.rs:42-48`) moves onto this screen with the checkbox the
  `--campaign-consent` flag becomes; consent recorded before Join enables.
- **Admitting:** "starting your session…" for up to the 30 s harness bind
  (§3.2 step 11); on 200, the client writes the response's `join` object to
  disk as a `CampaignJoinFileV1` (crash-recovery artifact and debugging aid)
  and constructs the same `CampaignConfig` the argv path builds
  (`campaign.rs:80-100`), with `configured` taken from the response. One
  config type, three producers: service, `--join` file, argv flags.
- **Between campaigns, the normal screen is an empty or all-closed list from
  a live service, not an error.** The service is permanent (§10), so
  `GET /v1/campaigns` answers year-round; the Browsing screen renders "No
  campaigns right now — check back later" (plus §3.1's `operator_note`, and
  `paused` states when the mint floor is holding, §10.3). It must not look
  broken, because nothing is.
- **While the service is genuinely unreachable** (DNS failure, refused
  connection, nginx 502, timeout): a screen stating exactly that — "Can't
  reach the campaign service — <error>" — with a Retry button. With a
  permanent host this is evidence of a real problem, and the screen says so:
  "if this persists, tell the operator". One quiet line names the escape
  hatch: "have a join file? start with `--join <path>`". No spinner-forever,
  no empty list pretending to be a quiet night.
- **Precedence:** argv `--join`/`--host-node` (and `--smoke-*`) bypass the
  boot UI entirely, preserving `scripts/p4-campaign-session.sh` and every
  existing automation unchanged. UI appears only when no campaign
  configuration arrived by argv — the same explicit-precedence posture #475
  tested for its three token sources.

## 9. The `--join` fallback

**It survives, as the offline/debug path.** Reasons, in order of weight:
`scripts/p4-campaign-session.sh` and the headless smoke path drive the client
by argv and must keep working without an HTTP service in the loop; a
localhost debugging session should not require standing up nginx; and when
admission is down mid-playtest, an operator who can SSH anywhere can still
run the old ceremony end to end — degraded, but not dark. The cost of keeping it is
near zero because §8 makes the service path *produce* a join file, so the
fallback is not a second format, merely a second way to obtain the first one.
`--join` stays undocumented to volunteers except by the unreachable screen's
hint; it is an operator tool now, not the volunteer path.

## 10. Two boxes: the permanent relay, and ephemeral campaign servers

§12.1 was resolved and then improved, in three owner statements on
2026-08-25. First: the 2 GB relay box stays — *"it has basically unmetered
bw"* — while the 16-core `orrery-hel1-1` (65.108.197.237) is replaced on
storage grounds: its consumer NVMe disqualifies it as a persistent-test
host, and its successor is either an ephemeral AWS instance stood up for a
few hours per campaign or a better-NVMe Hetzner machine. Second, the
placement that dissolves most of the resulting state-survival problem:
*"This would make the small iroh relay perfect to host admission and tally
while campaign servers come and go."* Third, the disk rule §10.3 designs
precisely: *"Refuse to mint sessions when less than 10gb disk."*

So the deployment is two-tier, each tier keeping what suits its lifetime:

- **Permanent: the relay box** — `ubuntu-2gb-hel1-1`, 62.238.59.131,
  serving `iroh-relay.distopik.com` (operator-supplied: 1 vCPU, 1.9 GB RAM,
  38 GB disk, effectively unmetered bandwidth; verified on the box: systemd
  with `iroh-relay.service` as the running-unit pattern to follow, and
  Python 3.14.4). It hosts admission and tally: `admission.py`,
  `campaigns.conf`, the ledgers and `joins.jsonl`, the campaign issuer key,
  and every uploaded client row. **No durable state ever faces a teardown**,
  which retires the provisioned-or-pulled bookkeeping an ephemeral admission
  host would have needed. 1 vCPU is ample: the service mints and signs per
  join; it never simulates.
- **Ephemeral: campaign servers** run exactly one thing, `p1-swarm
  --external-peer`, launched per admission by the service over SSH (§3.2
  step 10) and reached by volunteers over direct UDP — relays stay disabled
  on the exterior wire (§1); the relay box's iroh relay serves the wider
  fleet's NAT traversal, not this path. Stood up for a campaign, torn down
  after; the ledger of what they briefly hold is §10.2.

(The relay's effectively unmetered bandwidth is a fact that may bear on
#184's relay economics; flagged, not designed for — enabling relays for
cohort traffic remains #375's operator decision.)

### 10.1 Sharing 443 with the relay: the owner's route, done carefully

Verified on the box: `iroh-relay` (1.0.3, matching the repo's `iroh 1.0.3`
pin) listens on 0.0.0.0:80, 0.0.0.0:443 and :9090, with
`cert_mode = "LetsEncrypt"` and `enable_quic_addr_discovery = true` in
`/etc/iroh-relay/config.toml`.

An earlier draft of this section refused to front the relay with nginx,
believing QUIC address discovery and reverse-proxy mode were mutually
exclusive in 1.0.3. **That was wrong — the exclusion is narrower** (§14).
The actual guard is only that QAD requires a `[tls]` section to exist
(iroh-relay-1.0.3 `src/main.rs:585`: `enable_quic_addr_discovery &&
cfg.tls.is_none()` bails), not that the relay own public TCP 443. And the
QUIC listener is independently bindable: `quic_bind_addr` defaults to
`https_bind_addr`'s address with port `DEFAULT_RELAY_QUIC_PORT = 7842`
(`main.rs:400-406`, `defaults.rs:7`) — QAD's UDP socket is already not on
443 in the live deployment. So the owner's arrangement — nginx on public
TCP 443, relay behind it — keeps everything, **and is the recommendation**:

```toml
# /etc/iroh-relay/config.toml — the two lines that change, and the trap.
https_bind_addr = "127.0.0.1:8543"        # was public :443; nginx fronts it now
quic_bind_addr  = "62.238.59.131:7842"    # MUST be pinned — see below
# [tls] section stays exactly as it is: QAD requires it to exist (main.rs:585)
```

**The trap, named because it fails silent:** `quic_bind_addr` *defaults to
`https_bind_addr`'s IP*. Move `https_bind_addr` to loopback without pinning
`quic_bind_addr` and QAD silently binds `127.0.0.1:7842` — the relay starts,
nothing errors, and address discovery is quietly dead for every player. The
explicit pin above is therefore not a style choice, and the change carries a
**named verification step, `qad-listens-publicly`**, run after every relay
config change: on the box, `ss -ulpn 'sport = 7842'` must show
`62.238.59.131:7842` (not `127.0.0.1`); then one client off-network confirms
it still reaches the game host directly. A relay config change without that
check is not done.

nginx owns public TCP 443 and **terminates TLS for both hostnames** (§2 has
the exact config): `campaigns.distopik.com` proxies to admission on
loopback, `iroh-relay.distopik.com` to the relay's now-private TLS listener
on `127.0.0.1:8543`. The relay-side hop stays TLS (`proxy_pass https://`)
rather than plain HTTP deliberately: the relay must keep its `[tls]`
section for QAD regardless, and whether 1.0.3 serves the relay protocol
over its plain-HTTP listener while `[tls]` is configured is unverified —
double-terminating on loopback costs microseconds and removes the question.
One canonical hostname per service, no port in the baked URL (§8).

**The certificate arrangement is decided** (owner, 2026-08-25: *"manual /
reloading and certbot sounds appealing"*): **certbot owns renewal for both
hostnames, and the relay moves off `cert_mode = "LetsEncrypt"` onto
`Reloading`** — of `CertMode`'s three values, `Manual | LetsEncrypt |
Reloading` (`main.rs:63-68`), `Reloading` and not `Manual`, and the
distinction is the whole point:

- `Manual` reads the files **once at start**. Every certbot renewal —
  forever, roughly every sixty days — would need a relay restart: a
  standing appointment to interrupt NAT traversal for every player.
- `Reloading` builds a `reloading_resolver` over `cert_path()`/`key_path()`
  (`main.rs:673-683`) that re-reads both files every
  `DEFAULT_CERT_RELOAD_INTERVAL` = **24 hours**
  (`src/server/resolver.rs:19`) and hands the most recently loaded
  `CertifiedKey` to rustls per handshake — certbot renews, the relay picks
  it up within a day, nothing restarts, no deploy hook exists to forget.

The paths come from `manual_cert_path` / `manual_key_path` (`main.rs:423,
429`, resolved via `cert_path()`/`key_path()` at `:475,481`), pointed at
certbot's live directory:

```toml
[tls]
cert_mode        = "Reloading"   # re-read every 24 h (resolver.rs:19); no restart on renewal
manual_cert_path = "/etc/letsencrypt/live/iroh-relay.distopik.com/fullchain.pem"
manual_key_path  = "/etc/letsencrypt/live/iroh-relay.distopik.com/privkey.pem"
```

Two operational consequences belong in the runbook, not in a footnote:

1. **The 24-hour reload is the safety margin, not the renewal clock** —
   certbot renews ~30 days before expiry, so a day's lag is harmless. But
   the relay has now **lost its own ACME**: a certbot failure is invisible
   to it, and it will serve the old certificate happily until expiry, then
   fail every handshake — NAT traversal down for everyone, discovered by
   players. The monitoring that catches this is an **expiry check on the
   served certificate**, not "did the timer run": a daily
   `cert-expiry-watch` on the box —
   `openssl s_client -connect 127.0.0.1:8543 -servername
   iroh-relay.distopik.com </dev/null | openssl x509 -noout -checkend
   1209600` (and the same against public 443 for both hostnames) — alerting
   the operator when any served cert has under 14 days left, which is two
   missed renewal cycles' warning.
2. **Certbot must renew while nginx owns 443 and the relay owns 80** —
   neither HTTP-01 (needs 80) nor TLS-ALPN-01 (needs 443's handshake) has a
   clean home. **dns-01 for both hostnames**, via the DNS provider's API
   plugin: no listener contention at all, and it also issues for a hostname
   before its service is up, which stand-up ordering appreciates.

The road not taken, in one line: nginx `stream` SNI passthrough would have
kept the relay's own ACME untouched (nothing terminates at nginx), but
nginx then cannot terminate admission's hostname on the same socket — with
the certbot decision made, termination at nginx is the simpler
arrangement. And the earlier draft's coupling objection — one renewal
machinery, one silent failure expiring both certs — is now owner-accepted
risk, with the `cert-expiry-watch` above as the mitigation that makes it
tolerable.

Named fallback, not recommendation: if the relay migration misbehaves in
rehearsal, admission on its own port (`https://campaigns.distopik.com:8443`,
relay completely untouched) works with zero relay changes — the port is
baked (§8), so volunteers would never type it. It is the safety net, not
the plan.

### 10.2 The one migrant artifact: the host's report

With durable state on the relay, exactly one thing of value is ever on an
ephemeral box: `raw.json` (and its transient `listening.txt`), written by
the harness on the campaign server. If it dies with the server, the
uploaded client rows on the relay have nothing to reconcile against and the
volunteer's hour is unbankable — the reconciliation needs both copies (§7).
Two mechanisms prevent that, belt and braces:

- **The reaper pulls at session end, not at teardown.** The service holds
  the ssh child for every harness it launches (§3.2 step 12); the moment it
  exits, the reaper copies `raw.json` to `sessions/<sid>/raw.json` on the
  relay. The window in which the report exists only on the ephemeral box is
  one session, minutes to an hour — not a campaign.
- **Stand-down gates on the pull.** The campaign-server teardown script
  refuses to destroy the instance while any session id in the campaign's
  `joins.jsonl` lacks `raw.json` on the relay — excepting sessions whose
  harness died without writing one, which have no report to lose and bank
  nothing, exactly as a crashed client banks nothing (§7). Mutation target:
  `a_campaign_server_never_dies_with_an_unpulled_report` (§13).

A reaper failure (relay↔server network drop at exactly session end) leaves
`raw.json` on the still-running campaign server; the gate then blocks
teardown until the operator re-pulls by hand. Nothing in this path involves
the volunteer.

### 10.3 The 10 GB mint floor (owner requirement, designed precisely)

The owner's rule — *"Refuse to mint sessions when less than 10gb disk"* —
is implemented as a **named constant with its reasoning attached**, not a
literal in a branch:

```python
# Owner, 2026-08-25: "Refuse to mint sessions when less than 10gb disk".
# The relay's 38 GB disk is shared with iroh-relay: filling it would take
# NAT traversal down for every player, not just campaigns. Stopping *new*
# admissions at 10 GB — rather than stopping uploads at 0 — leaves at
# least 160 worst-case (64 MiB-capped, §3.3) uploads of headroom, so every
# session already admitted can still land its evidence.
MINT_FLOOR_BYTES = 10 * 1024**3
```

What it refuses, exactly: **new admissions only** — §3.2 step 3, before
anything is minted, so a refused join leaves no ledger row, no token, no
harness. Checked by `os.statvfs("/var/lib/orrery-admission")` — the upload
store's own filesystem, not `/`, in case they differ — read fresh on every
join request, never cached: a cached reading is a stale guard.

What it must **not** refuse, exactly:

- **Uploads from already-admitted sessions.** The upload endpoint carries no
  floor check at all (§3.3): that evidence is something the campaign already
  paid a volunteer's hour for, and the whole point of holding 10 GB back is
  that every in-flight session can still land. Headroom arithmetic: in-flight
  sessions are bounded by one per campaign (§1), so single digits at any
  realistic scale, against ≥160 worst-case uploads of room — two orders of
  magnitude of slack. A *typical* session's telemetry size is not derivable
  from the tree (no recorded figure exists; only the 64 MiB cap bounds it) —
  measure one real session before tuning anything, and until then the
  worst-case arithmetic governs.
- **Running sessions.** Same rule as `open = no` (§2.1): a session in
  progress belongs to its harness and runs to `--seconds`.

Legibility at both ends, because a silently-not-admitting service looks
identical to a crashed one: the volunteer gets §3.2's `admissions_paused`
sentence — "temporarily unavailable, nothing you did was wrong" — and the
campaign list shows `paused` before they ever click (§3.1); the operator
gets the same reason as a WARNING log line on the systemd journal and in
the listing's `operator_note`, naming the measured free bytes and the
constant, so `journalctl -u orrery-admission` answers "why did admissions
stop" without reading code.

Recovery is the operator's existing drain: pull-and-delete (§7) —
after a verified rsync the service deletes the session's `telemetry.jsonl`
(the bulk) and keeps `client-records.jsonl` and `raw.json` (kilobytes).
`sessions/` is a staging area with a drain, not an archive; the floor is
the backstop for the week nobody drains it, not the mechanism.

## 11. What this amends, as a proposal

Accepted ADRs are normative over this document; the following is **proposed**,
not decided, and needs an ADR (next free number: 0050) accepted by the owner
before implementation reaches the issuer key.

**Proposed D50 — campaign admission overturns D41's offline-issuer core.**
The owner's 2026-08-25 decision (quoted in #476) supersedes: D41's "no HTTP
listener, operator UI, secret store or rate limiter" exclusion (all four are
now in scope in miniature); D41 clause (a)'s invite-delivery ceremony *for
campaign admission* (nickname admission issues no invite codes at all); and
the posture that the issuer credential never resides on a serving host. It
deliberately **retains**: the escrow/restore lifecycle of clause (d)
unchanged (it is what makes the move survivable), the one-hour TTL and wire
format of clause (e), the invite-code machinery for any future gated
campaign, and the ledger file as the allocation record. The amendment must
record, per the epic's instruction, that the reasoning is volunteer time and
error rate, that a compromised admission host can forge session tokens and holds both
reconciliation copies until the operator's pull (§4, stated once), that the
offset is the per-campaign key id (§4), and that nickname admission changes what a banked hour attests for
#240's honest-cohort criterion. D41's clause (f) resolution 3 — the dated
single-host exception with off-host key generation and a restore rehearsal
before first use — carries over as a live obligation on this deployment.

Nothing else accepted is touched: D31's allocation boundary is inherited via
the invite ledger exactly as today, and D33 standing is untouched (`Good` +
`on_probation: true` in every minted token, as the CLI already does).

## 12. Owner decisions this design leaves open

Decided above on technical grounds; the following are *not* mine and are
listed apart from them:

1. ~~Whether the hel1 in D41 is this hel1.~~ **Resolved by the owner,
   2026-08-25** (§10): the relay stays, the 16-core `orrery-hel1-1` is the
   one being replaced — its consumer NVMe disqualifies it as a
   persistent-test host — and admission and tally live on the permanent
   relay while campaign servers come and go. Still open inside that answer:
   **which campaign-server successor** — ephemeral AWS per campaign or a
   better-NVMe Hetzner box. §10 is designed for the ephemeral case, so a
   persistent successor only makes the stand-up ceremony rarer, not
   different.
2. **Admission control beyond one-at-a-time.** Today: anyone reaching the
   service banks hours under any nickname. Rate limiting, a campaign
   capacity, or a shared campaign passphrase are all cheap to add to the
   Python file — whether shakedown wants any of them, and what a banked hour
   should attest for #240, is the owner's read.
3. **Nickname continuity once a real credential exists.** §5 refuses fake
   continuity now; whether `ada` should durably own an account post-#409 is a
   product decision, not a security one.
4. **Whether the operator ever wants auto-assembly.** §7 keeps `assemble` on
   the operator's machine. Running it on the relay against in-place data
   would be mechanical, but it moves the arbiter onto the box that also holds
   the signing key — the owner should look at that trade, not inherit it.
5. **Retention.** How long uploaded telemetry and ledgers persist on the
   relay and in the operator's pull, and whether nicknames
   (personal-adjacent labels, D41 clause (a)'s retention language) get an
   erasure schedule. The 10 GB mint floor (§10.3) bounds the damage of
   deferring this; it does not answer it.
6. **Concurrent sessions per campaign** (harness surgery, §10) — wanted at
   all, and when.
7. **When to run the relay-fronting migration.** The route and the
   certificate arrangement are both decided now (owner: reverse-proxy, and
   certbot with `Reloading` — §10.1), but the migration touches the box
   every player depends on: re-binding the relay, pinning
   `quic_bind_addr`, switching `cert_mode`, and a first forced certbot
   renewal to prove the loop. Scheduling that maintenance window — and
   whether to fall back to the `:8443` separate-port arrangement if the
   rehearsal misbehaves — is the owner's call.

## 13. Mutation-killable tests

For each: what an implementer breaks, and the named test that must die.
Service-side tests live in the same self-test posture as the p4 scripts
(`admission.py --self-test` against a stub harness and the real
`orrery-invite` binaries); client-side ones are ordinary Rust tests.

| Break this | This named test must die |
|---|---|
| Remove the per-campaign flock (§3.2 step 6) so two joins race | `a_busy_campaign_refuses_the_second_join_with_409` |
| Pass a session id to `--require-session` other than the one minted (§3.2 step 10 vs 7) | `the_harness_is_pinned_to_exactly_the_admitted_session_id` |
| Derive the signing `--node` from the slot instead of the request field | `the_token_binds_the_presented_node_not_the_slot` (decode the response token with the real verifier; `claims.node` must equal the request's) |
| Accept an upload for a session absent from `joins.jsonl` | `an_upload_for_an_unadmitted_session_refuses_404` |
| Let any request parameter reach a filesystem path (upload filenames, campaign id) | `hostile_ids_never_escape_the_sessions_directory` (campaign id `../x`, session id with `/` — both 4xx, no file created) |
| Skip nickname validation before the mint subprocess | `a_tab_bearing_nickname_refuses_422_before_minting` (and the ledger gains no row) |
| Serve a malformed campaigns.conf as an empty list instead of last-good | `a_broken_control_file_keeps_serving_the_previous_parse` |
| Let a second, different upload overwrite the first | `a_conflicting_reupload_refuses_409_and_leaves_the_first_bytes` |
| Client: build `CampaignConfig` from the response but drop/alter the token | `the_service_response_round_trips_through_the_join_file` (config built from response == config built from the written `CampaignJoinFileV1`) |
| Client: reverse `--admission-url` / env / baked-default precedence | `admission_url_flag_wins_over_environment_and_default` |
| Upload rows whose `session_id` differs from the admitted one, then assemble | `an_uploaded_row_for_another_session_still_refuses_assembly` (drives the real `p4-campaign-session.sh assemble`, which must exit nonzero — proving the two-copy check, not a restatement of it) |
| Remove the `statvfs` mint-floor check (§3.2 step 3, §10.3) | `the_mint_floor_refuses_new_admissions_below_10gb` (statvfs stubbed under the floor: join must 503 `admissions_paused`, the ledger must gain no row, and the listing must show `paused`) |
| Make the floor guard the upload endpoint too — the too-aggressive defect | `an_admitted_session_still_uploads_below_the_floor` (admit above the floor, drop free space under it, upload: must 204 — refusing paid-for evidence is as much a bug as admitting without room) |
| Skip the reaper's pull, or let stand-down proceed without it (§10.2) | `a_campaign_server_never_dies_with_an_unpulled_report` (stub harness exits leaving raw.json remote-only: stand-down must refuse until `sessions/<sid>/raw.json` exists on the relay) |

The last row is the load-bearing one: it must invoke the actual script, the
same way `p4-campaign-session.sh --self-test` already banks its fixture
through the real `p4-ledger.sh append` (`p4-campaign-session.sh:232-235`), so
that the reconciliation invariant is tested end to end through this new
transport rather than asserted beside it.

## 14. Findings log: stale, corrected, and unevidenced claims

Recorded per house rule — a citation that no longer holds is a finding.

- **"`--join` file support just landed (#475)" — not landed.** PR #475 is
  OPEN (auto-merge enabled, branch `feat/473-join-file`, `mergedAt: null` at
  writing); the working tree has neither `--join` nor `CampaignJoinFileV1`.
  This design depends on it merging and cites its shapes from the PR diff,
  not the tree.
- **hel1's hardware was mis-supplied twice.** First as 1 vCPU / 1.9 GB /
  33 GB free — figures for `ubuntu-2gb-hel1-1` (62.238.59.131), the relay
  box, at that point believed to be a different concern — then corrected to
  16 cores / ~62 GB / 351 GB free at 65.108.197.237. Both are
  operator-supplied and unverifiable in the tree; after the placement
  decision (§10) the relay's figures matter and the 16-core figures size
  nothing.
- **Whether D41's September-2026 decommissioning sentence describes the
  serving box is now unknown** for the same near-identical-hostname reason;
  initially carried as owner question §12.1, since resolved (next bullets).
- Brief citations that **did** verify: `slot_secret` at
  `clients/regolith/src/net.rs:418`; `RelayMode::Disabled` at
  `gates/p1-swarm/src/bridge.rs:71` (the statement spans 63–76);
  `MAX_SESSION_TOKEN_TTL_MS = 3_600_000` at
  `crates/orrery_protocol/src/identity.rs:42`; the invite pre-minting and
  session-id uniqueness story in `crates/orrery_identity/src/invite.rs`
  as cited throughout §1.
- **§12.1 was answered while this document was in review — and the answer
  then improved twice** (owner, 2026-08-25). First: the relay box
  (`ubuntu-2gb-hel1-1`, 62.238.59.131) stays, with basically unmetered
  bandwidth (flagged toward #184 in §10); the 16-core `orrery-hel1-1` is
  replaced on storage grounds (consumer NVMe), possibly by a per-campaign
  ephemeral instance. Second: admission and tally move onto the permanent
  relay box — which dissolved the teardown-survival section an ephemeral
  admission host had briefly required. Third: the 10 GB mint floor (§10.3).
  §2, §3, §4, §7, §8, §10, §12 and §13 were revised accordingly; the
  16-core hardware figures earlier drafts quoted describe **the machine
  being replaced** and must size nothing.
- **The relay-fronting question went through three states, and the middle
  one was wrong.** First reported (coordinator) as a hard blocker — the
  relay owns 80/443. Then corrected to a trade: reverse-proxy mode exists
  (surfaced by the owner via n0-computer/iroh#2341) but was said to be
  mutually exclusive with `enable_quic_addr_discovery` in 1.0.3 — a
  coordinator-supplied claim this document built §10.1's `:8443`
  recommendation on. Then corrected again by the coordinator against the
  source: the guard (iroh-relay-1.0.3 `src/main.rs:585`) requires only that
  a `[tls]` section *exist*, and `quic_bind_addr` is independently bindable
  with default port 7842 (`main.rs:400-406`, `defaults.rs:7`), so the
  owner's route keeps QAD. §10.1 was rewritten to recommend it, with the
  silent-loopback `quic_bind_addr` trap documented; the separate-port
  arrangement was demoted to named fallback. The owner then also decided
  the certificate question §10.1 had left open — certbot owns renewal for
  both hostnames, relay on `CertMode::Reloading` (24 h reload,
  `resolver.rs:19`) — which reversed this document's initial preference
  for SNI passthrough; the coupling it had objected to is now
  owner-accepted risk carried by the `cert-expiry-watch`. The lesson §14
  exists to record: both wrong states entered this document through
  briefs, and only source-reading left it.
- **Operator-supplied, unverifiable in the tree:** the relay box's
  1 vCPU / 1.9 GB / 38 GB, its socket listing (`iroh-relay` on
  80/443/9090), its `/etc/iroh-relay/config.toml` contents, systemd and
  Python 3.14.4. The only in-tree facts about this infrastructure remain
  D41's September-2026 sentence (now understood to describe the retiring
  box) and the `iroh 1.0.3` version pin.
- **Unevidenced and marked as such:** harness RSS/CPU under
  `--impaired --witness` on a candidate campaign server (measure before the
  first public session);
  whether any verifier beyond per-process `--issuer-key` harness invocations
  trusts any issuer key today (nothing found in-tree wiring one into
  persistd/coordinator — the campaign-scoped-key argument in §4 relies only
  on the per-process trust that *is* in the tree); and Bevy
  `bevy_ui_widgets`' API stability across 0.19.x (verified present and
  default-enabled in 0.19.1 from the local cargo cache; its own docs warn
  the API moves).
