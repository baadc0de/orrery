# Spike — The island manifest as a beyond-AOI position feed (#535)

**Read-and-report only. No production code changed.** Answers
[#535](https://github.com/baadc0de/orrery/issues/535) as a review, not a
proposal. Every `path:line` below was opened at this branch's HEAD (`2548519`,
`docs/manifest-leak-review`, even with `origin/main`) before being cited.
Owner rulings of 2026-08-27 and 2026-08-30 in the issue thread are recorded
and respected; nothing here re-litigates them. ADR-0040 and ADR-0050 are
**Proposed** and therefore non-normative; the accepted records (D6, D12, D16)
are cited only where they are actually silent or actually applicable.

Branch: `docs/manifest-leak-review`. Evidence: this document only.

---

## 0. The verdict, first

**The issue's structural claim holds at HEAD, and the exposure is latent, not
live — which the two owner rulings already reflect.** One line-item drift
matters: the 512 m campaign edge the issue describes as *proposed* (#532) has
**landed** — `CAMPAIGN_CELL_EDGE_M = 512.0`
(`crates/orrery_games/src/regolith/mod.rs:309`, asserted in
`crates/orrery_games/tests/regolith.rs:501-502`). The leak analysis below is
therefore run at the live edge, not a hypothetical one.

Four findings carry the assessment:

| | Finding | Evidence |
|---|---|---|
| **1. The broadcast is unfiltered by construction, not by accident.** | `broadcast` sends one cloned manifest to every node the manifest itself names; `manifest()` takes no recipient parameter. | §1.1 |
| **2. The recipient can recover more than the set it is shown.** | An unswept presence set is a contiguous 3×3×3 block, so its single centre — the committed cell — is recoverable arithmetically; a swept set additionally discloses heading. | §2.2 |
| **3. Membership does not need the cells.** | Every membership consumer reads `.node` only; both non-test readers of `PeerEntry.cells` are replication-source uses. The field exists so a replication *source* can learn its clients' AOIs without a second channel. | §3 |
| **4. No Accepted record sanctions it, and no Proposed record answers it.** | D50 puts #535 out of scope and parks it in clause (g); D40 names manifest redaction a forward extrapolation; D6/D12/D16 are silent on the wire shape. | §4 |

What is **not** claimed: that anything is exploited, that a shipping client
sends presence or consumes `PeerEntry.cells` (none does — §3.2), or that a
change should be dispatched. Per D50 clause (g) and the owner's rulings, the
next move belongs to the owner, inside the hearsay frame. §5 prices the
options so that decision is informed, and §6 records where the issue text and
prior comments have drifted.

---

## 1. What is broadcast today (Q1)

### 1.1 The shape and the loop

`PeerEntry` is `{ node: NodeId, cells: Vec<CellId> }`
(`crates/orrery_protocol/src/coord.rs:73-78`), carried in
`IslandManifest.peers` (`coord.rs:85-102`), whose doc comment states the
topology as a property, not a side effect:

> "Every peer in the island, **including the recipient**. One manifest is
> broadcast to everyone it names, so it cannot be relative to any one of
> them; a peer filters itself out on receipt." (`coord.rs:94-100`)

The producer side confirms there is no per-recipient path anywhere:

- `Registry::manifest(island_id)`
  (`crates/orrery_coordinator/src/registry.rs:406-433`) copies the island's
  cell set and each peer's stored presence cells verbatim (sorted, capped by
  the 64-cell presence bound `MAX_PRESENCE_CELLS = MAX_INTEREST_GRANT_CELLS`,
  `coord.rs:158`, `coord.rs:990`). It takes **no recipient**.
- `CoordinatorServer::broadcast`
  (`crates/orrery_coordinator/src/server.rs:832-848`) loops over
  `manifest.peers.clone()` and sends the **whole manifest, cloned**, to each
  entry as `CoordMsg::IslandAssignment`
  (`crates/orrery_protocol/src/coord.rs:1044-1048`). There is no AOI, regime,
  or relationship filter in the loop body — deliberately, because the roster
  must be self-describing (`server.rs:827-831`).

### 1.2 Granularity

