#!/usr/bin/env bash
# Decide whether a changed-path list contains anything the expensive CI lanes
# can act on.
#
#   ci-changed-code.sh <base-sha>     -> prints "true" or "false"
#   ci-changed-code.sh --self-test
#
# Why this is a script and not four lines inline in ci.yml: a filter that
# wrongly answers "false" silently skips clippy, the static gates and the whole
# test suite, and nothing downstream notices — the PR goes green having checked
# nothing. Inline YAML cannot be tested; `yaml.safe_load` happily parses a
# filter whose condition has been replaced by `if false`. Here the logic has a
# self-test, and the gates lane runs it on every commit.
#
# THE RULE: every changed path must end in `.md`, or the answer is "true".
#
# Deliberately not "under docs/". `docs/assets/` holds binary art that
# scripts/asset-provenance.sh genuinely inspects, so a PNG landing there is a
# code-affecting change. Markdown is the only extension in this tree that
# cannot reach a compiled artifact: verified that no `include_str!`,
# `include_bytes!` or build.rs reads a `.md` file or anything under `docs/`.
# If that ever stops being true, this filter starts lying and must be narrowed
# with it.
#
# FAILS OPEN, always. No base sha, a git failure, an empty diff, an unreadable
# argument — every one of them answers "true" and the full lanes run. A filter
# that guesses wrong must run CI, not skip it.
set -uo pipefail

# Classify a newline-separated path list. The only place the rule lives.
classify() { # <<< paths on stdin
  local files; files=$(cat)
  [[ -n $files ]] || { echo true; return; }          # empty list: fail open
  if printf '%s\n' "$files" | grep -qv '\.md$'; then
    echo true
  else
    echo false
  fi
}

self_test() {
  local got want desc rc=0
  check() { # want, desc, paths...
    want=$1; desc=$2; shift 2
    got=$(printf '%s\n' "$@" | classify)
    if [[ $got != "$want" ]]; then
      echo "ci-changed-code: self-test: $desc — wanted $want, got $got" >&2
      rc=1
    fi
  }

  check false 'a single doc'                     'docs/plans/a1-map.md'
  check false 'several docs'                     'docs/06-verifiable-core.md' 'README.md'
  check false 'markdown outside docs/'           'crates/orrery_core/NOTES.md'

  # The cases that must NOT be filtered out. Each of these has bitten someone.
  check true  'a rust source beside a doc'       'docs/x.md' 'crates/orrery_core/src/lib.rs'
  check true  'binary art under docs/'           'docs/assets/cover.png'
  check true  'a gate script'                    'scripts/core-gates.sh'
  check true  'the workflow itself'              '.github/workflows/ci.yml'
  check true  'a manifest'                       'crates/orrery_core/Cargo.toml'
  check true  'a lockfile'                       'Cargo.lock'
  check true  'a dotfile with no extension'      '.gitattributes'
  check true  'a path that merely contains .md'  'crates/x/src/md.rs'

  # Fail-open on nothing at all.
  got=$(printf '' | classify)
  [[ $got == true ]] || { echo 'ci-changed-code: self-test: an empty path list must fail open' >&2; rc=1; }

  if [[ $rc -eq 0 ]]; then echo 'ci-changed-code: self-test passed'; fi
  return $rc
}

case ${1:-} in
  --self-test) self_test ;;
  '')          echo true ;;                        # no base sha: fail open
  *)
    if ! files=$(git diff --name-only "$1" HEAD 2>/dev/null); then
      echo true                                    # git could not answer: fail open
    else
      printf '%s\n' "$files" | classify
    fi
    ;;
esac
