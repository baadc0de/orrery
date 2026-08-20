#!/usr/bin/env python3
"""Derive D20's journal-open figures from their versioned evidence.

Every number in ADR-0020 §Context — the per-record slope, the per-record index
cost, and the uptime extrapolation that motivates retention — is re-derived
here from docs/data/p2-journal-open-2026-08-20.jsonl rather than transcribed.
The arrival rate the extrapolation uses is itself read from the D19 gate
evidence, so the two records cannot drift apart.

    python3 scripts/p2-journal-open-report.py
    python3 scripts/p2-journal-open-report.py --self-test
"""

from __future__ import annotations

import copy
import json
import os
import pathlib
import sys
from collections.abc import Callable

ROOT = pathlib.Path(__file__).resolve().parents[1]
DATA = pathlib.Path(
    os.environ.get(
        "P2_JOURNAL_OPEN_DATA",
        ROOT / "docs/data/p2-journal-open-2026-08-20.jsonl",
    )
)
HOST = pathlib.Path(
    os.environ.get(
        "P2_JOURNAL_OPEN_HOST",
        ROOT / "docs/data/p2-journal-open-host-2026-08-20.json",
    )
)
GATE = pathlib.Path(
    os.environ.get(
        "P2_JOURNAL_RAW_DATA",
        ROOT / "docs/data/p2-journal-raw-2026-08-20.jsonl",
    )
)

# The budget D20 adds to D16, in milliseconds.
OPEN_BUDGET_MS = 2_000


def load() -> tuple[list[dict], dict, list[dict]]:
    steps = [json.loads(line) for line in DATA.read_text().splitlines() if line.strip()]
    gate = [json.loads(line) for line in GATE.read_text().splitlines() if line.strip()]
    return steps, json.loads(HOST.read_text()), gate


def slope_us_per_record(steps: list[dict]) -> float:
    """Least-squares slope of open time against record count, in µs/record."""
    n = len(steps)
    xs = [float(step["records"]) for step in steps]
    ys = [step["open_ms"] * 1e3 for step in steps]
    mean_x = sum(xs) / n
    mean_y = sum(ys) / n
    covariance = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys))
    variance = sum((x - mean_x) ** 2 for x in xs)
    return covariance / variance


def index_bytes_per_record(steps: list[dict]) -> float:
    """RSS growth per record between the first and last step, in bytes."""
    first, last = steps[0], steps[-1]
    grown_kb = last["rss_kb_after_open"] - first["rss_kb_after_open"]
    return grown_kb * 1024 / (last["records"] - first["records"])


def linearity(steps: list[dict]) -> float:
    """Worst relative deviation of any step from the fitted slope."""
    slope = slope_us_per_record(steps)
    return max(
        abs(step["open_ms"] * 1e3 - slope * step["records"]) / (step["open_ms"] * 1e3)
        for step in steps
    )


def gate_rate(gate: list[dict]) -> tuple[float, float]:
    """Records/s and journal bytes/s from the D19 gate's passing raw runs."""
    raw = [row for row in gate if row.get("backend") == "raw"]
    seconds = [row["run"]["duration_secs"] for row in raw]
    records = sum(row["durable_acks"] for row in raw) / len(raw)
    disk = sum(row["on_disk_bytes"] for row in raw) / len(raw)
    return records / seconds[0], disk / seconds[0]


def validate(steps: list[dict], host: dict, gate: list[dict]) -> list[str]:
    failures: list[str] = []

    def check(name: str, condition: bool, detail: str) -> None:
        if not condition:
            failures.append(f"{name}: {detail}")

    check("population", len(steps) >= 6, f"expected at least 6 steps, found {len(steps)}")
    check(
        "step spacing",
        len({steps[i + 1]["records"] - steps[i]["records"] for i in range(len(steps) - 1)}) == 1,
        "the steps must be equally spaced, or the slope is fitted to a shape rather than a rate",
    )
    check(
        "monotone records",
        all(steps[i + 1]["records"] > steps[i]["records"] for i in range(len(steps) - 1)),
        "record counts must increase: this is one journal grown in place",
    )
    check(
        "monotone open cost",
        all(steps[i + 1]["open_ms"] > steps[i]["open_ms"] for i in range(len(steps) - 1)),
        "open cost must increase with the journal, or the measurement is not of what it claims",
    )
    check(
        "payload consistency",
        len({step["payload_bytes"] for step in steps}) == 1,
        "every step must use one payload size",
    )
    check(
        "linearity",
        linearity(steps) < 0.05,
        f"open cost deviates from linear by {linearity(steps):.1%}; the D20 slope assumes it does not",
    )
    check(
        "budget relevance",
        steps[-1]["open_ms"] > OPEN_BUDGET_MS,
        f"the sweep never reaches the {OPEN_BUDGET_MS} ms budget, so it cannot say where the budget lies",
    )
    check(
        "scan is measured",
        all(step["scan_ms"] > 0 for step in steps),
        "the replay-scan column must be populated: it is the other half of a cold recovery",
    )
    check(
        "index memory grows",
        index_bytes_per_record(steps) > 0,
        "RSS must grow with the index, or the memory half of D19's consequence is unmeasured",
    )
    check(
        "page cache disclosed",
        host.get("page_cache") == "warm" and "floor" in host.get("page_cache_note", ""),
        "the host record must say the measurement is a warm-cache floor",
    )
    check(
        "journal device disclosed",
        bool(host.get("journal_device", {}).get("model")),
        "the host record must name the device the journal was on",
    )
    check(
        "backend",
        host.get("backend") == "raw",
        "D20's curve is the indexed raw backend's; the Fjall fallback does not implement retention",
    )
    raw = [row for row in gate if row.get("backend") == "raw"]
    check(
        "gate population",
        len(raw) == 5,
        f"expected 5 passing raw gate runs to derive the arrival rate, found {len(raw)}",
    )
    check(
        "gate duration agreement",
        len({row["run"]["duration_secs"] for row in raw}) == 1,
        "the gate runs must share one duration, or their mean rate is not a rate",
    )
    return failures


