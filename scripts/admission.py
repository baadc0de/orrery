#!/usr/bin/env python3
"""Campaign admission service (#476).

This is deliberately only a box office: it invokes ``orrery-invite`` for
allocation and signing, starts the harness, and files reports.  In particular,
there is no SessionTokenV1 encoder or ledger append path in this file.
"""
from __future__ import annotations

import argparse
import configparser
import errno
import gc
import fcntl
import http.client
import ipaddress
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
from typing import Any, Callable
from urllib.parse import unquote, urlparse

MINT_FLOOR_BYTES = 10 * 1024**3
MAX_UPLOAD_BYTES = 64 * 1024**2
CAMPAIGN_ID = re.compile(r"[a-z0-9-]{1,64}\Z")
NODE = re.compile(r"[0-9a-f]{64}\Z")
SESSION = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\Z")
DISPLAY_LABEL_MAX_CHARS = 32
LOBBY_SECONDS = 180
ARRIVAL_LEASE_SECONDS = 45
# How long after the host loses a lobby peer its reservation stays reclaimable
# by the *same* transport identity (#1001).  It is the arrival lease, and
# deliberately the same number: both answer one question -- how long may a seat
# be held for somebody who is not sitting in it? -- and a volunteer whose
# connection lapsed is arriving, not playing.
#
# It has to be longer than the client can take to find out.  The host beats the
# lobby every 2 s, the client waits 8 s of silence before it says the lobby is
# lost, and QUIC's own `EXTERIOR_MAX_IDLE_TIMEOUT` kills the connection at 10 s,
# so the volunteer learns between 8 and 10 seconds after the lapse.  Anything
# under about 15 s would expire while they are still reading the notice.
#
# And it has to be shorter than the lobby it holds a seat inside: the lobby runs
# `lobby_seconds` (180 s by default), so 45 s spends at most a quarter of one
# window holding a seat against everyone else -- and only a seat somebody
# demonstrably held a moment ago.
RECLAIM_GRACE_SECONDS = ARRIVAL_LEASE_SECONDS
RESTART_DELAY_SECONDS = 5
UPLOAD_PROBE_SESSION = "00000000-0000-7000-8000-000000000002"
UPLOAD_PROBE_OTHER_SESSION = "00000000-0000-7000-8000-000000000003"
UPLOAD_PROBE_TIMEOUT_SECONDS = 10
UPLOAD_PROBE_STEP_BYTES = 1024**2


def display_label(raw: str) -> str | None:
    """Return the bounded ASCII text the client is allowed to draw."""
    cleaned = "".join(glyph for glyph in raw if " " <= glyph <= "~").strip()
    return cleaned[:DISPLAY_LABEL_MAX_CHARS] or None


@dataclass(frozen=True)
class UploadLimit:
    """What the startup probe could prove about the public origin's body ceiling (#1002)."""
    status: str  # "ok", "too_small" or "unverifiable"; only too_small fails startup
    verified: int | None  # largest body size proven to reach admission, if any
    detail: str  # why the verdict is what it is, for the operator's log


def probe_body(size: int) -> bytes:
    """An upload-shaped body of exactly `size` bytes that can never be stored.

    The probe session is never minted (a zero-timestamp UUIDv7) and its records
    name another synthetic session, so admission refuses before any write and
    the probe cannot collide with a real upload in either direction.
    """
    head = json.dumps({"records": [{"session_id": UPLOAD_PROBE_OTHER_SESSION}]}, separators=(",", ":"))[:-1] + ',"telemetry_jsonl":"'
    pad = size - len(head) - 2
    if pad < 0: raise ValueError("the probe size cannot hold the upload shape")
    return (head + "x" * pad + '"}').encode()


def post_json(url: str, body: bytes) -> tuple[int, str]:
    """One POST through whatever fronts the public origin: HTTP statuses return, network trouble raises OSError.

    The body goes to the socket before the response is read, so a proxy that
    refuses an oversized body mid-send and closes can still have its 413 read
    out of the socket afterwards; a reset with nothing to read is network
    trouble, not a verdict.
    """
    parts = urlparse(url)
    # The scheme decides the connection class. Dialling an https origin over
    # plain HTTP lands on whatever answers port 80 -- for a TLS-fronted origin
    # that is a 301 to itself, which carries no refusal marker and leaves the
    # check permanently unverifiable. That is exactly how this probe was inert
    # against the only origin it exists to protect (#1002).
    connect = http.client.HTTPSConnection if parts.scheme == "https" else http.client.HTTPConnection
    conn = connect(parts.hostname, parts.port, timeout=UPLOAD_PROBE_TIMEOUT_SECONDS)
    try:
        conn.putrequest("POST", parts.path or "/")
        conn.putheader("Content-Type", "application/json"); conn.putheader("Content-Length", str(len(body))); conn.putheader("Connection", "close")
        try: conn.endheaders(message_body=body)
        except (BrokenPipeError, ConnectionResetError): pass  # the refusal may already be in flight; getresponse reads it
        response = conn.getresponse()
        return response.status, response.read(65536).decode("utf-8", "replace")
    finally:
        conn.close()


def reached_admission(status: int, text: str) -> bool:
    """True only when the probe's whole body demonstrably arrived at this service.

    A proxy that refused the body first answers with an HTML error page or a
    redirect-follower's answer, neither of which is admission's own JSON
    refusal for the probe path. `unknown_session` is the normal marker; `wrong_session`
    also counts, though the probe's shape makes it unreachable in practice.
    """
    try: payload = json.loads(text)
    except (ValueError, json.JSONDecodeError): return False
    return (status in (HTTPStatus.NOT_FOUND, HTTPStatus.UNPROCESSABLE_ENTITY)
            and isinstance(payload, dict) and payload.get("error") in ("unknown_session", "wrong_session"))


def probe_once(url: str, size: int, post: Callable[[str, bytes], tuple[int, str]]) -> bool | None:
    """One probe POST of `size` bytes: True arrived, False was refused for size, None is network trouble."""
    try: status, text = post(url, probe_body(size))
    except (OSError, http.client.HTTPException): return None
    if status == HTTPStatus.REQUEST_ENTITY_TOO_LARGE: return False
    return reached_admission(status, text)


def probe_upload_limit(origin: str, post: Callable[[str, bytes], tuple[int, str]] = post_json) -> UploadLimit:
    """POST a body of exactly MAX_UPLOAD_BYTES to the public origin and read what answers (#1002).

    nginx's 1 MiB default refused every volunteer upload with 413 before
    admission saw it, and #735's refusal logging in `_store_upload` never fired
    for those because the rejection happened one layer up. This checks the
    effective ceiling end to end instead of parsing any proxy's config: a 413
    proves the ceiling is smaller than the application's, and a bounded search
    walks down to name the number an operator must raise. Network trouble or an
    answer without admission's marker returns unverifiable, which is not
    evidence of a small ceiling.
    """
    url = origin.rstrip("/") + "/v1/sessions/" + UPLOAD_PROBE_SESSION + "/upload"
    try: status, text = post(url, probe_body(MAX_UPLOAD_BYTES))
    except (OSError, http.client.HTTPException) as e:
        return UploadLimit("unverifiable", None, f"{type(e).__name__}: {e}")
    if reached_admission(status, text):
        return UploadLimit("ok", MAX_UPLOAD_BYTES, "")
    if status != HTTPStatus.REQUEST_ENTITY_TOO_LARGE:
        return UploadLimit("unverifiable", None, f"HTTP {status} did not carry admission's refusal marker ({text[:120]!r}); is --public-origin the origin clients upload to, without a redirect?")
    floor = probe_once(url, UPLOAD_PROBE_STEP_BYTES, post)
    if floor is None:
        return UploadLimit("too_small", None, "the search for the effective limit lost the network")
    if not floor:
        return UploadLimit("too_small", None, f"even a body of {UPLOAD_PROBE_STEP_BYTES} bytes was refused")
    low, high = UPLOAD_PROBE_STEP_BYTES, MAX_UPLOAD_BYTES
    while high - low > UPLOAD_PROBE_STEP_BYTES:
        mid = (low + high) // 2 // UPLOAD_PROBE_STEP_BYTES * UPLOAD_PROBE_STEP_BYTES
        verdict = probe_once(url, mid, post)
        if verdict is None:
            return UploadLimit("too_small", None, "the search for the effective limit lost the network")
        if verdict: low = mid
        else: high = mid
    return UploadLimit("too_small", low, "")


def enforce_upload_limit(origin: str, post: Callable[[str, bytes], tuple[int, str]] = post_json) -> None:
    """Fail startup when the public origin's body ceiling is smaller than ours.

    `_store_upload` says out loud when it refuses an upload (#735), because
    silence there is indistinguishable from a player who never played, which is
    the blind spot #711 existed to close. A proxy refusing the body one layer
    up (#1002) never reaches that logging, so this closes the same blind spot
    from above: prove at startup that a MAX_UPLOAD_BYTES body can actually
    arrive, and refuse to serve 413-generating silence when it cannot.
    """
    check = probe_upload_limit(origin, post)
    wanted = f"{MAX_UPLOAD_BYTES} bytes ({MAX_UPLOAD_BYTES // 1024**2} MiB)"
    if check.status == "ok":
        logging.info("upload-limit self-check passed: the public origin accepted a probe body of %s", wanted); return
    if check.status == "unverifiable":
        logging.warning("upload-limit self-check cannot verify: %s. Continuing without proof that the public origin accepts a probe body of %s; if it does not, volunteer uploads are refused upstream with HTTP 413 and their sessions never bank (#1002)", check.detail, wanted); return
    got = (f"the largest body it accepted was {check.verified} bytes ({check.verified // 1024**2} MiB)" if check.verified is not None
           else f"no accepted body could be measured ({check.detail})")
    logging.critical("refusing to start: the public origin rejected a probe body of %s with HTTP 413 before admission saw it; %s. Volunteer uploads are being refused upstream of this service and their sessions never bank (#1002). Raise the proxy's request-body limit to at least MAX_UPLOAD_BYTES = %s — for nginx, set `client_max_body_size 64m;` in the site fronting this origin, then reload — and start admission again.", wanted, got, wanted)
    raise SystemExit(1)


class Refusal(Exception):
    def __init__(self, status: int, error: str, detail: str, **extra: Any):
        self.status, self.error, self.detail, self.extra = status, error, detail, extra


# ── The configured impairment profile (#1030) ────────────────────────────────
#
# A campaign config carries one jitter figure, and this file used to send it as
# both `jitter_p50_ms` and `jitter_p99_ms`. No distribution satisfies that, so
# every honest session recorded on 2026-09-04 banked with
# `impairment_mismatch: true` — the exact false positive P4's exit condition
# (#240) counts, arriving on the first real cohort.
#
# What the host actually applies: `Impairment::p4_profile_at_loss` in
# `gates/p1-swarm/src/router.rs` sets `jitter_rate: 0.10` beside
# `jitter_ticks: 6`, and `Router::schedule` there holds a datagram for the whole
# six ticks (100 ms at 60 Hz) or not at all. The added delay is a two-point
# distribution — zero for nine datagrams in ten, one full spike for the tenth —
# so the campaign's `jitter_ms` is the *height of the spike*, not a median.
#
# Its quantiles follow directly: zero below the spike's mass, the spike's full
# height above it. At a 0.10 rate that is p50 = 0 and p99 = jitter_ms. This
# mirrors `ConfiguredImpairment::from_spike` in
# `clients/regolith/src/session.rs`; `--self-test` holds this constant against
# the router's, so the two cannot drift.
#
# The wire schema is untouched: the join answer still carries
# `{loss_pct, jitter_p50_ms, jitter_p99_ms}`, and only the values change. No
# client re-download.
HOST_JITTER_SPIKE_RATE = 0.10


def configured_impairment(c: "Campaign") -> dict[str, Any]:
    """The impairment profile a joining client is told to expect."""
    def quantile(q: float) -> int:
        return c.jitter_ms if q > 1.0 - HOST_JITTER_SPIKE_RATE else 0
    return {"loss_pct": c.loss_pct,
            "jitter_p50_ms": quantile(0.50), "jitter_p99_ms": quantile(0.99)}


@dataclass(frozen=True)
class Campaign:
    ident: str; title: str; open: bool; host: str; peers: int; seconds: int
    loss_pct: int; jitter_ms: int; external_port: int; client_rev: str | None; ruleset_version: int | None
    always_on: bool; humans: int; lobby_seconds: int


@dataclass(frozen=True)
class SeatOccupancy:
    """Why one human seat counts as taken: its row, and whether it is bound."""
    row: dict[str, Any]
    bound: bool


@dataclass(frozen=True)
class StandingHostMembership:
    """One generation's host-authored seat bindings."""
    attempt_id: str
    active_slots: frozenset[int]
    # Seats the host holds a connection for but has not bound: a volunteer
    # waiting in the lobby, or one part-way through a live join handshake.
    #
    # The arrival lease below answers "how long is a seat held for somebody who
    # has not turned up?", and it is 45 s because that is all a reservation
    # proves.  It was the *only* thing holding a lobby seat, and a lobby runs
    # `lobby_seconds` (180 s), so any volunteer who arrived more than 45 s
    # before the run started went invisible here: their row was dropped, their
    # slot re-offered, and the next dialler refused by the host with
    # `reservation_slot_occupied` because the first player was still in it
    # (#1016).  A seat the host is visibly holding is not a seat nobody is
    # arriving to, so the lease is the wrong instrument for it and this is the
    # right one.  Nothing here is a liveness claim -- such a seat draws
    # `reserved`, not `active` -- and a host too old to publish the key simply
    # holds no seats this way, which is the pre-#1016 behaviour.
    pending_slots: frozenset[int]
    released_sessions: frozenset[str]
    running: bool
    # session -> the second the host lost the binding, for the releases it says
    # a redial may still reclaim.  The host contributes only the instant, which
    # is the one fact it alone observed; how long that instant is worth is this
    # service's policy, because this service owns the reservation.  A release
    # with no entry here -- an explicit goodbye, a run-time departure, or any
    # host too old to publish `released_at` -- is spent on sight, exactly as
    # every release was before #1001.
    released_at: dict[str, int]

    @property
    def held_slots(self) -> frozenset[int]:
        """Every seat the host says it has, bound or merely connected."""
        return self.active_slots | self.pending_slots

    def reclaimable(self, session_id: Any, now: int) -> bool:
        """True while only the identity that held this seat may have it back."""
        lost_at = self.released_at.get(session_id) if isinstance(session_id, str) else None
        return lost_at is not None and now < lost_at + RECLAIM_GRACE_SECONDS

    def reclaim_closes(self, session_id: Any) -> int | None:
        """The second this reservation stops being reissuable, if it still is."""
        lost_at = self.released_at.get(session_id) if isinstance(session_id, str) else None
        return None if lost_at is None else lost_at + RECLAIM_GRACE_SECONDS

    def spent(self, session_id: Any, now: int) -> bool:
        """True once a released reservation is worth nothing to anybody."""
        return session_id in self.released_sessions and not self.reclaimable(session_id, now)


