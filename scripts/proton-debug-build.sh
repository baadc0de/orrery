#!/usr/bin/env bash
# Build a Regolith Windows client that starts under Proton/Wine (#1060).
#
# ── Why this script exists ──────────────────────────────────────────────────
#
# The shipped Windows client aborts before its first frame under every Wine on
# the box (Proton Experimental, Proton Hotfix, system wine-11.16):
#
#   wine: Call from ... to unimplemented function iphlpapi.dll.GetIpNetEntry2, aborting
#
# `netdev 0.46.1` calls it from `get_neighbor_mac`
# (`src/os/windows/interface.rs:124-134`) to fill in the default gateway's MAC
# while enumerating interfaces. That runs at startup, under `iroh 1.0.3` ->
# `netwatch 0.19.2`. Wine implements `GetIpNetTable2` but not the single-entry
# lookup, so no runtime setting and no `WINEDLLOVERRIDES` can help.
#
# The obvious fix -- `netdev` with `default-features = false`, dropping its
# `gateway` feature -- does not compile. `netwatch 0.19.2` calls
# `netdev::get_default_gateway()` unconditionally on Windows
# (`src/interfaces/netdev_impl.rs:117`), and `netdev` gates that function
# behind exactly that feature (`src/lib.rs:28-29`). Turning the feature off
# removes the function netwatch needs. So the patch has to be to netdev's
# *source*: keep the feature, stub the one call Wine lacks.
#
# ── Why it cannot reach a volunteer or the ledger ───────────────────────────
#
# Four independent structural facts, none of them a convention:
#
#  1. Nothing here is committed. The patched netdev is generated into a cache
#     directory outside the repository, and the patch is passed to cargo with
#     `--config`, so `clients/regolith/Cargo.toml` and `Cargo.lock` are
#     untouched. `package-client.yml` builds `cargo build --release --locked`
#     with no `--config`, which resolves the ordinary netdev.
#
#     A caveat stated rather than glossed: the *bytes* of the release binary do
#     change, because this repository grew four `#[cfg(proton_debug)]`
#     statements. That is unavoidable and not specific to them -- the binary is
#     a deterministic function of its source, and appending a lone comment to
#     `lib.rs` moves its hash just as far (measured: e4cccd1... -> b8baee0...
#     -> e4cccd1... on revert). What is guaranteed is that with the cfg unset
#     every one of those statements is stripped before lowering, so the shipped
#     build runs the same code it ran before, resolved from the same lockfile.
#  2. The artifact is written outside the repository too. Packaging copies
#     `clients/regolith/target/release/<binary>`; this build never writes
#     there.
#  3. It is compiled with `--cfg proton_debug`, which packaging never passes,
#     and which sets `orrery_regolith_client::BANKABLE` false. That makes
#     `append_session_record` refuse to write the row,
#     `queue_finished_session` refuse to queue it, and `retry_pending_uploads`
#     refuse to post anything an ordinary build left in the same directory. The
#     session produces no campaign evidence at all -- measured: after this run
#     there is no `campaign-records.jsonl` and no `uploads.json`.
#     A bare `--cfg` rather than a cargo feature, and that is fact (1) again:
#     a feature would live in `Cargo.toml`, in `cargo tree`, and in cargo's
#     `-C metadata`, where this debugging aid has no business being.
#     `build.rs` declares the cfg name so `unexpected_cfgs` stays quiet without
#     a manifest entry.
#  4. `--build-info` exits non-zero under that cfg, and packaging's staging
#     step runs it under `set -euo pipefail`. Staging such a binary fails.
#
# ── Use ─────────────────────────────────────────────────────────────────────
#
#   scripts/proton-debug-build.sh
#
# `ORRERY_BUILD_REV` is honoured (the client's `build.rs` reads it). The live
# campaign refuses a client whose revision is not the one it pins -- see
# `admit_headless`'s `campaign-compatible` stage -- so a run against
# `campaigns.distopik.com` must stamp the revision that
# `GET /v1/campaigns` reports, and that stamp is a claim about provenance the
# binary cannot honour. It is safe here only because (3) means the binary can
# never turn a session into a row.
#
# The script prints the path of the exe it built.

set -euo pipefail

readonly NAME='proton-debug-build'
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
readonly CLIENT="$ROOT/clients/regolith"
readonly TARGET='x86_64-pc-windows-gnu'

