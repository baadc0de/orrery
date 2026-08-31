#!/usr/bin/env bash
# Build-cache and target-directory hygiene for the Orrery worktrees.
#
#   ./scripts/dev-cache.sh doctor   check the cache is wired up and working
#   ./scripts/dev-cache.sh stats    hit rate and cache size
#   ./scripts/dev-cache.sh disk     what every target/ in the repo is costing
#   ./scripts/dev-cache.sh prune    delete every target/ (safe: sources are in git)
#   ./scripts/dev-cache.sh reclaim  report what is reclaimable in the worktrees
#   ./scripts/dev-cache.sh reclaim --apply   and delete it
#   ./scripts/dev-cache.sh --self-test       the reclaimer's two gates hold
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
#
# **The -maxdepth 3 is load-bearing, and only by accident.** `prune` deletes
# everything this returns unconditionally, with no liveness check at all. That
# is safe today for one reason and one reason only: the agent worktrees live at
# `.claude/worktrees/<lane>/`, so their `target/` sits at depth 4 and this find
# never sees it. Widen the depth, or add a `-path` that reaches into the
# worktrees, and `prune` starts deleting build outputs out from under whichever
# of the four or five concurrent lanes happens to be compiling.
#
# That is scope, not a safety property. Anything that wants to reach the
# worktree targets has to earn its safety explicitly — see `reclaim` below,
# which does, and `worktree_target_dirs`, which is deliberately a separate
# function so that widening one cannot widen the other.
target_dirs() {
  find "$ROOT" -maxdepth 3 -type d -name target -prune 2>/dev/null | sort
}

# The worktree targets `target_dirs` deliberately cannot see. Read-only callers
# (`disk`) and the guarded caller (`reclaim`) only; never `prune`.
worktree_target_dirs() {
  local primary
  primary=$(primary_checkout) || return 0
  find "$primary/.claude/worktrees" -mindepth 2 -maxdepth 2 \
    -type d -name target -prune 2>/dev/null | sort
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

# ── Reclaiming dead lanes ───────────────────────────────────────────────────
#
# The measured problem (#781, from the mbx evaluation in docs/mbx-evaluation.md)
# is not insufficient deduplication: 202 GiB across fifteen target trees, only
# 5.2 GiB of which overlaps the kache store. It is that nothing ever reclaims
# them. Two orphaned 100 GiB-class worktrees appeared in a single day.
#
# `reclaim` widens the scope `prune` is only accidentally safe within, so it
# cannot inherit that accident. Every tree it touches passes two independent
# gates, and each gate is conservative in the same direction — anything
# unreadable, unresolvable or unfetchable is *reported and skipped*, never
# deleted:
#
#   liveness  no build process is rooted in the tree. Resolved through
#             /proc/<pid>/cwd, never guessed from a command line.
#   content   the branch's changes are already on a freshly fetched main,
#             compared by content rather than by ancestry.
#
# What it does with a tree that passes both depends on what the tree is:
#
#   orphan     a directory under .claude/worktrees/ that `git worktree list`
#              does not know about. Dead by definition — no lane can ever build
#              there again, and `git worktree remove` refuses it, which is why
#              both of #781's incidents survived. The whole directory goes.
#   registered a live worktree whose PR has landed. Only its `target/` goes;
#              the checkout is the agent's and is not this script's to remove.

readonly RECLAIM_REF=refs/dev-cache/main
# Matches scripts/agent-lane.sh's own staleness window, and reads the same
# override, so the two cannot disagree about whether a lane is still there.
readonly LANE_STALE_SECS=${AGENT_LANE_STALE_SECS:-2700}

# The primary checkout, whatever we were invoked from.
#
# **Every git question about whether work has landed must be asked here.** An
# orphaned worktree's own `origin` can point at a local filesystem path rather
# than at GitHub, so its `origin/main` is stale by however many commits have
# landed since it was cut, and `git log origin/main..HEAD` run *inside* the
# orphan reports merged work as unmerged. Worktrees share one object database,
# so the primary can resolve the orphan's HEAD anyway; nothing is lost by
# asking from here, and correctness is gained.
primary_checkout() {
  local common
  common=$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) \
    || return 1
  dirname "$common"
}

