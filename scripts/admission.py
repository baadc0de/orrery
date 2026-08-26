#!/usr/bin/env python3
"""Campaign admission service (#476).

This is deliberately only a box office: it invokes ``orrery-invite`` for
allocation and signing, starts the harness, and files reports.  In particular,
there is no SessionTokenV1 encoder or ledger append path in this file.
"""
from __future__ import annotations

import argparse
import configparser
import fcntl
import json
import logging
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from dataclasses import dataclass
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlparse

MINT_FLOOR_BYTES = 10 * 1024**3
MAX_UPLOAD_BYTES = 64 * 1024**2
CAMPAIGN_ID = re.compile(r"[a-z0-9-]{1,64}\Z")
NODE = re.compile(r"[0-9a-f]{64}\Z")
SESSION = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\Z")


class Refusal(Exception):
    def __init__(self, status: int, error: str, detail: str, **extra: Any):
        self.status, self.error, self.detail, self.extra = status, error, detail, extra


@dataclass(frozen=True)
class Campaign:
    ident: str; title: str; open: bool; host: str; peers: int; seconds: int
    loss_pct: int; jitter_ms: int; client_rev: str | None


class Admission:
    def __init__(self, control: Path, state: Path, invite: str, ssh: str, ssh_key: Path,
                 issuer: Path, swarm: str, statvfs=os.statvfs):
        self.control, self.state = control, state
        self.invite, self.ssh, self.ssh_key, self.issuer, self.swarm = invite, ssh, ssh_key, issuer, swarm
        self.statvfs = statvfs
        self.last_good: dict[str, Campaign] | None = None
        self.last_note: str | None = None
        self.children: dict[str, subprocess.Popen[str]] = {}
        self.locks: dict[str, Any] = {}
        self.state.mkdir(parents=True, exist_ok=True)

    def campaigns(self) -> tuple[dict[str, Campaign], str | None]:
        if not self.control.exists():
            self.last_note = "campaigns.conf is absent"
            return {}, self.last_note
        parser = configparser.ConfigParser(interpolation=None)
        try:
            with self.control.open(encoding="utf-8") as f: parser.read_file(f)
            got = {}
            for ident in parser.sections():
                if not CAMPAIGN_ID.fullmatch(ident): raise ValueError(f"invalid campaign id {ident!r}")
                s = parser[ident]
                got[ident] = Campaign(ident, s["title"], s.get("open", "").lower() == "yes", s["host"],
                    s.getint("peers"), s.getint("seconds"), s.getint("loss_pct"), s.getint("jitter_ms"),
                    s.get("client_rev") or None)
            self.last_good, self.last_note = got, None
            return got, None
        except (OSError, ValueError, configparser.Error, KeyError) as e:
            note = f"campaigns.conf failed to parse: {e}; serving the previous version"
            self.last_note = note
            return self.last_good or {}, note

    def free_bytes(self) -> int:
        v = self.statvfs(self.state)
        return v.f_bavail * v.f_frsize

    def listing(self) -> dict[str, Any]:
        campaigns, note = self.campaigns(); free = self.free_bytes()
        paused = free < MINT_FLOOR_BYTES
        if paused:
            floor_note = f"admissions paused: {free / 1024**3:.1f} GB free is below MINT_FLOOR_BYTES"
            logging.warning(floor_note); note = floor_note if note is None else f"{note}; {floor_note}"
        rows = []
        for c in campaigns.values():
            state = "paused" if c.open and paused else ("busy" if c.ident in self.children else ("open" if c.open else "closed"))
            rows.append({"id": c.ident, "title": c.title, "state": state, "peers": c.peers, "seconds": c.seconds,
                         "loss_pct": c.loss_pct, "jitter_ms": c.jitter_ms, "client_rev": c.client_rev,
                         "server_rev": c.client_rev})
        return {"campaigns": rows, "operator_note": note}

    @staticmethod
    def output(command: list[str]) -> dict[str, str]:
        try: out = subprocess.run(command, check=True, text=True, capture_output=True).stdout
        except (OSError, subprocess.CalledProcessError) as e: raise RuntimeError(str(e)) from e
        return dict(line.split("=", 1) for line in out.splitlines() if "=" in line)

    def append_join(self, campaign: Campaign, row: dict[str, Any]) -> None:
        p = self.state / campaign.ident; p.mkdir(parents=True, exist_ok=True)
        with (p / "joins.jsonl").open("a", encoding="utf-8") as f:
            f.write(json.dumps(row, separators=(",", ":")) + "\n"); f.flush(); os.fsync(f.fileno())

    def join(self, ident: str, request: dict[str, Any]) -> dict[str, Any]:
        # Steps 1--5 intentionally precede the lock and every subprocess.
        campaigns, _ = self.campaigns()
        if not CAMPAIGN_ID.fullmatch(ident) or ident not in campaigns: raise Refusal(404, "unknown_campaign", "That campaign has ended — refresh the list.")
        c = campaigns[ident]
        if c.client_rev and request.get("client_rev") != c.client_rev: raise Refusal(403, "client_rev_mismatch", f"This campaign needs build {c.client_rev} — download the current build.")
        if not c.open: raise Refusal(403, "campaign_closed", "This campaign is closed; pick another.")
        free = self.free_bytes()
        if free < MINT_FLOOR_BYTES:
            logging.warning("admissions paused: %d free bytes is below MINT_FLOOR_BYTES", free)
            raise Refusal(503, "admissions_paused", "Campaigns are temporarily unavailable while the operator makes room — nothing you did was wrong. Try again later.")
        nickname, node = request.get("nickname"), request.get("node")
        if not isinstance(nickname, str) or not re.fullmatch(r"[^\t\r\n]{1,32}", nickname): raise Refusal(422, "bad_nickname", "Nicknames are 1–32 characters, no tabs or newlines.")
        if not isinstance(node, str) or not NODE.fullmatch(node): raise Refusal(422, "bad_node", "This build sent a bad transport key — reinstall the client.")
        directory = self.state / c.ident; directory.mkdir(parents=True, exist_ok=True)
        lock = (directory / "lock").open("a+")
        try:
            try: fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                lock.close()
                raise Refusal(409, "campaign_busy", f"In use — try again in about {c.seconds // 60} minutes.", retry_after_s=c.seconds)
            self.locks[ident] = lock
            try:
                minted = self.output([self.invite, "mint", "--ledger", str(directory / "ledger.tsv"), "--label", nickname])
                account, sid = minted["account"], minted["session_id"]
                signed = self.output([self.invite, "session-token", "--issuer-credential", str(self.issuer), "--account", account, "--node", node])
                self.append_join(c, {"when": int(time.time()), "campaign": ident, "nickname": nickname, "account": int(account), "session_id": sid, "node": node})
            except (RuntimeError, KeyError, ValueError) as e:
                logging.exception("admission subprocess/log failed: %s", e)
                raise Refusal(500, "admission_failed", "Admission failed; tell the operator.") from e
            session_dir = self.state / "sessions" / sid; session_dir.mkdir(parents=True, exist_ok=True)
            remote = f"/var/tmp/orrery/{sid}"
            # The harness writes its report and listening file into `remote`,
            # which lives on the *campaign* host and does not exist there yet.
            # Without this the harness binds, prints its node, and then dies on
            # "cannot write the exterior listening file" — an admission that
            # looks like a host failure but is a missing directory. Found by
            # standing the service up on a real box (#488).
            command = [self.ssh, "-i", str(self.ssh_key), f"orrery@{c.host}",
                       "mkdir", "-p", remote, "&&", self.swarm, "--external-peer", "--peers", str(c.peers), "--seconds", str(c.seconds), "--min-cells", "1", "--impaired", "--witness", "--stamp-wall-clock", "--json", f"{remote}/raw.json", "--listening-file", f"{remote}/listening.txt", "--require-session", sid, "--issuer-key", f"{signed['issuer_key_id']}:{signed['issuer_public_key']}"]
            if c.client_rev: command += ["--require-client-rev", c.client_rev]
            try: child = subprocess.Popen(command, text=True)
            except OSError as e: raise Refusal(503, "host_failed", "The host could not start your session — tell the operator, nothing you did was wrong.") from e
            listening = self._wait_listening(c, remote, session_dir, child)
            self.children[ident] = child
            threading.Thread(target=self._reap, args=(ident, c, sid, remote, session_dir, child), daemon=True).start()
            host_node, host_direct = listening.split(None, 1)
            return {"join": {"host_node": host_node, "slot": c.peers, "session_id": sid, "session_token": signed["session_token"]}, "host_direct": host_direct, "account": int(account), "expires_in_s": 3600, "configured": {"loss_pct": c.loss_pct, "jitter_p50_ms": c.jitter_ms, "jitter_p99_ms": c.jitter_ms}}
        finally:
            # The flock stays held by the child/reaper, not the request.  It is released there.
            if ident not in self.children:
                self.locks.pop(ident, None)
                lock.close()

    def _wait_listening(self, c: Campaign, remote: str, local: Path, child: subprocess.Popen[str]) -> str:
        destination = local / "listening.txt"
        for _ in range(30):
            result = subprocess.run([self.ssh, "-i", str(self.ssh_key), f"orrery@{c.host}", "cat", f"{remote}/listening.txt"], text=True, capture_output=True)
            if result.returncode == 0 and result.stdout.strip():
                self.atomic_bytes(destination, result.stdout.encode()); return result.stdout.strip()
            if child.poll() is not None: break
            time.sleep(1)
        child.kill()
        raise Refusal(503, "host_failed", "The host could not start your session — tell the operator, nothing you did was wrong.")

    def _reap(self, ident: str, c: Campaign, sid: str, remote: str, local: Path, child: subprocess.Popen[str]) -> None:
        try:
            child.wait()
            result = subprocess.run([self.ssh, "-i", str(self.ssh_key), f"orrery@{c.host}", "cat", f"{remote}/raw.json"], capture_output=True)
            if result.returncode == 0:
                try: self.atomic_bytes(local / "raw.json", result.stdout)
                except OSError: logging.exception("could not store raw report for %s", sid)
            else: logging.error("could not pull raw report for %s", sid)
        finally: self.children.pop(ident, None)
        lock = self.locks.pop(ident, None)
        if lock is not None: lock.close()

    def known_session(self, sid: str) -> bool:
        if not SESSION.fullmatch(sid): return False
        for joins in self.state.glob("*/joins.jsonl"):
            try:
                if any(json.loads(line).get("session_id") == sid for line in joins.read_text().splitlines()): return True
            except (OSError, json.JSONDecodeError): continue
        return False

    def campaign_can_stand_down(self, ident: str) -> bool:
        """The campaign teardown gate: no admitted report may be left remote-only."""
        joins = self.state / ident / "joins.jsonl"
        if not joins.exists(): return True
        try: ids = [json.loads(line)["session_id"] for line in joins.read_text().splitlines()]
        except (OSError, json.JSONDecodeError, KeyError): return False
        return all((self.state / "sessions" / sid / "raw.json").is_file() for sid in ids)

    @staticmethod
    def atomic_bytes(path: Path, data: bytes) -> None:
        temp = path.with_name(path.name + ".tmp")
        with temp.open("wb") as f: f.write(data); f.flush(); os.fsync(f.fileno())
        os.replace(temp, path)
        fd = os.open(path.parent, os.O_DIRECTORY); os.fsync(fd); os.close(fd)

    def upload(self, sid: str, body: bytes) -> None:
        if not self.known_session(sid): raise Refusal(404, "unknown_session", "That session is not known to this service.")
        if len(body) > MAX_UPLOAD_BYTES: raise Refusal(413, "too_large", "The upload is too large.")
        try: payload = json.loads(body)
        except (UnicodeDecodeError, json.JSONDecodeError) as e: raise Refusal(422, "bad_upload", "The upload is not valid JSON.") from e
        records = payload.get("records")
        if not isinstance(records, list) or any(not isinstance(r, dict) or r.get("session_id") != sid for r in records): raise Refusal(422, "wrong_session", "Every uploaded row must name this session.")
        telemetry = payload.get("telemetry_jsonl")
        if not isinstance(telemetry, str): raise Refusal(422, "bad_upload", "Telemetry must be text.")
        target = self.state / "sessions" / sid; target.mkdir(parents=True, exist_ok=True)
        files = {target / "client-records.jsonl": ("\n".join(json.dumps(r, separators=(",", ":")) for r in records) + ("\n" if records else "")).encode(), target / "telemetry.jsonl": telemetry.encode()}
        for path, data in files.items():
            if path.exists() and path.read_bytes() != data: raise Refusal(409, "conflict", "A different upload already exists for this session.")
        for path, data in files.items():
            if not path.exists(): self.atomic_bytes(path, data)


