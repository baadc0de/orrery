# ADR-0052: R9's client platform set narrows to Windows and Linux; the server's does not

**Status:** Proposed · **Date:** 2026-09-03 · **Decision:** D52

This record is non-normative until accepted. See the [ADR
index](../DECISIONS.md) for precedence, scope, and the complete accepted
decision set. Acceptance is reserved to the owner.

> **Citation status, 2026-09-04.** This record was written against [#1021]
> while it was open. #1021 merged on 2026-09-04 (`ee5d671`); every
> `game/docs/...` link below is now live, and [G10.5]'s merged text is the
> text clause (a) quotes, unchanged. #1021 also added **G11** (scale targets
> and stack boundaries) after this record was written. G11 is re-read in
> [D53]; it was checked against this record too and **touches nothing here** —
> G11 fixes the slice's size and the Rust/Unreal authoring boundary, and this
> record is about which platforms the client ships on. Its "24 invited testers"
> does not answer the roster question in §"What this record could not
> establish"; that stays open. Status stays Proposed.

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
because a proposal that has already made its change is not a proposal.

**Out of scope, and the distinction this record exists to keep sharp:**

- **The determinism matrix.** [D43]'s ring 2 names four targets including
  `aarch64-macos`, and `ci.yml`'s `determinism` job builds it
  (`.github/workflows/ci.yml:930`). That leg compiles `orrery_core`,
  `orrery_protocol`, `orrery_conformance` and `orrery_games`
  (`ci.yml:955`) — no Bevy, no client, no display stack. **A client platform
  decision does not reach it,** and clause (c) refuses to let the two be
  conflated.
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
  excludes as unsupported.

## Context

### 1. R9 is one row doing three jobs

[D1] R9 reads: *"Platforms: **native only** (Windows/Linux/macOS); no WASM path
required"* (`0001-requirements.md:17`). Three separable claims are welded into
it — what the **client** ships on, what the **server and tooling** run on, and
that **no WASM path is required**. [G10.5] changes exactly one of the three,
and says so in its own words: *"Platforms: Windows first, Linux second, macOS
dropped. The server remains engine-free on Linux."* The asymmetry is the
requirement, not an interpretation of it.

### 2. macOS appears in three job families, and only one of them is the client

| Family | Where | What it builds | Client? |
|---|---|---|---|
| `package-client` matrix | `.github/workflows/package-client.yml:43` | `orrery_regolith_client` | **yes** |
| `determinism` matrix | `.github/workflows/ci.yml:930` | headless core spine (`:955`) | no |
| `p4-accumulate` shard | `.github/workflows/nightly.yml:375-378` | `gates/p1-swarm` | no |

The client's *per-PR* Windows and macOS legs were already removed on
2026-08-24; `ci.yml:910-916` records it. So after this decision there is no
macOS client CI left at all, and two macOS legs that have nothing to do with
the client remain. A reader who finds that confusing is reading it correctly,
which is why clause (c) is in the record rather than in a commit message.

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

This is a fact about **one attempt on one day**, not about a roster. What could
not be established from it is stated in the last section, and it is the input
Option 1 needs and does not have.

## Decision

### (a) R9 splits into a client set and a server set

On acceptance, [D1]'s R9 row and its [00-overview] §2 restatement are amended
to read, in substance:

> **R9** — Platforms: **native only**; no WASM path required. **Client:**
> Windows first, Linux second. **Server, services and tooling:** Linux.

Three properties of that wording are deliberate.

1. **The native-only / no-WASM clause is carried verbatim.** It is the half
   [D3] and [02-networking] §2 cite, and nothing in [G10.5] touches it.
2. **"first / second" is [G10.5]'s own priority language and is not
   strengthened.** G10.5 says which platform leads. It does not define a
   support tier, a service level, a release gate, or a promise about what
   Linux receives; neither does this clause, and reading one in would be
   exactly the paraphrase-into-something-stronger this record must not commit.
3. **macOS is absent rather than forbidden.** The row stops committing to it.
   It does not commit against it, and clause (c) shows why that distinction has
   to survive into the text: two macOS legs remain, correctly.

### (b) The change is a client change, and this record does not make it

Nothing in the tree is edited by this record. On acceptance the following are
the work items, listed so the cost is visible before the decision rather than
discovered after it:

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
`:142`, `:364-375`) is deliberately **not** on that list. It is
runtime-dispatched, so it compiles and its unit tests run on every platform
(`:265`, `:310`); deleting it is a separate judgement about dead code, not part
of dropping a build target. `clients/regolith/src/campaign.rs:3534`, which
accepts `apple` among target-triple vendors, is in the same category.

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