Each `PeerEntry.cells` is the peer's active interest set as reported: the D5
27-cell baseline `neighbors27` (a 3×3×3 block, self included —
`crates/orrery_protocol/src/cell.rs:302-315`) optionally grown by
`swept_neighbors27` along the velocity vector (`cell.rs:345-404`), plus
hysteresis lag. The interest cell is a `CellId` at
`INTEREST_LEVEL = MAX_LEVEL = 21` (`cell.rs:38`, `cell.rs:162`), whose edge
is `DEFAULT_CELL_EDGE_M = 128.0` m framework-wide (D16, `cell.rs:58`) and
`CAMPAIGN_CELL_EDGE_M = 512.0` m in the campaign (`regolith/mod.rs:309`).

### 1.3 Audience

Every member of the island, including the recipient. Island membership is an
overlap-connected component formed by `report_presence`
(`registry.rs:272-332`), and — a point the issue does not make — **the
island's cell set only ever grows while populated**: `island.cells.extend`
(`registry.rs:320`) adds on every report, `remove_peer_from_island`
(`registry.rs:444-464`) retains peers but never shrinks cells, and the set is
discarded only when the island drains (`registry.rs:456-463`). The footprint
is a high-water mark, so long-lived islands are not bounded by current
spatial proximity.

### 1.4 Frequency

Presence and crossings share one rate bucket per peer:
`PRESENCE_PER_SECOND = 4`, `PRESENCE_BURST = 16`
(`server.rs:77-78`), enforced at `server.rs:1345-1354` (presence) and
`server.rs:1389-1402` (crossings, "share the presence bucket"). Every
*accepted* report runs `registry.report_presence`
(`server.rs:1362`) → `Shared::apply`
(`server.rs:883-886`) → `broadcast(change.manifests)`
(`server.rs:885`). So one member moving re-delivers **everyone's cells to
everyone**, at up to 4 accepted reports/second sustained per moving peer
(burst 16). The client half queues and replays the newest report on
reconnect (`crates/orrery_net/src/coordinator.rs:176-183`).

### 1.5 Who actually feeds it today

No shipping code does. The only `report_presence`/`report_crossing` callers
outside `orrery_coordinator` itself and `orrery_net`'s API/tests are the P3
gate harnesses, which report a single static cell
(`gates/p3-island/src/peer.rs:125`; likewise `gates/p3-siblings`). The 4 Hz
standing feed is the designed ceiling, not observed behaviour.

---

## 2. What a recipient can derive (Q2)

### 2.1 What a CellId is worth in metres

An interest-level `CellId` names a cube whose edge is **128 m** at the
framework default (D16, `cell.rs:58`) and **512 m** at the campaign edge
(`regolith/mod.rs:309`). The shard level — one `CellId` 3 levels up — is
8×8×8 interest cells (`SHARD_LEVEL = INTEREST_LEVEL - 3`, `cell.rs:43`),
i.e. 1 024 m or 4 096 m. The 27-cell set as a whole spans 3 edges per axis:
**384 m** (framework) or **1 536 m** (campaign). For scale, the campaign's
guaranteed AOI radius is 460.8 m (`campaign_guaranteed_aoi_radius_m`,
`regolith/mod.rs:311-320`).

### 2.2 Position, movement, activity — concretely

A recipient holding `PeerEntry.cells` for a peer learns, per accepted
presence report:

- **The committed cell exactly, from an unswept set.** `neighbors27` is a
  contiguous 3×3×3 block (`cell.rs:302-315`); its bounding box has exactly
  one centre, and that centre is the committed cell. The wire carries no
  centre label and the registry keeps `committed_cell` server-side, but the
  recovery is arithmetic on the set. Position is thereby bounded to one
  128 m (or 512 m) cube — the set as a whole adds no ambiguity, it only
  pads. (Reasoning, not demonstrated; §7.)
- **Heading, from a swept set.** `swept_neighbors27` grows only on axes the
  velocity can cross during the refresh period (`cell.rs:327-328`,
  `cell.rs:370-385`), so set asymmetry is directional information.
- **Movement and activity.** At the report cadence the set tracks the peer's
  interest as it moves; presence/absence of a peer between manifests is
  itself information (a departure removes the entry, `registry.rs:451`).
- **Beyond-AOI members.** Because islands are overlap-*connected* (not
  pairwise-overlapping) components (`registry.rs:296-300`) and cell sets are
  a high-water mark (§1.3), the feed includes peers whose cells are far
  outside the recipient's own AOI.