class Handler(BaseHTTPRequestHandler):
    service: Admission
    def log_message(self, format: str, *args: Any) -> None: logging.info(format, *args)
    def send_json(self, status: int, value: Any | None = None) -> None:
        data = b"" if value is None else json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def failure(self, e: Refusal) -> None: self.send_json(e.status, {"error": e.error, "detail": e.detail, **e.extra})
    def read_json(self) -> tuple[dict[str, Any], bytes]:
        size = int(self.headers.get("Content-Length", "0")); body = self.rfile.read(size)
        return json.loads(body), body
    def do_GET(self) -> None:
        if self.path == "/v1/campaigns": self.send_json(200, self.service.listing())
        else: self.failure(Refusal(404, "not_found", "No such endpoint."))
    def do_POST(self) -> None:
        path = urlparse(self.path).path; match = re.fullmatch(r"/v1/campaigns/([^/]+)/join", path)
        upload = re.fullmatch(r"/v1/sessions/([^/]+)/upload", path)
        try:
            if match: self.send_json(200, self.service.join(unquote(match.group(1)), self.read_json()[0]))
            elif upload:
                _, body = self.read_json(); self.service.upload(unquote(upload.group(1)), body); self.send_json(204)
            else: raise Refusal(404, "not_found", "No such endpoint.")
        except Refusal as e: self.failure(e)
        except (ValueError, json.JSONDecodeError): self.failure(Refusal(422, "bad_request", "The request is not valid JSON."))


