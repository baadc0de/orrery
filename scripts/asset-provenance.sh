#!/usr/bin/env bash
# Bidirectional provenance guard over `assets/` (#332).
#
#   ./scripts/asset-provenance.sh              check the working tree
#   ./scripts/asset-provenance.sh --self-test  prove the check still bites
#
# ASSET_PROVENANCE_ROOT overrides the tree checked (the self-test uses this to
# point at synthetic fixture forests; it defaults to the checkout root).
#
# ── Why this exists ──────────────────────────────────────────────────────────
#
# This repository is PUBLIC, which makes committing an asset file an act of
# *redistribution*. Marketplace licences routinely permit using an asset in a
# compiled product while forbidding exactly that, and conversion to Bevy's
# native format strips whatever metadata the original carried — so once the
# bytes are in the tree, nothing beside them can say where they came from
# unless the repo itself says it. `assets/provenance.toml` is where the repo
# says it: one TOML entry per asset file, recording source, licence text or a
# stable link to it, author, retrieval date, and the conversion that produced
# the file. The licensing bar — explicitly permitting public-repo
# redistribution, not "the download said free" — is stated in
# docs/15-asset-provenance.md; this script is what keeps the statement true.
#
# The shape is #317's DISK_TELEMETRY_JOBS guard (`scripts/gate-status.sh`),
# which refuses both a listed job that stops emitting and an unlisted job that
# starts. Here that is two inventories and two refusals:
#
#   * every regular file under `assets/` other than the manifest itself must
#     have an entry whose sha256 matches its bytes — an asset with no
#     provenance fails, naming the file;
#   * every entry must name a file that exists — an entry whose file vanished
#     fails the other way, naming the path.
#
# Three further clauses hold the policy rather than just the bookkeeping:
#
#   * the entry's licence identifier must be on the allowlist below, each of
#     which records why it permits public-repo redistribution. An identifier
#     not on the list fails, so a new licence is confronted once, by a human,
#     and mechanical forever after;
#   * every entry carries the licence text itself or a stable link to it —
#     a marketplace category name is not a licence;
#   * no `.glb`/`.gltf` file may sit outside `assets/` (pruned directories
#     excepted) — converted models straying into `crates/` or `docs/` escape
#     the manifest by construction, so the stray direction is checked
#     tree-wide, not just inside the managed root.
#
# Two size ceilings are enforced here rather than stated in prose: a per-file
# cap and a total cap over everything under `assets/`. They are the strict-
# budget weight strategy recommended in docs/15-asset-provenance.md — every CI
# lane checks this repository out, so asset weight taxes all of them, and a
# ceiling nobody enforces is a suggestion.
#
# The manifest is TOML, decoded by python3's stdlib `tomllib` (3.11+). The
# split of labour is deliberate: python answers "is the manifest well-formed?"
# (schema, types, duplicates, control characters that would corrupt the TSV
# handoff) and dies loudly there; bash answers "does it cover reality?" — the
# set operations, hashes, ceilings and the stray scan.
set -euo pipefail

ROOT="${ASSET_PROVENANCE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
MANIFEST="$ROOT/assets/provenance.toml"

# Weight strategy: strict budget, committed directly (docs/15-asset-provenance.md).
# Per-file 512 KiB, 2 MiB total — measured against a 9.58 MB repository, these
# bound the clone tax below roughly a quarter of current size while leaving
# room for the handful of models a skin actually ships.
readonly MAX_ASSET_BYTES=$((512 * 1024))
readonly MAX_TOTAL_BYTES=$((2 * 1024 * 1024))

