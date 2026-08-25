#!/usr/bin/env bash
# Build-cache and target-directory hygiene for the Orrery worktrees.
#
#   ./scripts/dev-cache.sh doctor   check the cache is wired up and working
#   ./scripts/dev-cache.sh stats    hit rate and cache size
#   ./scripts/dev-cache.sh disk     what every target/ in the repo is costing
#   ./scripts/dev-cache.sh prune    delete every target/ (safe: sources are in git)
#
# Why this exists: each agent worktree keeps its own `target/`, because cargo
# takes an exclusive lock on a target directory and sharing one would serialize
# concurrent agents. kache's local store is shared by this user's worktrees;
# see AGENTS.md § Build cache for the current box arrangement and the sccache
# post-mortem.
set -euo pipefail

readonly NAME=dev-cache
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# An optional filesystem remote is deliberately opt-in. This box has none;
# deployments which configure one may set this to have doctor verify it.
readonly SHARED_REMOTE="${KACHE_SHARED_REMOTE:-}"
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

# Every target/ this repo produces: the workspace's, plus one per standalone
# tool (each declares its own `[workspace]`, so each gets its own).
target_dirs() {
  find "$ROOT" -maxdepth 3 -type d -name target -prune 2>/dev/null | sort
}

# Total cache operations kache has recorded, across every outcome. Used to
# prove a build actually reached the cache rather than bypassing it.
cache_ops() {
  kache stats 2>/dev/null \
    | awk -F'[(),]' '/^Hit rate:/ {
        total = 0
        for (i = 2; i <= NF; i++) { if (match($i, /[0-9]+/)) total += substr($i, RSTART, RLENGTH) }
        print total
        exit
      }'
}

case "${1:-stats}" in
  doctor)
    command -v kache >/dev/null \
      || die 'kache is not installed. See https://github.com/kunobi-ninja/kache.'
    note "kache: $(kache --version)"

    grep -q 'rustc-wrapper = "kache"' "$ROOT/.cargo/config.toml" 2>/dev/null \
      || die "$ROOT/.cargo/config.toml does not route rustc through kache"
    note 'repo .cargo/config.toml routes rustc through kache'

    # The nested standalone tools must inherit the repo config too — cargo
    # walks up from the working directory, so they do, but a moved or renamed
    # config would silently drop them back to uncached builds.
    for tool in gates/p2-load gates/p3-island gates/p0-nat-test gates/p0-dashboard gates/p2-dashboard; do
      [[ -d "$ROOT/$tool" ]] || continue
      [[ -f "$ROOT/$tool/.cargo/config.toml" ]] \
        && note "note: $tool has its own .cargo/config.toml, which shadows the repo one"
    done

    # kache's own checks are diagnostic only — `kache doctor` exits 0 even when
    # it reports issues, so the assertions below remain the gates.
    kache doctor >&2 || true

    # A remote is optional. An unconfigured one is reported as such, not passed
    # as a working remote; once configured, though, it must be a writable
    # directory or cache sharing has silently stopped working.
    if [[ -z $SHARED_REMOTE ]]; then
      note 'shared remote: unconfigured (local-only cache)'
    else
      [[ -d $SHARED_REMOTE ]] \
        || die "the configured shared remote $SHARED_REMOTE does not exist"
      probe="$SHARED_REMOTE/.writetest-$(id -un)"
      ( : > "$probe" ) 2>/dev/null \
        || die "cannot write to the shared remote $SHARED_REMOTE as $(id -un)"
      rm -f "$probe"
      note "configured shared remote $SHARED_REMOTE is writable by $(id -un)"
    fi

    # And prove a compile actually reaches the cache, rather than trusting the
    # wiring. Deliberately NOT `cargo check` on a repo crate: cargo skips rustc
    # entirely when the target directory is already fresh, so that test passed
    # or failed depending on what happened to be built, which is no test at all.
    #
    # The probe has its own fixed cache under target/, so `prune` cleans it with
    # the worktree and a full or concurrently-used live cache cannot obscure
    # this run's operation count. Its source changes on every run: the resulting
    # miss must be recorded, whereas a hit may leave aggregate stats unchanged.
    probe_dir="$ROOT/target/.dev-cache-doctor-probe"
    probe_cache="$ROOT/target/.dev-cache-doctor-cache"
    mkdir -p "$probe_dir"
    probe_nonce=$(date +%s%N)
    printf 'pub fn probe() -> u128 { %s }\n' "$probe_nonce" > "$probe_dir/probe.rs"
    before=$(KACHE_CACHE_DIR="$probe_cache" cache_ops)
    KACHE_CACHE_DIR="$probe_cache" kache rustc --crate-type=lib --crate-name=kache_doctor_probe \
      --emit=metadata "$probe_dir/probe.rs" --out-dir "$probe_dir" >/dev/null 2>&1 \
      || die 'compiling through kache failed; run `kache doctor` for detail'
    after=$(KACHE_CACHE_DIR="$probe_cache" cache_ops)
    [[ ${after:-0} -gt ${before:-0} ]] \
      || die 'a compile produced no kache activity; the wrapper is not taking effect'
    note "verified: compiles reach kache ($((after - before)) cache operation(s) recorded)"
    echo 'doctor: build cache is wired up'
    ;;

  stats)
    command -v kache >/dev/null || die 'kache is not installed'
    kache stats
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
    if command -v kache >/dev/null; then
      printf '%8s  kache local store (%s)\n' \
        "$(du -sh "${KACHE_CACHE_DIR:-$HOME/.cache/kache}" 2>/dev/null | cut -f1)" "$(id -un)"
    fi
    if [[ -z $SHARED_REMOTE ]]; then
      printf '%8s  kache shared remote (unconfigured; local-only cache)\n' '-'
    elif [[ -d $SHARED_REMOTE ]]; then
      printf '%8s  kache shared remote (configured)\n' \
        "$(du -sh "$SHARED_REMOTE" 2>/dev/null | cut -f1)"
    else
      printf '%8s  kache shared remote (configured but unavailable: %s)\n' \
        '-' "$SHARED_REMOTE"
    fi
    df -h "$ROOT" | tail -1
    ;;

  prune)
    # Deleting a target/ is not destructive here: sources are in git. The local
    # object cache may accelerate the rebuild, but this box has no remote.
    while read -r dir; do
      [[ -n $dir ]] || continue
      note "removing ${dir#"$ROOT"/}"
      rm -rf "$dir"
    done < <(target_dirs)
    note 'done; the next build repopulates from kache'
    ;;

  *)
    die "unknown command '${1}'; expected doctor, stats, disk, or prune"
    ;;
esac