class Admission:
    def __init__(self, control: Path, state: Path, invite: str, ssh: str, ssh_key: Path,
                 issuer: Path, swarm: str, standing_host_state: Path, statvfs=os.statvfs):
        self.control, self.state = control, state
        self.invite, self.ssh, self.ssh_key, self.issuer, self.swarm = invite, ssh, ssh_key, issuer, swarm
        self.standing_host_state = standing_host_state
        self.statvfs = statvfs
        self.last_good: dict[str, Campaign] | None = None
        self.last_note: str | None = None
        self.children: dict[str, subprocess.Popen[str]] = {}
        self.reapers: set[threading.Thread] = set()
        self.always_on: set[str] = set()
        self.locks: dict[str, Any] = {}
        # Slot -> display label for the sessions currently live on each campaign.
        # Labels only (#484): never identity, never authority, never
        # addressing, and nothing banks off them. It is deliberately in memory
        # and deliberately not `joins.jsonl` — that file is the durable
        # *history* of every join this service ever made, and serving it as a
        # roster would name players who left hours ago.
        self.rosters: dict[str, dict[int, str]] = {}
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
                host = str(ipaddress.ip_address(s["host"]))
                external_port = s.getint("external_port")
                if not 1 <= external_port <= 65535: raise ValueError("external_port must be 1..65535")
                ruleset_version = s.getint("ruleset_version", fallback=None)
                if ruleset_version is not None and not 0 <= ruleset_version <= 0xffffffff:
                    raise ValueError("ruleset_version must be a u32")
                got[ident] = Campaign(ident, s["title"], s.get("open", "").lower() == "yes", host,
                    s.getint("peers"), s.getint("seconds"), s.getint("loss_pct"), s.getint("jitter_ms"),
                    external_port, s.get("client_rev") or None, ruleset_version,
                    s.get("always_on", "").lower() == "yes", s.getint("humans", fallback=1),
                    s.getint("lobby_seconds", fallback=LOBBY_SECONDS))
                # Existing one-human configuration predates `humans` and may
                # use peers=8.  The explicit multi-human shape is bounded by
                # D6's eight-peer full-mesh regime.
                if got[ident].humans < 1 or ("humans" in s and got[ident].peers + got[ident].humans > 8):
                    raise ValueError(f"{ident!r}: peers + humans must be between 1 and 8")
                if got[ident].lobby_seconds < 0:
                    raise ValueError(f"{ident!r}: lobby_seconds must not be negative")
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
            attempt = self._always_on_attempt(c) if c.always_on else None
            phase, slots_free = self._campaign_phase(c, attempt)
            # `state` is the JOINABILITY word every shipped client keys on:
            # `clients/regolith/src/admission.rs` treats `state == "open"` as
            # joinable and nothing else. A lobby *is* open for joining, so it
            # must say "open" here or every released binary refuses to offer the
            # campaign. The finer phase travels in `phase`, which the roster
            # endpoint has always carried and a lobby-aware client reads.
            phase_state = "open" if phase in ("lobby", "running") else phase
            state = "paused" if c.open and paused else (phase_state if c.always_on else ("busy" if c.ident in self.children else ("open" if c.open else "closed")))
            rows.append({"id": c.ident, "title": c.title, "state": state, "phase": phase, "peers": c.peers, "seconds": c.seconds,
                         "loss_pct": c.loss_pct, "jitter_ms": c.jitter_ms, "client_rev": c.client_rev,
                         "server_rev": c.client_rev, "ruleset_version": c.ruleset_version,
                         "humans": c.humans, "slots_free": slots_free})
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

    def _always_on_attempt(self, campaign: Campaign) -> dict[str, Any] | None:
        """Read the co-located supervisor generation; it is the lease clock of record."""
        path = self.standing_host_state / campaign.ident / "attempt.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None
        try:
            if (not isinstance(value.get("attempt_id"), str) or not isinstance(value.get("started"), int)
                    or not isinstance(value.get("expires_at"), int) or value["expires_at"] <= value["started"]):
                raise ValueError("invalid attempt record")
            return value
        except (ValueError, AttributeError):
            logging.error("always-on host returned an invalid attempt record for %s", campaign.ident)
            return None

    def _read_slots(self, campaign: Campaign) -> list[dict[str, Any]] | None:
        """The reservation journal, or `None` when it could not be read (#1150).

        A missing file is a real, empty journal: nobody has reserved a seat on
        this campaign yet, and answering `[]` is correct.  Every other failure
        -- `EIO`, `EACCES`, `EMFILE` from a burst of threaded requests, a torn
        or truncated document -- is *not* an empty journal, and answering `[]`
        for one is the dangerous direction: the caller draws every seat free
        and the next join durably rewrites the file with a single row, taking
        out every live reservation on the campaign at once.

        So this reads like `_published_standing_host_membership` and not like
        the old version of itself: a feed it cannot trust says nothing, says so
        in the log, and the callers turn that into `restarting` / `503
        host_failed` rather than into an empty seat map.  Nothing may write
        over state it could not read.
        """
        path = self.state / campaign.ident / "slots.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return []
        except (OSError, json.JSONDecodeError) as error:
            logging.error("campaign %s: reservation journal %s is unreadable (%s); "
                          "refusing to treat it as empty", campaign.ident, path, error)
            return None
        if not isinstance(value, list) or not all(isinstance(row, dict) for row in value):
            logging.error("campaign %s: reservation journal %s is not a list of rows; "
                          "refusing to treat it as empty", campaign.ident, path)
            return None
        return value

    def _write_slots(self, campaign: Campaign, slots: list[dict[str, Any]]) -> None:
        self.atomic_bytes(self.state / campaign.ident / "slots.json",
                          json.dumps(slots, separators=(",", ":")).encode())

    def _standing_host_listening(self, campaign: Campaign) -> str | None:
        try:
            listening = (self.standing_host_state / campaign.ident / "listening.txt").read_text(encoding="utf-8")
        except OSError:
            return None
        return listening if listening.strip() else None

    def _published_standing_host_membership(self, campaign: Campaign
                                            ) -> StandingHostMembership | None:
        """Read whichever generation the host says is actually bound."""
        path = self.standing_host_state / campaign.ident / "active-seats.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
            slots = value["active_slots"]
            held = value.get("pending_slots", [])
            released = value.get("released_sessions", [])
            released_at = value.get("released_at", {})
            running = value["running"]
            attempt_id = value.get("attempt_id")
        except FileNotFoundError:
            raise
        except (OSError, json.JSONDecodeError, KeyError, TypeError):
            return None
        if (not isinstance(attempt_id, str) or not isinstance(slots, list)
                or not isinstance(released, list)
                or not isinstance(running, bool)
                or any(not isinstance(session, str) or not SESSION.fullmatch(session)
                       for session in released)):
            return None
        # A reissue window is only as trustworthy as the timestamp under it, so
        # nothing malformed buys one: an unparseable feed fails the whole read
        # closed, exactly as a malformed slot list does.  A session named here
        # but not released is a contradiction, and the safe reading of a
        # contradiction is that no seat is held.
        if (not isinstance(released_at, dict)
                or any(not isinstance(session, str) or session not in set(released)
                       or not isinstance(when, int) or isinstance(when, bool) or when < 0
                       for session, when in released_at.items())):
            return None
        if not isinstance(held, list):
            return None
        for listed in (slots, held):
            if (any(not isinstance(slot, int) or isinstance(slot, bool)
                    or not campaign.peers <= slot < campaign.peers + campaign.humans
                    for slot in listed)
                    or len(set(listed)) != len(listed)):
                return None
        # A seat cannot be both bound and merely connected, and the safe reading
        # of a contradiction is that this feed says nothing (#1016).
        if set(slots) & set(held):
            return None
        return StandingHostMembership(attempt_id, frozenset(slots), frozenset(held),
                                      frozenset(released), running, dict(released_at))

    def _standing_host_membership(self, campaign: Campaign, attempt: dict[str, Any]
                                  ) -> StandingHostMembership | None:
        """Read the expected generation's binding feed, failing closed on mismatch."""
        try:
            membership = self._published_standing_host_membership(campaign)
        except FileNotFoundError:
            return StandingHostMembership(attempt["attempt_id"], frozenset(), frozenset(),
                                          frozenset(), False, {})
        if membership is None or membership.attempt_id != attempt["attempt_id"]:
            return None
        return membership

    def _current_slots(self, campaign: Campaign, attempt: dict[str, Any], *, persist: bool
                       ) -> tuple[list[dict[str, Any]], StandingHostMembership] | None:
        """Return active, unexpired or still-reclaimable reservations for one host generation.

        A row the host has released survives its arrival lease for as long as
        the identity that held it may still redial (#1001), and not one second
        longer: `spent` is what drops it, and once it drops the seat is free for
        whoever asks next.

        A row whose seat the host *holds* -- bound, or connected and waiting in
        the lobby -- outlives its arrival lease for as long as the host keeps
        saying so (#1016).  The lease still governs the rows nobody is arriving
        for, which is every row the host has never seen a connection for, and
        those are exactly the rows it was written for.
        """
        membership = self._standing_host_membership(campaign, attempt)
        if membership is None:
            return None
        now = int(time.time())
        rows = self._read_slots(campaign)
        if rows is None:
            return None
        generation = [row for row in rows
                      if row.get("attempt_id") == attempt["attempt_id"]]
        current = [row for row in generation
                   if not membership.spent(row.get("session_id"), now)
                   and (row.get("slot") in membership.held_slots
                        or membership.reclaimable(row.get("session_id"), now)
                        or (isinstance(row.get("expires_at"), int)
                            and row["expires_at"] > now))]
        if persist and current != generation:
            self._write_slots(campaign, current)
        return current, membership

    def _campaign_phase(self, campaign: Campaign, attempt: dict[str, Any] | None) -> tuple[str, int]:
        if not campaign.open:
            return "closed", 0
        if attempt is None or self._standing_host_listening(campaign) is None:
            return "restarting", 0
        now = int(time.time())
        if now >= attempt["expires_at"]:
            return "restarting", 0
        if self._current_slots(campaign, attempt, persist=False) is None:
            return "restarting", 0
        # Counted from the same definition the roster renders, so the listing
        # can never answer `full` while the roster draws an empty seat (#713).
        slots = self._occupied_human_seats(campaign, attempt)
        if slots is None:
            return "restarting", 0
        free = campaign.humans - len(slots)
        # A standing host reopens empty lobby windows without respawning. Its
        # supervisor advances `started` at the same boundary, but this clause
        # also keeps the listing joinable during that atomic hand-off.
        if not slots:
            return "lobby", max(free, 0)
        if not free:
            return "full", 0
        membership = self._standing_host_membership(campaign, attempt)
        if membership is not None and membership.running:
            return "running", max(free, 0)
        return "lobby", max(free, 0)

    def _occupied_human_seats(self, campaign: Campaign, attempt: dict[str, Any] | None
                             ) -> dict[int, SeatOccupancy] | None:
        """The one definition of a taken human seat, for counting and rendering.

        A seat is taken when the host says it is bound, or when an unexpired
        reservation still holds it.  Both readers must agree: the listing's
        `slots_free` and the roster's per-seat state used to answer this from
        different sources -- the count keyed on admission's attempt pointer
        while the roster keyed on the host's published generation, and only the
        roster required an unexpired lease.  During a generation hand-off a
        bound-but-expired row was then counted as taken and drawn as empty, so
        the lobby showed free seats while admission answered `full` (#713).
        """
        rows = self._read_slots(campaign)
        if rows is None:
            # An unreadable journal is not an empty one (#1150): every seat
            # would draw free and the listing would advertise the campaign
            # wide open.  Say "I do not know" and let the caller fail closed.
            return None
        try:
            bound = self._published_standing_host_membership(campaign)
        except FileNotFoundError:
            bound = None
        occupied: dict[int, SeatOccupancy] = {}
        # The host publication, not admission's independently moving attempt
        # pointer, defines a binding's lifetime (#706).
        if bound is not None:
            for row in rows:
                if (row.get("attempt_id") == bound.attempt_id
                        and row.get("slot") in bound.active_slots
                        and row.get("session_id") not in bound.released_sessions):
                    occupied[row["slot"]] = SeatOccupancy(row=row, bound=True)
            # A seat the host holds a lobby connection for is taken, and taken
            # for as long as the host holds it rather than for the arrival
            # lease (#1016).  It draws `reserved`, not `active`: the host is
            # talking to that volunteer, but nothing is bound to the transport
            # and a reservation is still not a liveness claim.
            for row in rows:
                if (row.get("slot") not in occupied
                        and row.get("attempt_id") == bound.attempt_id
                        and row.get("slot") in bound.pending_slots
                        and row.get("session_id") not in bound.released_sessions):
                    occupied[row["slot"]] = SeatOccupancy(row=row, bound=False)
        generation = (bound if bound is not None and attempt is not None
                      and bound.attempt_id == attempt["attempt_id"] else None)
        released = generation.released_sessions if generation is not None else frozenset()
        now = int(time.time())
        if attempt is not None:
            for row in rows:
                if row.get("slot") in occupied or row.get("attempt_id") != attempt["attempt_id"]:
                    continue
                # A seat inside its reissue window is taken by the volunteer who
                # just lost it, whatever their arrival lease says: their lease
                # ran out while they were flying, and the window is what is
                # holding the seat now (#1001).  It draws `reserved`, not
                # `active`, because nothing is bound to the transport.
                held = (isinstance(row.get("expires_at"), int) and row["expires_at"] > now
                        and row.get("session_id") not in released)
                if held or (generation is not None
                            and generation.reclaimable(row.get("session_id"), now)):
                    occupied[row["slot"]] = SeatOccupancy(row=row, bound=False)
        return occupied

    def session_roster(self, campaign: Campaign, attempt: dict[str, Any] | None) -> list[dict[str, Any]]:
        """Return every configured seat; a reservation is not a liveness claim."""
        occupied = self._occupied_human_seats(campaign, attempt)
        # An unreadable journal (#1150) leaves the human seats unlabelled and
        # drawn `empty`, which is the documented "it does not know" answer for
        # this sideband and costs nothing: joinability travels beside it in
        # `phase`, which the same read has already turned into `restarting`.
        occupied = occupied if occupied is not None else {}
        roster: list[dict[str, Any]] = []
        for slot in range(campaign.peers):
            suffix = f"-{slot + 1}"
            roster.append({"slot": slot, "kind": "bot", "state": "active",
                           "nickname": display_label(campaign.ident[:DISPLAY_LABEL_MAX_CHARS - len(suffix)] + suffix)})
        for slot in range(campaign.peers, campaign.peers + campaign.humans):
            seat = occupied.get(slot)
            roster.append({"slot": slot, "kind": "human",
                           "state": "active" if seat and seat.bound else ("reserved" if seat else "empty"),
                           "nickname": seat.row.get("nickname") if seat else None})
        return roster

    @staticmethod
    def legacy_session_roster(campaign: Campaign, nickname: str) -> dict[int, str]:
        """The original one-human sideband, retained for non-standing campaigns."""
        roster = {slot: display_label(campaign.ident[:DISPLAY_LABEL_MAX_CHARS - len(f"-{slot + 1}")] + f"-{slot + 1}")
                  for slot in range(campaign.peers)}
        player = display_label(nickname)
        if player is not None:
            roster[campaign.peers] = player
        return {slot: label for slot, label in roster.items() if label is not None}

    @staticmethod
    def socket_address(host: str, port: int) -> str:
        return f"[{host}]:{port}" if ":" in host else f"{host}:{port}"

    @classmethod
    def harness_bind(cls, campaign: Campaign) -> str:
        wildcard = "::" if ":" in campaign.host else "0.0.0.0"
        return cls.socket_address(wildcard, campaign.external_port)

    @classmethod
    def dialable_listening(cls, campaign: Campaign, listening: str) -> tuple[str, str]:
        host_node, direct = listening.split(None, 1)
        try:
            port = int(direct.rsplit(":", 1)[1])
        except (IndexError, ValueError) as error:
            raise ValueError("harness listening address has no numeric port") from error
        if port != campaign.external_port:
            raise ValueError(f"harness reported UDP port {port}, expected {campaign.external_port}")
        return host_node, cls.socket_address(campaign.host, port)

    def join(self, ident: str, request: dict[str, Any]) -> dict[str, Any]:
        # The ordered campaign guards intentionally precede the lock and every subprocess.
        campaigns, _ = self.campaigns()
        if not CAMPAIGN_ID.fullmatch(ident) or ident not in campaigns: raise Refusal(404, "unknown_campaign", "That campaign has ended — refresh the list.")
        c = campaigns[ident]
        if c.client_rev and request.get("client_rev") != c.client_rev: raise Refusal(403, "client_rev_mismatch", f"This campaign needs build {c.client_rev} — download the current build.")
        if c.ruleset_version is not None and (not isinstance(request.get("ruleset_version"), int) or isinstance(request.get("ruleset_version"), bool) or request["ruleset_version"] != c.ruleset_version): raise Refusal(403, "ruleset_version_mismatch", f"This campaign needs ruleset v{c.ruleset_version} — download the current build.")
        if not c.open: raise Refusal(403, "campaign_closed", "This campaign is closed; pick another.")
        free = self.free_bytes()
        if free < MINT_FLOOR_BYTES:
            logging.warning("admissions paused: %d free bytes is below MINT_FLOOR_BYTES", free)
            raise Refusal(503, "admissions_paused", "Campaigns are temporarily unavailable while the operator makes room — nothing you did was wrong. Try again later.")
        nickname, node = request.get("nickname"), request.get("node")
        if not isinstance(nickname, str) or not re.fullmatch(r"[^\t\r\n]{1,32}", nickname) or display_label(nickname) is None or any(not " " <= glyph <= "~" for glyph in nickname): raise Refusal(422, "bad_nickname", "Nicknames are 1–32 visible ASCII characters, with no tabs or newlines.")
        if not isinstance(node, str) or not NODE.fullmatch(node): raise Refusal(422, "bad_node", "This build sent a bad transport key — reinstall the client.")
        directory = self.state / c.ident; directory.mkdir(parents=True, exist_ok=True)
        lock = (directory / "lock").open("a+")
        try:
            if c.always_on:
                # The short flock is an allocation transaction. A reservation
                # proves only that its holder is arriving, so it expires after
                # 45 seconds unless the host has published that slot as bound.
                fcntl.flock(lock, fcntl.LOCK_EX)
                attempt = self._always_on_attempt(c)
                phase, _ = self._campaign_phase(c, attempt)
                if attempt is None or phase == "restarting":
                    raise Refusal(503, "host_failed", "The always-on host is not ready — try again shortly.")
                current_slots = self._current_slots(c, attempt, persist=True)
                if current_slots is None:
                    raise Refusal(503, "host_failed", "The always-on host membership is invalid — try again shortly.")
                slots, membership = current_slots
                # The reservation is found by the transport identity that holds
                # it, which is what makes the reissue safe: knowing a session id
                # buys nothing, because no row is ever looked up by one.  The
                # token minted below is signed for the node presented here, and
                # the host checks both — the QUIC-authenticated dialler against
                # the token, and the token's node against this row (#583).
                existing = next((row for row in slots if row.get("node") == node), None)
                slot = existing.get("slot") if existing else None
                granted_nickname = existing.get("nickname") if existing else None
                if existing is None:
                    occupied = {row.get("slot") for row in slots}
                    free_slots = [candidate for candidate in range(c.peers, c.peers + c.humans) if candidate not in occupied]
                    if not free_slots:
                        raise self._full_refusal(c, slots, membership)
                    slot = free_slots[0]
                    try:
                        minted = self.output([self.invite, "mint", "--ledger", str(directory / "ledger.tsv"), "--label", nickname])
                        account, sid = minted["account"], minted["session_id"]
                        granted_nickname = display_label(nickname)
                        slots.append({"attempt_id": attempt["attempt_id"], "slot": slot, "session_id": sid,
                                      "account": int(account), "node": node, "nickname": granted_nickname,
                                      "expires_at": int(time.time()) + ARRIVAL_LEASE_SECONDS})
                        # A generation may turn over while minting.  Do not let a
                        # stale answer reserve a seat in the next attempt.
                        current = self._always_on_attempt(c)
                        if current is None or current["attempt_id"] != attempt["attempt_id"]:
                            raise Refusal(503, "host_failed", "The attempt restarted while reserving your seat — try again shortly.")
                        # The join log is appended *before* the reservation is
                        # committed (#1152).  These two writes are not one
                        # transaction and cannot be made into one, so the only
                        # choice is which way they fail, and the two directions
                        # are not symmetric:
                        #
                        #   slot row, no join row -> the seat is burned and the
                        #     account is minted, but `known_session` cannot see
                        #     the session, so every evidence upload it ever
                        #     makes is refused `404 unknown_session`, forever.
                        #   join row, no slot row -> nothing is reserved, the
                        #     volunteer is refused and retries into a clean
                        #     journal, and the orphan line costs one known
                        #     session id that never uploads anything.
                        #
                        # The second is a spare line in an append-only audit
                        # log.  The first is unrecoverable evidence loss, so
                        # the durable commit goes last.
                        self.append_join(c, {"when": int(time.time()), "campaign": ident, "nickname": nickname,
                                             "account": int(account), "session_id": sid, "node": node, "slot": slot,
                                             "attempt_id": attempt["attempt_id"]})
                        self._write_slots(c, slots)
                    except Refusal:
                        raise
                    except (OSError, RuntimeError, KeyError, ValueError) as e:
                        logging.exception("admission subprocess/log failed: %s", e)
                        raise Refusal(500, "admission_failed", "Admission failed; tell the operator.") from e
                else:
                    account, sid = existing["account"], existing["session_id"]
                    # The redial of a volunteer whose connection lapsed. Nothing
                    # is minted: the same session, the same account and the same
                    # seat come back, and the only new thing is a freshly signed
                    # token for the same node.
                    #
                    # The row gets a fresh arrival lease on *every* reissue, not
                    # only on the reclaimable one (#1147).  The host's journal
                    # refuses an expired row outright
                    # (`gates/p1-swarm/src/exterior.rs:927`,
                    # `reservation_journal_stale`), and a reconnect inside the
                    # ~12 s before the host publishes `released_at` used to be
                    # handed back exactly that: the same seat, with a lease
                    # nothing had touched since the original join 45 s or an hour
                    # ago.  That is the moment the client tells the volunteer to
                    # rejoin *now* (`crates/orrery_net/src/net.rs:751`), so it was
                    # the one moment the advice could not work.
                    #
                    # Refreshing it here is not a way to hold a seat longer than
                    # the lease allows: the row is found by the transport
                    # identity that owns it and by nothing else, and a lease is
                    # exactly the promise that a seat is held for somebody who is
                    # arriving.  Somebody dialling this second is arriving.
                    reclaiming = membership.reclaimable(sid, int(time.time()))
                    existing["expires_at"] = int(time.time()) + ARRIVAL_LEASE_SECONDS
                    try:
                        # Log first, commit second, for the reason spelled out
                        # on the mint path above (#1152).  This branch had no
                        # `except` of its own at all, so an `ENOSPC` or `EIO`
                        # here escaped `join` as a bare `OSError` and reached
                        # the handler, which answered nothing.
                        self.append_join(c, {"when": int(time.time()), "campaign": ident,
                                             "nickname": nickname, "account": int(account),
                                             "session_id": sid, "node": node, "slot": slot,
                                             "attempt_id": attempt["attempt_id"], "reissued": True})
                        self._write_slots(c, slots)
                    except (OSError, RuntimeError, KeyError, ValueError) as e:
                        logging.exception("admission could not reissue seat %s on %s: %s", slot, ident, e)
                        raise Refusal(500, "admission_failed", "Admission failed; tell the operator.") from e
                    logging.info("campaign %s: reissued seat %s to the transport identity that "
                                 "holds it (session %s, host has published the release: %s)",
                                 ident, slot, sid, reclaiming)
                try:
                    # `--assume-standing-good` is mandatory since #1014: the
                    # offline mint reads no standing ledger, so it refuses
                    # rather than assume. The attestation is sound here because
                    # every account this service signs for is one it minted
                    # into this campaign's own ledger, with no strike history
                    # anywhere to skip.
                    signed = self.output([self.invite, "session-token", "--issuer-credential", str(self.issuer),
                                          "--account", str(account), "--node", node, "--assume-standing-good"])
                    session_dir = self.state / "sessions" / sid; session_dir.mkdir(parents=True, exist_ok=True)
                    listening = self._wait_always_on_listening(c, session_dir)
                    host_node, host_direct = self.dialable_listening(c, listening)
                except (RuntimeError, KeyError, ValueError) as e:
                    logging.error("always-on host returned an unusable listening address: %s", e)
                    raise Refusal(503, "host_failed", "The always-on host is not ready — try again shortly.") from e
                return {"join": {"host_node": host_node, "slot": slot, "session_id": sid, "session_token": signed["session_token"]}, "host_direct": host_direct, "account": int(account), "nickname": granted_nickname, "expires_in_s": 3600, "configured": configured_impairment(c)}
            try: fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                lock.close()
                raise Refusal(409, "campaign_busy", f"In use — try again in about {c.seconds // 60} minutes.", retry_after_s=c.seconds)
            self.locks[ident] = lock
            try:
                minted = self.output([self.invite, "mint", "--ledger", str(directory / "ledger.tsv"), "--label", nickname])
                account, sid = minted["account"], minted["session_id"]
                # See the always-on path above for why the #1014 attestation
                # holds: the account was minted one line earlier.
                signed = self.output([self.invite, "session-token", "--issuer-credential", str(self.issuer), "--account", account, "--node", node, "--assume-standing-good"])
                self.append_join(c, {"when": int(time.time()), "campaign": ident, "nickname": nickname, "account": int(account), "session_id": sid, "node": node})
            # `append_join` raises a bare `OSError` on a full or failing disk
            # and this tuple did not name it (#1152), so the exception escaped
            # the request handler and the socket closed with no HTTP status.
            except (OSError, RuntimeError, KeyError, ValueError) as e:
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
            # Everything after `orrery@host` is re-parsed by a shell (#1155).
            # `ssh` joins its command arguments with spaces and hands the
            # result to the remote login shell -- which is the only reason the
            # literal `&&` in the middle of this line works at all.  The local
            # `Popen` is a list with no `shell=True`, so nothing is re-parsed
            # here; the remote end is where a value with a space splits into
            # two arguments and a value with a `;` or a backtick runs as
            # `orrery` on the campaign host.
            #
            # The only operator-controlled value on the line is `c.client_rev`,
            # read from `campaigns.conf` -- a root-owned file that no request
            # path writes and that the plan calls "the only file a human
            # edits".  So this is a robustness fix, not a security one: the
            # exposure needs the operator to attack their own host.  What it
            # actually costs today is silent: a `client_rev` with a stray space
            # starts the harness pinned to the wrong build, or not at all, and
            # the operator sees `host_failed` with nothing pointing at the
            # config.
            #
            # `&&` is shell syntax and must stay unquoted; every other element
            # is data.  `shlex.quote` leaves a well-formed value untouched, so
            # the wire form of a correct configuration does not change.
            remote_argv = ["mkdir", "-p", remote, "&&", self.swarm, "--external-peer", "--external-bind", self.harness_bind(c), "--peers", str(c.peers), "--seconds", str(c.seconds), "--min-cells", "1", "--impaired", "--witness", "--stamp-wall-clock", "--json", f"{remote}/raw.json", "--listening-file", f"{remote}/listening.txt", "--require-session", sid, "--issuer-key", f"{signed['issuer_key_id']}:{signed['issuer_public_key']}"]
            if c.client_rev: remote_argv += ["--require-client-rev", c.client_rev]
            command = [self.ssh, "-i", str(self.ssh_key), f"orrery@{c.host}"] + [
                word if word == "&&" else shlex.quote(word) for word in remote_argv]
            try: child = subprocess.Popen(command, text=True)
            except OSError as e: raise Refusal(503, "host_failed", "The host could not start your session — tell the operator, nothing you did was wrong.") from e
            listening = self._wait_listening(c, remote, session_dir, child)
            try: host_node, host_direct = self.dialable_listening(c, listening)
            except ValueError as e:
                child.kill(); child.wait()
                logging.error("host returned an unusable listening address: %s", e)
                raise Refusal(503, "host_failed", "The host could not start your session — tell the operator, nothing you did was wrong.") from e
            self.children[ident] = child
            self.rosters[ident] = self.legacy_session_roster(c, nickname)
            reaper = threading.Thread(target=self._reap, args=(ident, c, sid, remote, session_dir, child), daemon=True)
            self.reapers.add(reaper)
            reaper.start()
            return {"join": {"host_node": host_node, "slot": c.peers, "session_id": sid, "session_token": signed["session_token"]}, "host_direct": host_direct, "account": int(account), "nickname": display_label(nickname), "expires_in_s": 3600, "configured": configured_impairment(c)}
        finally:
            # The flock stays held by the child/reaper, not the request.  It is released there.
            #
            # Pop by *identity*, not by key (#1146).  `self.locks.pop(ident)`
            # removes whatever object is under the key, which need not be this
            # request's lock.  `flock(2)` locks belong to the open file
            # description and every request gets its own `open()` at the top of
            # this method, so no request can release another's by closing its
            # own fd -- but the dict entry is the incumbent lock's only
            # long-lived reference.  Popping it drops the refcount to zero once
            # the incumbent's frame returns, CPython closes the file, and the
            # flock goes with it mid-session.
            #
            # The window is bounded but real, and it is not a "409 during a
            # run": the guard below is false for a genuinely running session,
            # because `self.children[ident]` is set before this method returns.
            # The hole is between `self.locks[ident] = lock` and that
            # assignment, which spans two `orrery-invite` subprocesses, a
            # `Popen` and `_wait_listening` -- seconds.  A second join landing
            # inside it is refused `campaign_busy`, still sees `ident not in
            # self.children`, and used to pop the incumbent's lock.  Two joins
            # a second apart is how a tester double-clicking Join produces it.
            #
            # Same "keyed by the wrong thing" shape as #1118.  Closing this
            # request's own `lock` is always right and always harmless: in the
            # 409 path it is already closed, and `close()` is idempotent.
            if ident not in self.children:
                if self.locks.get(ident) is lock:
                    del self.locks[ident]
                lock.close()

    @staticmethod
    def _full_refusal(campaign: Campaign, slots: list[dict[str, Any]],
                      membership: StandingHostMembership) -> Refusal:
        """Say which kind of full this is, because the two need different actions.

        A seat inside its reissue window is not occupied by a player, and
        "occupied" is the wrong thing to tell somebody who could have it in
        under a minute by waiting.  The seconds are named so waiting is a
        decision rather than a guess, and they go in the sentence rather than in
        `retry_after_s`: shipped clients append "Next lobby in ..." to that
        field, and this is not the next lobby, it is this one.
        """
        now = int(time.time())
        closes = [when for when in (membership.reclaim_closes(row.get("session_id"))
                                    for row in slots) if when is not None and when > now]
        if closes:
            wait = min(closes) - now
            return Refusal(409, "seat_held_for_reconnect",
                           f"Every other seat is taken, and the last one is being held for about "
                           f"{wait} more second{'' if wait == 1 else 's'} for a player whose "
                           "connection just dropped. Nothing you did was wrong — try again in a "
                           "moment.")
        return Refusal(409, "campaign_full", f"All {campaign.humans} player seats are currently occupied.",
                       retry_after_s=ARRIVAL_LEASE_SECONDS)

    def roster(self, ident: str) -> dict[str, Any]:
        """Slot -> display label for every craft in this live campaign.

        A nickname is a label and nothing else (#484), which is exactly why it
        is served here instead of riding with replicated craft state: it is not
        simulation state, determinism does not depend on it, and putting
        unbounded player-supplied text on the replication hot path would spend
        the replication budget on decoration. A label may therefore be late,
        stale or missing with no correctness consequence, and this endpoint is
        allowed to answer with an empty roster whenever it does not know.

        Empty is a real answer, not an error: a campaign nobody has joined has
        no session and therefore no labels. The client must draw any craft for
        which this sideband has no row with no label rather than inventing one.
        """
        campaigns, _ = self.campaigns()
        if not CAMPAIGN_ID.fullmatch(ident) or ident not in campaigns:
            raise Refusal(404, "unknown_campaign", "That campaign has ended — refresh the list.")
        c = campaigns[ident]
        attempt = self._always_on_attempt(c) if c.always_on else None
        phase, _ = self._campaign_phase(c, attempt)
        if c.always_on:
            return {"campaign": ident, "phase": phase, "roster": self.session_roster(c, attempt)}
        live = self.rosters.get(ident, {})
        return {"campaign": ident, "roster": [{"slot": slot, "nickname": nickname} for slot, nickname in sorted(live.items())]}

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

    def _wait_always_on_listening(self, c: Campaign, local: Path) -> str:
        listening = self._standing_host_listening(c)
        if listening is not None:
            self.atomic_bytes(local / "listening.txt", listening.encode())
            return listening.strip()
        raise Refusal(503, "host_failed", "The always-on host is not ready — try again shortly.")

    def _reap(self, ident: str, c: Campaign, sid: str, remote: str, local: Path, child: subprocess.Popen[str]) -> None:
        try:
            child.wait()
            result = subprocess.run([self.ssh, "-i", str(self.ssh_key), f"orrery@{c.host}", "cat", f"{remote}/raw.json"], capture_output=True)
            if result.returncode == 0:
                try: self.atomic_bytes(local / "raw.json", result.stdout)
                except OSError: logging.exception("could not store raw report for %s", sid)
            else: logging.error("could not pull raw report for %s", sid)
        finally:
            self.children.pop(ident, None)
            self.rosters.pop(ident, None)
            lock = self.locks.pop(ident, None)
            if lock is not None: lock.close()
            self.reapers.discard(threading.current_thread())

    def shutdown(self) -> None:
        """Stop child sessions and wait for their reapers to finish."""
        children = list(self.children.values())
        for child in children:
            if child.poll() is None: child.kill()
        for child in children:
            child.wait()
        for reaper in list(self.reapers): reaper.join()

    @staticmethod
    def _session_ids_in(joins: Path) -> tuple[list[str], int]:
        """Every session id in one append-only join log, and how many lines were skipped.

        `joins.jsonl` is an append-ordered audit log, so a damaged line is a
        damaged *line* and never a reason to stop reading the file (#1151).
        The old reader put the whole scan inside one `try`, and `any()` is
        lazy, so the first `JSONDecodeError` escaped the generator and ended
        the file at the bad line -- making every session admitted *after* it
        permanently unknown and refusing all of their evidence uploads with
        `404 unknown_session`, forever.

        The realistic way to get a truncated line is a full disk, and this
        service is deliberately built to keep appending here below
        `MINT_FLOOR_BYTES` (see
        `test_an_admitted_session_still_uploads_below_the_floor`), so the
        trigger is a condition the design already expects to meet.

        A line that parses but is not an object (`null`, a bare number) is
        skipped the same way: `.get`/`[]` on one raises `AttributeError` or
        `TypeError`, neither of which the old `except` tuples named, so it
        propagated out of the request handler entirely.
        """
        skipped = 0
        ids: list[str] = []
        for number, line in enumerate(joins.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
                session = row["session_id"]
                if not isinstance(session, str):
                    raise TypeError("session_id is not a string")
            except (json.JSONDecodeError, KeyError, AttributeError, TypeError, ValueError):
                skipped += 1
                if skipped == 1:
                    logging.warning("join log %s: line %d is unreadable and was skipped; the rest "
                                    "of the file is still being read (#1151)", joins, number)
                continue
            ids.append(session)
        if skipped:
            logging.warning("join log %s: %d unreadable line(s) skipped in total", joins, skipped)
        return ids, skipped

    def known_session(self, sid: str) -> bool:
        if not SESSION.fullmatch(sid): return False
        for joins in self.state.glob("*/joins.jsonl"):
            try:
                ids, _ = self._session_ids_in(joins)
            except OSError as error:
                # The file itself is unreadable, which is not evidence that this
                # session is unknown -- so say so rather than letting silence
                # stand in for an answer, and keep looking in the other
                # campaigns' logs.
                logging.error("join log %s could not be read (%s); it cannot answer for session %s",
                              joins, error, sid)
                continue
            if sid in ids: return True
        return False

    def campaign_can_stand_down(self, ident: str) -> bool:
        """The campaign teardown gate: no admitted report may be left remote-only."""
        joins = self.state / ident / "joins.jsonl"
        if not joins.exists(): return True
        # This gate fails *closed* where `known_session` skips: a line it cannot
        # read may name a session whose report is still remote-only, and the
        # cost of standing down over one is an unrecoverable loss of evidence,
        # while the cost of refusing is that an operator looks at the log.
        try: ids, skipped = self._session_ids_in(joins)
        except OSError: return False
        if skipped: return False
        return all((self.state / "sessions" / sid / "raw.json").is_file() for sid in ids)

    @staticmethod
    def atomic_bytes(path: Path, data: bytes) -> None:
        """Write `data` to `path` atomically, through a temp name nobody else can hold.

        The temp name used to be a *fixed* `<name>.tmp` (#1153), which is safe
        only on a serial server, and this one is not: `main` runs a
        `ThreadingHTTPServer`, so two requests writing the same target run this
        concurrently.  Two `write`s and two `fsync`s interleaving into one
        shared file, followed by two `os.replace`s, bank a *spliced* record --
        the first half of one body and the second half of another -- which
        #1119's conflict check then treats as the canonical evidence, because
        it compares against whatever is stored.  Failing would be fine; banking
        a wrong artifact is the #1051/#1053 shape.

        `mkstemp` in the target's own directory gives a name unique per call
        and keeps the rename on one filesystem.  The explicit `chmod` restores
        the 0644 the old `open("wb")` produced under the service umask --
        `mkstemp` creates 0600, and the evidence directory is read by operator
        tooling (`p4-ledger.sh`), not only by this service.
        """
        fd, temp_name = tempfile.mkstemp(dir=str(path.parent), prefix=path.name + ".", suffix=".tmp")
        temp = Path(temp_name)
        try:
            with os.fdopen(fd, "wb") as f: f.write(data); f.flush(); os.fsync(f.fileno())
            os.chmod(temp, 0o644)
            os.replace(temp, path)
        except BaseException:
            temp.unlink(missing_ok=True)
            raise
        dir_fd = os.open(path.parent, os.O_DIRECTORY); os.fsync(dir_fd); os.close(dir_fd)

    def upload(self, sid: str, body: bytes, increment: int = 0) -> None:
        """Store one increment of one session's client evidence, and say so out loud when it is refused."""
        try: self._store_upload(sid, body, increment)
        except Refusal as e:
            # A refused upload is a session that went unrecorded, and the only
            # other report of it is a line in the player's own log. Admission
            # is the one party that can see it at all, so it says so here
            # (#735); silence here is indistinguishable from a player who
            # never played, which is the blind spot #711 existed to close.
            logging.error("upload refused for session %s increment %d: %d %s (%d bytes)", sid, increment, e.status, e.error, len(body))
            raise

    @staticmethod
    def increment_of(row: Any) -> int:
        """The increment index a client row carries, or zero for a row without one.

        A row written before #1048 has no `increment` object and is a whole
        seat, which is increment zero of a seat of one -- the same reading the
        client's `increment_index_of` takes and the same one `p4-ledger.sh`
        takes.
        """
        increment = row.get("increment")
        if not isinstance(increment, dict): return 0
        index = increment.get("index")
        return index if isinstance(index, int) and not isinstance(index, bool) else 0

    def _store_upload(self, sid: str, body: bytes, increment: int = 0) -> None:
        if not self.known_session(sid): raise Refusal(404, "unknown_session", "That session is not known to this service.")
        if len(body) > MAX_UPLOAD_BYTES: raise Refusal(413, "too_large", "The upload is too large.")
        try: payload = json.loads(body)
        except (UnicodeDecodeError, json.JSONDecodeError) as e: raise Refusal(422, "bad_upload", "The upload is not valid JSON.") from e
        records = payload.get("records")
        if not isinstance(records, list) or any(not isinstance(r, dict) or r.get("session_id") != sid for r in records): raise Refusal(422, "wrong_session", "Every uploaded row must name this session.")
        telemetry = payload.get("telemetry_jsonl")
        if not isinstance(telemetry, str): raise Refusal(422, "bad_upload", "Telemetry must be text.")
        # Every row of one body is one increment of one seat, and the URL says
        # which (#1119). Before this, a seat longer than the client's
        # five-minute cadence posted each of its increments to the *same* path
        # and every one after the first was refused 409 as a conflicting
        # re-upload of the seat -- so a 60-minute session banked five minutes,
        # on that launch and on every later one.
        if any(self.increment_of(r) != increment for r in records): raise Refusal(422, "wrong_increment", "Every uploaded row must name this seat increment.")
        # Increment zero keeps the unsuffixed names, so everything already
        # banked stays exactly where it is and every reader of a session
        # directory keeps working unchanged; the increments after it sit
        # beside it in the same seat's directory. The seat is still one
        # directory: `sessions/<sid>` is where the host's `raw.json` and
        # listening file live, and an increment is not a session.
        suffix = "" if increment == 0 else f".increment-{increment}"
        target = self.state / "sessions" / sid; target.mkdir(parents=True, exist_ok=True)
        files = {target / f"client-records{suffix}.jsonl": ("\n".join(json.dumps(r, separators=(",", ":")) for r in records) + ("\n" if records else "")).encode(), target / f"telemetry{suffix}.jsonl": telemetry.encode()}
        # The check and the write are one transaction, and the server is
        # threaded (#1153).  Without this lock two concurrent posts of
        # *differing* bodies for the same session and increment both pass the
        # `exists()` test below -- neither file is there yet -- and both go on
        # to write, so the conflict that exists to refuse exactly this is never
        # seen and a spliced or arbitrary body is banked silently.  That is not
        # hypothetical for this client: the retry sweep and the exit-path flush
        # both call `send_pending` with no mutual exclusion between them
        # (`clients/regolith/src/admission.rs:1671-1680`, `:1785-1803`).
        #
        # An `flock` on a per-seat file, taken through this method's own
        # `open()`, so it serialises threads in this process and any second
        # process reading the same state directory.  It is per seat rather than
        # per campaign because uploads are addressed by session and know no
        # campaign, and holding it across the writes below makes no other
        # seat's upload wait.
        with (target / ".upload.lock").open("a+") as guard:
            fcntl.flock(guard, fcntl.LOCK_EX)
            # Still a conflict, but per increment: two *different* bodies claiming
            # the same increment of the same seat cannot both be that increment's
            # evidence. An identical re-send is not a conflict and never was --
            # which is what makes the client's retry, and its #1118 repair, free.
            for path, data in files.items():
                if path.exists() and path.read_bytes() != data: raise Refusal(409, "conflict", "A different upload already exists for this session increment.")
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
        """Read the request body, refusing an over-large one before a byte of it (#1153).

        This used to be `self.rfile.read(int(Content-Length))` with no cap, and
        `MAX_UPLOAD_BYTES` was checked afterwards in `_store_upload`.  So a
        declared `Content-Length` of 4 GB was allocated and read into memory
        *before* it was refused, on a threaded server, once per connection.
        nginx normally caps this upstream -- but #1002 exists precisely because
        the proxy's limit cannot be assumed, and the service is reachable
        directly during a campaign stand-up.

        The declared length is refused first, then the body is read in bounded
        chunks rather than one sized `read`, so a client that declares a length
        and then stalls holds one chunk, not the whole declaration.  A refusal
        here closes the connection: the unread body would otherwise be parsed
        as the next request on a keep-alive socket.
        """
        raw = self.headers.get("Content-Length", "0")
        try: size = int(raw)
        except ValueError: raise Refusal(411, "bad_length", "The request needs a numeric Content-Length.") from None
        if size < 0: raise Refusal(411, "bad_length", "The request needs a numeric Content-Length.")
        if size > MAX_UPLOAD_BYTES:
            self.close_connection = True
            raise Refusal(413, "too_large", "The upload is too large.")
        chunks: list[bytes] = []
        remaining = size
        while remaining > 0:
            chunk = self.rfile.read(min(remaining, 1024**2))
            if not chunk:
                self.close_connection = True
                raise Refusal(400, "short_body", "The request body ended before its declared length.")
            chunks.append(chunk); remaining -= len(chunk)
        body = b"".join(chunks)
        return json.loads(body), body
    def unexpected(self, e: BaseException) -> None:
        """Answer an HTTP status for a failure nothing named (#1152).

        `http.server` handles an escaping exception by logging a traceback and
        closing the socket with *no response at all*.  The volunteer sees a
        connection reset: the campaign list does not load, or the join button
        does nothing, with no code and no sentence to act on.  That is the
        "it's broken" report, which is the most expensive kind to receive when
        a tester is available once or twice a day.

        The reachable sources are all `OSError`: `append_join` on a full disk,
        `statvfs` in `listing()` -- on the endpoint every client hits first --
        and `EMFILE` from a burst against a `ThreadingHTTPServer`.  Each named
        site now catches its own; this is the floor under all of them, so that
        every outcome of every request is an HTTP status.
        """
        logging.exception("unhandled admission failure on %s %s: %s", self.command, self.path, e)
        try: self.failure(Refusal(500, "admission_failed", "Admission failed; tell the operator."))
        except OSError: self.close_connection = True  # the peer is already gone

    def do_GET(self) -> None:
        path = urlparse(self.path).path
        roster = re.fullmatch(r"/v1/campaigns/([^/]+)/roster", path)
        try:
            if path == "/v1/campaigns": self.send_json(200, self.service.listing())
            elif roster: self.send_json(200, self.service.roster(unquote(roster.group(1))))
            else: raise Refusal(404, "not_found", "No such endpoint.")
        except Refusal as e: self.failure(e)
        except Exception as e: self.unexpected(e)
    def do_POST(self) -> None:
        path = urlparse(self.path).path; match = re.fullmatch(r"/v1/campaigns/([^/]+)/join", path)
        upload = re.fullmatch(r"/v1/sessions/([^/]+)/upload", path)
        # One increment of a seat, addressed as itself (#1119). Increment zero
        # keeps the unsuffixed route above so a client that predates this
        # service build banks its first increment exactly as it does today,
        # and the two halves of the seam can be deployed in either order.
        increment = re.fullmatch(r"/v1/sessions/([^/]+)/increments/(\d{1,9})/upload", path)
        try:
            if match: self.send_json(200, self.service.join(unquote(match.group(1)), self.read_json()[0]))
            elif upload:
                _, body = self.read_json(); self.service.upload(unquote(upload.group(1)), body); self.send_json(204)
            elif increment:
                _, body = self.read_json(); self.service.upload(unquote(increment.group(1)), body, int(increment.group(2))); self.send_json(204)
            else: raise Refusal(404, "not_found", "No such endpoint.")
        except Refusal as e: self.failure(e)
        except (ValueError, json.JSONDecodeError): self.failure(Refusal(422, "bad_request", "The request is not valid JSON."))
        except Exception as e: self.unexpected(e)


def main() -> None:
    p = argparse.ArgumentParser(); p.add_argument("--control", type=Path, default=Path("/etc/orrery/campaigns.conf")); p.add_argument("--state", type=Path, default=Path("/var/lib/orrery-admission")); p.add_argument("--invite", default="orrery-invite"); p.add_argument("--ssh", default="ssh"); p.add_argument("--ssh-key", type=Path, default=Path("/var/lib/orrery-admission/campaign_ssh_key")); p.add_argument("--issuer", type=Path, default=Path("/var/lib/orrery-admission/issuer.cred")); p.add_argument("--swarm", default="p1-swarm"); p.add_argument("--standing-host-state", type=Path, default=Path("/var/lib/orrery-p1-swarm")); p.add_argument("--public-origin", default=""); p.add_argument("--listen", default="127.0.0.1:8323"); p.add_argument("--self-test", action="store_true"); a = p.parse_args()
    if a.self_test:
        loader = unittest.TestLoader()
        selected = os.environ.get("ADMISSION_TEST")
        suite = (loader.loadTestsFromName(selected, module=sys.modules[__name__])
                 if selected else loader.loadTestsFromTestCase(AdmissionTests))
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        if result.testsRun == 0:
            raise SystemExit("admission self-test ran zero tests")
        if len(result.skipped) == result.testsRun:
            raise SystemExit(f"admission self-test skipped all {result.testsRun} tests")
        raise SystemExit(0 if result.wasSuccessful() else 1)
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    host, port = a.listen.rsplit(":", 1); Handler.service = Admission(a.control, a.state, a.invite, a.ssh, a.ssh_key, a.issuer, a.swarm, a.standing_host_state)
    server = ThreadingHTTPServer((host, int(port)), Handler)
    if a.public_origin:
        # The probe needs the service answering behind the public origin, so a
        # temporary server thread carries it while the gate runs (#1002); a
        # failed gate exits the process before steady serving starts.
        keeper = threading.Thread(target=server.serve_forever, daemon=True); keeper.start()
        enforce_upload_limit(a.public_origin)
        server.shutdown(); keeper.join()
    else: logging.info("upload-limit self-check is off: no --public-origin configured, so the effective upstream body limit cannot be verified (#1002)")
    server.serve_forever()


class AdmissionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(); root = Path(self.tmp.name); self.state, self.control = root / "state", root / "campaigns.conf"
        self.standing_host_state = root / "standing-host"
        self.control.write_text("[test]\ntitle = Test\nopen = yes\nhost = 203.0.113.7\nexternal_port = 52011\npeers = 4\nhumans = 4\nseconds = 60\nloss_pct = 3\njitter_ms = 100\nclient_rev = rev\nruleset_version = 14\n")
        repo = Path(__file__).parents[1]; self.invite = repo / "target/debug/orrery-invite"; issuer_key = repo / "target/debug/orrery-issuer-key"
        if not self.invite.exists() or not issuer_key.exists(): self.skipTest("build orrery-invite and orrery-issuer-key before self-test")
        self.issuer = root / "issuer"; subprocess.run([str(issuer_key), "generate", "--key-id", "476", "--output", str(self.issuer)], check=True, capture_output=True)
        self.ssh = root / "ssh"; self.ssh.write_text("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/ssh.args\"\ncase \" $* \" in *' cat '*listening.txt*) echo 'f2a1 0.0.0.0:52011';; *' cat '*raw.json*) echo '{}';; *) sleep 60;; esac\n"); self.ssh.chmod(0o755)
        self.service = Admission(self.control, self.state, str(self.invite), str(self.ssh), root / "key", self.issuer, "swarm", self.standing_host_state, lambda _: type("V", (), {"f_bavail": 20 * 1024**3, "f_frsize": 1})())
    def tearDown(self) -> None:
        self.service.shutdown()
        for lock in self.service.locks.values(): lock.close()
        self.tmp.cleanup()
    def request(self) -> dict[str, Any]: return {"nickname": "ada", "node": "a" * 64, "client_rev": "rev", "ruleset_version": 14}
    def enable_always_on(self) -> dict[str, Any]:
        self.control.write_text(self.control.read_text() + "always_on = yes\n")
        attempt = {"attempt_id": "test-attempt", "started": int(time.time()), "expires_at": int(time.time()) + 900}
        host = self.standing_host_state / "test"; host.mkdir(parents=True, exist_ok=True)
        (host / "attempt.json").write_text(json.dumps(attempt))
        (host / "listening.txt").write_text("f2a1 0.0.0.0:52011\n")
        return attempt
    def publish_seats(self, attempt: dict[str, Any], *, active: tuple[int, ...] = (),
                      pending: tuple[int, ...] = (),
                      released: tuple[str, ...] = (), released_at: dict[str, int] | None = None,
                      running: bool = True) -> None:
        """Write one host membership publication, exactly as the harness does."""
        (self.standing_host_state / "test" / "active-seats.json").write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": list(active),
            "pending_slots": list(pending),
            "released_sessions": list(released), "released_at": released_at or {},
            "running": running}))

    def one_seat(self) -> None:
        """Narrow the campaign to a single human seat, so `full` is one drop away."""
        self.control.write_text(self.control.read_text().replace("humans = 4", "humans = 1"))

    def recorded_harness_command(self) -> str:
        args = self.ssh.parent / "ssh.args"
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            if args.exists():
                for line in args.read_text().splitlines():
                    if "--require-session" in line: return line
            time.sleep(0.001)
        self.fail("harness command was not recorded after join returned")
    def test_a_busy_legacy_campaign_refuses_the_second_join_with_409(self) -> None:
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
        harness = self.recorded_harness_command()
        fields = shlex.split(harness)
        remote = f"/var/tmp/orrery/{sid}"
        self.assertIn("mkdir", fields, f"the harness launch does not create {remote}: {harness}")
        self.assertLess(fields.index("mkdir"), fields.index("--external-peer"),
                        f"mkdir must precede the harness: {harness}")
        self.assertIn(remote, fields)

    def test_the_harness_is_pinned_to_exactly_the_admitted_session_id(self) -> None:
        answer = self.service.join("test", self.request()); sid = answer["join"]["session_id"]
        harness = self.recorded_harness_command()
        fields = shlex.split(harness)
        self.assertEqual(fields[fields.index("--require-session") + 1], sid)
    def test_the_harness_uses_the_campaigns_fixed_external_port(self) -> None:
        answer = self.service.join("test", self.request())
        harness = self.recorded_harness_command()
        fields = shlex.split(harness)
        self.assertEqual(fields[fields.index("--external-bind") + 1], "0.0.0.0:52011")
        self.assertEqual(answer["host_direct"], "203.0.113.7:52011")

    def test_an_always_on_campaign_reads_the_standing_host_without_launching_one(self) -> None:
        attempt = self.enable_always_on()
        attempt["started"] -= LOBBY_SECONDS + 1
        (self.standing_host_state / "test" / "attempt.json").write_text(json.dumps(attempt))
        listing = self.service.listing()["campaigns"][0]
        self.assertEqual((listing["state"], listing["phase"], listing["slots_free"]),
                         ("open", "lobby", 4), "an empty standing host remains joinable")
        answer = self.service.join("test", self.request())
        self.assertEqual(answer["host_direct"], "203.0.113.7:52011")
        self.assertEqual(answer["join"]["slot"], 4)
        self.assertFalse((self.ssh.parent / "ssh.args").exists(),
                         "a co-located standing host must not depend on self-SSH")
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
    def test_client_and_ruleset_mismatches_refuse_before_minting(self) -> None:
        client_mismatch = self.request(); client_mismatch["client_rev"] = "old"
        with self.assertRaises(Refusal) as x: self.service.join("test", client_mismatch)
        self.assertEqual(x.exception.error, "client_rev_mismatch")
        ruleset_mismatch = self.request(); ruleset_mismatch["ruleset_version"] = 13
        with self.assertRaises(Refusal) as x: self.service.join("test", ruleset_mismatch)
        self.assertEqual(x.exception.error, "ruleset_version_mismatch")
        # The new compatibility guard stays before campaign availability guards.
        self.control.write_text(self.control.read_text().replace("open = yes", "open = no"))
        with self.assertRaises(Refusal) as x: self.service.join("test", ruleset_mismatch)
        self.assertEqual(x.exception.error, "ruleset_version_mismatch")
        self.control.write_text(self.control.read_text().replace("open = no", "open = yes"))
        self.service.statvfs = lambda _: type("V", (), {"f_bavail": 0, "f_frsize": 1})()
        with self.assertRaises(Refusal) as x: self.service.join("test", ruleset_mismatch)
        self.assertEqual(x.exception.error, "ruleset_version_mismatch")
        self.assertFalse((self.state / "test" / "ledger.tsv").exists())

        self.service.statvfs = lambda _: type("V", (), {"f_bavail": 20 * 1024**3, "f_frsize": 1})()
        r = self.request(); r["nickname"] = "a\tb"
        with self.assertRaises(Refusal) as x: self.service.join("test", r)
        self.assertEqual(x.exception.status, 422); self.assertFalse((self.state / "test" / "ledger.tsv").exists())
        r = self.request(); r["nickname"] = "Ren\N{LATIN SMALL LETTER E WITH ACUTE}e"
        with self.assertRaises(Refusal) as x: self.service.join("test", r)
        self.assertEqual(x.exception.status, 422); self.assertFalse((self.state / "test" / "ledger.tsv").exists())
    def test_the_roster_names_every_craft_including_an_opponent(self) -> None:
        # The label the client draws on a ship comes from here, and the slot is
        # the only thing that maps it to a craft (entity = slot + 1, client
        # side). The eight harness slots are the opposition; checking only the
        # exterior slot would preserve #523's exact blind spot.
        c = self.service.campaigns()[0]["test"]
        self.assertEqual(self.service.roster("test")["roster"], [], "nobody has joined yet")
        self.service.join("test", self.request())
        roster = self.service.roster("test")["roster"]
        self.assertEqual(len(roster), c.peers + 1, "every harness craft and the exterior craft need a label")
        self.assertEqual(roster[0], {"slot": 0, "nickname": "test-1"}, "an opponent must be named")
        self.assertEqual(roster[-1], {"slot": c.peers, "nickname": "ada"})

    def test_every_roster_label_takes_the_same_display_sanitiser(self) -> None:
        self.assertEqual(display_label("  ada\N{BELL}  "), "ada")
        self.assertEqual(display_label("Ren\N{LATIN SMALL LETTER E WITH ACUTE}e"), "Rene")
        self.assertIsNone(display_label("\N{LATIN SMALL LETTER E WITH ACUTE}\N{RIGHT-TO-LEFT OVERRIDE}"))
        c = self.service.campaigns()[0]["test"]
        roster = self.service.legacy_session_roster(c, "  ad\N{LATIN SMALL LETTER E WITH ACUTE}a  ")
        self.assertEqual(roster[c.peers], "ada")
        self.assertTrue(all(display_label(label) == label for label in roster.values()))

    def test_always_on_allocator_is_ascending_idempotent_and_never_duplicates_a_seat(self) -> None:
        self.enable_always_on()
        first = self.service.join("test", self.request())
        retry = self.service.join("test", self.request())
        second_request = self.request(); second_request.update({"nickname": "lin", "node": "b" * 64})
        second = self.service.join("test", second_request)
        self.assertEqual((first["join"]["slot"], second["join"]["slot"]), (4, 5))
        self.assertEqual((retry["join"]["slot"], retry["join"]["session_id"]),
                         (first["join"]["slot"], first["join"]["session_id"]))
        slots = json.loads((self.state / "test" / "slots.json").read_text())
        self.assertEqual([row["slot"] for row in slots], [4, 5])

    def test_join_reply_echoes_the_granted_nickname_and_a_retry_cannot_rename_it(self) -> None:
        self.enable_always_on()
        first = self.service.join("test", self.request())
        renamed = self.request(); renamed["nickname"] = "shooshte"
        retry = self.service.join("test", renamed)
        self.assertEqual(first["nickname"], "ada")
        self.assertEqual(retry["nickname"], "ada",
                         "the reply must echo the existing grant, not the retry's request")
        self.assertEqual((retry["join"]["slot"], retry["join"]["session_id"]),
                         (first["join"]["slot"], first["join"]["session_id"]))

    def test_always_on_full_roster_maps_all_eight_seats_and_refuses_the_ninth_reservation(self) -> None:
        self.enable_always_on()
        for number in range(4):
            request = self.request(); request.update({"nickname": f"p{number}", "node": f"{number:x}" * 64})
            self.service.join("test", request)
        listing = self.service.listing()["campaigns"][0]
        self.assertEqual((listing["state"], listing["slots_free"]), ("full", 0))
        roster = self.service.roster("test")["roster"]
        self.assertEqual(len(roster), 8)
        self.assertEqual([seat["state"] for seat in roster[4:]], ["reserved"] * 4)
        request = self.request(); request.update({"nickname": "late", "node": "f" * 64})
        with self.assertRaises(Refusal) as caught: self.service.join("test", request)
        self.assertEqual((caught.exception.status, caught.exception.error), (409, "campaign_full"))

    def test_always_on_roster_marks_only_host_bound_seats_active(self) -> None:
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        second_request = self.request(); second_request.update({"nickname": "lin", "node": "b" * 64})
        second = self.service.join("test", second_request)
        self.assertEqual([seat["state"] for seat in self.service.roster("test")["roster"][4:]],
                         ["reserved", "reserved", "empty", "empty"],
                         "a reservation alone is not a connection claim")
        (self.standing_host_state / "test" / "active-seats.json").write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [first["join"]["slot"]],
            "running": False,
        }))
        self.assertEqual([seat["state"] for seat in self.service.roster("test")["roster"][4:]],
                         ["active", "reserved", "empty", "empty"],
                         "only the seat bound by the host is active")
        (self.standing_host_state / "test" / "active-seats.json").write_text(json.dumps({
            "attempt_id": "stale-attempt", "active_slots": [first["join"]["slot"], second["join"]["slot"]],
            "running": False,
        }))
        self.assertEqual([seat["state"] for seat in self.service.roster("test")["roster"][4:]],
                         ["reserved", "reserved", "empty", "empty"],
                         "a prior generation must not assert present membership")

    def test_the_join_answer_derives_both_jitter_percentiles_from_the_spike(self) -> None:
        # #1030. The campaign config carries one `jitter_ms`, and this file used
        # to send it as both percentiles. The host holds a tenth of datagrams
        # for the whole spike and the rest not at all, so the median added delay
        # is zero and only the p99 sees the spike. Sending 100 for both asserted
        # something no distribution satisfies, and every honest session of
        # 2026-09-04 banked with `impairment_mismatch: true`.
        campaign = self.service.campaigns()[0]["test"]
        self.assertEqual(campaign.jitter_ms, 100)
        self.assertEqual(configured_impairment(campaign),
                         {"loss_pct": 3, "jitter_p50_ms": 0, "jitter_p99_ms": 100})
        # The wire schema is unchanged: same three keys, in the same block.
        answer = self.service.join("test", self.request())
        self.assertEqual(answer["configured"], configured_impairment(campaign))

    def test_the_spike_rate_is_the_one_the_host_router_applies(self) -> None:
        # A derived profile is only honest while it derives from what the host
        # does. `gates/p1-swarm/src/router.rs` owns the number; if the profile
        # there changes rate, this must move with it or the coordinator resumes
        # advertising a distribution nobody applies.
        router = (Path(__file__).parents[1] / "gates/p1-swarm/src/router.rs").read_text()
        found = re.search(r"fn p4_profile_at_loss\(.*?jitter_rate: ([0-9.]+),", router, re.S)
        self.assertIsNotNone(found, "cannot read the P4 profile's jitter_rate out of router.rs")
        self.assertEqual(float(found.group(1)), HOST_JITTER_SPIKE_RATE,
                         "the host's spike rate drifted from the advertised profile")
        session_rs = (Path(__file__).parents[1] / "clients/regolith/src/session.rs").read_text()
        mirrored = re.search(r"^pub const HOST_JITTER_SPIKE_RATE: f64 = ([0-9.]+);$", session_rs, re.M)
        self.assertIsNotNone(mirrored, "cannot read HOST_JITTER_SPIKE_RATE out of session.rs")
        self.assertEqual(float(mirrored.group(1)), HOST_JITTER_SPIKE_RATE,
                         "the client derives its expectation from a different rate")

    def test_the_listing_and_the_roster_agree_on_which_seats_are_taken(self) -> None:
        # `slots_free` and the roster answered "is this seat taken" from
        # different sources, so the listing could report a seat taken that the
        # roster drew empty and refuse a player the lobby had just offered a
        # seat to (#713). They now share one definition, and this pins that
        # they cannot drift apart again while the campaign is joinable.
        #
        # `restarting` is deliberately exempt: it reports zero free seats by
        # design, whatever the roster last knew, because there is no host
        # generation to admit anyone into.
        attempt = self.enable_always_on()
        c = self.service.campaigns()[0]["test"]

        def agree(note: str) -> None:
            listing = self.service.listing()["campaigns"][0]
            if listing["phase"] == "restarting":
                return
            drawn_taken = sum(1 for seat in self.service.roster("test")["roster"]
                              if seat["kind"] == "human" and seat["state"] != "empty")
            self.assertEqual(listing["slots_free"], c.humans - drawn_taken, note)

        agree("an empty lobby agrees")
        joined = self.service.join("test", self.request())
        slot = joined["join"]["slot"]
        agree("a reserved seat agrees")

        active = self.standing_host_state / "test" / "active-seats.json"
        active.write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [slot],
            "released_sessions": [], "running": True,
        }))
        agree("a bound seat agrees while the attempt runs")

        # Bound, and its arrival lease long gone: the count used to keep this
        # row through the bound branch while the roster's reservation filter
        # dropped it for being expired.
        slots_path = self.state / "test" / "slots.json"
        rows = json.loads(slots_path.read_text())
        for row in rows:
            if row.get("slot") == slot:
                row["expires_at"] = int(time.time()) - 1
        slots_path.write_text(json.dumps(rows))
        agree("a bound seat whose lease expired agrees")

        active.write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [],
            "released_sessions": [], "running": True,
        }))
        agree("a released seat agrees")

    def test_a_bound_seat_keeps_its_label_when_the_attempt_pointer_moves(self) -> None:
        attempt = self.enable_always_on()
        joined = self.service.join("test", self.request())
        active = self.standing_host_state / "test" / "active-seats.json"
        active.write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [joined["join"]["slot"]],
            "released_sessions": [], "running": True,
        }))

        moved = dict(attempt)
        moved["attempt_id"] = "next-attempt"
        (self.standing_host_state / "test" / "attempt.json").write_text(json.dumps(moved))
        seat = self.service.roster("test")["roster"][joined["join"]["slot"]]
        self.assertEqual((seat["state"], seat["nickname"]), ("active", "ada"),
                         "a bound seat keeps its label across an attempt-pointer hand-off")

        slots_path = self.state / "test" / "slots.json"
        rows = json.loads(slots_path.read_text())
        rows.append({
            "attempt_id": moved["attempt_id"], "slot": joined["join"]["slot"],
            "session_id": "018f8f4e-5c90-7abc-8123-0000000000ab",
            "node": "b" * 64, "nickname": "lin",
            "expires_at": int(time.time()) + ARRIVAL_LEASE_SECONDS,
        })
        slots_path.write_text(json.dumps(rows))
        seat = self.service.roster("test")["roster"][joined["join"]["slot"]]
        self.assertEqual((seat["state"], seat["nickname"]), ("active", "ada"),
                         "another generation cannot rename the seat the host still binds")

        (self.standing_host_state / "test" / "attempt.json").unlink()
        seat = self.service.roster("test")["roster"][joined["join"]["slot"]]
        self.assertEqual((seat["state"], seat["nickname"]), ("active", "ada"),
                         "a transiently absent pointer must not blank a bound seat")

        active.write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [],
            "released_sessions": [joined["join"]["session_id"]], "running": True,
        }))
        seat = self.service.roster("test")["roster"][joined["join"]["slot"]]
        self.assertEqual((seat["state"], seat["nickname"]), ("empty", None),
                         "the label disappears when the host releases the binding")

    def test_running_campaign_stays_joinable_while_a_slot_is_unbound(self) -> None:
        self.control.write_text(self.control.read_text() + "lobby_seconds = 12\n")
        attempt = self.enable_always_on()
        attempt["started"] -= 13
        (self.standing_host_state / "test" / "attempt.json").write_text(json.dumps(attempt))
        (self.state / "test").mkdir(parents=True, exist_ok=True)
        (self.state / "test" / "slots.json").write_text(json.dumps([{
            "attempt_id": attempt["attempt_id"], "slot": 4,
            "session_id": "018f8f4e-5c90-7abc-8123-0000000000ab",
            "node": "b" * 64, "expires_at": int(time.time()) + ARRIVAL_LEASE_SECONDS,
        }]))
        (self.standing_host_state / "test" / "active-seats.json").write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [4],
            "released_sessions": [], "running": True,
        }))
        listing = self.service.listing()["campaigns"][0]
        self.assertEqual((listing["state"], listing["phase"], listing["slots_free"]),
                         ("open", "running", 3),
                         "a running campaign with an unbound slot must remain joinable")
        self.assertEqual(self.service.campaigns()[0]["test"].lobby_seconds, 12,
                         "admission must use the same configured initial delay as the host")
        answer = self.service.join("test", self.request())
        self.assertEqual(answer["join"]["slot"], 5)

    def expire_every_reservation(self) -> None:
        """Age every row past its arrival lease without waiting one out.

        The lease is 45 s and a lobby is 180 s, so the interesting instant is
        always "after the lease, during the lobby".  Naming it by moving the
        rows is deterministic; sleeping it is a slow flaky test.
        """
        path = self.state / "test" / "slots.json"
        rows = json.loads(path.read_text())
        for row in rows:
            row["expires_at"] = int(time.time()) - 1
        path.write_text(json.dumps(rows))

    def test_a_lobby_seat_the_host_holds_outlives_its_arrival_lease(self) -> None:
        # #1016. The ordinary join: a volunteer reserves a seat, dials the
        # host, and waits in the lobby for the run to fill.  The lobby is
        # `lobby_seconds` (180 s) and the arrival lease is 45 s, so a quarter of
        # the way in their row used to be dropped and their seat re-offered --
        # and the volunteer who took the offer was then refused by the host
        # with `reservation_slot_occupied`, because the first player was
        # correctly still sitting in it.  Neither of them could act on that.
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        slot = first["join"]["slot"]
        self.publish_seats(attempt, pending=(slot,), running=False)
        self.expire_every_reservation()

        self.assertEqual([seat["state"] for seat in self.service.roster("test")["roster"][4:]],
                         ["reserved", "empty", "empty", "empty"],
                         "a seat the host holds a lobby connection for is taken, "
                         "and reserved rather than active: nothing is bound to it yet")
        listing = self.service.listing()["campaigns"][0]
        self.assertEqual((listing["phase"], listing["slots_free"]), ("lobby", 3),
                         "the lobby stays joinable, with the held seat counted taken")

        second_request = self.request(); second_request.update({"nickname": "lin", "node": "b" * 64})
        second = self.service.join("test", second_request)
        self.assertNotEqual(second["join"]["slot"], slot,
                            "the seat under a waiting volunteer must never be re-offered")
        self.assertEqual(second["join"]["slot"], slot + 1)
        rows = json.loads((self.state / "test" / "slots.json").read_text())
        self.assertEqual(sorted(row["slot"] for row in rows), [slot, slot + 1],
                         "the waiting volunteer's row survives its arrival lease")

        # And the seat is still theirs when the run finally starts.
        self.publish_seats(attempt, active=(slot,), pending=(slot + 1,), running=True)
        self.assertEqual([seat["state"] for seat in self.service.roster("test")["roster"][4:]],
                         ["active", "reserved", "empty", "empty"])

    def test_a_seat_the_host_stops_holding_frees_even_though_the_lobby_runs_on(self) -> None:
        # The other direction, and why this is not "make the lease longer".
        # The host holds a seat for exactly as long as it has a connection for
        # it; a volunteer who never dialled, or whose connection lapsed and was
        # swept, is named by nothing, and the arrival lease -- which is what it
        # is actually for -- frees the seat inside one lobby window.
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        slot = first["join"]["slot"]
        self.publish_seats(attempt, pending=(slot,), running=False)
        self.expire_every_reservation()
        self.publish_seats(attempt, running=False)

        second_request = self.request(); second_request.update({"nickname": "lin", "node": "b" * 64})
        second = self.service.join("test", second_request)
        self.assertEqual(second["join"]["slot"], slot,
                         "a seat nobody is arriving to is freed by the arrival lease")
        rows = json.loads((self.state / "test" / "slots.json").read_text())
        self.assertEqual(len(rows), 1, "the abandoned row is dropped, not merely hidden")

    def test_a_feed_that_calls_one_seat_both_bound_and_held_is_refused(self) -> None:
        # A slot cannot be bound and merely connected at once; the host removes
        # it from one set in the same publication that adds it to the other.
        # A feed that says both is not readable, and the safe reading of an
        # unreadable feed is that this generation admits nobody.
        attempt = self.enable_always_on()
        self.service.join("test", self.request())
        self.publish_seats(attempt, active=(4,), pending=(4,), running=False)
        self.assertEqual(self.service.listing()["campaigns"][0]["phase"], "restarting")
        (self.standing_host_state / "test" / "active-seats.json").write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [], "pending_slots": [99],
            "released_sessions": [], "released_at": {}, "running": False}))
        self.assertEqual(self.service.listing()["campaigns"][0]["phase"], "restarting",
                         "a held seat outside the human range is not a seat")

    def test_a_host_that_publishes_no_held_seats_behaves_exactly_as_before(self) -> None:
        # `pending_slots` is additive: a host too old to publish it holds no
        # seats that way, and every row falls back on its arrival lease.  That
        # is the pre-#1016 reading, and it is the safe direction.
        attempt = self.enable_always_on()
        self.service.join("test", self.request())
        (self.standing_host_state / "test" / "active-seats.json").write_text(json.dumps({
            "attempt_id": attempt["attempt_id"], "active_slots": [],
            "released_sessions": [], "running": False}))
        self.expire_every_reservation()
        second_request = self.request(); second_request.update({"nickname": "lin", "node": "b" * 64})
        self.assertEqual(self.service.join("test", second_request)["join"]["slot"], 4)

    def test_no_show_reservation_expires_after_the_arrival_lease(self) -> None:
        attempt = self.enable_always_on()
        directory = self.state / "test"; directory.mkdir(parents=True, exist_ok=True)
        (directory / "slots.json").write_text(json.dumps([{
            "attempt_id": attempt["attempt_id"], "slot": 4,
            "session_id": "018f8f4e-5c90-7abc-8123-0000000000ab",
            "node": "b" * 64, "expires_at": int(time.time()) - 1,
        }]))
        answer = self.service.join("test", self.request())
        self.assertEqual(answer["join"]["slot"], 4,
                         "an expired no-show must not consume the earliest free slot")
        rows = json.loads((directory / "slots.json").read_text())
        self.assertEqual(len(rows), 1, "the expired no-show row must be removed")

    def test_host_release_makes_the_departed_slot_immediately_reservable(self) -> None:
        # A release the host published no `released_at` for — an explicit
        # goodbye, a run-time departure, or a host too old to say — is spent on
        # sight. #1001's reissue window is opt-in per release, and this is the
        # path everything that is not a lobby lapse still takes.
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        active = self.standing_host_state / "test" / "active-seats.json"
        active.write_text(json.dumps({"attempt_id": attempt["attempt_id"],
                                      "active_slots": [4], "released_sessions": [],
                                      "running": True}))
        active.write_text(json.dumps({"attempt_id": attempt["attempt_id"],
                                      "active_slots": [],
                                      "released_sessions": [first["join"]["session_id"]],
                                      "running": True}))
        second_request = self.request(); second_request.update({"nickname": "lin", "node": "b" * 64})
        second = self.service.join("test", second_request)
        self.assertEqual(second["join"]["slot"], 4,
                         "the host's unbind publication must release the real allocator row")

    def test_a_lapsed_lobby_peer_redials_into_the_reservation_it_already_had(self) -> None:
        # #1001's decision. A volunteer whose connection lapsed for eight
        # seconds gets their own seat back by redialling, and spends no invite
        # doing it: the same session, the same account, the same slot.
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        session = first["join"]["session_id"]
        self.publish_seats(attempt, active=(4,))
        # Long enough in the lobby that the arrival lease is gone: the window,
        # not the lease, is what holds this seat now.
        self.publish_seats(attempt, released=(session,),
                           released_at={session: int(time.time()) - RECLAIM_GRACE_SECONDS + 5})
        rows = json.loads((self.state / "test" / "slots.json").read_text())
        for row in rows:
            row["expires_at"] = int(time.time()) - 1
        (self.state / "test" / "slots.json").write_text(json.dumps(rows))

        ledger = self.state / "test" / "ledger.tsv"
        minted = ledger.read_text()

        again = self.service.join("test", self.request())
        self.assertEqual(again["join"]["session_id"], session,
                         "the redial must reissue the reservation, not mint another")
        self.assertEqual(again["join"]["slot"], first["join"]["slot"])
        self.assertEqual(again["account"], first["account"])
        self.assertGreater(len(again["join"]["session_token"]), 100,
                           "and it must carry a freshly signed token for the same node")

        rows = json.loads((self.state / "test" / "slots.json").read_text())
        self.assertEqual([row["session_id"] for row in rows], [session],
                         "exactly one reservation, the one that was already there")
        self.assertGreater(rows[0]["expires_at"], int(time.time()),
                           "the host's journal refuses an expired row, so an arriving "
                           "volunteer needs their arrival lease back")
        banked = [json.loads(line) for line in
                  (self.state / "test" / "joins.jsonl").read_text().splitlines()]
        self.assertEqual([row.get("reissued") for row in banked], [None, True],
                         "the reissue is filed, so an operator can see the window firing")
        self.assertEqual(ledger.read_text(), minted,
                         "reissuing must not mint a second invite")

    def test_the_reissue_window_closes_on_the_second_it_says(self) -> None:
        # The boundary, from both sides, with no clock to wait out. One second
        # inside the window the seat is the lapsed volunteer's; on the second it
        # closes it belongs to whoever asks.
        attempt = self.enable_always_on()
        self.one_seat()
        first = self.service.join("test", self.request())
        session = first["join"]["session_id"]
        stranger = self.request(); stranger.update({"nickname": "lin", "node": "b" * 64})

        self.publish_seats(attempt, active=(4,))
        self.publish_seats(attempt, released=(session,),
                           released_at={session: int(time.time()) - RECLAIM_GRACE_SECONDS + 1})
        with self.assertRaises(Refusal) as caught:
            self.service.join("test", stranger)
        self.assertEqual((caught.exception.status, caught.exception.error),
                         (409, "seat_held_for_reconnect"),
                         "a different transport identity is refused inside the window")
        self.assertIn("try again in a moment", caught.exception.detail.lower(),
                      "and told what to do about it")
        self.assertNotIn("retry_after_s", caught.exception.extra,
                         "shipped clients render that as `Next lobby in ...`, and this is "
                         "the same lobby")
        self.assertEqual([row["session_id"] for row in
                          json.loads((self.state / "test" / "slots.json").read_text())],
                         [session], "and the held reservation is untouched by the attempt")

        self.publish_seats(attempt, released=(session,),
                           released_at={session: int(time.time()) - RECLAIM_GRACE_SECONDS})
        taken = self.service.join("test", stranger)
        self.assertEqual(taken["join"]["slot"], 4,
                         "once the window closes the seat genuinely frees for somebody else")
        self.assertNotEqual(taken["join"]["session_id"], session,
                            "and it is their own session, never the one that was held")

    def test_an_expired_window_does_not_reissue_to_the_identity_that_held_it(self) -> None:
        # The other half of the boundary: the volunteer who lapsed comes back
        # too late. Their old session is spent — they are a new joiner now, and
        # get a seat only if one is free.
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        session = first["join"]["session_id"]
        self.publish_seats(attempt, active=(4,))
        self.publish_seats(attempt, released=(session,),
                           released_at={session: int(time.time()) - RECLAIM_GRACE_SECONDS})

        again = self.service.join("test", self.request())
        self.assertNotEqual(again["join"]["session_id"], session,
                            "a redial after the window cannot resume the spent reservation")
        rows = json.loads((self.state / "test" / "slots.json").read_text())
        self.assertEqual([row["session_id"] for row in rows], [again["join"]["session_id"]],
                         "and the expired row is gone rather than lingering beside its successor")

    def test_a_held_seat_is_drawn_reserved_and_counted_taken(self) -> None:
        # #713's invariant across the new state: the count and the roster
        # answer from one definition, so a held seat can never be `full` in the
        # listing and an empty chair in the lobby.
        attempt = self.enable_always_on()
        joined = self.service.join("test", self.request())
        session = joined["join"]["session_id"]
        self.publish_seats(attempt, active=(4,))
        self.publish_seats(attempt, released=(session,),
                           released_at={session: int(time.time())})

        seat = self.service.roster("test")["roster"][joined["join"]["slot"]]
        self.assertEqual((seat["state"], seat["nickname"]), ("reserved", "ada"),
                         "the seat is held for a volunteer who is not in it: reserved, "
                         "not active and not empty")
        self.assertEqual(self.service.listing()["campaigns"][0]["slots_free"], 3,
                         "and the listing counts it taken, exactly as the roster draws it")

    def test_an_unstamped_or_malformed_release_never_holds_a_seat(self) -> None:
        # A reissue window is only worth the timestamp under it. Anything the
        # host could not say cleanly frees the seat, which is the safe
        # direction: the pre-#1001 behaviour.
        attempt = self.enable_always_on()
        joined = self.service.join("test", self.request())
        session = joined["join"]["session_id"]
        self.publish_seats(attempt, active=(4,))

        active = self.standing_host_state / "test" / "active-seats.json"
        for broken in ({session: "soon"}, {session: True}, {session: -1},
                       {"018f8f4e-5c90-7abc-8123-0000000000ab": int(time.time())}):
            active.write_text(json.dumps({
                "attempt_id": attempt["attempt_id"], "active_slots": [],
                "released_sessions": [session], "released_at": broken, "running": True}))
            listing = self.service.listing()["campaigns"][0]
            self.assertEqual((listing["state"], listing["slots_free"]), ("restarting", 0),
                             f"a feed this service cannot read must fail closed: {broken}")

        self.publish_seats(attempt, released=(session,))
        self.assertEqual(self.service.listing()["campaigns"][0]["slots_free"], 4,
                         "and a release with no window at all frees the seat at once")

    def test_malformed_host_membership_fails_capacity_closed(self) -> None:
        self.enable_always_on()
        active = self.standing_host_state / "test" / "active-seats.json"
        active.write_text("{broken")
        listing = self.service.listing()["campaigns"][0]
        self.assertEqual((listing["state"], listing["slots_free"]), ("restarting", 0))
        with self.assertRaises(Refusal) as caught:
            self.service.join("test", self.request())
        self.assertEqual((caught.exception.status, caught.exception.error), (503, "host_failed"))

    def test_an_unreadable_reservation_journal_is_not_an_empty_one(self) -> None:
        # #1150. `slots.json` is admission's own authoritative reservation
        # journal and it used to be the one feed in this file that failed open:
        # any `OSError` or a torn document read as "no seats reserved", the
        # listing advertised the campaign wide open, and the next join
        # durably rewrote the file with a single row -- taking out every live
        # reservation on the campaign at once.  The suite only ever removed the
        # file or wrote valid JSON into it, so nothing caught it.
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        self.publish_seats(attempt, active=(first["join"]["slot"],))
        journal = self.state / "test" / "slots.json"
        intact = journal.read_text()
        journal.write_text(intact[:len(intact) // 2])  # a short write, as a full disk makes

        listing = self.service.listing()["campaigns"][0]
        self.assertEqual((listing["state"], listing["phase"], listing["slots_free"]),
                         ("restarting", "restarting", 0),
                         "a journal we could not read must never advertise free seats")

        stranger = self.request(); stranger.update({"nickname": "lin", "node": "b" * 64})
        with self.assertRaises(Refusal) as caught:
            self.service.join("test", stranger)
        self.assertEqual((caught.exception.status, caught.exception.error), (503, "host_failed"),
                         "the same posture the host-membership feed already takes")
        self.assertEqual(journal.read_text(), intact[:len(intact) // 2],
                         "nothing may write over state it could not read")

    def test_a_reservation_journal_that_cannot_be_opened_refuses_rather_than_empties(self) -> None:
        # The other half of #1150: not a torn document but a failed `read` --
        # `EMFILE` from a burst against the threaded server, `EIO`, `EACCES`.
        # `OSError` is the whole family and all of it used to become `[]`.
        attempt = self.enable_always_on()
        self.service.join("test", self.request())
        journal = self.state / "test" / "slots.json"
        journal.unlink(); journal.mkdir()  # any OSError that is not FileNotFoundError
        self.assertIsNone(self.service._read_slots(self.service.campaigns()[0]["test"]))
        self.assertEqual(self.service._campaign_phase(self.service.campaigns()[0]["test"], attempt)[0],
                         "restarting")

    def test_a_missing_reservation_journal_is_still_an_empty_one(self) -> None:
        # And the fail-closed posture must not swallow the legitimate empty
        # case: a campaign nobody has joined has no `slots.json` at all, and
        # that is a real answer of "no seats reserved", not a read failure.
        attempt = self.enable_always_on()
        self.assertFalse((self.state / "test" / "slots.json").exists())
        listing = self.service.listing()["campaigns"][0]
        self.assertEqual((listing["phase"], listing["slots_free"]), ("lobby", 4))
        self.assertEqual(self.service.join("test", self.request())["join"]["slot"], 4)
        self.assertEqual(attempt["attempt_id"], "test-attempt")

    def test_one_damaged_join_line_does_not_hide_the_sessions_after_it(self) -> None:
        # #1151. `joins.jsonl` is append-ordered and the whole scan used to sit
        # inside one `try`; `any()` is lazy, so the first `JSONDecodeError`
        # escaped the generator and ended the file *at the bad line*.  Every
        # session admitted afterwards was permanently unknown and every one of
        # their evidence uploads was refused `404 unknown_session` forever --
        # a cohort, not a seat.  A full disk truncating the last line is the
        # realistic way in, and this service deliberately keeps appending here
        # below `MINT_FLOOR_BYTES`.
        self.enable_always_on()
        early = self.service.join("test", self.request())["join"]["session_id"]
        joins = self.state / "test" / "joins.jsonl"
        with joins.open("a", encoding="utf-8") as f:
            f.write('{"when":123,"session_id":"018f8f4e-5c90-7abc-81')  # a short write
            f.write("\n")
        later = self.request(); later.update({"nickname": "lin", "node": "b" * 64})
        late = self.service.join("test", later)["join"]["session_id"]

        self.assertTrue(self.service.known_session(early), "the session before the damage")
        self.assertTrue(self.service.known_session(late),
                        "one damaged line must cost one line, not the rest of the file")
        body = json.dumps({"records": [{"session_id": late}], "telemetry_jsonl": "x"}).encode()
        self.service.upload(late, body)  # would have raised 404 unknown_session

    def test_a_join_line_that_is_not_an_object_is_skipped_not_raised(self) -> None:
        # The narrower hole on the same lines: a line that is valid JSON but
        # not an object raises `AttributeError`/`TypeError` from the member
        # access, and neither was in either `except` tuple -- so it propagated
        # out of the request handler entirely rather than being refused.
        self.enable_always_on()
        sid = self.service.join("test", self.request())["join"]["session_id"]
        joins = self.state / "test" / "joins.jsonl"
        with joins.open("a", encoding="utf-8") as f:
            f.write("null\n7\n[]\n\n")
        self.assertTrue(self.service.known_session(sid))
        self.assertFalse(self.service.known_session("018f8f4e-5c90-7abc-8123-0000000000aa"))
        # The teardown gate reads the same log, and it fails *closed*: a line
        # it cannot read may name a session whose report is still remote-only.
        self.assertFalse(self.service.campaign_can_stand_down("test"),
                         "a damaged join log is not permission to stand a campaign down")

    def test_a_reconnect_before_the_host_publishes_the_release_gets_a_live_lease(self) -> None:
        # #1147. The client tells the volunteer, in as many words, to rejoin
        # now; for the ~12 s before the host publishes `released_at` the row is
        # still held, so the reissue came back with the lease from the original
        # join -- long expired on any session older than 45 s.  The host then
        # refuses it `reservation_journal_stale`, which is a handshake error
        # the client does not retry, so "rejoin now" was the one thing that
        # could not work.  Measured at 12.1 s of hard lockout end to end.
        attempt = self.enable_always_on()
        first = self.service.join("test", self.request())
        slot, session = first["join"]["slot"], first["join"]["session_id"]
        # The host holds the seat -- bound, and with no release published.
        self.publish_seats(attempt, active=(slot,), running=True)
        self.expire_every_reservation()

        again = self.service.join("test", self.request())
        self.assertEqual((again["join"]["session_id"], again["join"]["slot"]), (session, slot),
                         "the same identity gets its own seat back, as it always did")
        rows = json.loads((self.state / "test" / "slots.json").read_text())
        self.assertEqual([row["session_id"] for row in rows], [session])
        self.assertGreater(rows[0]["expires_at"], int(time.time()),
                           "the host's journal refuses an expired row, so the reconnect "
                           "the client advises must be handed a live lease")

        # The same again for a seat the host still has only in the lobby.
        self.publish_seats(attempt, pending=(slot,), running=False)
        self.expire_every_reservation()
        self.service.join("test", self.request())
        rows = json.loads((self.state / "test" / "slots.json").read_text())
        self.assertGreater(rows[0]["expires_at"], int(time.time()))

    def test_the_roster_forgets_a_session_when_it_ends(self) -> None:
        # A label that outlives its session names a player who is not there.
        self.service.join("test", self.request())
        self.assertEqual(len(self.service.roster("test")["roster"]), 5)
        for child in list(self.service.children.values()):
            child.kill()
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and self.service.rosters:
            time.sleep(0.01)
        self.assertEqual(self.service.roster("test")["roster"], [], "the reaper must drop the label")

    def test_the_roster_of_an_unknown_campaign_refuses_404(self) -> None:
        with self.assertRaises(Refusal) as caught: self.service.roster("nope")
        self.assertEqual((caught.exception.status, caught.exception.error), (404, "unknown_campaign"))

    def test_a_broken_control_file_keeps_serving_the_previous_parse(self) -> None:
        self.service.listing(); self.control.write_text("[broken\n")
        self.assertEqual(self.service.listing()["campaigns"][0]["id"], "test")
    def test_legacy_eight_bot_campaigns_remain_parseable_until_they_opt_into_humans(self) -> None:
        self.control.write_text(self.control.read_text().replace("peers = 4\nhumans = 4", "peers = 8"))
        self.assertEqual(self.service.listing()["campaigns"][0]["humans"], 1)
    def test_a_conflicting_reupload_refuses_409_and_leaves_the_first_bytes(self) -> None:
        sid = self.service.join("test", self.request())["join"]["session_id"]; one = json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "one"}).encode(); self.service.upload(sid, one)
        with self.assertRaises(Refusal) as x: self.service.upload(sid, json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "two"}).encode())
        self.assertEqual(x.exception.status, 409); self.assertEqual((self.state / "sessions" / sid / "telemetry.jsonl").read_text(), "one")

    def test_a_seat_longer_than_the_increment_cadence_stores_every_increment(self) -> None:
        # #1119. A 60-minute session mints twelve signed rows, one per
        # five-minute increment. Every one of them used to be posted to
        # `/v1/sessions/<sid>/upload`, where the second and every later body
        # differed from the first and was refused `409 conflict` -- so a long
        # playtest banked its first five minutes and nothing else, forever.
        #
        # Twelve increments, not two: one boundary proves nothing about a
        # cadence, and the seat that actually gets flown is an hour long.
        sid = self.service.join("test", self.request())["join"]["session_id"]
        def increment(index: int) -> bytes:
            return json.dumps({"records": [{"session_id": sid, "banked_minutes": 5.0,
                                            "increment": {"index": index, "since_tick": index * 18000,
                                                          "until_tick": (index + 1) * 18000,
                                                          "final_increment": index == 11}}],
                               "telemetry_jsonl": f"increment {index}\n"}).encode()
        for index in range(12):
            self.service.upload(sid, increment(index), index)
        seat = self.state / "sessions" / sid
        # Increment zero keeps the unsuffixed names, so nothing already banked
        # moved and every existing reader of a session directory still works.
        self.assertEqual((seat / "telemetry.jsonl").read_text(), "increment 0\n")
        for index in range(1, 12):
            self.assertEqual((seat / f"telemetry.increment-{index}.jsonl").read_text(), f"increment {index}\n",
                             f"increment {index} of the seat is not stored as its own evidence")
            self.assertIn(f'"index":{index}', (seat / f"client-records.increment-{index}.jsonl").read_text())
        # The whole hour is on disk, as twelve distinct signed rows.
        self.assertEqual(len(sorted(seat.glob("client-records*.jsonl"))), 12)
        # And a retry of any of them is free: identical bytes are not a
        # conflict, which is what lets the client re-send without fear.
        self.service.upload(sid, increment(7), 7)

    def test_an_increment_may_not_be_filed_under_another_increments_address(self) -> None:
        # The address and the row must agree, for the same reason
        # `wrong_session` exists: a row filed under the wrong increment is
        # evidence in the wrong place, and the ledger dedupes increments by
        # their span, so a misfiled one is a span attributed twice or not at
        # all. Nothing upstream of here would say so.
        sid = self.service.join("test", self.request())["join"]["session_id"]
        row = json.dumps({"records": [{"session_id": sid, "increment": {"index": 2, "since_tick": 36000, "until_tick": 54000}}],
                          "telemetry_jsonl": "x"}).encode()
        with self.assertRaises(Refusal) as x: self.service.upload(sid, row, 3)
        self.assertEqual((x.exception.status, x.exception.error), (422, "wrong_increment"))
        self.assertFalse((self.state / "sessions" / sid / "client-records.increment-3.jsonl").exists())
        # A row with no `increment` object at all is a whole seat, which is
        # increment zero -- so it may not be filed under a later increment.
        whole = json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "x"}).encode()
        with self.assertRaises(Refusal) as x: self.service.upload(sid, whole, 1)
        self.assertEqual(x.exception.error, "wrong_increment")
        self.service.upload(sid, whole, 0)

    def test_the_increment_route_addresses_the_increment_the_url_names(self) -> None:
        # The client builds `/v1/sessions/<sid>/increments/<n>/upload` for
        # every increment after the first (#1119). If this route is not
        # matched the body 404s as an unknown endpoint, which is the whole
        # seam between the two halves of the fix.
        matched = re.fullmatch(r"/v1/sessions/([^/]+)/increments/(\d{1,9})/upload",
                               "/v1/sessions/018f8f4e-5c90-7abc-8123-0000000000ab/increments/7/upload")
        self.assertIsNotNone(matched)
        assert matched is not None
        self.assertEqual((matched.group(1), int(matched.group(2))), ("018f8f4e-5c90-7abc-8123-0000000000ab", 7))
        # And increment zero keeps the route it has always had, so a client
        # older than this service build still banks its first increment.
        self.assertIsNotNone(re.fullmatch(r"/v1/sessions/([^/]+)/upload",
                                          "/v1/sessions/018f8f4e-5c90-7abc-8123-0000000000ab/upload"))

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
    def test_every_refused_upload_is_logged_where_an_operator_can_see_it(self) -> None:
        # #735: a client whose upload is refused writes one line to the
        # player's own log and the service writes nothing, so an unrecorded
        # session is indistinguishable here from a player who never played.
        # Every refusal path must say so on the host, not just the size one.
        sid = self.service.join("test", self.request())["join"]["session_id"]
        refusals = {
            "too_large": json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "x" * MAX_UPLOAD_BYTES}).encode(),
            "bad_upload": b"not json",
            "wrong_session": json.dumps({"records": [{"session_id": "018f8f4e-5c90-7abc-8123-0000000000ab"}], "telemetry_jsonl": "x"}).encode(),
        }
        for error, body in refusals.items():
            with self.assertLogs(level=logging.ERROR) as logs, self.assertRaises(Refusal):
                self.service.upload(sid, body)
            line = "\n".join(logs.output)
            self.assertIn("upload refused", line, f"a {error} refusal is silent on the host")
            self.assertIn(sid, line, f"a {error} refusal does not name its session")
            self.assertIn(error, line)
        # An upload that is accepted stays quiet on the error channel.
        self.service.upload(sid, json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "x"}).encode())
    def test_an_origin_with_a_smaller_body_limit_refuses_to_start(self) -> None:
        # #1002: nginx's 1 MiB default sat invisibly in front of the 64 MiB
        # ceiling, so every volunteer upload died with HTTP 413 before
        # admission saw it and #735's refusal logging never fired. The startup
        # probe must fail the process, name both numbers, and say what to change.
        def fake_post(url: str, body: bytes) -> tuple[int, str]:
            if len(body) > UPLOAD_PROBE_STEP_BYTES: return HTTPStatus.REQUEST_ENTITY_TOO_LARGE, "<html>413 Request Entity Too Large</html>"
            return HTTPStatus.NOT_FOUND, json.dumps({"error": "unknown_session"})
        with self.assertLogs(level=logging.CRITICAL) as logs, self.assertRaises(SystemExit) as x:
            enforce_upload_limit("https://campaigns.example", fake_post)
        line = "\n".join(logs.output)
        self.assertIn(str(MAX_UPLOAD_BYTES), line, "the message must name the ceiling the application wanted")
        self.assertIn("1 MiB", line, "the message must name the effective limit the origin actually has")
        self.assertIn("client_max_body_size", line, "the message must say what an operator changes")
        self.assertEqual(x.exception.code, 1)
    def test_an_unreachable_origin_warns_but_starts(self) -> None:
        # A dev box, a container in CI, or a firewall leaves the public origin
        # unreachable; that is not evidence of a small ceiling, so it must not
        # fail startup (#1002).
        def fake_post(url: str, body: bytes) -> tuple[int, str]: raise ConnectionRefusedError("connection refused")
        with self.assertLogs(level=logging.WARNING) as logs:
            self.assertIsNone(enforce_upload_limit("https://campaigns.example", fake_post))
        line = "\n".join(logs.output)
        self.assertIn("cannot verify", line)
        self.assertIn(str(MAX_UPLOAD_BYTES), line, "the warning must still name the ceiling it could not verify")
    def test_the_probe_dials_https_over_tls_not_port_80(self) -> None:
        # `post_json` chose its connection class unconditionally, so an https
        # origin was dialled over plain HTTP; a TLS-fronted origin answers port
        # 80 with a 301 that carries no refusal marker, and the check sat
        # permanently unverifiable against the only origin it protects. The
        # other arms here stub `post_json` and so could not see it (#1002).
        import http.client as _hc
        dialled: list[tuple[str, object, object]] = []
        class _Recorder:
            def __init__(self, kind: str) -> None: self.kind = kind
            def __call__(self, host, port, timeout=None):  # noqa: ANN001 - stdlib signature
                dialled.append((self.kind, host, port)); raise ConnectionRefusedError("recorded")
        real_https, real_http = _hc.HTTPSConnection, _hc.HTTPConnection
        _hc.HTTPSConnection, _hc.HTTPConnection = _Recorder("https"), _Recorder("http")
        try:
            with self.assertRaises(OSError): post_json("https://campaigns.example/v1/x", b"{}")
            with self.assertRaises(OSError): post_json("http://localhost:8080/v1/x", b"{}")
        finally:
            _hc.HTTPSConnection, _hc.HTTPConnection = real_https, real_http
        self.assertEqual([d[0] for d in dialled], ["https", "http"], "the url scheme must pick the connection class")
    def test_an_origin_that_passes_the_full_probe_verifies_and_starts(self) -> None:
        seen = []
        def fake_post(url: str, body: bytes) -> tuple[int, str]:
            seen.append((url, len(body)))
            return HTTPStatus.NOT_FOUND, json.dumps({"error": "unknown_session"})
        with self.assertLogs(level=logging.INFO) as logs:
            self.assertIsNone(enforce_upload_limit("https://campaigns.example/", fake_post))
        line = "\n".join(logs.output)
        self.assertIn("accepted", line)
        self.assertIn(str(MAX_UPLOAD_BYTES), line)
        self.assertEqual(seen, [("https://campaigns.example/v1/sessions/" + UPLOAD_PROBE_SESSION + "/upload", MAX_UPLOAD_BYTES)],
                         "the happy path is exactly one probe, carrying exactly the ceiling it claims to verify")
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
        raw.write_text('{"external":[],"witnessing":true,"identity":{"target":"x","commit":"0000000000000000000000000000000000000000"}}')
        records.write_text('{"session_id":"018f8f4e-5c90-7abc-8123-0000000000ab","actor":"human","platform_triple":"x","impairment_mismatch":false,"configured_impairment_profile":{"loss_pct":0,"jitter_p50_ms":0,"jitter_p99_ms":0},"observed_loss_pct":0,"observed_jitter_p50_ms":0,"observed_jitter_p99_ms":0}\n')
        self.assertNotEqual(subprocess.run([str(script), "assemble", str(raw), str(records), sid, str(Path(self.tmp.name) / "out.json")], env={**os.environ, "P4_PIPELINE_ID": "test"}).returncode, 0)

    # --- Hunt 2 remainder: #1146, #1152, #1153, #1155 -------------------------

    def serving(self) -> tuple[ThreadingHTTPServer, str, threading.Thread]:
        """The real threaded server on loopback, so a test can post concurrently.

        The suite otherwise calls the service methods directly, which is why
        neither #1146 nor #1153 nor #1152 was reachable from it: none of the
        three is a property of a single call.  This is the missing fixture the
        hunt asked for -- "concurrency in `admission.py` ... the server is
        genuinely threaded and #1146 and #1153 are both what that costs".
        """
        Handler.service = self.service
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join)
        self.addCleanup(server.shutdown)
        self.addCleanup(server.server_close)
        return server, f"127.0.0.1:{server.server_address[1]}", thread

    @staticmethod
    def post(origin: str, path: str, body: bytes, *, declared: int | None = None,
             send_body: bool = True, timeout: float = 10.0) -> tuple[int, bytes]:
        """One POST, with the declared length and the sent body under the test's control."""
        conn = http.client.HTTPConnection(origin, timeout=timeout)
        try:
            conn.putrequest("POST", path)
            conn.putheader("Content-Type", "application/json")
            conn.putheader("Content-Length", str(len(body) if declared is None else declared))
            conn.putheader("Connection", "close")
            conn.endheaders(message_body=body if send_body else b"")
            response = conn.getresponse()
            return response.status, response.read()
        finally:
            conn.close()

    @staticmethod
    def get(origin: str, path: str, timeout: float = 10.0) -> tuple[int, bytes]:
        conn = http.client.HTTPConnection(origin, timeout=timeout)
        try:
            conn.request("GET", path, headers={"Connection": "close"})
            response = conn.getresponse()
            return response.status, response.read()
        finally:
            conn.close()

    def test_a_join_refused_inside_the_startup_window_keeps_the_incumbents_flock(self) -> None:
        # #1146.  The `finally` popped `self.locks[ident]` by *key*, not by
        # identity, so a second join refused `campaign_busy` removed the
        # incumbent's entry.  `lock.close()` is not the mechanism and never was
        # -- `flock(2)` locks belong to the open file description and every
        # request gets its own `open()`, so closing one fd cannot release
        # another's.  What the pop removes is the incumbent lock's only
        # long-lived reference: once the incumbent's `join` frame returns,
        # CPython closes the file and the flock goes with it, mid-session.  A
        # third join could then take the campaign flock and start a second
        # harness on the same UDP port.
        #
        # The trigger is a bounded race, not a standing property.  The guard is
        # `ident not in self.children`, and a genuinely running session has
        # `self.children[ident]` set -- so the hole is only the window between
        # `self.locks[ident] = lock` and that assignment, which spans two
        # `orrery-invite` subprocesses, a `Popen` and `_wait_listening`.  This
        # test parks the incumbent inside exactly that window.
        window, resume, admitted = threading.Event(), threading.Event(), []
        real_wait = self.service._wait_listening

        def parked(*args: Any, **kwargs: Any) -> str:
            window.set()
            self.assertTrue(resume.wait(10), "the test never released the parked join")
            return real_wait(*args, **kwargs)

        self.service._wait_listening = parked  # type: ignore[method-assign]
        incumbent = threading.Thread(target=lambda: admitted.append(self.service.join("test", self.request())))
        incumbent.start()
        self.addCleanup(incumbent.join)
        self.addCleanup(resume.set)
        self.assertTrue(window.wait(10), "the incumbent join never reached the startup window")

        # Deliberately no local reference to the lock object: holding one would
        # keep it alive and hide the very refcount bug under test.
        self.assertIn("test", self.service.locks)
        self.assertNotIn("test", self.service.children, "the incumbent must still be inside the window")

        with self.assertRaises(Refusal) as refused:
            self.service.join("test", self.request())
        self.assertEqual((refused.exception.status, refused.exception.error), (409, "campaign_busy"))
        self.assertIn("test", self.service.locks,
                      "the refused join popped the incumbent's lock out of the map (#1146)")

        resume.set()
        incumbent.join(10)
        self.assertFalse(incumbent.is_alive())
        self.assertEqual(len(admitted), 1, "the incumbent join did not complete")
        self.assertIn("test", self.service.children)
        gc.collect()

        # The consequence that costs the session: with the entry popped, the
        # incumbent's file object is unreferenced the moment its frame returns,
        # so the campaign flock is free and a third join takes it.
        with (self.state / "test" / "lock").open("a+") as probe:
            with self.assertRaises(BlockingIOError,
                                   msg="the running session's campaign flock was released (#1146)"):
                fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_an_unexpected_failure_still_answers_an_http_status(self) -> None:
        # #1152.  `do_GET`/`do_POST` caught only `Refusal`, so anything else --
        # every reachable one an `OSError` -- escaped into `http.server`, which
        # logs a traceback and closes the socket with *no response*.  The
        # volunteer gets a connection reset on the endpoint every client hits
        # first, with no code and no sentence: the "it's broken" report.
        _, origin, _ = self.serving()

        def no_statvfs(_: Any) -> Any:
            raise OSError(errno.EIO, "the state filesystem is gone")

        self.service.statvfs = no_statvfs  # type: ignore[method-assign]
        with self.assertLogs(level=logging.ERROR):
            status, body = self.get(origin, "/v1/campaigns")
        self.assertEqual(status, 500, "an unexpected failure closed the socket with no HTTP status (#1152)")
        self.assertEqual(json.loads(body)["error"], "admission_failed")

    def test_a_failed_join_log_does_not_leave_a_committed_seat_behind(self) -> None:
        # #1152, the second half.  `_write_slots` durably committed the
        # reservation *before* `append_join` ran, and `append_join` raises a
        # bare `OSError` -- which the surrounding `except` tuple did not name.
        # An `ENOSPC` or `EIO` on the join log therefore left the row on disk
        # and the account minted for a volunteer whose socket was dropped with
        # no status; and because `known_session` reads `joins.jsonl`, that
        # seat's evidence uploads would have been refused `404` forever.
        attempt = self.enable_always_on()
        self.publish_seats(attempt)

        def no_log(*_: Any, **__: Any) -> None:
            raise OSError(errno.ENOSPC, "no space left on device")

        self.service.append_join = no_log  # type: ignore[method-assign]
        with self.assertLogs(level=logging.ERROR), self.assertRaises(Refusal) as refused:
            self.service.join("test", self.request())
        # A status, not a bare OSError escaping into the handler.
        self.assertEqual((refused.exception.status, refused.exception.error), (500, "admission_failed"))
        # And nothing was committed, so the seat is still free for the retry.
        slots = self.state / "test" / "slots.json"
        self.assertEqual([] if not slots.exists() else json.loads(slots.read_text()), [],
                         "the reservation was committed although its join row was never written (#1152)")
        self.assertEqual(self.service.roster("test")["roster"][self.service.campaigns()[0]["test"].peers]["state"], "empty")

    def test_concurrent_atomic_writes_never_bank_a_spliced_body(self) -> None:
        # #1153, the evidence-integrity half.  `atomic_bytes` wrote through a
        # *fixed* `<name>.tmp`, which is safe only on a serial server -- and
        # `main` runs a `ThreadingHTTPServer`.  Two writers interleaving into
        # one shared temp file bank the first half of one body and the second
        # half of another, and #1119's conflict check then treats that spliced
        # record as the canonical evidence, because it compares against
        # whatever is stored.  Failing would be acceptable; banking a wrong
        # artifact is not.
        target = self.state / "sessions" / "splice"; target.mkdir(parents=True)
        path = target / "client-records.jsonl"
        bodies = [bytes([65 + n]) * (256 * 1024 + n) for n in range(8)]
        gate = threading.Barrier(len(bodies))
        failures: list[BaseException] = []

        def write(data: bytes) -> None:
            try:
                gate.wait(10)
                self.service.atomic_bytes(path, data)
            except BaseException as error:  # noqa: BLE001 - reported below
                failures.append(error)

        writers = [threading.Thread(target=write, args=(data,)) for data in bodies]
        for writer in writers: writer.start()
        for writer in writers: writer.join(30)
        self.assertEqual(failures, [])
        self.assertIn(path.read_bytes(), bodies,
                      "a concurrent write banked bytes that no client ever sent (#1153)")
        self.assertEqual(sorted(p.name for p in target.iterdir() if p.name.endswith(".tmp")), [],
                         "a temp file was left behind")

    def test_two_concurrent_uploads_of_one_increment_cannot_both_be_banked(self) -> None:
        # #1153.  `_store_upload` does check-then-write with no lock, on a
        # threaded server.  Two posts of *differing* bodies for the same
        # session and increment both pass `path.exists()` -- neither file is
        # there yet -- and both go on to write, so the `409 conflict` that
        # exists to refuse exactly this is never reached and one body is
        # silently overwritten or spliced.  This is not hypothetical for the
        # shipped client: the retry sweep and the exit-path flush both call
        # `send_pending` with no mutual exclusion between them
        # (`clients/regolith/src/admission.rs:1671-1680`, `:1785-1803`).
        #
        # The posts are made over a real socket against a real
        # `ThreadingHTTPServer`, released together by a barrier, and repeated
        # across several increments -- a sequential test cannot see any of this.
        _, origin, _ = self.serving()
        sid = self.service.join("test", self.request())["join"]["session_id"]

        # Half a megabyte per body, so the check-then-write window is a real
        # one: the losing writer has to be still inside it when the winner
        # arrives, and a four-kilobyte body is over before the second request
        # is parsed.  Sixteen rounds, because a race that needs one interleave
        # is not proven by one attempt.
        span = 512 * 1024

        def body(mark: str, index: int) -> bytes:
            return json.dumps({"records": [{"session_id": sid, "increment": {"index": index}}],
                               "telemetry_jsonl": mark * span}).encode()

        for index in range(1, 17):
            gate = threading.Barrier(2)
            answers: list[tuple[int, bytes]] = []

            def send(mark: str, index: int = index, gate: threading.Barrier = gate) -> None:
                gate.wait(10)
                answers.append(self.post(origin, f"/v1/sessions/{sid}/increments/{index}/upload",
                                         body(mark, index)))

            racers = [threading.Thread(target=send, args=(mark,)) for mark in ("a", "b")]
            for racer in racers: racer.start()
            for racer in racers: racer.join(30)
            self.assertEqual(len(answers), 2)
            self.assertEqual(sorted(status for status, _ in answers), [204, 409],
                             f"increment {index}: both differing bodies were accepted (#1153)")
            banked = (self.state / "sessions" / sid / f"telemetry.increment-{index}.jsonl").read_bytes()
            self.assertIn(banked, (b"a" * span, b"b" * span),
                          f"increment {index}: the banked evidence is neither body that was posted (#1153)")

    def test_an_over_large_declared_body_is_refused_before_it_is_read(self) -> None:
        # #1153, the second half.  `read_json` did
        # `self.rfile.read(int(Content-Length))` with no cap, and
        # `MAX_UPLOAD_BYTES` was only checked afterwards in `_store_upload`, so
        # a declared 4 GB was buffered into memory before it was refused.  The
        # proof that it is refused *before* the read is that nothing is sent:
        # a server that reads first would block until the timeout.
        _, origin, _ = self.serving()
        status, body = self.post(origin, "/v1/sessions/018f8f4e-5c90-7abc-8123-0000000000ab/upload",
                                 b"", declared=4 * 1024**3, send_body=False, timeout=10)
        self.assertEqual(status, 413, "an over-large declared body was read before it was capped (#1153)")
        self.assertEqual(json.loads(body)["error"], "too_large")
        # A body within the cap is still read whole, so the bounded loop did
        # not break the ordinary path.
        sid = self.service.join("test", self.request())["join"]["session_id"]
        payload = json.dumps({"records": [{"session_id": sid}], "telemetry_jsonl": "x" * (2 * 1024**2)}).encode()
        self.assertGreater(len(payload), 1024**2, "the body must span more than one read chunk")
        status, _ = self.post(origin, f"/v1/sessions/{sid}/upload", payload)
        self.assertEqual(status, 204)
        self.assertEqual(len((self.state / "sessions" / sid / "telemetry.jsonl").read_bytes()), 2 * 1024**2)

    def test_a_config_value_cannot_split_or_inject_on_the_campaign_host(self) -> None:
        # #1155.  `ssh` joins its command arguments with spaces and hands the
        # result to the remote *login shell* -- which is the only reason the
        # literal `&&` on that line works.  Nothing was quoted, so a
        # `client_rev` from `campaigns.conf` containing a space split into two
        # arguments and one containing `;` or a backtick ran as `orrery` on the
        # campaign host.
        #
        # `campaigns.conf` is root-owned and operator-written -- no request
        # path writes it -- so this is a robustness fix and not an exposure:
        # what it costs in practice is a harness silently pinned to the wrong
        # build, reported to the operator as `host_failed`.
        revision = "rev; touch /tmp/orrery-1155-pwned"
        self.control.write_text(self.control.read_text().replace("client_rev = rev", f"client_rev = {revision}"))
        self.service.join("test", {**self.request(), "client_rev": revision})
        fields = shlex.split(self.recorded_harness_command())
        self.assertIn("--require-client-rev", fields)
        self.assertEqual(fields[fields.index("--require-client-rev") + 1], revision,
                         "the config value split into several remote arguments (#1155)")
        self.assertNotIn("touch", fields, "the config value reached the remote shell as syntax (#1155)")
        # `&&` must stay unquoted: it is the one element on the line that is
        # shell syntax rather than data.
        self.assertIn("&&", fields)
        self.assertLess(fields.index("mkdir"), fields.index("&&"))


if __name__ == "__main__": main()
