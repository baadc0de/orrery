# Spike: design brief → concept → orthographic views → callout sheet (G12, concept stage)

**Date:** 2026-09-04 · **Model:** `gemini-3-pro-image` (Nano Banana Pro) on Vertex AI, `global` endpoint · **Cost:** ≈ $3 (10 images at 1,120 output tokens, one failed call at 13,440) · **Status:** research, nothing ships.

`ortho_callouts.py` takes a brief (`brief-escort.md`), generates a ¾ concept, four orthographic views conditioned on the concept, and a composed callout sheet. Every artifact has a `.provenance.json` (brief hash, prompt, references, model version, response id, usage, output sha256) per G12.1. PNGs are gitignored: G12.12 makes the private store the default for generated images; they live in `out/` locally and will move to the bucket (G12.13).

## What worked

- **Brief conformance is high.** Hull number, stripe, RCS clusters, slit canopy, conduit spine, skids, fin crest, emissive thrusters all present in the concept on the first call. Two misses: silhouette read as a fighter jet rather than the brief's blunt wedge, and the hull colour warmed toward tan from the specified warm grey. Both are iteration-loop fixes, not pipeline problems.
- **Cross-view consistency is high.** Side, top and front views keep every panel, decal and fitting from the concept.
- **The composed callout sheet is the best artifact.** From concept plus views it produced labelled front, side, rear and top elevations at one scale, dimensions from the brief, material zones with PBR notes, moving parts, hardpoints, and a palette strip with the exact hex values. It also got camera semantics right where single-view calls did not.

## What did not

- **Single-view camera semantics.** "front view" and "rear view" were rendered as top-down plans on the first pass; explicit camera text ("camera directly ahead of the nose at mid-height, looking aft; the top is NOT visible") fixed front on the second pass, but rear still came back top-down. The hosted model does not honour a camera position reliably from prose. **This is the case for the open track's control maps (G12.2a): an orthographic skeleton or depth map forces the view.**
- **Design drift under a strong camera instruction.** The corrected front elevation changed the wing planform (delta → straight spars) while keeping colours and fittings. Consistency of *silhouette* across views is not guaranteed; a modeller would need to pick one view as canonical, which the callout sheet effectively does.
- **Output token budget.** Vertex's default `maxOutputTokens` truncated an image generation (`finishReason: MAX_TOKENS`, 13,440 image tokens spent, no image). Set `maxOutputTokens` high (32768 used) and `imageConfig.aspectRatio`; a 1024² image is 1,120 output tokens when it succeeds.
- **Endpoint.** Image models are served from `global`, not regional endpoints (404 on `us-central1`). A project without billing is refused; the developer API (`generativelanguage`) needs an API key, not an OAuth token.

## Recommendations for the pipeline

1. Keep the **composed sheet** as the primary callout artifact and treat single orthographic views as inputs to it, not deliverables.
2. Generate orthographic views on the **open track with control maps** when image-to-3D needs true orthographics; use the hosted model for concept and sheet.
3. Add a **silhouette-consistency check** (mask IoU of each view against the sheet's matching panel) as one of the G12.3 automated checks.
4. Always pass `maxOutputTokens` and record `usageMetadata`; the failed call cost more than five successful ones.

## Reproduce

    python3 ortho_callouts.py --project <billed-project> --location global --model gemini-3-pro-image \
        --brief brief-escort.md --out out --name escort-pro
    # resume from a saved concept, regenerate selected views and the sheet:
    python3 ortho_callouts.py ... --reuse-concept --views front,back
