#!/usr/bin/env python3
"""Spike #1046 — the fits. Reads the measured JSON/logs pulled back from the Mac (results/) and prints
size(n) = a + b*n and cook(n) = c + d*n with residuals, then season(N) and cook(N) at N in {12,24,36,48}
against the three anchors. Pure arithmetic over measured points; every input file is named in the output.
"""
import json, re, statistics, sys, os

HERE = os.path.dirname(os.path.abspath(__file__))
RES = os.path.join(HERE, '..', 'results')

def load(name):
    with open(os.path.join(RES, name)) as f:
        return json.load(f)

def lsq(xs, ys):
    """Least squares y = a + b x; returns a, b, residuals, max |resid|."""
    n = len(xs)
    mx, my = sum(xs) / n, sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    b = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx if sxx else 0.0
    a = my - b * mx
    resid = [y - (a + b * x) for x, y in zip(xs, ys)]
    return a, b, resid, max(abs(r) for r in resid)

def cumulative(rows, key):
    """size(n) for n = 1..8: the first n bodies' bytes summed, in body-id order."""
    rows = sorted(rows, key=lambda r: r['body'])
    out, acc = [], 0
    for r in rows:
        acc += r[key]
        out.append(acc)
    return out

def main():
    per_body = load('per-body.json')
    s1 = [r for r in per_body if r['season'] == 's1']
    s2 = [r for r in per_body if r['season'] == 's2']
    print('== per-body (measured, fs = bytes on disk, zstd19 = zstd -19 of the raw bytes)')
    print('season body instances umap_fs umap_zstd19 tri_fs tri_zstd19 hf_fs hf_zstd19 main_s')
    for r in s1 + s2:
        print(f"{r['season']} {r['body']} {r['instances']} {r['umap_fs']} {r['umap_zstd19']} {r['tri_fs']} {r['tri_zstd19']} {r['hf_fs']} {r['hf_zstd19']} {r['commandlet_main_s']:.2f}")

    print('\n== size(n) fits over the s1 bodies in id order, n = 1..8 (a = intercept bytes, b = bytes per body)')
    ns = list(range(1, 9))
    fits = {}
    for label, key in [('umap_fs', 'umap_fs'), ('umap_zstd19', 'umap_zstd19'), ('tri_fs', 'tri_fs'), ('tri_zstd19', 'tri_zstd19'), ('hf_zstd19', 'hf_zstd19')]:
        ys = cumulative(s1, key)
        a, b, resid, mr = lsq(ns, ys)
        fits[label] = (a, b, mr)
        print(f"{label:12s} a={a:12.0f} b={b:12.0f} max|resid|={mr:10.0f} ({100*mr/ys[-1]:.2f}% of size(8)); points={ys}")
    # per-body spread (the honest error bar: PCG density variance across bodies of one seed)
    print('\n== per-body spread across the 16 bodies (both seeds), the error bar on b')
    for key in ['umap_fs', 'umap_zstd19', 'tri_fs', 'tri_zstd19', 'instances']:
        vals = [r[key] for r in s1 + s2]
        print(f"{key:12s} mean={statistics.mean(vals):12.0f} sd={statistics.pstdev(vals):10.0f} min={min(vals)} max={max(vals)}")

    # cook(n) from the timing pass
    print('\n== cook(n): sequential walls from timing-cook.txt (one process per body, -NullRHI, crash reporters killed between cooks)')
    walls, mains = [], []
    par = {}
    engine_start = None
    with open(os.path.join(RES, 'timing-cook.txt')) as f:
        for line in f:
            m = re.search(r'out=t1 wall=([\d.]+) real ([\d.]+) main=([\d.]+)', line)
            if m:
                walls.append(float(m.group(1))); mains.append(float(m.group(3)))
            m = re.search(r'par(\d) wall total=([\d.]+)', line)
            if m:
                par[int(m.group(1))] = float(m.group(2))
            m = re.search(r'engine start \+ exit .* wall=([\d.]+)', line)
            if m:
                engine_start = float(m.group(1))
    cum = [sum(walls[:i + 1]) for i in range(len(walls))]
    c, d, resid, mr = lsq(list(range(1, len(walls) + 1)), cum)
    print(f"engine start alone (TraceBody, no args) = {engine_start} s")
    print(f"per-process walls = {walls}")
    print(f"commandlet_main_s = {mains}  mean={statistics.mean(mains):.2f} sd={statistics.pstdev(mains):.2f}")
    print(f"cook(n) sequential, one process per body: c={c:.1f} s, d={d:.2f} s/body, max|resid|={mr:.2f} s; cumulative={[round(x,1) for x in cum]}")
    print(f"mean wall per body-process = {statistics.mean(walls):.2f} s = ~{engine_start:.1f} s engine start + {statistics.mean(mains):.2f} s commandlet main + exit/ensure overhead")
    for k, v in sorted(par.items()):
        print(f"par{k}: {k} concurrent processes, total wall {v:.1f} s -> {v/k:.1f} s per body amortised (sequential would be {k*statistics.mean(walls):.1f} s)")

    print('\n== season(N): bodies only, before interiors. One "body" here = one 256 m x 256 m tile (the thing spike 2 measured).')
    print('   Columns: N tiles; umap+tri zstd19 (the transfer size a zstd-compressed bundle would carry); umap+tri raw (on disk after install)')
    b_z = fits['umap_zstd19'][1] + fits['tri_zstd19'][1]
    b_r = fits['umap_fs'][1] + fits['tri_fs'][1]
    b_hf = fits['umap_zstd19'][1] + fits['hf_zstd19'][1]
    for N in [12, 24, 36, 48]:
        print(f"N={N:3d} tiles: zstd19 (umap+tri) {N*b_z/1e6:8.1f} MB; raw {N*b_r/1e6:8.1f} MB; zstd19 (umap+hf) {N*b_hf/1e6:8.1f} MB")
    print(f"per tile: zstd19 umap+tri = {b_z/1e6:.2f} MB, raw = {b_r/1e6:.2f} MB")
    for anchor in [20e9, 50e9, 100e9]:
        print(f"anchor {anchor/1e9:.0f} GB: {anchor/b_z:.0f} tiles at zstd19 umap+tri; {anchor/b_r:.0f} tiles raw")

if __name__ == '__main__':
    main()