# Licences this repository has accepted as explicitly permitting
# redistribution in a public repository. Each value records WHY, because the
# reason is the thing a reviewer extends: adding an identifier here is a
# deliberate edit confronting its terms, after which the check is mechanical.
#
# Deliberately absent: CC-BY-ND-* (our pipeline converts formats, which is an
# adaptation ND forbids outright) and CC-BY-NC-* / marketplace "royalty-free"
# categories (use permission is not redistribution permission, and NC terms do
# not survive contact with unknown downstream users of a public repository).
# See docs/15-asset-provenance.md for the argument.
declare -Ar LICENCE_ALLOWLIST=(
  ["CC0-1.0"]="public-domain dedication; redistribution unrestricted"
  ["CC-BY-4.0"]="redistribution permitted with attribution, commercial or not"
  ["CC-BY-SA-4.0"]="redistribution permitted with attribution, share-alike"
  ["MIT"]="redistribution permitted with the licence text"
  ["Apache-2.0"]="redistribution permitted with NOTICE attribution"
  ["BSD-2-Clause"]="redistribution permitted with the licence text"
  ["BSD-3-Clause"]="redistribution permitted with the licence text"
  ["OFL-1.1"]="font redistribution and embedding permitted"
)

die() { echo "::error::$*" >&2; exit 1; }

# Decode the manifest into `path<TAB>sha256<TAB>licence` lines, dying on every
# way a manifest can be malformed: missing or empty required fields, neither
# licence text nor link, a sha256 that is not lowercase hex, a retrieval date
# that is not ISO, a path escaping the managed root, duplicate paths, unknown
# keys (which is how "licence" misspelled as "liscence" gets caught), and tab
# or newline characters anywhere (they would corrupt this very handoff).
decode_manifest() {
  python3 - "$1" <<'PYEOF'
import re, sys, tomllib

with open(sys.argv[1], "rb") as f:
    doc = tomllib.load(f)

assets = doc.get("asset")
if not isinstance(assets, list):
    sys.exit(f"{sys.argv[1]}: no [[asset]] entries")

REQUIRED = ["path", "sha256", "source_url", "licence",
            "author", "retrieved", "conversion"]
KNOWN = set(REQUIRED) | {"licence_url", "licence_text", "notes"}
seen = set()
for i, e in enumerate(assets, 1):
    label = f"entry #{i}"
    if not isinstance(e, dict):
        sys.exit(f"{label}: not a table")
    unknown = sorted(set(e) - KNOWN)
    if unknown:
        sys.exit(f"{label} ({e.get('path', '?')}): unknown key(s): {', '.join(unknown)}")
    missing = [k for k in REQUIRED if not isinstance(e.get(k), str) or not e[k].strip()]
    if missing:
        sys.exit(f"{label} ({e.get('path', '?')}): missing or empty: {', '.join(missing)}")
    if not (isinstance(e.get("licence_url"), str) and e["licence_url"].strip()) \
       and not (isinstance(e.get("licence_text"), str) and e["licence_text"].strip()):
        sys.exit(f"{label} ({e['path']}): neither licence_url nor licence_text "
                 "- a licence name without its text stands behind nothing")
    if not re.fullmatch(r"[0-9a-f]{64}", e["sha256"]):
        sys.exit(f"{label} ({e['path']}): sha256 is not 64 lowercase hex chars")
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", e["retrieved"]):
        sys.exit(f"{label} ({e['path']}): retrieved is not an ISO date")
    p = e["path"]
    if p.startswith("/") or "\\" in p or any(part == ".." for part in p.split("/")):
        sys.exit(f"{label}: path escapes the managed root: {p}")
    for k, v in e.items():
        if isinstance(v, str) and any(c in v for c in "\t\n\r"):
            sys.exit(f"{label} ({e['path']}): control character in {k}")
    if p in seen:
        sys.exit(f"{label}: duplicate path: {p}")
    seen.add(p)
    print(f"{p}\t{e['sha256']}\t{e['licence']}")
PYEOF
}

