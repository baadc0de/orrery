#!/usr/bin/env bash
# Read-only release satisfiability preflight for admission campaigns (#486).
#
#   ./scripts/campaign-release-preflight.sh [--control /etc/orrery/campaigns.conf]
#   ./scripts/campaign-release-preflight.sh --self-test
#
# Run this on the admission box before opening a campaign.  It reads the same
# INI control file as admission.py and asks GitHub for every release.  It does
# not change the control file, GitHub, or the admission service.
#
# A pinned campaign passes only when a non-draft GitHub release targets that
# revision and carries at least one packaged Regolith client archive.  GitHub
# exposes the release target but not an asset's embedded build revision, so this
# is release metadata provenance, not binary attestation.  A later asset upload
# can disagree with a release target.  package-client.yml stamps ORRERY_BUILD_REV
# and creates the release with GITHUB_SHA as its target.
#
# Empty client_rev is WARN, not PASS: the control format permits an unpinned
# debug campaign, but it admits builds older than the campaign's ruleset.  The
# warning preserves that deliberate escape hatch while making it visible.
set -uo pipefail

readonly NAME=campaign-release-preflight
readonly DEFAULT_CONTROL=/etc/orrery/campaigns.conf
readonly DEFAULT_REPO=baadc0de/orrery

CONTROL="$DEFAULT_CONTROL"
REPO="${CAMPAIGN_RELEASE_PREFLIGHT_REPO:-$DEFAULT_REPO}"
GH_BIN="${CAMPAIGN_RELEASE_PREFLIGHT_GH_BIN:-gh}"
PYTHON_BIN="${CAMPAIGN_RELEASE_PREFLIGHT_PYTHON_BIN:-python3}"

failures=0
unknowns=0
warnings=0

usage() {
    sed -n '2,/^set -uo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -uo/d' >&2
}

result() { # verdict, name, detail...
    local verdict=$1 name=$2
    shift 2
    printf '%s %s %s\n' "$verdict" "$name" "$*"
    case "$verdict" in
        FAIL) ((failures += 1)) ;;
        UNKNOWN) ((unknowns += 1)) ;;
        WARN) ((warnings += 1)) ;;
    esac
}

require_tool() { # binary, named check
    local binary=$1 check=$2
    if ! command -v "$binary" >/dev/null 2>&1; then
        result UNKNOWN "$check" "required command '$binary' is unavailable"
        return 1
    fi
}

read_campaigns() {
    # Keep the parser's acceptance criteria aligned with Admission.campaigns:
    # an INI that cannot actually start a campaign is UNKNOWN here, not a
    # convenient partial success.  JSON keeps section values unambiguous at
    # the bash boundary.
    "$PYTHON_BIN" - "$CONTROL" <<'PY'
import configparser
import json
import re
import sys

path = sys.argv[1]
parser = configparser.ConfigParser(interpolation=None)
try:
    with open(path, encoding="utf-8") as source:
        parser.read_file(source)
    campaigns = []
    for ident in parser.sections():
        if not re.fullmatch(r"[a-z0-9-]{1,64}", ident):
            raise ValueError(f"invalid campaign id {ident!r}")
        section = parser[ident]
        # These accesses deliberately mirror scripts/admission.py:73-75.
        section["title"]
        section["host"]
        section.getint("peers")
        section.getint("seconds")
        section.getint("loss_pct")
        section.getint("jitter_ms")
        campaigns.append({"id": ident, "client_rev": section.get("client_rev") or None})
except (OSError, ValueError, configparser.Error, KeyError) as error:
    raise SystemExit(f"{path}: campaigns.conf failed to parse: {error}")

print(json.dumps(campaigns, separators=(",", ":")))
PY
}

