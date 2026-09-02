#!/usr/bin/env bash
# Exercise the archive that will be published, rather than the build it came
# from (#774).
#
#   scripts/package-artifact-smoke.sh --archive PATH --label LABEL [options]
#   scripts/package-artifact-smoke.sh --self-test
#
# Options:
#   --archive PATH          the .tar.gz or .zip about to be uploaded
#   --label LABEL           x86_64-linux | x86_64-windows | aarch64-macos
#   --campaign ID           also join that campaign with the extracted binary
#   --join-timeout-secs N   bound for --campaign (default 600)
#   --keep-extraction DIR   extract here and leave it, so a later step can run
#                           against the extracted binary instead of target/
#
# Every defect the first Windows volunteer hit -- an extensionless binary under
# an internal `stage/` folder (#768), a join artifact written CWD-relative into
# `target/` so a read-only launch folder denied it (#766), a telemetry open that
# panicked before any UI existed (#772) -- is invisible to a build and obvious
# to an extraction. `package-client.yml` verified what it compiled: it launched
# `./target/release/<binary>` from the checkout, and the staged copy's only
# exercise was `--build-info` through Git-Bash, which tolerates a missing `.exe`
# because MSYS does. So this script never looks at `target/`. It takes the
# archive, extracts it into a fresh directory, and does what the shipped
# README tells a volunteer to do.
#
# The four stages, and which defect each one is for:
#
#   manifest      the extracted names are exactly the four shipped names, flat,
#                 with the platform's extension, and the README inside the
#                 archive names both the binary and the archive (#768). This is
#                 an *extraction*, not a listing: the archive's table of
#                 contents and what lands on disk are two different claims.
#   launch        the extracted binary runs from the extraction directory, and
#                 writes its artifacts into the per-user application data
#                 directory rather than into a `target/` beside itself (#766).
#   read-only     the same launch, rendered, from a directory the process
#                 genuinely cannot write. It must still reach a usable state,
#                 must not report its recording unavailable, and must still
#                 have written where it is allowed to (#766, #772).
#   join          optionally, that this platform's extracted binary can enter
#                 the deployed campaign at all -- the check that was Linux-only,
#                 which is why a Windows join defect reached a volunteer (#769).
#
# The read-only stage probes the directory before trusting it. A run that can
# still write there cannot observe the property this stage exists for, and says
# so as a failure rather than reporting green: mode bits do not restrain uid 0,
# and neither does a Windows ACL nobody applied.
set -uo pipefail

readonly NAME=package-artifact-smoke

ARCHIVE=
LABEL=
CAMPAIGN=
JOIN_TIMEOUT_SECS=600
KEEP_EXTRACTION=
XVFB_RUN_BIN="${PACKAGE_ARTIFACT_SMOKE_XVFB_RUN:-xvfb-run}"
failures=0

usage() {
    sed -n '2,/^set -uo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -uo/d' >&2
}

die() { echo "$NAME: $*" >&2; exit 2; }

result() {
    local verdict=$1 check=$2
    shift 2
    printf '%s %s %s\n' "$verdict" "$check" "$*"
    [[ $verdict == PASS ]] || failures=$((failures + 1))
}

summary() {
    if ((failures == 0)); then
        echo "SUMMARY PASS $NAME failures=0"
    else
        echo "SUMMARY FAIL $NAME failures=$failures"
    fi
}

# ── What each label ships ───────────────────────────────────────────────────
#
# Stated literally rather than derived from the archive, and that is the whole
# point: #768 shipped because the packaging step's assertions were computed
# from the same expression that produced the name, so when the name lost its
# extension the assertion lost it too and stayed green. These four lines are a
# second source. They are also the names `clients/regolith/PLAYTEST.md` puts in
# front of a volunteer, which the README clauses below hold the archive to.
asset_for_label() { # label
    case "$1" in
        x86_64-linux) echo 'orrery-regolith-x86_64-linux' ;;
        x86_64-windows) echo 'orrery-regolith-x86_64-windows.exe' ;;
        aarch64-macos) echo 'orrery-regolith-aarch64-macos' ;;
        *) return 1 ;;
    esac
}

archive_name_for_label() { # label
    case "$1" in
        x86_64-linux) echo 'orrery-regolith-x86_64-linux.tar.gz' ;;
        x86_64-windows) echo 'orrery-regolith-x86_64-windows.zip' ;;
        aarch64-macos) echo 'orrery-regolith-aarch64-macos.tar.gz' ;;
        *) return 1 ;;
    esac
}

# The per-user application data directory the shipped binary resolves to, given
# a root that every relevant environment variable has been pointed at. Mirrors
# `clients/regolith/src/paths.rs`, per the *target* platform rather than the
# host: a Windows archive resolves the Windows convention wherever this script
# happens to run.
data_dir_for_label() { # label, root
    case "$1" in
        x86_64-windows) echo "$2/Orrery/Regolith" ;;
        aarch64-macos) echo "$2/Library/Application Support/Orrery/Regolith" ;;
        *) echo "$2/orrery/regolith" ;;
    esac
}

