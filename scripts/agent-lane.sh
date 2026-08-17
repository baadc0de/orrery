#!/usr/bin/env bash
# Coordination between coding agents sharing this machine (AGENTS.md §Working
# alongside other agents).
#
# Agents here work in parallel git worktrees. Worktrees isolate the *filesystem*,
# so two agents editing the same file do not clobber each other — the collision
# surfaces later, at merge, which is the expensive place to find it. What they do
# not isolate is everything else: one `.git` directory, one build cache, one disk,
# one FoundationDB dev cluster, one set of harness ports, one GitHub remote.
#
# This script is the shared ledger for both halves. Lanes announce what an agent
# is working on and which paths it expects to touch — advisory, because a
# same-file edit across worktrees is legal and often correct. Leases are
# exclusive, because a port or a database cluster genuinely is.
#
# State lives in the git *common* directory, which every worktree of this clone
# resolves to the same absolute path and which is never committed. That is the
# only location with those two properties: a tracked path would be copied per
# worktree and committed by accident, and a git-ignored path is per-worktree too
# — `.agents/memory/` exists only in whichever checkout created it, which is the
# bug this arrangement avoids.
set -euo pipefail

readonly NAME=agent-lane

die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

command -v git >/dev/null || die 'git is required'
command -v jq >/dev/null || die 'jq is required (pacman -S jq)'

# ── Where state lives ────────────────────────────────────────────────────────

GIT_COMMON="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" \
  || die 'not inside a git repository'
readonly GIT_COMMON
readonly ROOT="$GIT_COMMON/agents"
readonly LANES="$ROOT/lanes"
readonly LEASES="$ROOT/leases"
readonly LOCK="$ROOT/.lock"
mkdir -p "$LANES" "$LEASES"

# The worktree is the identity. One agent per worktree is the working
# assumption and the thing this whole arrangement is arranged around: two
# agents in one worktree share a filesystem and cannot be told apart here.
WORKTREE="$(git rev-parse --path-format=absolute --show-toplevel)"
readonly WORKTREE
LANE_ID="$(printf '%s' "$WORKTREE" | sha1sum | cut -c1-12)"
readonly LANE_ID
readonly LANE_FILE="$LANES/$LANE_ID.json"

# A lane whose heartbeat has gone quiet for this long is treated as gone. Long
# enough to survive a slow build or a long single tool call, short enough that a
# killed session does not hold a claim for the rest of the day.
readonly STALE_SECS=${AGENT_LANE_STALE_SECS:-2700}

now() { date +%s; }
iso() { date -u +%Y-%m-%dT%H:%M:%SZ; }

with_lock() { flock "$LOCK" "$@"; }

# ── Liveness ─────────────────────────────────────────────────────────────────

# A lane is live if its heartbeat is recent, and — only when a caller supplied a
# pid it vouches for — if that pid still exists. The pid is optional on purpose:
# this script re-execs itself under `flock`, so anything it could observe about
# its own process tree describes the wrapper, which has already exited by the
# time anyone reads the file. Recording that would make every lane read as dead
# the moment it was written, which is exactly what it did.
lane_is_live() {
  local file=$1 beat pid
  [[ -r $file ]] || return 1
  beat=$(jq -r '.heartbeat // 0' "$file" 2>/dev/null) || return 1
  pid=$(jq -r '.pid // empty' "$file" 2>/dev/null) || true
  if [[ -n ${pid:-} ]] && ! kill -0 "$pid" 2>/dev/null; then
    return 1
  fi
  (( $(now) - beat < STALE_SECS ))
}

