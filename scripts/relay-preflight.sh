#!/usr/bin/env bash
# Read-only preflight/postflight evidence for the iroh-relay nginx migration.
#
# Run this *on the relay host*.  It never opens an SSH connection, writes a
# remote configuration file, restarts a service, or changes firewall state.
# `--output` writes only the local evidence file requested by the operator.
#
# The name is deliberately relay-preflight even though it has a postflight
# mode: one instrument records the before state and judges the after state, so
# the two snapshots use the same probes rather than two checklists drifting.
#
# Usage:
#   relay-preflight.sh --preflight [--output before.txt]
#   relay-preflight.sh --postflight [--output after.txt]
#   relay-preflight.sh --self-test
#
# Defaults are the operator-supplied endpoints in
# docs/plans/campaign-admission-service.md §10.1.  Override them when
# rehearsing elsewhere; they are not independently discoverable from this
# checkout.
set -uo pipefail

readonly NAME=relay-preflight
readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PUBLIC_IP="${RELAY_PREFLIGHT_PUBLIC_IP:-62.238.59.131}"
RELAY_HOST="${RELAY_PREFLIGHT_RELAY_HOST:-iroh-relay.distopik.com}"
CAMPAIGNS_HOST="${RELAY_PREFLIGHT_CAMPAIGNS_HOST:-campaigns.distopik.com}"
RELAY_HTTPS_PORT=""
OUTPUT=""
MODE=""
SS_BIN="${RELAY_PREFLIGHT_SS_BIN:-ss}"
OPENSSL_BIN="${RELAY_PREFLIGHT_OPENSSL_BIN:-openssl}"
CURL_BIN="${RELAY_PREFLIGHT_CURL_BIN:-curl}"

failures=0
unknowns=0

usage() {
    sed -n '2,/^set -uo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -uo/d' >&2
}

result() { # name verdict detail...
    local name=$1 verdict=$2
    shift 2
    printf '%s %s %s\n' "$verdict" "$name" "$*"
    case "$verdict" in
        FAIL) ((failures += 1)) ;;
        UNKNOWN) ((unknowns += 1)) ;;
    esac
}

require_tool() { # binary, check name
    local binary=$1 check=$2
    if ! command -v "$binary" >/dev/null 2>&1; then
        result "$check" UNKNOWN "required command '$binary' is unavailable"
        return 1
    fi
    return 0
}

listen_snapshot() {
    local port=$1 proto=$2 output status=0
    if ! require_tool "$SS_BIN" "listeners-$port-$proto"; then return; fi
    output="$("$SS_BIN" -"l$proto" -n -p "sport = $port" 2>&1)" || status=$?
    if ((status != 0)); then
        result "listeners-$port-$proto" UNKNOWN "ss could not inspect port $port ($output)"
    elif [[ -z $output ]]; then
        # This is evidence, not an expectation.  In particular, the normal
        # relay shape has TCP 80/443 and UDP 7842, so absence on the opposite
        # protocol is useful to record and must not make preflight fail.
        result "listeners-$port-$proto" OBSERVED "no $proto listener reported on port $port"
    else
        # Keep the actual listener rows in the evidence; the verdict alone is
        # not enough to compare before and after addresses.
        result "listeners-$port-$proto" OBSERVED "$(tr '\n' ';' <<<"$output")"
    fi
}

certificate_snapshot() { # check, connect host:port, SNI
    local check=$1 connect=$2 sni=$3 pem status=0 summary
    if ! require_tool "$OPENSSL_BIN" "$check"; then return; fi
    pem="$("$OPENSSL_BIN" s_client -connect "$connect" -servername "$sni" </dev/null 2>/dev/null)" || status=$?
    if ((status != 0)) || ! grep -q -- 'BEGIN CERTIFICATE' <<<"$pem"; then
        result "$check" UNKNOWN "could not obtain the served certificate from $connect (sni $sni)"
        return
    fi
    summary="$(printf '%s\n' "$pem" | "$OPENSSL_BIN" x509 -noout -subject -issuer -serial -enddate 2>&1)" || status=$?
    if ((status != 0)); then
        result "$check" UNKNOWN "openssl could not parse the served certificate from $connect ($summary)"
    else
        result "$check" PASS "${summary//$'\n'/; }"
    fi
}

cert_expiry_watch() { # named check, connect host:port, SNI
    local name=$1 connect=$2 sni=$3 pem status=0
    if ! require_tool "$OPENSSL_BIN" "$name"; then return; fi
    pem="$("$OPENSSL_BIN" s_client -connect "$connect" -servername "$sni" </dev/null 2>/dev/null)" || status=$?
    if ((status != 0)) || ! grep -q -- 'BEGIN CERTIFICATE' <<<"$pem"; then
        result "$name" UNKNOWN "could not obtain the served certificate from $connect (sni $sni)"
        return
    fi

    # openssl x509 -checkend exits 0 only when the supplied certificate remains
    # valid for the complete interval; 1 means it expires sooner.  The PEM is
    # the certificate just served by s_client, not certbot's file on disk.
    if printf '%s\n' "$pem" | "$OPENSSL_BIN" x509 -noout -checkend 1209600 >/dev/null 2>&1; then
        result "$name" PASS "served certificate at $connect (sni $sni) remains valid for at least 14 days"
    else
        status=$?
        if ((status == 1)); then
            result "$name" FAIL "served certificate at $connect (sni $sni) expires within 14 days"
        else
            result "$name" UNKNOWN "openssl could not check the served certificate from $connect"
        fi
    fi
}