# One pass over the world: violations accumulate in `bad`, are printed one per
# line, and any at all is a failure — the same contract as gate-status's
# check_disk_telemetry.
check_tree() {
  local root=$1 manifest="$1/assets/provenance.toml" bad=""

  # Structural floor: the manifest is committed infrastructure. An assets/
  # directory deleted wholesale takes its obligations with it unless this says
  # otherwise — so it says otherwise.
  [[ -f $manifest ]] || bad+="${bad:+
}no provenance manifest at ${manifest#$root/}"

  local entries=0 entry_lines=""
  if [[ -f $manifest ]]; then
    # A malformed manifest is a violation like any other rather than a crash,
    # and stderr is folded in because the decoder's diagnosis lives there.
    if ! entry_lines=$(decode_manifest "$manifest" 2>&1); then
      bad+="${bad:+
}malformed provenance manifest:
$entry_lines"
    fi
  fi

  # Forward inventory: every manifested path must exist, hash to its declared
  # digest, and carry an allowlisted licence.
  declare -A manifested=()
  local path sha licence
  while IFS=$'\t' read -r path sha licence; do
    [[ -n $path ]] || continue
    manifested["$path"]=1
    entries=$((entries + 1))
    local file="$root/assets/$path"
    if [[ ! -f $file ]]; then
      bad+="${bad:+
}provenance entry '$path' names a file that does not exist"
      continue
    fi
    local actual
    actual=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$file")
    [[ $actual == "$sha" ]] \
      || bad+="${bad:+
}provenance entry '$path' declares sha256 $sha but the file hashes to $actual"
    if [[ -z ${LICENCE_ALLOWLIST[$licence]+x} ]]; then
      bad+="${bad:+
}provenance entry '$path' carries licence '$licence', which is not on the allowlist - extend scripts/asset-provenance.sh only after reading its actual terms"
    fi
  done <<<"$entry_lines"

  # Reverse inventory: every file under assets/ except the manifest itself
  # must be manifested, and within the per-file ceiling.
  local file rel size total_bytes=0 files=0
  while IFS= read -r -d '' file; do
    rel=${file#"$root/assets/"}
    files=$((files + 1))
    size=$(wc -c <"$file")
    total_bytes=$((total_bytes + size))
    (( size <= MAX_ASSET_BYTES )) \
      || bad+="${bad:+
}'$rel' is $size bytes, over the per-asset ceiling of $MAX_ASSET_BYTES"
    [[ -n ${manifested[$rel]+x} ]] \
      || bad+="${bad:+
}asset file 'assets/$rel' has no provenance entry"
  done < <(find "$root/assets" -type f ! -name provenance.toml -print0 2>/dev/null | sort -z)

  (( total_bytes <= MAX_TOTAL_BYTES )) \
    || bad+="${bad:+
}assets/ totals $total_bytes bytes, over the ceiling of $MAX_TOTAL_BYTES - docs/15-asset-provenance.md's budget is enforced, not advisory"

  # Stray scan: a loadable model outside the managed root escapes the manifest
  # by construction, wherever it hides. Pruned: VCS internals, build output,
  # agent worktrees (full sibling checkouts), the dev cluster, vendored
  # upstream code and node junk — none of them places this repo's art belongs.
  local stray
  while IFS= read -r stray; do
    [[ -n $stray ]] || continue
    bad+="${bad:+
}loadable model '${stray#"$root"/}' sits outside assets/ and so outside provenance - move it under assets/ and manifest it"
  done < <(find "$root" \
      \( -path "$root/.git" -o -path "$root/target" -o -path "$root/.claude" \
         -o -path "$root/.fdb-dev" -o -path "$root/vendor" -o -path "$root/node_modules" \) -prune -o \
      -type f \( -name '*.glb' -o -name '*.gltf' \) \
      ! -path "$root/assets/*" -print 2>/dev/null)

  if [[ -n $bad ]]; then
    printf '%s\n' "$bad"
    return 1
  fi

  printf '%s\n' "ok: $entries entries cover $files files, $total_bytes of $MAX_TOTAL_BYTES bytes; no strays"
}