**Not exposed:** sub-cell position, entity identity, or anything about
non-peer entities.

### 2.3 Measured against A13/A14's H4

H4 (A13 `docs/plans/a13-aggregation-beyond-aoi.md:193-197`, amended in
A14 `docs/plans/a14-summary-tier-as-performance-mechanism.md:786-792`) wants
a hearsay datum's **delivered age ≥ E / v_max**, with resolution no finer
than the product's declared cell. The manifest satisfies the resolution
clause only if "interest cell" is declared as E — A13's deliberate-map floor
is the **shard** cell (`a13:379-381`), 8× coarser. The staleness clause it
inverts: the manifest has a frequency *ceiling* (0.25 s between accepted
reports), which bounds age **above by nothing**.

| quantity | value (verified) |
|---|---|
| E (campaign interest cell) | 512 m — `regolith/mod.rs:309` |
| Interceptor `max_speed_mms` | 480 000 → floor **1.07 s** — `regolith/archetype.rs:94` |
| Cruiser `max_speed_mms` | 120 000 → floor **4.27 s** — `regolith/archetype.rs:102` |
| Manifest cadence ceiling | **0.25 s** — `server.rs:77` |

So the feed is **4×–17× fresher than H4's floor would permit** a deliberate
feature to be. At the framework edge (E = 128 m) the floors are 0.27 s and
1.07 s — still 1×–4× over. H3 fails by shape (`PeerEntry` carries no source
or age label, `coord.rs:73-78`). H5 is moot while the world is fully public.
H2 is the live tension: A13 forbids hearsay gating membership or rate
(`a13:179-184`), and here the position data *is* the membership channel that
feeds visibility — it cannot simply be dismissed as hearsay (§4).

---

## 3. Is `cells` required for the membership purpose? (Q3)

### 3.1 Consumer trace

Every read of `PeerEntry.cells` and `IslandManifest`, at HEAD:

| Consumer | Reads | Use |
|---|---|---|
| `orrery_spatial/src/visibility.rs:104-119` (`update_client_aoi`) | `.cells` | copies each connected client's peer entry cells into `ClientAoi` — the **AOI source** for replication visibility |
| `orrery_spatial/src/visibility.rs:122-133` (`update_visibility`) | derived `ClientAoi` | entity visible iff its `Cell` is in the client's cells, via exact `CellId` equality (`:65-67`) |
| `gates/p1-swarm/src/bot.rs:1707-1728` (`broadcast_state`) | `.cells` | per-entity audience filter: peers whose `entry.cells.contains(&cell)` (`:1720-1725`) |
| `orrery_net/src/island.rs:151-176` (`apply_manifest`) | `.node` | filters out the local peer, diffs membership by NodeId (`:224-244`) |
| `orrery_witness/src/plugin.rs:607-615` (`witness_links`) | `.node` | `membership.peer_ids()` only (`island.rs:119`) |
| `orrery_net/src/island.rs:183-194` (`follow_sessions`) | writes | constructs `PeerEntry { cells: Vec::new() }` — the coordinator-less path already carries **empty** cells |
| tests | both | `coordinator_server.rs:920-925`, `client_group.rs:194-205`, `island_drain.rs:80-91`, `visibility.rs` tests, `island.rs` tests, `streaming.rs:253,488`, `coord.rs:1133-1144` |

### 3.2 The answer

**Membership proper needs only the NodeId roster** (plus island, epoch,
regime — all already fields, `coord.rs:85-102`). Linking, dialling,
witnessing, and roster diffing are all node-only today, and the
coordinator-less path already tolerates empty cells — so nothing in
*membership* consumes the field.

The per-peer cells exist for exactly one stated purpose: so a peer acting as
a **replication source** can learn its island-mates' (i.e. its clients') AOIs
from a handout it already holds instead of a second channel
(`visibility.rs:8-15`), and gate replicon visibility on it. That is a
replication feature wearing a membership struct. The island-level `cells`
vector (`coord.rs:90-91`) is *not* a substitute — it says what the island
covers, not which peer subscribes to what.

### 3.3 Exposure is latent

