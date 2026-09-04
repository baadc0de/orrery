#!/usr/bin/env python3
"""Render spike #1045's tables from the run outputs.

    docs/spikes/1045-moving-interiors/summarize.py [RESULTS_DIR]

Reads results/unreal/summary-*.json (the Unreal runs) and
results/rollback-*.json (the C consumer's rollback runs) and prints the
drift, hitch, CMC and rollback tables as Markdown, so the README's numbers
are produced, not typed.
"""
import glob
import json
import os
import sys

results = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "results")

SCENES = ["rest", "straight", "roll", "mech", "transitions"]
VARIANTS = ["mirror", "cmc", "cmc_nobase", "cmc_drive"]


def load(pattern):
    out = {}
    for path in sorted(glob.glob(os.path.join(results, pattern))):
        with open(path) as f:
            d = json.load(f)
        out[os.path.basename(path)] = d
    return out


ue = load("unreal/summary-*.json")
offscreen = load("unreal-offscreen/summary-*.json")
c = load("rollback-*.json")


def find(scene, variant, interior="resident"):
    return ue.get(f"summary-{scene}-{variant}-{interior}.json")


print("### Drift (mm), per variant x scene — Unreal, per-grid local frame\n")
print("`direct` is the relative transform the mirror holds minus the ruleset's local pose; `reproj` is the frame's Unreal world transform inverted over the mirror's world location, minus the same pose (where LWC enters); `cmc` is the capsule's frame-local position after CharacterMovementComponent's own update, minus the pose it was given.\n")
print("| scene | variant | direct p50 / p99 / max | reproj p50 / p99 / max | cmc p50 / p99 / max | ticks | chain = C run |")
print("|---|---|---|---|---|---|---|")
chains = {}
for line in open(os.path.join(results, "chains.txt")):
    if line.startswith("trace "):
        kv = dict(t.split("=", 1) for t in line.split()[1:])
        chains[kv["scene"]] = kv["chain"]
for scene in SCENES:
    for variant in VARIANTS:
        d = find(scene, variant)
        if not d:
            continue
        dm = d["drift_mm"]
        same = "yes" if chains.get(scene) == d["chain"] else f"NO ({d['chain']} vs {chains.get(scene)})"
        f = lambda k: f"{dm[k]['p50']:.3g} / {dm[k]['p99']:.3g} / {dm[k]['max']:.3g}"
        print(f"| {scene} | {variant} | {f('direct')} | {f('reproj')} | {f('cmc') if variant != 'mirror' else '—'} | {d['ticks']} | {same} |")

print("\n### CMC verdict as a number — assertions per 36,000 ticks\n")
print("An assertion is a tick on which the capsule, after CMC's own update, sits more than 1 mm from the pose the mirror wrote (variants cmc, cmc_nobase) or from the ruleset's pose (cmc_drive, where the mirror never writes the capsule).\n")
print("| scene | variant | assertions | vertical-only | horizontal | with based-movement delta | ticks walking / falling / flying | base as expected |")
print("|---|---|---|---|---|---|---|---|")
for scene in SCENES:
    for variant in VARIANTS[1:]:
        d = find(scene, variant)
        if not d:
            continue
        t = d["cmc_ticks"]
        print(f"| {scene} | {variant} | {d['cmc_assertions']} / {d['cmc_assertion_ticks']} | {d['cmc_assertions_vertical_only']} | {d['cmc_assertions_horizontal']} | {d['cmc_assertions_with_based_movement']} | {t['walking']} / {t['falling']} / {t['flying']} | {t['base_ok']} |")

print("\n### Hitches — frames over 16.7 ms within ±120 ticks of each transition (Unreal, NullRHI unless stated)\n")
print("| scene | variant | interior | transition | n | hitches | with GC | with spawn/destroy | max frame ms in window | frame that stepped the transition, ms | steady p50 / p99 / max ms | first frame ms |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|")
for name, d in ue.items():
    kinds = {}
    for t in d["transitions"]:
        k = kinds.setdefault(t["kind"], {"n": 0, "h": 0, "gc": 0, "sp": 0, "max": 0.0, "at": 0.0})
        k["n"] += 1
        k["h"] += t["hitches"]
        k["gc"] += t["hitches_with_gc"]
        k["sp"] += t["hitches_with_spawn"]
        k["max"] = max(k["max"], t["max_frame_ms"])
        k["at"] = max(k["at"], t["transition_frame_ms"])
    fm = d["frame_ms"]
    for kind, k in kinds.items():
        print(f"| {d['scene']} | {d['variant']} | {d['interior']} | {kind} | {k['n']} | {k['h']} | {k['gc']} | {k['sp']} | {k['max']:.2f} | {k['at']:.2f} | {fm['p50']:.2f} / {fm['p99']:.2f} / {fm['max']:.2f} | {fm.get('first', 0):.0f} |")