# ── Self-test ────────────────────────────────────────────────────────────────
#
# The same idiom as the other gates: prove the assertions still assert,
# per-commit, against synthetic forests written to disk. Every case copies a
# healthy fixture tree and breaks ONE thing in the guarded stage, then calls
# the very check_tree the real invocation runs — the fixtures are data, never
# a haystack of this file's own text.
#
# The anti-vacuity property is structural, and worth stating: each failing
# case asserts on the OFFENDER'S OWN NAME (a filename invented for the case),
# which appears nowhere in this script outside the fixture builder. Mutating
# the checker's message strings cannot make a case pass vacuously, because the
# assertion literal lives in the data, not the code — the inverse of the
# shared-literal trap hit in p2-kill9-gate.sh, where mutating every occurrence
# of a literal satisfied a count-based clause while the stage stayed broken.
self_test() {
  command -v python3 >/dev/null || die "self-test needs python3"
  python3 -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)' \
    || die "self-test needs python3 >= 3.11 for stdlib tomllib"

  local base failures=0 rc out
  base=$(mktemp -d)
  trap 'rm -rf "$base"' RETURN

  # A healthy forest: two manifested fixtures, correct digests. Built by the
  # same generator logic documented in assets/provenance.toml — a minimal but
  # VALID glTF 2.0 empty scene, so the fixtures stay honest about the format
  # they claim to exercise.
  build_forest() {
    local dst="$1" js pad
    mkdir -p "$dst/assets/fixtures"
    python3 - "$dst" <<'PYEOF'
import json, struct, sys
dst = sys.argv[1]
js = json.dumps({"asset": {"version": "2.0"}, "scene": 0,
                 "scenes": [{"nodes": []}]}, separators=(",", ":")).encode()
pad = (-len(js)) % 4
jc = js + b" " * pad
open(dst + "/assets/fixtures/probe.glb", "wb").write(
    struct.pack("<III", 0x46546C67, 2, 12 + 8 + len(jc))
    + struct.pack("<I", len(jc)) + b"JSON" + jc)
open(dst + "/assets/fixtures/probe.gltf", "wb").write(js + b"\n")
PYEOF
    cat > "$dst/assets/provenance.toml" <<EOF
[[asset]]
path = "fixtures/probe.glb"
sha256 = "$(sha256sum "$dst/assets/fixtures/probe.glb" | cut -d' ' -f1)"
source_url = "fixture"
licence = "CC0-1.0"
licence_text = "fixture"
author = "fixture"
retrieved = "2026-08-23"
conversion = "none"

[[asset]]
path = "fixtures/probe.gltf"
sha256 = "$(sha256sum "$dst/assets/fixtures/probe.gltf" | cut -d' ' -f1)"
source_url = "fixture"
licence = "CC0-1.0"
licence_text = "fixture"
author = "fixture"
retrieved = "2026-08-23"
conversion = "none"
EOF
  }

  expect() { # <case name> <pass|fail> <must-name substring> [forest dir]
    local name=$1 want=$2 needle=$3 dir=${4:-}
    rc=0
    ASSET_PROVENANCE_ROOT="$dir" check_tree "$dir" >"$base/out.txt" 2>"$base/err.txt" || rc=$?
    if [[ $want == pass && $rc -ne 0 ]]; then
      echo "FAIL: $name should have passed:" >&2; sed 's/^/  /' "$base/out.txt" "$base/err.txt" >&2
      failures=$((failures + 1)); return
    elif [[ $want == fail && $rc -eq 0 ]]; then
      echo "FAIL: $name should have failed, exited 0" >&2
      failures=$((failures + 1)); return
    fi
    if [[ $want == fail ]] && ! grep -qF -- "$needle" "$base/out.txt"; then
      echo "FAIL: $name failed but did not name the offender ('$needle'):" >&2
      sed 's/^/  /' "$base/out.txt" >&2
      failures=$((failures + 1)); return
    fi
    echo "  ok: $name"
  }

  # 1. Healthy forest passes — the check is not a constant failure, and its
  #    success line reports what was actually covered (never a silent pass).
  build_forest "$base/good"
  expect "a fully-covered forest passes" pass "" "$base/good"

  # 2. THE forward tooth: an unmanifested asset file fails, naming it.
  cp -r "$base/good" "$base/unmanifested"
  printf 'stray bytes' > "$base/unmanifested/assets/fixtures/sneaky.glb"
  expect "an unmanifested asset file is refused, naming it" fail \
    "sneaky.glb" "$base/unmanifested"

  # 3. THE reverse tooth: a file an entry names going missing fails the other
  #    way, naming the path.
  cp -r "$base/good" "$base/vanished"
  rm "$base/vanished/assets/fixtures/probe.glb"
  expect "a vanished file a manifest entry names is refused" fail \
    "probe.glb" "$base/vanished"

  # 4. Bytes swapped under an unchanged digest — the sha clause binds
  #    provenance to the exact file, so replacing the content without new
  #    provenance is caught.
  cp -r "$base/good" "$base/swapped"
  printf 'different bytes entirely' > "$base/swapped/assets/fixtures/probe.glb"
  expect "bytes diverging from the declared sha256 are refused" fail \
    "probe.glb" "$base/swapped"

  # 5. A licence off the allowlist — a marketplace category name is exactly
  #    what this clause exists to refuse.
  cp -r "$base/good" "$base/badlic"
  sed -i 's/licence = "CC0-1.0"/licence = "Royalty-Free"/' \
    "$base/badlic/assets/provenance.toml"
  expect "a non-redistribution licence is refused" fail \
    "Royalty-Free" "$base/badlic"

  # 6. An entry with a licence NAME but neither text nor link.
  cp -r "$base/good" "$base/noref"
  sed -i '/^licence_text/d' "$base/noref/assets/provenance.toml"
  expect "an entry with no licence text or link is refused" fail \
    "licence" "$base/noref"

  # 7. The stray direction: a loadable model outside assets/.
  cp -r "$base/good" "$base/stray"
  printf 'glTF' > "$base/stray/crates-somewhere.glb"
  expect "a model outside assets/ is refused, naming it" fail \
    "crates-somewhere.glb" "$base/stray"

  # 8. The manifest deleted wholesale is structural failure, not silence.
  cp -r "$base/good" "$base/nomanifest"
  rm "$base/nomanifest/assets/provenance.toml"
  expect "a missing manifest is refused" fail \
    "no provenance manifest" "$base/nomanifest"

  # 9. A duplicate entry — the decoder refuses, so one file cannot quietly
  #    carry two conflicting stories about where it came from.
  cp -r "$base/good" "$base/dup"
    cat >> "$base/dup/assets/provenance.toml" <<'EOF'
