# 15 — Asset provenance

The licensing bar for third-party game assets, the record that stands behind
it, and the guard that keeps the record true. Written before the first real
asset arrives (#332); the client that will load these files does not exist yet
(#327). The machinery is normative now; the art can wait.

The guard is [`scripts/asset-provenance.sh`](../scripts/asset-provenance.sh)
and the record is [`assets/provenance.toml`](../assets/provenance.toml). Its
self-test rides the `gates` lane of `scripts/check.sh`, per-commit.

## §1 The bar: redistribution, not use

This repository is **public**. Committing an asset file to it is an act of
**redistribution**, and the two rights routinely differ on asset marketplaces:
a "free" or "royalty-free" download often permits using an asset inside a
compiled product while restricting redistribution of the source asset. Use
permission is not redistribution permission, and a public git repository is
redistribution.

So the bar is not "the download said free". It is:

> This licence **explicitly permits redistributing this file in a public
> repository**, and the entry carries the licence text itself or a stable link
> to it — not a marketplace category name.

"Royalty-free", "editorial use only" and "for use in your projects" are
category names, not licences; they name nothing a reviewer or a downstream
user can read. An entry whose licence reference is a category fails the guard.

## §2 The allowlist

`scripts/asset-provenance.sh` carries the list of licence identifiers accepted
under §1, each with its reason: CC0-1.0, CC-BY-4.0, CC-BY-SA-4.0, MIT,
Apache-2.0, BSD-2-Clause, BSD-3-Clause, OFL-1.1. An identifier off the list
fails the check naming the entry — so a new licence is confronted once, by a
human who has actually read it, after which the check is mechanical forever.

Deliberately excluded, with reasons:

- **No-derivatives variants (CC-BY-ND-\*)** — our pipeline converts formats
  (below), which is an adaptation ND forbids outright. There is no honest way
  to ship a converted ND asset.
- **Non-commercial variants (CC-BY-NC-\*)** — NC terms do not survive contact
  with unknown downstream users of a public repository.
- **Marketplace "royalty-free" categories** — see §1; not licences.

Extending the list is an edit to `LICENCE_ALLOWLIST` in the guard script plus
a paragraph here, in the same change.

## §3 Format and conversion

Bevy 0.19's native path is glTF 2.0 (`GltfAssetLabel::Scene(0)`,
`GltfLoaderSettings::convert_coordinates`; there is no first-party `.obj`/
`.fbx` loader). Marketplace assets commonly arrive as `.fbx`/`.obj`/`.blend`,
so conversion is unavoidable, and **conversion strips whatever metadata the
original carried**. That loss is why provenance lives beside the file in the
repo rather than inside it.

Real assets are committed as **`.glb`** — one file per asset, textures
embedded — so the licence-to-file mapping is unambiguous. A `.gltf` plus loose
buffers/textures splits one asset across several files and several licence
questions; if a case ever genuinely needs it, the extension of the stray-scan
and this section change together, deliberately.

## §4 The manifest

One `[[asset]]` table per file in `assets/`, fields documented at the top of
[`assets/provenance.toml`](../assets/provenance.toml): source URL, sha256 of
the exact bytes, licence identifier (§2), licence text or stable link, author,
retrieval date, and the conversion tool and version that produced the file.
The sha256 binds the entry to the bytes: replacing a file without new
provenance fails the guard, so a manifest entry always describes exactly what
is in the tree.

The invariant is bidirectional, in the shape of #317's `DISK_TELEMETRY_JOBS`
guard, which refuses both a listed job that stops emitting and an unlisted job
that starts:

- every regular file under `assets/` other than the manifest itself must have
  an entry — an asset with no provenance fails, naming the file;
- every entry must name a file that exists — an entry whose file vanished
  fails the other way, naming the path.

Plus three policy clauses the guard enforces: allowlist membership (§2), the
licence-text-or-link rule (§1), and a tree-wide stray scan refusing any
loadable `.glb`/`.gltf` outside `assets/`, because a model dropped into
`crates/` or `docs/` escapes the manifest by construction.

## §5 Weight: strict budget, committed directly

Measured 2026-08-23: the repository is **9.58 MB** on GitHub (`diskUsage`
9581 KB), `.git` is 113 MB locally, `git-lfs` is **not installed**, and
`.gitattributes` covers only line endings. Every CI lane checks this repo out,
so asset weight taxes all eleven nightly jobs and every PR lane.

Of #332's three strategies, this document adopts the first and recommends
staying with it:

1. **Strict budget, committed directly** — *adopted.* Enforced ceilings: **512
   KiB per asset, 2 MiB total** under `assets/` (about a quarter of current
   repository size), checked by the guard, so the budget cannot erode one
   convenient exception at a time. No new tooling anywhere; diffs reviewable
   like any other file.
2. *Git LFS* — rejected for now: it adds a dependency every contributor and
   every CI lane must satisfy, and a missing LFS install produces confusing
   pointer-file failures rather than loud ones.
3. *Fetch at build* — rejected: makes builds network-dependent, which sits
   badly with offline-ish determinism lanes.

A handful of models for one skin fits comfortably under 2 MiB. If real art
ever wants more than the ceilings, that is a new decision against this
section — raise the ceiling consciously or revisit strategy 2 then, not by
accident.

## §6 Fixtures

`assets/fixtures/fixture-empty-scene.{glb,gltf}` are self-authored empty glTF
2.0 scenes generated inside this repository, dedicated CC0-1.0, 84 and 62
bytes. They exist only so the guard asserts something true before the first
third-party asset arrives, and they are never loaded by anything — no client
exists yet (#327). When real assets land they should be deleted, keeping at
least one manifested file so the guard cannot rot into checking nothing.

## §7 Assets stay cosmetic

Art never enters the simulation. Asset geometry must not reach collision
shapes, hitboxes or any ruleset input — #320 constraint 3 makes bot hours and
human hours one code path "modulo input source and rendering", and a mesh that
does gameplay breaks the denominator while moving the four-platform
determinism goldens whenever someone swaps a model. Collision shapes live in
the ruleset. State this again wherever assets get loaded, not only here.

The flip side is genuinely useful: rendering touches none of the four
pipeline-digest trees (`crates/orrery_witness`, `crates/orrery_core`,
`crates/orrery_games`, `gates/p1-swarm`), and `assets/` and this document sit
outside all four. Art work can land during the freeze window without resetting
the banked-hours count, provided those trees stay untouched.

## §8 Out of scope

Extracted game assets of any kind (excluded at #320, unchanged here); audio;
original commissioned art; raster or AI-generated imagery (#332 records why:
the licensing status of generated imagery is unsettled, and marketplace
licences meet §1 more honestly). The client and its asset-path indirection is
#327; when it lands, it must run with `assets/` absent entirely, falling back
to primitives, so a licence problem is never a broken build.

## §9 Why there is no separate CI job

Nothing heavier than the self-test exists to gate: the check is static over
committed bytes and runs in about a second, so it rides the existing `gates`
lane rather than earning a job. A dedicated job would also need teaching to
`scripts/gate-status.sh`'s discovery (`jobs:` keys with no matching trio exit
2), for no additional coverage. Revisit only if a runtime asset pipeline
appears.