def report(steps: list[dict], host: dict, gate: list[dict]) -> None:
    slope = slope_us_per_record(steps)
    per_record_index = index_bytes_per_record(steps)
    records_per_s, bytes_per_s = gate_rate(gate)

    print("D20 — journal open cost vs retained journal")
    print(
        f"  {host['host']['cpu']}, journal on {host['journal_device']['model']} "
        f"({host['journal_device']['filesystem']}, {host['journal_device']['mount_options']}), "
        f"page cache {host['page_cache']}"
    )
    print()
    print("  records      on-disk    open ms    scan ms   RSS after open")
    for step in steps:
        print(
            f"  {step['records']:>9,}  {step['disk_bytes'] / 1e6:>8.0f} MB  "
            f"{step['open_ms']:>8.0f}  {step['scan_ms']:>9.0f}  "
            f"{step['rss_kb_after_open'] / 1024:>12.1f} MB"
        )
    print()
    print(f"  slope                 {slope:.2f} µs per record (worst deviation {linearity(steps):.1%})")
    per_mb = slope * steps[-1]["records"] / 1e3 / (steps[-1]["disk_bytes"] / 1e6)
    print(f"  per MB of journal     {per_mb:.2f} ms")
    print(f"  index memory          {per_record_index:.0f} bytes per record")
    print(f"  D16 budget            {OPEN_BUDGET_MS} ms ≈ {OPEN_BUDGET_MS * 1e3 / slope:,.0f} records")
    print()
    print(f"  At the D19 gate's arrival rate ({records_per_s:,.0f} records/s, {bytes_per_s / 1e6:.0f} MB/s),")
    print("  with nothing ever released:")
    for label, seconds in (("1 minute", 60), ("1 hour", 3600), ("1 day", 86400)):
        records = records_per_s * seconds
        print(
            f"    {label:<9} {records * slope / 1e6:>8.1f} s of open   "
            f"{bytes_per_s * seconds / 1e9:>8.1f} GB on disk"
        )
    print()
    print("  Warm page cache: the slope is a floor, not a worst case.")


def self_test(steps: list[dict], host: dict, gate: list[dict]) -> int:
    failures = validate(steps, host, gate)
    if failures:
        print("SELF-TEST FAILED")
        for failure in failures:
            print("  " + failure)
        return 1

    mutations: list[tuple[str, str, Callable[[list[dict], dict, list[dict]], None]]] = [
        ("population", "population", lambda rows, _h, _g: [rows.pop() for _ in range(len(rows) - 4)]),
        ("spacing", "step spacing", lambda rows, _h, _g: rows[-1].update(records=rows[-1]["records"] + 7)),
        ("monotone records", "monotone records", lambda rows, _h, _g: rows[1].update(records=1)),
        ("monotone open", "monotone open cost", lambda rows, _h, _g: rows[1].update(open_ms=0.001)),
        ("payload", "payload consistency", lambda rows, _h, _g: rows[0].update(payload_bytes=1)),
        ("linearity", "linearity", lambda rows, _h, _g: rows[-1].update(open_ms=rows[-1]["open_ms"] * 3)),
        ("budget", "budget relevance", lambda rows, _h, _g: [rows.pop() for _ in range(len(rows) - 6)]),
        ("scan", "scan is measured", lambda rows, _h, _g: rows[0].update(scan_ms=0)),
        ("index", "index memory grows", lambda rows, _h, _g: [row.update(rss_kb_after_open=1000) for row in rows]),
        ("cache", "page cache disclosed", lambda _r, host_, _g: host_.update(page_cache="cold")),
        ("cache note", "page cache disclosed", lambda _r, host_, _g: host_.update(page_cache_note="fine")),
        ("device", "journal device disclosed", lambda _r, host_, _g: host_["journal_device"].update(model="")),
        ("backend", "backend", lambda _r, host_, _g: host_.update(backend="fjall")),
        ("gate population", "gate population", lambda _r, _h, g: g.remove(next(row for row in g if row["backend"] == "raw"))),
        ("gate duration", "gate duration agreement", lambda _r, _h, g: next(row for row in g if row["backend"] == "raw")["run"].update(duration_secs=99)),
    ]

    mutation_failures: list[str] = []
    for name, expected, mutate in mutations:
        rows, host_, gate_ = copy.deepcopy(steps), copy.deepcopy(host), copy.deepcopy(gate)
        mutate(rows, host_, gate_)
        found = validate(rows, host_, gate_)
        if not any(expected in failure for failure in found):
            mutation_failures.append(f"{name}: expected a failure containing {expected!r}, got {found}")

    if mutation_failures:
        print("SELF-TEST FAILED: mutation checks")
        for failure in mutation_failures:
            print("  " + failure)
        return 1

    print(
        f"SELF-TEST PASSED: {len(steps)} steps, slope {slope_us_per_record(steps):.2f} µs/record, "
        f"{len(mutations)} guarded-fact mutations rejected"
    )
    return 0


if __name__ == "__main__":
    loaded_steps, loaded_host, loaded_gate = load()
    if "--self-test" in sys.argv:
        raise SystemExit(self_test(loaded_steps, loaded_host, loaded_gate))
    report(loaded_steps, loaded_host, loaded_gate)
