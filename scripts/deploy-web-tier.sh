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
# The same installer also installs the application pair the unit runs --
# admission.py and orrery-invite -- as one transaction (#1049). Those two
# ship in one commit but deploy as two files, and every half-deploy of them
# fails every admission; "Behaviour, for the pair" below is that coupling
# and what the installer does about it. orrery-invite is installed from a
# built binary the operator supplies, because the repository tracks the
# binary's source, not the binary:
#
# Usage:
#   sudo ORRERY_INVITE_BIN=<built orrery-invite> \
#        ./scripts/deploy-web-tier.sh            install all four files, reload what changed
#   ./scripts/deploy-web-tier.sh --self-test     per-commit checks: no root, no host,
#                                                no nginx, no network, no build
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
# so that stays the operator's call. The one exception is the pair below:
# when the install changes either of its two halves, the restart runs as
# part of the same transaction, because deferring it is itself the outage.
#
# The ORRERY_WEB_TIER_* variables exist only so --self-test can drive the
# installer against throwaway roots; an operator run leaves them unset.
# ORRERY_INVITE_BIN is the one variable an operator run must set (see the
# pair section); --self-test sets it to a stand-in.
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
BIN_DIR=${ORRERY_WEB_TIER_BIN_DIR:-/opt/orrery/bin}
NGINX_SITE_DST=$NGINX_SITES_DIR/campaigns
UNIT_DST=$UNITS_DIR/orrery-admission.service
ENABLED_LINK=$NGINX_ENABLED_DIR/campaigns
ADMISSION_DST=$BIN_DIR/admission.py
INVITE_DST=$BIN_DIR/orrery-invite
# Where the binary half of the #1049 pair comes from. The repository tracks
# orrery-invite's source (crates/orrery_identity), not its binary, so there
# is no path to default to and none is guessed: an unset value refuses the
# install the way an unfilled ORRERY_PLACEHOLDER refuses the config files.
INVITE_SRC=${ORRERY_INVITE_BIN:-}

# Any override means --self-test is driving: skip everything beyond the
# plain file work -- no nginx, no systemctl, no root check.
SANDBOX=0
if [[ -n ${ORRERY_WEB_TIER_SRC_DIR:-}${ORRERY_WEB_TIER_NGINX_SITES_DIR:-}${ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR:-}${ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR:-}${ORRERY_WEB_TIER_BIN_DIR:-}${ORRERY_INVITE_BIN:-} ]]; then
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
    note "systemd unit: daemon-reloaded"
}

# ---------------------------------------------------------------------------
# The #1049 pair: admission.py and orrery-invite deploy together or not at
# all.
#
# admission.py shells out to `orrery-invite session-token` on every admission
# (admission.py:750 and :769), and since #1014/#1047 that call carries
# --assume-standing-good while the binary refuses to mint without it. The
# two ship in one commit but install as two files, and the four host states
# are not symmetric. Each cell below was verified by running the real
# binaries -- current source, and the pre-#1047 tree (39b36f8) -- against a
# generated issuer credential:
#
#   old invite + old admission.py    works (the live host's current state)
#   NEW invite + old admission.py    every admission fails: the mint refuses
#                                    without the flag
#   old invite + NEW admission.py    every admission fails: clap exits 2 on
#                                    the unknown flag -- "unexpected argument
#                                    '--assume-standing-good' found"
#   new invite + new admission.py    works
#
# BOTH mixed cells fail, so there is no safe order to enforce: installing
# admission.py first does not tolerate the old binary, and installing the
# binary first is the dangerous cell outright. What both halves do agree on
# is the flag. --assume-standing-good is therefore the version marker this
# installer classifies both sides by -- the binary's `session-token --help`
# names it exactly when the binary accepts it, and the script carries it
# exactly when it passes it -- and the install is one transaction:
# everything that can refuse does so before anything is written, both files
# are staged and renamed into place back-to-back, and the box office is
# restarted before the run ends.
#
# The restart is part of the transaction because the running service is
# itself half of the coupling: it imported admission.py once at start, but
# it execs orrery-invite fresh from disk on every admission. A swapped
# binary beside the old script still in memory is the dangerous cell with
# admissions live, and it lasts exactly as long as the restart is deferred.
# For the nginx site and the unit, a deferred restart only delays
# activating new settings; here the wait is the outage, so this restart
# runs rather than prints.

