#!/bin/bash
# Spike #1046 — client side of the one measured transfer (runs on the Linux box, 192.168.0.120 on wlan0).
# Fetches the 8-body season bundle from the Mac over the home LAN (Mac on Ethernet/WiFi at 192.168.0.155,
# this box on WiFi; tailscale reports the path as direct, 6 ms RTT), records bytes and wall seconds from
# curl itself, extracts, and re-runs spike 2's digest per body on the client, comparing with the origin's
# digests-s1.json. A transfer from a local disk cache is not a transfer: the URL is the Mac's LAN address.
set -o pipefail
ORIGIN="${ORIGIN:-http://192.168.0.155:8046}"
CT="${CT:-/tmp/claude-1000/-home-baadc0de-Development-baadc0de-orrery/ff3037fe-b526-49a8-be85-0d1ab0b54199/scratchpad/ct/target/release/collision-trace}"
W="${W:-/tmp/claude-1000/-home-baadc0de-Development-baadc0de-orrery/ff3037fe-b526-49a8-be85-0d1ab0b54199/scratchpad/xfer}"
rm -rf "$W"; mkdir -p "$W"; cd "$W" || exit 1
echo "== fetch"
for f in s1-season.tar.zst s1.pak digests-s1.json; do
  curl -s -o "$f" -w "$f bytes=%{size_download} seconds=%{time_total} speed_Bps=%{speed_download} http=%{http_code}\n" "$ORIGIN/$f" | tee -a transfer.txt
done
echo "== extract + verify"
zstd -d -q s1-season.tar.zst -o s1-season.tar && tar -xif s1-season.tar 2>/dev/null
python3 - "$CT" <<'EOF'
import json, subprocess, sys
ct = sys.argv[1]
origin = json.load(open('digests-s1.json'))
ok = 0
for b in range(11, 19):
    subprocess.run([ct, 'digest', '--unreal', f'Body_{b}.umap', '--collision', f'body-{b}.tri.collision', '--out', f'client-digest-{b}.json'], check=True, capture_output=True)
    d = json.load(open(f'client-digest-{b}.json'))
    same = d['digest_blake3'] == origin[str(b)]['digest_blake3']
    ok += same
    print(f"body {b}: origin {origin[str(b)]['digest_blake3'][:16]}… client {d['digest_blake3'][:16]}… {'MATCH' if same else 'MISMATCH'}")
print(f"verified {ok}/8 bodies on the client")
json.dump({'verified': ok, 'of': 8}, open('verify.json', 'w'))
EOF
