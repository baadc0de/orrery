#!/usr/bin/env python3
"""Test-only signer for campaign SessionRecord fixtures."""

from __future__ import annotations

import binascii
import json
import subprocess
import sys

DOMAIN = b"orrery/campaign-measurement/v1\0"
PKCS8_ED25519_PREFIX = binascii.unhexlify("302e020100300506032b657004220420")
SPKI_ED25519_PREFIX = binascii.unhexlify("302a300506032b6570032100")
TEST_SECRET_BYTE = 0x49


def openssl(command: list[str], data: bytes) -> bytes:
    return subprocess.run(command, input=data, capture_output=True, check=True).stdout


def main() -> None:
    # One fixture key is enough for a single-human row, and not enough for a
    # cohort: #572's binding fixtures need each seat to sign with a *different*
    # key, or "bound to the wrong node" is indistinguishable from "bound to the
    # right one". `--secret-byte` gives each fixture seat its own key; the
    # default is the byte every existing fixture already signs with, so the
    # rows in `p4-ledger.sh` and `p4-campaign-session.sh` are unchanged.
    secret_byte = TEST_SECRET_BYTE
    argv = sys.argv[1:]
    if argv[:1] == ["--secret-byte"]:
        secret_byte = int(argv[1], 0)
        if not 0 <= secret_byte <= 0xFF:
            raise SystemExit("--secret-byte must be one byte")
        argv = argv[2:]
    if argv:
        raise SystemExit("usage: sign-campaign-measurement-fixture.py [--secret-byte N]")
    secret = bytes([secret_byte]) * 32
    row = json.load(sys.stdin)
    if not isinstance(row, dict):
        raise SystemExit("fixture row must be an object")
    public_der = openssl(
        ["openssl", "pkey", "-inform", "DER", "-pubout", "-outform", "DER"],
        PKCS8_ED25519_PREFIX + secret,
    )
    if not public_der.startswith(SPKI_ED25519_PREFIX):
        raise SystemExit("openssl returned an unexpected Ed25519 public key")
    public = public_der[len(SPKI_ED25519_PREFIX) :]
    row["measurement_node"] = public.hex()
    unsigned = dict(row)
    unsigned.pop("pipeline_digest", None)
    unsigned.pop("measurement_payload", None)
    unsigned.pop("measurement_signature", None)
    payload = json.dumps(unsigned, sort_keys=True, separators=(",", ":")).encode()
    # `pkeyutl` cannot take both the key and message on stdin, so sign through
    # temporary files.
    import tempfile

    with tempfile.TemporaryDirectory() as directory:
        key_path = f"{directory}/key.der"
        message_path = f"{directory}/message"
        with open(key_path, "wb") as file:
            file.write(PKCS8_ED25519_PREFIX + secret)
        with open(message_path, "wb") as file:
            file.write(DOMAIN + payload)
        signature = subprocess.run(
            [
                "openssl",
                "pkeyutl",
                "-sign",
                "-inkey",
                key_path,
                "-keyform",
                "DER",
                "-rawin",
                "-in",
                message_path,
            ],
            capture_output=True,
            check=True,
        ).stdout
    row["measurement_payload"] = payload.hex()
    row["measurement_signature"] = signature.hex()
    json.dump(row, sys.stdout, sort_keys=True, separators=(",", ":"))
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
