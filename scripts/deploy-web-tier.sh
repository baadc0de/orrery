#!/usr/bin/env bash
# Install the tracked web-tier config for campaigns.distopik.com (#1002).
#
# Owner decision, 2026-09-03: the campaigns host's nginx site and the
# orrery-admission.service unit both carry settings that make banking work
# and that existed only as hand-edits on the host, lost when it was rebuilt:
#
#   client_max_body_size 64m;
#       in the nginx site. Without it nginx's 1 MiB default silently
#       refused every volunteer telemetry upload with HTTP 413 before
#       admission saw the request -- a 3.5-minute session is ~2 MB (#1002).
#   --public-origin https://campaigns.distopik.com
#       on the unit's ExecStart. Without it admission skips its startup
#       upload probe (#1011), so the nginx regression above is silent again.
#
# The tracked copies live beside the application whose ceiling they serve:
# campaigns.nginx.conf and orrery-admission.service, next to admission.py
# in scripts/. The nginx limit must equal MAX_UPLOAD_BYTES in admission.py
# -- limits_agree() below is the one definition of that agreement, and it
# runs both here, before anything is installed, and per-commit through
# --self-test in the gates lane.
#
# Usage:
#   sudo ./scripts/deploy-web-tier.sh            install both files, reload what changed
#   ./scripts/deploy-web-tier.sh --self-test     per-commit checks: no root, no host,
#                                                no nginx, no network
#
# Behaviour, per file:
#   - a host file matching the tracked one is left untouched (idempotent);
#   - a host file differing from the tracked one aborts the install with a
#     `diff -u` (host -> tracked) and changes nothing: reconcile by bringing
#     the host's version into the repo copy -- the host is the truth for
#     anything the 2026-09-03 recording missed -- committing it, and
#     re-running;
#   - an unfilled ORRERY_PLACEHOLDER marker in a tracked file refuses the
#     whole install before anything is written. The tracked files
#     deliberately do not guess what was not recorded from the live host --
#     the TLS listen directive, the certificate paths, the exact port-80
#     redirect, and the unit's scaffolding -- because a file that looks
#     right and is wrong is worse than one that is obviously incomplete.
#
# On change: the nginx site is validated with `nginx -t` and only then
# reloaded; the unit gets `systemctl daemon-reload`, and the script prints
# (but never runs) the `systemctl restart orrery-admission` that activates
# a new ExecStart -- restarting the box office interrupts joins in flight,
# so that stays the operator's call.
#
# The ORRERY_WEB_TIER_* variables exist only so --self-test can drive the
# installer against throwaway roots; an operator run leaves them unset.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SELF=${BASH_SOURCE[0]}

SRC_DIR=${ORRERY_WEB_TIER_SRC_DIR:-$SCRIPT_DIR}
NGINX_SITE_SRC=$SRC_DIR/campaigns.nginx.conf
UNIT_SRC=$SRC_DIR/orrery-admission.service
# The constant's home, whatever SRC_DIR is: the truth lives in the tracked
# module, not in the copy under test.
ADMISSION_PY=$SCRIPT_DIR/admission.py

NGINX_SITES_DIR=${ORRERY_WEB_TIER_NGINX_SITES_DIR:-/etc/nginx/sites-available}
NGINX_ENABLED_DIR=${ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR:-/etc/nginx/sites-enabled}
UNITS_DIR=${ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR:-/etc/systemd/system}
NGINX_SITE_DST=$NGINX_SITES_DIR/campaigns
UNIT_DST=$UNITS_DIR/orrery-admission.service
ENABLED_LINK=$NGINX_ENABLED_DIR/campaigns

# Any override means --self-test is driving: skip everything beyond the
# plain file work -- no nginx, no systemctl, no root check.
SANDBOX=0
if [[ -n ${ORRERY_WEB_TIER_SRC_DIR:-}${ORRERY_WEB_TIER_NGINX_SITES_DIR:-}${ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR:-}${ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR:-} ]]; then
    SANDBOX=1
fi

die() { echo "deploy-web-tier: $*" >&2; exit 2; }
note() { echo "deploy-web-tier: $*" >&2; }

