# Kitbash assets: licensing and distribution of assembled variations (#347 design)

**Independent read of #347.** This node answers, concrete against the CGTrader
General Terms as fetched on this branch: can Orrery/Regolith assemble ships by
kitbashing parts from purchased CGTrader kitbash packs, ship those assembled
variations inside a distributed game client, and stay clean? This is a
documentation-only review: it changes no code, commits no money, and defers to
the parallel analysis of the same question in #347's other agent. Two reads of
the same terms were requested deliberately; this is one of them, reasoned
independently, and it does not anticipate the other's conclusions.

**Not legal advice. I am not a lawyer.** The split that matters is kept visible
throughout: what a fetched clause actually says, what industry practice does,
and what I believe a court would do. Three of those are evidence; one is
judgement. They do not collapse into each other here.

The evidence basis, stated up front:
- **CGTrader General Terms, fetched directly on this branch** (the
  `terms-and-conditions` page, sections cited by clause number below). This is
  the load-bearing source and it is verbatim-verified, not recalled.
- **CGTrader "Royalty Free License" and "What is a Custom License" help pages,
  fetched directly.** They restate 21A and 23A in plainer language.
- **Kit B's seller licence sentence**, corroborated across multiple
  marketplace listings (CGTrader, FlippedNormals, CubeBrush, Gumroad) via web
  search within this session.
- **The two CGTrader product pages themselves I could not fetch** — they return
  403/empty to direct fetches, matching the note already in #347. Their licence
  *type labels* and Kit A's contents are therefore taken from #347's body as
  *reported*, not independently re-verified. Everything that turns on a
  product-page label is flagged `[product page: not re-verified]`.

---

## Verdict, in one paragraph

**Kitbashing is expressly licensed, and shipping the assembled ship inside the
client is permitted — but only under one condition that the whole plan must be
built around: the assembled ship must be distributed only as what CGTrader
calls an "Incorporated Product," i.e. in a form from which a player cannot
extract it as a stand-alone object without reverse-engineering tools. That
condition is the entire legal load-bearing wall here, and it is the one thing
the license grades on.** Assembling parts is fine (derivative works are
explicitly granted); recognisability is a red herring for the pack's own
licence; the real question is entirely about extractability of the shipped
binary. The existing #345 decision to embed assets in the executable is the
correct direction; the residual risk is that a player can peel the ship out as
a stand-alone `.glb`, which the license's own definition would then catch. The
plan is viable with moderate confidence (7/10), and it stays clean by embedding
private, never committing to a public repo, and treating per-pack licence
differences — Kit B is a Custom/no-AI licence, not Royalty Free — as real.

---

## 1. What licence the packs carry

CGTrader runs four licensing frameworks, and the operative one for a given
pack is whichever is marked on its product page:

| Licence | When it governs | Operative clauses |
|---|---|---|
| Royalty Free | default for paid/free, per 20.2 | 21A |
| Royalty Free, No AI | RF minus ML/training use | 21A + 21B.1 |
| Editorial | journalistic/news only; prohibits most commercial game use | 22 |
| Custom / Custom, No AI | only if the seller adds terms in the product's "Custom license terms" field | 23A / 23B, read as additions or exceptions to 20 and 21A / 21B |

Per the two products named in #347, **reported not re-verified by me (product
pages 403'd in this session, exactly as #347 records)**:

- **Kit A** — `700 PART SCIFI KITBASH` ($10, 3DKitbash): `Royalty Free License
  (no AI)`.
- **Kit B** — `SPACESHIP Sci-Fi Hard Surface KITBASH 350 DETAILS` ($8,
  olegushenok): `Custom License (no AI)`, seller terms one sentence.

**Verified verbatim: the framework that governs whichever label applies.** The
Royalty Free licence is the one that does the work for Kit A, and it is uniform
marketplace-wide. From the fetched General Terms:

- **20.2** — a seller who does not state terms gets Royalty Free by default.
  So a pack's licence is whatever its listing says, defaulting to RF.
