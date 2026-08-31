#!/usr/bin/env bash
# Claude Code hook adapter for scripts/agent-lane.sh (AGENTS.md §Working
# alongside other agents).
#
# Hooks are the part that makes coordination happen without anyone remembering
# to do it. An agent that has to be told to announce itself will forget on the
# session where it matters.
#
# Everything here is best-effort and must never take a session down with it: a
# coordination ledger that can block work is worse than no ledger. Every path
# out of this script exits 0 unless it is deliberately returning a decision.
set -uo pipefail

readonly EVENT=${1:-}
readonly HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly LANE="$HERE/agent-lane.sh"

[[ -x $LANE ]] || exit 0
command -v jq >/dev/null || exit 0

payload=$(cat 2>/dev/null || true)
field() { printf '%s' "$payload" | jq -r "$1 // empty" 2>/dev/null || true; }

case $EVENT in
  session-start)
    # Register before briefing, so this session appears in its own brief and an
    # agent can see the lane it is about to be held to.
    "$LANE" register --session "$(field '.session_id')" >/dev/null 2>&1 || exit 0
    brief=$("$LANE" brief 2>/dev/null) || exit 0
    jq -n --arg c "$brief" '{
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: $c
      }
    }'
    ;;

  heartbeat)
    "$LANE" heartbeat >/dev/null 2>&1 || true
    exit 0
    ;;

  pre-edit)
    # Two callers, two tool vocabularies. Claude Code names the file directly
    # (`Edit`/`Write` -> tool_input.file_path). Codex edits through
    # `apply_patch`, whose payload is the patch itself and whose paths live in
    # its `*** Add|Update|Delete File:` and `*** Move to:` headers — so a hook
    # that only reads file_path sees nothing and silently waves every Codex
    # edit through, which is worse than not running at all.
    mapfile -t paths < <(
      printf '%s' "$payload" | jq -r '
        [ .tool_input.file_path?, .tool_input.path?, .tool_input.file? ]
        + ( [ .tool_input.command? ] | flatten
            | map(select(type == "string")) )
        + [ .tool_input.input?, .tool_input.patch? ]
        | map(select(. != null and . != ""))
        | .[]
      ' 2>/dev/null |
      awk '
        /^\*\*\* (Add|Update|Delete) File: / { sub(/^\*\*\* [A-Za-z]+ File: /, ""); print; next }
        /^\*\*\* Move to: /                  { sub(/^\*\*\* Move to: /, "");        print; next }
        !/[[:space:]]/ && !/^\*\*\*/          { print }
      ' | sort -u
    )
    (( ${#paths[@]} )) || exit 0
    overlap=""
    for path in "${paths[@]}"; do
      [[ -n $path ]] || continue
      hit=$("$LANE" check "$path" 2>/dev/null) && continue
      [[ -n $hit ]] || continue
      overlap+="$hit"$'\n'
    done
    [[ -n ${overlap//[[:space:]]/} ]] || exit 0
    path=$(printf '%s' "${paths[*]}")
    # "ask", never "deny". Two worktrees editing one file is legal — they are
    # separate checkouts and neither can clobber the other. What it predicts is
    # a merge conflict, and whether that is worth avoiding is a judgement about
    # the task, which belongs to the agent and the user rather than to a hook.
    jq -n --arg o "$overlap" --arg p "$path" '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "ask",
        permissionDecisionReason: (
          "Another agent has declared it is working on this path:\n" + $o +
          "\n\nEditing it here is safe on disk — separate worktrees — but the two " +
          "versions will meet at merge. Consider messaging that session " +
          "(ListAgents / SendMessage) or narrowing your change."
        )
      }
    }'
    ;;

  session-end)
    "$LANE" release >/dev/null 2>&1 || true

    # Post-merge reclaim (#781). A lane whose PR has landed no longer needs its
    # target directory, and a worktree directory that has outlived its
    # registration will never be built in again — 202 GiB across fifteen target
    # trees, and two orphaned 100 GiB lanes in one day, because nothing ever
    # asked. Session end is the moment a lane stops needing its outputs, so it
    # is where the question gets asked.
    #
    # Not a systemd timer: a timer fires on wall-clock time, which is unrelated
    # to whether anything landed, and it would live outside this repository as
    # a user unit nobody clones. This hook already exists, already fires exactly
    # when a lane finishes, and the reclaimer is idempotent and cheap — it does
    # a fetch and a `du` and then, almost always, nothing.
    #
    # Two details that are load-bearing:
    #   * the *primary* checkout's copy is invoked, not this worktree's. The
    #     reclaimer never deletes the tree it is being read from, so a worktree
    #     running its own copy could never reclaim itself — which is exactly the
    #     tree that just finished.
    #   * detached, from `/`, best-effort. This hook has a 10 s budget and must
    #     never take a session down with it; a reclaim that has to fetch can
    #     outlast that, and its own flock keeps two of them from racing.
    common=$(git -C "$HERE" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || exit 0
    primary=$(dirname "$common")
    reclaimer="$primary/scripts/dev-cache.sh"
    if [[ -x $reclaimer ]]; then
      ( cd / && setsid nohup "$reclaimer" reclaim --apply >/dev/null 2>&1 </dev/null & ) || true
    fi
    exit 0
    ;;

  *) exit 0 ;;
esac
exit 0
