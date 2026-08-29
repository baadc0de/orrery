#!/usr/bin/env python3
"""Supervise the P1 harness as a standing campaign host.

The supervisor remains the lifetime boundary: it waits for each finite child,
gives every attempt its own report directory, and forwards shutdown before
starting the next generation. Admission and this harness are deliberately
co-located on hel1 so the child can read admission's authoritative slots.json;
that shared-storage assumption is not guaranteed by the protocol and every
journal failure is a fail-closed join refusal.
"""
from __future__ import annotations

import argparse
import configparser
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

LOBBY_SECONDS = 180
CHILD_EXIT_GRACE_SECONDS = 30
# `CAMPAIGN_LOBBY_HOLD` in clients/regolith/src/lib.rs: the longest lobby a
# shipped client will sit through. The host answers a join only when the lobby
# closes, so a longer lobby is a campaign nobody can join.
CLIENT_LOBBY_HOLD_SECONDS = 180


def lobby_seconds(campaign_config: dict[str, str]) -> int:
    """Configured initial cohort delay, retaining 180 seconds as the default."""
    value = int(campaign_config.get("lobby_seconds", str(LOBBY_SECONDS)))
    if value < 0:
        raise ValueError("lobby_seconds must not be negative")
    # A lobby the shipped client cannot outwait is not a slow campaign, it is a
    # campaign nobody can join: admission accepts the seat, then every client
    # that arrives early fails with `handshake read timed out`. That is exactly
    # how the 180-second lobby broke the standing campaign against a client
    # built for a 90-second freeze, so refuse it here rather than discover it
    # from a playtester.
    if value > CLIENT_LOBBY_HOLD_SECONDS:
        raise ValueError(
            f"lobby_seconds {value} exceeds {CLIENT_LOBBY_HOLD_SECONDS}, "
            "the shipped client's join patience"
        )
    return value


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
        active_seats = self.args.state / "active-seats.json"
        return [self.args.swarm, "--external-peer", "--external-bind", f"{bind_host}:{c['external_port']}",
                "--peers", c["peers"], "--external-slots", c.get("humans", "1"),
                "--lobby-seconds", str(lobby_seconds(c)), "--seconds", c["seconds"], "--min-cells", "1",
                "--impaired", "--witness", "--stamp-wall-clock", "--json", str(attempt / "raw.json"),
                # Every directed seat pair, with a reason code (#612). The owner
                # chose the log volume -- roughly 3,360 lines/min at eight seats
                # against 1,060 without -- over being unable to classify the next
                # "someone is shooting me and I cannot see them" report. That
                # question has already cost one investigation, which concluded
                # on evidence that turned out to be a logging-order artifact.
                "--replica-scope-capture",
                # #653. At v18's 480 m/s interceptor ceiling a craft clears the
                # 460.8 m one-body AOI guarantee in 0.96 s -- inside one 1 Hz
                # interest refresh -- so a human can be shot by someone their
                # roster has not heard of yet. This turns on both halves: the
                # swept margin that covers where a craft can reach, and the
                # ordered crossing event that corrects the roster on the
                # commitment rather than at the next refresh.
                "--swept-interest-margin",
                "--listening-file", str(listening), "--active-seats-file", str(active_seats),
                "--issuer-key", issuer_key(self.args.issuer_key),
                "--reservation-journal", str(self.args.reservation_journal),
                "--attempt-id", attempt.name] + (
                ["--require-client-rev", c["client_rev"]] if c.get("client_rev") else [])

    def write_attempt(self, attempt_id: str, started: int, seconds: int,
                      initial_delay: int) -> dict[str, int | str]:
        record: dict[str, int | str] = {
            "attempt_id": attempt_id,
            "started": started,
            "expires_at": started + initial_delay + seconds,
        }
        temporary = self.args.state / "attempt.json.tmp"
        with temporary.open("w", encoding="utf-8") as f:
            json.dump(record, f, separators=(",", ":")); f.flush(); os.fsync(f.fileno())
        os.replace(temporary, self.args.state / "attempt.json")
        return record

    def has_reservations(self, attempt_id: str) -> bool | None:
        """Return whether this generation has an active or arriving player."""
        try:
            rows = json.loads(self.args.reservation_journal.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return False
        except (OSError, json.JSONDecodeError):
            return None
        if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
            return None
        now = int(time.time())
        active_slots: set[int] = set()
        try:
            active = json.loads((self.args.state / "active-seats.json").read_text(encoding="utf-8"))
            if active.get("attempt_id") != attempt_id or not isinstance(active.get("active_slots"), list):
                return None
            active_slots = set(active["active_slots"])
        except FileNotFoundError:
            pass
        except (OSError, json.JSONDecodeError, AttributeError, TypeError):
            return None
        return any(row.get("attempt_id") == attempt_id
                   and (row.get("slot") in active_slots
                        or (isinstance(row.get("expires_at"), int) and row["expires_at"] > now))
                   for row in rows)

    def host_running(self, attempt_id: str) -> bool | None:
        """Read the generation-bound transition from cohort formation to run."""
        try:
            active = json.loads((self.args.state / "active-seats.json").read_text(encoding="utf-8"))
        except FileNotFoundError:
            return False
        except (OSError, json.JSONDecodeError):
            return None
        if (not isinstance(active, dict) or active.get("attempt_id") != attempt_id
                or not isinstance(active.get("running"), bool)):
            return None
        return active["running"]

    def run(self) -> int:
        c = campaign(self.args.control, self.args.campaign)
        initial_delay = lobby_seconds(c)
        self.args.state.mkdir(parents=True, exist_ok=True)
        signal.signal(signal.SIGTERM, self.stop)
        signal.signal(signal.SIGINT, self.stop)
        attempts = 0
        while not self.stopping:
            attempts += 1
            attempt = self.args.state / f"attempt-{time.time_ns()}-{attempts}"
            attempt.mkdir(mode=0o750)
            started = int(time.time())
            # Never publish a new generation beside the previous child's
            # listening address.
            (self.args.state / "listening.txt").unlink(missing_ok=True)
            # The host republishes this after every bind and release. It is
            # generation-bound, but removing it also makes a new lobby's
            # lack of connections explicit to admission.
            (self.args.state / "active-seats.json").unlink(missing_ok=True)
            # Admission uses this generation as its single lease boundary.  It
            # is deliberately beside listening.txt, which it already reads via
            # the co-located standing-host path. The active lease includes the
            # lobby and the finite simulation, not only the latter.
            record = self.write_attempt(attempt.name, started, int(c["seconds"]), initial_delay)
            self.child = subprocess.Popen(self.command(attempt, c))
            running = False
            while self.child.poll() is None and not self.stopping:
                now = int(time.time())
                host_running = self.host_running(attempt.name)
                if host_running is True and not running:
                    # The host, not process spawn or the first HTTP reservation,
                    # knows when the initial cohort actually started. Reset the
                    # child boundary to one full simulation from that fact.
                    running = True
                    record = self.write_attempt(attempt.name, now, int(c["seconds"]), 0)
                if not running and now >= int(record["started"]) + max(initial_delay, 1):
                    reservations = self.has_reservations(attempt.name)
                    if reservations is False:
                        # The child has reopened the same empty standing lobby.
                        # Advance the lease clock without changing generation;
                        # there are no rows whose expiry could now disagree.
                        record = self.write_attempt(attempt.name, now, int(c["seconds"]), initial_delay)
                    elif now >= int(record["expires_at"]) + CHILD_EXIT_GRACE_SECONDS:
                        # A reservation that never became a connection, or an
                        # unreadable journal, must not pin this generation
                        # forever. Normal finite attempts exit before this
                        # grace expires.
                        self.child.terminate()
                elif running and now >= int(record["expires_at"]) + CHILD_EXIT_GRACE_SECONDS:
                    # An empty live membership is ordinary drop-in/drop-out,
                    # never a reason to renew the finite running attempt.
                    self.child.terminate()
                time.sleep(0.1)
            status = self.child.wait()
            self.child = None
            if self.stopping:
                return 0
            if self.args.max_runs and attempts >= self.args.max_runs:
                return status
            # Between children there is no generation to reserve against.  In
            # particular, do not leave the prior attempt's future expiry
            # visible during the restart delay.
            (self.args.state / "attempt.json").unlink(missing_ok=True)
            time.sleep(self.args.restart_delay)
        return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--control", type=Path)
    parser.add_argument("--campaign")
    parser.add_argument("--swarm")
    parser.add_argument("--issuer-key", type=Path)
    parser.add_argument("--state", type=Path)
    parser.add_argument("--reservation-journal", type=Path)
    parser.add_argument("--restart-delay", type=float, default=5.0)
    parser.add_argument("--max-runs", type=int, default=0, help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


class Tests(unittest.TestCase):
    def test_a_lobby_longer_than_the_client_can_outwait_is_refused(self) -> None:
        budget = CLIENT_LOBBY_HOLD_SECONDS
        # The shipped default must be joinable, or the standing campaign is
        # open to reservations nobody can complete.
        self.assertLessEqual(LOBBY_SECONDS, budget)
        self.assertEqual(lobby_seconds({"lobby_seconds": str(budget)}), budget)
        with self.assertRaises(ValueError) as refused:
            lobby_seconds({"lobby_seconds": str(budget + 1)})
        self.assertIn("join patience", str(refused.exception))

    def test_command_passes_shared_journal_attempt_and_lobby_shape(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            issuer = root / "issuer.pub"
            issuer.write_text("41:" + "a" * 64)
            args = argparse.Namespace(swarm="p1-swarm", issuer_key=issuer,
                                      state=root / "state", reservation_journal=root / "slots.json")
            command = Supervisor(args).command(root / "attempt-7", {
                "host": "203.0.113.7", "external_port": "41641", "peers": "4",
                "humans": "4", "seconds": "900",
            })
            self.assertEqual(command[command.index("--external-slots") + 1], "4")
            self.assertEqual(command[command.index("--lobby-seconds") + 1], str(LOBBY_SECONDS))
            self.assertEqual(command[command.index("--reservation-journal") + 1], str(root / "slots.json"))
            self.assertEqual(command[command.index("--attempt-id") + 1], "attempt-7")
            self.assertEqual(command[command.index("--active-seats-file") + 1], str(root / "state" / "active-seats.json"))
            # The standing host must always capture scope; a live report we
            # cannot classify is the expensive outcome, not the log volume.
            self.assertIn("--replica-scope-capture", command)
            # #653 must reach the live host, not only the gate harness.
            self.assertIn("--swept-interest-margin", command)

            configured = Supervisor(args).command(root / "attempt-8", {
                "host": "203.0.113.7", "external_port": "41641", "peers": "4",
                "humans": "4", "seconds": "900", "lobby_seconds": "12",
            })
            self.assertEqual(configured[configured.index("--lobby-seconds") + 1], "12",
                             "the campaign's cohort delay must not be hard-coded")

    def test_only_live_or_unexpired_arrivals_hold_the_generation_lease(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            state = root / "state"
            state.mkdir()
            journal = root / "slots.json"
            args = argparse.Namespace(state=state, reservation_journal=journal)
            supervisor = Supervisor(args)
            now = int(time.time())
            journal.write_text(json.dumps([
                {"attempt_id": "attempt-7", "slot": 4, "expires_at": now - 1},
            ]))
            self.assertFalse(supervisor.has_reservations("attempt-7"),
                             "an expired no-show must not pin the attempt")
            (state / "active-seats.json").write_text(json.dumps({
                "attempt_id": "attempt-7", "active_slots": [4], "released_sessions": [],
                "running": False,
            }))
            self.assertTrue(supervisor.has_reservations("attempt-7"),
                            "a bound player remains live after the arrival lease")

    def test_running_empty_membership_is_not_an_idle_lobby(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            state = root / "state"
            state.mkdir()
            args = argparse.Namespace(state=state, reservation_journal=root / "slots.json")
            (state / "active-seats.json").write_text(json.dumps({
                "attempt_id": "attempt-7", "active_slots": [],
                "released_sessions": [], "running": True,
            }))
            self.assertTrue(Supervisor(args).host_running("attempt-7"),
                            "all players leaving must not turn a running child into an idle lobby")

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
                                      state=state, reservation_journal=root / "slots.json",
                                      restart_delay=0, max_runs=2)
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
            record = json.loads((state / "attempt.json").read_text())
            self.assertEqual(record["attempt_id"], attempts[-1].name)
            self.assertEqual(record["expires_at"] - record["started"], LOBBY_SECONDS + 900)

    def test_idle_child_renews_attempt_lease_without_restarting(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            control, issuer, state = root / "campaigns.conf", root / "issuer.pub", root / "state"
            control.write_text("[test]\nhost = 203.0.113.7\nexternal_port = 41641\npeers = 8\nseconds = 4\nloss_pct = 3\njitter_ms = 100\nalways_on = yes\n")
            issuer.write_text("41:" + "a" * 64)
            wrapper = root / "wrapper.py"
            wrapper.write_text("#!/usr/bin/env python3\nimport time\ntime.sleep(2.2)\n")
            args = argparse.Namespace(control=control, campaign="test", swarm=str(wrapper),
                                      issuer_key=issuer, state=state,
                                      reservation_journal=root / "slots.json",
                                      restart_delay=0, max_runs=1)
            original_command = Supervisor.command
            original_lobby = globals()["LOBBY_SECONDS"]
            Supervisor.command = lambda self, attempt, c: [sys.executable, str(wrapper)]
            globals()["LOBBY_SECONDS"] = 1
            try:
                self.assertEqual(Supervisor(args).run(), 0)
            finally:
                globals()["LOBBY_SECONDS"] = original_lobby
                Supervisor.command = original_command
            attempts = list(state.glob("attempt-*"))
            self.assertEqual(len(attempts), 1, "an idle lobby must not respawn the child")
            record = json.loads((state / "attempt.json").read_text())
            self.assertGreater(record["started"], int(time.time()) - 2,
                               "the idle generation's lease clock was not renewed")
            self.assertEqual(record["expires_at"] - record["started"], 5)

    def test_running_child_does_not_renew_when_every_player_leaves(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            control, issuer, state = root / "campaigns.conf", root / "issuer.pub", root / "state"
            control.write_text("[test]\nhost = 203.0.113.7\nexternal_port = 41641\npeers = 8\nseconds = 4\nloss_pct = 3\njitter_ms = 100\nalways_on = yes\nlobby_seconds = 1\n")
            issuer.write_text("41:" + "a" * 64)
            wrapper = root / "wrapper.py"
            wrapper.write_text(
                "#!/usr/bin/env python3\n"
                "import json, pathlib, sys, time\n"
                "pathlib.Path(sys.argv[1]).write_text(json.dumps({'attempt_id': sys.argv[2], 'active_slots': [], 'released_sessions': [], 'running': True}))\n"
                "time.sleep(2.2)\n"
            )
            args = argparse.Namespace(control=control, campaign="test", swarm=str(wrapper),
                                      issuer_key=issuer, state=state,
                                      reservation_journal=root / "slots.json",
                                      restart_delay=0, max_runs=1)
            original_command = Supervisor.command
            Supervisor.command = lambda self, attempt, c: [
                sys.executable, str(wrapper), str(state / "active-seats.json"), attempt.name,
            ]
            try:
                self.assertEqual(Supervisor(args).run(), 0)
            finally:
                Supervisor.command = original_command
            record = json.loads((state / "attempt.json").read_text())
            self.assertEqual(record["expires_at"] - record["started"], 4,
                             "a running empty campaign keeps its finite run boundary, not an idle-lobby renewal")


def main() -> None:
    args = parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]])
    else:
        if None in (args.control, args.campaign, args.swarm, args.issuer_key, args.state,
                    args.reservation_journal):
            raise SystemExit("--control, --campaign, --swarm, --issuer-key, --state, and --reservation-journal are required")
        raise SystemExit(Supervisor(args).run())


if __name__ == "__main__":
    main()
