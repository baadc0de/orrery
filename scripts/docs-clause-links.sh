#!/usr/bin/env bash
# Clause letters glued onto reference-style links in the documentation.
#
#   ./scripts/docs-clause-links.sh              scan the tree
#   ./scripts/docs-clause-links.sh --self-test  prove the scan still bites
#
# DOCS_CLAUSE_LINKS_DIR overrides the scanned directory (the self-test uses
# this to point at synthetic fixtures); it defaults to `$ROOT/docs`, which
# covers `docs/adr/` and every numbered expansion written in the same house
# style. Vendored and tool READMEs are a different corpus and are not scanned.
#
# ── Why this exists ──────────────────────────────────────────────────────────
#
# ADR-0046 landed with six occurrences of `[D43](f)`. The author meant the
# reference-style link `[D43]` — resolved by the record's tail definition —
# followed by a clause letter; Markdown reads it as an *inline* link whose
# destination is the relative URL `f`: dead on click, silent in review, and
# invisible to anything that checks link *targets* rather than link shapes.
# Twenty of the same shape had already been repaired out of ADR-0049 before
# merge; these six landed. The repair is `[D43] clause (f)`: the reference
# link keeps resolving and the prose reads the way it was meant to.
#
# Repairing six links is worth little if the seventh lands next week, so this
# is the check that refuses the shape.
#
# The pattern is deliberately narrow — an inline link whose destination is a
# single bare character, `\[[^][]*\]\([a-zA-Z0-9]\)` — because that is the
# one shape that is never legitimate here:
#
#   * real relative links name paths — `[ADR-0042](0042-….md)`,
#     `[INV-4](../04-authority.md)` — so they carry a `/`, `.` or `#`;
#   * clause references are written `[D43] clause (f)` or bare `(f)(3)`;
#     a single character inside a *link's* parentheses has no reading that
#     renders correctly;
#   * reference definitions (`[D15]: 0015-crate-set.md`) have no `](` at all.
#
# Known limits, stated rather than buried: fenced code blocks and inline code
# spans are skipped, because quoting the broken shape to discuss it must not
# fail — but CommonMark's *indented* code blocks are not modelled, so the
# shape quoted inside one would fail; and links whose text spans lines are
# not seen. Neither occurs in the corpus today.
set -euo pipefail

readonly NAME=doc-links
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "$NAME: $*" >&2; exit 1; }
note() { echo "$NAME: $*" >&2; }

SCAN_DIR="${DOCS_CLAUSE_LINKS_DIR:-$ROOT/docs}"

# The scan itself: skip fenced blocks, strip inline code spans, then match.
# One match loop per line — a line can carry more than one dead link.
scan_file() {
  awk '
    /^[[:space:]]*```/ || /^[[:space:]]*~~~/ { f = !f; next }
    f { next }
    {
      line = $0
      gsub(/`[^`]+`/, "", line)
      while (match(line, /\[[^][]*\]\([a-zA-Z0-9]\)/)) {
        print FILENAME ":" FNR ": " substr(line, RSTART, RLENGTH)
        line = substr(line, RSTART + RLENGTH)
      }
    }
  ' "$1"
}

