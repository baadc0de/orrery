# ADR-0052: R9 splits into a client set (Windows first, Linux second, macOS kept) and a server set (Linux)

**Status:** Proposed · **Date:** 2026-09-03 · **Revised:** 2026-09-04 · **Decision:** D52

This record is non-normative until accepted. See the [ADR
index](../DECISIONS.md) for precedence, scope, and the complete accepted
decision set. Acceptance is reserved to the owner.

> **Revision, 2026-09-04.** As first written on 2026-09-03 this record was
> titled *"R9's client platform set narrows to Windows and Linux"* and
> recommended dropping the macOS client asset once the Bevy client stopped
> being the playtest client. **The owner reversed that on 2026-09-04**
> (comment on [#1022]): the macOS drop was premature, and **macOS stays a
> supported client platform.** The owner's reason, in the owner's words: a
> substantial part of the interest group and the friends who will be testing
> carry macOS, and that remains true regardless of how good Unreal's support
> for it is — dropping it would cut off the people most likely to actually
> play. This revision records that decision as taken. It keeps the
> Windows-first / Linux-second *ordering*, which is not disputed; it keeps the
> cost analysis of §3 and clause (b) exactly as written, because it is
> accurate and now argues the other way; and it answers the roster question
> the original record left open, in the way the owner chose (clause (e)).
> Status stays Proposed: the R9 amendment in clause (a) still takes effect
> only on acceptance.

> **Citation status, 2026-09-04.** This record was written against [#1021]
> while it was open. #1021 merged on 2026-09-04 (`ee5d671`); every
> `game/docs/...` link below is now live. [G10.5]'s merged text is the text
> clause (a) quotes — including the words *"macOS dropped"*, which the owner's
> 2026-09-04 decision reverses; §1 says how this record reads that. #1021 also
> added **G11** (scale targets and stack boundaries) after this record was
> written. G11 is re-read in [D53]; it was checked against this record too and
> **touches nothing here** — G11 fixes the slice's size and the Rust/Unreal
> authoring boundary, and this record is about which platforms the client
> ships on.

**Citation convention, established here.** *Mothership* is a separate project
with its own decision trail under `game/docs/adr/`, co-located because Orrery
has to grow to support it. An Orrery change made to satisfy a `G`-numbered game
requirement is **an Orrery ADR that cites the G number** — not a game record
reaching into Orrery's trail, and not an Orrery record restating game
requirements as its own. This is the first of the two records [game ADR-0002]'s
"Consequences for Orrery" asks for; the second is [D53].

**Supersedes on acceptance:** nothing. It **amends** [D1]'s R9 row — the
platform half of it, and only for the client — and the restatement of that row
in [00-overview] §2. While Proposed it amends nothing and edits neither: the
overview table still reads "Native only (Windows/Linux/macOS)", deliberately,
because a proposal that has already made its change is not a proposal. After
the 2026-09-04 decision the amendment changes the row's *shape* (a client set
with an effort order, and a separate server set), not its membership.

**Out of scope, and the distinction this record exists to keep sharp:**

- **The determinism matrix.** [D43]'s ring 2 names four targets including
  `aarch64-macos`, and `ci.yml`'s `determinism` job builds it
  (`.github/workflows/ci.yml:930`). That leg compiles `orrery_core`,
  `orrery_protocol`, `orrery_conformance` and `orrery_games`
  (`ci.yml:955`) — no Bevy, no client, no display stack. **A client platform
  decision does not reach it,** and clause (c) refuses to let the two be
  conflated. That was true when the proposal was to drop the client leg, and
  it is equally true now that the client leg is kept: keeping macOS in the
  client set is not what keeps `aarch64-macos` in the determinism matrix.
- **The P4 player-hour accumulation leg** (`.github/workflows/nightly.yml:375-378`),
  which builds `gates/p1-swarm`, not the client, and which
  `scripts/p4-ledger.sh` requires by name in a hard-coded three-platform
  criterion (`:432`, `:1335`).
- **The no-WASM half of R9**, which is the half every consumer of R9 actually
  leans on ([D3]'s rejection of matchbox-as-primary; [02-networking] §2's
  verdict row, "only wins if WASM parity mattered; R9 says native-only").
  Clause (a) carries it verbatim.
- **The Unreal host's own design**, which is [D53]'s.
- **Intel macOS**, which R9 never separated and which [D43] ring 2 already
  excludes as unsupported. "macOS" in this record means Apple silicon
  (`aarch64-macos`), as it does everywhere else in the tree.
- **[#1051]'s cause.** That issue is cited below as evidence about *whether*
  macOS belongs in the tested set; this record states nothing about *why* two
  uploads failed, because that is not established.

## Context

### 1. R9 is one row doing three jobs

[D1] R9 reads: *"Platforms: **native only** (Windows/Linux/macOS); no WASM path
required"* (`0001-requirements.md:17`). Three separable claims are welded into
it — what the **client** ships on, what the **server and tooling** run on, and
that **no WASM path is required**. [G10.5] touches exactly one of the three,
and says so in its own words: *"Platforms: Windows first, Linux second, macOS
dropped. The server remains engine-free on Linux."* The asymmetry between
client and server is the requirement, not an interpretation of it.

[G10.5] makes two distinct statements about the client, and the owner's
2026-09-04 decision treats them differently:

- **The ordering — Windows first, Linux second — stands.** It says where
  effort goes first. It is not disputed by the decision and it is not
  strengthened by this record: it defines neither a support tier nor a
  release gate, and it says nothing about what any platform is denied.
- **The drop — "macOS dropped" — is reversed.** The owner's stated ground is
  not about Unreal's macOS support at all; it is about who the testers are.
  *Ordering is not dropping*: a platform can be third in effort order and
  still be built, join-tested and shipped, which is exactly what §3 and §4
  show macOS to be today.

This record does not edit `game/docs/00-requirements.md`. The words *"macOS
dropped"* remain in [G10.5]'s merged text as of this revision, and reconciling
that text with the owner's decision is an edit to the game trail, by the
owner, not something an Orrery record can do by citing it. Until that edit
lands, the owner's comment on [#1022] is the authority this record cites for
the reversal.

### 2. macOS appears in three job families, and only one of them is the client

| Family | Where | What it builds | Client? |
|---|---|---|---|
| `package-client` matrix | `.github/workflows/package-client.yml:43` | `orrery_regolith_client` | **yes** |
| `determinism` matrix | `.github/workflows/ci.yml:930` | headless core spine (`:955`) | no |
| `p4-accumulate` shard | `.github/workflows/nightly.yml:375-378` | `gates/p1-swarm` | no |

The client's *per-PR* Windows and macOS legs were already removed on
2026-08-24; `ci.yml:910-916` records it. The `package-client` leg is therefore
the **only** macOS client build left in the tree, and under the 2026-09-04
decision it stays. Had the drop gone ahead there would have been no macOS
client CI at all while two macOS legs with nothing to do with the client
remained — a reader who found that confusing would have been reading it
correctly, which is why clause (c) is in the record rather than in a commit
message.

### 3. What the client leg actually costs, and what it actually buys

**One line builds it.** `package-client.yml:43` is the whole macOS-specific
surface of that workflow: `- { os: macos-latest, label: aarch64-macos, binary:
orrery_regolith_client }`. Everything after it is `runner.os`-conditional and
macOS falls into the generic Unix path — packaging at `:187`, the
`shasum -a 256` branch at `:150-154`, the
`dist/orrery-regolith-aarch64-macos.tar.gz` name at `:191`. The workflow never
names a target triple; it builds host-native (`:100-105`).

**Nothing is signed.** The single occurrence of notarization in the tree is
prose saying it does not happen: `clients/regolith/PLAYTEST.md:32` tells the
volunteer the build is not notarized. There is no `codesign`, no `xcrun`, no
`APPLE_ID` secret anywhere in `.github/`, `scripts/` or `clients/`. **There is
no signing or notarization spend to recover by dropping macOS,** which removes
the usual first argument for dropping an Apple target.

**The leg join-tests.** `package-client.yml:246-252` gives non-Linux runners
`--campaign shakedown --join-timeout-secs 900`, so the macOS build performs a
real campaign join against the deployed lobby before publication. The comment
at `:246` says why: *"Windows and macOS have never join-tested the artifact
they publish, which is how #769 reached a volunteer."* Dropping the leg drops
that check for macOS and for nothing else.

### 4. It ships today, and a human was flying it yesterday

`playtest-2026-09-03b` — the current **Latest** release, published by
`github-actions[bot]` at 2026-09-03T12:46:26Z — carries exactly three assets,
one of them `orrery-regolith-aarch64-macos.tar.gz`. There is no CHANGELOG and
no per-release notes file; releases are cut with `--generate-notes`
(`package-client.yml:318-320`), so `README.md:49` and
`clients/regolith/PLAYTEST.md:20-42` are the entire in-repo statement of what a
release ships.

**A human volunteer flew a witnessed `shakedown` seat on macOS on 2026-09-02.**
[#942] records two humans on one full attempt; its first defect is headed
*"(macOS peer, seat 6)"*, with 776 campaign overlay rows and 45,268 host-side
ticks. The tree corroborates it twice, in comments written against that
attempt: `clients/regolith/src/lib.rs:2973` and `:3211`.

This is a fact about **one attempt on one day**, not about a roster. On
2026-09-03 the record stopped here and listed what it could not establish
from one attempt. §5 and clause (e) now carry what the ledger as a whole
says, which is the evidence the roster question is answered from.


### 5. What the ledger and the 2026-09-04 attempt add

Two facts arrived after the record was first written, and both bear on the
decision rather than merely on its timing:

- **macOS volunteers have banked 0.281 of the 1.733 player-hours currently in
  the ledger** ([#1022] comment; [#1051]). That is not a projection about who
  might test; it is what the banked rows say about who has.
- **A MacBook is being made available over ssh for automated testing** ([#1022]
  comment). Until now macOS was the one client platform with no machine behind
  it other than a volunteer's own; a macOS defect could be reproduced only
  when a volunteer had the spare time. That stops being true.

And one defect, cited for what it shows about coverage and for nothing else:
**both macOS clients in the 2026-09-04 attempt recorded valid signed sessions
and never uploaded them, while both non-macOS uploads in the same attempt
succeeded** ([#1051]). The rows were valid — signed, non-zero
`banked_minutes`, `client_rev` and `ruleset_version` matching the pin — and
were hand-carried into the ledger; only the upload leg failed. It is the same
class as the HTTP 413 defect ([#1002]): a volunteer plays, the client records
everything properly, and the evidence never reaches the server. **The cause is
under investigation on [#1051] and is not stated here.** What the record does
take from it is this: an untested platform is exactly where a defect of this
class lives unnoticed, and the argument it makes is for keeping macOS in the
tested set, not for dropping it.

## Decision

### (a) R9 splits into a client set and a server set; macOS is in the client set

On acceptance, [D1]'s R9 row and its [00-overview] §2 restatement are amended
to read, in substance:

> **R9** — Platforms: **native only**; no WASM path required. **Client:**
> Windows first, Linux second, macOS kept. **Server, services and tooling:**
> Linux.

Three properties of that wording are deliberate.

1. **The native-only / no-WASM clause is carried verbatim.** It is the half
   [D3] and [02-networking] §2 cite, and nothing in [G10.5] touches it.
2. **"first / second" is [G10.5]'s own priority language and is not
   strengthened.** G10.5 says which platform leads. It does not define a
   support tier, a service level, a release gate, or a promise about what
   Linux receives; neither does this clause, and reading one in would be
   exactly the paraphrase-into-something-stronger this record must not commit.
   The same restraint applies to macOS's position after the two: it is an
   *effort order*, and this record does not turn it into a statement about
   what macOS is denied.
3. **macOS is kept, by owner decision of 2026-09-04, not by silence.** The
   original text of this clause left macOS *absent rather than forbidden*.
   That is no longer the position: the row names it. The distinction clause
   (c) guards — that the client set and the determinism matrix are separate
   questions — survives unchanged, because macOS's presence in the client set
   and `aarch64-macos`'s presence in [D43] ring 2 are established by different
   records for different reasons.

### (b) What dropping the client leg would cost, kept as written; none of it is done

This clause was originally the work list for dropping the leg. **Under the
2026-09-04 decision none of these items lands.** The list is kept exactly as
written because it is accurate, and because it is now the record's clearest
argument in the other direction: the drop was cheap in the workflow — one
matrix line, item 1 — and expensive in the gates — items 2 and 3, where
`gate-status.sh` **asserts** a three-runner matrix and
`package-artifact-smoke.sh` iterates the three labels by name in a self-test
that runs on every commit. Those assertions were written so that a platform
could not vanish from the shipped set silently. They are doing that job now.
The expense of removing them is a reason not to, not an obstacle to be cleared.

1. `.github/workflows/package-client.yml:43` — remove the matrix entry.
2. `scripts/gate-status.sh:890` — the `package-client` gate **asserts** that
   the string `macos-latest` is present in that workflow. Removing the entry
   without removing this assertion turns the gate red. (`:888` and `:889` are
   its Windows and Ubuntu siblings and stay.)
3. `scripts/package-artifact-smoke.sh` — the label tables and the script's own
   self-tests iterate `x86_64-linux x86_64-windows aarch64-macos` (`:832-833`,
   `:843`, `:888`) and die by name when the macOS asset name drifts; asset and
   archive names at `:102`, `:111`; the `Darwin) echo macos` arm at `:132`; the
   `~/Library/Application Support/Orrery/Regolith` path at `:124`, `:689`,
   `:715`. This script's `--self-test` runs in `./scripts/check.sh gates` on
   every commit, so this item is not optional and not deferrable.
4. `clients/regolith/PLAYTEST.md:28-34` — the Apple-silicon volunteer
   instructions. `package-client.yml:141` copies this file into every archive
   as its `README.md` and `:167-169` asserts the README names the shipped
   asset, so this edit is load-bearing rather than cosmetic.
5. `README.md:49`, `clients/regolith/README.md:25`,
   `docs/ci-and-gates.md:238-250` — the three-platform prose.
6. `clients/regolith/src/identity.rs:56`, `clients/regolith/src/paths.rs:84-86`
   and `:88` — the only three `cfg(target_os = "macos")` sites in the
   repository. There are **zero** under `crates/` and **zero** under `gates/`.

The `Platform::MacOs` runtime branch in `clients/regolith/src/paths.rs` (`:71`,
`:142`, `:364-375`) was deliberately **not** on that list. It is
runtime-dispatched, so it compiles and its unit tests run on every platform
(`:265`, `:310`); deleting it would have been a separate judgement about dead
code, not part of dropping a build target. `clients/regolith/src/campaign.rs:3534`,
which accepts `apple` among target-triple vendors, is in the same category.
Both stay, and nothing about them is now in question.

### (c) The determinism and P4 legs are untouched, and conflating them is expensive

`ci.yml:1039` fails the `determinism-verdict` job when fewer than **3**
non-baseline reports arrive, and `:1035-1038` says why in terms: *"Keep this
count in step with the matrix above, or a dropped leg passes silently."*
Removing `ci.yml:930` therefore turns that job permanently red **and**
contradicts [D43] ring 2, which names `aarch64-macos` among its four targets.
[D43] is Accepted; this record amends no accepted record but [D1], and amends
[D1] only in its client half.

Likewise `scripts/p4-ledger.sh` hard-codes `["linux","windows","macos"]`
(`:1335`, criterion at `:432`) and its self-test asserts that a Linux-only
ledger prints `macos: 0 hours — MISSING` (`:376-377`); [11-roadmap] §P4's exit
criterion (`:868`) is *"identical core replays on Windows/Linux/macOS binaries
every commit"*, which is the determinism claim and not a client claim.

**A future decision may drop those legs. It is not this one, and it would be a
different record amending [D43].**

### (d) The server's platform story does not change

[G10.5]'s second sentence — *"The server remains engine-free on Linux"* — is
already true, and is restated here only so that acceptance cannot be read as
having moved it. `docs/09-services-and-ops.md` contains no macOS client
content; its only Apple references are FoundationDB documentation URLs
(`:102`, `:105`, `:324`). No service, gate workspace or tool changes platform
under this record.


### (e) Platform coverage is derived from banked rows, not from a roster

The original record listed the tester roster as its first open question —
how many testers there are and which platforms they carry — and recommended
its option partly because that option did not need the answer. **The owner
answered the question differently than it was asked: coverage is derived from
the banked rows, and no roster document is wanted.**

The evidence already exists. Every banked interval is a `SessionRecord`
(`clients/regolith/src/session.rs:184-198`) carrying a coordinator-issued
`session_id` — the ledger's `identity.human_session_id` for humans — and a
`platform_triple`, the Rust target triple stamped at build time
(`clients/regolith/src/campaign.rs:2667`, `build.rs:39`). `scripts/p4-ledger.sh`
refuses a row whose `platform_triple` does not equal the host report's target
(`:1076`) and requires all three platforms by name in its criterion (`:432`,
`:1335`). So "which platforms are being played, by how many distinct sessions,
for how many banked hours" is a query over rows the ledger already holds and
already validates — the 0.281-of-1.733 figure in §5 is that query's answer for
macOS today — and a roster would be a second, hand-maintained copy of it that
could only drift.

Two consequences follow for how the record is read:

1. The former open question is closed, not by a roster but by pointing at
   the rows. A later reader who wants the platform split asks the ledger.
2. Per-platform coverage is therefore a *measured* quantity that can go to
   zero, which is precisely the case the ledger's `macos: 0 hours — MISSING`
   self-test (`p4-ledger.sh:376-377`) exists to surface. A platform that
   stops being played will show it in the evidence before anyone has to
   decide anything about it.

## Options that were put to the owner, and the decision taken

**Decision, 2026-09-04: macOS stays in the client set.** None of the three
options below was taken as written. Option 1 and Option 2 differ only in *when*
the macOS asset stops being built; the owner's answer is that it does not, and
the reasons are the ones the original record itself supplied — the leg costs
one matrix line and no signing budget (§3), a volunteer was flying it (§4),
the gates make removing it expensive (clause (b)), and testers are the scarce
resource — plus the two facts in §5. The options are kept below as the record
of what was considered, with their original for/against unchanged; the
recommendation marker is struck.

**Option 1 — Narrow R9 and drop the client asset now.** Clause (b)'s six work
items land in one branch; the next `package-client` run publishes two assets.
**Not taken.**

*For:* the tree stops carrying a platform the requirement no longer names, and
the two coupled gates (`gate-status.sh:890`, `package-artifact-smoke.sh`) are
edited once, together, while the reason is fresh — they are exactly the kind of
assertion that becomes a mystery six weeks later. *Against:* it removes the
seat a volunteer flew on 2026-09-02 ([#942]) and the publication join-test at
`package-client.yml:246-252`, and it buys nothing until the Unreal client
exists — which it does not, and which [#744] is still explicitly
`propose-only` about. Testers are the scarce resource; this option spends one
to retire a build that costs one matrix line and no signing budget.

**Option 2 — Narrow R9 now; drop the asset when the Bevy client stops being the
playtest client.** Clause (a) lands on acceptance; clause (b) is filed as a
work item gated on the Unreal client shipping a playtest build. **Recommended
in the 2026-09-03 text; not taken.**

*For:* it records the forward commitment exactly as [G10.5] states it, while
what macOS is currently doing — carrying a live volunteer through witnessed
campaign attempts on the **Bevy** client — keeps happening until there is a
replacement. [G10.5] is a requirement about *the client*, and the client it
describes is Unreal 5.8. *Against:* the tree carries a build target the
requirement no longer names, for a period this record cannot bound; someone
must notice when the gate opens. The mitigation was that the gate is [#744]
track D, a named open issue, rather than an intention. *Why it was not
taken:* the owner's ground is that the people who will test carry macOS, and
that does not change when the client engine does — so a gate on the Unreal
client is a gate on the wrong event.

**Option 3 — Narrow R9 and drop macOS entirely, determinism leg included.**
Requires a second record amending [D43] ring 2, an edit to `ci.yml:1039`'s
count, and a decision about `p4-ledger.sh`'s three-platform criterion and
[11-roadmap] `:868`. **Not taken.**

*For:* one fewer platform across the whole matrix; hosted-runner minutes,
though `nightly.yml:336` notes those are not billed on a public repository.
*Against:* `aarch64-macos` is the determinism matrix's only Apple libm and its
only non-Linux aarch64 leg, and that matrix exists precisely to catch what one
toolchain agrees with itself about. **[G10.5] does not ask for this.** Listed
so that it is rejected explicitly rather than by omission.

## Consequences

- **Nothing in the tree changes on acceptance except the R9 row and its
  [00-overview] §2 restatement.** `package-client.yml:43` stays;
  `gate-status.sh:890` and `package-artifact-smoke.sh`'s three-label self-test
  stay and keep doing what they were written to do. `docs/ci-and-gates.md`'s
  "three-platform `package-client` matrix" claim (`:238-250`) and its sentence
  naming `macos-latest` (`:250`) stay true; `README.md:49`,
  `clients/regolith/README.md:25` and `clients/regolith/PLAYTEST.md:28-34`
  stay as they are.
- The next `package-client` run publishes three assets, as the current one
  did. Already-published `orrery-regolith-aarch64-macos.tar.gz` assets stay
  published, and every macOS install keeps working — which was true under the
  original text too, and is now simply the steady state.
- The repository builds the client on three platforms and builds *something*
  on macOS in two other job families. Clause (c) remains the record of why
  those are separate questions: a future record could drop `aarch64-macos`
  from [D43] ring 2 without touching the client set, and this record has
  kept macOS in the client set without saying anything about ring 2.
- **macOS gains a machine.** With the MacBook reachable over ssh, [#1051]'s
  reproduction and any later macOS-specific defect can be captured on demand
  rather than waiting for a volunteer. How that machine is wired into the
  gates — whether as a self-hosted runner, an ad-hoc target for
  `package-artifact-smoke.sh`, or something else — is not decided here and
  is not this record's to decide.
- Platform coverage is a ledger query (clause (e)), so no roster document is
  created and none is owed.
- No release-notes or CHANGELOG artifact needs revision, because none exists.

## What this record could not establish

Stated rather than guessed, because filling these in by inference is the
failure mode this record is most exposed to. The original list had four
items; the first is now answered and the rest are re-read against the
decision.

1. ~~**The tester roster.**~~ **Answered by clause (e).** There is still no
   roster document in the repository, and under the owner's decision there is
   not going to be one: platform coverage is computed from the banked rows'
   `platform_triple` and session identity. The number this record could not
   state on 2026-09-03 is stated in §5 from that evidence — 0.281 of 1.733
   player-hours on macOS — and the same query answers it again whenever it is
   asked.
2. **Whether the Bevy client keeps shipping alongside the Unreal client**, or
   is retired when the Unreal client exists. [G10.5] describes the client
   engine set; it does not state a retirement plan for Regolith, and no record
   in either trail settles it. This stays open. It no longer gates anything
   in this record — Option 2's "when the playtest client changes" trigger was
   the only thing that depended on it, and Option 2 was not taken — but [D53]
   still cites it as open, correctly.
3. **Whether the macOS leg is currently finding defects the other legs are
   not.** This record still has not measured defect history by platform, and
   a rate either way would be invented. What can be said without inventing
   anything is narrower: the two defects that witnessed multi-human attempts
   have so far named as platform-specific — [#942]'s *"(macOS peer, seat 6)"*
   and [#1051]'s two failed uploads — are both on macOS, and both were found
   because a macOS build existed to be flown. That is evidence
   about the value of testing the platform, not a claim about its defect
   rate, and it is the reason §5 cites [#1051] at all.
4. **The date of the Unreal client.** [#744] is open and explicitly
   propose-only; nothing in the tree schedules it. Under the original text
   this date bounded Option 2's gate. It no longer bounds anything here.
5. **The cause of [#1051].** Not established as of this revision, and not
   this record's to establish. A cause stated here before the reproduction
   on the MacBook has been captured would be exactly the "fix without a
   reproduction" that issue warns against.

[#744]: https://github.com/baadc0de/orrery/issues/744
[#942]: https://github.com/baadc0de/orrery/issues/942
[#1002]: https://github.com/baadc0de/orrery/issues/1002
[#1021]: https://github.com/baadc0de/orrery/pull/1021
[#1022]: https://github.com/baadc0de/orrery/pull/1022
[#1051]: https://github.com/baadc0de/orrery/issues/1051
[D1]: 0001-requirements.md
[D3]: 0003-transport.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
[D53]: 0053-unreal-client-host-scope.md
[00-overview]: ../00-overview.md
[02-networking]: ../02-networking.md
[11-roadmap]: ../11-roadmap.md
[G10.5]: ../../game/docs/00-requirements.md#g10--client-engine-and-content-pipeline
[game ADR-0002]: ../../game/docs/adr/0002-client-engine.md