host_platform() {
    case "$(uname -s)" in
        Linux) echo linux ;;
        Darwin) echo macos ;;
        MINGW* | MSYS* | CYGWIN*) echo windows ;;
        *) echo other ;;
    esac
}

# ── Extraction ──────────────────────────────────────────────────────────────

extract_archive() { # archive, into
    case "$1" in
        *.tar.gz) tar -xzf "$1" -C "$2" ;;
        *.zip)
            if command -v 7z >/dev/null 2>&1; then
                7z x -y -bso0 -bsp0 -o"$2" "$1" >/dev/null
            elif command -v unzip >/dev/null 2>&1; then
                unzip -q -d "$2" "$1"
            elif command -v python3 >/dev/null 2>&1; then
                python3 -m zipfile -e "$1" "$2"
            else
                echo "$NAME: no zip extractor (7z, unzip, python3)" >&2
                return 1
            fi
            ;;
        *) echo "$NAME: unknown archive kind: $1" >&2; return 1 ;;
    esac
}

# Everything the extraction put on disk, directories included, one relative
# path per line. A `stage/` prefix therefore shows up as an extra entry rather
# than disappearing into a path component nobody compared.
list_entries() { # directory
    (cd "$1" && find . -mindepth 1 | sed 's|^\./||' | LC_ALL=C sort)
}

sha256_of() { # path
    # GNU coreutils escapes a checksum line whose *filename* contains a
    # backslash by prefixing the whole line with one, so on Windows — where
    # every extraction path does — the first field comes back as `\HASH`
    # rather than `HASH`. Strip it, or a digest compares unequal to itself.
    local line
    if command -v sha256sum >/dev/null 2>&1; then
        line="$(sha256sum "$1")"
    else
        line="$(shasum -a 256 "$1")"
    fi
    printf '%s' "${line%% *}" | sed 's/^\\//'
}

# ── Read-only directories, both conventions ─────────────────────────────────

make_read_only() { # directory
    # Reports how it made the directory read-only, or why it could not. The
    # previous version suppressed every error, so when Windows reported the
    # folder "still writable by this process" there was nothing in the log to
    # say whether icacls had failed, whether cygpath was missing, or whether
    # the branch had been taken at all. A precondition that cannot be
    # established must say which step did not hold.
    read_only_method=''
    read_only_why=''
    if [[ $(host_platform) == windows ]]; then
        if ! command -v icacls >/dev/null 2>&1; then
            read_only_why='icacls is not on PATH'
        elif ! command -v cygpath >/dev/null 2>&1; then
            read_only_why='cygpath is not on PATH, so the Windows path is unknown'
        else
            local win_path account deny_err
            win_path="$(cygpath -w "$1")"
            # Windows tools emit CRLF, and command substitution strips the
            # newline but not the carriage return — which would make the
            # account argument malformed and the deny fail for a reason no
            # message would explain.
            account="$(whoami 2>/dev/null | tr -d '\r\n')"
            if [[ -z $account ]]; then
                read_only_why='whoami named no account to deny'
            else
                # The deny goes on FIRST, while the directory still carries
                # the inherited ACEs that let this process edit the DACL.
                # Resetting inheritance first strips those, and the deny that
                # followed reported "could not deny writes" — which is what
                # the runner said after the previous attempt.
                # `(W)` is icacls' simple write right. The specific-rights
                # spelling `(WD,AD)` is documented but the runner rejected it
                # with "Invalid parameter", and a deny that will not parse is
                # a deny that does not exist.
                deny_err="$(MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 icacls "$win_path" /deny "$account:(OI)(CI)(W)" 2>&1)" \
                    && read_only_method="icacls deny (OI)(CI)(W) for $account" \
                    || read_only_why="icacls could not deny writes to $account: ${deny_err//$'\n'/ }"
                MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 \
                    icacls "$win_path" /inheritance:r /grant:r '*S-1-1-0:(OI)(CI)(RX)' >/dev/null 2>&1 \
                    || read_only_why="${read_only_why:-icacls could not reset inheritance}"
            fi
        fi
    else
        if chmod -R a-w "$1" 2>/dev/null && chmod 555 "$1" 2>/dev/null; then
            read_only_method='chmod a-w'
        else
            read_only_why='chmod could not clear the write bits'
        fi
    fi
}

make_writable() { # directory
    if [[ $(host_platform) == windows ]] && command -v icacls >/dev/null 2>&1; then
        MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 \
            icacls "$(cygpath -w "$1")" /remove:d "$(whoami | tr -d '\r\n')" >/dev/null 2>&1
        MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 \
            icacls "$(cygpath -w "$1")" /inheritance:e \
            /grant '*S-1-1-0:(OI)(CI)(F)' >/dev/null 2>&1
    else
        chmod -R u+w "$1" 2>/dev/null
    fi
}

# Whether this process really cannot create a file there.
directory_denies_writes() { # directory
    local probe="$1/write-probe"
    # In a subshell, so the shell's own "Permission denied" for the failed
    # redirection is what is being suppressed rather than the command's.
    if ( : >"$probe" ) 2>/dev/null; then
        rm -f "$probe" 2>/dev/null
        return 1
    fi
    return 0
}