# Every .md under the scanned directory, sorted so the output order is stable.
check_tree() {
  local dir="$1"
  [[ -d $dir ]] || die "no docs directory at $dir"

  local report="" file rel found
  while IFS= read -r file; do
    found="$(scan_file "$file")"
    if [[ -n $found ]]; then
      rel="${file#"$dir"/}"
      report+="${found//"$file"/$rel}
"
    fi
  done < <(find "$dir" -type f -name '*.md' | sort)

  [[ -z ${report//[[:space:]]/} ]] || {
    printf '%s' "$report"
    local n
    n="$(grep -c ': \[' <<<"$report")" || n='?'
    die "$n broken clause link(s): '[id](x)' parses as an inline link to x; write '[id] clause (x)' instead"
  }
}

if [[ ${1:-} == '--self-test' ]]; then
  # Functional, not structural: run the same scanner the live invocation runs,
  # against three fixture forests — one where every planted defect must fire
  # by name, one where nothing may fire, and one holding the fence toggle
  # open/closed. Breaking the pattern breaks the bad half; widening it breaks
  # the good half; breaking fence tracking shows up in both directions of the
  # third. Any of the three failing here fails before a doc author does.
  fx="$(mktemp -d "${TMPDIR:-/tmp}/doc-links-selftest.XXXXXX")"
  trap 'rm -rf "$fx"' EXIT

  mkdir "$fx/bad" "$fx/good" "$fx/fenced"

  cat >"$fx/bad/bad.md" <<'EOF'
exactly as [D43](f) placed its overflow flag
is [D43](f)(3)'s reason, applied unchanged
Rejected [ADR-0042](g) for the same reason
upper-case href [R7](C) and digit [x](9) are the same accident
  indented list body with [D44](k) still fires
two on one line [A6](d) then [P0](e)
EOF

  cat >"$fx/good/good.md" <<'EOF'
legit relative [ADR-0042](adr/0042-canonical-simulation-architecture.md)
parent-relative [INV-4](../04-authority.md) and sibling [x](./y/z.md)
reference definitions are not links:
[D15]: 0015-crate-set.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
shortcut use [D43] resolves through the tail definition
the repaired form [D43] clause (f), sub-clauses [D43] clause (f)(3)
bare trailing clauses (f)(4) carry no bracket at all
anchor [text](#clause-f); external [u](https://example.com/f)
extension [e](b.pdf); empty destination [n]()
quoted to discuss it: `[D43](f)` stays inert in an inline span
EOF

  cat >"$fx/fenced/fenced.md" <<'EOF'
```bash
[D43](f) inside a fence is code, not a link
```
after it [D45](m) renders again and must fire
EOF

  # 1. The bad forest: exit 1, every line named, eight hits in total (lines 4
  # and 6 each carry two).
  bad_status=0
  bad_out="$(DOCS_CLAUSE_LINKS_DIR="$fx/bad" "$0" 2>&1)" || bad_status=$?
  (( bad_status == 1 )) || die "self-test: exited $bad_status on the bad forest, expected 1 (output: $bad_out)"
  for want in 'bad.md:1' 'bad.md:2' 'bad.md:3' 'bad.md:4' 'bad.md:5' 'bad.md:6'; do
    grep -qF "$want" <<<"$bad_out" || die "self-test: missed a planted defect at $want"
  done
  (( $(grep -cE '^bad\.md:[0-9]+:' <<<"$bad_out") == 8 )) \
    || die 'self-test: expected exactly 8 hits in the bad forest'

  # 2. The good forest: exit 0 and not one hit line named.
  good_status=0
  good_out="$(DOCS_CLAUSE_LINKS_DIR="$fx/good" "$0" 2>&1)" || good_status=$?
  (( good_status == 0 )) \
    || die "self-test: failed on the good forest (false positive): $good_out"
  if grep -qE '^good\.md:[0-9]+:' <<<"$good_out"; then
    die "self-test: reported hits in the good forest: $good_out"
  fi

  # 3. The fence really resumes: exactly one hit, on the post-fence line. Two
  # hits means the fence stopped skipping; zero means it never reopened.
  fenced_status=0
  fenced_out="$(DOCS_CLAUSE_LINKS_DIR="$fx/fenced" "$0" 2>&1)" || fenced_status=$?
  (( fenced_status == 1 )) || die "self-test: exited $fenced_status on the fenced forest, expected 1"
  (( $(grep -cE '^fenced\.md:[0-9]+:' <<<"$fenced_out") == 1 )) \
    || die "self-test: fence tracking broke (expected exactly 1 hit, got: $fenced_out)"
  grep -qF 'fenced.md:4' <<<"$fenced_out" \
    || die 'self-test: the fence resumed on the wrong line'

  note 'self-test: 8 planted defects caught by name, good forest clean, fences tracked'
  exit 0
fi

check_tree "$SCAN_DIR"
note 'docs clause links: OK'