### (e) When the asset stops being built is a separate question, and it is the owner's

Clause (a) states what Orrery commits to. **It does not by itself say that the
next release drops the tarball.** This record deliberately does not choose that
date, because the argument for the change and the argument for its timing are
different arguments and only the first is settled by [G10.5]. The choice is
below. Whichever arm is taken is recorded as a dated note against this clause,
not left as a silent scheduling fact.

## Options for the owner

**Option 1 — Narrow R9 and drop the client asset now.** Clause (b)'s six work
items land in one branch; the next `package-client` run publishes two assets.

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
playtest client. (Recommended.)** Clause (a) lands on acceptance; clause (b) is
filed as a work item gated on the Unreal client shipping a playtest build.

*For:* it records the forward commitment exactly as [G10.5] states it, while
what macOS is currently doing — carrying a live volunteer through witnessed
campaign attempts on the **Bevy** client — keeps happening until there is a
replacement. [G10.5] is a requirement about *the client*, and the client it
describes is Unreal 5.8. *Against:* the tree carries a build target the
requirement no longer names, for a period this record cannot bound; someone
must notice when the gate opens. The mitigation is that the gate is [#744]
track D, a named open issue, rather than an intention.

**Option 3 — Narrow R9 and drop macOS entirely, determinism leg included.**
Requires a second record amending [D43] ring 2, an edit to `ci.yml:1039`'s
count, and a decision about `p4-ledger.sh`'s three-platform criterion and
[11-roadmap] `:868`.

*For:* one fewer platform across the whole matrix; hosted-runner minutes,
though `nightly.yml:336` notes those are not billed on a public repository.
*Against:* `aarch64-macos` is the determinism matrix's only Apple libm and its
only non-Linux aarch64 leg, and that matrix exists precisely to catch what one
toolchain agrees with itself about. **[G10.5] does not ask for this.** Listed
so that it is rejected explicitly rather than by omission; not recommended.

## Consequences

- `docs/ci-and-gates.md`'s "three-platform `package-client` matrix" claim
  (`:238-250`) and its "all jobs on hosted runners" sentence naming
  `macos-latest` (`:250`) become false on the day clause (b) lands. They are on
  clause (b)'s list for that reason.
- After clause (b), the repository builds the client on two platforms and
  builds *something* on macOS in two other job families. Clause (c) is the
  record of why that is correct rather than an oversight.
- No release-notes or CHANGELOG artifact needs revision, because none exists.
  The shipped `PLAYTEST.md` is the only per-release statement, and clause (b)
  item 4 covers it.
- Already-published `orrery-regolith-aarch64-macos.tar.gz` assets stay
  published. Nothing here retracts a shipped artifact, and nothing here makes
  an existing macOS install stop working.

## What this record could not establish

Stated rather than guessed, because filling these in by inference is the
failure mode this record is most exposed to:

1. **The tester roster.** There is no roster document in the repository — no
   `docs/playtest*`, no `CONTRIBUTING.md`, nothing enumerating volunteers or
   their platforms. The macOS tester is evidenced only by [#942] and two source
   comments written against that one attempt. **How many testers there are,
   whether the macOS one is still active, and whether they also have a Windows
   or Linux machine are unknown to this record.** Option 2 is recommended
   partly *because* it does not require that answer; Option 1 does, and the
   owner has it where this record does not.
2. **Whether the Bevy client keeps shipping alongside the Unreal client**, or
   is retired when the Unreal client exists. [G10.5] describes the client
   engine set; it does not state a retirement plan for Regolith, and no record
   in either trail settles it. Option 2's gate is worded against "the playtest
   client" — the observable thing — precisely because the retirement question
   is open.
3. **Whether the macOS leg is currently finding defects the other legs are
   not.** [#942] is closed and its macOS defect was one of two on that attempt,
   the other on Linux. This record did not measure defect history by platform,
   and a claim either way would be invented.
4. **The date of the Unreal client**, on which Option 2's gate depends. [#744]
   is open and explicitly propose-only; nothing in the tree schedules it.

[#744]: https://github.com/baadc0de/orrery/issues/744
[#942]: https://github.com/baadc0de/orrery/issues/942
[#1021]: https://github.com/baadc0de/orrery/pull/1021
[D1]: 0001-requirements.md
[D3]: 0003-transport.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
[D53]: 0053-unreal-client-host-scope.md
[00-overview]: ../00-overview.md
[02-networking]: ../02-networking.md
[11-roadmap]: ../11-roadmap.md
[G10.5]: ../../game/docs/00-requirements.md#g10--client-engine-and-content-pipeline
[game ADR-0002]: ../../game/docs/adr/0002-client-engine.md
