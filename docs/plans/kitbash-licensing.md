# CGTrader kitbash packs: licensing and distributability of assembled variations

> Research node for #347, licensing half only -- the collision-geometry half was
> settled by #352 landing (tracking-based hit resolution; no hulls needed) and is
> not reopened here. The question: may Orrery/Regolith assemble ship variations
> by kitbashing parts from purchased CGTrader packs and ship those assemblies in
> a distributed game client -- and under what conditions? **I am not a lawyer and
> this is not legal advice**; said once, here, and the analysis below separates
> what the licence text says from what practice assumes from what I believe.
> Repository facts verified 2026-08-27 on this worktree; CGTrader terms fetched
> live 2026-08-27 (sections quoted below were read, not recalled). **Propose,
> not decide** -- every commitment below is reserved to the owner.

## Verdict up front

**Yes, conditionally -- and the conditions are already this repository's policy.**
Under the CGTrader Royalty Free licence as fetched 2026-08-27, kitbashed
assemblies are expressly permitted derivative works, and shipping them inside a
game client is expressly permitted distribution, on one condition that does all
the work: the assembly must be an **Incorporated Product** -- not extractable
from the client "and used as a stand-alone object without the use of reverse
engineering tools or techniques" -- and the buyer carries an affirmative duty to
take "all commercially reasonable measures to prevent the end user from gaining
access to the Product" (s21A.3). Embedding assets in the executable, as #345's
default plan already chooses, is the strongest available discharge of that duty
short of encryption. Loose `.glb` files in a distributed build would likely
breach it.

Confidence, stated plainly: **high** that the terms say what is quoted below
(read live today); **medium** that embedding satisfies the extraction clause
(that reading is industry custom plus the "commercially reasonable" standard,
not an adjudicated result); **low-to-medium** on the three residual questions
flagged for a lawyer in the final section. Nothing found contradicts the
adjudication already recorded in #347 and its comments; this node re-verifies
its licensing claims from the primary source and extends the records list.

## 1. What licences CGTrader packs carry