`clients/regolith` depends on neither `orrery_coordinator` nor `orrery_net`
nor `orrery_spatial` (`clients/regolith/Cargo.toml:13-24`: orrery_core,
orrery_games, orrery_predict, orrery_protocol only). Its live membership
handout is `lobby::StartManifest` → `ActiveSeat { slot, node, entity }` —
**no cells** (`clients/regolith/src/lobby.rs:380-389`). No shipping client
sends presence (§1.5). The receive-and-retain half (`OrreryNetPlugin`) *is*
in the default client plugin group (`crates/orrery/src/lib.rs:495-503`),
while the sole AOI consumer (`AoiVisibilityPlugin`) is deliberately kept out
of it (`crates/orrery/src/prelude.rs:23-25`, asserted in
`crates/orrery/tests/client_group.rs:125`). The pieces are adjacent by
design: one wiring step converts a design-level disclosure into a live feed.

---

## 4. Records and rulings (Q4)

**No Accepted record designs or reviews this disclosure.** D6
(`docs/adr/0006-population-adaptive-topology.md`) sanctions islands and
regimes; D12 (`docs/adr/0012-backend-services.md`) lists the coordinator and
its island duties; D16 (`docs/adr/0016-parameter-reference.md`) fixes the
128 m default edge. All three are silent on the manifest's wire shape. The
sketch lives in `docs/02-networking.md:78-92` — an expansion doc, subordinate
to Accepted ADRs.

Where the Proposed records sit, verified at HEAD:

- **A13** (`docs/plans/a13-aggregation-beyond-aoi.md`) flagged this as a
  finding: "the existing leak is bigger than the proposed one" — interest-cell
  granularity at presence cadence, "finer than anything H4 would permit a map
  to say" (`a13:388-394`). A deliberate spatial-knowledge feature under A13's
  rules (H1–H5, `a13:168-203`) would be: aggregates not positions, shard-cell
  resolution (1 024 m / 4 096 m), age ≥ E/v_max enforced by a serving fold
  (delivered age in [F, 2F), A14 `:619-634`, `:786-792`), source/age-labelled
  end to end (H3), reveal-filtered at the source (H5), never a simulation
  input (H1) and never gating membership or rate (H2).
- **A14** (`docs/plans/a14-summary-tier-as-performance-mechanism.md`) carries
  the amended H4 and re-cites the exposure (`a14:690`); its verdict section
  preserves "the island-manifest exposure" as surviving, unanswered
  (`a14:747-751`).
- **D40** (`docs/adr/0040-visibility-and-spatial-query-layering.md`,
  Proposed) names the manifest as a presence oracle — "redacted entity state
  + named (NodeId, cells) manifest => presence + coarse location" — and makes
  manifest redaction a **forward extrapolation** requiring its own
  owner-approved record (`:305-315`, `:325-326`, `:455-470`).
- **D50** (`docs/adr/0050-knowledge-tiers.md`, Proposed) puts #535 explicitly
  out of scope (`:37-42`) and clause (g) records the shape of the answer it
  declines to give: "a *hearsay feed delivered without H3–H5*" whose
  settlement belongs "inside the hearsay frame", next move the owner's
  (`:288-297`).

**Owner rulings already in the thread:** 2026-08-27 — the *concept* of a
beyond-AOI feed is accepted; mechanics owner-reserved; settle inside A14's
hearsay record. 2026-08-30 — the disclosure is accepted as-is for now; no
coarsening, no age floor, no source labels, no change to `PeerEntry`; the H4
gap is a known, accepted property; revisit on observed cheating **or** when
`update_client_aoi`/coordinator presence is wired into a shipping client,
whichever comes first.

So: a deliberate spatial-knowledge feature would have to enter through the
hearsay frame at shard resolution and enforced staleness. The manifest sits
*outside* that frame — it delivers finer-than-shard data at cadence, with no
labels, as a side effect of membership. It is not a sanctioned instance of
the feature; it is the unreviewed predecessor of one.

---

## 5. Options, with costs (Q5)