[[asset]]
path = "fixtures/probe.glb"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
source_url = "conflicting story"
licence = "CC0-1.0"
licence_text = "fixture"
author = "fixture"
retrieved = "2026-08-23"
conversion = "none"
EOF
  expect "a duplicated path is refused" fail \
    "duplicate path" "$base/dup"

  # 10. Over-budget bytes: the ceiling is enforced, not advisory. The per-
  #     file cap trips first (the probe is tiny), which is the clause a real
  #     oversized model would meet.
  cp -r "$base/good" "$base/fat"
  head -c $((MAX_ASSET_BYTES + 1)) /dev/zero > "$base/fat/assets/fixtures/fat.glb"
  expect "a file over the per-asset ceiling is refused" fail \
    "fat.glb" "$base/fat"

  (( failures == 0 )) || die "$failures self-test case(s) failed"

  # Live tree: the committed manifest covers the committed fixtures. Run AFTER
  # the synthetic cases so a broken checker fails here with the cases above as
  # diagnosis, and so the live pass can never substitute for the teeth.
  local live_out
  live_out=$(check_tree "$ROOT") || die "live tree fails its own provenance check:
$live_out"
  echo "asset-provenance self-test: 10/10 · $live_out"
}

if [[ "${1:-}" == "--self-test" ]]; then
  self_test
  exit 0
fi

out=$(check_tree "$ROOT") || {
  printf '::error::%s\n' "$out"
  echo "asset provenance check FAILED:" >&2
  sed 's/^/  /' <<<"$out" >&2
  exit 1
}
echo "$out"
