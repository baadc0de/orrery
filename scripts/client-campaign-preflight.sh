#!/usr/bin/env bash
# Prove that one built Regolith binary can join the deployed campaign after its
# initial cohort delay, that every seated client receives the other replicated
# crafts, and that closing those clients leaves a seat reusable (#587, #601,
# #681).
#
#   scripts/client-campaign-preflight.sh --binary PATH --campaign ID
#   scripts/client-campaign-preflight.sh --binary PATH --campaign ID --force-live
#   scripts/client-campaign-preflight.sh --self-test
#
# The binary is the probe. Its --build-info supplies the baked origin, and the
# three launches receive no --admission-url override. Their own headless join
# mode fetches the listing, applies the shipped joinability predicate, asks
# admission for a seat, completes the iroh handshake, and binds the requested
# nickname to a craft that arrived through replication. HTTP substitutes such
# as curl are deliberately absent.
#
# These clients take human seats and move no uplink frames, so any attempt
# they are frozen into fails its participation clause and banks nothing -- and
# a volunteer seated nearby reads as a broken player, which is how #995 came
# to be filed against a real one (#1008). A campaign with human seats taken or
# an attempt in progress is therefore refused before anything starts; a run
# against a live campaign is a decision, made by passing --force-live.
#
# Every client gets its own X server, and how that is arranged is load-bearing
# rather than incidental -- see `choose_display_isolation` (#1003).
set -uo pipefail

readonly NAME=client-campaign-preflight

BINARY=
CAMPAIGN=
FORCE_LIVE=0
TIMEOUT_SECS=1020
LATE_JOIN_DELAY_SECS=185
REJOIN_DELAY_SECS=3
# A full lobby plus an attempt, so a wait that starts just after one closes
# still reaches the next.
FRESH_LOBBY_TIMEOUT_SECS=${CLIENT_CAMPAIGN_PREFLIGHT_FRESH_LOBBY_TIMEOUT:-1200}
FRESH_LOBBY_POLL_SECS=5
XVFB_RUN_BIN="${CLIENT_CAMPAIGN_PREFLIGHT_XVFB_RUN:-xvfb-run}"
PYTHON_BIN="${CLIENT_CAMPAIGN_PREFLIGHT_PYTHON:-python3}"
# How each client is given its own X server. Set by `choose_display_isolation`.
XVFB_DISPLAY_MODE=
XVFB_DISPLAY_BASE=99
failures=0

usage() {
    sed -n '2,/^set -uo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -uo/d' >&2
}

die() { echo "$NAME: $*" >&2; exit 2; }

result() {
    local verdict=$1 check=$2
    shift 2
    printf '%s %s %s\n' "$verdict" "$check" "$*"
    [[ $verdict == PASS ]] || ((failures += 1))
}

require_marker() { # check, log, exact marker
    local check=$1 log=$2 marker=$3
    if grep -Fq -- "$marker" "$log"; then
        result PASS "$check" "$marker"
    else
        result FAIL "$check" "missing marker: $marker (log: $log)"
    fi
}

# Whether this phase is one the scenario below can actually start from.
#
# The scenario is written against one attempt: two clients join the lobby, a
# third joins 185 s later so it lands just after the cohort freezes, and a
# fourth reuses a released seat. Starting mid-attempt gets none of that -- one
# run measured today banked 9.6 s before the attempt ended underneath it, and
# another had all three clients join live rather than through the lobby, which
# silently changes which path is being tested.
#
# A lobby is the whole requirement. An earlier version of this waited for the
# *transition* into one, on the theory that a lobby about to close is as bad as
# an attempt; that is true in principle and useless in practice, because a
# standing host with nobody on it reopens its empty lobby without ever leaving
# the phase. The transition never came, every run paid the full timeout, and
# the wait then proceeded anyway -- twenty minutes for nothing, on the idle
# campaign that is the normal case for CI.
lobby_is_joinable() { # phase
    [[ $1 == lobby ]]
}

# Four clients, four X servers, and never `xvfb-run -a`.
#
# `-a`/`--auto-servernum` was this script's display allocator and it is the
# whole of #1003. It picks a number in shell by walking `/tmp/.X<n>-lock`
# (`find_free_servernum` in xvfb-run) -- a check that stakes no claim, so two
# wrappers started in the same instant both see :99 free and both take it.
# Only the first Xvfb starts; the second wrapper waits out its `--wait`, hands
# its client `DISPLAY=:99` anyway, and that client silently attaches to the
# *first* client's server. The two then share one X server, and whichever
# finishes first kills it out from under the other: Xlib prints
# "X connection to :99 broken" and calls `exit(1)`, so a client whose every
# clause PASSED reports a nonzero status. The loser's wrapper also leaves
# `kill: (PID) - No such process` in the log, because the Xvfb it thinks it
# owns never existed. Both preserved failing runs of 2026-09-03 have exactly
# those two lines in `b.log` and neither has a single `PREFLIGHT FAIL`;
# reproduced here, three concurrent `xvfb-run -a` took :99 every time.
#
# xvfb-run's own help calls `-a` deprecated and points at `--auto-display`,
# which asks Xvfb to bind a display and report the number it got
# (`Xvfb -displayfd`). That is allocation and claim in one step, so it cannot
# hand two wrappers the same server. Where the local xvfb-run is too old to
# offer it, fall back to explicit per-side `--server-num`s taken from one
# window of free numbers scanned before any client starts: still a check
# without a claim against *other* users of the box, but no longer a collision
# this script causes with itself.
choose_display_isolation() {
    if "$XVFB_RUN_BIN" --help 2>&1 | grep -Fq -- '--auto-display'; then
        XVFB_DISPLAY_MODE=auto
        printf 'NOTE display-isolation each client gets its own Xvfb via --auto-display\n'
        return 0
    fi
    XVFB_DISPLAY_MODE=numbered
    XVFB_DISPLAY_BASE="$(find_free_display_window 4)"
    printf 'NOTE display-isolation %s has no --auto-display; using --server-num %s..%s\n' \
        "$XVFB_RUN_BIN" "$XVFB_DISPLAY_BASE" "$((XVFB_DISPLAY_BASE + 3))"
}

