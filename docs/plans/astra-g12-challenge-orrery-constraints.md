# Orrery-side constraints for the G12 content-pipeline challenge

> Written 2026-09-06 for **gpt6 astra**, which the owner has given ownership of
> the `game/` trail and a brief to challenge G12
> (`game/docs/adr/0004-content-pipeline.md`) — the mesh generation approach
> **and what it implies downstream**.
>
> The outgoing session's handover covers the `game/` side: what is on the
> branch, what plateaued, and the choices made around the current generator.
> **This document covers the other side of the seam** — what is already built
> and load-bearing in the Orrery trail (`crates/`, `gates/`, `scripts/`,
> `clients/`, `docs/`), which a content pivot can touch without meaning to.
>
> Every `path:line` here was opened before being cited. This corpus drifts
> within hours; re-verify before acting on any of it.

## 1. The thing most likely to be conflated: there are two determinism claims

They live at different stages, are enforced by different mechanisms, and mean
different things. A pivot that "keeps determinism" must say *which*.

| | **Content-record determinism** (`game/` trail) | **Cook-layer determinism** (Orrery trail) |
|---|---|---|
| Claim | The *program* reproduces, not the process | The *bytes* reproduce, across independent editor saves |
| Artifact | build list / `zones.json` + `choices.json` | the cooked `.uexp` / `.umap` |
| Mechanism | provenance JSON per stage (prompt, refs by sha256, model, response id, output sha256) | canonical GUID rewriting before `SavePackage` |
| Holds for | local generators (TRELLIS, Blender) bit-for-bit; hosted stages best-effort | every entity in the cook |
| Does **not** hold for | the critic loop — non-deterministic across runs | the *editor-save* layer, which cannot reach zero |
| Landed in | the outgoing session's pipeline work | **#1082**, merged as PR #1095 |

**Cook-layer determinism, precisely.** Two independent editor saves of the same
seed, each cooked by the book, now produce byte-identical output:

```
Body_2.uexp: 4082109 / 4082109 bytes, sha1 EQUAL, 0 differing bytes in 0 runs
Body_2.umap:   27073 /   27073 bytes, sha1 EQUAL, 0 differing bytes in 0 runs
```

(`docs/spikes/1082-canonical-guids/results/cooked-same-seed-diff-after.txt`.)
Before the change those were 275 B in 31 runs and 20 B respectively. The pass
costs 0.1–0.4 ms per body and cook times are unchanged within noise. A
different seed still differs by 4.46 MB under a different content key, so
nothing collapsed into a constant.

**What it canonicalises, and why that list matters to a pivot.** In object-path
order from the seed stream: `ActorGuid`/`ActorInstanceGuid`,
`FCompiledSectionBuildInfo::BuildKey`, `OriginalMapBuildDataId` and its derived
`MapBuildDataId` (6 LODs across 8 static-mesh components — the cook writes
*both*), `UInstancedStaticMeshComponent::InstancingRandomSeed` (re-rolled in
PreSave with `FMath::Rand()`), `UStaticMesh::LightingGuid`,
`ULevel::LevelBuildDataId`. `UBodySetup::BodySetupGuid` is deliberately
**content-derived, not seed-derived**, because the engine uses it as the DDC
change indicator — seeding it would collide across distinct bodies.

**The editor-save layer cannot reach zero, and #1082 does not claim it does.**
3,859 B / 569 runs remain: FText localisation keys re-minted per save
(`TextHistory.cpp:882`), `LocalizationId`, `PersistentGuid`, three
`FEditorBulkData` ids, and a `DateModified` tag (`World.cpp:10318`). All
editor-only and stripped by the cook — but `DateModified` alone makes
editor-layer determinism impossible without an engine change.

**So the question for a pivot is narrow:** does a new generator still hand the
cook a scene whose non-determinism is confined to that list? If it introduces a
new per-save identifier the pass does not rewrite, the cooked bytes stop being
identical and #1082's guarantee is silently lost. The pass is a fixed list, not
a general canonicaliser.

## 2. The hard invariant: assets stay cosmetic

`docs/15-asset-provenance.md` §7, and this one is not negotiable by a content
decision:

> Art never enters the simulation. Asset geometry must not reach collision
> shapes, hitboxes or any ruleset input — #320 constraint 3 makes bot hours and
> human hours one code path "modulo input source and rendering", and a mesh
> that does gameplay breaks the denominator while moving the four-platform
> determinism goldens whenever someone swaps a model. Collision shapes live in
> the ruleset.

The P4 gate counts bot and human hours toward one 500-hour total precisely
because they are the same code path. A mesh that participates in gameplay makes
those two populations incomparable, and the gate stops measuring what it claims
to.

**A generative pipeline is not exempt from this, and is arguably more exposed
to it** — a generator that produces collision-ready geometry invites exactly
the coupling the rule forbids. If a pivot wants generated geometry to inform
gameplay, that is an Orrery-side ADR, not a content decision.