# ── Launching the extracted binary ──────────────────────────────────────────
#
# Always with the extraction directory as the working directory, always by the
# shipped file name. Never from `target/`.
run_extracted() { # directory, data-root, log, rendered(0|1), args...
    local directory=$1 data_root=$2 log=$3 rendered=$4
    shift 4
    local asset status=0
    asset="$(asset_for_label "$LABEL")"

    local -a wrapper=()
    if ((rendered)) && [[ $(host_platform) == linux ]]; then
        # A hosted Linux runner has no display, and winit prefers a real
        # Wayland session over Xvfb when one is present.
        wrapper=("$XVFB_RUN_BIN" -a env -u WAYLAND_DISPLAY
            WINIT_UNIX_BACKEND=x11 WGPU_BACKEND=vulkan)
    fi
    # `timeout` is coreutils; a stock macOS runner has none. The client bounds
    # both of the modes used here on its own (a rendered smoke fails after 60 s,
    # a headless join after --headless-timeout-secs), so its absence costs an
    # outer belt rather than the check.
    local -a bound=()
    if command -v timeout >/dev/null 2>&1; then
        bound=(timeout --kill-after=30s "$((JOIN_TIMEOUT_SECS + 120))s")
    fi

    (
        cd "$directory" || exit 127
        # One writable root for every convention the three platforms consult,
        # so the artifacts land somewhere this script can then look.
        export HOME="$data_root"
        export XDG_DATA_HOME="$data_root"
        if command -v cygpath >/dev/null 2>&1; then
            LOCALAPPDATA="$(cygpath -w "$data_root")"
            APPDATA="$LOCALAPPDATA"
        else
            LOCALAPPDATA="$data_root"
            APPDATA="$data_root"
        fi
        export LOCALAPPDATA APPDATA
        # The defaults are the subject. An override here would test the
        # workaround the volunteer was given, not the fix.
        unset ORRERY_TELEMETRY_JSONL ORRERY_IDENTITY_FILE
        ${bound[@]+"${bound[@]}"} ${wrapper[@]+"${wrapper[@]}"} "./$asset" "$@"
    ) >"$log" 2>&1 || status=$?
    return "$status"
}

# ── The run ─────────────────────────────────────────────────────────────────

