#!/usr/bin/env python3
"""Supervise the one-external-peer P1 harness as a standing campaign host.

The harness deliberately ends after one peer and one configured session.  This
supervisor is therefore the lifetime boundary: it waits for that child before
starting a fresh process, gives every attempt its own report directory, and
forwards shutdown to the child before exiting.
"""
from __future__ import annotations

import argparse
import configparser
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path


def campaign(control: Path, ident: str) -> dict[str, str]:
    parser = configparser.ConfigParser(interpolation=None)
    with control.open(encoding="utf-8") as source:
        parser.read_file(source)
    if ident not in parser or parser[ident].get("always_on", "").lower() != "yes":
        raise ValueError(f"{ident!r} is not an always_on campaign")
    required = ("host", "external_port", "peers", "seconds", "loss_pct", "jitter_ms")
    missing = [key for key in required if not parser[ident].get(key)]
    if missing:
        raise ValueError(f"{ident!r} is missing " + ", ".join(missing))
    return dict(parser[ident])


def issuer_key(path: Path) -> str:
    value = path.read_text(encoding="utf-8").strip()
    if not value or "\n" in value or ":" not in value:
        raise ValueError(f"{path}: expected one <key_id>:<public_key_hex> line")
    return value


class Supervisor:
    def __init__(self, args: argparse.Namespace):
        self.args, self.child = args, None
        self.stopping = False

    def stop(self, _signum: int, _frame: object) -> None:
        self.stopping = True
        if self.child is not None and self.child.poll() is None:
            self.child.terminate()

    def command(self, attempt: Path, c: dict[str, str]) -> list[str]:
        bind_host = "::" if ":" in c["host"] else "0.0.0.0"
        listening = self.args.state / "listening.txt"
        return [self.args.swarm, "--external-peer", "--external-bind", f"{bind_host}:{c['external_port']}",
                "--peers", c["peers"], "--seconds", c["seconds"], "--min-cells", "1",
                "--impaired", "--witness", "--stamp-wall-clock", "--json", str(attempt / "raw.json"),
                "--listening-file", str(listening), "--issuer-key", issuer_key(self.args.issuer_key)] + (
                ["--require-client-rev", c["client_rev"]] if c.get("client_rev") else [])

    def run(self) -> int:
        c = campaign(self.args.control, self.args.campaign)
        self.args.state.mkdir(parents=True, exist_ok=True)
        signal.signal(signal.SIGTERM, self.stop)
        signal.signal(signal.SIGINT, self.stop)
        attempts = 0
        while not self.stopping:
            attempts += 1
            attempt = self.args.state / f"attempt-{time.time_ns()}-{attempts}"
            attempt.mkdir(mode=0o750)
            # A stale record could point a volunteer at a process that has
            # exited.  Remove it before spawning; the child writes replacement
            # only after its socket is bound.
            (self.args.state / "listening.txt").unlink(missing_ok=True)
            self.child = subprocess.Popen(self.command(attempt, c))
            status = self.child.wait()
            self.child = None
            if self.stopping:
                return 0
            if self.args.max_runs and attempts >= self.args.max_runs:
                return status
            time.sleep(self.args.restart_delay)
        return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--control", type=Path)
    parser.add_argument("--campaign")
    parser.add_argument("--swarm")
    parser.add_argument("--issuer-key", type=Path)
    parser.add_argument("--state", type=Path)
    parser.add_argument("--restart-delay", type=float, default=5.0)
    parser.add_argument("--max-runs", type=int, default=0, help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


class Tests(unittest.TestCase):
    def test_failed_child_is_reaped_then_a_fresh_attempt_starts(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            control, issuer, state = root / "campaigns.conf", root / "issuer.pub", root / "state"
            control.write_text("[test]\nhost = 203.0.113.7\nexternal_port = 41641\npeers = 8\nseconds = 900\nloss_pct = 3\njitter_ms = 100\nalways_on = yes\n")
            issuer.write_text("41:" + "a" * 64)
            # A separate tiny wrapper gives each child an observable exit code.
            wrapper = root / "wrapper.py"
            wrapper.write_text("#!/usr/bin/env python3\nimport pathlib, sys\ncount=pathlib.Path(sys.argv[1]); n=int(count.read_text() or '0')+1 if count.exists() else 1; count.write_text(str(n)); raise SystemExit(1 if n == 1 else 0)\n")
            count = root / "count"
            args = argparse.Namespace(control=control, campaign="test", swarm=str(wrapper), issuer_key=issuer,
                                      state=state, restart_delay=0, max_runs=2)
            # Add a harmless positional argument the wrapper uses; command() is
            # intentionally the real production command, so use a symlink-like
            # launcher which ignores all harness flags and records invocations.
            original = Supervisor.command
            def command(self: Supervisor, attempt: Path, c: dict[str, str]) -> list[str]:
                return [sys.executable, str(wrapper), str(count)]
            Supervisor.command = command
            try:
                self.assertEqual(Supervisor(args).run(), 0)
            finally:
                Supervisor.command = original
            self.assertEqual(count.read_text(), "2", "failure must start a second child")
            attempts = sorted(state.glob("attempt-*"))
            self.assertEqual(len(attempts), 2, "each child needs an isolated report directory")


def main() -> None:
    args = parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]])
    else:
        if None in (args.control, args.campaign, args.swarm, args.issuer_key, args.state):
            raise SystemExit("--control, --campaign, --swarm, --issuer-key, and --state are required")
        raise SystemExit(Supervisor(args).run())


if __name__ == "__main__":
    main()