print("\nRendered (`-RenderOffScreen`, Vulkan on the RTX 4090, no window; `unreal-offscreen/`, PSO cache warm, no screenshots):\n")
print("| scene | variant | interior | cycles | transition | n | hitches | with GC | max frame ms in window | transition frame ms (max) | steady p50 / p99 / max ms | first frame ms |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|")
for name, d in offscreen.items():
    kinds = {}
    for t in d["transitions"]:
        k = kinds.setdefault(t["kind"], {"n": 0, "h": 0, "gc": 0, "max": 0.0, "at": 0.0})
        k["n"] += 1
        k["h"] += t["hitches"]
        k["gc"] += t["hitches_with_gc"]
        k["max"] = max(k["max"], t["max_frame_ms"])
        k["at"] = max(k["at"], t["transition_frame_ms"])
    fm = d["frame_ms"]
    for kind, k in kinds.items():
        print(f"| {d['scene']} | {d['variant']} | {d['interior']} | {d['ticks'] // 600} | {kind} | {k['n']} | {k['h']} | {k['gc']} | {k['max']:.2f} | {k['at']:.2f} | {fm['p50']:.2f} / {fm['p99']:.2f} / {fm['max']:.2f} | {fm.get('first', 0):.0f} |")

print("\n### Rollback in the Unreal process (transitions scene, one correction per frame change, shape `ship`)\n")
print("| variant | interior | corrections | hash mismatches in window | FrameChanged re-emitted by replay | avatar frame differs after correction | presentation residual max mm | restore+install+replay ns p50 / p99 / max |")
print("|---|---|---|---|---|---|---|---|")
for name, d in ue.items():
    r = d.get("rollback")
    if not r:
        continue
    print(f"| {d['variant']} | {d['interior']} | {r['corrections']} | {r['mismatch_window']} | {r['events_reemitted_by_replay']} | {r['avatar_frame_differs_after_correction']} | {r['presentation_residual_mm_max']} | {r['total_ns_p50']:.0f} / {r['total_ns_p99']:.0f} / {r['total_ns_max']:.0f} |")

print("\n### Rollback across the frame change — C consumer against the stand-in authority, hash for hash\n")
print("| scene | transitions | corrections | spanning a frame change | mismatches in window | mismatches after | rollback / snap | events re-emitted | total ns p50 / p99 / max |")
print("|---|---|---|---|---|---|---|---|---|")
for name, d in c.items():
    spanning = sum(b["n"] for b in d["by_transition"] if b["transition"] != "control")
    print(f"| {d['scene']} | {d['transitions']} | {d['corrections']} | {spanning} | {d['mismatch_window']} | {d['mismatch_after']} | {d['rollback']} / {d['snap']} | {d['events_reemitted_by_replay']} | {d['total_ns']['p50']} / {d['total_ns']['p99']} / {d['total_ns']['max']} |")

t = c.get("rollback-transitions.json")
if t:
    print("\nPer transition kind and correction shape (transitions scene, 24 cycles):\n")
    print("| transition | shape | n | mismatches in window | after | hashes changed vs abandoned timeline | residual mm max | ns p50 / p99 |")
    print("|---|---|---|---|---|---|---|---|")
    for b in t["by_transition"]:
        print(f"| {b['transition']} | {b['shape']} | {b['n']} | {b['mismatch_window']} | {b['mismatch_after']} | {b['hashes_changed']} | {b['residual_mm_max']} | {b['total_ns_p50']} / {b['total_ns_p99']} |")
    print("\nMech scene (second nesting level; `rollback-mech.json` plus one run per divergent shape):\n")
    print("| transition | shape | n | mismatches in window | after | hashes changed | residual mm max |")
    print("|---|---|---|---|---|---|---|")
    for name in ["rollback-mech.json", "rollback-mech-ship.json", "rollback-mech-avatar.json"]:
        m = c.get(name)
        if not m:
            continue
        for b in m["by_transition"]:
            print(f"| {b['transition']} | {b['shape']} | {b['n']} | {b['mismatch_window']} | {b['mismatch_after']} | {b['hashes_changed']} | {b['residual_mm_max']} |")

print("\n### Every frame over 16.7 ms in every Unreal run, attributed (from the per-tick CSVs; first frame excluded)\n")
print("| run | ticks | frames > 16.7 ms | of which in a frame that ran garbage collection | max ms |")
print("|---|---|---|---|---|")
for path in sorted(glob.glob(os.path.join(results, "unreal", "ticks-*.csv"))):
    n = h = g = 0
    mx = 0.0
    with open(path) as f:
        next(f)
        next(f)
        for line in f:
            cols = line.split(",")
            n += 1
            ms = float(cols[1])
            mx = max(mx, ms)
            if ms > 16.7:
                h += 1
                if int(cols[3]) > 0:
                    g += 1
    print(f"| {os.path.basename(path)[6:-4]} | {n + 1} | {h} | {g} | {mx:.2f} |")