run_smoke() {
    [[ -n $ARCHIVE ]] || die '--archive is required'
    [[ -n $LABEL ]] || die '--label is required'
    local asset archive_name
    asset="$(asset_for_label "$LABEL")" || die "unknown --label '$LABEL'"
    archive_name="$(archive_name_for_label "$LABEL")"
    [[ $JOIN_TIMEOUT_SECS =~ ^[1-9][0-9]*$ ]] || die '--join-timeout-secs needs a positive integer'

    local work extraction
    work="$(mktemp -d)"
    # shellcheck disable=SC2064 # Expand the validated mktemp path now.
    # A rendered launch leaves a graphics cache under the HOME it was given,
    # and the driver can still be writing it as this returns, so removal is
    # best-effort rather than a diagnostic nobody can act on.
    trap "make_writable '$work' >/dev/null 2>&1; rm -rf '$work' 2>/dev/null" EXIT

    # ── archive ──
    if [[ -s $ARCHIVE ]]; then
        result PASS archive-present "$ARCHIVE"
    else
        result FAIL archive-present "no non-empty archive at $ARCHIVE"
        summary
        return 1
    fi
    if [[ $(basename "$ARCHIVE") == "$archive_name" ]]; then
        result PASS archive-name "$archive_name"
    else
        result FAIL archive-name \
            "archive is named $(basename "$ARCHIVE"), and PLAYTEST.md sends the volunteer to $archive_name"
    fi

    # ── extract ──
    if [[ -n $KEEP_EXTRACTION ]]; then
        extraction="$KEEP_EXTRACTION"
        rm -rf "$extraction"
        mkdir -p "$extraction"
    else
        extraction="$work/extracted"
        mkdir -p "$extraction"
    fi
    if extract_archive "$ARCHIVE" "$extraction" 2>"$work/extract.err"; then
        result PASS extract "into a fresh $extraction"
    else
        result FAIL extract "could not extract $ARCHIVE: $(cat "$work/extract.err")"
        summary
        return 1
    fi

    # ── manifest ──
    local expected actual
    expected="$(printf '%s\n' "$asset" "$asset.sha256" build-info.json README.md | LC_ALL=C sort)"
    actual="$(list_entries "$extraction")"
    if [[ $actual == "$expected" ]]; then
        result PASS manifest "four flat files: $(echo $actual)"
    else
        result FAIL manifest \
            "the extraction holds [$(echo $actual)] and the volunteer was promised [$(echo $expected)]"
    fi

    # Named separately from the manifest so a lost extension is not reported as
    # a generic listing difference. This is #768's exact shape.
    if [[ -f $extraction/$asset ]]; then
        result PASS shipped-name "$asset"
    else
        result FAIL shipped-name \
            "no $asset in the extraction, so the file PLAYTEST.md names is not the file that shipped"
    fi

    # ── the README that shipped, against the archive that shipped ──
    if [[ -f $extraction/README.md ]]; then
        # As a whole name, not a substring. `orrery-regolith-x86_64-linux` is a
        # prefix of `orrery-regolith-x86_64-linux.tar.gz`, so a plain -F match
        # would call a README that names only the archive a README that names
        # the binary — and #768's README named an archive whose binary was
        # gone.
        local asset_pattern
        asset_pattern="$(printf '%s' "$asset" | sed 's/[.[\*^$]/\\&/g')"
        if grep -qE "(^|[^A-Za-z0-9._-])$asset_pattern([^A-Za-z0-9._-]|\$)" "$extraction/README.md"; then
            result PASS readme-names-binary "$asset"
        else
            result FAIL readme-names-binary "the shipped README never names $asset"
        fi
        if grep -qF "$archive_name" "$extraction/README.md"; then
            result PASS readme-names-archive "$archive_name"
        else
            result FAIL readme-names-archive "the shipped README never names $archive_name"
        fi
    else
        result FAIL readme-names-binary 'the archive shipped no README.md'
        result FAIL readme-names-archive 'the archive shipped no README.md'
    fi

    # ── the checksum a technical helper is told to verify ──
    if [[ -f $extraction/$asset && -f $extraction/$asset.sha256 ]]; then
        local recorded computed
        recorded="$(cut -d' ' -f1 <"$extraction/$asset.sha256")"
        computed="$(sha256_of "$extraction/$asset")"
        if [[ $recorded == "$computed" ]]; then
            result PASS checksum "$computed"
        else
            result FAIL checksum "the shipped digest is $recorded and the shipped binary hashes to $computed"
        fi
        # Three spellings are all correct, and Windows produces the two
        # that a leading-space search misses: GNU text mode writes
        # `HASH  NAME`, GNU binary mode — the Git for Windows default —
        # writes `HASH *NAME`, and a name containing a backslash escapes
        # the line. Compare the recorded name as a field instead.
        if awk -v want="$asset" '{
               name = $NF
               sub(/^\*/, "", name)
               gsub(/\\\\/, "\\", name)
               if (name == want || name ~ ("(^|[/\\\\])" want "$")) { found = 1 }
           } END { exit found ? 0 : 1 }' "$extraction/$asset.sha256"; then
            result PASS checksum-names-binary "$asset"
        else
            result FAIL checksum-names-binary \
                "the checksum file does not record the name $asset, so sha256sum -c cannot find it"
        fi
    else
        result FAIL checksum 'the archive shipped no binary/checksum pair'
        result FAIL checksum-names-binary 'the archive shipped no binary/checksum pair'
    fi

    if [[ ! -f $extraction/$asset ]]; then
        summary
        return 1
    fi
    # What PLAYTEST.md tells the Linux and macOS volunteer to do before the
    # first launch. Windows needs nothing, and a `chmod` there is a no-op.
    chmod +x "$extraction/$asset" 2>/dev/null

    # ── build-info, from the extraction ──
    local info_status=0 info_origin=
    mkdir -p "$work/build-info-home"
    run_extracted "$extraction" "$work/build-info-home" "$work/build-info.log" 0 --build-info \
        || info_status=$?
    if ((info_status == 0)); then
        info_origin="$(python3 - "$work/build-info.log" 2>"$work/build-info.err" <<'INFO'
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
if not isinstance(value.get("client_rev"), str) or not value["client_rev"]:
    raise SystemExit("build-info has no client_rev")
if not isinstance(value.get("ruleset_version"), int):
    raise SystemExit("build-info has no integer ruleset_version")
origin = value.get("admission_url")
if not isinstance(origin, str) or not origin.startswith("https://"):
    raise SystemExit("build-info has no HTTPS admission_url")