# The lowest display number with `needed` consecutive free numbers after it.
find_free_display_window() { # how many consecutive displays are needed
    local needed=$1 base=99 offset free
    while ((base < 500)); do
        free=1
        for ((offset = 0; offset < needed; offset++)); do
            if [[ -e /tmp/.X$((base + offset))-lock ]]; then
                free=0
                break
            fi
        done
        ((free)) && break
        base=$((base + 1))
    done
    printf '%s\n' "$base"
}

# The display arguments for one side, as separate words.
xvfb_display_args() { # side
    local offset
    case $1 in
        a) offset=0 ;;
        b) offset=1 ;;
        c) offset=2 ;;
        d) offset=3 ;;
        *) die "internal error: no display slot for side $1" ;;
    esac
    if [[ $XVFB_DISPLAY_MODE == auto ]]; then
        printf '%s\n' --auto-display
    else
        printf '%s\n%s\n' --server-num "$((XVFB_DISPLAY_BASE + offset))"
    fi
}

# Whether this side's client lost its X server rather than failing a clause.
#
# Latching this is the difference between a day spent reading the replication
# path and a line that names the harness: a nonzero client with no
# `PREFLIGHT FAIL` of its own is not a verdict about the campaign, and #1003
# was filed as a replication defect because nothing said so.
client_lost_its_display() { # log
    grep -Eq '^X connection to :[0-9]+ broken' "$1" 2>/dev/null
}

# One client's exit status, and what it is evidence of.
#
# A nonzero status stays a failure -- the criterion is not being softened. What
# is added is attribution, because the status alone was ambiguous in exactly
# the case that mattered: a client whose X server was pulled out from under it
# exits 1 with every clause green, which reads identically to a client that
# failed a clause, and #1003 was diagnosed as a replication defect on that
# basis.
client_exit_clause() { # check, status, log
    local check=$1 status=$2 log=$3 detail=
    if ((status == 0)); then
        result PASS "$check" 'binary exited 0'
        return
    fi
    if client_lost_its_display "$log"; then
        detail="; its X server was torn down under it, so this is a display-isolation \
fault in the harness and not a verdict about the campaign"
    elif ! grep -Fq 'PREFLIGHT FAIL' "$log" 2>/dev/null; then
        detail='; the binary printed no PREFLIGHT FAIL clause of its own, so the cause is \
outside its assertions -- read the end of the log before blaming the campaign'
    fi
    result FAIL "$check" "binary exited $status$detail (log: $log)"
}

# The campaign's phase and taken human seats, from one roster poll.
#
# A taken human seat counts the way admission counts it (and its listing with
# it): `kind: human` with a state other than `empty`, `reserved` included
# because a reservation is a person mid-join. `unreachable` is an answer, not
# a failure -- the fixtures in this script's own self-test have no origin to
# poll, and a real origin this unreachable fails every clause below anyway.
campaign_live_state() { # campaign id, origin
    "$PYTHON_BIN" - "$1" "$2" <<'STATE' 2>/dev/null
import json, sys, urllib.request
campaign, origin = sys.argv[1], sys.argv[2]
try:
    with urllib.request.urlopen(f"{origin}/v1/campaigns/{campaign}/roster", timeout=10) as answer:
        body = json.load(answer)
except Exception:
    print("unreachable")
    raise SystemExit(0)
roster = body.get("roster") or []
humans = sum(1 for seat in roster
             if seat.get("kind") == "human" and seat.get("state") != "empty")
print(f"phase={body.get('phase') or 'unknown'} humans={humans}")
STATE
}

