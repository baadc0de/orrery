#!/bin/bash
# Spike #1046 — origin side of the one measured transfer (runs on the Mac, bojans-max, 192.168.0.155).
# Digests every s1 body (umap + tri, spike 2's two-half blake3) into digests-s1.json, then serves s1046/
# over plain HTTP for the client (transfer-client.sh on the Linux box) to fetch and verify.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
S=$HOME/Development/orrery-onebody
R=$S/s1046
CT="$S/rust/target/release/collision-trace"
cd "$R/s1" || exit 1
python3 - <<'EOF'
import json, subprocess, os
out = {}
for b in range(11, 19):
    subprocess.run([os.path.expanduser('~/Development/orrery-onebody/rust/target/release/collision-trace'), 'digest', '--unreal', f'Body_{b}.umap', '--collision', f'body-{b}.tri.collision', '--out', f'digest-{b}.json'], check=True, capture_output=True)
    d = json.load(open(f'digest-{b}.json'))
    out[str(b)] = {'digest_blake3': d['digest_blake3'], 'halves': d['halves']}
json.dump(out, open('../digests-s1.json', 'w'), indent=1)
print('digested', len(out), 'bodies')
EOF
cd "$R" && nohup python3 -m http.server 8046 --bind 0.0.0.0 > "$R/http.log" 2>&1 &
echo "http pid $!"