print(origin)
INFO
)" || info_status=97
    fi
    if ((info_status == 0)) && [[ -n $info_origin ]]; then
        result PASS extracted-build-info "the shipped copy reports $info_origin"
    else
        result FAIL extracted-build-info \
            "the extracted $asset did not report usable build info (status $info_status; $(cat "$work/build-info.err" 2>/dev/null); log: $work/build-info.log)"
    fi

    # A pristine copy, taken before anything has been launched anywhere, so
    # what the read-only stage observes is the archive and not the leavings of
    # the stage above it.
    local read_only="$work/read-only"
    cp -R "$extraction" "$read_only"

    # ── launch from the extraction directory ──
    #
    # `--smoke-test` composes the whole client without a graphics device, and
    # opens its recording stream while doing it -- which is the write #766 and
    # #772 were about. The directory is writable here on purpose: that is what
    # makes "no target/ appeared beside the binary" an observation rather than
    # a tautology.
    local data_root launch_status=0 data_dir
    data_root="$work/launch-home"
    mkdir -p "$data_root"
    data_dir="$(data_dir_for_label "$LABEL" "$data_root")"
    run_extracted "$extraction" "$data_root" "$work/launch.log" 0 --smoke-test \
        || launch_status=$?
    if ((launch_status == 0)); then
        result PASS extracted-launch 'the extracted binary composed the client and exited 0'
    else
        result FAIL extracted-launch \
            "the extracted $asset exited $launch_status when launched from its own folder (log: $work/launch.log)"
    fi
    if [[ -f $data_dir/smoke.jsonl ]]; then
        result PASS launch-writes-to-data-dir "$data_dir/smoke.jsonl"
    else
        result FAIL launch-writes-to-data-dir \
            "nothing was written under the per-user application data directory $data_dir"
    fi
    local stray
    stray="$(list_entries "$extraction")"
    if [[ $stray == "$expected" ]]; then
        result PASS launch-leaves-folder-alone 'the extraction folder is unchanged'
    else
        result FAIL launch-leaves-folder-alone \
            "launching wrote into the volunteer's folder; it now holds [$(echo $stray)]"
    fi

    # ── and again from a folder she cannot write ──
    local ro_home ro_status=0 ro_data_dir
    ro_home="$work/read-only-home"
    mkdir -p "$ro_home"
    ro_data_dir="$(data_dir_for_label "$LABEL" "$ro_home")"
    make_read_only "$read_only"
    if directory_denies_writes "$read_only"; then
        result PASS read-only-precondition \
            "$read_only refuses a write probe (${read_only_method:-unknown method})"

        # The rendered mode, because that is the launch a volunteer performs:
        # a window, an identity key, a recording stream, all resolved from
        # defaults, from a folder that denies every one of them a home.
        run_extracted "$read_only" "$ro_home" "$work/read-only.log" 1 --render-smoke \
            || ro_status=$?
        if ((ro_status == 0)); then
            result PASS read-only-launch 'the extracted binary rendered and exited 0 from a read-only folder'
        else
            result FAIL read-only-launch \
                "the extracted $asset exited $ro_status from a read-only folder (log: $work/read-only.log)"
        fi
        # #772's message. A client that degrades here is a client whose
        # artifacts still resolve against the launch directory: the session
        # plays and banks nothing, which is what #773 cost a volunteer.
        if grep -qF 'cannot open telemetry' "$work/read-only.log"; then
            result FAIL read-only-recording-available \
                "the client reported its recording unavailable, so this session would bank nothing (log: $work/read-only.log)"
        else
            result PASS read-only-recording-available 'the session records'
        fi
        if [[ -f $ro_data_dir/session.jsonl ]]; then
            result PASS read-only-writes-to-data-dir "$ro_data_dir/session.jsonl"
        else
            result FAIL read-only-writes-to-data-dir \
                "a read-only launch wrote nothing under $ro_data_dir, so its artifacts still resolve against the launch folder"
        fi
        make_writable "$read_only"
        local ro_entries
        ro_entries="$(list_entries "$read_only")"
        if [[ $ro_entries == "$expected" ]]; then
            result PASS read-only-folder-untouched 'the read-only folder is unchanged'
        else
            result FAIL read-only-folder-untouched \
                "the read-only folder now holds [$(echo $ro_entries)]"
        fi
    else
        make_writable "$read_only"
        result FAIL read-only-precondition \
            "$read_only is still writable by this process, so the read-only launch cannot be observed here" \
            "(${read_only_why:-${read_only_method:-no method reported}})"
    fi

    # ── the deployed campaign, from this platform's artifact ──
    if [[ -n $CAMPAIGN ]]; then
        local join_home join_status=0 nickname
        join_home="$work/join-home"
        mkdir -p "$join_home"
        nickname="artifact-$LABEL-${work##*.}"
        run_extracted "$extraction" "$join_home" "$work/join.log" 1 \
            --headless-join "$CAMPAIGN" --nickname "$nickname" --campaign-consent \
            --headless-timeout-secs "$JOIN_TIMEOUT_SECS" \
            || join_status=$?
        if ((join_status == 0)); then
            result PASS artifact-join-exit 'the extracted binary joined and exited 0'
        else
            result FAIL artifact-join-exit \
                "the extracted $asset exited $join_status joining $CAMPAIGN (log: $work/join.log)"
        fi
        local marker check
        for marker in \
            "artifact-join-origin:PREFLIGHT PASS admission-origin origin=" \
            "artifact-join-joinable:PREFLIGHT PASS campaign-joinable campaign=$CAMPAIGN" \
            "artifact-join-admitted:PREFLIGHT PASS admission-accepted campaign=$CAMPAIGN" \
            "artifact-join-seated:PREFLIGHT PASS handshake-seated "; do
            check="${marker%%:*}"
            if grep -Fq -- "${marker#*:}" "$work/join.log"; then
                result PASS "$check" "${marker#*:}"
            else
                result FAIL "$check" "missing marker: ${marker#*:} (log: $work/join.log)"
            fi
        done
    fi

    summary
    ((failures == 0))
}

# ── Self-test ───────────────────────────────────────────────────────────────
#
# There is no workflow linter in this repository, so the script's own behaviour
# is the only thing that can be held to anything. The fixtures below are
# archives around a stand-in client, and each mutation is one of the defects
# this exists for, reintroduced at the layer it actually lived at.
st_dir=