relay_answers() {
    local connect_ip=$1 port=$2 status=0 output
    if ! require_tool "$CURL_BIN" relay-answers; then return; fi
    output="$("$CURL_BIN" --silent --show-error --fail --insecure --max-time 10 \
        --resolve "$RELAY_HOST:$port:$connect_ip" "https://$RELAY_HOST:$port/" 2>&1)" || status=$?
    if ((status == 0)); then
        result relay-answers PASS "relay answered HTTPS at $connect_ip:$port"
    else
        result relay-answers UNKNOWN "relay did not answer HTTPS at $connect_ip:$port ($output)"
    fi
}

qad_listens_publicly() {
    local output status=0
    if ! require_tool "$SS_BIN" qad-listens-publicly; then return; fi
    # This is deliberately the plan's command shape.  Do not replace it with a
    # service-status check: a cleanly started relay can still bind QAD to
    # loopback after https_bind_addr moved there.
    output="$("$SS_BIN" -ulpn 'sport = 7842' 2>&1)" || status=$?
    if ((status != 0)); then
        result qad-listens-publicly UNKNOWN "ss could not inspect UDP 7842 ($output)"
    elif grep -Fq -- "$PUBLIC_IP:7842" <<<"$output"; then
        result qad-listens-publicly PASS "QAD listens on $PUBLIC_IP:7842"
    elif grep -Eq '127\.0\.0\.1:\[?7842\]?' <<<"$output"; then
        result qad-listens-publicly FAIL "QAD is bound to loopback, not $PUBLIC_IP:7842"
    elif [[ -z $output ]]; then
        result qad-listens-publicly FAIL "no UDP listener reported on port 7842"
    else
        result qad-listens-publicly FAIL "UDP 7842 is not bound to $PUBLIC_IP:7842 ($(tr '\n' ';' <<<"$output"))"
    fi
}

run_checks() {
    local public_relay_port
    case "$MODE" in
        preflight) RELAY_HTTPS_PORT="${RELAY_PREFLIGHT_PRE_RELAY_PORT:-443}" ;;
        postflight) RELAY_HTTPS_PORT="${RELAY_PREFLIGHT_POST_RELAY_PORT:-8543}" ;;
    esac
    public_relay_port=443

    printf 'MODE %s public_ip=%s relay_host=%s campaigns_host=%s\n' \
        "$MODE" "$PUBLIC_IP" "$RELAY_HOST" "$CAMPAIGNS_HOST"
    listen_snapshot 80 t
    listen_snapshot 80 u
    listen_snapshot 443 t
    listen_snapshot 443 u
    listen_snapshot 7842 t
    listen_snapshot 7842 u
    certificate_snapshot "served-cert-relay-private" "127.0.0.1:$RELAY_HTTPS_PORT" "$RELAY_HOST"
    certificate_snapshot "served-cert-relay-public" "$PUBLIC_IP:$public_relay_port" "$RELAY_HOST"
    relay_answers "127.0.0.1" "$RELAY_HTTPS_PORT"

    if [[ $MODE == postflight ]]; then
        certificate_snapshot "served-cert-campaigns-public" "$PUBLIC_IP:443" "$CAMPAIGNS_HOST"
        qad_listens_publicly
        cert_expiry_watch "cert-expiry-watch:relay-private" "127.0.0.1:$RELAY_HTTPS_PORT" "$RELAY_HOST"
        cert_expiry_watch "cert-expiry-watch:relay-public" "$PUBLIC_IP:443" "$RELAY_HOST"
        cert_expiry_watch "cert-expiry-watch:campaigns-public" "$PUBLIC_IP:443" "$CAMPAIGNS_HOST"
    fi

    if ((failures || unknowns)); then
        printf 'SUMMARY FAIL failures=%d unknown=%d\n' "$failures" "$unknowns"
        return 1
    fi
    printf 'SUMMARY PASS failures=0 unknown=0\n'
}

self_test() {
    local dir output status=0
    dir="$(mktemp -d)"
    # shellcheck disable=SC2064 # Expand now: this exact temporary path is safe.
    trap "rm -rf '$dir'" EXIT

    # The fixtures emulate only the read-only tools this script calls.  They
    # let the guarded outcomes be tested without a listener, certificate, or
    # network connection.
    mkdir "$dir/bin"
    apply_fixture() {
        local name=$1 body=$2
        printf '%s\n' "$body" > "$dir/bin/$name"
        chmod +x "$dir/bin/$name"
    }
    apply_fixture ss '#!/usr/bin/env bash
case "$RELAY_PREFLIGHT_FIXTURE" in
  loopback) printf "udp UNCONN 0 0 127.0.0.1:7842 0.0.0.0:*\\n" ;;
  empty) : ;;
  erroring) echo "ss: permission denied" >&2; exit 1 ;;
  *) printf "udp UNCONN 0 0 62.238.59.131:7842 0.0.0.0:*\\n" ;;