| Option | What it is | What breaks | Cost |
|---|---|---|---|
| **A. Leave as-is, documented** | Keep the field; record the disclosure as an accepted exemption when D50 (or a successor) moves to Accepted. This document plus the thread rulings are most of that record. | Nothing today. The exemption must name the H4 gap (4×–17× under floor) and the wiring trigger, or it will be re-litigated. | Zero code; one paragraph in the future Accepted record. Risk grows the moment presence/visibility wiring ships to players. |
| **B. Coarsen the cells field** | Ship each peer's cells at shard level (`ancestor_at(SHARD_LEVEL)`, `cell.rs:65`) or coarse-set only. | `update_visibility` and `ClientAoi::contains` compare `CellId` by **equality** (`visibility.rs:65-67`, `:130`); coarsening needs prefix matching, and a coarser cell admits *more* entities — it widens replication while narrowing the leak. The P1 swarm filter (`bot.rs:1723`) breaks identically. Wire shape changes; H4 arithmetic must be redone at the new E. | Moderate; and the fix trades replication volume for privacy at a ratio the budget docs would have to price. Fixes resolution, not range or cadence. |
| **C. Gate by AOI (per-recipient intersection)** | `manifest()` gains a recipient parameter; each recipient's copy carries peers' cells intersected with the recipient's own presence; `broadcast` builds N manifests instead of cloning one (`server.rs:832-848`). | Breaks the self-describing property the doc comment promises (`coord.rs:94-100`) in its strong form; a source can no longer learn a **non-overlapping** island-mate's AOI — precisely the beyond-overlap disclosure being removed, but it also removes data a Mesh-regime source may lawfully use. Recipient-relative manifests complicate any future log/diff tooling that compares manifests. Tests asserting manifest contents break. | Moderate: one function signature, N constructions, no wire-shape change. Epoch idempotence survives (roster and epoch unchanged). |
| **D. Remove `cells` from `PeerEntry`** | Membership carries NodeIds only; the island-level `cells` (`coord.rs:90-91`) stays for coverage. | `update_client_aoi` loses its stated AOI source (`visibility.rs:8-15`) — a source would need clients' AOIs by another explicit path (e.g. clients presenting their own coverage/grant), which is a new protocol surface. `bot.rs` audience filter breaks. Wire format change (postcard struct, protocol version). The coordinator-less path is unaffected (already empty). | Highest. The only option where the manifest carries **zero positional information**, making any future spatial-knowledge feature enter through the hearsay frame deliberately (D50 clause (g) becomes trivially answerable). |

Sequencing note, consistent with the owner's 2026-08-30 ruling: **A** is the
standing decision; **C** is the cheap retrofit if the wiring trigger fires
without observed cheating; **D** is the structural answer if the manifest is
ever required to be clean by a record that audits metadata channels (D40
(g)'s inventory); **B** is dominated by C for most threat models (it costs
replication breadth and still leaks range and cadence).

---

## 6. Drift, against the issue text and prior comments

1. **#532's 512 m campaign edge has landed.** The issue says "#532 proposes
   raising the campaign edge to 512 m"; `CAMPAIGN_CELL_EDGE_M = 512.0` is in
   the tree (`regolith/mod.rs:309`) with a sizing test
   (`crates/orrery_games/tests/regolith.rs:501-502`). A13's own note that #532
   was "not yet in this tree" (`a13:85-87`, `:456-459`) has drifted likewise.
2. **PeerEntry's line numbers have moved** (issue text `:71-77` → HEAD
   `:73-78`); the definition is unchanged.
3. **#534 and #532 do not resolve** in this repository (GraphQL cannot
   resolve either number as of this writing); A13/A14's plan docs carry the
   substance.
4. Nothing else in the issue's protocol claims has drifted: the definitions,
   the broadcast loop, the presence cadence bound, and the record statuses
   (D40/D50 Proposed) all verify at HEAD.

---

## 7. Not established

- Whether any deployment runs `orrery-coordinator` with long-lived islands —
  no in-repo deployment unit; hosts not probed. The high-water-mark effect
  (§1.3) matters only if so.
- How precisely heading is recoverable from a swept set beyond axis-arity
  (reasoned from `cell.rs:345-404`, not demonstrated).
- Whether option A is lossless for `update_client_aoi` under hysteresis
  (reasoned, not proven).
- Whether coarsening (B) would be acceptable to the replication budget at
  shard resolution — unpriced here.
- That no consumer outside the traced set reads `IslandMembership.peers`
  cells transitively (trace is by grep + read of all `PeerEntry` references;
  dynamic dispatch through generics was not excluded).

