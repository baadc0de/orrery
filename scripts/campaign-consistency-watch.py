#!/usr/bin/env python3
"""Capture the raw inputs when the listing and the roster disagree (#713).

    ./scripts/campaign-consistency-watch.py --campaign shakedown --out /var/tmp/713
    ./scripts/campaign-consistency-watch.py --self-test

Live once, `slots_free` reported zero while the roster drew two human seats
empty, so admission would have refused a player the lobby had just offered a
seat to.  Two rounds of reading the code did not reproduce it, and the fixture
built to prove the suspected mechanism disproved it instead: a generation
mismatch takes `_campaign_phase`'s `restarting` branch, which reports zero free
by design and never reaches the count.

So this stops reasoning and takes evidence.  It polls both endpoints and, on the
first disagreement, writes both answers and the three files that produce them --
`slots.json`, `active-seats.json`, `attempt.json` -- as one timestamped set.
Whatever the mechanism is, it is in those bytes.

`restarting` is skipped deliberately: it reports zero free whatever the roster
last knew, which is correct and is not the bug.
"""
import argparse
import json
import shutil
import sys
import time
import urllib.request
from pathlib import Path


def taken_by_roster(roster: list[dict]) -> int:
    """Human seats the roster draws as occupied."""
    return sum(1 for seat in roster
               if seat.get("kind") == "human" and seat.get("state") != "empty")


def disagrees(listing: dict, roster: list[dict], humans: int) -> bool:
    """Whether the listing's free count contradicts what the roster draws.

    A campaign that is restarting has no generation to admit into and reports
    zero free by design, so it cannot contradict anything.
    """
    if listing.get("phase") == "restarting":
        return False
    free = listing.get("slots_free")
    if not isinstance(free, int):
        return False
    return free != humans - taken_by_roster(roster)


def fetch(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.load(response)


def capture(out: Path, state: Path, host_state: Path, campaign: str,
            listing: dict, roster: dict) -> Path:
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    target = out / f"mismatch-{stamp}"
    target.mkdir(parents=True, exist_ok=True)
    (target / "listing.json").write_text(json.dumps(listing, indent=2))
    (target / "roster.json").write_text(json.dumps(roster, indent=2))
    for source in (state / campaign / "slots.json",
                   host_state / campaign / "active-seats.json",
                   host_state / campaign / "attempt.json"):
        if source.exists():
            shutil.copy2(source, target / source.name)
        else:
            (target / f"{source.name}.missing").write_text(str(source))
    return target


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--origin", default="https://campaigns.distopik.com")
    p.add_argument("--campaign", default="shakedown")
    p.add_argument("--out", type=Path, default=Path("/var/tmp/campaign-713"))
    p.add_argument("--state", type=Path, default=Path("/var/lib/orrery-admission"))
    p.add_argument("--host-state", type=Path, default=Path("/var/lib/orrery-p1-swarm"))
    p.add_argument("--interval", type=float, default=2.0)
    p.add_argument("--seconds", type=float, default=0.0, help="0 runs until killed")
    p.add_argument("--self-test", action="store_true")
    a = p.parse_args()
    if a.self_test:
        return self_test()

    started = time.time()
    captured = 0
    while a.seconds <= 0 or time.time() - started < a.seconds:
        try:
            listing = next(c for c in fetch(f"{a.origin}/v1/campaigns")["campaigns"]
                           if c["id"] == a.campaign)
            roster = fetch(f"{a.origin}/v1/campaigns/{a.campaign}/roster")
        except Exception as error:  # a poller must outlive a blip
            print(f"poll failed: {error}", file=sys.stderr, flush=True)
            time.sleep(a.interval)
            continue
        if disagrees(listing, roster.get("roster", []), int(listing["humans"])):
            target = capture(a.out, a.state, a.host_state, a.campaign, listing, roster)
            captured += 1
            print(f"MISMATCH captured to {target} "
                  f"(slots_free={listing['slots_free']} phase={listing['phase']})",
                  flush=True)
        time.sleep(a.interval)
    print(f"captured {captured} mismatch set(s)")
    return 0


def self_test() -> int:
    seats = [{"slot": 0, "kind": "bot", "state": "active"},
             {"slot": 5, "kind": "human", "state": "reserved"},
             {"slot": 6, "kind": "human", "state": "empty"},
             {"slot": 7, "kind": "human", "state": "empty"}]
    assert taken_by_roster(seats) == 1, "only occupied human seats count"
    # The live observation: one seat drawn taken, none reported free.
    assert disagrees({"phase": "running", "slots_free": 0}, seats, 3)
    assert not disagrees({"phase": "running", "slots_free": 2}, seats, 3)
    # Restarting reports zero free by design and must not be captured.
    assert not disagrees({"phase": "restarting", "slots_free": 0}, seats, 3)
    # A malformed answer is not evidence of a mismatch.
    assert not disagrees({"phase": "running", "slots_free": None}, seats, 3)
    print("campaign-consistency-watch self-test OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