# A main ref that is actually current.
#
# The fallback is loud but not fatal: a stale ref can only make a landed branch
# look unmerged, which costs disk rather than data. The reverse — treating an
# unlanded branch as landed — is the failure this whole function exists to
# avoid, and no local ref can produce it.
fresh_main_ref() {
  local repo=$1 ref
  if git -C "$repo" fetch --quiet --no-tags --force origin "main:$RECLAIM_REF" 2>/dev/null; then
    printf '%s\n' "$RECLAIM_REF"
    return 0
  fi
  note 'could not fetch origin main; falling back to a local ref (stale reads as "not merged")'
  for ref in refs/remotes/origin/main refs/heads/main; do
    if git -C "$repo" rev-parse --verify --quiet "$ref" >/dev/null 2>&1; then
      printf '%s\n' "$ref"
      return 0
    fi
  done
  return 1
}

# Process names that mean a build is happening. Padded with spaces at both ends
# so the membership test below cannot match a substring.
readonly BUILD_COMMS=' cargo rustc rustdoc cargo-clippy cargo-fmt cargo-nextest kache sccache cc cc1 cc1plus collect2 ld lld rust-analyzer '

# Why this tree must not be touched, printed; empty and non-zero if nothing
# objects.
#
# **Liveness is the signal — not file counts and not mtimes**, which look
# identical for a lane that finished and a lane that stalled at 3am.
tree_busy_reason() {
  local tree=${1%/} pidpath pid comm cwd fd

  for pidpath in /proc/[0-9]*; do
    { read -r comm < "$pidpath/comm"; } 2>/dev/null || continue
    [[ $BUILD_COMMS == *" $comm "* ]] || continue

    # /proc/<pid>/cwd, resolved by the kernel — *not* a grep of the command
    # line. `pgrep -f "$tree"` matches a string in argv, which says nothing
    # about where the process is running: a build in tree A whose argv happens
    # to name tree B gets attributed to B and missed in A. That mistake was
    # made by hand on 2026-08-31; the self-test below pins that this function
    # cannot make it.
    cwd=$(readlink "$pidpath/cwd" 2>/dev/null) || continue
    if [[ $cwd == "$tree" || $cwd == "$tree"/* ]]; then
      pid=${pidpath#/proc/}
      printf 'a live %s (pid %s) is working in %s\n' "$comm" "$pid" "$cwd"
      return 0
    fi
  done

  # A second, independent signal: any process at all — whatever it is called —
  # holding an open descriptor under this tree's target/. cargo keeps
  # `target/*/.cargo-lock` open for the whole of a build, so this catches a
  # build whose own cwd is somewhere else entirely, and catches linkers and
  # helper processes this script has never heard of.
  fd=$(find /proc/[0-9]*/fd -maxdepth 1 -type l -lname "$tree/target/*" -print -quit 2>/dev/null) \
    || true
  if [[ -n $fd ]]; then
    pid=${fd#/proc/}
    pid=${pid%%/*}
    printf 'pid %s holds an open file under %s\n' "$pid" "$tree/target"
    return 0
  fi

  return 1
}

# Is this commit's content already on main?
#
# Deliberately **not** `git merge-base --is-ancestor`. PRs here are
# squash-merged, so a lane's own commits are never ancestors of main and an
# ancestry test would report every single landed lane as unmerged — a reclaimer
# that never reclaims. Compare the files the lane actually changed instead: if
# every one of them is byte-identical on main, the lane's work is on main
# however it got there.
#
# The asymmetry is deliberate. A file the lane touched that main has since
# changed *again* reads as not-merged and the tree is kept. That is a false
# negative, and false negatives cost disk.
content_is_on_main() {
  local repo=$1 main_ref=$2 sha=$3 base names
  local -a paths=()

  git -C "$repo" cat-file -e "$sha^{commit}" 2>/dev/null || return 1
  base=$(git -C "$repo" merge-base "$main_ref" "$sha" 2>/dev/null) || return 1

  names=$(git -C "$repo" diff --name-only "$base" "$sha" 2>/dev/null) || return 1
  # Nothing of its own to lose.
  [[ -n $names ]] || return 0
  # git quotes a path containing a newline or a control character, and a quoted
  # string used as a pathspec matches nothing — which would make `git diff
  # --quiet` succeed and report the branch merged. Refuse rather than guess.
  if grep -q '^"' <<<"$names"; then
    note 'a changed path needed quoting; refusing to judge this branch by pathspec'
    return 1
  fi
  mapfile -t paths <<<"$names"

  git -C "$repo" diff --quiet "$main_ref" "$sha" -- "${paths[@]}" 2>/dev/null
}

# The worktrees git still knows about.
registered_worktrees() {
  git -C "$1" worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p'
}

# A lane registered in the agent ledger whose heartbeat is still warm. Not a
# safety gate — the liveness check above is — but deleting a live agent's
# target between two of its builds costs it a full cold rebuild, and the ledger
# already knows.
lane_is_live_in() {
  local common=$1 tree=$2 file wt beat
  command -v jq >/dev/null || return 1
  local now
  now=$(date +%s)
  for file in "$common"/agents/lanes/*.json; do
    [[ -f $file ]] || continue
    wt=$(jq -r '.worktree // empty' "$file" 2>/dev/null) || continue
    [[ $wt == "$tree" ]] || continue
    beat=$(jq -r '.heartbeat // 0' "$file" 2>/dev/null) || continue
    if (( now - beat < LANE_STALE_SECS )); then
      return 0
    fi
  done
  return 1
}

# ── The reclaim command ─────────────────────────────────────────────────────

cmd_reclaim() {
  local apply=0 arg
  # Any bare argument restricts the run to the worktrees named. The whole-set
  # run is the one the trigger makes; naming trees is for testing this script
  # against a scratch worktree without four live lanes in scope.
  local -a only=()
  for arg in "$@"; do
    case $arg in
      --apply) apply=1 ;;
      --dry-run) apply=0 ;;
      -*) die "reclaim: unknown option '$arg'; expected --apply or --dry-run" ;;
      *) only+=("$arg") ;;
    esac
  done

  local primary common
  primary=$(primary_checkout) || die 'reclaim: not inside a git repository'
  common="$primary/.git"
  [[ -d $common ]] || common=$(git -C "$primary" rev-parse --path-format=absolute --git-common-dir)

  local worktrees="$primary/.claude/worktrees"
  if [[ ! -d $worktrees ]]; then
    note "reclaim: $worktrees does not exist; nothing to do"
    return 0
  fi

  # Two sessions ending at the same moment would otherwise race two `rm -rf`s
  # over one directory. Losing the race is not an error; there is nothing left
  # to do by then.
  local lock="$common/.dev-cache-reclaim.lock"
  exec 9>"$lock"
  if ! flock -n 9; then
    note 'reclaim: another reclaim holds the lock; nothing to do'
    return 0
  fi

  local main_ref
  main_ref=$(fresh_main_ref "$primary") \
    || die 'reclaim: no usable main ref; refusing to judge anything merged'
  note "reclaim: judging against $main_ref ($(git -C "$primary" rev-parse --short "$main_ref"))"
  (( apply )) || note 'reclaim: dry run; pass --apply to delete'

  # The tree this very script is being read from. bash reads a script
  # incrementally, so deleting it mid-run is its own kind of hazard, and an
  # agent's own checkout is never the one that needs reclaiming.
  local self_tree
  self_tree=$(cd "$ROOT" && pwd)

  local registered reclaimed_kb=0
  registered=$(registered_worktrees "$primary")

  local dir name head reason size kind
  while IFS= read -r dir; do
    [[ -n $dir ]] || continue
    name=${dir#"$worktrees"/}

    if (( ${#only[@]} )); then
      local wanted=0 pick
      for pick in "${only[@]}"; do
        if [[ $pick == "$name" ]]; then wanted=1; fi
      done
      (( wanted )) || continue
    fi

    if [[ $dir == "$self_tree" || $self_tree == "$dir"/* ]]; then
      printf '  keep     %-16s the reclaimer is running from this tree\n' "$name"
      continue
    fi

    if grep -Fxq "$dir" <<<"$registered"; then
      kind=registered
    else
      kind=orphan
    fi

    if reason=$(tree_busy_reason "$dir"); then
      printf '  BUSY     %-16s %s\n' "$name" "$reason"
      continue
    fi

    if [[ $kind == registered ]] && lane_is_live_in "$common" "$dir"; then
      printf '  keep     %-16s a live agent lane is registered here\n' "$name"
      continue
    fi

    # An orphan with unreadable git metadata is not proof of anything; refuse
    # to delete blind. (#781's orphans both had *working* metadata — the
    # directory outlived its registration, not its .git file.)
    if ! head=$(git -C "$dir" rev-parse HEAD 2>/dev/null); then
      printf '  keep     %-16s cannot read its git HEAD; refusing to delete blind\n' "$name"
      continue
    fi

    local dirty
    if ! dirty=$(git -C "$dir" status --porcelain 2>/dev/null); then
      printf '  keep     %-16s cannot read its git status; refusing to delete blind\n' "$name"
      continue
    fi
    if [[ -n $dirty ]]; then
      printf '  keep     %-16s %s uncommitted change(s) live only here\n' \
        "$name" "$(wc -l <<<"$dirty")"
      continue
    fi

    if ! content_is_on_main "$primary" "$main_ref" "$head"; then
      printf '  keep     %-16s its content is not on %s\n' "$name" "${main_ref##*/}"
      continue
    fi

    if [[ $kind == orphan ]]; then
      size=$(du -sk "$dir" 2>/dev/null | cut -f1) || size=0
      reclaimed_kb=$((reclaimed_kb + ${size:-0}))
      printf '  %-8s %-16s orphaned (not in `git worktree list`), content on %s, idle — %s\n' \
        "$( (( apply )) && echo DELETE || echo would )" "$name" "${main_ref##*/}" \
        "$(numfmt --to=iec $(( ${size:-0} * 1024 )))"
      if (( apply )); then
        # Re-check immediately before deleting. `du` over a 100 GiB tree takes
        # seconds, and a build can start inside them; the gate above is only
        # worth what it is worth at the instant of the `rm`.
        if reason=$(tree_busy_reason "$dir"); then
          printf '  BUSY     %-16s %s\n' "$name" "$reason"
          reclaimed_kb=$((reclaimed_kb - ${size:-0}))
          continue
        fi
        rm -rf -- "$dir"
        printf '%s reclaim orphan %s %sK\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$dir" "${size:-0}" \
          >> "$common/dev-cache-reclaim.log"
      fi
    else
      [[ -d "$dir/target" ]] || continue
      size=$(du -sk "$dir/target" 2>/dev/null | cut -f1) || size=0
      reclaimed_kb=$((reclaimed_kb + ${size:-0}))
      printf '  %-8s %-16s landed on %s and idle — target/ only, %s\n' \
        "$( (( apply )) && echo DELETE || echo would )" "$name" "${main_ref##*/}" \
        "$(numfmt --to=iec $(( ${size:-0} * 1024 )))"
      if (( apply )); then
        # See above: the liveness gate is only worth what it is worth at the
        # instant of the `rm`, and `du` is not instant.
        if reason=$(tree_busy_reason "$dir"); then
          printf '  BUSY     %-16s %s\n' "$name" "$reason"
          reclaimed_kb=$((reclaimed_kb - ${size:-0}))
          continue
        fi
        rm -rf -- "$dir/target"
        printf '%s reclaim target %s %sK\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$dir/target" "${size:-0}" \
          >> "$common/dev-cache-reclaim.log"
      fi
    fi
  done < <(find "$worktrees" -mindepth 1 -maxdepth 1 -type d | sort)

  printf '%s: %s %s\n' "$NAME" \
    "$( (( apply )) && echo reclaimed || echo reclaimable )" \
    "$(numfmt --to=iec $((reclaimed_kb * 1024)))"
}

# ── --self-test ─────────────────────────────────────────────────────────────
#
# This script's `reclaim` runs `rm -rf` over 100 GiB-class directories without a
# human in the loop, so the two gates that stop it are the things worth pinning.
# Both clauses are functional rather than structural: they build the situation
# and run the real function over it. A `grep` for the word `cwd` would pass on a
# reclaimer that had stopped checking anything.

ST_TMP=
st_cleanup() {
  local pid
  for pid in ${ST_PIDS:-}; do kill "$pid" 2>/dev/null || true; done
  [[ -n ${ST_TMP:-} && -d ${ST_TMP:-} ]] && rm -rf -- "$ST_TMP"
  return 0
}

st_ok() { echo "  ok: $*" >&2; }

# Wait for a just-started process to be visible under the name we expect, so
# the assertions below cannot race the fork.
st_await_comm() {
  local pid=$1 want=$2 i comm
  for i in $(seq 1 200); do
    { read -r comm < "/proc/$pid/comm"; } 2>/dev/null || comm=
    [[ $comm == "$want" ]] && return 0
    sleep 0.01
  done
  return 1
}

self_test() {
  command -v git >/dev/null || die 'self-test: git is required'
  ST_TMP=$(mktemp -d) || die 'self-test: could not make a scratch directory'
  ST_PIDS=
  trap st_cleanup EXIT

  # ── Clause 1: content, not ancestry ───────────────────────────────────────
  #
  # Reproduce this repository's actual merge shape — a squash merge, where the
  # lane's own commits never become ancestors of main — and require the merge
  # test to see the work as landed anyway. The same fixture proves the obvious
  # alternative is wrong: `--is-ancestor` says no on a branch that is fully on
  # main, which is a reclaimer that reclaims nothing.
  local repo="$ST_TMP/repo"
  git init -q -b main "$repo"
  git -C "$repo" config user.email dev-cache@example.invalid
  git -C "$repo" config user.name 'dev-cache self-test'
  git -C "$repo" config commit.gpgsign false
  printf 'one\n' > "$repo/a.txt"
  git -C "$repo" add -A
  git -C "$repo" commit -qm base

  git -C "$repo" checkout -q -b lane
  printf 'two\n' > "$repo/a.txt"
  printf 'new\n' > "$repo/b.txt"
  git -C "$repo" add -A
  git -C "$repo" commit -qm 'lane: first'
  printf 'more\n' >> "$repo/b.txt"
  git -C "$repo" add -A
  git -C "$repo" commit -qm 'lane: second'
  local landed_tip
  landed_tip=$(git -C "$repo" rev-parse HEAD)

  git -C "$repo" checkout -q main
  git -C "$repo" checkout -q lane -- .
  git -C "$repo" commit -qm 'lane (#1)'

  if git -C "$repo" merge-base --is-ancestor "$landed_tip" refs/heads/main 2>/dev/null; then
    die 'self-test: the fixture is not a squash merge; the whole clause is vacuous'
  fi
  st_ok 'the fixture is a real squash merge (the lane tip is not an ancestor of main)'

  if ! content_is_on_main "$repo" refs/heads/main "$landed_tip"; then
    die 'self-test: a squash-merged lane was not recognised as landed'
  fi
  st_ok 'content_is_on_main sees a squash-merged lane as landed'

  git -C "$repo" checkout -q lane
  printf 'unlanded\n' > "$repo/c.txt"
  git -C "$repo" add -A
  git -C "$repo" commit -qm 'lane: not landed'
  local unlanded_tip
  unlanded_tip=$(git -C "$repo" rev-parse HEAD)
  git -C "$repo" checkout -q main

  if content_is_on_main "$repo" refs/heads/main "$unlanded_tip"; then
    die 'self-test: a lane carrying work that is not on main was called landed'
  fi
  st_ok 'content_is_on_main refuses a lane carrying work main does not have'

  # ── Clause 2: liveness is cwd, not argv ───────────────────────────────────
  #
  # The mistake this pins was made by hand on 2026-08-31: grepping a process's
  # command line for a path and concluding the process was running there. The
  # negative case below is a real build-named process whose argv contains the
  # tree and whose working directory is somewhere else. `pgrep -f` finds it;
  # the reclaimer must not.
  local tree="$ST_TMP/tree" elsewhere="$ST_TMP/elsewhere"
  mkdir -p "$tree/target/debug" "$elsewhere"
  local fake="$ST_TMP/cargo"
  cp "${BASH:-/bin/bash}" "$fake"

  ( cd "$elsewhere" && exec "$fake" -c 'sleep 30; :' "$tree/src/lib.rs" ) &
  local decoy=$!
  ST_PIDS="$ST_PIDS $decoy"
  st_await_comm "$decoy" cargo || die 'self-test: the decoy process never appeared as `cargo`'

  if ! pgrep -f "$tree" >/dev/null 2>&1; then
    die 'self-test: the decoy does not carry the tree in its argv; the clause is vacuous'
  fi
  st_ok 'the decoy is a live `cargo` whose argv names the tree (pgrep -f finds it)'

  local reason
  if reason=$(tree_busy_reason "$tree"); then
    die "self-test: a process whose argv merely mentions the tree was called busy: $reason"
  fi
  st_ok 'tree_busy_reason is not fooled by argv: an idle tree reads as idle'

  ( cd "$tree" && exec "$fake" -c 'sleep 30; :' ) &
  local builder=$!
  ST_PIDS="$ST_PIDS $builder"
  st_await_comm "$builder" cargo || die 'self-test: the builder process never appeared as `cargo`'

  if ! reason=$(tree_busy_reason "$tree"); then
    die 'self-test: a live cargo with its cwd in the tree was not detected'
  fi
  case $reason in
    *"$builder"*) ;;
    *) die "self-test: the busy reason names the wrong process: $reason" ;;
  esac
  st_ok "tree_busy_reason detects a cargo whose cwd is the tree, and names it (pid $builder)"

  kill "$builder" 2>/dev/null || true
  wait "$builder" 2>/dev/null || true

  # And the second signal, which exists for the builds this script has never
  # heard of: a process with no build-ish name at all, cwd outside the tree,
  # holding cargo's lock file open.
  : > "$tree/target/debug/.cargo-lock"
  ( cd / && exec 9< "$tree/target/debug/.cargo-lock" && exec sleep 30 ) &
  local holder=$!
  ST_PIDS="$ST_PIDS $holder"
  st_await_comm "$holder" sleep || die 'self-test: the fd holder never started'

  if ! reason=$(tree_busy_reason "$tree"); then
    die 'self-test: an open descriptor under target/ did not register as busy'
  fi
  case $reason in
    *"open file"*) ;;
    *) die "self-test: the open-descriptor signal did not fire: $reason" ;;
  esac
  st_ok 'tree_busy_reason detects an open descriptor under target/ from any process'

  kill "$holder" 2>/dev/null || true
  wait "$holder" 2>/dev/null || true

  if reason=$(tree_busy_reason "$tree"); then
    die "self-test: the tree still reads as busy after both processes exited: $reason"
  fi
  st_ok 'and the same tree reads as idle once nothing is running in it'

  # ── Clause 3: an unregistered directory is an orphan ──────────────────────
  local wt="$ST_TMP/wt-live"
  git -C "$repo" worktree add -q -b probe "$wt" main
  mkdir -p "$ST_TMP/wt-orphan"
  local listed
  listed=$(registered_worktrees "$repo")
  grep -Fxq "$wt" <<<"$listed" \
    || die 'self-test: a registered worktree was not listed'
  if grep -Fxq "$ST_TMP/wt-orphan" <<<"$listed"; then
    die 'self-test: a directory git does not know about was listed as registered'
  fi
  st_ok 'registered_worktrees distinguishes a live worktree from a bare directory'

  echo "$NAME: self-test passed"
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
    # The worktree targets are the ones `target_dirs` deliberately cannot see,
    # and on 2026-08-31 they were 150 of the 202 GiB. A `disk` that omits them
    # under-reports the problem by three quarters, which is how two 100 GiB
    # orphans went unnoticed for a day. Reported separately because `prune`
    # does not touch them and `reclaim` is what does.
    wt_total=0
    while read -r dir; do
      [[ -n $dir ]] || continue
      size=$(du -sk "$dir" 2>/dev/null | cut -f1)
      wt_total=$((wt_total + ${size:-0}))
      printf '%8s  %s\n' "$(du -sh "$dir" 2>/dev/null | cut -f1)" "${dir#"$ROOT"/}"
    done < <(worktree_target_dirs)
    printf '%8s  TOTAL across the agent worktrees (see `reclaim`)\n' \
      "$(numfmt --to=iec $((wt_total * 1024)))"
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

  reclaim)
    shift
    cmd_reclaim "$@"
    ;;

  --self-test)
    self_test
    ;;

  *)
    die "unknown command '${1}'; expected doctor, stats, disk, prune, reclaim or --self-test"
    ;;
esac
