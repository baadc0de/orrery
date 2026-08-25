# A9 — Bevy and Unreal integration boundaries (#405)

**Parent:** #395 · **Source brief:** `docs/plans/ruleset-ecs-migration-brief.md`
(Bevy integration and "Unreal integration implications" sections; boundary
sketch at `ruleset-ecs-migration-brief.md:481-505`) · **Status:** planning
document; proposes, does not decide. ADR acceptance is the owner's alone.

**What this node establishes:** whether `bevy_ecs` genuinely stays an internal
substrate, or leaks — stated as two boundaries, one per engine, each with the
mechanism (or absence of mechanism) that enforces it.

## 0. The asymmetry, stated before anything else

The two halves of this document are not the same kind of document, and reading
them as if they were would be the most misleading outcome this node could
produce.

- **The Bevy half describes a boundary that exists.** Canonical state already
  lives outside every Bevy application world (`crates/orrery_core/src/executor.rs:48-52`),
  a named gate already fails a gated crate with Bevy in its graph
  (`scripts/core-gates.sh` clause 1, mutation-checked in §6), and the backend
  already links zero Bevy (`cargo tree -p orrery_persistd | grep -ci bevy` = 0,
  re-run for this document). Claims in §2 are cited to code and gates.
- **The Unreal half describes a boundary that does not exist. There is zero
  Unreal code in this tree.** Verified for this document:
  `grep -ril unreal` over `crates/` and `gates/` matches nothing; the only
  matches anywhere are three documentation references to Epic's Replication
  Graph *as prior art for cell-based interest management*
  (`docs/01-spatial-model.md:9,139`, `docs/03-replication.md:106`,
  `docs/references.md:103`) — citations about Fortnite's AOI pattern, not
  integration code. Both A3 lanes flagged this; the second opinion weighted
  the entire Unreal axis down to 3/10 for exactly this reason
  (`docs/plans/a3-simulation-host-second-opinion.md:391`: "Owner-stated
  requirement with zero in-tree code … a high weight would let an unevidenced
  future dominate evidenced present costs").

Everything in §4–§5 is therefore **specification against an absent
implementation, unevidenced by construction**. Each subsection there carries
the marker **[SPEC — no implementation exists]** so that no sentence of it can
be quoted without its status. The asymmetry is not a defect of this document;
it is the finding.