# Classify an orrery-invite binary by the one flag both halves of the pair
# must agree on:
#   0  runs, and `session-token` accepts --assume-standing-good (post-#1014)
#   1  runs, and it does not (pre-#1014 -- the old half of the table above)
#   2  cannot be run at all; classify nothing, refuse everything
invite_flag_awareness() {  # invite_flag_awareness <binary>
    local out
    out=$("$1" session-token --help 2>&1) || return 2
    grep -q "assume-standing-good" <<<"$out"
}

script_passes_standing_flag() {  # script_passes_standing_flag <admission.py>
    grep -q -- "--assume-standing-good" "$1"
}

sha256_of() {  # sha256_of <file> -- for the record when a binary is replaced
    sha256sum "$1" | cut -d' ' -f1
}

# Copy src beside its destination without touching the destination: the
# commit is then a rename(2), so a failure partway through a copy cannot
# leave a truncated half at the destination the way an in-place copy could.
stage_file() {  # stage_file <src> <dst>
    install -m 0755 "$1" "$2.orrery-staged" || die "staging $(basename "$2") failed: nothing was installed, and the host files are untouched"
}

commit_staged() {  # commit_staged <dst> <label>
    local dst=$1 label=$2
    mv -f "$dst.orrery-staged" "$dst" || die "$label: the rename into place failed after the pair's other half was installed; the host is half-swapped (#1049). Re-run this installer -- it detects the mixed pair and completes it."
}