## 3. The flip side, which is genuinely useful

Same section:

> rendering touches none of the four pipeline-digest trees
> (`crates/orrery_witness`, `crates/orrery_core`, `crates/orrery_games`,
> `gates/p1-swarm`), and `assets/` and this document sit outside all four. Art
> work can land during the freeze window without resetting the banked-hours
> count, provided those trees stay untouched.

`pipeline_id()` (`scripts/p4-ledger.sh:1081-1095`) hashes those four subtrees;
hours banked under one hash group separately from hours banked under another.
**So content work is freeze-window-safe by construction — as long as it stays
out of those four trees.** That is a real freedom worth preserving in whatever
shape the pivot takes; it is also lost the moment content reaches into a
ruleset.

For calibration on what crossing that line costs: PR #1115 (S6.c) touched
`gates/p1-swarm` and moved the digest, splitting the one existing banked
attempt (1.25 bot + 0.483 human hours) from everything after it. The owner
authorised it knowingly on 2026-09-06. At 1.73 hours against a 500-hour target
that was cheap; it will not stay cheap.

## 4. Where the Orrery position and G12 are already in tension

`docs/15-asset-provenance.md` §8 lists as **out of scope**:

> raster or AI-generated imagery (#332 records why: the licensing status of
> generated imagery is unsettled, and marketplace licences meet §1 more
> honestly)

G12's pipeline generates imagery and geometry. That is not a contradiction —
§15 governs what is *committed and redistributed* through the Orrery asset
path, and the outgoing session kept mesh bytes out of the repo entirely
(manifests with sha256 and licence are the versioned input, G12.8; generated
parts carry licence "project-owned"). But the two documents were written
against different assumptions and **have never been reconciled in writing**.

A challenge to G12 is the right moment to say plainly which of these holds:

- generated assets never enter the redistributed path, so §8 is untouched; or
- §8 needs amending, in which case it is an Orrery ADR with a licensing
  question behind it that is explicitly *unsettled*, not merely undecided.

Do not resolve it silently by shipping a pipeline that assumes one.

## 5. What else on the Orrery side a content pivot touches

- **The asset store is private by default (G12.12) and does not exist yet.** A
  Hetzner bucket is intended (G12.13); AWS was unblocked on 2026-09-06 and
  #341's adjudication proposes S3. The owner has parked #341 as not the
  bottleneck. Staging is all workstation-local under `~/assets`.
- **`clients/regolith` must run with `assets/` absent entirely**, falling back
  to primitives (`docs/15-asset-provenance.md` §8, landed as #327). *A licence
  problem must never be a broken build.* Any pipeline that makes assets
  mandatory to start breaks this.
- **The client is the live one.** It was reworked twice this week — #1111
  converged its campaign driver onto the `SimulationHost` seam and #1114 added
  `seat_at`. Read the current shape rather than an older one.
- **Testers are the scarce resource** — 1–2 playtest shots a day, and nobody
  has flown `playtest-2026-09-04` yet. The owner's standing priority
  (2026-09-06) is to harden against everything a machine can find before
  spending one. A content pivot that lands untested art in the tester path
  spends that resource.

## 6. What is genuinely open, so the challenge does not overcorrect

- **The prong question (D53) is unsettled and its record is stale.** Clause (e)
  says no Windows report exists; one landed 2026-09-04 and says SIDECAR STANDS.
  PR #1117 drafts the amendment, unaccepted at time of writing. Note the
  verdict is **conditional on `timeBeginPeriod(1)`** — the committed
  default-resolution companion renders OWNER'S CALL, with a 2.6806% frame drop
  rate against a ≤0.1% band.
- **No Windows in-process measurement exists and no available box can take
  one.** Neither the Linux workstation nor the MacBook is Windows.
- **The App / non-App fork is untouched**, and GD3's actual pool-capped,
  driver-connected configuration was measured by neither spike.
- **`orrery_unreal_observer` is not in `core-gates.sh`'s
  `DECLARED_BEVY_FREE_CRATES`** — the gate discovers `lib`-kind crates and it
  is `staticlib`+`rlib`. An `nm` archive-purity assertion enforces it instead.

## 7. Summary for the challenge

Three things to preserve or explicitly renegotiate, in descending order of how
expensive they are to lose:

1. **Assets stay cosmetic.** Losing this breaks the P4 gate's denominator.
2. **Content stays out of the four pipeline-digest trees.** Losing this makes
   every content change reset banked-hour grouping.
3. **The cook's non-determinism stays confined to #1082's rewritten list.**
   Losing this silently un-does byte-identical cooks.

And one to settle rather than inherit: **whether generated assets are inside or
outside `docs/15-asset-provenance.md` §8.**
