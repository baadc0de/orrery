#!/usr/bin/env bash
# Prove that one built Regolith binary can join the deployed campaign three
# times and that each seated client receives both other replicated crafts
# (#587, #601).
#
#   scripts/client-campaign-preflight.sh --binary PATH --campaign ID
#   scripts/client-campaign-preflight.sh --self-test
#
# The binary is the probe. Its --build-info supplies the baked origin, and the
# three launches receive no --admission-url override. Their own headless join
# mode fetches the listing, applies the shipped joinability predicate, asks
# admission for a seat, completes the iroh handshake, and binds the requested
# nickname to a craft that arrived through replication. HTTP substitutes such
# as curl are deliberately absent.
set -uo pipefail

readonly NAME=client-campaign-preflight

BINARY=
CAMPAIGN=
TIMEOUT_SECS=1020
XVFB_RUN_BIN="${CLIENT_CAMPAIGN_PREFLIGHT_XVFB_RUN:-xvfb-run}"
PYTHON_BIN="${CLIENT_CAMPAIGN_PREFLIGHT_PYTHON:-python3}"
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

run_preflight() {
    [[ -n $BINARY ]] || die '--binary is required'
    [[ -n $CAMPAIGN ]] || die '--campaign is required'
    [[ -x $BINARY ]] || die "binary is not executable: $BINARY"
    [[ $TIMEOUT_SECS =~ ^[1-9][0-9]*$ ]] || die '--timeout-secs needs a positive integer'
    command -v "$PYTHON_BIN" >/dev/null 2>&1 || die "required command is unavailable: $PYTHON_BIN"
    command -v "$XVFB_RUN_BIN" >/dev/null 2>&1 || die "required command is unavailable: $XVFB_RUN_BIN"
    command -v timeout >/dev/null 2>&1 || die 'required command is unavailable: timeout'

    local dir build_info origin run_suffix nickname_a nickname_b nickname_c outer_timeout
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
    outer_timeout=$((TIMEOUT_SECS * 2 + 60))

    run_client() { # side, nickname, expected peers...
        local side=$1 nickname=$2 wrapper_status binary_status peer
        shift 2
        local expected_args=()
        for peer in "$@"; do
            expected_args+=(--expect-peer "$peer")
        done
        timeout --kill-after=10s "${outer_timeout}s" \
            "$XVFB_RUN_BIN" -a env -u WAYLAND_DISPLAY WINIT_UNIX_BACKEND=x11 \
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

    run_client a "$nickname_a" "$nickname_b" "$nickname_c" & local pid_a=$!
    run_client b "$nickname_b" "$nickname_a" "$nickname_c" & local pid_b=$!
    run_client c "$nickname_c" "$nickname_a" "$nickname_b" & local pid_c=$!
    local status_a=0 status_b=0 status_c=0
    wait "$pid_a" || status_a=$?
    wait "$pid_b" || status_b=$?
    wait "$pid_c" || status_c=$?

    if ((status_a == 0)); then result PASS client-a-exit 'binary exited 0';
    else result FAIL client-a-exit "binary exited $status_a (log: $dir/a.log)"; fi
    if ((status_b == 0)); then result PASS client-b-exit 'binary exited 0';
    else result FAIL client-b-exit "binary exited $status_b (log: $dir/b.log)"; fi
    if ((status_c == 0)); then result PASS client-c-exit 'binary exited 0';
    else result FAIL client-c-exit "binary exited $status_c (log: $dir/c.log)"; fi

    require_marker client-a-origin "$dir/a.log" "PREFLIGHT PASS admission-origin origin=$origin"
    require_marker client-b-origin "$dir/b.log" "PREFLIGHT PASS admission-origin origin=$origin"
    require_marker client-c-origin "$dir/c.log" "PREFLIGHT PASS admission-origin origin=$origin"
    require_marker client-a-joinable "$dir/a.log" "PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN"
    require_marker client-b-joinable "$dir/b.log" "PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN"
    require_marker client-c-joinable "$dir/c.log" "PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN"
    require_marker client-a-admitted "$dir/a.log" "PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN"
    require_marker client-b-admitted "$dir/b.log" "PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN"
    require_marker client-c-admitted "$dir/c.log" "PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN"
    require_marker client-a-seated "$dir/a.log" 'PREFLIGHT PASS handshake-seated '
    require_marker client-b-seated "$dir/b.log" 'PREFLIGHT PASS handshake-seated '
    require_marker client-c-seated "$dir/c.log" 'PREFLIGHT PASS handshake-seated '
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
    local dir output status passing=0 mutations=0 pass_count fail_count
    dir="$(mktemp -d)"
    # shellcheck disable=SC2064 # Expand the validated mktemp path now.
    trap "rm -rf '$dir'" EXIT
    mkdir -p "$dir/bin"

    cat >"$dir/bin/xvfb-run" <<'SH'
#!/usr/bin/env bash
shift # -a
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
SH
    chmod +x "$dir/bin/xvfb-run" "$dir/bin/client"

    st_run() {
        CLIENT_CAMPAIGN_PREFLIGHT_XVFB_RUN="$dir/bin/xvfb-run" \
            CLIENT_CAMPAIGN_PREFLIGHT_FIXTURE="$1" \
            "$0" --binary "$dir/bin/client" --campaign fixture --timeout-secs 1 2>&1
    }

    status=0; output="$(st_run good)" || status=$?
    ((status == 0)) || die "self-test baseline failed ($output)"
    grep -Fq 'SUMMARY PASS client-campaign-preflight failures=0' <<<"$output" \
        || die 'self-test baseline emitted no passing summary'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 22 && $fail_count == 0 ]] \
        || die "self-test baseline counted $pass_count pass / $fail_count fail, expected 22 / 0"
    ((passing += 1))

    status=0; output="$(st_run wrapper-cleanup-error)" || status=$?
    ((status == 0)) || die "self-test wrapper-cleanup regression failed ($output)"
    grep -Fq 'PASS client-a-exit binary exited 0' <<<"$output" \
        || die 'self-test wrapper-cleanup regression did not retain client-a-exit success'
    grep -Fq 'PASS client-b-exit binary exited 0' <<<"$output" \
        || die 'self-test wrapper-cleanup regression did not retain client-b-exit success'
    grep -Fq 'PASS client-c-exit binary exited 0' <<<"$output" \
        || die 'self-test wrapper-cleanup regression did not retain client-c-exit success'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 22 && $fail_count == 0 ]] \
        || die "self-test wrapper-cleanup regression counted $pass_count pass / $fail_count fail, expected 22 / 0"
    ((passing += 1))

    status=0; output="$(st_run client-error)" || status=$?
    ((status != 0)) || die 'self-test mutation with three client errors passed'
    grep -Fq 'FAIL client-a-exit binary exited 7' <<<"$output" \
        || die 'self-test client-error mutation did not fail named check client-a-exit'
    grep -Fq 'FAIL client-b-exit binary exited 7' <<<"$output" \
        || die 'self-test client-error mutation did not fail named check client-b-exit'
    grep -Fq 'FAIL client-c-exit binary exited 7' <<<"$output" \
        || die 'self-test client-error mutation did not fail named check client-c-exit'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 19 && $fail_count == 3 ]] \
        || die "self-test client-error mutation counted $pass_count pass / $fail_count fail, expected 19 / 3"
    ((mutations += 1))

    status=0; output="$(st_run not-seated)" || status=$?
    ((status != 0)) || die 'self-test mutation with no seated client passed'
    grep -Fq 'FAIL client-a-seated ' <<<"$output" \
        || die 'self-test no-seat mutation did not fail named check client-a-seated'
    grep -Fq 'FAIL client-b-seated ' <<<"$output" \
        || die 'self-test no-seat mutation did not fail named check client-b-seated'
    grep -Fq 'FAIL client-c-seated ' <<<"$output" \
        || die 'self-test no-seat mutation did not fail named check client-c-seated'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 19 && $fail_count == 3 ]] \
        || die "self-test no-seat mutation counted $pass_count pass / $fail_count fail, expected 19 / 3"
    ((mutations += 1))

    status=0; output="$(st_run one-seated)" || status=$?
    ((status != 0)) || die 'self-test mutation with only one seated client passed'
    grep -Fq 'PASS client-a-seated ' <<<"$output" \
        || die 'self-test one-seat mutation did not retain client-a-seated pass'
    grep -Fq 'FAIL client-b-seated ' <<<"$output" \
        || die 'self-test one-seat mutation did not fail named check client-b-seated'
    grep -Fq 'FAIL client-c-seated ' <<<"$output" \
        || die 'self-test one-seat mutation did not fail named check client-c-seated'
    pass_count=$(grep -c '^PASS ' <<<"$output" || true)
    fail_count=$(grep -c '^FAIL ' <<<"$output" || true)
    [[ $pass_count == 20 && $fail_count == 2 ]] \
        || die "self-test one-seat mutation counted $pass_count pass / $fail_count fail, expected 20 / 2"
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
    [[ $pass_count == 18 && $fail_count == 4 ]] \
        || die "self-test one-peer mutation counted $pass_count pass / $fail_count fail, expected 18 / 4"
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
    [[ $pass_count == 20 && $fail_count == 2 ]] \
        || die "self-test third-peer mutation counted $pass_count pass / $fail_count fail, expected 20 / 2"
    ((mutations += 1))

    echo "$NAME: self-test passed ($passing baselines: ordinary + wrapper-cleanup each 22 pass / 0 fail; client-error mutation 19 pass / 3 fail at client-a-exit + client-b-exit + client-c-exit; $mutations total mutations: no-seat 19 pass / 3 fail at all seated checks, one-seat 20 pass / 2 fail at client-b-seated + client-c-seated, one-peer 18 pass / 4 fail at every B/C observation, third-peer-hidden 20 pass / 2 fail at client-a-peer-c + client-b-peer-c)"
}

while (($#)); do
    case "$1" in
        --binary) shift; (($#)) || die '--binary needs a path'; BINARY=$1 ;;
        --campaign) shift; (($#)) || die '--campaign needs an id'; CAMPAIGN=$1 ;;
        --timeout-secs) shift; (($#)) || die '--timeout-secs needs a value'; TIMEOUT_SECS=$1 ;;
        --self-test) self_test; exit $? ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown argument '$1'" ;;
    esac
    shift
done

run_preflight