# Everything this build produces lives here, outside the repository, so that
# neither a stray `git add` nor packaging's `target/release` copy can pick it
# up. See fact (1) and fact (2) above.
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/orrery-proton-debug"
readonly CACHE

die() { printf '%s: %s\n' "$NAME" "$1" >&2; exit 1; }
say() { printf '%s: %s\n' "$NAME" "$1" >&2; }

command -v cargo >/dev/null || die 'cargo is not on PATH'
command -v x86_64-w64-mingw32-gcc >/dev/null ||
    die "no mingw-w64 linker; install it (Arch: pacman -S mingw-w64-gcc)"
rustc --print target-list | grep -qx "$TARGET" || die "rustc does not know $TARGET"
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" ||
    die "the $TARGET std is not installed; run: rustup target add $TARGET"

# ── The netdev the client actually locks ────────────────────────────────────
#
# Read from the client's own lockfile rather than hardcoded, so a dependency
# bump fails loudly here instead of silently patching a version nothing uses.
version="$(awk '
    /^name = "netdev"$/ { want = 1; next }
    want && /^version = / { gsub(/[",]/, "", $3); print $3; exit }
' "$CLIENT/Cargo.lock")"
[[ -n $version ]] || die "clients/regolith/Cargo.lock does not lock netdev"
say "clients/regolith locks netdev $version"

source_dir=''
for candidate in "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/"netdev-$version"; do
    [[ -d $candidate ]] && source_dir="$candidate"
done
[[ -n $source_dir ]] ||
    die "netdev-$version is not unpacked in the registry; run a build first"

patched="$CACHE/netdev-$version"
rm -rf -- "$patched"
mkdir -p -- "$(dirname -- "$patched")"
cp -r -- "$source_dir" "$patched"
chmod -R u+w -- "$patched"

# ── The patch ───────────────────────────────────────────────────────────────
#
# `get_neighbor_mac` returns `Option<MacAddr>` and its one caller already
# handles `None` (`interface.rs:330` feeds it to `GatewayCandidates`, whose
# `*_mac` fields are `Option`). So the gateway's MAC is simply absent, which
# for a P2P transport is informational -- iroh does not need it to connect.
#
# The import of `GetIpNetEntry2` goes with the call. A `#[link]` declaration
# that is never referenced would not reach the import table anyway, but leaving
# it would leave the reader unsure whether the abort could still happen.
target_file="$patched/src/os/windows/interface.rs"
[[ -f $target_file ]] || die "patched netdev has no $target_file"

python3 - "$target_file" <<'PATCH'
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()

before = text

imports_old = (
    "use windows_sys::Win32::NetworkManagement::IpHelper::"
    "{GetIpNetEntry2, MIB_IPNET_ROW2, SendARP};"
)
imports_new = (
    "// proton-debug (#1060): `GetIpNetEntry2` and the `MIB_IPNET_ROW2` it\n"
    "// fills are gone with the call below. Wine has no such export and\n"
    "// aborts the process on it.\n"
    "use windows_sys::Win32::NetworkManagement::IpHelper::SendARP;"
)
if imports_old not in text:
    sys.exit("proton-debug patch: netdev's iphelper import is not the expected one")
text = text.replace(imports_old, imports_new, 1)

body_old = """fn get_neighbor_mac(address: SOCKADDR_INET, interface_luid: NET_LUID_LH) -> Option<MacAddr> {
    let mut row = MIB_IPNET_ROW2 {
        Address: address,
        InterfaceLuid: interface_luid,
        ..Default::default()
    };
    let result = unsafe { GetIpNetEntry2(&mut row) };
    if result != NO_ERROR {
        return None;
    }
    physical_address_to_mac(&row.PhysicalAddress, row.PhysicalAddressLength)
}"""
body_new = """fn get_neighbor_mac(_address: SOCKADDR_INET, _interface_luid: NET_LUID_LH) -> Option<MacAddr> {
    // proton-debug (#1060): upstream calls `GetIpNetEntry2` here to read the
    // gateway's MAC out of the neighbour table. Wine does not implement that
    // export and aborts the process rather than failing the call, so under
    // Proton the client dies before its first frame.
    //
    // The one caller already treats `None` as "no MAC known", so reporting
    // that is the whole patch. It costs the default gateway's MAC address,
    // which nothing in iroh's path needs.
    None
}"""
if body_old not in text:
    sys.exit("proton-debug patch: netdev's get_neighbor_mac is not the expected one")
text = text.replace(body_old, body_new, 1)

if text == before:
    sys.exit("proton-debug patch: nothing changed")
open(path, "w", encoding="utf-8").write(text)
PATCH
say "patched $patched"

# `interfaces.rs` keeps `physical_address_to_mac` and `NO_ERROR` for their other
# users, so no further edit is needed. Warnings, not errors, if that ever stops
# being true -- and this is a debugging build, so a warning is the right cost.

# ── Keeping the lockfile out of it ──────────────────────────────────────────
#
# A `[patch]` forces cargo to re-resolve, and cargo writes the result back to
# `clients/regolith/Cargo.lock`. Measured: the first run of this script moved
# `netdev` from 0.46.1 to 0.46.2 in the committed lockfile. That is precisely
# the "the shipped build must not change" failure, arriving through the back
# door -- packaging builds `--locked`, so a bumped lockfile is a bumped
# release. So the lockfile is restored unconditionally, on success, on failure
# and on interrupt, and the restore is checked rather than assumed.
lock="$CLIENT/Cargo.lock"
lock_backup="$CACHE/Cargo.lock.committed"
cp -- "$lock" "$lock_backup"
restore_lock() {
    local status=$?
    if ! cmp -s -- "$lock_backup" "$lock"; then
        cp -- "$lock_backup" "$lock"
        say 'restored clients/regolith/Cargo.lock, which the patched resolve rewrote'
    fi
    cmp -s -- "$lock_backup" "$lock" ||
        die 'clients/regolith/Cargo.lock could not be restored -- restore it by hand before committing'
    return "$status"
}
trap restore_lock EXIT

target_dir="$CACHE/target"
build_log="$CACHE/build.log"
say "building $TARGET (this is a cold Bevy build the first time)"
(
    cd "$CLIENT"
    # RUSTFLAGS, not `--features`: see fact (3). It replaces any inherited
    # value on purpose, so the cfg cannot be lost to an ambient setting.
    RUSTFLAGS='--cfg proton_debug' cargo build \
        --release \
        --target "$TARGET" \
        --target-dir "$target_dir" \
        --config "patch.crates-io.netdev.path='$patched'"
) 2>&1 | tee "$build_log"
[[ ${PIPESTATUS[0]} -eq 0 ]] || die 'the patched build failed'

# A `[patch]` cargo decides it does not need is a warning, not an error, and
# the binary that comes out of that build still calls `GetIpNetEntry2` and
# still aborts under Wine. Refuse to hand back a binary that was not patched.
if grep -q 'was not used in the crate graph' "$build_log"; then
    die 'cargo ignored the netdev patch -- see the warning above; the version it locks has moved'
fi

exe="$target_dir/$TARGET/release/orrery_regolith_client.exe"
[[ -s $exe ]] || die "no binary at $exe"

# The import table is the claim, checked rather than argued: an `iphlpapi.dll`
# import of `GetIpNetEntry2` is exactly what Wine aborts on. A patched build
# imports `SendARP` from that DLL and nothing else that Wine lacks.
if command -v x86_64-w64-mingw32-objdump >/dev/null; then
    if x86_64-w64-mingw32-objdump -p "$exe" | grep -q 'GetIpNetEntry2'; then
        die 'the binary still imports iphlpapi.GetIpNetEntry2, so it will abort under Wine'
    fi
    say 'the binary imports no GetIpNetEntry2'
fi

# Fact (4) as an assertion rather than a claim: if this ever prints build-info,
# the boundary has been lost and packaging would happily stage the binary.
# Needs a Windows loader, so it runs only where one is installed; the refusal
# itself lives in `main.rs` and does not depend on this check.
if command -v wine >/dev/null; then
    if WINEDEBUG=-all wine "$exe" --build-info >/dev/null 2>&1; then
        die 'this build answered --build-info, so it is packageable -- the proton_debug refusal is gone'
    fi
    say '--build-info is refused, so packaging cannot stage this binary'
else
    say 'no wine on PATH: could not exercise the --build-info refusal here'
fi

say "built $exe"
printf '%s\n' "$exe"