run_checks() {
    local campaigns releases output status=0
    printf 'MODE campaign-release-preflight control=%s repo=%s\n' "$CONTROL" "$REPO"

    if ! require_tool "$PYTHON_BIN" campaigns-control; then
        summary
        return 1
    fi
    output="$(read_campaigns 2>&1)" || status=$?
    if ((status != 0)); then
        result UNKNOWN campaigns-control "$output"
        summary
        return 1
    fi
    campaigns=$output

    if ! require_tool "$GH_BIN" published-client-releases; then
        summary
        return 1
    fi
    # `gh release list --json` does not expose targetCommitish.  The REST
    # release endpoint does; --paginate --slurp retains every page as valid
    # JSON so an empty or malformed response cannot be mistaken for no work.
    output="$("$GH_BIN" api --paginate --slurp "repos/$REPO/releases?per_page=100" 2>&1)" || status=$?
    if ((status != 0)); then
        result UNKNOWN published-client-releases "GitHub release probe failed ($output)"
        summary
        return 1
    fi
    releases=$output

    output="$("$PYTHON_BIN" - "$campaigns" "$releases" 2>&1 <<'PY'
import json
import re
import sys

try:
    campaigns = json.loads(sys.argv[1])
    pages = json.loads(sys.argv[2])
    if not isinstance(campaigns, list) or not isinstance(pages, list):
        raise ValueError("expected JSON arrays")
    if not pages:
        raise ValueError("GitHub returned no release pages")
    releases = []
    for page in pages:
        if not isinstance(page, list):
            raise ValueError("a GitHub release page is not an array")
        releases.extend(page)
    if not releases:
        raise ValueError("GitHub returned no releases")
    available = []
    for release in releases:
        if not isinstance(release, dict):
            raise ValueError("a GitHub release is not an object")
        draft = release.get("draft")
        if not isinstance(draft, bool):
            raise ValueError("a GitHub release lacks a boolean draft flag")
        if draft:
            continue
        target = release.get("target_commitish")
        name = release.get("name") or release.get("tag_name")
        assets = release.get("assets")
        if not isinstance(target, str) or not isinstance(name, str) or not isinstance(assets, list):
            raise ValueError("a published release lacks target_commitish, name, or assets")
        client_assets = [asset.get("name") for asset in assets if isinstance(asset, dict)
                         and isinstance(asset.get("name"), str)
                         and re.fullmatch(r"orrery-regolith-[a-z0-9_]+-[a-z0-9-]+\.(?:tar\.gz|zip)", asset["name"])]
        if client_assets:
            available.append((name, target.lower(), len(client_assets)))
except (json.JSONDecodeError, ValueError, TypeError) as error:
    raise SystemExit(f"published-client-releases could not be interpreted: {error}")

for campaign in campaigns:
    ident = campaign.get("id")
    revision = campaign.get("client_rev")
    check = f"client-release:{ident}"
    if revision is None:
        print(f"WARN\t{check}\tno client_rev pin; legal for debug, unsafe for a banked campaign")
        continue
    if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-fA-F]{7,40}", revision):
        print(f"FAIL\t{check}\tclient_rev {revision!r} is not a 7-40 character Git revision")
        continue
    revision = revision.lower()
    matched = [(name, count) for name, target, count in available if target.startswith(revision)]
    if matched:
        name, count = matched[0]
        print(f"PASS\t{check}\trelease {name!r} targets {revision} and carries {count} Regolith client archive(s)")
    else:
        print(f"FAIL\t{check}\tno published Regolith release targets client_rev {revision}")
PY
    )" || status=$?
    if ((status != 0)); then
        result UNKNOWN published-client-releases "$output"
        summary
        return 1
    fi

    local verdict check detail
    while IFS=$'\t' read -r verdict check detail; do
        [[ -n $verdict && -n $check && -n $detail ]] \
            || { result UNKNOWN published-client-releases "parser emitted an incomplete verdict"; continue; }
        result "$verdict" "$check" "$detail"
    done <<<"$output"
    summary
    ((failures == 0 && unknowns == 0))
}

summary() {
    if ((failures || unknowns)); then
        printf 'SUMMARY FAIL failures=%d unknown=%d warnings=%d\n' "$failures" "$unknowns" "$warnings"
    else
        printf 'SUMMARY PASS failures=0 unknown=0 warnings=%d\n' "$warnings"
    fi
}