# A stand-in for the shipped binary: same command surface, same write
# behaviour, and -- in the `cwd-relative` arm -- the pre-#775 habit of
# resolving its artifacts against the current working directory.
write_fixture_client() { # path, label, behaviour
    cat >"$1" <<SH
#!/usr/bin/env bash
label='$2'
behaviour='$3'
SH
    cat >>"$1" <<'SH'
if [[ ${1:-} == --build-info ]]; then
    echo '{"client_rev":"fixture","ruleset_version":19,"admission_url":"https://fixture.invalid"}'
    exit 0
fi
case "$label" in
    x86_64-windows) data_dir="${LOCALAPPDATA:-}/Orrery/Regolith" ;;
    aarch64-macos) data_dir="${HOME:-}/Library/Application Support/Orrery/Regolith" ;;
    *) data_dir="${XDG_DATA_HOME:-}/orrery/regolith" ;;
esac
file=session.jsonl
for arg in "$@"; do
    [[ $arg == --smoke-test ]] && file=smoke.jsonl
done
if [[ $behaviour == read-only-crash ]]; then
    # #772 as it shipped: the open happened during plugin registration, before
    # any UI existed, and a denial was a panic rather than a notice.
    if ( : >./write-probe ) 2>/dev/null; then
        rm -f ./write-probe
    else
        echo "thread 'main' panicked at src/lib.rs: telemetry: Access is denied (os error 5)" >&2
        exit 101
    fi
fi
if [[ $behaviour == cwd-relative ]]; then
    # #766 exactly: Cargo's build directory, relative to wherever the
    # volunteer happened to launch from.
    data_dir='target/regolith-client'
fi
if mkdir -p "$data_dir" 2>/dev/null && : >>"$data_dir/$file" 2>/dev/null; then
    :
else
    # #772's post-fix degradation: say so, keep going, bank nothing.
    echo "regolith: cannot open telemetry $data_dir/$file; this session will not be recorded or banked" >&2
fi
for arg in "$@"; do
    if [[ $arg == --headless-join ]]; then
        echo 'PREFLIGHT PASS admission-origin origin=https://fixture.invalid'
        echo 'PREFLIGHT PASS campaign-joinable campaign=fixture'
        echo 'PREFLIGHT PASS admission-accepted campaign=fixture slot=2'
        echo 'PREFLIGHT PASS handshake-seated slot=2 entity=7 ticks=64'
    fi
done
exit 0
SH
    chmod +x "$1"
}

# Build one fixture archive. `layout` is the packaging mutation.
build_fixture_archive() { # label, behaviour, layout -> echoes the archive path
    local label=$1 behaviour=$2 layout=$3
    local asset archive_name stage out
    asset="$(asset_for_label "$label")"
    archive_name="$(archive_name_for_label "$label")"
    case "$layout" in
        dropped-extension) asset="${asset%.exe}" ;;
    esac
    stage="$st_dir/stage-$label-$behaviour-$layout"
    out="$st_dir/out-$label-$behaviour-$layout"
    rm -rf "$stage" "$out"
    mkdir -p "$stage" "$out"

    write_fixture_client "$stage/$asset" "$label" "$behaviour"
    echo '{"client_rev":"fixture","ruleset_version":19,"admission_url":"https://fixture.invalid"}' \
        >"$stage/build-info.json"
    case "$layout" in
        readme-disagrees)
            printf 'Open orrery-regolith-some-other-name and read %s\n' "$archive_name" \
                >"$stage/README.md"
            ;;
        *)
            printf 'Download %s, extract it, and open %s\n' "$archive_name" "$asset" \
                >"$stage/README.md"
            ;;
    esac
    (cd "$stage" && sha256_of "$asset" >"$asset.sha256.digest" \
        && printf '%s  %s\n' "$(cat "$asset.sha256.digest")" "$asset" >"$asset.sha256" \
        && rm -f "$asset.sha256.digest")
    case "$layout" in
        bad-checksum)
            printf '%s  %s\n' "$(printf '0%.0s' $(seq 64))" "$asset" >"$stage/$asset.sha256"
            ;;
    esac

    local archive="$out/$archive_name"
    local -a names=("$asset" "$asset.sha256" build-info.json README.md)
    if [[ $archive_name == *.tar.gz ]]; then
        if [[ $layout == stage-prefix ]]; then
            local wrapper="$st_dir/wrap-$behaviour-$layout"
            rm -rf "$wrapper"
            mkdir -p "$wrapper/stage"
            cp -R "$stage/." "$wrapper/stage/"
            (cd "$wrapper" && tar -czf "$archive" stage)
        else
            (cd "$stage" && tar -czf "$archive" "${names[@]}")
        fi
    else
        if [[ $layout == stage-prefix ]]; then
            local wrapper="$st_dir/wrap-$behaviour-$layout"
            rm -rf "$wrapper"
            mkdir -p "$wrapper/stage"
            cp -R "$stage/." "$wrapper/stage/"
            (cd "$wrapper" && zip_up "$archive" stage)
        else
            (cd "$stage" && zip_up "$archive" "${names[@]}")
        fi
    fi
    echo "$archive"
}

zip_up() { # archive, paths...
    local archive=$1
    shift
    if command -v 7z >/dev/null 2>&1; then
        7z a -tzip -bso0 -bsp0 "$archive" "$@" >/dev/null
    elif command -v zip >/dev/null 2>&1; then
        zip -q -r "$archive" "$@"
    else
        python3 - "$archive" "$@" <<'ZIP'
import os, sys, zipfile
archive, roots = sys.argv[1], sys.argv[2:]
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as out:
    for root in roots:
        if os.path.isdir(root):
            for base, _, files in os.walk(root):
                for name in files:
                    path = os.path.join(base, name)
                    out.write(path, path)
        else:
            out.write(root, root)
ZIP
    fi
}