# The one definition of "the tracked web-tier config agrees with
# admission": every client_max_body_size must equal MAX_UPLOAD_BYTES, the
# block proxying to admission must carry one (a missing directive silently
# inherits nginx's 1 MiB default -- the exact shape of #1002), the unit's
# ExecStart must keep --public-origin armed (#1011's probe is off without
# it, and the nginx regression is silent again), and the two files must
# not quietly disagree about the port admission listens on or the name it
# is served as.
limits_agree() {  # limits_agree <nginx-conf> <unit-file>
    PYTHONDONTWRITEBYTECODE=1 python3 - "$1" "$2" "$ADMISSION_PY" <<'PY'
import re, os, sys

nginx_path, unit_path, admission_path = sys.argv[1:4]

def fail(msg):
    print(f"deploy-web-tier: {msg}", file=sys.stderr)
    raise SystemExit(1)

# Read from the module, not from its source text, so a refactor of the
# constant cannot desync this check from what admission actually enforces.
sys.path.insert(0, os.path.dirname(os.path.abspath(admission_path)))
import admission
ceiling = admission.MAX_UPLOAD_BYTES

raw = open(nginx_path, encoding="utf-8").read()
# Comments first: the file's own comment names the constant and the
# directive, and must not be mistaken for a directive.
text = re.sub(r"#[^\n]*", "", raw)
if "MAX_UPLOAD_BYTES" not in raw:
    fail(f"{nginx_path}: no comment names MAX_UPLOAD_BYTES; the tie to the application's ceiling must stay visible in the file")

def blocks(name, text):
    """Bodies of every `name { ... }` block, nesting honoured."""
    out, i = [], 0
    pat = re.compile(r"\b" + name + r"\s*\{")
    while (m := pat.search(text, i)):
        depth, j = 1, m.end()
        while j < len(text) and depth:
            depth += (text[j] == "{") - (text[j] == "}")
            j += 1
        if depth: fail(f"{nginx_path}: unterminated {name} block")
        out.append(text[m.end():j - 1]); i = j
    return out

servers = blocks("server", text)
proxy_pat = re.compile(r"\bproxy_pass\s+http://(127\.0\.0\.1|localhost|\[::1\]):(\d+)\s*;")
proxies = [b for b in servers if proxy_pat.search(b)]
if len(proxies) != 1:
    fail(f"{nginx_path}: expected exactly one server block proxying to admission on loopback, found {len(proxies)}")

SIZE = re.compile(r"\bclient_max_body_size\s+(\d+)([kKmMgG]?)\s*;")
def to_bytes(value, suffix):
    return int(value) * {"": 1, "k": 1024, "m": 1024**2, "g": 1024**3}[suffix.lower()]

everywhere = SIZE.findall(text)
if not everywhere:
    fail(f"{nginx_path}: client_max_body_size is gone; nginx's 1 MiB default would refuse every volunteer upload again (#1002)")
if not SIZE.findall(proxies[0]):
    fail(f"{nginx_path}: the block proxying to admission carries no client_max_body_size and would inherit nginx's 1 MiB default (#1002)")
for value, suffix in everywhere:
    limit = to_bytes(value, suffix)
    if limit != ceiling:
        relation = "below" if limit < ceiling else "above"
        fail(f"{nginx_path}: client_max_body_size {value}{suffix} is {limit} bytes, {relation} MAX_UPLOAD_BYTES ({ceiling}); change MAX_UPLOAD_BYTES in scripts/admission.py first and keep the two equal (#1002)")

execstart = [line for line in open(unit_path, encoding="utf-8").read().splitlines() if line.startswith("ExecStart=")]
if len(execstart) != 1:
    fail(f"{unit_path}: expected exactly one ExecStart= line, found {len(execstart)}")
if "admission.py" not in execstart[0]:
    fail(f"{unit_path}: ExecStart does not run admission.py")
origin = re.search(r"(^|\s)--public-origin(?:=|\s+)(\S+)", execstart[0])
if origin is None:
    fail(f"{unit_path}: ExecStart carries no --public-origin; admission skips the startup upload probe (#1011) and a lost nginx body limit is silent again (#1002)")

listen = re.search(r"(^|\s)--listen(?:=|\s+)(\S+)", execstart[0])
if listen is None:
    fail(f"{unit_path}: ExecStart carries no --listen; the port nginx proxies to cannot be tied to admission's")
proxy_port = proxy_pat.search(proxies[0]).group(2)
if listen.group(2).rsplit(":", 1)[1] != proxy_port:
    fail(f"{unit_path} vs {nginx_path}: the unit's --listen port and the nginx proxy_pass port ({proxy_port}) disagree; one of the two was edited without the other")

host = re.sub(r"^https?://", "", origin.group(2)).rstrip("/")
for server in servers:
    if not re.search(rf"\bserver_name\s+[^;]*\b{re.escape(host)}\b", server):
        fail(f"{nginx_path}: a server block no longer names {host}, the origin the unit advertises and the probe dials")

print(f"deploy-web-tier: the nginx limit and MAX_UPLOAD_BYTES agree ({ceiling} bytes); the unit keeps --public-origin {origin.group(2)}")
PY
}

