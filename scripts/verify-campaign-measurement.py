#!/usr/bin/env python3
"""Verify one client SessionRecord against the host-authenticated NodeId."""

from __future__ import annotations

import binascii
import json
import subprocess
import sys
import tempfile

DOMAIN = b"orrery/campaign-measurement/v1\0"
SPKI_ED25519_PREFIX = binascii.unhexlify("302a300506032b6570032100")


def refuse(detail: str) -> "NoReturn":
    raise SystemExit(f"campaign measurement refused: {detail}")


def lowercase_hex(value: object, size: int, field: str) -> bytes:
    if not isinstance(value, str) or len(value) != size * 2 or value.lower() != value:
        refuse(f"{field} is not {size} lowercase-hex bytes")
    try:
        decoded = bytes.fromhex(value)
    except ValueError:
        refuse(f"{field} is not {size} lowercase-hex bytes")
    if len(decoded) != size:
        refuse(f"{field} is not {size} lowercase-hex bytes")
    return decoded


def main() -> None:
    if len(sys.argv) != 2:
        refuse("usage: verify-campaign-measurement.py <host-authenticated-node>")
    expected_node = sys.argv[1]
    try:
        row = json.load(sys.stdin)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        refuse(f"row is not JSON: {error}")
    if not isinstance(row, dict):
        refuse("row is not a JSON object")

    node = row.get("measurement_node")
    if node != expected_node:
        refuse("measurement_node is not the host-authenticated transport identity")
    public = lowercase_hex(node, 32, "measurement_node")
    payload = lowercase_hex(row.get("measurement_payload"), len(str(row.get("measurement_payload", ""))) // 2, "measurement_payload")
    if not payload:
        refuse("measurement_payload is empty")
    signature = lowercase_hex(row.get("measurement_signature"), 64, "measurement_signature")
    try:
        signed_fields = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        refuse(f"measurement_payload is not JSON: {error}")

    unsigned_row = dict(row)
    unsigned_row.pop("pipeline_digest", None)
    unsigned_row.pop("measurement_payload", None)
    unsigned_row.pop("measurement_signature", None)
    if signed_fields != unsigned_row:
        refuse("signed payload does not equal the client-owned row fields")

    with tempfile.TemporaryDirectory() as directory:
        public_path = f"{directory}/public.der"
        message_path = f"{directory}/message"
        signature_path = f"{directory}/signature"
        with open(public_path, "wb") as file:
            file.write(SPKI_ED25519_PREFIX + public)
        with open(message_path, "wb") as file:
            file.write(DOMAIN + payload)
        with open(signature_path, "wb") as file:
            file.write(signature)
        result = subprocess.run(
            [
                "openssl",
                "pkeyutl",
                "-verify",
                "-pubin",
                "-inkey",
                public_path,
                "-keyform",
                "DER",
                "-rawin",
                "-in",
                message_path,
                "-sigfile",
                signature_path,
            ],
            capture_output=True,
            check=False,
        )
    if result.returncode != 0:
        refuse("measurement_signature is not valid for measurement_node")


if __name__ == "__main__":
    main()