# Refuse to seat this harness where people are, before anything starts (#1008).
#
# These clients take human seats and move no uplink frames, so the attempt
# they are frozen into fails its participation clause and banks nothing, and
# anyone playing in it loses the session. The criterion is right to fail such
# a peer; what is wrong is the harness sitting in its scope, and the exterior
# wire has no way to say "this seat is a harness" without a protocol bump.
# The refusal therefore lives here, at the only place that can choose not to
# go.
#
# One poll is a moment in time and cannot see a human who joins during the
# run. What it buys is that a run against a live campaign is a decision --
# --force-live, named in the refusal -- rather than an accident.
guard_live_campaign() { # campaign id, origin
    local state phase humans
    state="$(campaign_live_state "$1" "$2")"
    if [[ $state == unreachable ]]; then
        printf 'NOTE live-guard %s is not reachable; cannot tell who is on it, so nothing is refused\n' "$2"
        return 0
    fi
    phase=${state%% *}
    phase=${phase#phase=}
    humans=${state##*humans=}
    if ! [[ $phase =~ ^[a-z]+$ && $humans =~ ^[0-9]+$ ]]; then
        die "live-guard: could not read $1's roster state: '$state'"
    fi
    printf 'NOTE live-guard %s is %s with %s human seat(s) taken\n' "$1" "$phase" "$humans"
    if ((humans > 0)); then
        if ((FORCE_LIVE)); then
            printf 'NOTE live-guard forcing past the occupied seats on %s\n' "$1"
            return 0
        fi
        die "live-guard: $1 has $humans human seat(s) taken; these clients take human seats and move no \
uplink frames, so any attempt they are frozen into fails its participation clause and banks nothing, \
ending the session of anyone playing in it (#1008); run with --force-live to proceed deliberately"
    fi
    if [[ $phase == running ]]; then
        if ((FORCE_LIVE)); then
            printf 'NOTE live-guard forcing past the attempt in progress on %s\n' "$1"
            return 0
        fi
        die "live-guard: $1 has an attempt in progress; these clients would be seated into it and, moving \
no uplink frames, fail its participation clause and bank nothing for the hours it ran (#1008); run \
with --force-live to proceed deliberately"
    fi
}

# Wait for a lobby that has just opened, so the whole scenario fits one attempt.
wait_for_fresh_lobby() { # campaign id, origin
    local campaign=$1 origin=$2 state current= waited=0
    while ((waited < FRESH_LOBBY_TIMEOUT_SECS)); do
        state="$(campaign_live_state "$campaign" "$origin")"
        current=${state%% *}
        current=${current#phase=}
        # An origin we cannot poll tells us nothing about the cycle, so waiting
        # buys nothing and costs the whole timeout. The fixtures in this
        # script's own self-test are exactly that case.
        if [[ $current == unreachable ]]; then
            printf 'NOTE fresh-lobby %s is not reachable; not waiting for a lobby\n' "$origin"
            return 0
        fi
        if lobby_is_joinable "$current"; then
            result PASS fresh-lobby "started from a lobby after ${waited}s"
            return 0
        fi
        sleep "$FRESH_LOBBY_POLL_SECS"
        waited=$((waited + FRESH_LOBBY_POLL_SECS))
    done
    # Not fatal, and deliberately not a `result`: every non-PASS verdict there
    # counts as a failure, and an operator running this by hand against an idle
    # campaign should still get a run. The clauses below report what happened.
    printf 'NOTE fresh-lobby no new lobby within %ss; running from phase %s\n' \
        "$FRESH_LOBBY_TIMEOUT_SECS" "$current"
    return 0
}

run_preflight() {
    [[ -n $BINARY ]] || die '--binary is required'
    [[ -n $CAMPAIGN ]] || die '--campaign is required'
    [[ -x $BINARY ]] || die "binary is not executable: $BINARY"
    [[ $TIMEOUT_SECS =~ ^[1-9][0-9]*$ ]] || die '--timeout-secs needs a positive integer'
    [[ $LATE_JOIN_DELAY_SECS =~ ^[0-9]+$ ]] || die '--late-join-delay-secs needs a non-negative integer'
    [[ $REJOIN_DELAY_SECS =~ ^[0-9]+$ ]] || die '--rejoin-delay-secs needs a non-negative integer'
    command -v "$PYTHON_BIN" >/dev/null 2>&1 || die "required command is unavailable: $PYTHON_BIN"
    command -v "$XVFB_RUN_BIN" >/dev/null 2>&1 || die "required command is unavailable: $XVFB_RUN_BIN"
    command -v timeout >/dev/null 2>&1 || die 'required command is unavailable: timeout'

    local dir build_info origin run_suffix nickname_a nickname_b nickname_c nickname_d outer_timeout
    dir="$(mktemp -d)"
    # shellcheck disable=SC2064 # Expand the validated mktemp path now.
    trap "rm -rf '$dir'" EXIT

    build_info="$($BINARY --build-info 2>"$dir/build-info.err")" || {
        result FAIL build-info "binary --build-info failed: $(cat "$dir/build-info.err")"
        summary
        return 1
    }
    origin="$($PYTHON_BIN -c '
import json, sys
value = json.load(sys.stdin)
origin = value.get("admission_url")
if not isinstance(origin, str) or not origin.startswith("https://"):
    raise SystemExit("build-info has no HTTPS admission_url")
print(origin.rstrip("/"))
' <<<"$build_info" 2>"$dir/build-info-parse.err")" || {
        result FAIL build-info-default-origin "$(cat "$dir/build-info-parse.err")"
        summary
        return 1
    }
    result PASS build-info-default-origin "binary reports $origin"

    # The mktemp suffix is unique across concurrent runners and PID namespaces;
    # `$$` is not (sandboxed invocations all commonly run as PID 2). The final
    # suffix gives the self-test fixture and the log reader a stable side.
    run_suffix=${dir##*.}
    nickname_a="preflight-$run_suffix-a"
    nickname_b="preflight-$run_suffix-b"
    nickname_c="preflight-$run_suffix-c"
    nickname_d="preflight-$run_suffix-d"
    outer_timeout=$((TIMEOUT_SECS * 2 + 60))

    run_client() { # side, nickname, expected peers...
        local side=$1 nickname=$2 wrapper_status binary_status peer
        shift 2
        local expected_args=()
        for peer in "$@"; do
            expected_args+=(--expect-peer "$peer")
        done
        local display_args=()
        mapfile -t display_args < <(xvfb_display_args "$side")
        timeout --kill-after=10s "${outer_timeout}s" \
            "$XVFB_RUN_BIN" "${display_args[@]}" \
            env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 \
            WGPU_BACKEND=vulkan bash -c '
                status_file=$1
                shift
                "$@"
                status=$?
                printf "%s\n" "$status" >"$status_file"
                exit "$status"
            ' client-status "$dir/$side.status" "$BINARY" \
                --headless-join "$CAMPAIGN" --nickname "$nickname" \
                "${expected_args[@]}" --campaign-consent \
                --headless-timeout-secs "$TIMEOUT_SECS" \
                --identity-file "$dir/$side.identity" \
                --telemetry-jsonl "$dir/$side.jsonl" >"$dir/$side.log" 2>&1
        wrapper_status=$?

        # xvfb-run can fail while tearing down Xvfb after the client has
        # already exited successfully. The release signal is the binary's
        # status, recorded inside that wrapper. If the binary never started or
        # the outer timeout killed it before it could record a status, retain
        # the wrapper's failure instead.
        if [[ -r $dir/$side.status ]] \
            && read -r binary_status <"$dir/$side.status" \
            && [[ $binary_status =~ ^(0|[1-9][0-9]{0,2})$ ]] \
            && ((binary_status <= 255)); then
            return "$binary_status"
        fi
        return "$wrapper_status"
    }

    guard_live_campaign "$CAMPAIGN" "$origin"
    choose_display_isolation
    wait_for_fresh_lobby "$CAMPAIGN" "$origin"

    run_client a "$nickname_a" "$nickname_b" "$nickname_c" & local pid_a=$!
    run_client b "$nickname_b" "$nickname_a" "$nickname_c" & local pid_b=$!
    sleep "$LATE_JOIN_DELAY_SECS"
    run_client c "$nickname_c" "$nickname_a" "$nickname_b" & local pid_c=$!
    local status_a=0 status_b=0 status_c=0
    wait "$pid_a" || status_a=$?
    wait "$pid_b" || status_b=$?
    wait "$pid_c" || status_c=$?

    # All three binaries have sent their explicit goodbye before wait returns.
    # Give the host's transport-close fallback grace room as well, then prove a
    # fourth process can bind one of the released seats in the same attempt.
    sleep "$REJOIN_DELAY_SECS"
    local status_d=0
    run_client d "$nickname_d" || status_d=$?

    client_exit_clause client-a-exit "$status_a" "$dir/a.log"
    client_exit_clause client-b-exit "$status_b" "$dir/b.log"
    client_exit_clause client-c-exit "$status_c" "$dir/c.log"
    client_exit_clause client-d-rejoin-exit "$status_d" "$dir/d.log"

    require_marker client-a-origin "$dir/a.log" "PREFLIGHT PASS admission-origin origin=$origin"
    require_marker client-b-origin "$dir/b.log" "PREFLIGHT PASS admission-origin origin=$origin"
    require_marker client-c-origin "$dir/c.log" "PREFLIGHT PASS admission-origin origin=$origin"
    require_marker client-d-rejoin-origin "$dir/d.log" "PREFLIGHT PASS admission-origin origin=$origin"
    require_marker client-a-joinable "$dir/a.log" "PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN"
    require_marker client-b-joinable "$dir/b.log" "PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN"
    require_marker client-c-joinable "$dir/c.log" "PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN"
    require_marker client-d-rejoin-joinable "$dir/d.log" "PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN"
    require_marker client-a-admitted "$dir/a.log" "PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN"
    require_marker client-b-admitted "$dir/b.log" "PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN"
    require_marker client-c-admitted "$dir/c.log" "PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN"
    require_marker client-d-rejoin-admitted "$dir/d.log" "PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN"
    require_marker client-a-seated "$dir/a.log" 'PREFLIGHT PASS handshake-seated '
    require_marker client-b-seated "$dir/b.log" 'PREFLIGHT PASS handshake-seated '
    require_marker client-c-seated "$dir/c.log" 'PREFLIGHT PASS handshake-seated '
    require_marker client-d-rejoin-seated "$dir/d.log" 'PREFLIGHT PASS handshake-seated '
    require_marker client-a-peer-b "$dir/a.log" "PREFLIGHT PASS peer-observed nickname=$nickname_b "
    require_marker client-a-peer-c "$dir/a.log" "PREFLIGHT PASS peer-observed nickname=$nickname_c "
    require_marker client-b-peer-a "$dir/b.log" "PREFLIGHT PASS peer-observed nickname=$nickname_a "
    require_marker client-b-peer-c "$dir/b.log" "PREFLIGHT PASS peer-observed nickname=$nickname_c "
    require_marker client-c-peer-a "$dir/c.log" "PREFLIGHT PASS peer-observed nickname=$nickname_a "
    require_marker client-c-peer-b "$dir/c.log" "PREFLIGHT PASS peer-observed nickname=$nickname_b "

    summary
    ((failures == 0))
}

summary() {
    if ((failures == 0)); then
        echo 'SUMMARY PASS client-campaign-preflight failures=0'
    else
        echo "SUMMARY FAIL client-campaign-preflight failures=$failures"
    fi
}

self_test() {
    local dir output status real_python passing=0 mutations=0
    local ST_PYTHON_BIN= pass_count fail_count

    # The gate must start on a lobby that has just opened, not on the word
    # "lobby" -- which is equally true one second before it closes, and that is
    # the difference between running the scenario and running whatever is left
    # of an attempt.
    lobby_is_joinable lobby || die 'self-test: a lobby is where this scenario starts'
    ! lobby_is_joinable running || die 'self-test: an attempt already under way is not a start'
    ! lobby_is_joinable restarting || die 'self-test: a restarting campaign has nothing to join'
    ! lobby_is_joinable full || die 'self-test: a full campaign has no seat for the cohort'
    ! lobby_is_joinable unreachable || die 'self-test: an unpollable origin is not a lobby'
    dir="$(mktemp -d)"
    # shellcheck disable=SC2064 # Expand the validated mktemp path now.
    trap "rm -rf '$dir'" EXIT
    mkdir -p "$dir/bin"

    cat >"$dir/bin/xvfb-run" <<'SH'
#!/usr/bin/env bash
# The fixture answers --help the way the real wrapper does, because that is
# what `choose_display_isolation` reads. The `no-auto-display` arm hides the
# flag so the numbered fallback is exercised too.
if [[ ${1:-} == --help ]]; then
    echo 'Usage: xvfb-run [OPTION ...] COMMAND'
    echo '-a        --auto-servernum          deprecated'
    if [[ ${CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE:-good} != no-auto-display ]]; then
        echo '-d        --auto-display            use the X server to find a display'
    fi
    echo '-n NUM    --server-num=NUM          server number to use (default: 99)'
    exit 0
fi
# Record what display arguments the script chose, and refuse `-a`: taking it
# again is the regression this fixture exists to catch.
display_args=
while (($#)); do
    case "$1" in
        -a|--auto-servernum)
            echo 'fixture-xvfb-run: -a shares one display between concurrent clients' >&2
            exit 6
            ;;
        -d|--auto-display) display_args="$1"; shift ;;
        -n|--server-num) display_args="$1 $2"; shift 2 ;;
        *) break ;;
    esac
done
echo "fixture-xvfb-run display-args=$display_args" >&2
[[ $1 == env ]] && shift
while [[ ${1:-} == -* || ${1:-} == *=* ]]; do
    if [[ $1 == -u ]]; then shift 2; else shift; fi
done
"$@"
status=$?
if [[ ${CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE:-good} == wrapper-cleanup-error ]]; then
    exit 1
fi
exit "$status"
SH
    cat >"$dir/bin/client" <<'SH'
#!/usr/bin/env bash
if [[ ${1:-} == --build-info ]]; then
    echo '{"client_rev":"fixture","ruleset_version":16,"admission_url":"https://fixture.invalid"}'
    exit 0
fi
nickname=
peers=()
campaign=
while (($#)); do
    case "$1" in
        --nickname) shift; nickname=$1 ;;
        --expect-peer) shift; peers+=("$1") ;;
        --headless-join) shift; campaign=$1 ;;
    esac
    shift
done
echo 'PREFLIGHT PASS admission-origin origin=https://fixture.invalid'
echo "PREFLIGHT PASS campaign-joinable campaign=$campaign"
echo "PREFLIGHT PASS admission-accepted campaign=$campaign slot=5"
case ${CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE:-good} in
    not-seated) ;;
    one-seated)
        if [[ $nickname == *-a ]]; then
            echo 'PREFLIGHT PASS handshake-seated slot=5'
        fi
        ;;
    *) echo 'PREFLIGHT PASS handshake-seated slot=5' ;;