# The tracked files carry ORRERY_PLACEHOLDER markers for everything the
# 2026-09-03 recording did not capture (see the files' headers). An install
# that slipped one through would put a file on the host that looks finished
# and is not, so this refuses before anything is written.
check_placeholders() {
    local src hits
    for src in "$NGINX_SITE_SRC" "$UNIT_SRC"; do
        hits=$(grep -n "ORRERY_PLACEHOLDER" "$src") || true
        if [[ -n $hits ]]; then
            note "unfilled ORRERY_PLACEHOLDER markers in $src:"
            sed 's/^/  /' <<<"$hits" >&2
            die "refusing to install: fill the placeholders from the live host, commit, and re-run"
        fi
    done
}

# Copy src over dst unless dst already matches (no-op) or differs (refuse,
# with the diff, changing nothing). Returns 0 when it changed the file,
# 1 when there was nothing to do.
install_file() {  # install_file <src> <dst> <label>
    local src=$1 dst=$2 label=$3
    if [[ ! -e $dst ]]; then
        install -m 0644 "$src" "$dst"
        note "$label: installed $dst"
        return 0
    fi
    if cmp -s "$dst" "$src"; then
        note "$label: already installed; unchanged"
        return 1
    fi
    note "$label: the host file differs from the tracked one; refusing to clobber it. The diff (host -> tracked):"
    diff -u "$dst" "$src" >&2 || true
    die "$label: reconcile first -- bring the host's version into the repo copy and commit it, or fix the repo copy -- then re-run"
}

# Debian's nginx layout activates a site through a sites-enabled symlink.
# The live host carries one for this site; a rebuild loses it with
# everything else, so it is part of the install. Returns 0 when it created
# the link.
ensure_enabled_symlink() {
    local wanted=../sites-available/campaigns
    if [[ -L $ENABLED_LINK ]]; then
        if [[ $(readlink -f "$ENABLED_LINK") == $(readlink -f "$NGINX_SITE_DST") ]]; then
            return 1
        fi
        die "$ENABLED_LINK points at $(readlink "$ENABLED_LINK"), not at the site this script installs; refusing to touch it"
    fi
    if [[ -e $ENABLED_LINK ]]; then
        die "$ENABLED_LINK exists and is not a symlink; refusing to touch it"
    fi
    ln -s "$wanted" "$ENABLED_LINK"
    note "nginx site: created the $ENABLED_LINK symlink"
}

reload_nginx() {
    nginx -t || die "nginx -t failed after installing the site; nginx is still serving the old configuration -- fix what nginx -t names (usually the tracked copy), commit, and re-run"
    nginx -s reload
    note "nginx site: reloaded"
}

reload_systemd() {
    systemctl daemon-reload
    note "systemd unit: daemon-reloaded; run 'systemctl restart orrery-admission' to activate the new ExecStart (this script does not restart it)"
}

install_all() {
    if (( ! SANDBOX )) && (( EUID != 0 )); then
        die "run as root: the install writes $NGINX_SITES_DIR and $UNITS_DIR"
    fi
    check_placeholders
    if ! limits_agree "$NGINX_SITE_SRC" "$UNIT_SRC"; then
        die "the tracked web-tier config disagrees with admission; nothing was installed"
    fi

    local site_changed=0
    if install_file "$NGINX_SITE_SRC" "$NGINX_SITE_DST" "nginx site"; then site_changed=1; fi
    if ensure_enabled_symlink; then site_changed=1; fi
    if (( site_changed && ! SANDBOX )); then reload_nginx; fi

    if install_file "$UNIT_SRC" "$UNIT_DST" "systemd unit"; then
        (( SANDBOX )) || reload_systemd
    fi
}