live_lanes() {
  local file
  for file in "$LANES"/*.json; do
    [[ -e $file ]] || continue
    lane_is_live "$file" && printf '%s\n' "$file"
  done
}

# ── Commands ─────────────────────────────────────────────────────────────────

cmd_register() {
  local task='' paths='' session='' pid=''
  while (( $# )); do
    case $1 in
      --task) task=${2:-}; shift 2 ;;
      --paths) paths=${2:-}; shift 2 ;;
      --session) session=${2:-}; shift 2 ;;
      # Only a caller that knows a pid outliving this command should pass one.
      --pid) pid=${2:-}; shift 2 ;;
      *) die "register: unknown argument $1" ;;
    esac
  done

  local branch
  branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo detached)

  # Preserve a task or path set already recorded when the caller did not supply
  # one: a heartbeat from a hook must not blank out what the agent declared.
  if [[ -r $LANE_FILE ]]; then
    [[ -z $task ]] && task=$(jq -r '.task // ""' "$LANE_FILE")
    [[ -z $paths ]] && paths=$(jq -r '(.paths // []) | join(",")' "$LANE_FILE")
    [[ -z $session ]] && session=$(jq -r '.session // ""' "$LANE_FILE")
    [[ -z $pid ]] && pid=$(jq -r '.pid // ""' "$LANE_FILE")
  fi

  local tmp
  tmp=$(mktemp "$LANES/.tmp.XXXXXX")
  jq -n \
    --arg id "$LANE_ID" \
    --arg worktree "$WORKTREE" \
    --arg branch "$branch" \
    --arg task "$task" \
    --arg session "$session" \
    --arg paths "$paths" \
    --arg started "$(iso)" \
    --arg pid "$pid" \
    --argjson heartbeat "$(now)" \
    '{
      id: $id, worktree: $worktree, branch: $branch, task: $task,
      session: $session, started: $started, heartbeat: $heartbeat,
      paths: ($paths | split(",") | map(select(length > 0)))
    }
    + (if $pid == "" then {} else {pid: ($pid | tonumber)} end)' > "$tmp"
  mv -f "$tmp" "$LANE_FILE"
}

cmd_heartbeat() {
  [[ -r $LANE_FILE ]] || { cmd_register; return; }
  local tmp
  tmp=$(mktemp "$LANES/.tmp.XXXXXX")
  jq --argjson heartbeat "$(now)" '.heartbeat = $heartbeat' "$LANE_FILE" > "$tmp"
  mv -f "$tmp" "$LANE_FILE"
}

cmd_release() {
  rm -f "$LANE_FILE"
  # Leases are dropped with the lane that took them. A lease outliving its
  # holder is the failure mode that makes the next agent wait on nobody.
  local file
  for file in "$LEASES"/*.json; do
    [[ -e $file ]] || continue
    [[ $(jq -r '.lane // ""' "$file") == "$LANE_ID" ]] && rm -f "$file"
  done
  return 0
}

cmd_reap() {
  local file dropped=0
  for file in "$LANES"/*.json; do
    [[ -e $file ]] || continue
    if ! lane_is_live "$file"; then
      rm -f "$file"
      (( ++dropped ))
    fi
  done
  for file in "$LEASES"/*.json; do
    [[ -e $file ]] || continue
    local lane expires
    lane=$(jq -r '.lane // ""' "$file")
    expires=$(jq -r '.expires // 0' "$file")
    if [[ ! -r $LANES/$lane.json ]] || (( $(now) > expires )); then
      rm -f "$file"
      (( ++dropped ))
    fi
  done
  (( dropped )) && note "reaped $dropped stale entr$( ((dropped==1)) && echo y || echo ies)"
  return 0
}

cmd_list() {
  cmd_reap >/dev/null 2>&1 || true
  local any=0 file
  while read -r file; do
    any=1
    jq -r --arg self "$LANE_ID" '
      (if .id == $self then "* " else "  " end) +
      (.branch // "?") + "  " +
      (if (.task // "") == "" then "(no task declared)" else .task end) +
      "\n     " + .worktree +
      (if (.paths | length) > 0 then "\n     touching: " + (.paths | join(", ")) else "" end)
    ' "$file"
  done < <(live_lanes)
  (( any )) || echo "no live lanes"
  # Counted by globbing rather than `ls | wc -l`: with `pipefail` set, an `ls`
  # over an empty directory fails the whole pipeline and takes the command with
  # it, so listing lanes would exit non-zero exactly when there was nothing to
  # report.
  local -a held=()
  for file in "$LEASES"/*.json; do
    [[ -e $file ]] && held+=("$file")
  done
  if (( ${#held[@]} )); then
    echo "leases held:"
    for file in "${held[@]}"; do
      jq -r '"  " + .resource + " — " + (.branch // "?") + " (" + .worktree + ")"' "$file"
    done
  fi
}

# Is any *other* live lane claiming these paths? Advisory by design: the answer
# is a warning, not a veto, because two worktrees editing one file is legal and
# the real cost lands at merge.
cmd_check() {
  (( $# )) || die 'check: give at least one path'
  cmd_reap >/dev/null 2>&1 || true
  local hit=0 file target rel
  for target in "$@"; do
    # Compare repo-relative, so a claim reads the same from any worktree.
    rel=${target#"$WORKTREE"/}
    while read -r file; do
      [[ $(jq -r '.id' "$file") == "$LANE_ID" ]] && continue
      # `.paths[]? as $claim` rather than a bare `.`: inside `$p | startswith(.)`
      # the dot rebinds to `$p`, so the test reads "does $p start with itself",
      # which is true for every claim ever recorded. Binding first is what makes
      # the comparison compare the two things.
      #
      # A claim matches a path when they are equal, when the claim is a
      # directory the path sits under, or when the path is a directory
      # containing the claim — the last so that claiming a whole crate is
      # noticed by someone about to touch one file in it, and vice versa.
      local matched
      matched=$(jq -r --arg p "$rel" '
        .paths[]? as $claim
        | (if ($claim | endswith("/")) then $claim else $claim + "/" end) as $dir
        | (if ($p | endswith("/")) then $p else $p + "/" end) as $pdir
        | select($claim == $p or ($p | startswith($dir)) or ($claim | startswith($pdir)))
        | $claim
      ' "$file" | head -1)
      if [[ -n $matched ]]; then
        hit=1
        jq -r --arg p "$rel" --arg m "$matched" \
          '"  " + $p + " overlaps \"" + $m + "\" claimed by " + (.branch // "?") + " — " + (if (.task // "") == "" then "no task declared" else .task end)' \
          "$file"
      fi
    done < <(live_lanes)
  done
  return $hit
}

# Exclusive, unlike a lane. For the things that genuinely cannot be shared: the
# FoundationDB dev cluster, a harness's fixed ports, a `git push`, anything that
# writes outside a worktree.
cmd_lease() {
  local action=${1:-} resource=${2:-}
  [[ -n $action && -n $resource ]] || die 'lease: acquire|release <resource>'
  local safe=${resource//[^A-Za-z0-9._-]/_}
  local file="$LEASES/$safe.json"
  local ttl=${AGENT_LEASE_TTL_SECS:-5400}

  case $action in
    acquire)
      cmd_reap >/dev/null 2>&1 || true
      if [[ -r $file ]]; then
        local holder
        holder=$(jq -r '.lane' "$file")
        if [[ $holder != "$LANE_ID" ]] && [[ -r $LANES/$holder.json ]]; then
          jq -r '"held by " + (.branch // "?") + " (" + .worktree + ") since " + .taken' "$file" >&2
          return 1
        fi
      fi
      local branch tmp
      branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo detached)
      tmp=$(mktemp "$LEASES/.tmp.XXXXXX")
      jq -n --arg resource "$resource" --arg lane "$LANE_ID" \
        --arg worktree "$WORKTREE" --arg branch "$branch" --arg taken "$(iso)" \
        --argjson expires "$(( $(now) + ttl ))" \
        '{resource: $resource, lane: $lane, worktree: $worktree, branch: $branch,
          taken: $taken, expires: $expires}' > "$tmp"
      mv -f "$tmp" "$file"
      ;;
    release)
      [[ -r $file ]] || return 0
      [[ $(jq -r '.lane' "$file") == "$LANE_ID" ]] || die "lease: $resource is not yours to release"
      rm -f "$file"
      ;;
    *) die "lease: unknown action $action" ;;
  esac
}

# Give this worktree its view of the shared, machine-local memory.
#
# The link is absolute rather than relative because a linked worktree's `.git`
# is a *file* pointing at the common directory, so `../.git/...` resolves to
# nothing from anywhere except the main checkout — which is how the memory came
# to exist in one worktree and be invisible from the others.
cmd_init() {
  local target="$ROOT/memory"
  local link="$WORKTREE/.agents/memory"
  mkdir -p "$target" "$WORKTREE/.agents"
  if [[ -L $link ]]; then
    if [[ $(readlink "$link") == "$target" ]]; then
      warn_unignored "$link"
      return 0
    fi
    rm -f "$link"
  elif [[ -d $link ]]; then
    # A real directory here predates the shared arrangement. Fold it in rather
    # than clobbering it: these are notes someone wrote and nothing else has a
    # copy.
    cp -an "$link/." "$target/" 2>/dev/null || true
    rm -rf "$link"
    note "folded this worktree's .agents/memory into the shared store"
  fi
  ln -s "$target" "$link"
  note "memory linked: $link -> $target"
  warn_unignored "$link"
}

# The link is only invisible to git if this worktree's branch carries the
# `.agents/` ignore rule. On a branch that predates it, `.agents/memory/` with a
# trailing slash matches a directory and *not* a symlink named `memory`, so the
# link shows up untracked and the next `git add -A` sweeps it into a commit.
# That has already happened once, to a neighbouring worktree, and it is the only
# part of this arrangement that reaches into a tree whose branch may not know
# about it yet — so it warns every time, not only when it creates the link.
warn_unignored() {
  git check-ignore -q "$1" 2>/dev/null && return 0
  note "WARNING: .agents/ is not ignored on this branch, so the link shows as"
  note "         untracked and 'git add -A' will commit it. Rebase onto a main"
  note "         carrying the '.agents/' rule, or ignore it locally."
}

# Everything a session should know about its neighbours, in the form a
# SessionStart hook can hand straight to the model.
cmd_brief() {
  cmd_reap >/dev/null 2>&1 || true
  local count
  count=$(live_lanes | wc -l)
  if (( count <= 1 )); then
    echo "No other coding agents are active in this repository right now."
    return 0
  fi
  echo "Other coding agents are active in this repository on this machine."
  echo "Their lanes (yours marked *):"
  cmd_list
  echo
  echo "Before editing, run: scripts/agent-lane.sh check <path>"
  echo "Before a shared resource (fdb cluster, harness ports, push), take a lease."
}

case ${1:-} in
  register)  shift; with_lock "$0" _locked_register "$@" ;;
  heartbeat) shift; with_lock "$0" _locked_heartbeat "$@" ;;
  release)   shift; with_lock "$0" _locked_release "$@" ;;
  lease)     shift; with_lock "$0" _locked_lease "$@" ;;
  reap)      shift; with_lock "$0" _locked_reap "$@" ;;
  init)      shift; with_lock "$0" _locked_init "$@" ;;
  list|who)  shift; cmd_list "$@" ;;
  check)     shift; cmd_check "$@" ;;
  brief)     shift; cmd_brief "$@" ;;

  # Re-entry points under flock. Split out so the lock is held for the write and
  # nothing else: taking it around a read would serialise every `check`.
  _locked_register)  shift; cmd_register "$@" ;;
  _locked_heartbeat) shift; cmd_heartbeat "$@" ;;
  _locked_release)   shift; cmd_release "$@" ;;
  _locked_lease)     shift; cmd_lease "$@" ;;
  _locked_reap)      shift; cmd_reap "$@" ;;
  _locked_init)      shift; cmd_init "$@" ;;

  ''|-h|--help|help)
    cat <<'USAGE'
agent-lane.sh — coordination between agents in parallel worktrees

  register --task "..." [--paths a/,b/c.rs]   announce this worktree's work
                        [--pid N]              tie the lane to a process's life
  heartbeat                                    keep the lane live
  release                                      drop the lane and its leases
  list | who                                   who else is working, and where
  check <path>...                              does anyone else claim these?
  brief                                        list + what to do about it
  lease acquire <resource>                     take an exclusive resource
  lease release <resource>                     give it back
  reap                                         drop dead lanes and leases
  init                                         link .agents/memory into this worktree

State lives in the git common directory, shared by every worktree of this
clone and never committed.
USAGE
    ;;
  *) die "unknown command ${1:-}; try --help" ;;
esac
