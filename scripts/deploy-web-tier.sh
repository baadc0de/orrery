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
#   sudo ORRERY_INVITE_BIN=<built orrery-invite> ORRERY_ADMISSION_UPGRADE=1 \
#        ./scripts/deploy-web-tier.sh            ...and ship a NEW admission.py over the
#                                                host's, which the drift gate otherwise
#                                                refuses (see ADMISSION_UPGRADE below)
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
# That restart is then verified against the running process rather than
# assumed (#1067): the installer compares what systemd says the service's
# main process started at with the mtime of the admission.py it just
# installed, and refuses to claim the host runs the new pair unless the
# process is the younger of the two.
#
# The ORRERY_WEB_TIER_* variables exist only so --self-test can drive the
# installer against throwaway roots; an operator run leaves them unset, and
# they alone decide whether this run is a sandbox one. ORRERY_INVITE_BIN is
# the one variable an operator run must set (see the pair section), so it is
# emphatically NOT one of them: reading it as a sandbox marker silently
# disarmed the restart on every real deploy, which is #1067 exactly.
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

# The operator's statement that the tracked admission.py is a NEW VERSION
# rather than a host hand-edit the repo has not caught up with.
#
# The drift rule below was written for the 2026-09-03 recording, where the
# host was the truth and the repo was the copy: any host script differing
# from the tracked one was assumed to carry a hand-edit worth more than
# whatever the repo said, so the install refused. Correct then, and it made
# admission.py *undeployable* -- every genuine change to the service is a
# difference from the host by definition, so the installer refused every
# upgrade it existed to perform. #1119 was the first time that mattered: a
# client fix was ready and the service half could not be shipped by the one
# script that is allowed to ship it.
#
# The two cases are genuinely indistinguishable from the bytes alone -- a
# hand-edit and a new version both just differ -- so this asks. Setting it
# does not skip anything: the diff is still printed for the record, the
# pair's flag gate still runs, and the stage/rename/restart/verify
# transaction is unchanged. It only answers the question the installer
# cannot answer for itself.
#
# It is not a ORRERY_WEB_TIER_* variable and must never become one: those
# mark a sandbox run, and reading an operator-set variable as a sandbox
# marker is #1067 exactly (see the SANDBOX list below).
ADMISSION_UPGRADE=${ORRERY_ADMISSION_UPGRADE:-0}

# Any ORRERY_WEB_TIER_* override means --self-test is driving: skip
# everything beyond the plain file work -- no nginx, no root check.
#
# ORRERY_INVITE_BIN is deliberately absent from this list. It used to be in
# it, and because an operator run is *required* to set it (see the pair
# section), every operator run classified itself as a sandbox run and
# skipped the very restart the pair transaction exists to perform -- while
# still printing the line that says the host runs the new pair. That is
# #1067: the host ran the new binary against the old in-memory script for
# 33 minutes, and the installer said it had not. Anything added here must
# be a variable an operator run never sets.
SANDBOX=0
if [[ -n ${ORRERY_WEB_TIER_SRC_DIR:-}${ORRERY_WEB_TIER_NGINX_SITES_DIR:-}${ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR:-}${ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR:-}${ORRERY_WEB_TIER_BIN_DIR:-}${ORRERY_WEB_TIER_SYSTEMCTL:-} ]]; then
    SANDBOX=1
fi

# The service half of the install. A sandbox run has no systemd, so it
# normally does not touch the service at all -- but the restart and its
# verification are the part of the pair transaction that #1067 broke, so
# --self-test drives them through a stand-in systemctl instead of skipping
# them. When ORRERY_WEB_TIER_SYSTEMCTL names one, the service lane runs
# against that; an operator run leaves it unset and gets the real thing.
SERVICE_NAME=orrery-admission.service
SYSTEMCTL=${ORRERY_WEB_TIER_SYSTEMCTL:-systemctl}
MANAGE_SERVICE=1
if (( SANDBOX )) && [[ -z ${ORRERY_WEB_TIER_SYSTEMCTL:-} ]]; then
    MANAGE_SERVICE=0
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
    "$SYSTEMCTL" daemon-reload
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