- **20.4/20.6** — the Buyer never owns the Product; they hold a licence. All
  rights terminate immediately if a sale is reversed.
- **20.5** — the licence is non-exclusive, non-transferable, and runs to the
  original Buyer. A legal-entity Buyer may share within the legal entity.
- **20.7** — seller conditions placed **outside** the designated "Custom
  license terms" field are null and void. This matters for Kit B (below).
- **21A.2** — the Buyer's licence is "strictly limited to Incorporated
  Product." Any use or republication of a Product that is not an Incorporated
  Product is "strictly prohibited."
- **21A.4** — for a Buyer using the Product "solely as Incorporated Product,"
  the Seller grants a non-exclusive, worldwide licence to "reproduce, post,
  promote, license, sell, **modify, create derivative works of**, publicly
  perform ... or otherwise exploit for promotional and commercial purposes,"
  and to "**distribute, and sublicense the right to use**, such 3D model ...
  as long as it meets the definition of Incorporated Product."

So the pack's licence is the RF framework, and everything downstream hangs off
one definition and one condition: **definition 8** and **the Incorporated
Product condition**.

## 2. Kitbashing: derivative works, and whether recognisability matters

**Assembling kit parts into a ship is creating a derivative work, and under
this licence that is expressly permitted.** 21A.4 grants the right to "create
derivative works of" the Product to any Buyer using it solely as an
Incorporated Product. Kitbashing is a textbook exercise of that grant: take the
copyrighted source parts, transform and combine them into a new expression. No
separate permission is needed for the act of assembly.

**Recognisability is a red herring *for the pack's own licence*.** The common
fear — "if the assembled ship doesn't look like any single part, it must be
fine" — inverts the actual analysis. Within 21A, derivation from the parts is
licensed outright, so whether the ship is recognisable as one part or none is
irrelevant: the licence already covers the derived work regardless of how far
it strays. Recognisability would only be relevant to a *third party's* claim
that the licence cannot convey — i.e. trade dress or copyright that sits
*inside* a part but belongs to someone else (a recognisable Gundam silhouette,
say). That is outside what CGTrader's grant can transfer, and no amount of
kitbashing changes it. So the two halves must not be conflated:

- against the *seller's* licence: recognisability never matters (derivation is
  granted);
- against a *third party whose IP a part embodies*: recognisability is
  everything, and the pack's licence is irrelevant because the seller could
  not license what they do not own.

**The one reservation on derivation is the "solely as Incorporated Product"
condition that 21A.4 ties it to.** The derivative-works right is granted *only*
to a Buyer "who ... uses it solely as Incorporated Product." If an assembled
ship were ever to leave the Incorporated-Product context — published as a loose
file, sold as a stand-alone model, committed to a public repo — the derivation
right evaporates with the condition, and the assembly becomes an unlicensed
derivative (21A.2, 21A.6). This is exactly the provenance-of-bytes line #341
already drew, and kitbashing does not soften it; it is the fullest case of
"computed from vendor bytes." Recognisability plays no role in this either.

## 3. The sharp one: redistribution and extractability

**The licence cares about extractability, explicitly and affirmatively.** Two
clauses, verbatim from the fetched terms:

- **Definition 8** — an Incorporated Product is "Product that cannot be
  extracted from an application or other product ... and used as a stand-alone
  object without the use of reverse engineering tools or techniques. For
  avoidance of doubt, Incorporated Product is such use of a Product that does
  not allow further distribution of the Product outside of the application ...
  containing the Incorporated Product."
- **21A.3** — "If you use any Product in software products (such as video
  games ...) you must take **all commercially reasonable measures to prevent
  the end user from gaining access to the Product**. Methods of safeguarding
  the Product include but are not limited to: using a proprietary disc format
  ...; **using a proprietary Product format**; using a proprietary and/or
  password protected database or resource file that stores the Product data;
  **encrypting the Product data**."