self_test() {
    local dir output status=0 passing=0 mutations=0
    dir="$(mktemp -d)"
    trap "rm -rf '$dir'" EXIT
    mkdir -p "$dir/bin"

    apply_fixture() {
        local name=$1 body=$2
        printf '%s\n' "$body" > "$dir/bin/$name"
        chmod +x "$dir/bin/$name"
    }
    apply_fixture gh '#!/usr/bin/env bash
case "${CAMPAIGN_RELEASE_PREFLIGHT_FIXTURE:-good}" in
  unavailable) echo "gh: API unavailable" >&2; exit 1 ;;
  empty) echo "[[]]" ;;
  no-match) echo "[[{\"name\":\"fixture-release\",\"draft\":false,\"target_commitish\":\"aaaaaaaa11111111111111111111111111111111\",\"assets\":[{\"name\":\"orrery-regolith-x86_64-linux.tar.gz\"}]}]]" ;;
  *) echo "[[{\"name\":\"fixture-release\",\"draft\":false,\"target_commitish\":\"11111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"assets\":[{\"name\":\"orrery-regolith-x86_64-linux.tar.gz\"}]}]]" ;;
esac'
    write_control() {
        local revision=$1
        printf '%s\n' "[fixture]" "title = Fixture" "open = yes" "host = fixture" \
            "peers = 8" "seconds = 60" "loss_pct = 3" "jitter_ms = 100" "client_rev = $revision" > "$dir/campaigns.conf"
    }
    st_run() {
        PATH="$dir/bin:$PATH" CAMPAIGN_RELEASE_PREFLIGHT_GH_BIN=gh \
            CAMPAIGN_RELEASE_PREFLIGHT_FIXTURE="$1" "$0" --control "$dir/campaigns.conf" 2>&1
    }
    st_missing_gh() {
        PATH="$dir/bin:$PATH" CAMPAIGN_RELEASE_PREFLIGHT_GH_BIN=missing-gh \
            "$0" --control "$dir/campaigns.conf" 2>&1
    }
    st_good() {
        status=0; output="$(st_run good)" || status=$?
        ((status == 0)) || die "self-test: restored matching fixture returned $status ($output)"
        grep -Fq 'PASS client-release:fixture ' <<<"$output" \
            || die 'self-test: matching release did not pass the named client-release:fixture check'
        ((passing += 1))
    }

    write_control 11111111
    st_good

    # The guarded stage must fail by the campaign's own named check, then
    # recover when the matching revision is restored.
    write_control deadbeef
    status=0; output="$(st_run no-match)" || status=$?
    ((status != 0)) || die 'self-test: unmatched pinned revision passed'
    grep -Fq 'FAIL client-release:fixture no published Regolith release targets client_rev deadbeef' <<<"$output" \
        || die 'self-test: unmatched pinned revision did not fail the named client-release:fixture check'
    ((mutations += 1))
    write_control 11111111
    st_good

    status=0; output="$(st_run unavailable)" || status=$?
    ((status != 0)) || die 'self-test: unavailable GitHub probe passed'
    grep -Fq 'UNKNOWN published-client-releases GitHub release probe failed' <<<"$output" \
        || die 'self-test: unavailable GitHub probe was not UNKNOWN'
    ((mutations += 1))
    st_good

    status=0; output="$(st_missing_gh)" || status=$?
    ((status != 0)) || die 'self-test: unavailable gh command passed'
    grep -Fq "UNKNOWN published-client-releases required command 'missing-gh' is unavailable" <<<"$output" \
        || die 'self-test: unavailable gh command was not UNKNOWN'
    ((mutations += 1))
    st_good

    status=0; output="$(st_run empty)" || status=$?
    ((status != 0)) || die 'self-test: empty release listing passed'
    grep -Fq 'UNKNOWN published-client-releases published-client-releases could not be interpreted: GitHub returned no releases' <<<"$output" \
        || die 'self-test: empty release listing was not UNKNOWN'
    ((mutations += 1))
    st_good

    echo "$NAME: self-test passed ($passing passing fixtures: baseline + $((passing - 1)) reversions; $mutations guarded mutations: unmatched FAIL, erroring UNKNOWN, missing-gh UNKNOWN, empty UNKNOWN)"
}

die() { echo "$NAME: $*" >&2; exit 2; }

while (($#)); do
    case "$1" in
        --control) shift; (($#)) || die '--control needs a path'; CONTROL=$1 ;;
        --repo) shift; (($#)) || die '--repo needs an owner/repository'; REPO=$1 ;;
        --self-test) self_test; exit $? ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown argument '$1'" ;;
    esac
    shift
done

run_checks