# Epoch seconds at which the service's main process started, per systemd,
# or empty when there is no such process (never started, or dead).
#
# Two properties, because neither is universal. ExecMainStartTimestampUSec
# is preferred where it exists -- it needs no locale, no timezone and no
# date(1) round-trip -- but systemd 259 (the campaigns host, Ubuntu
# 259.5-0ubuntu3.4) does not expose it: `systemctl show -p` prints NOTHING
# and exits 0 for a property it does not know, so the reading came back
# empty on a perfectly healthy service. The caller reads empty as "no
# running main process", so every real deploy on that host died claiming
# the box office was DOWN while it was serving 200s -- fail-closed, and
# therefore never a false success, but it made the pair transaction
# impossible to complete and pointed the operator at an outage that was not
# happening (#1119).
#
# So: fall back to the human-readable ExecMainStartTimestamp, which every
# systemd in play does emit, parsed by date(1). It carries an explicit zone
# ("Sun 2026-09-06 14:18:07 CEST"), so the round-trip is exact rather than
# locale-dependent; an unset property is the empty string there too, and
# date(1) refuses it, which keeps "no main process" distinguishable from
# "property unknown to this systemd".
service_main_start_epoch() {
    local raw
    raw=$("$SYSTEMCTL" show "$SERVICE_NAME" -p ExecMainStartTimestampUSec 2>/dev/null) || raw=
    raw=${raw##*=}
    raw=${raw//[[:space:]]/}
    if [[ $raw =~ ^[0-9]+$ ]] && (( raw > 0 )); then
        echo $(( raw / 1000000 ))
        return 0
    fi
    raw=$("$SYSTEMCTL" show "$SERVICE_NAME" -p ExecMainStartTimestamp 2>/dev/null) || return 0
    raw=${raw#*=}
    # systemd prints an empty value for a service with no main process, and
    # the literal "n/a" for one that has never started.
    [[ -n ${raw//[[:space:]]/} && $raw != *n/a* ]] || return 0
    date -d "$raw" +%s 2>/dev/null || return 0
}

# Prove the *running service* is the pair that was just installed, not just
# the files on disk (#1067). The service imported admission.py once at
# start, so a process older than the script on disk is running code that no
# longer exists there -- the dangerous cell, live, and the state the
# installer's success line has no business describing as fixed.
#
# The comparison is the process's start time against the installed script's
# mtime, both in whole epoch seconds. The restart happens after the
# renames, so a genuinely restarted service starts at or after that mtime;
# only a restart that did not happen puts the process before it.
verify_service_runs_installed_pair() {
    local started installed_at
    installed_at=$(stat -c %Y "$ADMISSION_DST") \
        || die "post-restart check: cannot stat $ADMISSION_DST to date the install; the pair is on disk but the running service is unverified -- restart orrery-admission and check 'systemctl status $SERVICE_NAME' by hand"
    started=$(service_main_start_epoch)
    if [[ -z $started ]]; then
        die "post-restart check: $SERVICE_NAME reports no running main process after the restart. The new pair is on disk and the box office is DOWN -- admissions are failing now. Investigate with 'systemctl status $SERVICE_NAME' and 'journalctl -u $SERVICE_NAME -n 50'."
    fi
    if (( started < installed_at )); then
        die "post-restart check: $SERVICE_NAME has been running since epoch $started, which predates the admission.py installed at epoch $installed_at -- the restart did not replace the process. The host is in the dangerous cell RIGHT NOW: the new orrery-invite is on disk while the service still runs the admission.py it imported at start, so every admission fails (#1067/#1049). Run 'systemctl restart $SERVICE_NAME' and re-run this installer."
    fi
    note "pair: verified the running service was replaced -- $SERVICE_NAME started at epoch $started, at or after the pair installed at epoch $installed_at"
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
        # Either the host carries a hand-edit the repo has not caught up
        # with, or the repo carries a new version of the service. Only the
        # operator knows which; see ORRERY_ADMISSION_UPGRADE above. The diff
        # is printed either way, because it is the record of what changed on
        # the box office in both cases.
        note "admission.py: the host file differs from the tracked one. The diff (host -> tracked):"
        diff -u "$ADMISSION_DST" "$ADMISSION_PY" >&2 || true
        if [[ $ADMISSION_UPGRADE != 1 ]]; then
            die "admission.py: refusing to clobber the host copy. If the host carries a hand-edit, bring it into scripts/admission.py and commit it, then re-run. If the tracked copy is a NEW VERSION to ship, say so: re-run with ORRERY_ADMISSION_UPGRADE=1."
        fi
        note "admission.py: ORRERY_ADMISSION_UPGRADE=1 -- installing the tracked copy as a new version of the service (host sha256 $(sha256_of "$ADMISSION_DST"), tracked $(sha256_of "$ADMISSION_PY"))"
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

    # "Nothing to do" is a *byte* question, not a flag question. This used to
    # read `if [[ $host_adm == new ]]`, which called every post-#1014 host
    # script up to date whatever it contained -- harmless only because the
    # drift gate above refused such a run before it could get here. With an
    # upgrade able to pass that gate, a flag-only test would stage nothing,
    # restart nothing, and print "both halves already installed; unchanged"
    # over an unshipped fix: the #1067 shape, in the other direction.
    local adm_changes=1 inv_changes=1
    if [[ $host_adm == new ]] && cmp -s "$ADMISSION_DST" "$ADMISSION_PY"; then adm_changes=0; fi
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
    if (( MANAGE_SERVICE )); then
        "$SYSTEMCTL" restart "$SERVICE_NAME" || die "the pair is installed but 'systemctl restart $SERVICE_NAME' failed: the box office is down or still running the old script against the new binary. Investigate with 'systemctl status $SERVICE_NAME' and restart it."
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

    # And close it on the running service, which the files cannot speak
    # for: the success line below is a claim about what the host *runs*,
    # and #1067 was that claim printed over an unrestarted process. Nothing
    # says "the host now runs the new pair" until this has proved it.
    if (( ! MANAGE_SERVICE )); then
        note "pair: no systemd in this run, so the running service could not be verified; the files on disk are the only claim made"
        return 0
    fi
    verify_service_runs_installed_pair
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
        if (( MANAGE_SERVICE )); then reload_systemd; fi
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
    # The third argument is the one addition #1067 needed: a stand-in
    # systemctl. Without it the run has no service lane at all (the old
    # behaviour, and what every arm above wants); with it the installer
    # restarts and verifies through the stand-in, so the live path can be
    # driven with no systemd, no root and no real service.
    sandbox_install() {  # sandbox_install [invite-src] [bin-dir] [systemctl]
        local -a envv=(
            "ORRERY_WEB_TIER_SRC_DIR=$filled"
            "ORRERY_WEB_TIER_NGINX_SITES_DIR=$root/nginx/sites-available"
            "ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR=$root/nginx/sites-enabled"
            "ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR=$root/systemd"
            "ORRERY_WEB_TIER_BIN_DIR=${2:-$root/opt-bin}"
            "ORRERY_INVITE_BIN=${1:-$fakes/invite-new}"
        )
        if [[ -n ${3:-} ]]; then
            envv+=("ORRERY_WEB_TIER_SYSTEMCTL=$3")
        fi
        env -- "${envv[@]}" "$SELF"
    }

    local out status=0

    # An install whose source tree still carries a placeholder refuses, and
    # nothing may be written. The tracked files were filled from the live host
    # on 2026-09-04, so this arm plants its own marker in a copy rather than
    # relying on the shipped files staying incomplete -- a tracked file that
    # cannot install is a deploy blocked, not a safety property worth keeping.
    local unfilled=$root/unfilled
    mkdir -p "$unfilled"
    cp "$SRC_DIR"/campaigns.nginx.conf "$SRC_DIR"/orrery-admission.service \
       "$SRC_DIR"/admission.py "$unfilled"/ 2>/dev/null || true
    sed -i 's/^Description=.*/Description=ORRERY_PLACEHOLDER_UNIT_DESCRIPTION/' \
        "$unfilled/orrery-admission.service"
    out=$(ORRERY_WEB_TIER_SRC_DIR=$unfilled \
          ORRERY_WEB_TIER_NGINX_SITES_DIR=$root/nginx/sites-available \
          ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR=$root/nginx/sites-enabled \
          ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR=$root/systemd \
          "$SELF" 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: installing a source tree with a placeholder should have refused"
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
    grep -q "ORRERY_ADMISSION_UPGRADE=1" <<<"$out" \
        || die "self-test: the admission.py refusal does not say how to ship a new version, so the only script allowed to deploy the service cannot deploy a change to it"
    note "self-test: a drifted host admission.py is refused with a diff and left untouched"

    # ...and the same host, told the tracked copy is a new version, ships it
    # (#1119). Without this the drift gate makes admission.py undeployable:
    # every genuine change to the service differs from the host by
    # definition, so the refusal above fired on exactly the runs the
    # installer exists to perform.
    out=$(ORRERY_ADMISSION_UPGRADE=1 sandbox_install "$fakes/invite-new" "$drift" 2>&1) \
        || die "self-test: ORRERY_ADMISSION_UPGRADE=1 did not ship a new admission.py"
    cmp -s "$drift/admission.py" "$ADMISSION_PY" \
        || die "self-test: ORRERY_ADMISSION_UPGRADE=1 left the host's old admission.py in place"
    grep -q "installing the tracked copy as a new version" <<<"$out" \
        || die "self-test: the upgrade did not say it was replacing the host script"
    grep -q "both halves already installed; unchanged" <<<"$out" \
        && die "self-test: the upgrade reported the host unchanged while shipping a new script -- the restart would have been skipped over an unshipped fix (#1067)"
    note "self-test: ORRERY_ADMISSION_UPGRADE=1 ships a new admission.py over a differing host copy"

    # And it is not a blanket override: with the tracked and host copies
    # already identical there is nothing to ship, and the run must still be
    # the no-op it is without the flag.
    out=$(ORRERY_ADMISSION_UPGRADE=1 sandbox_install "$fakes/invite-new" "$drift" 2>&1) \
        || die "self-test: re-running the upgrade failed"
    grep -q "both halves already installed; unchanged" <<<"$out" \
        || die "self-test: an upgrade run with nothing to ship is not a no-op"
    note "self-test: an upgrade run with both halves already current stays a no-op"

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

    # ---- #1067: the pair installed over an ALREADY-RUNNING service --------
    # Every arm above proves things about files. The live failure was not
    # about files: the pair landed byte-perfect and the *process* went on
    # running the admission.py it had imported 16 minutes earlier, against
    # the new binary, while the installer printed its success line. Nothing
    # above could see that, because nothing above has a service at all --
    # the old/old arm installs onto a host where no process exists, so a
    # restart that never happens is indistinguishable from one that did.
    #
    # These two arms give the sandbox a service. A stand-in systemctl keeps
    # the running main process's start time in a state file, which is the
    # single fact the installer's verification reads; `restart` moves it to
    # now, `show` reports it. Seeding the file in the past is a service
    # that started before the install, exactly as on the live host.
    # Stand-in systemctl for --self-test. State: the main process's start
    # time in epoch microseconds, the one fact
    # verify_service_runs_installed_pair reads back through 'show'.
    #
    # `dialect` is which property this systemd knows, and it is a parameter
    # because the two in play disagree. "usec" is the older shape. "human"
    # is systemd 259 on the campaigns host, which does NOT know
    # ExecMainStartTimestampUSec and prints nothing for it -- the shape that
    # made every real deploy report a false outage. A stand-in that only
    # ever spoke "usec" is why the self-test could not see that.
    service_fake() {  # service_fake <path> <state-file> <restart-behaviour> [usec|human]
        cat > "$1" <<FAKE
#!/bin/sh
state=$2
dialect=${4:-usec}
case "\$1" in
    restart)
        $3
        ;;
    show)
        usec=\$(cat "\$state" 2>/dev/null || echo 0)
        case "\$*" in
            *ExecMainStartTimestampUSec*)
                # systemd 259 knows no such property: it prints nothing and
                # exits 0, which is the whole point of the "human" dialect.
                [ "\$dialect" = usec ] && printf 'ExecMainStartTimestampUSec=%s\\n' "\$usec"
                ;;
            *ExecMainStartTimestamp*)
                if [ "\$dialect" = human ]; then
                    if [ "\$usec" -gt 0 ] 2>/dev/null; then
                        printf 'ExecMainStartTimestamp=%s\\n' "\$(date -d "@\$(( usec / 1000000 ))" '+%a %Y-%m-%d %H:%M:%S %Z')"
                    else
                        printf 'ExecMainStartTimestamp=\\n'
                    fi
                fi
                ;;
        esac
        ;;
esac
exit 0
FAKE
        chmod 0755 "$1"
    }

    # (i) the restart works: the process is replaced, and only then does
    #     the installer get to claim the host runs the new pair.
    local live=$tmp/live-bin live_state=$tmp/live-state
    mkdir -p "$live"
    cp "$adm_old" "$live/admission.py"
    cp "$fakes/invite-old" "$live/orrery-invite"
    printf '%s000000\n' "$(( $(date +%s) - 3600 ))" > "$live_state"
    service_fake "$fakes/systemctl-live" "$live_state" 'date +%s000000 > "$state"'
    out=$(sandbox_install "$fakes/invite-new" "$live" "$fakes/systemctl-live" 2>&1) \
        || die "self-test: installing the pair over a running service failed"
    grep -q "verified the running service was replaced" <<<"$out" \
        || die "self-test: the install over a running service did not verify the process was replaced"
    grep -q "the host now runs the new pair" <<<"$out" \
        || die "self-test: a verified install did not print the success line"
    if (( $(cat "$live_state") / 1000000 < $(stat -c %Y "$live/admission.py") )); then
        die "self-test: the stand-in service was not restarted after the install"
    fi
    note "self-test: installing the pair over a running service restarts it and verifies the process was replaced"

    # (i-b) the same install against systemd 259, which does not know
    #       ExecMainStartTimestampUSec. This is the campaigns host. The
    #       verification used to read that one property, get the empty
    #       string systemd prints for a property it does not know, and
    #       declare the box office DOWN over a service that had restarted
    #       cleanly and was serving -- so the pair transaction could never
    #       be completed there, and the operator was sent after an outage
    #       that was not happening (#1119).
    local live259=$tmp/live259-bin live259_state=$tmp/live259-state
    mkdir -p "$live259"
    cp "$adm_old" "$live259/admission.py"
    cp "$fakes/invite-old" "$live259/orrery-invite"
    printf '%s000000\n' "$(( $(date +%s) - 3600 ))" > "$live259_state"
    service_fake "$fakes/systemctl-259" "$live259_state" 'date +%s000000 > "$state"' human
    out=$(sandbox_install "$fakes/invite-new" "$live259" "$fakes/systemctl-259" 2>&1) \
        || die "self-test: installing the pair on a systemd that lacks ExecMainStartTimestampUSec failed; the campaigns host cannot be deployed to"
    grep -q "verified the running service was replaced" <<<"$out" \
        || die "self-test: a healthy restart on systemd 259 was not verified"
    grep -q "the box office is DOWN" <<<"$out" \
        && die "self-test: a healthy restart on systemd 259 was reported as an outage"
    note "self-test: a systemd that exposes only ExecMainStartTimestamp still verifies the restart"

    # (i-c) and the guard still bites on that dialect: a restart that does
    #       nothing must fail there too, or the fallback has bought
    #       deployability by disarming #1067's check.
    local stale259=$tmp/stale259-bin stale259_state=$tmp/stale259-state
    mkdir -p "$stale259"
    cp "$adm_old" "$stale259/admission.py"
    cp "$fakes/invite-old" "$stale259/orrery-invite"
    printf '%s000000\n' "$(( $(date +%s) - 3600 ))" > "$stale259_state"
    service_fake "$fakes/systemctl-259-stale" "$stale259_state" ':' human
    status=0
    out=$(sandbox_install "$fakes/invite-new" "$stale259" "$fakes/systemctl-259-stale" 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: on systemd 259 a restart that did not replace the process was reported as a successful install (#1067)"
    fi
    grep -q "the host now runs the new pair" <<<"$out" \
        && die "self-test: on systemd 259 the installer claimed the host runs the new pair over a stale process (#1067)"
    note "self-test: the #1067 guard still bites on a systemd that exposes only ExecMainStartTimestamp"

    # (ii) #1067 itself: the restart silently does nothing, so the process
    #      still predates the installed script. The installer must fail
    #      loudly and must NOT print the success line -- a wrong success
    #      message is worse than an error, because it is the one an
    #      operator acts on.
    local stale=$tmp/stale-bin stale_state=$tmp/stale-state
    mkdir -p "$stale"
    cp "$adm_old" "$stale/admission.py"
    cp "$fakes/invite-old" "$stale/orrery-invite"
    printf '%s000000\n' "$(( $(date +%s) - 3600 ))" > "$stale_state"
    service_fake "$fakes/systemctl-stale" "$stale_state" ':'
    status=0
    out=$(sandbox_install "$fakes/invite-new" "$stale" "$fakes/systemctl-stale" 2>&1) || status=$?
    if (( status == 0 )); then
        die "self-test: a restart that did not replace the process was reported as a successful install (#1067)"
    fi
    grep -q "the restart did not replace the process" <<<"$out" \
        || die "self-test: the un-restarted service was not named as the failure"
    if grep -q "the host now runs the new pair" <<<"$out"; then
        die "self-test: the installer claimed the host runs the new pair while the old process was still live (#1067)"
    fi
    note "self-test: a pair install whose service was not really restarted fails loudly and never claims success (#1067)"

    # (iii) the root cause under the two arms above: ORRERY_INVITE_BIN is
    #       required of every operator run, so reading it as a sandbox
    #       marker disarmed the restart on every real deploy. Setting it
    #       alone must leave the run a full one -- proved by the root check
    #       biting, which only a non-sandbox run reaches. Skipped when the
    #       gate itself runs as root, because there the next thing the
    #       installer would do is write /etc/nginx.
    if (( EUID != 0 )); then
        status=0
        out=$(env -u ORRERY_WEB_TIER_SRC_DIR -u ORRERY_WEB_TIER_NGINX_SITES_DIR \
                  -u ORRERY_WEB_TIER_NGINX_SITES_ENABLED_DIR \
                  -u ORRERY_WEB_TIER_SYSTEMD_UNITS_DIR -u ORRERY_WEB_TIER_BIN_DIR \
                  -u ORRERY_WEB_TIER_SYSTEMCTL \
                  -- "ORRERY_INVITE_BIN=$fakes/invite-new" "$SELF" 2>&1) || status=$?
        if (( status == 0 )); then
            die "self-test: an install with only ORRERY_INVITE_BIN set did not refuse"
        fi
        grep -q "run as root" <<<"$out" \
            || die "self-test: ORRERY_INVITE_BIN alone still puts the installer in sandbox mode, which is #1067's root cause"
        note "self-test: ORRERY_INVITE_BIN alone does not make a run a sandbox run (#1067)"
    else
        note "self-test: skipping the ORRERY_INVITE_BIN sandbox-marker arm -- it must not run as root"
    fi

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