self_test() {
    # Not local: the EXIT trap cleans it up after self_test has returned.
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    # As committed: the tracked files agree with admission.
    limits_agree "$NGINX_SITE_SRC" "$UNIT_SRC" \
        || die "self-test: the tracked files as committed fail the agreement check"
    note "self-test: the committed nginx limit equals MAX_UPLOAD_BYTES and the unit keeps --public-origin"

    # The check bites. Each mutant is one silent defect, and each is
    # verified to have actually changed the file before the failure is
    # demanded, so an edit that later breaks the sed cannot turn these into
    # vacuous passes.
    local mut=$tmp/mutated
    mkdir -p "$mut"
    local mnginx=$mut/campaigns.nginx.conf munit=$mut/orrery-admission.service
    local err=$tmp/err

    # (a) the #1002 regression itself: the limit shrunk below the ceiling.
    sed 's/client_max_body_size 64m;/client_max_body_size 1m;/' "$NGINX_SITE_SRC" > "$mnginx"
    cp "$UNIT_SRC" "$munit"
    grep -q "client_max_body_size 1m;" "$mnginx" || die "self-test: the shrunken-limit mutant did not apply"
    if limits_agree "$mnginx" "$munit" 2>"$err"; then
        die "self-test: a 1 MiB nginx limit passed the agreement check"
    fi
    grep -q "keep the two equal" "$err" || die "self-test: the shrunken-limit failure does not name the disagreement"
    note "self-test: a shrunken client_max_body_size fails the agreement check"

    # (b) the invisible half of the same defect: no directive at all, so
    #     nginx's 1 MiB default applies silently.
    sed '/client_max_body_size/d' "$NGINX_SITE_SRC" > "$mnginx"
    if grep -q "client_max_body_size" "$mnginx"; then
        die "self-test: the removed-directive mutant did not apply"
    fi
    if limits_agree "$mnginx" "$munit" 2>/dev/null; then
        die "self-test: a missing client_max_body_size passed the agreement check"
    fi
    note "self-test: a missing client_max_body_size fails the agreement check"

    # (c) the probe disarmed: --public-origin gone from the unit, so the
    #     startup check is off and any nginx regression is silent.
    sed 's/ --public-origin https:\/\/campaigns\.distopik\.com//' "$UNIT_SRC" > "$munit"
    if grep -q "^ExecStart=.*--public-origin" "$munit"; then
        die "self-test: the disarmed-probe mutant did not apply"
    fi
    cp "$NGINX_SITE_SRC" "$mnginx"
    if limits_agree "$mnginx" "$munit" 2>/dev/null; then
        die "self-test: an ExecStart without --public-origin passed the agreement check"
    fi
    note "self-test: an ExecStart without --public-origin fails the agreement check"

    # (d) the tie made invisible: the comment naming the constant removed.
    grep -v "MAX_UPLOAD_BYTES" "$NGINX_SITE_SRC" > "$mnginx"
    if grep -q "MAX_UPLOAD_BYTES" "$mnginx"; then
        die "self-test: the untied-comment mutant did not apply"
    fi
    cp "$UNIT_SRC" "$munit"
    if limits_agree "$mnginx" "$munit" 2>/dev/null; then
        die "self-test: a site whose comment no longer names MAX_UPLOAD_BYTES passed the agreement check"
    fi
    note "self-test: dropping the MAX_UPLOAD_BYTES comment fails the agreement check"

    # Installer behaviour, against throwaway roots. The filled copies stand
    # in for the operator's first real deploy: placeholders filled from the
    # host, committed, then installed.
    local root=$tmp/host
    mkdir -p "$root/nginx/sites-available" "$root/nginx/sites-enabled" "$root/systemd"
    local filled=$tmp/filled
    mkdir -p "$filled"
    sed 's/ORRERY_PLACEHOLDER[A-Z_]*/filled-in-by-operator/g' "$NGINX_SITE_SRC" > "$filled/campaigns.nginx.conf"
    sed 's/ORRERY_PLACEHOLDER[A-Z_]*/filled-in-by-operator/g' "$UNIT_SRC" > "$filled/orrery-admission.service"
    limits_agree "$filled/campaigns.nginx.conf" "$filled/orrery-admission.service" \
        || die "self-test: filling the placeholders broke the agreement check"

    local site=$root/nginx/sites-available/campaigns unit=$root/systemd/orrery-admission.service link=$root/nginx/sites-enabled/campaigns
    sandbox_install() {
        ORRERY_WEB_TIER_SRC_DIR=$filled \
        ORRERY_WEB_TIER_NGINX_SITES_DIR=$root/nginx/sites-available \
        ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR=$root/nginx/sites-enabled \
        ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR=$root/systemd \
        "$SELF"
    }

    local out status=0

    # A fresh install of the tree as committed refuses: the placeholders are
    # still there, and nothing may be written.
    out=$(ORRERY_WEB_TIER_NGINX_SITES_DIR=$root/nginx/sites-available \
          ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR=$root/nginx/sites-enabled \
          ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR=$root/systemd \
          "$SELF" 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: installing the unfilled tracked files should have refused"
    fi
    grep -q "ORRERY_PLACEHOLDER" <<<"$out" || die "self-test: the refusal does not name the placeholders"
    if [[ -e $site || -e $unit ]]; then
        die "self-test: the placeholder refusal wrote a file anyway"
    fi
    note "self-test: a fresh install refuses while ORRERY_PLACEHOLDER markers remain, writing nothing"

    # Filled copies install fresh, including the sites-enabled symlink.
    sandbox_install || die "self-test: a filled, agreeing config refused to install"
    cmp -s "$site" "$filled/campaigns.nginx.conf" || die "self-test: the installed site is not the tracked one"
    cmp -s "$unit" "$filled/orrery-admission.service" || die "self-test: the installed unit is not the tracked one"
    [[ -L $link ]] || die "self-test: the sites-enabled symlink was not created"
    note "self-test: a filled config installs fresh, including the sites-enabled symlink"

    # And installing again is a no-op.
    local before_site before_unit
    before_site=$(cat "$site")
    before_unit=$(cat "$unit")
    sandbox_install >/dev/null 2>&1 || die "self-test: re-running the install failed"
    if [[ $(cat "$site") != "$before_site" || $(cat "$unit") != "$before_unit" ]]; then
        die "self-test: re-running the install changed the files"
    fi
    note "self-test: re-running the installer is a no-op"

    # Host drift is refused with a diff, and the host file is left as found.
    sed 's/client_max_body_size 64m;/client_max_body_size 32m;/' "$site" > "$site.tmp" && mv "$site.tmp" "$site"
    status=0
    out=$(sandbox_install 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: a drifted host site did not refuse the install"
    fi
    grep -q "client_max_body_size 32m;" <<<"$out" || die "self-test: the site refusal did not print the diff"
    grep -q "client_max_body_size 32m;" "$site" || die "self-test: the site refusal clobbered the host file"
    note "self-test: a drifted host site is refused with a diff and left untouched"

    # The same for the unit. The site is restored first so the installer
    # reaches the unit step.
    cp "$filled/campaigns.nginx.conf" "$site"
    sed 's/ --public-origin https:\/\/campaigns\.distopik\.com//' "$unit" > "$unit.tmp" && mv "$unit.tmp" "$unit"
    if grep -q "^ExecStart=.*--public-origin" "$unit"; then
        die "self-test: the drifted-unit mutant did not apply"
    fi
    status=0
    out=$(sandbox_install 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: a drifted host unit did not refuse the install"
    fi
    grep -q "public-origin" <<<"$out" || die "self-test: the unit refusal did not print the diff"
    if grep -q "^ExecStart=.*--public-origin" "$unit"; then
        die "self-test: the unit refusal clobbered the host file"
    fi
    note "self-test: a drifted host unit is refused with a diff and left untouched"

    echo "deploy-web-tier: self-test passed"
}

case ${1:-} in
    --self-test) self_test ;;
    "") install_all ;;
    *) die "unknown argument '${1}'; expected no arguments to install, or --self-test" ;;
esac
