#!/usr/bin/env bash
# Build-cache and target-directory hygiene for the Orrery worktrees.
#
#   ./scripts/dev-cache.sh doctor   check the cache is wired up and working
#   ./scripts/dev-cache.sh stats    hit rate and cache size
#   ./scripts/dev-cache.sh disk     what every target/ in the repo is costing
#   ./scripts/dev-cache.sh prune    delete every target/ (safe: sccache refills)
#
# Why this exists: each agent worktree keeps its own `target/`, because cargo
# takes an exclusive lock on a target directory and sharing one would serialize
# concurrent agents. sccache is the layer they share instead — so the cost of a
# worktree is its own crates, not the whole dependency graph, and `prune` is
# cheap enough to run whenever disk gets tight.
set -euo pipefail

readonly NAME=dev-cache
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

# Every target/ this repo produces: the workspace's, plus one per standalone
# tool (each declares its own `[workspace]`, so each gets its own).
target_dirs() {
  find "$ROOT" -maxdepth 3 -type d -name target -prune 2>/dev/null | sort
}

case "${1:-stats}" in
  doctor)
    command -v sccache >/dev/null \
      || die 'sccache is not installed. `pacman -S sccache`, or `cargo install sccache --locked`.'
    note "sccache: $(sccache --version)"

    grep -q 'rustc-wrapper' "$ROOT/.cargo/config.toml" 2>/dev/null \
      || die "$ROOT/.cargo/config.toml does not set build.rustc-wrapper"
    note 'repo .cargo/config.toml routes rustc through sccache'

    # The nested standalone tools must inherit the repo config too — cargo
    # walks up from the working directory, so they do, but a moved or renamed
    # config would silently drop them back to uncached builds.
    for tool in p2-load p3-island p0-nat-test p0-dashboard p2-dashboard; do
      [[ -d "$ROOT/$tool" ]] || continue
      [[ -f "$ROOT/$tool/.cargo/config.toml" ]] \
        && note "note: $tool has its own .cargo/config.toml, which shadows the repo one"
    done

    # Capture the whole report before filtering: closing sccache's stdout
    # early (an `exit` inside a piped awk) makes it die on a broken pipe.
    requests() { sccache --show-stats > "$tmp"; awk '/^Compile requests / {print $3}' "$tmp" | head -1; }
    tmp=$(mktemp)
    trap 'rm -f "$tmp"' RETURN EXIT
    before=$(requests)
    (cd "$ROOT" && cargo check -p orrery_protocol --quiet 2>/dev/null) || true
    after=$(requests)
    [[ ${after:-0} -gt ${before:-0} ]] \
      || die 'a build produced no sccache compile requests; the wrapper is not taking effect'
    note 'verified: builds reach sccache'
    echo 'doctor: build cache is wired up'
    ;;

  stats)
    command -v sccache >/dev/null || die 'sccache is not installed'
    report=$(sccache --show-stats)
    grep -E '^(Compile requests|Cache hits|Cache misses|Cache hits rate|Cache size|Max cache size|Non-cacheable calls)' \
      <<< "$report"
    ;;

  disk)
    total=0
    while read -r dir; do
      [[ -n $dir ]] || continue
      size=$(du -sk "$dir" 2>/dev/null | cut -f1)
      total=$((total + size))
      printf '%8s  %s\n' "$(du -sh "$dir" 2>/dev/null | cut -f1)" "${dir#"$ROOT"/}"
    done < <(target_dirs)
    printf '%8s  TOTAL across this checkout\n' "$(numfmt --to=iec $((total * 1024)))"
    if command -v sccache >/dev/null; then
      cache_report=$(sccache --show-stats)
      printf '%8s  sccache (shared by every worktree)\n' \
        "$(awk '/^Cache size/ {print $3 $4}' <<< "$cache_report")"
    fi
    df -h "$ROOT" | tail -1
    ;;

  prune)
    # Deleting a target/ is not destructive here: sources are in git and the
    # object cache refills the rebuild. This is the lever to pull when disk is
    # tight, and it is meant to be pulled often.
    while read -r dir; do
      [[ -n $dir ]] || continue
      note "removing ${dir#"$ROOT"/}"
      rm -rf "$dir"
    done < <(target_dirs)
    note 'done; the next build repopulates from sccache'
    ;;

  *)
    die "unknown command '${1}'; expected doctor, stats, disk, or prune"
    ;;
esac
