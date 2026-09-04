#!/bin/bash
# Spike #1046 — sizes, containers and patches over the cooked bodies.
#   s1 = season seed 1001, bodies 11..18; s2 = season seed 2002, bodies 11..18 (same ids, different seed)
#   out-256a / out-256b = spike 2's same-seed cooks (the umap nondeterminism case)
# Every number is labelled by what produced it: fs = file size on disk; zstd19 = `zstd -19` of the raw
# bytes; pak = UnrealPak container (Oodle, -compress); patch = the tool named, with its settings.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
S=$HOME/Development/orrery-onebody
R=$S/s1046
UE="/Users/Shared/Epic Games/UE_5.8"
PAK="$UE/Engine/Binaries/Mac/UnrealPak"
OUTJ="$R/sizes.json"
cd "$R" || exit 1

py() { python3 - "$@"; }

echo "== per-body file sizes (fs, zstd19)"
py <<'EOF'
import json, os, subprocess, glob
R = os.path.expanduser('~/Development/orrery-onebody/s1046')
def z(p):
    return int(subprocess.run(['zstd','-19','-q','-c',p], capture_output=True).stdout.__len__())
rows = []
for season in ['s1','s2']:
    for b in range(11,19):
        d = f'{R}/{season}'
        files = {'umap': f'{d}/Body_{b}.umap', 'tri': f'{d}/body-{b}.tri.collision', 'hf': f'{d}/body-{b}.hf.collision', 'vox': f'{d}/body-{b}.vox.collision'}
        cj = json.load(open(f'{d}/body-{b}.cook.json'))
        row = {'season': season, 'body': b, 'instances': cj['instances'], 'instance_tris': cj['instance_tris'],
               'commandlet_main_s': cj['timing']['commandlet_main_s'], 'transformers_s': cj['timing']['transformers_s'], 'pcg_s': cj['timing']['pcg_s']}
        for k, p in files.items():
            row[k+'_fs'] = os.path.getsize(p); row[k+'_zstd19'] = z(p)
        rows.append(row); print(row)
json.dump(rows, open(f'{R}/per-body.json','w'), indent=1)
EOF

echo "== season bundles (tar of umap + tri, raw; then zstd19)"
for s in s1 s2; do
  tar -cf "$R/$s-umap.tar" -C "$R/$s" $(cd "$R/$s" && ls Body_*.umap)
  tar -cf "$R/$s-tri.tar" -C "$R/$s" $(cd "$R/$s" && ls body-*.tri.collision)
  tar -cf "$R/$s-hf.tar" -C "$R/$s" $(cd "$R/$s" && ls body-*.hf.collision)
  cat "$R/$s-umap.tar" "$R/$s-tri.tar" > "$R/$s-season.tar"
  for f in umap tri hf season; do zstd -19 -q -f "$R/$s-$f.tar" -o "$R/$s-$f.tar.zst"; done
done
ls -l "$R"/s?-*.tar "$R"/s?-*.tar.zst

echo "== patches between seasons (different seeds, same body ids): zstd --patch-from, xdelta3, bsdiff"
for f in umap tri season; do
  zstd -19 -q -f --patch-from="$R/s1-$f.tar" "$R/s2-$f.tar" -o "$R/patch-s1-s2-$f.zstd.patch" --memory=1024MB --long=27
  xdelta3 -e -f -9 -S djw -s "$R/s1-$f.tar" "$R/s2-$f.tar" "$R/patch-s1-s2-$f.xdelta3"
  ls -l "$R/patch-s1-s2-$f.zstd.patch" "$R/patch-s1-s2-$f.xdelta3"
done
bsdiff "$R/s1-umap.tar" "$R/s2-umap.tar" "$R/patch-s1-s2-umap.bsdiff"; ls -l "$R/patch-s1-s2-umap.bsdiff"

echo "== the unchanged-body case: same seed cooked twice (spike 2's out-256a vs out-256b), umap"
cmp -l "$S/out-256a/Body_2.umap" "$S/out-256b/Body_2.umap" | wc -l
zstd -19 -q -f --patch-from="$S/out-256a/Body_2.umap" "$S/out-256b/Body_2.umap" -o "$R/patch-same-seed-umap.zstd.patch" --memory=1024MB
xdelta3 -e -f -9 -S djw -s "$S/out-256a/Body_2.umap" "$S/out-256b/Body_2.umap" "$R/patch-same-seed-umap.xdelta3"
bsdiff "$S/out-256a/Body_2.umap" "$S/out-256b/Body_2.umap" "$R/patch-same-seed-umap.bsdiff"
ls -l "$R"/patch-same-seed-umap.*
cmp "$S/out-256a/body-2.tri.collision" "$S/out-256b/body-2.tri.collision" && echo "tri collision byte-identical across the two cooks"

echo "== UnrealPak containers (Oodle, default compression block) and UnrealPak -Diff"
for s in s1 s2; do
  RESP="$R/$s.pakresp.txt"; : > "$RESP"
  for f in "$R/$s"/Body_*.umap "$R/$s"/body-*.tri.collision; do echo "\"$f\" \"../../../OneBodyCook/Content/Bodies/$(basename "$f")\"" >> "$RESP"; done
  "$PAK" "$R/$s.pak" -Create="$RESP" -compress -compressionformats=Oodle -platform=Mac > "$R/$s.pak.log" 2>&1 || tail -5 "$R/$s.pak.log"
  "$PAK" "$R/$s-nocomp.pak" -Create="$RESP" -platform=Mac > "$R/$s-nocomp.pak.log" 2>&1 || tail -5 "$R/$s-nocomp.pak.log"
done
ls -l "$R"/s?.pak "$R"/s?-nocomp.pak
"$PAK" "$R/s1.pak" "$R/s2.pak" -Diff 2>&1 | grep -E "Unique|Different|NumEqual|Diff" | tail -5
echo "-- zstd/xdelta over the Oodle paks (what a content-agnostic patcher sees if handed UE's container)"
zstd -19 -q -f --patch-from="$R/s1.pak" "$R/s2.pak" -o "$R/patch-s1-s2.pak.zstd.patch" --memory=1024MB --long=27
xdelta3 -e -f -9 -S djw -s "$R/s1.pak" "$R/s2.pak" "$R/patch-s1-s2.pak.xdelta3"
ls -l "$R"/patch-s1-s2.pak.*
echo "-- zstd of the paks themselves (double compression)"
zstd -19 -q -c "$R/s1.pak" | wc -c; zstd -19 -q -c "$R/s1-nocomp.pak" | wc -c

echo "== SIZES DONE"