install_pair() {  # install_pair <unit_changed>
    local unit_changed=$1

    # Gate the inputs before the host is touched: a half the tracked pair
    # cannot work with must never be installed, or the installer's own
    # product is a mixed cell (#1049).
    if [[ -z $INVITE_SRC ]]; then
        die "ORRERY_INVITE_BIN is not set: the repository tracks orrery-invite's source, not its binary, so no path is guessed. Build the release binary (cargo build --release -p orrery_identity --bin orrery-invite) and point ORRERY_INVITE_BIN at it."
    fi
    local state=0
    invite_flag_awareness "$INVITE_SRC" || state=$?
    if (( state == 2 )); then
        die "ORRERY_INVITE_BIN ($INVITE_SRC) cannot be run; refusing to install an unrunnable binary beside the tracked admission.py"
    elif (( state == 1 )); then
        die "$INVITE_SRC does not accept --assume-standing-good: installing it beside the tracked admission.py is the old-invite/new-script cell of #1049, where clap rejects the unknown flag and every admission fails. Build the current source (crates/orrery_identity) and point ORRERY_INVITE_BIN at that binary."
    fi
    if ! script_passes_standing_flag "$ADMISSION_PY"; then
        die "tracked scripts/admission.py no longer passes --assume-standing-good to session-token: the pair contract (#1047) is broken at the source, and installing this beside a current binary is the new-invite/old-script cell, where the mint refuses every admission. Restore the call before deploying."
    fi

    # Classify the host by the same flag. An unrunnable host binary is
    # refused rather than read as "old": old leads to replacement, and
    # replacing a binary the installer could not examine is a clobber.
    local host_inv=none host_adm=none
    if [[ -e $INVITE_DST ]]; then
        state=0
        invite_flag_awareness "$INVITE_DST" || state=$?
        if (( state == 2 )); then
            die "$INVITE_DST exists but cannot be run; refusing to classify or replace it. Remove it by hand (or fix its permissions) if it is genuinely broken, then re-run."
        fi
        host_inv=old
        if (( state == 0 )); then host_inv=new; fi
    fi
    if [[ -e $ADMISSION_DST ]]; then
        host_adm=old
        if script_passes_standing_flag "$ADMISSION_DST"; then host_adm=new; fi
    fi

    # The dangerous cell, named here rather than discovered per admission
    # when admissions start failing. Completing the pair is the remedy for
    # a mixed host, so the installer says what it found and finishes the
    # job instead of refusing and leaving the host broken.
    if [[ $host_inv != "$host_adm" ]]; then
        note "DANGER: the host pair is mixed -- orrery-invite is ${host_inv}, admission.py is ${host_adm} (#1049)."
        case "$host_inv:$host_adm" in
            new:old) note "DANGER: in this cell the running script's session-token call is refused by the new mint, so every admission fails right now." ;;
            old:new) note "DANGER: in this cell the running script passes --assume-standing-good to a mint that has never heard of it; clap rejects the unknown flag and every admission fails right now." ;;
            *)       note "DANGER: only one of the two files is present, so the service cannot run as installed." ;;
        esac
        note "DANGER: completing the pair so the host ends consistent."
    fi

    # Per-file drift decisions, all made before anything is written.
    #
    # admission.py keeps the config files' rule: a host script that already
    # passes the flag but differs from the tracked one is a hand-edit or a
    # newer version, and the diff is the operator's to reconcile through
    # the repo. A script that predates the flag is the old half of a paired
    # upgrade; it always differs from tracked by definition, so its diff is
    # printed for the record and the upgrade proceeds.
    if [[ $host_adm == new ]] && ! cmp -s "$ADMISSION_DST" "$ADMISSION_PY"; then
        note "admission.py: the host file differs from the tracked one; refusing to clobber it. The diff (host -> tracked):"
        diff -u "$ADMISSION_DST" "$ADMISSION_PY" >&2 || true
        die "admission.py: reconcile first -- bring the host's version into scripts/admission.py and commit it, or fix the repo copy -- then re-run"
    fi
    if [[ $host_adm == old ]]; then
        note "admission.py: the host copy predates --assume-standing-good; the paired upgrade replaces it. The diff (host -> tracked), for the record:"
        diff -u "$ADMISSION_DST" "$ADMISSION_PY" >&2 || true
    fi
    #
    # The binary gets the shorter rule, because it can get no other: it has
    # no readable diff and no repo copy to reconcile into, and the flag is
    # the only version marker it carries -- a host binary that accepts the
    # flag is compatible with the tracked script by definition (#1047's
    # contract), whatever else changed between builds. Refusing byte
    # differences here would make every rebuild after the first
    # undeployable, so a differing flag-aware binary is replaced, loudly,
    # with both hashes for the record.
    if [[ $host_inv == new ]] && ! cmp -s "$INVITE_DST" "$INVITE_SRC"; then
        note "orrery-invite: the host binary is a different post-#1014 build (host sha256 $(sha256_of "$INVITE_DST"), supplied $(sha256_of "$INVITE_SRC")); both accept the flag, so the supplied build replaces it. Point ORRERY_INVITE_BIN at the build you intend."
    fi

    local adm_changes=1 inv_changes=1
    if [[ $host_adm == new ]]; then adm_changes=0; fi
    if [[ $host_inv == new ]] && cmp -s "$INVITE_DST" "$INVITE_SRC"; then inv_changes=0; fi
    if (( ! adm_changes && ! inv_changes )); then
        note "pair: both halves already installed; unchanged"
        if (( unit_changed && ! SANDBOX )); then
            note "the unit changed this run: run 'systemctl restart orrery-admission' to activate the new ExecStart (restarting the box office interrupts joins in flight, so that stays your call)"
        fi
        return 0
    fi

    # Every refusal is above this line: past it, the install happens. The
    # bin directory is created here rather than assumed -- a rebuilt host
    # has nothing until the operator puts it there.
    mkdir -p "$BIN_DIR"

    # The transaction: both halves staged, then committed back-to-back,
    # admission.py first. The two renames are not one syscall, so a kill
    # exactly between them leaves the on-disk pair mixed -- a window one
    # rename wide, which the next run of this installer classifies as mixed
    # and completes. Script-first keeps even that window harmless while it
    # lasts: the running service pairs its in-memory old script with the
    # still-old binary until the second rename.
    if (( adm_changes )); then
        stage_file "$ADMISSION_PY" "$ADMISSION_DST"
    else
        note "admission.py: already installed; unchanged"
    fi
    if (( inv_changes )); then
        stage_file "$INVITE_SRC" "$INVITE_DST"
    else
        note "orrery-invite: already installed; unchanged"
    fi
    if (( adm_changes )); then commit_staged "$ADMISSION_DST" "admission.py"; fi
    if (( inv_changes )); then commit_staged "$INVITE_DST" "orrery-invite"; fi

    # Not printed: with the old script still in memory, every admission
    # execs the new binary and fails, so deferring the restart is itself
    # the outage (#1049). A restart after a daemon-reload also activates
    # the unit's new ExecStart when the unit changed in the same run.
    if (( ! SANDBOX )); then
        systemctl restart orrery-admission || die "the pair is installed but 'systemctl restart orrery-admission' failed: the box office is down or still running the old script against the new binary. Investigate with 'systemctl status orrery-admission' and restart it."
        if (( unit_changed )); then
            note "pair: orrery-admission restarted on the new script and binary; this also activated the unit's new ExecStart"
        else
            note "pair: orrery-admission restarted on the new script and binary"
        fi
    fi

    # Close the loop on the host's own files: both installed halves must
    # classify as the new pair, or the transaction failed in a way the
    # renames did not report.
    invite_flag_awareness "$INVITE_DST" || die "post-install check failed: $INVITE_DST does not accept --assume-standing-good; the host is left in a mixed cell (#1049) and admissions will fail until it is fixed"
    script_passes_standing_flag "$ADMISSION_DST" || die "post-install check failed: $ADMISSION_DST does not pass --assume-standing-good; the host is left in a mixed cell (#1049) and admissions will fail until it is fixed"
    note "pair: the host now runs the new pair -- both halves agree on --assume-standing-good"
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

    local unit_changed=0
    if install_file "$UNIT_SRC" "$UNIT_DST" "systemd unit"; then
        unit_changed=1
        (( SANDBOX )) || reload_systemd
    fi

    install_pair "$unit_changed"
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
    mkdir -p "$root/nginx/sites-available" "$root/nginx/sites-enabled" "$root/systemd" "$root/opt-bin"
    local filled=$tmp/filled
    mkdir -p "$filled"
    sed 's/ORRERY_PLACEHOLDER[A-Z_]*/filled-in-by-operator/g' "$NGINX_SITE_SRC" > "$filled/campaigns.nginx.conf"
    sed 's/ORRERY_PLACEHOLDER[A-Z_]*/filled-in-by-operator/g' "$UNIT_SRC" > "$filled/orrery-admission.service"
    limits_agree "$filled/campaigns.nginx.conf" "$filled/orrery-admission.service" \
        || die "self-test: filling the placeholders broke the agreement check"

    # Fixtures for the #1049 pair. The installer classifies an orrery-invite
    # by `session-token --help`, so a fake that prints help with or without
    # the flag stands in for the real binary, post- and pre-#1014. The
    # old-lineage admission.py is the tracked one with the flag stripped,
    # which is exactly what made the pre-#1047 file the old half.
    local fakes=$tmp/fakes
    mkdir -p "$fakes"
    cat > "$fakes/invite-new" <<'FAKE'