- **21A.2** lists the approved game case: the Product is fine "as purchased by
  a game's creators as part of a game **if the Product is contained inside a
  proprietary format and displays inside the game during play**, but not for
  users to re-package as goods distributed or sold inside a virtual world."

So the licence grades on **extractability**, not on whether the bytes are
technically present anywhere on the player's disk. This is the heart of the
matter, and the three relevant postures fall out of it directly:

1. **A loose `.glb` shipped to players — breach.** A file a player can open
   with a file manager is extractable as a stand-alone object with no reverse
   engineering at all. That fails definition 8 and 21A.2's "proprietary
   format," and directly against 21A.6's resale/redistribution bar. This is
   the post the #345 "embed in executable" decision exists to defend.
2. **`Packing/compression alone — does not fix a breach.** Compressing a
   `.glb` to `.zip` or `.pak` is not a "proprietary format" in the sense 21A.2
   means, because standard tooling decodes both with no reverse engineering.
   A `.pak` that any game-unpacker reads is cosmetic, not conformant. **Format
   conversion helps only to the extent it genuinely makes extraction require
   a reverse-engineering step**; otherwise it is obscuring a breach, not
   curing one.
3. **A proprietary or encrypted bundle, decrypted only in memory at load —
   the plausible conformant posture.** If the ship is stored in a format (or
   under encryption) the game itself must decode, and the raw stand-alone
   `.glb` is never exposed to the player, then extraction *does* require
   reverse-engineering tools — satisfying the definition's letter.

**The favourably-read boundary, stated plainly:** the definition's bar is
"cannot be extracted ... without the use of reverse engineering tools," *not*
"cannot be extracted, period." Nothing shipped on a player's machine is truly
unextractable; any client must hold enough to render the model. The licence
knows this and sets the bar at "requires reverse engineering," which industry
answers with exactly the pak/encrypted-bundle posture. **I believe** (judgement,
not citation) a proprietary bundle that resists casual extraction satisfies 21A
even though a determined person with RE tools could still pull the model out.
That is the industry reading; it is not a ruling, and it is precisely the gap
#341 flagged as lawyer territory. The honest way to fail is to ship bytes a
player can lift with unzip; the honest way to pass is to ship bytes that at
least force a RE tool.

**One caveat the license raises that a naive reading misses:** 21A.2's
approved game bullet is conditioned "not for users to re-package as goods
distributed or sold inside a virtual world." That targets in-world
re-packaging (user economies), not ordinary player possession, and a normal
client does not trip it. Worth knowing it exists; not a blocker for shipping.

## 4. What the project must do

Concrete obligations that fall out of the verified clauses, not a general
survey:

1. **Embed only; never publish bytes.** The assembled ship and all kit parts
   live in the private `orrery-assets` bucket (#345 §2), referenced by sha256
   in a private manifest, and are embedded in the executable (decrypted /
   proprietary-bundled at load). They never enter `assets/` in this public
   repo, never attach to a public release. This is not a nicety; it is the
   Incorporated-Product condition that the entire licence grant hangs on.
2. **Make the embedding real, not cosmetic.** Choose a proprietary bundle or
   encryption for the shipped mesh data (21A.3 names exactly "proprietary
   Product format" and "encrypting the Product data" as sufficient methods).
   Do not ship a plain `.glb` in the depot. Concretely: the client's current
   path loads `assets/regolith/craft.glb` directly from disk
   (`clients/regolith/src/assets.rs:19`); that must become an embedded-and-
   decoded resource before real kit assets ship.
3. **Keep the per-pack record.** For each pack: the purchase receipt/invoice,
   the licence type and terms *as of purchase date*, and which pack and which
   specific part each assembly uses. CGTrader listings are delistable (#347
   and help pages both warn to "discuss usage with the seller"); the grant of
   record must be a snapshot in your own private storage, not a live page.
4. **Treat no-AI as a real (if narrow) constraint.** 21B.1 drops the RF
   permission for using the Product "as an input (the Product itself, its
   metadata, rendered images, etc.) to machine learning or training of neural
   network models" (21A.2's ML bullet). The proposed Blender-over-MCP loop
   that renders a candidate assembly and feeds the render to a vision model
   for judgement walks directly at this clause: the Product's *rendered
   images* go into a neural network. Whether day-to-day *inference* input is
   "input to machine learning" as 21A.2 phrases it is a genuine ambiguity
   (lawyer question). The safe posture: if parts or renders of parts reach any
   model, use only tools with proven no-training-on-input defaults, or keep
   the vision-judgement human. Using a code agent as a *tool that moves and
   joins meshes* is authoring, not ML training, and is not reached.
5. **Per-pack terms differ enough that a blanket policy is unsafe.** Kit A
   (RF, no AI) is the framework #341 already adjudicated. Kit B is **Custom,
   no AI** whose seller terms are one sentence — and 23A/23B make those terms
   operative only if they sit in the product's "Custom license terms" field,
   and 20.7 voids them if placed elsewhere. The sentence (corroborated across
   olegushenok's listings) is: *"Reselling these assets is not allowed, but
   other than that they are free to use in private and commercial projects."*
   Read as an exception to 21A it arguably *relaxes* the Incorporated-Product
   limit; read as an addition it just restates the resale bar. Which reading
   governs is a lawyer question — but the safe posture is to treat Kit B
   exactly like Kit A (private, embedded, never public), under which both
   readings are satisfied and the ambiguity never has to resolve.
6. **Attribution:** **I found no attribution requirement in the fetched RF
   text** (definitions, 20, 21A, 23 have no credit clause; the help page adds
   none). CGTrader's RF regime does not appear to demand attribution.
   Recall-checked but not separately citable to a clause: stock-3D royalty-
   free terms routinely omit attribution. Do not assume a credit is harmless
   to add, but do not build workflow around one being required. This is the
   one place to ask the seller if you want certainty.
7. **Watch the trade-dress residue.** A part that embodies a third party's
   trade dress (a Gundam-adjacent silhouette) cannot be licensed by CGTrader,
   and 21A.4's license of "trademarks, service marks or trade names
   incorporated in the Product" only reaches marks the *seller* controlled.
   Selection discipline: use generic greebles, avoid whole recognisable
   subassemblies. Human judgement per part, at selection time. Not fixable by
   licence reading.

## 5. What would make this unambiguous

The core terms are verifiable and I fetched them; the residual ambiguity sits
in exactly four places, and a lawyer should confirm them (in decreasing order
of how much the answer moves the plan):

1. **Whether the distributed client meets definition 8 / 21A.2's "proprietary
   format."** Specifically: does a proprietary/encrypted bundle that holds the
   ship but that a determined RE could still extract satisfy "cannot be
   extracted ... without the use of reverse engineering tools"? Industry says
   yes; no ruling does. This is the single highest-stakes question and it is
   unresolvable from public text.
2. **Kit B's Custom reading.** Does its one sentence supplement (restating the
   resale/redistribution bar under 21A) or *exceed* (relaxing Incorporated
   Product) the RF baseline? Operative only if the sentence is in the
   "Custom license terms" field (20.7).
3. **The no-AI inference boundary.** Whether feeding *renders* of kit parts to
   a vision model for assembly judgement is "input to machine learning" under
   21A.2/21B.1.
4. **Attribution and third-party trade dress in the parts.** Whether the
   selected parts carry any outside IP, and whether the seller's grant is
   clean. Only the seller can answer the latter with authority; a licence
   review of the actual part set handles the former.

To de-risk before spending: **ask each seller directly** (CGTrader's own help
pages repeatedly advise buyers to "discuss the usage with the seller and gain
their approval"). A seller's written confirmation that assembled ships embedded
in a distributed game client are in-scope would collapse most of the ambiguity
above into a matter of record. Snapshot the product page, the custom-terms
field, the licence type, and the invoice at purchase.

## Strongest argument against my conclusion

The plan's viability rests on the favourable reading of definition 8: that a
proprietary/obfuscated bundle satisfies "cannot be extracted without
reverse-engineering tools" and that embedding in the executable is enough. The
case that it is wrong: a court (or CGTrader, enforcing on the seller's behalf)
could read "commercially reasonable measures to prevent the end user from
gaining access to the Product" (21A.3) strictly, hold that a shipped asset is
by definition accessible to a player's machine, and find that any client that
renders a mesh has effectively distributed it. Under that reading, no amount of
embedding saves you, because a player receives the render and could in principle
capture it — and game assets are "extractable almost by definition," as the
question itself notes. If that strict reading prevails, then kitbashing plus
distribution is not clean under this licence at all, and the only clean paths
are either buying explicit written permission per pack or commissioning
original geometry. I do not think that is the likely reading — every commercial
game with a stock-asset supply chain relies on the incorporated-product bar
meaning "you cannot unpak it casually," and the licence's own drafting (it
names encryption/proprietary formats as sufficient) gestures at exactly that
industry meaning — but it is a real and material risk, and it is why the honest
answer is "viable, with the extractability obligation taken seriously," not
"unambiguous."

## What I could not verify

- **The two product pages' licence-type labels and Kit A's contents.** Direct
  fetch returns 403/empty; the RF/no-AI and Custom/no-AI labels and Kit A's
  review count/polycount/format list are taken from #347's body as reported,
  not re-fetched. Re-read and snapshot them at purchase.
- **Any explicit attribution clause** — I verified there is *no* attribution
  requirement in the fetched RF framework, but I did not find a clause
  affirmatively stating it, so "attribution not required" rests on absence
  from a full read, not on a positive sentence.
- **How CGTrader actually enforces** the incorporated-product bar in practice
  (take-downs, disputes) — not public, not fetched.
- **The assembled-ship-as-derivative interplay with the seller's moral/about-
  art rights** under the governing Lithuanian law (16.2) — a jurisdiction
  question no fetched clause resolves.

## Housekeeping notes (engineering, not legal)

- The shipped client has **one** art slot, `assets/regolith/craft.glb`
  (`clients/regolith/src/assets.rs:19`), loaded for both entities with a
  primitive fallback (`clients/regolith/src/lib.rs:131-143`). Kitbashed
  "variations" currently have nowhere to go; the variation system is its own
  problem, and is a sideshow to the licence question.
- `scripts/asset-provenance.sh` fails a build on any `.glb`/`.gltf` outside
  `assets/` (verified: the guard's stray-scan). Do all assembly outside the
  checkout, or CI breaks on a scratch file.
- Size ceilings: in-repo assets are capped (512 KiB/asset, 2 MiB total, per
  `docs/15-asset-provenance.md` §5). A kitbashed ship will not fit and does
  not belong in-repo anyway under §4.1 above; it belongs in the private bucket
  where #345's still-open per-asset ceiling is the number to set.

## Sources

- **Fetched this session, verbatim:** CGTrader General Terms &
  Conditions (`www.cgtrader.com/pages/terms-and-conditions`) — definition 8;
  §§20.1–20.8, 21A.1–21A.7, 21B.1, 22, 23A, 23B. Royalty Free License help
  page; What is a Custom License help page.
- **Corroborated by web search this session:** Kit B's seller licence sentence
  appearing identically across olegushenok's CGTrader / FlippedNormals /
  CubeBrush / Gumroad listings.
- **Reported, flagged `[product page: not re-verified]`:** product-page licence
  labels and Kit A contents, from #347's body.
- **In-repo context:** `docs/15-asset-provenance.md` §1–§8;
  `clients/regolith/src/assets.rs:19`; `clients/regolith/src/lib.rs:131-143`;
  issues #341, #345, #347. Repository facts for the engineering notes were
  read on this branch, not re-measured at new commit.

*Proposes, does not decide. This is an owner decision (and, on the residual
ambiguities, a lawyer's). It is one of two deliberately independent reads of
#347; where it and the other read agree or disagree is itself the signal the
owner asked for.*
