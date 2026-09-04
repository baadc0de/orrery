#!/usr/bin/env python3
"""Spike #1046 — where do the bytes that differ between two cooks of one seed live?
Compares cooked-same-a vs cooked-same-b (spike 2's out-256a / out-256b editor saves of seed 1 body 2,
each cooked for the Mac target), per file: sha1, differing byte count, runs of differing offsets, and
for each run the bytes on both sides plus the nearest ASCII names in the file, so the run can be
attributed (GUID, hash, name table entry ...). Runs on the Mac; output is results/cooked-same-seed-diff.txt.
"""
import hashlib, os, sys

R = os.path.expanduser('~/Development/orrery-onebody/s1046')
base = 'OneBodyCook/Content/Bodies'

def runs(offsets):
    out = []
    for o in offsets:
        if out and o <= out[-1][1] + 1:
            out[-1][1] = o
        else:
            out.append([o, o])
    return out

def nearest_strings(buf, pos, window=96, minlen=6):
    lo, hi = max(0, pos - window), min(len(buf), pos + window)
    seg = buf[lo:hi]
    found, cur, start = [], [], None
    for i, ch in enumerate(seg):
        if 32 <= ch < 127:
            if start is None: start = i
            cur.append(chr(ch))
        else:
            if len(cur) >= minlen: found.append((lo + start, ''.join(cur)))
            cur, start = [], None
    if len(cur) >= minlen: found.append((lo + start, ''.join(cur)))
    return found

for fn in sorted(os.listdir(f'{R}/cooked-same-a/{base}')):
    a = open(f'{R}/cooked-same-a/{base}/{fn}', 'rb').read()
    b = open(f'{R}/cooked-same-b/{base}/{fn}', 'rb').read()
    sa, sb = hashlib.sha1(a).hexdigest(), hashlib.sha1(b).hexdigest()
    diff = [i for i in range(min(len(a), len(b))) if a[i] != b[i]]
    print(f'== {fn}: {len(a)} / {len(b)} bytes, sha1 {"EQUAL" if sa == sb else "DIFFER"} ({sa[:12]} / {sb[:12]}), {len(diff)} differing bytes, {len(runs(diff))} runs')
    for lo, hi in runs(diff):
        n = hi - lo + 1
        # widen to a 16-byte aligned window to show the whole field the run sits in
        wlo, whi = max(0, lo - 4), min(len(a), hi + 5)
        print(f'  run @{lo:>8}..{hi:<8} ({n:>3} B)  a={a[wlo:whi].hex()}  b={b[wlo:whi].hex()}')
        names = nearest_strings(a, lo)
        if names:
            print(f'      nearby ASCII: ' + '; '.join(f'@{p} "{s[:48]}"' for p, s in names[:6]))
