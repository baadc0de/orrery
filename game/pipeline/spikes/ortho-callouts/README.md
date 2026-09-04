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

## Local track (added later the same day): FLUX.2 klein 4B on ComfyUI, plus a Blender proxy depth map

**Setup** (all on the box, no money): `uv` venv with torch 2.14 cu130, ComfyUI 0.34, `black-forest-labs/FLUX.2-klein-4B` via the Comfy-Org repack (Apache-2.0: diffusion model 7.7 GB, Qwen3-4B text encoder 8 GB, Flux2 VAE), TRELLIS.2-4B weights (MIT, 14 GB) and code cloned but not yet run, Blender 5.2.1 with the `blender-mcp` add-on enabled. Runner: `local_ortho.py` (ComfyUI API graph: UNETLoader → CLIPLoader `flux2` → ReferenceLatent(concept) [→ ReferenceLatent(depth)] → KSampler euler/simple, 4 steps, cfg 1 → SaveImage), provenance per output with the full graph and seed.

**Numbers:** first view 8 s (model load), then **3 s per 1024² view** (5 s with the second reference). Deterministic from seed. VRAM: ~16 GB staged by ComfyUI's dynamic loader.

**Concept-only (same prompts as hosted):** quality and design consistency are close to Nano Banana Pro (stripe, conduits, RCS pods, canopy all preserved; the `E-07` decal is the main loss). Camera semantics fail the same way: side is a true elevation, front and back come out as raised three-quarter views, top is tilted.

**With a proxy depth map as a second reference** (`proxy_depth.py`: Blender builds a box-and-cylinder proxy from the brief's 9 × 6 × 2.5 m and renders 1024² orthographic depth from four cameras via an emission material, since Blender 5 has no `scene.node_tree`): **rear became a true rear elevation and top a true plan**; front improved but still leans into a raised three-quarter. So a reference latent carries camera intent partially; it is not a hard constraint the way a ControlNet is.

**Conclusions for G12.2a.** The local track is viable today for batch variation and re-derivation at negligible cost, and a Blender proxy is a cheap, fully reproducible way to state the camera. To make it a hard constraint, the next step is a proper depth/canny ControlNet for the chosen open model (or the base 4B with a control adapter), and a silhouette IoU check against the proxy's mask as the automated gate.

**Operational.** ComfyUI is started with `~/ComfyUI/.venv/bin/python ~/ComfyUI/main.py --listen 127.0.0.1 --port 8188` and holds ~16 GB VRAM while idle; stop it before any latency measurement on this box. Generated PNGs and the proxy `.blend` stay out of the public repository (G12.12).

## Image-to-3D (TRELLIS.2-4B): built, blocked on a licence gate

The full TRELLIS.2 stack builds on this box in a user-local conda env (CUDA 12.4 toolkit + gcc 12, since the system has no nvcc and gcc 16): torch 2.6 cu124, flash-attn 2.7.3 (prebuilt; needs `TMPDIR` on the same filesystem as the pip cache), nvdiffrast, nvdiffrec (needs `-L<env>/lib/stubs` for `-lcuda`), CuMesh, FlexGEMM, o-voxel. Script: `~/trellis2-build.sh`; runner: `trellis_run.py`; Blender import-and-render: `blender_render.py` (tested on a stand-in).

**Blocker:** the 4B pipeline conditions on **DINOv3** (`facebook/dinov3-vitl16-pretrain-lvd1689m`), a gated Hugging Face repo under Meta's DINOv3 licence. The model was trained on those features, so the ungated DINOv2 extractor in the code is not a drop-in. Needs the owner to accept the licence on Hugging Face and put a token on the machine (`hf auth login`). Note for G12.8: DINOv3's licence is a versioned input of every TRELLIS.2 artifact even though the encoder never ships.