esac
# Keep every non-seat assertion green in the seat mutations. The peer arms
# remove directed observations while leaving every other stage green.
case ${CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE:-good} in
    one-peer)
        if [[ $nickname == *-a ]]; then
            for peer in "${peers[@]}"; do
                echo "PREFLIGHT PASS peer-observed nickname=$peer entity=7"
            done
        fi
        ;;
    third-peer-hidden)
        for peer in "${peers[@]}"; do
            [[ $peer == *-c ]] \
                || echo "PREFLIGHT PASS peer-observed nickname=$peer entity=7"
        done
        ;;
    *)
        for peer in "${peers[@]}"; do
            echo "PREFLIGHT PASS peer-observed nickname=$peer entity=7"
        done
        ;;
esac
if [[ ${CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE:-good} == client-error ]]; then
    exit 7
fi
# Every clause green, then the X server disappears: Xlib's own message,
# followed by the exit(1) Xlib takes on a fatal IO error. This is what both
# preserved failing runs of #1003 actually contained.
if [[ ${CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE:-good} == lost-display ]]; then
    echo 'X connection to :99 broken (explicit kill or server shutdown).'
    exit 1
fi
SH
    chmod +x "$dir/bin/xvfb-run" "$dir/bin/client"

    # The live-guard arms poll a campaign that answers, so a fixture python
    # stands in for the interpreter and answers the roster poll by campaign
    # id. Only the poll is the fixture's business: the build-info parse is
    # handed to the real interpreter, captured before the shim can shadow it.
    real_python="$(command -v python3)" \
        || die 'self-test: no python3 for the roster fixture to delegate the build-info parse to'
    cat >"$dir/bin/python3" <<'PYS'
