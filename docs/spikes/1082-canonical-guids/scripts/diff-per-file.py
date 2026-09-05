#!/usr/bin/env python3
"""Per-file sha1 + differing byte offsets between two directories (spike 4's diff-cooked-same-seed.py,
generalised to take the two directories and a glob on the command line).

  diff-per-file.py <dirA> <dirB> [glob]     e.g. cooked-same-a/OneBodyCook/Content/Bodies cooked-same-b/... 'Body_2.*'

For every file matched in dirA: sizes, sha1 of each side, number of differing bytes, runs of differing
offsets, and for each run the bytes on both sides (widened by 4 bytes each way) plus the nearest ASCII
strings, so the run can be attributed (GUID, hash, name table entry ...). Exit 0 iff every file is equal.
"""
import fnmatch, hashlib, os, sys

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

def main():
    da, db = sys.argv[1], sys.argv[2]
    pat = sys.argv[3] if len(sys.argv) > 3 else '*'
    all_equal = True
    for fn in sorted(os.listdir(da)):
        if not fnmatch.fnmatch(fn, pat) or not os.path.isfile(f'{da}/{fn}'):
            continue
        a = open(f'{da}/{fn}', 'rb').read()
        b = open(f'{db}/{fn}', 'rb').read()
        sa, sb = hashlib.sha1(a).hexdigest(), hashlib.sha1(b).hexdigest()
        diff = [i for i in range(min(len(a), len(b))) if a[i] != b[i]]
        extra = abs(len(a) - len(b))
        equal = sa == sb
        all_equal &= equal
        print(f'== {fn}: {len(a)} / {len(b)} bytes, sha1 {"EQUAL" if equal else "DIFFER"} ({sa[:12]} / {sb[:12]}), {len(diff)} differing bytes in {len(runs(diff))} runs' + (f', {extra} bytes of length difference' if extra else ''))
        for lo, hi in runs(diff):
            n = hi - lo + 1
            wlo, whi = max(0, lo - 4), min(len(a), hi + 5)
            print(f'  run @{lo:>8}..{hi:<8} ({n:>3} B)  a={a[wlo:whi].hex()}  b={b[wlo:whi].hex()}')
            names = nearest_strings(a, lo)
            if names:
                print('      nearby ASCII: ' + '; '.join(f'@{p} "{s[:48]}"' for p, s in names[:6]))
    sys.exit(0 if all_equal else 1)

main()