#!/bin/sh
if [ "$1 $2" = "session-token --help" ]; then
    echo "Usage: orrery-invite session-token [options]"
    echo "  --assume-standing-good  attest the account's standing yourself"
    exit 0
fi
echo "fake orrery-invite: refusing: $*" >&2
exit 2
FAKE
    cat > "$fakes/invite-old" <<'FAKE'
#!/bin/sh
if [ "$1 $2" = "session-token --help" ]; then
    echo "Usage: orrery-invite session-token --issuer-credential <CRED> --account <ACCOUNT> --node <NODE>"
    exit 0
fi
echo "fake orrery-invite: refusing: $*" >&2
exit 2
FAKE
    chmod 0755 "$fakes/invite-new" "$fakes/invite-old"
    local adm_old=$tmp/admission-old.py
    sed 's/--assume-standing-good//g' "$ADMISSION_PY" > "$adm_old"
    if grep -q -- "--assume-standing-good" "$adm_old"; then
        die "self-test: the old-lineage admission fixture still passes the flag"
    fi

    local site=$root/nginx/sites-available/campaigns unit=$root/systemd/orrery-admission.service link=$root/nginx/sites-enabled/campaigns
    sandbox_install() {  # sandbox_install [invite-src] [bin-dir]
        ORRERY_WEB_TIER_SRC_DIR=$filled \
        ORRERY_WEB_TIER_NGINX_SITES_DIR=$root/nginx/sites-available \
        ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR=$root/nginx/sites-enabled \
        ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR=$root/systemd \
        ORRERY_WEB_TIER_BIN_DIR=${2:-$root/opt-bin} \
        ORRERY_INVITE_BIN=${1:-$fakes/invite-new} \
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
    # Restore the unit so the pair arms below reach their own checks.
    cp "$filled/orrery-admission.service" "$unit"

    # ---- the #1049 pair ---------------------------------------------------
    # The two application halves install together or not at all: a supplied
    # binary without the flag is refused at the gate, a correct install puts
    # down both halves and verifies them, the mixed host (the dangerous
    # cell) is named and completed, script drift still refuses with a diff,
    # and an old/old host upgrades in one transaction.

    # The input gate: an ORRERY_INVITE_BIN that does not accept the flag
    # would make the installer itself produce the old-invite/new-script
    # cell, so it is refused before anything is written.
    local gate=$tmp/gate-bin
    mkdir -p "$gate"
    status=0
    out=$(sandbox_install "$fakes/invite-old" "$gate" 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: an ORRERY_INVITE_BIN without --assume-standing-good did not refuse the install"
    fi
    grep -q "does not accept --assume-standing-good" <<<"$out" \
        || die "self-test: the supplied-binary refusal does not name the flag and the cell"
    if [[ -e $gate/admission.py || -e $gate/orrery-invite ]]; then
        die "self-test: the supplied-binary refusal wrote files anyway"
    fi
    note "self-test: a supplied orrery-invite without --assume-standing-good is refused, writing nothing"

    # A correct install puts down both halves together and both verify
    # flag-aware afterwards; installing again touches nothing.
    local fresh=$tmp/fresh-bin
    local padm=$fresh/admission.py pinv=$fresh/orrery-invite
    sandbox_install "$fakes/invite-new" "$fresh" >/dev/null 2>&1 \
        || die "self-test: a fresh paired install failed"
    [[ -x $pinv && -x $padm ]] || die "self-test: the fresh paired install did not produce an executable pair"
    "$pinv" session-token --help | grep -q "assume-standing-good" \
        || die "self-test: the installed binary does not accept the flag"
    grep -q -- "--assume-standing-good" "$padm" \
        || die "self-test: the installed script does not pass the flag"
    note "self-test: a fresh install puts down both halves and both verify flag-aware"

    local before_adm before_inv
    before_adm=$(cat "$padm"); before_inv=$(cat "$pinv")
    out=$(sandbox_install "$fakes/invite-new" "$fresh" 2>&1) \
        || die "self-test: re-running the pair install failed"
    grep -q "pair: both halves already installed; unchanged" <<<"$out" \
        || die "self-test: re-running the pair install is not a no-op"
    if [[ $(cat "$padm") != "$before_adm" || $(cat "$pinv") != "$before_inv" ]]; then
        die "self-test: re-running the pair install changed the files"
    fi
    note "self-test: re-running the pair install is a no-op"

    # The dangerous cell itself: a host whose binary is new while its script
    # is old -- every admission fails right now, because the mint refuses
    # the flag-less call. The installer must say so, by name, and complete
    # the pair rather than leave the host broken.
    local mixed=$tmp/mixed-bin
    mkdir -p "$mixed"
    sandbox_install "$fakes/invite-new" "$mixed" >/dev/null 2>&1 \
        || die "self-test: seeding the mixed-cell host failed"
    cp "$adm_old" "$mixed/admission.py"
    if grep -q -- "--assume-standing-good" "$mixed/admission.py"; then
        die "self-test: the dangerous-cell fixture did not apply"
    fi
    before_inv=$(cat "$mixed/orrery-invite")
    out=$(sandbox_install "$fakes/invite-new" "$mixed" 2>&1) \
        || die "self-test: completing the dangerous cell failed"
    grep -q "the host pair is mixed" <<<"$out" \
        || die "self-test: the dangerous cell was not named"
    grep -q "every admission fails right now" <<<"$out" \
        || die "self-test: the dangerous cell's consequence was not said"
    cmp -s "$mixed/admission.py" "$ADMISSION_PY" \
        || die "self-test: completing the pair did not restore the tracked script"
    if [[ $(cat "$mixed/orrery-invite") != "$before_inv" ]]; then
        die "self-test: completing the pair replaced the binary needlessly"
    fi
    note "self-test: the dangerous cell (new invite + old script) is named and completed"

    # The other mixed cell, old binary + new script, is named with its own
    # consequence and completed the same way.
    cp "$fakes/invite-old" "$mixed/orrery-invite"
    out=$(sandbox_install "$fakes/invite-new" "$mixed" 2>&1) \
        || die "self-test: completing the old-invite mixed cell failed"
    grep -q "never heard of it" <<<"$out" \
        || die "self-test: the old-invite mixed cell was not named with its consequence"
    "$mixed/orrery-invite" session-token --help | grep -q "assume-standing-good" \
        || die "self-test: the old invite was not replaced"
    note "self-test: the other mixed cell (old invite + new script) is named and completed"

    # Script drift still refuses with a diff, dangerous cell or not: a host
    # script that passes the flag but differs from the tracked one is a
    # hand-edit or a newer version, and the diff is the operator's to
    # reconcile through the repo.
    local drift=$tmp/drift-bin
    mkdir -p "$drift"
    sandbox_install "$fakes/invite-new" "$drift" >/dev/null 2>&1 \
        || die "self-test: seeding the drift host failed"
    printf '\n# a hand-edit the repo has never seen\n' >> "$drift/admission.py"
    status=0
    out=$(sandbox_install "$fakes/invite-new" "$drift" 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: a drifted host admission.py did not refuse the install"
    fi
    grep -q "a hand-edit the repo has never seen" <<<"$out" \
        || die "self-test: the admission.py refusal did not print the diff"
    grep -q "a hand-edit the repo has never seen" "$drift/admission.py" \
        || die "self-test: the admission.py refusal clobbered the host file"
    note "self-test: a drifted host admission.py is refused with a diff and left untouched"

    # The live host's current cell, old/old, upgrades to the new pair in one
    # transaction, with the replaced script's diff printed for the record.
    local oldpair=$tmp/oldpair-bin
    mkdir -p "$oldpair"
    cp "$adm_old" "$oldpair/admission.py"
    cp "$fakes/invite-old" "$oldpair/orrery-invite"
    out=$(sandbox_install "$fakes/invite-new" "$oldpair" 2>&1) \
        || die "self-test: the paired upgrade from the old pair failed"
    grep -q "predates --assume-standing-good" <<<"$out" \
        || die "self-test: the upgrade did not name the old script for the record"
    cmp -s "$oldpair/admission.py" "$ADMISSION_PY" \
        || die "self-test: the upgrade did not install the tracked script"
    "$oldpair/orrery-invite" session-token --help | grep -q "assume-standing-good" \
        || die "self-test: the upgrade did not install the supplied binary"
    note "self-test: an old/old host upgrades to the new pair in one transaction"

    # An unrunnable host binary is refused rather than classified old:
    # "old" leads to replacement, and replacing a binary the installer could
    # not examine would be a clobber.
    local dead=$tmp/dead-bin
    mkdir -p "$dead"
    printf '#!/bin/sh\nexit 127\n' > "$dead/orrery-invite"
    chmod 0755 "$dead/orrery-invite"
    status=0
    out=$(sandbox_install "$fakes/invite-new" "$dead" 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: an unrunnable host orrery-invite did not refuse the install"
    fi
    grep -q "cannot be run" <<<"$out" \
        || die "self-test: the unrunnable-binary refusal does not say so"
    note "self-test: an unrunnable host orrery-invite is refused rather than classified and clobbered"

    echo "deploy-web-tier: self-test passed"
}

case ${1:-} in
    --self-test) self_test ;;
    "") install_all ;;
    *) die "unknown argument '${1}'; expected no arguments to install, or --self-test" ;;
esac