CGTrader sells under sitewide licence types plus a seller-custom escape hatch.
All quotes below are from the
[CGTrader General Terms](https://www.cgtrader.com/pages/terms-and-conditions),
fetched 2026-08-27:

- **Royalty Free License** (s21A). The default commercial licence. Its grant
  (s21A.4) is broad: the buyer who uses the Product "solely as Incorporated
  Product" may "reproduce, post, promote, license, sell, modify, create
  derivative works of, publicly perform, publicly display, digitally perform,
  transmit" -- kitbashing is inside that grant by name ("modify, create
  derivative works of"). Its limit (s21A.6): "The resale or redistribution by
  the Buyer of any Product, obtained from the Site is expressly prohibited
  unless it is an Incorporated Product."
- **Royalty Free License, no AI** (s21B.1): "the same licensing terms apply as
  for Royalty Free License, except that Product use for machine learning or
  training of neural network models, including generative AI models, is not
  permitted." The permission withdrawn is s21A.2's "as an input (the Product
  itself, its metadata, rendered images, etc.) to machine learning or training
  of neural network models, including generative AI".
- **Editorial License**: no commercial use; disqualifying for this project on
  its face. (Category recalled from training data and from #341; not re-read
  today -- no pack under consideration carries it, so nothing turns on it.)
- **Custom License** (s23B): applies "if and only if the Seller provides
  additional license terms in the specific area of Product description --
  'Custom license terms'", read "as additions and (or) exceptions to" the
  standard sections. s20.7 voids seller terms placed anywhere else: "any
  additional end user license agreements, licenses, custom licenses, or Seller
  requirements inserted into Seller Products in any area outside that
  explicitly provided by CGTrader for additional license terms are invalid."

**The two packs in question, re-verified 2026-08-27** (product pages 403 plain
fetches; read via reader proxy, as #347 also had to):

| | Kit A (700 PART SCIFI KITBASH, seller 3DKitbash) | Kit B (SPACESHIP Sci-Fi Hard Surface KITBASH 350, seller olegushenok) |
|---|---|---|
| Licence today | Royalty Free License | Custom License (no AI) |
| Price today | $20.00 -- the 50%-off $10 price #347 recorded on 2026-08-23 is no longer shown | $8.00 (50% off $16.00, unchanged) |
| Custom terms | none | one sentence, verbatim: "Reselling these assets is not allowed, but other than that they are free to use in private and commercial projects." |

Two drifts since #347's 2026-08-23 reading, both worth knowing: Kit A's
discount is gone (the "sale" was not a permanent anchor after all), and today's
proxy read of Kit A shows "Royalty Free License" without the "(no AI)" suffix
#347 recorded -- most likely a rendering artifact of the proxy, but it is
exactly the kind of seller-editable field that must be **snapshotted at
purchase**, not trusted from either reading.

**Does the game's end user receive "the model itself"?** Under the terms'
own frame, no -- provided the Incorporated Product condition holds. The
definition's tail (fetched today): "Incorporated Product is such use of a
Product that does not allow further distribution of the Product outside of the
application... containing the Incorporated Product." The player receives the
application; whether they also effectively receive the model is precisely the
extraction question of section 3 below.

## 2. Kitbashing produces a derivative work -- and that is fine, not a problem

Two separate legal frames, and recognisability matters in only one of them:

- **Under the CGTrader contract, recognisability is a red herring.** An
  assembly does not merely resemble the parts, it *contains their bytes* -- it
  is a derivative work regardless of whether any single part is recognisable.
  But the contract does not care, because s21A.4 grants "modify, create
  derivative works of" by name, conditioned on Incorporated Product use. The
  derivative inherits the source licence: an assembly built from kit parts is
  vendor bytes in the fullest sense, so #341's provenance-of-bytes rule
  ("nothing computed from vendor bytes is ever public") applies a fortiori.
  This is the ruling #347 already adopted; nothing found today disturbs it.
- **Under third-party copyright/trade dress, recognisability is the whole
  game.** A seller can only license what they own. A buyer review of Kit B
  (quoted in #347) warns some parts are "so gundam-like you probably couldn't
  use them as-is" -- if a part traces Bandai Namco trade dress, the CGTrader
  grant conveys nothing against Bandai, and no amount of licence compliance
  helps. Here an assembly being *unrecognisable* as the infringing part is the
  mitigation. Practical rule: per-part human vetting of Kit B; prefer generic
  greebles; avoid whole recognisable subassemblies. (This is my judgement of
  the risk shape, not a licence term.)

One more clause with teeth for the *pipeline* rather than the product: both
packs' "no AI" variants withdraw s21A.2's machine-learning-input permission. An
agent workflow that renders candidate assemblies and feeds the renders to a
vision model for critique is putting "rendered images" of the Product into a
neural network. Whether inference-time input falls inside "input to machine
learning" is genuinely arguable (lawyer question; #347 s2(d) flagged it and I
agree after reading the clause today). The safe posture costs nothing: keep kit
bytes and renders out of any model call, and out of any service with
training-on-input defaults.

## 3. Redistribution -- the sharp question

The licence cares, in two stacked ways, both read live today:

1. **A definitional gate.** s21A.6 prohibits redistribution "unless it is an
   Incorporated Product", and the definition requires that the Product "cannot
   be extracted from an application... and used as a stand-alone object without
   the use of reverse engineering tools or techniques."
2. **An affirmative duty.** s21A.3: "If you use any Product in software
   products (such as video games, simulations, or VR-worlds) you must take all
   commercially reasonable measures to prevent the end user from gaining
   access to the Product", with a non-exhaustive method list including "using a
   proprietary disc format such as Xbox, Playstation, etc.; using a proprietary
   Product format; using a proprietary and/or password protected database" and
   "encrypting the Product data".

What follows for the proposed mitigations -- separating licence text from
practice from belief:

- **Loose files fail.** A `.glb` on disk next to the executable is a
  stand-alone object; no tool is needed at all. The client's only current art
  slot is exactly that shape -- `regolith/craft.glb` resolved from an on-disk
  root (`clients/regolith/src/assets.rs:12,19`) -- which is fine today because
  no licensed asset exists, and is the thing the packaging step must change
  before one does.
- **Format conversion alone does not help.** OBJ-to-glTF produces another
  standard, loose, stand-alone file. Conversion is pipeline hygiene, not a
  compliance measure.
- **Packing/embedding plausibly satisfies both clauses.** The licence's own
  method list blesses "a proprietary Product format" without demanding
  encryption, and the duty's standard is "commercially reasonable measures",
  not "impossible to extract". Embedding assets in the executable (#345 s2,
  point 4 of its default plan) means extraction requires a binary-analysis
  tool -- squarely "reverse engineering tools or techniques" on any ordinary
  reading. *That reading is industry custom* (every pak-file game relies on
  it), *not a ruling*; #341 flagged it as the lawyer question and it remains
  one.
- **Obscurity vs breach, honestly.** Game assets are extractable almost by
  definition -- generic rippers exist for every engine. The licence
  anticipates this: it does not require preventing extraction, it requires
  that extraction need *tools/techniques* (definition) and that the buyer took
  *commercially reasonable* measures (duty). So packing is not "merely
  obscuring the breach" -- under this text, the measure itself is the
  compliance, and a determined extractor is the licensor's problem with that
  third party, not evidence of the buyer's breach. That last sentence is my
  reading of the two clauses together; a lawyer should bless it.

**Kit B's custom licence, and why the owner's decision moots it.** The seller's
one sentence, read as an *exception* to the standard sections, is broader than
Royalty Free (only reselling forbidden); read as an *addition*, the
Incorporated Product limits still bind. #347's fourth comment records the
owner's 2026-08-24 decision: **treat Kit B exactly as Kit A**. Under the Kit A
posture -- embedded, never loose, never public -- both readings are satisfied and
the ambiguity never needs resolving. This node concurs and proposes ratifying
it as the blanket policy: per-pack terms demonstrably differ (these two packs
prove it), so a blanket policy is unsafe *unless* it is the strictest
applicable posture -- which this one is.

## 4. What the project must do

Standing obligations, each mapped to machinery that exists or is specified:

1. **Never public.** No kit part, assembly, or artifact computed from either
   enters `assets/` or any public channel -- `docs/15-asset-provenance.md` s1
   (public repo commit is redistribution) and #341's provenance-of-bytes rule.
   The in-repo guard already refuses stray model files; do assembly work
   outside the checkout (#347 s2 corollary).
2. **Embed, do not ship loose** -- #345 s2 point 4; this is also the s21A.3
   duty discharge. `assets/private-manifest.toml` is specified by #341 s4 /
   #345 s2 and **does not exist yet** (verified: `assets/` holds only
   `provenance.toml` and `fixtures/` today) -- it must land before the first
   licensed byte does, with one field this node adds: **which pack each part
   came from**, per assembly, so a future dispute or revocation can be scoped
   to the affected assemblies.
3. **Evidence bundle at purchase**, per #341 s4: invoice, licence text as of
   that date, and the full product page -- for Kit B specifically, proof that
   the custom sentence sits in the designated "Custom license terms" field,
   because s20.7 voids it anywhere else (and if it is not there, Kit B falls
   back to the standard terms, which the adopted posture also satisfies).
   Store under `originals/` in the private bucket. Marketplace pages are
   seller-editable and delistable; the grant must not depend on them.
4. **Attribution: none found required** for Royalty Free game use in the
   sections fetched today -- but I did not read the full terms end to end
   through a proxy, so this is a **checked-for-and-not-found** claim, not a
   verified absence. Confirm against the complete purchased-licence snapshot
   at purchase time.
5. **No kit bytes or renders into model calls** (section 2's no-AI point).
6. **Keep the primitive fallback load-bearing.** The client already runs with
   the art slot absent (`clients/regolith/src/assets.rs:32-36` returns `None`
   and the caller falls back to primitives), so a licence problem can never be
   a broken build -- preserve that property through any embedding change.

## 5. What would make it unambiguous

Owner-verifiable at purchase (no lawyer): Kit A's exact licence label
including the no-AI suffix; Kit B's custom sentence in the designated field;
absence of per-pack riders; the perpetuity/revocation clause for purchased
licences. Snapshot all of it.

Lawyer, if the project ever wants a written opinion (ranked by exposure):

1. Whether embedding in an executable -- whose loader is itself MIT and public --
   satisfies "reverse engineering tools or techniques" and "commercially
   reasonable measures" (the one question every distribution channel shares).
2. Whether Kit B's sentence is an exception or an addition to s21A/s21B
   (avoided, not answered, by the treat-as-Kit-A posture).
3. Whether inference-time image input is "input to machine learning" under the
   no-AI variant (avoided by keeping renders out of model calls).
4. Residual trade-dress exposure of Gundam-adjacent Kit B parts (mitigated by
   selection discipline, never eliminated by the marketplace licence).

None of these blocks buying the packs or building the pipeline; all of them
are avoidable by posture except (1), and (1) is the risk every commercial game
with stock assets carries.

## Strongest argument against this conclusion

The open-source loader is the crack in the Incorporated Product armor, and it
is wider here than for a typical game. The definition turns on extraction
needing "reverse engineering tools or techniques" -- but this repository
publishes, under MIT, the exact code that locates, decodes and loads the
embedded assets. An extractor is not a reverse-engineering effort; it is a
twenty-line program linking the project's own public crates. A hostile reading
says the buyer has not taken "all commercially reasonable measures to prevent
the end user from gaining access" when the buyer simultaneously publishes a
manual and a library for gaining access; on that reading no packing short of
real encryption with a non-public key satisfies s21A.3 for an open-source
client, and the whole "embedding discharges the duty" position collapses.
The rejoinder -- the enumerated methods include unencrypted "proprietary
Product format", and "commercially reasonable" is a flexible standard that
ioquake3-pattern projects have relied on for two decades -- is custom and
argument, not text. If any single point sends the owner to a lawyer before
shipping a public build with licensed assets, it should be this one; and if
the answer worries him, per-build encryption of the embedded pack with a key
derived at runtime is the cheap over-compliance that moots it.

A second, smaller counter: today's Kit A proxy read lacking the "(no AI)"
suffix could mean the seller changed the licence variant between 2026-08-23
and 2026-08-27. If sellers can and do flip licence variants, then *nothing*
about a listing is stable until purchased -- which strengthens, not weakens,
the snapshot-at-purchase rule.

## What I could not verify

- **The product pages, directly.** CGTrader serves 403/empty to plain fetches;
  both listings were read through a reader proxy (as #347 also had to). Licence
  labels, prices and the custom-terms sentence are proxy readings.
- **Whether Kit B's sentence sits in the designated custom-terms field** -- the
  proxy flattens page structure; only a purchase-time screenshot settles it.
- **Kit A's "(no AI)" suffix today** (present in #347's 2026-08-23 read, absent
  in today's proxy read; cannot distinguish seller edit from proxy artifact).
- **Attribution and revocation clauses in the full terms** -- the key sections
  were fetched verbatim, but the terms were queried section-by-section, not
  read end to end; a checked-for-and-not-found result is recorded in section
  4 point 4.
- **The Editorial licence text** -- recalled category, not re-read; nothing in
  this decision turns on it.
- **Any adjudicated case on marketplace "Incorporated Product" clauses** -- I
  did not search case law; the "pak file is enough" position rests on industry
  custom throughout this node and is labeled as such wherever it appears.

## Decisions proposed to the owner (none taken here)

1. Ratify the blanket policy: every CGTrader pack, whatever its per-pack
   licence, gets the strictest posture -- embedded, private bucket, never
   public, evidence bundle at purchase (this is the already-decided Kit B
   treatment, generalized).
2. Land `assets/private-manifest.toml` with a per-part pack-attribution field
   before the first licensed byte ships.
3. Decide whether the open-source-loader argument (strongest-against, above)
   warrants either a written legal opinion or cheap over-compliance via
   encrypted embedding, before the first *public* asset-bearing build -- the
   recruited-cohort playtest (#345) is lower-exposure and need not wait.
4. Keep the no-render rule for agent pipelines until the no-AI inference
   question is resolved or mooted.

## Sources

- **External, fetched live 2026-08-27:**
  [CGTrader General Terms](https://www.cgtrader.com/pages/terms-and-conditions)
  -- Incorporated Product definition, s20.7, s21A.2, s21A.3, s21A.4, s21A.6,
  s21B.1, s23B, all quoted verbatim above; Kit A and Kit B listings via reader
  proxy (URLs in #347).
- **In-repo, read on this worktree 2026-08-27:**
  `clients/regolith/src/assets.rs:12,19,32-36` -
  `docs/15-asset-provenance.md` s1, s7, s8 - `assets/provenance.toml`
  (and the verified absence of `assets/private-manifest.toml`).
- **Issues:** #347 (body and all four comments, including the owner's
  2026-08-24 Kit B decision), #341, #345, #352.

## Appendix: the independent second read (#561), and where it diverges

This question was dispatched to **two agents in parallel and deliberately**, in separate
worktrees, neither seeing the other's work, at the owner's instruction: "see if they
agree on legalities." Both were told not to hedge toward agreement, because artificial
convergence carries no information. The second read landed as PR #561
(`docs/plans/kitbash-assets-licensing.md`, opencode); its distinct findings are folded
here and that PR is closed rather than landing a near-duplicate doc beside this one.

### Where the two reads agree

Both worked from the fetched CGTrader text rather than recollection, and both reached
the same operative conclusions independently:

- **Kitbashing is expressly licensed.** s21A.4 grants "modify, create derivative works
  of" to a buyer using the product solely as an Incorporated Product. Assembly is not
  the problem.
- **Extraction is the whole question.** Definition 8 plus s21A.2/s21A.3's affirmative
  duty. Embedded in the executable is defensible; a peelable loose `.glb` is not.
- **Recognisability is a red herring** against the seller's licence.
- **Kit B carries its own Custom/no-AI terms** and must not be folded into Royalty Free.
- **The uncertain link is identical**: whether embedding actually discharges s21A.3 is
  industry custom, not anything adjudicated. Both send exactly that to a lawyer.

Both also independently reproduced the prior adjudication recorded in #347 on
2026-08-24, from primary sources, without mirroring its claims.

### Where they diverge, and what each divergence is worth

**1. A second, stronger counter-argument.** This doc's counter is that the MIT loader is
effectively a published extraction manual, which makes "requires reverse-engineering
tools" thin. The second read raises a different and broader one: a court (or CGTrader
enforcing on a seller's behalf) could read s21A.3 strictly — hold that a shipped asset
is *by definition* accessible to the player's machine, and that any client rendering a
mesh has effectively distributed it. Under that reading **no amount of embedding
saves you**, and the only clean paths are written per-pack permission or original
geometry.

Both reads judge it the unlikely construction, and for the same stated reason: the
licence's own drafting names encryption and proprietary formats as sufficient, which
gestures at the industry meaning ("you cannot unpak it casually"). It is recorded here
because it is the risk that would invalidate the plan rather than merely constrain it.

**2. A stronger requirement on the shipped form.** This doc concludes embedding
discharges the duty "about as well as anything short of encryption". The second read
asks for more: a **proprietary or encrypted bundle**, never a peelable `.glb`. The
stricter reading costs little at #345's stage and is the safer default; adopt it.

**3. Product-listing verification differed, and this is a methodological note worth
keeping.** Both reads hit 403 on direct product-page fetches. This one routed through a
reader proxy and obtained live figures (Kit A now $20, the $10 sale price in #347 having
lapsed; Kit B $8). The second declined to substitute a proxy for the source and marked
the listing labels as taken from #347's body, not re-verified.

Neither approach is wrong, and the disagreement itself is the finding: a proxy read
returned data but also a suspect artifact (Kit A's "(no AI)" suffix absent where #347
records it). **Both outcomes argue the same rule** — snapshot the licence type, price and
terms text at purchase, because the listing is not a stable citation.

**4. Trade-dress risk is raised only here.** The second read does not address it. That is
a gap in it rather than a disagreement: the exposure is real, it is third-party rather
than contractual, and no CGTrader clause can cure it. It stays a live consideration.

### What the double-read changes about confidence

Two independent derivations from the same primary text reaching the same operative
conclusion raises confidence in the *reading of the licence* materially. It raises
confidence in "embedding satisfies s21A.3" **not at all** — both reads flagged that same
link as unadjudicated, so agreement there reflects a shared limit rather than
corroboration. That link is what a lawyer must confirm.
