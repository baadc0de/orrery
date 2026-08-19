#!/usr/bin/env python3
"""Fold a two-arm `fdbcli status json` sample stream into per-arm min/median/max
under ONE windowing rule, applied to both arms.

Why this exists rather than a paragraph telling you to be careful. An engine-arm
study runs one sampler against **both** clusters continuously — including while
the other arm is under test, and while the harness is between points tearing a
world down. "The largest number in the file carrying this arm's name" is
therefore not "this arm's cost", and the two are easy to confuse in a direction
that flatters one arm: docs/14-capacity.md §11.7 published a 27.92 ms `ssd`
commit latency taken one second after that point's window closed, at an instant
when the *idle* `memory` cluster read 35.47 ms, while excluding `memory`'s own
matching extreme. Both numbers were box state. See §11.7's F1/F2 notes.

The rule, and the only one this script implements:

    a sample belongs to an arm iff its timestamp falls inside
    [end - duration_secs, end] of one of THAT arm's own point directories,

where `end` is the mtime of the point's `load.jsonl` — the instant the rig
wrote its run footer — and `duration_secs` comes from its `point.json`. An arm
is read off the point directory's name prefix, which `p2-capacity-sweep.sh`
writes together with the engine it read back from `status json`.

Usage:
    scripts/fdb-status-window.py <status.jsonl> <sweep-dir> [point-substring]
        [--arm-key ssd=ssd- --arm-key mem=memory-] [--skip-secs N] [--per-point]

`--arm-key <sample-arm>=<dir-prefix>` maps the `arm` field the sampler writes
to the directory prefix its points carry; the defaults match §11's study.
`--skip-secs` drops the first N seconds of every window on both arms alike,
which is how §11.8 states a steady-state band without letting the run-start
lease-claim burst into it on one arm only.

Reads nothing but the files named; writes nothing.
"""

from __future__ import annotations

import argparse
import datetime
import json
import pathlib
import statistics
import sys

# (label, sampler field, multiplier) — the columns §11.7's table reports.
FIELDS = [
    ("commit latency ms", "commit_latency_s", 1000.0),
    ("read latency ms", "read_latency_s", 1000.0),
    ("GRV latency ms", "grv_latency_s", 1000.0),
    ("bytes written kB/s", "written_bytes_hz", 0.001),
    ("commits /s", "commits_hz", 1.0),
    ("conflicts /s", "conflicted_hz", 1.0),
]


def utc(ts: float) -> str:
    return datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%H:%M:%S")


def windows(sweep: pathlib.Path, want: str, arm_keys: dict[str, str]):
    """Every point directory's [start, end], keyed by the sampler's arm name."""
    out: dict[str, list[tuple[float, float, str]]] = {arm: [] for arm in arm_keys}
    for d in sorted(p for p in sweep.iterdir() if p.is_dir()):
        if want not in d.name:
            continue
        point, load = d / "point.json", d / "load.jsonl"
        if not (point.exists() and load.exists()):
            continue
        arm = next((a for a, prefix in arm_keys.items() if d.name.startswith(prefix)), None)
        if arm is None:
            continue
        end = load.stat().st_mtime
        out[arm].append((end - json.loads(point.read_text())["duration_secs"], end, d.name))
    return out


def samples(path: pathlib.Path):
    rows = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        # The sampler emits `{"err": …}` when a `status json` call times out.
        # Those carry no measurement and must not count as an in-window sample.
        if "err" not in row:
            rows.append(row)
    return rows


def summarise(rows, label_width=22):
    for label, key, scale in FIELDS:
        xs = [row[key] * scale for row in rows if row.get(key) is not None]
        if not xs:
            continue
        print(
            f"   {label:<{label_width}} {min(xs):8.2f} / {statistics.median(xs):8.2f} /"
            f" {max(xs):8.2f}   (n={len(xs)})"
        )
    cores = [c for row in rows for c in (row.get("cpu_cores") or []) if c is not None]
    if cores:
        print(
            f"   {'fdbserver cores':<{label_width}} {min(cores):8.2f} /"
            f" {statistics.median(cores):8.2f} / {max(cores):8.2f}"
        )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("status_jsonl", type=pathlib.Path)
    ap.add_argument("sweep_dir", type=pathlib.Path)
    ap.add_argument("point", nargs="?", default="", help="substring of the point label")
    ap.add_argument("--arm-key", action="append", default=[], metavar="ARM=PREFIX")
    ap.add_argument("--skip-secs", type=float, default=0.0)
    ap.add_argument("--per-point", action="store_true")
    args = ap.parse_args()

    arm_keys = dict(k.split("=", 1) for k in args.arm_key) or {"ssd": "ssd-", "mem": "memory-"}
    wins = windows(args.sweep_dir, args.point, arm_keys)
    rows = samples(args.status_jsonl)
    if not any(wins.values()):
        print(f"no point directories matched {args.point!r}", file=sys.stderr)
        return 2

    for arm, ws in wins.items():
        for start, end, name in sorted(ws):
            print(f"# window {arm:<4} {name:<22} {utc(start)} -> {utc(end)}")

    for arm, ws in wins.items():
        mine = [r for r in rows if r.get("arm") == arm]
        inw = [
            r
            for r in mine
            if any(start + args.skip_secs <= r["ts"] <= end for start, end, _ in ws)
        ]
        print(f"\n== {arm}: {len(inw)} in-window samples of {len(mine)} carrying this arm's name")
        summarise(inw)
        if args.per_point:
            for start, end, name in sorted(ws):
                per = [r for r in mine if start + args.skip_secs <= r["ts"] <= end]
                print(f"\n   -- {name} (n={len(per)})")
                summarise(per, label_width=19)

    # The whole point of the exercise: name the samples the rule excluded, so a
    # reader can see what a "highest number in the file" reading would have
    # picked up, and what the OTHER cluster read at the same instant.
    print("\n== excluded, and what the other cluster read at that instant")
    for arm, ws in wins.items():
        out = [
            r
            for r in rows
            if r.get("arm") == arm and not any(start <= r["ts"] <= end for start, end, _ in ws)
        ]
        for r in sorted(out, key=lambda r: -(r.get("commit_latency_s") or 0.0))[:3]:
            other = min(
                (o for o in rows if o.get("arm") != arm),
                key=lambda o: abs(o["ts"] - r["ts"]),
                default=None,
            )
            other_ms = 1000 * (other or {}).get("commit_latency_s", 0.0)
            print(
                f"   {arm:<4} {utc(r['ts'])} commit={1000 * (r.get('commit_latency_s') or 0):6.2f} ms"
                f"  written={(r.get('written_bytes_hz') or 0) / 1000:7.1f} kB/s"
                f"  commits={r.get('commits_hz')}"
                f"  | other arm at that instant: commit={other_ms:6.2f} ms"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
