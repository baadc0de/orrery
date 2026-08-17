#!/usr/bin/env bash
# Build-cache and target-directory hygiene for the Orrery worktrees.
#
#   ./scripts/dev-cache.sh doctor   check the cache is wired up and working
#   ./scripts/dev-cache.sh stats    hit rate and cache size
#   ./scripts/dev-cache.sh disk     what every target/ in the repo is costing
#   ./scripts/dev-cache.sh prune    delete every target/ (safe: kache refills)
#
# Why this exists: each agent worktree keeps its own `target/`, because cargo
# takes an exclusive lock on a target directory and sharing one would serialize
# concurrent agents. kache is the layer they share instead — so the cost of a
# worktree is its own crates, not the whole dependency graph, and `prune` is
# cheap enough to run whenever disk gets tight.
#
# The cache is shared by both of the box's build identities (the dev user and
# the CI runners' `ci`) through one content-addressed directory. See
# AGENTS.md § Build cache for why that works with kache and did not with
# sccache.
set -euo pipefail

readonly NAME=dev-cache
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly SHARED_REMOTE=/var/cache/kache/shared
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
    for tool in p2-load p3-island p0-nat-test p0-dashboard p2-dashboard; do
      [[ -d "$ROOT/$tool" ]] || continue
      [[ -f "$ROOT/$tool/.cargo/config.toml" ]] \
        && note "note: $tool has its own .cargo/config.toml, which shadows the repo one"
    done

    # kache's own checks, for their diagnostics: binary, config, store, remote
    # reachability, compiler probe. Informational only — `kache doctor` exits 0
    # even when it reports issues, so it cannot gate anything, and two of its
    # checks are expected to fail on this box:
    #
    #   'Daemon service not installed' — we run kache from a systemd *system*
    #   unit (kache@<user>.service) rather than its own user-unit installer,
    #   because a user unit needs lingering and a D-Bus session that the `ci`
    #   service account does not have.
    #
    #   'N daemon processes running, expected 1' — the check is not uid-aware
    #   and counts every user's daemon. One per build identity is the design.
    #
    # The assertions below are the ones that actually gate.
    kache doctor >&2 || true

    # Then the two this box cares about and kache cannot know about. The shared
    # remote is the whole point of the arrangement: if it is unwritable, both
    # identities silently fall back to private local caches and the CI/dev
    # sharing quietly stops working.
    [[ -d $SHARED_REMOTE ]] \
      || die "the shared remote $SHARED_REMOTE does not exist"
    probe="$SHARED_REMOTE/.writetest-$(id -un)"
    ( : > "$probe" ) 2>/dev/null \
      || die "cannot write to the shared remote $SHARED_REMOTE as $(id -un); check group kache and its default ACL"
    rm -f "$probe"
    note "shared remote $SHARED_REMOTE is writable by $(id -un)"

    # And prove a compile actually reaches the cache, rather than trusting the
    # wiring. Deliberately NOT `cargo check` on a repo crate: cargo skips rustc
    # entirely when the target directory is already fresh, so that test passed
    # or failed depending on what happened to be built, which is no test at all.
    #
    # The probe path is fixed rather than mktemp'd, because the cache key
    # includes it: a fresh directory each run would miss every time and could
    # never demonstrate a hit.
    probe_dir="${KACHE_CACHE_DIR:-$HOME/.cache/kache}/.doctor-probe"
    mkdir -p "$probe_dir"
    printf 'pub fn probe() -> u32 { 0 }\n' > "$probe_dir/probe.rs"
    before=$(cache_ops)
    kache rustc --crate-type=lib --crate-name=kache_doctor_probe \
      --emit=metadata "$probe_dir/probe.rs" --out-dir "$probe_dir" >/dev/null 2>&1 \
      || die 'compiling through kache failed; run `kache doctor` for detail'
    after=$(cache_ops)
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
    if [[ -d $SHARED_REMOTE ]]; then
      printf '%8s  kache shared remote (both build identities)\n' \
        "$(du -sh "$SHARED_REMOTE" 2>/dev/null | cut -f1)"
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
    note 'done; the next build repopulates from kache'
    ;;

  *)
    die "unknown command '${1}'; expected doctor, stats, disk, or prune"
    ;;
esac