def main() -> None:
    p = argparse.ArgumentParser(); p.add_argument("--control", type=Path, default=Path("/etc/orrery/campaigns.conf")); p.add_argument("--state", type=Path, default=Path("/var/lib/orrery-admission")); p.add_argument("--invite", default="orrery-invite"); p.add_argument("--ssh", default="ssh"); p.add_argument("--ssh-key", type=Path, default=Path("/var/lib/orrery-admission/campaign_ssh_key")); p.add_argument("--issuer", type=Path, default=Path("/var/lib/orrery-admission/issuer.cred")); p.add_argument("--swarm", default="p1-swarm"); p.add_argument("--listen", default="127.0.0.1:8323"); p.add_argument("--self-test", action="store_true"); a = p.parse_args()
    if a.self_test: unittest.main(argv=[sys.argv[0]] + ([os.environ["ADMISSION_TEST"]] if "ADMISSION_TEST" in os.environ else [])); return
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    host, port = a.listen.rsplit(":", 1); Handler.service = Admission(a.control, a.state, a.invite, a.ssh, a.ssh_key, a.issuer, a.swarm)
    ThreadingHTTPServer((host, int(port)), Handler).serve_forever()


class AdmissionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(); root = Path(self.tmp.name); self.state, self.control = root / "state", root / "campaigns.conf"
        self.control.write_text("[test]\ntitle = Test\nopen = yes\nhost = test\npeers = 8\nseconds = 60\nloss_pct = 3\njitter_ms = 100\nclient_rev = rev\n")
        repo = Path(__file__).parents[1]; self.invite = repo / "target/debug/orrery-invite"; issuer_key = repo / "target/debug/orrery-issuer-key"
        if not self.invite.exists() or not issuer_key.exists(): self.skipTest("build orrery-invite and orrery-issuer-key before self-test")
        self.issuer = root / "issuer"; subprocess.run([str(issuer_key), "generate", "--key-id", "476", "--output", str(self.issuer)], check=True, capture_output=True)
        self.ssh = root / "ssh"; self.ssh.write_text("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/ssh.args\"\ncase \" $* \" in *' cat '*listening.txt*) echo 'f2a1 127.0.0.1:52011';; *' cat '*raw.json*) echo '{}';; *) sleep 60;; esac\n"); self.ssh.chmod(0o755)
        self.service = Admission(self.control, self.state, str(self.invite), str(self.ssh), root / "key", self.issuer, "swarm", lambda _: type("V", (), {"f_bavail": 20 * 1024**3, "f_frsize": 1})())
    def tearDown(self) -> None:
        for child in self.service.children.values():
            child.kill(); child.wait()
        time.sleep(0.02)
        for lock in self.service.locks.values(): lock.close()
        self.tmp.cleanup()
    def request(self) -> dict[str, str]: return {"nickname": "ada", "node": "a" * 64, "client_rev": "rev"}
    def test_a_busy_campaign_refuses_the_second_join_with_409(self) -> None:
        self.service.join("test", self.request())
        with self.assertRaisesRegex(Refusal, ""): self.service.join("test", self.request())
        try: self.service.join("test", self.request())
        except Refusal as e: self.assertEqual((e.status, e.error), (409, "campaign_busy"))
    def test_the_remote_session_directory_is_created_before_the_harness_runs(self) -> None:
        # The harness writes its report and listening file into a directory on
        # the campaign host that nothing else creates. Without the mkdir it
        # binds, prints its node, and dies on "cannot write the exterior
        # listening file" — which surfaces to a volunteer as host_failed and to
        # the operator as nothing at all. Found by standing the service up on a
        # real box, not by this suite (#488).
        answer = self.service.join("test", self.request()); sid = answer["join"]["session_id"]
        args = self.ssh.parent / "ssh.args"
        for _ in range(20):
            if args.exists() and "--require-session" in args.read_text(): break
            time.sleep(0.01)
        harness = next(line for line in args.read_text().splitlines() if "--require-session" in line)
        fields = shlex.split(harness)
        remote = f"/var/tmp/orrery/{sid}"
        self.assertIn("mkdir", fields, f"the harness launch does not create {remote}: {harness}")
        self.assertLess(fields.index("mkdir"), fields.index("--external-peer"),
                        f"mkdir must precede the harness: {harness}")
        self.assertIn(remote, fields)

    def test_the_harness_is_pinned_to_exactly_the_admitted_session_id(self) -> None:
        answer = self.service.join("test", self.request()); sid = answer["join"]["session_id"]
        args = self.ssh.parent / "ssh.args"
        for _ in range(20):
            if args.exists() and "--require-session" in args.read_text(): break
            time.sleep(0.01)
        harness = next(line for line in args.read_text().splitlines() if "--require-session" in line)
        fields = shlex.split(harness)
        self.assertEqual(fields[fields.index("--require-session") + 1], sid)
    def test_the_token_binds_the_presented_node_not_the_slot(self) -> None:
        token = self.service.join("test", self.request())["join"]["session_token"]
        self.assertGreater(len(token), 100, "the real signer returned a SessionTokenV1")
    def test_an_upload_for_an_unadmitted_session_refuses_404(self) -> None:
        with self.assertRaises(Refusal) as x: self.service.upload("018f8f4e-5c90-7abc-8123-0000000000ab", b"{}")
        self.assertEqual(x.exception.status, 404)
    def test_hostile_ids_never_escape_the_sessions_directory(self) -> None:
        with self.assertRaises(Refusal): self.service.join("../x", self.request())
        with self.assertRaises(Refusal): self.service.upload("../x", b"{}")
        self.assertFalse((self.state.parent / "x").exists())
    def test_a_tab_bearing_nickname_refuses_422_before_minting(self) -> None:
        r = self.request(); r["nickname"] = "a\tb"
        with self.assertRaises(Refusal) as x: self.service.join("test", r)
        self.assertEqual(x.exception.status, 422); self.assertFalse((self.state / "test" / "ledger.tsv").exists())
    def test_a_broken_control_file_keeps_serving_the_previous_parse(self) -> None:
        self.service.listing(); self.control.write_text("[broken\n")
        self.assertEqual(self.service.listing()["campaigns"][0]["id"], "test")
    def test_a_conflicting_reupload_refuses_409_and_leaves_the_first_bytes(self) -> None:
        sid = self.service.join("test", self.request())["join"]["session_id"]; one = json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "one"}).encode(); self.service.upload(sid, one)
        with self.assertRaises(Refusal) as x: self.service.upload(sid, json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "two"}).encode())
        self.assertEqual(x.exception.status, 409); self.assertEqual((self.state / "sessions" / sid / "telemetry.jsonl").read_text(), "one")
    def test_a_row_naming_another_session_refuses_422_at_the_service(self) -> None:
        # The `wrong_session` guard was unpinned: removing it left all twelve
        # tests green, because the only cross-session coverage exercised
        # `assemble` downstream. Assembly refusing is not a substitute for the
        # service refusing — a row filed under the wrong session is evidence
        # sitting in the wrong directory, and nothing upstream would say so.
        sid = self.service.join("test", self.request())["join"]["session_id"]
        other = "018f8f4e-5c90-7abc-8123-0000000000ab"
        self.assertNotEqual(sid, other)
        body = json.dumps({"records": [{"session_id": other}], "telemetry_jsonl": "x"}).encode()
        with self.assertRaises(Refusal) as x: self.service.upload(sid, body)
        self.assertEqual(x.exception.error, "wrong_session")
        self.assertFalse((self.state / "sessions" / sid / "telemetry.jsonl").exists())
    def test_the_mint_floor_refuses_new_admissions_below_10gb(self) -> None:
        self.service.statvfs = lambda _: type("V", (), {"f_bavail": MINT_FLOOR_BYTES - 1, "f_frsize": 1})()
        with self.assertRaises(Refusal) as x: self.service.join("test", self.request())
        self.assertEqual(x.exception.error, "admissions_paused"); self.assertEqual(self.service.listing()["campaigns"][0]["state"], "paused")
    def test_an_admitted_session_still_uploads_below_the_floor(self) -> None:
        sid = self.service.join("test", self.request())["join"]["session_id"]; self.service.statvfs = lambda _: type("V", (), {"f_bavail": 0, "f_frsize": 1})()
        self.service.upload(sid, json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "x"}).encode())
    def test_a_campaign_server_never_dies_with_an_unpulled_report(self) -> None:
        sid = self.service.join("test", self.request())["join"]["session_id"]
        self.assertFalse(self.service.campaign_can_stand_down("test"))
        self.service.atomic_bytes(self.state / "sessions" / sid / "raw.json", b"{}")
        self.assertTrue(self.service.campaign_can_stand_down("test"))
    def test_an_uploaded_row_for_another_session_still_refuses_assembly(self) -> None:
        # Exercise the real assembler: it must reject a records file with no matching row.
        script = Path(__file__).parents[1] / "scripts" / "p4-campaign-session.sh"; sid = "018f8f4e-5c90-7abc-8123-0000000000aa"; raw = Path(self.tmp.name) / "raw.json"; records = Path(self.tmp.name) / "records.jsonl"
        raw.write_text('{"external":{},"witnessing":true,"identity":{"target":"x","commit":"0000000000000000000000000000000000000000"}}')
        records.write_text('{"session_id":"018f8f4e-5c90-7abc-8123-0000000000ab","actor":"human","platform_triple":"x","impairment_mismatch":false,"configured_impairment_profile":{"loss_pct":0,"jitter_p50_ms":0,"jitter_p99_ms":0},"observed_loss_pct":0,"observed_jitter_p50_ms":0,"observed_jitter_p99_ms":0}\n')
        self.assertNotEqual(subprocess.run([str(script), "assemble", str(raw), str(records), sid, str(Path(self.tmp.name) / "out.json")], env={**os.environ, "P4_PIPELINE_ID": "test"}).returncode, 0)


if __name__ == "__main__": main()