#!/usr/bin/env bash
# `-` is the poll form (`python - campaign origin`); `-c` is not.
if [[ ${1:-} == -c ]]; then
    exec '@REAL_PYTHON@' "$@"
fi
case ${2:-} in
    live-humans) echo 'phase=lobby humans=1' ;;
    live-running) echo 'phase=running humans=0' ;;
    *) echo 'phase=lobby humans=0' ;;
esac
PYS
    sed -i "s|@REAL_PYTHON@|$real_python|" "$dir/bin/python3"
    chmod +x "$dir/bin/python3"

    # The fixture name is also the campaign id: the roster fixture keys its
    # answers on it, and every campaign marker the preflight requires is
    # written from the same id, so nothing else has to know both. Arms that
    # set ST_PYTHON_BIN poll through the roster fixture; the rest keep an
    # origin no poll can reach.
    st_run() {
        local fixture=$1
        shift
        CLIENT_CAMPAIGN_PREFLIGHT_XVFB_RUN="$dir/bin/xvfb-run" \
            CLIENT_CAMPAIGN_PREFLIGHT_PYTHON="${ST_PYTHON_BIN:-python3}" \
            CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE="$fixture" \
            "$0" --binary "$dir/bin/client" --campaign "$fixture" --timeout-secs 1 \
                --late-join-delay-secs 0 --rejoin-delay-secs 0 "$@" 2>&1
    }

    status=0; output="$(st_run good)" || status=$?
    ((status == 0)) || die "self-test baseline failed ($output)"
    grep -Fq 'SUMMARY PASS client-campaign-preflight failures=0' <<<"$output" \
        || die 'self-test baseline emitted no passing summary'
    grep -Fq 'NOTE display-isolation each client gets its own Xvfb via --auto-display' \
        <<<"$output" \
        || die 'self-test baseline did not isolate each client on its own display'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 27 && $fail_count == 0 ]] \
        || die "self-test baseline counted $pass_count pass / $fail_count fail, expected 27 / 0"
    ((passing += 1))

    # An xvfb-run with no --auto-display must still give each client its own
    # server, from a scanned window of numbers -- never the shared `-a`. The
    # fixture wrapper exits 6 on `-a`, so a regression to it fails loudly here
    # rather than becoming a coin-flip against the live campaign.
    status=0; output="$(st_run no-auto-display)" || status=$?
    ((status == 0)) || die "self-test numbered-display baseline failed ($output)"
    grep -Eq '^NOTE display-isolation .* using --server-num [0-9]+\.\.[0-9]+$' <<<"$output" \
        || die 'self-test numbered-display baseline did not fall back to --server-num'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 27 && $fail_count == 0 ]] \
        || die "self-test numbered-display baseline counted $pass_count pass / $fail_count fail, expected 27 / 0"
    ((passing += 1))

    status=0; output="$(st_run wrapper-cleanup-error)" || status=$?
    ((status == 0)) || die "self-test wrapper-cleanup regression failed ($output)"
    grep -Fq 'PASS client-a-exit binary exited 0' <<<"$output" \
        || die 'self-test wrapper-cleanup regression did not retain client-a-exit success'
    grep -Fq 'PASS client-b-exit binary exited 0' <<<"$output" \
        || die 'self-test wrapper-cleanup regression did not retain client-b-exit success'
    grep -Fq 'PASS client-c-exit binary exited 0' <<<"$output" \
        || die 'self-test wrapper-cleanup regression did not retain client-c-exit success'
    grep -Fq 'PASS client-d-rejoin-exit binary exited 0' <<<"$output" \
        || die 'self-test wrapper-cleanup regression did not retain client-d-rejoin-exit success'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 27 && $fail_count == 0 ]] \
        || die "self-test wrapper-cleanup regression counted $pass_count pass / $fail_count fail, expected 27 / 0"
    ((passing += 1))

    status=0; output="$(st_run client-error)" || status=$?
    ((status != 0)) || die 'self-test mutation with three client errors passed'
    grep -Fq 'FAIL client-a-exit binary exited 7' <<<"$output" \
        || die 'self-test client-error mutation did not fail named check client-a-exit'
    grep -Fq 'FAIL client-b-exit binary exited 7' <<<"$output" \
        || die 'self-test client-error mutation did not fail named check client-b-exit'
    grep -Fq 'FAIL client-c-exit binary exited 7' <<<"$output" \
        || die 'self-test client-error mutation did not fail named check client-c-exit'
    grep -Fq 'FAIL client-d-rejoin-exit binary exited 7' <<<"$output" \
        || die 'self-test client-error mutation did not fail named check client-d-rejoin-exit'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 23 && $fail_count == 4 ]] \
        || die "self-test client-error mutation counted $pass_count pass / $fail_count fail, expected 23 / 4"
    ((mutations += 1))

    # A client killed by a vanishing X server still fails -- the criterion is
    # not softened -- but it must say so, because a nonzero client with every
    # clause green and no `PREFLIGHT FAIL` of its own is what #1003 was read as
    # a replication defect on.
    status=0; output="$(st_run lost-display)" || status=$?
    ((status != 0)) || die 'self-test mutation whose clients lost their display passed'
    grep -Fq 'FAIL client-b-exit binary exited 1; its X server was torn down under it' \
        <<<"$output" \
        || die 'self-test lost-display mutation did not attribute the exit to the harness'
    grep -Fq 'PASS client-b-peer-c ' <<<"$output" \
        || die 'self-test lost-display mutation must keep every observation clause green'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 23 && $fail_count == 4 ]] \
        || die "self-test lost-display mutation counted $pass_count pass / $fail_count fail, expected 23 / 4"
    ((mutations += 1))

    status=0; output="$(st_run not-seated)" || status=$?
    ((status != 0)) || die 'self-test mutation with no seated client passed'
    grep -Fq 'FAIL client-a-seated ' <<<"$output" \
        || die 'self-test no-seat mutation did not fail named check client-a-seated'
    grep -Fq 'FAIL client-b-seated ' <<<"$output" \
        || die 'self-test no-seat mutation did not fail named check client-b-seated'
    grep -Fq 'FAIL client-c-seated ' <<<"$output" \
        || die 'self-test no-seat mutation did not fail named check client-c-seated'
    grep -Fq 'FAIL client-d-rejoin-seated ' <<<"$output" \
        || die 'self-test no-seat mutation did not fail named check client-d-rejoin-seated'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 23 && $fail_count == 4 ]] \
        || die "self-test no-seat mutation counted $pass_count pass / $fail_count fail, expected 23 / 4"
    ((mutations += 1))

    status=0; output="$(st_run one-seated)" || status=$?
    ((status != 0)) || die 'self-test mutation with only one seated client passed'
    grep -Fq 'PASS client-a-seated ' <<<"$output" \
        || die 'self-test one-seat mutation did not retain client-a-seated pass'
    grep -Fq 'FAIL client-b-seated ' <<<"$output" \
        || die 'self-test one-seat mutation did not fail named check client-b-seated'
    grep -Fq 'FAIL client-c-seated ' <<<"$output" \
        || die 'self-test one-seat mutation did not fail named check client-c-seated'
    grep -Fq 'FAIL client-d-rejoin-seated ' <<<"$output" \
        || die 'self-test one-seat mutation did not fail named check client-d-rejoin-seated'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 24 && $fail_count == 3 ]] \
        || die "self-test one-seat mutation counted $pass_count pass / $fail_count fail, expected 24 / 3"
    ((mutations += 1))

    status=0; output="$(st_run one-peer)" || status=$?
    ((status != 0)) || die 'self-test mutation with only one observing client passed'
    grep -Fq 'PASS client-a-peer-b ' <<<"$output" \
        || die 'self-test one-peer mutation did not retain client-a-peer-b pass'
    grep -Fq 'PASS client-a-peer-c ' <<<"$output" \
        || die 'self-test one-peer mutation did not retain client-a-peer-c pass'
    grep -Fq 'FAIL client-b-peer-a ' <<<"$output" \
        || die 'self-test one-peer mutation did not fail named check client-b-peer-a'
    grep -Fq 'FAIL client-c-peer-a ' <<<"$output" \
        || die 'self-test one-peer mutation did not fail named check client-c-peer-a'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 23 && $fail_count == 4 ]] \
        || die "self-test one-peer mutation counted $pass_count pass / $fail_count fail, expected 23 / 4"
    ((mutations += 1))

    status=0; output="$(st_run third-peer-hidden)" || status=$?
    ((status != 0)) || die 'self-test mutation hiding the third peer passed'
    grep -Fq 'FAIL client-a-peer-c ' <<<"$output" \
        || die 'self-test third-peer mutation did not fail named check client-a-peer-c'
    grep -Fq 'FAIL client-b-peer-c ' <<<"$output" \
        || die 'self-test third-peer mutation did not fail named check client-b-peer-c'
    grep -Fq 'PASS client-c-peer-a ' <<<"$output" \
        || die 'self-test third-peer mutation did not retain client-c-peer-a pass'
    grep -Fq 'PASS client-c-peer-b ' <<<"$output" \
        || die 'self-test third-peer mutation did not retain client-c-peer-b pass'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 25 && $fail_count == 2 ]] \
        || die "self-test third-peer mutation counted $pass_count pass / $fail_count fail, expected 25 / 2"
    ((mutations += 1))

    # The live guard refuses a campaign with a human seat taken before any
    # client starts, reports what it saw, and names the flag that overrides it.
    ST_PYTHON_BIN=$dir/bin/python3
    status=0; output="$(st_run live-humans)" || status=$?
    ((status != 0)) || die 'self-test: a campaign with a human seat taken was not refused'
    grep -Fq 'NOTE live-guard live-humans is lobby with 1 human seat(s) taken' <<<"$output" \
        || die 'self-test humans-present refusal did not report what it saw on the campaign'
    grep -Fq 'live-guard: live-humans has 1 human seat(s) taken' <<<"$output" \
        || die 'self-test humans-present refusal did not refuse by name'
    grep -Fq -- '--force-live' <<<"$output" \
        || die 'self-test humans-present refusal did not name the override flag'
    ! grep -Fq 'client-a-exit' <<<"$output" \
        || die 'self-test humans-present refusal started clients anyway'
    ((mutations += 1))

    # The same for an attempt already in progress on an otherwise empty
    # campaign: nobody to displace, still refused, because the attempt is what
    # banks nothing.
    status=0; output="$(st_run live-running)" || status=$?
    ((status != 0)) || die 'self-test: a campaign with an attempt in progress was not refused'
    grep -Fq 'NOTE live-guard live-running is running with 0 human seat(s) taken' <<<"$output" \
        || die 'self-test attempt-in-progress refusal did not report what it saw on the campaign'
    grep -Fq 'live-guard: live-running has an attempt in progress' <<<"$output" \
        || die 'self-test attempt-in-progress refusal did not refuse by name'
    ! grep -Fq 'client-a-exit' <<<"$output" \
        || die 'self-test attempt-in-progress refusal started clients anyway'
    ((mutations += 1))

    # --force-live is the deliberate run: the occupied campaign is announced
    # and the whole scenario still proves what it proves, lobby included --
    # the answered roster is one pass more than the unreachable arms count.
    status=0; output="$(st_run live-humans --force-live)" || status=$?
    ((status == 0)) || die "self-test forced run against an occupied campaign failed ($output)"
    grep -Fq 'NOTE live-guard forcing past the occupied seats on live-humans' <<<"$output" \
        || die 'self-test forced run did not announce what it was forcing past'
    grep -Fq 'PASS fresh-lobby started from a lobby after 0s' <<<"$output" \
        || die 'self-test forced run did not start from the answered lobby'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 28 && $fail_count == 0 ]] \
        || die "self-test forced run counted $pass_count pass / $fail_count fail, expected 28 / 0"
    ((passing += 1))

    # A live campaign with nobody on it is not refused: the guard reads the
    # roster, announces it, and stays out of the way.
    status=0; output="$(st_run live-empty)" || status=$?
    ((status == 0)) || die "self-test run against an empty live campaign failed ($output)"
    grep -Fq 'NOTE live-guard live-empty is lobby with 0 human seat(s) taken' <<<"$output" \
        || die 'self-test empty-live run was not announced by the guard'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 28 && $fail_count == 0 ]] \
        || die "self-test empty-live run counted $pass_count pass / $fail_count fail, expected 28 / 0"
    ((passing += 1))
    ST_PYTHON_BIN=

    echo "$NAME: self-test passed ($passing baselines: ordinary (--auto-display) + numbered-display fallback + wrapper-cleanup each 27 pass / 0 fail, forced-live + empty-live each 28 pass / 0 fail through the live guard; client-error mutation 23 pass / 4 fail at client-a-exit + client-b-exit + client-c-exit + client-d-rejoin-exit; $mutations total mutations: lost-display 23 pass / 4 fail at every exit check with the fault attributed to the harness, no-seat 23 pass / 4 fail at all seated checks, one-seat 24 pass / 3 fail at client-b-seated + client-c-seated + client-d-rejoin-seated, one-peer 23 pass / 4 fail at every B/C observation, third-peer-hidden 25 pass / 2 fail at client-a-peer-c + client-b-peer-c, live-guard humans-present refused before any client ran and attempt-in-progress refused)"
}

while (($#)); do
    case "$1" in
        --binary) shift; (($#)) || die '--binary needs a path'; BINARY=$1 ;;
        --campaign) shift; (($#)) || die '--campaign needs an id'; CAMPAIGN=$1 ;;
        --timeout-secs) shift; (($#)) || die '--timeout-secs needs a value'; TIMEOUT_SECS=$1 ;;
        --late-join-delay-secs) shift; (($#)) || die '--late-join-delay-secs needs a value'; LATE_JOIN_DELAY_SECS=$1 ;;
        --rejoin-delay-secs) shift; (($#)) || die '--rejoin-delay-secs needs a value'; REJOIN_DELAY_SECS=$1 ;;
        --force-live) FORCE_LIVE=1 ;;
        --self-test) self_test; exit $? ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown argument '$1'" ;;
    esac
    shift
done

run_preflight