self_test() {
    local passing=0 mutations=0

    # The label table is the second source the packaging step is held against.
    [[ $(asset_for_label x86_64-windows) == orrery-regolith-x86_64-windows.exe ]] \
        || die 'self-test: the Windows asset must keep the extension Windows needs to run it'
    [[ $(asset_for_label x86_64-linux) == orrery-regolith-x86_64-linux ]] \
        || die 'self-test: the Linux asset name drifted'
    [[ $(asset_for_label aarch64-macos) == orrery-regolith-aarch64-macos ]] \
        || die 'self-test: the macOS asset name drifted'
    asset_for_label x86_64-freebsd >/dev/null 2>&1 \
        && die 'self-test: an unknown label must not resolve to a name'

    # ...and against the names PLAYTEST.md actually puts in front of a
    # volunteer, which is the other half of #768: the README and the archive
    # disagreeing is itself the defect.
    local playtest label
    playtest="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/clients/regolith/PLAYTEST.md"
    if [[ -f $playtest ]]; then
        for label in x86_64-linux x86_64-windows aarch64-macos; do
            grep -qF "$(asset_for_label "$label")" "$playtest" \
                || die "self-test: PLAYTEST.md does not name $(asset_for_label "$label")"
            grep -qF "$(archive_name_for_label "$label")" "$playtest" \
                || die "self-test: PLAYTEST.md does not name $(archive_name_for_label "$label")"
        done
        echo "$NAME: self-test: PLAYTEST.md names all six shipped file names"
    else
        echo "$NAME: self-test: no PLAYTEST.md here; skipped the README clause" >&2
    fi

    st_dir="$(mktemp -d)"
    # shellcheck disable=SC2064 # Expand the validated mktemp path now.
    trap "chmod -R u+w '$st_dir' >/dev/null 2>&1; rm -rf '$st_dir'" EXIT
    mkdir -p "$st_dir/bin"
    # A passthrough for the rendered launch, so the self-test needs no X server.
    cat >"$st_dir/bin/xvfb-run" <<'SH'
#!/usr/bin/env bash
shift # -a
[[ $1 == env ]] && shift
while [[ ${1:-} == -* || ${1:-} == *=* ]]; do
    if [[ $1 == -u ]]; then shift 2; else shift; fi
done
exec "$@"
SH
    chmod +x "$st_dir/bin/xvfb-run"

    st_run() { # label, behaviour, layout, extra args...
        local label=$1 behaviour=$2 layout=$3
        shift 3
        local archive
        archive="$(build_fixture_archive "$label" "$behaviour" "$layout")"
        PACKAGE_ARTIFACT_SMOKE_XVFB_RUN="$st_dir/bin/xvfb-run" \
            "$0" --archive "$archive" --label "$label" "$@" 2>&1
    }

    st_counts() { # output -> "pass fail"
        printf '%s %s\n' \
            "$(grep -c '^PASS ' <<<"$1" | tr -d ' ')" \
            "$(grep -c '^FAIL ' <<<"$1" | tr -d ' ')"
    }

    local output status counts

    # ── baselines ──
    for label in x86_64-linux x86_64-windows aarch64-macos; do
        status=0; output="$(st_run "$label" good ordinary)" || status=$?
        ((status == 0)) || die "self-test: $label baseline failed
$output"
        grep -Fq "SUMMARY PASS $NAME failures=0" <<<"$output" \
            || die "self-test: $label baseline emitted no passing summary"
        counts="$(st_counts "$output")"
        [[ $counts == '18 0' ]] \
            || die "self-test: $label baseline counted $counts pass/fail, expected 18 0"
        passing=$((passing + 1))
    done

    # ...and with a campaign join, which adds four clauses.
    status=0; output="$(st_run x86_64-windows good ordinary --campaign fixture)" || status=$?
    ((status == 0)) || die "self-test: join baseline failed
$output"
    counts="$(st_counts "$output")"
    [[ $counts == '23 0' ]] \
        || die "self-test: join baseline counted $counts pass/fail, expected 23 0"
    grep -Fq 'PASS artifact-join-seated ' <<<"$output" \
        || die 'self-test: join baseline did not observe the seating marker'
    passing=$((passing + 1))

    # ── #768, the shape it actually shipped in: an internal build folder ──
    status=0; output="$(st_run x86_64-windows good stage-prefix)" || status=$?
    ((status != 0)) || die "self-test: a stage/-prefixed archive passed
$output"
    grep -Fq 'FAIL manifest ' <<<"$output" \
        || die 'self-test: the stage-prefix mutation did not fail named check manifest'
    grep -Fq 'FAIL shipped-name ' <<<"$output" \
        || die 'self-test: the stage-prefix mutation did not fail named check shipped-name'
    mutations=$((mutations + 1))

    # ── #768, the other half: the extension Windows needs to run the file ──
    status=0; output="$(st_run x86_64-windows good dropped-extension)" || status=$?
    ((status != 0)) || die "self-test: an extensionless Windows binary passed
$output"
    grep -Fq 'FAIL shipped-name ' <<<"$output" \
        || die 'self-test: the dropped-extension mutation did not fail named check shipped-name'
    grep -Fq 'FAIL manifest ' <<<"$output" \
        || die 'self-test: the dropped-extension mutation did not fail named check manifest'
    grep -Fq 'FAIL readme-names-binary ' <<<"$output" \
        || die 'self-test: the dropped-extension mutation did not fail named check readme-names-binary'
    mutations=$((mutations + 1))

    # ── the README and the archive disagreeing ──
    status=0; output="$(st_run x86_64-linux good readme-disagrees)" || status=$?
    ((status != 0)) || die "self-test: a README naming a file the archive lacks passed
$output"
    grep -Fq 'FAIL readme-names-binary ' <<<"$output" \
        || die 'self-test: the readme-disagrees mutation did not fail named check readme-names-binary'
    grep -Fq 'PASS manifest ' <<<"$output" \
        || die 'self-test: the readme-disagrees mutation should leave the manifest intact'
    mutations=$((mutations + 1))

    # ── a checksum that does not check ──
    status=0; output="$(st_run x86_64-linux good bad-checksum)" || status=$?
    ((status != 0)) || die "self-test: a wrong shipped digest passed
$output"
    grep -Fq 'FAIL checksum ' <<<"$output" \
        || die 'self-test: the bad-checksum mutation did not fail named check checksum'
    mutations=$((mutations + 1))

    # ── #766 and #772: artifacts resolved against the launch folder ──
    #
    # The client still exits 0 -- #775 made an unwritable recording degradable
    # rather than fatal -- so an exit-status check would pass this. What fails
    # is where the writes went.
    status=0; output="$(st_run x86_64-linux cwd-relative ordinary)" || status=$?
    ((status != 0)) || die "self-test: a CWD-relative artifact path passed
$output"
    grep -Fq 'PASS extracted-launch ' <<<"$output" \
        || die 'self-test: the cwd-relative mutation should still exit 0 from a writable folder'
    grep -Fq 'FAIL launch-writes-to-data-dir ' <<<"$output" \
        || die 'self-test: the cwd-relative mutation did not fail named check launch-writes-to-data-dir'
    grep -Fq 'FAIL launch-leaves-folder-alone ' <<<"$output" \
        || die 'self-test: the cwd-relative mutation did not fail named check launch-leaves-folder-alone'
    grep -Fq 'FAIL read-only-recording-available ' <<<"$output" \
        || die 'self-test: the cwd-relative mutation did not fail named check read-only-recording-available'
    grep -Fq 'FAIL read-only-writes-to-data-dir ' <<<"$output" \
        || die 'self-test: the cwd-relative mutation did not fail named check read-only-writes-to-data-dir'
    grep -Fq 'PASS read-only-precondition ' <<<"$output" \
        || die 'self-test: the read-only stage could not be observed, so its failures mean nothing'
    counts="$(st_counts "$output")"
    [[ $counts == '14 4' ]] \
        || die "self-test: the cwd-relative mutation counted $counts pass/fail, expected 14 4"
    mutations=$((mutations + 1))

    # ── #772 in its original form: dying instead of degrading ──
    status=0; output="$(st_run x86_64-linux read-only-crash ordinary)" || status=$?
    ((status != 0)) || die "self-test: a client that dies in a read-only folder passed
$output"
    grep -Fq 'FAIL read-only-launch ' <<<"$output" \
        || die 'self-test: the read-only-crash mutation did not fail named check read-only-launch'
    grep -Fq 'PASS extracted-launch ' <<<"$output" \
        || die 'self-test: the read-only-crash mutation should still launch from a writable folder'
    mutations=$((mutations + 1))

    echo "$NAME: self-test passed ($passing baselines: three platform archives at 18 pass / 0 fail each, plus a campaign join at 23 / 0; $mutations mutations: stage-prefix and dropped-extension each fail manifest + shipped-name, readme-disagrees fails readme-names-binary with the manifest intact, bad-checksum fails checksum, cwd-relative fails launch-writes-to-data-dir + launch-leaves-folder-alone + read-only-recording-available + read-only-writes-to-data-dir at 14 pass / 4 fail while still exiting 0, read-only-crash fails read-only-launch)"
}

while (($#)); do
    case "$1" in
        --archive) shift; (($#)) || die '--archive needs a path'; ARCHIVE=$1 ;;
        --label) shift; (($#)) || die '--label needs a value'; LABEL=$1 ;;
        --campaign) shift; (($#)) || die '--campaign needs an id'; CAMPAIGN=$1 ;;
        --join-timeout-secs) shift; (($#)) || die '--join-timeout-secs needs a value'; JOIN_TIMEOUT_SECS=$1 ;;
        --keep-extraction) shift; (($#)) || die '--keep-extraction needs a path'; KEEP_EXTRACTION=$1 ;;
        --self-test) self_test; exit $? ;;
        --help | -h) usage; exit 0 ;;
        *) die "unknown argument '$1'" ;;
    esac
    shift
done

run_smoke