esac'
    apply_fixture openssl '#!/usr/bin/env bash
if [[ $1 == s_client ]]; then echo "-----BEGIN CERTIFICATE-----"; echo fixture; echo "-----END CERTIFICATE-----"; exit 0; fi
if [[ $1 == x509 && " $* " == *" -checkend "* ]]; then [[ ${RELAY_PREFLIGHT_FIXTURE:-good} == expiring ]] && exit 1; exit 0; fi
echo "subject=CN = fixture"; echo "issuer=CN = fixture"; echo "serial=01"; echo "notAfter=Jan 01 00:00:00 2099 GMT"'
    apply_fixture curl '#!/usr/bin/env bash
[[ ${RELAY_PREFLIGHT_FIXTURE:-good} == unreachable ]] && exit 7
echo relay'

    st_run() {
        PATH="$dir/bin:$PATH" RELAY_PREFLIGHT_SS_BIN=ss RELAY_PREFLIGHT_OPENSSL_BIN=openssl \
            RELAY_PREFLIGHT_CURL_BIN=curl RELAY_PREFLIGHT_FIXTURE="$1" \
            "$0" --postflight 2>&1
    }

    st_good() {
        status=0; output="$(st_run good)" || status=$?
        ((status == 0)) || die "self-test: restored passing fixtures returned $status ($output)"
        grep -Fq 'PASS qad-listens-publicly ' <<<"$output" \
            || die 'self-test: public QAD fixture did not pass qad-listens-publicly'
        grep -Fq 'PASS cert-expiry-watch:relay-private ' <<<"$output" \
            || die 'self-test: valid served certificate did not pass cert-expiry-watch'
    }

    # Establish passing state, then restore it after every guarded mutation.
    # A test that mutates one fixture and moves on has not proved the intended
    # recovery path actually returns to PASS.
    st_good

    status=0; output="$(st_run loopback)" || status=$?
    ((status != 0)) || die 'self-test: loopback QAD fixture passed'
    grep -Fq 'FAIL qad-listens-publicly QAD is bound to loopback' <<<"$output" \
        || die 'self-test: loopback QAD did not fail the named qad-listens-publicly check'
    st_good

    status=0; output="$(st_run expiring)" || status=$?
    ((status != 0)) || die 'self-test: expiring certificate fixture passed'
    grep -Fq 'FAIL cert-expiry-watch:relay-private ' <<<"$output" \
        || die 'self-test: expiring certificate did not fail cert-expiry-watch'
    st_good

    # A present-but-failing probe is a different branch from a missing binary:
    # `ss` exists, runs, and exits non-zero (permission denied, an unexpected
    # kernel). Downgrading that branch to PASS was invisible to this self-test
    # until this case existed, so the operator would have been told the relay
    # was fine by a check that had learned nothing.
    status=0; output="$(st_run erroring)" || status=$?
    ((status != 0)) || die 'self-test: an erroring ss fixture passed'
    grep -Fq 'UNKNOWN qad-listens-publicly ss could not inspect UDP 7842' <<<"$output" \
        || die 'self-test: an erroring probe was not reported UNKNOWN by qad-listens-publicly'
    st_good

    status=0
    output="$(PATH="$dir/bin:$PATH" RELAY_PREFLIGHT_SS_BIN=missing-ss \
        RELAY_PREFLIGHT_OPENSSL_BIN=openssl RELAY_PREFLIGHT_CURL_BIN=curl "$0" --postflight 2>&1)" || status=$?
    ((status != 0)) || die 'self-test: unavailable ss fixture passed'
    grep -Fq "UNKNOWN qad-listens-publicly required command 'missing-ss' is unavailable" <<<"$output" \
        || die 'self-test: unavailable probe was not reported UNKNOWN by qad-listens-publicly'
    st_good

    echo "$NAME: self-test passed (5 passing fixtures: baseline + 4 reversions; 4 guarded mutations: loopback FAIL, expiring FAIL, erroring UNKNOWN, unavailable UNKNOWN)"
}

die() { echo "$NAME: $*" >&2; exit 2; }

while (($#)); do
    case "$1" in
        --preflight) MODE=preflight ;;
        --postflight) MODE=postflight ;;
        --output) shift; (($#)) || die '--output needs a path'; OUTPUT=$1 ;;
        --self-test) self_test; exit $? ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown argument '$1'" ;;
    esac
    shift
done

[[ -n $MODE ]] || { usage; die 'choose --preflight or --postflight'; }
if [[ -n $OUTPUT ]]; then
    run_checks | tee "$OUTPUT"
    exit "${PIPESTATUS[0]}"
fi
run_checks
