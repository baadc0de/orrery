#!/usr/bin/env python3
"""Derive the journal-raw Phase 4 headline from its versioned evidence.

This is a measurement report, not a backing-store decision.  The paired gate
data lives in docs/data/p2-journal-raw-2026-08-20.jsonl; host identity and the
mandatory fio qualification live beside it in the device JSON.

    python3 scripts/p2-journal-raw-report.py
    python3 scripts/p2-journal-raw-report.py --self-test
"""

from __future__ import annotations

import copy
import json
import os
import pathlib
import statistics
import sys
from collections.abc import Callable

ROOT = pathlib.Path(__file__).resolve().parents[1]
DATA = pathlib.Path(
    os.environ.get(
        "P2_JOURNAL_RAW_DATA",
        ROOT / "docs/data/p2-journal-raw-2026-08-20.jsonl",
    )
)
DEVICE = pathlib.Path(
    os.environ.get(
        "P2_JOURNAL_RAW_DEVICE",
        ROOT / "docs/data/p2-journal-raw-device-2026-08-20.json",
    )
)


def load() -> tuple[list[dict], dict]:
    runs = [json.loads(line) for line in DATA.read_text().splitlines() if line.strip()]
    return runs, json.loads(DEVICE.read_text())


def median(rows: list[dict], getter: Callable[[dict], float]) -> float:
    return statistics.median(getter(row) for row in rows)


def validate(runs: list[dict], device: dict) -> list[str]:
    failures: list[str] = []

    def check(name: str, condition: bool, detail: str) -> None:
        if not condition:
            failures.append(f"{name}: {detail}")

    check("population", len(runs) == 10, f"expected 10 runs, found {len(runs)}")
    check(
        "backend population",
        {backend: sum(row.get("backend") == backend for row in runs) for backend in ("fjall", "raw")}
        == {"fjall": 5, "raw": 5},
        "the comparison must contain five runs of each backend",
    )

    expected_order = {
        pair: (["fjall", "raw"] if pair % 2 else ["raw", "fjall"])
        for pair in range(1, 6)
    }
    for pair, expected in expected_order.items():
        cell = sorted((row for row in runs if row.get("pair") == pair), key=lambda row: row.get("order", 0))
        actual = [row.get("backend") for row in cell]
        check(f"pair {pair} order", actual == expected, f"expected {expected}, found {actual}")
        check(
            f"pair {pair} order numbers",
            [row.get("order") for row in cell] == [1, 2],
            "each pair must contain order positions 1 and 2",
        )

    gate = device.get("gate", {})
    check("device pair count", gate.get("pairs") == 5, f"device says {gate.get('pairs')!r} pairs")
    check(
        "device order",
        gate.get("order") == [expected_order[pair] for pair in range(1, 6)],
        "device manifest no longer records the alternating arm order",
    )
    check("full gate duration", gate.get("duration_seconds") == 30, "device duration is not 30 seconds")

    fio_jobs = device.get("fio", {}).get("jobs", [])
    check("fio population", len(fio_jobs) == 2, f"expected two fio jobs, found {len(fio_jobs)}")
    for index, job in enumerate(fio_jobs, 1):
        check(f"fio job {index} rate", job.get("iops", 0) >= 469, f"only {job.get('iops')} IOPS")
        check(
            f"fio job {index} qualification",
            job.get("sync_max_ms", float("inf")) < 1.0,
            f"maximum {job.get('sync_max_ms')} ms is not below 1 ms",
        )

    required_stage = (
        "sync_data_us_max",
        "sync_data_us_max_bytes",
        "sync_data_us_max_records",
        "slow_syncs",
        "flushes",
        "records",
        "bytes",
    )
    for row in runs:
        label = row.get("label", "unlabelled")
        check(f"{label} duration", row.get("run", {}).get("duration_secs") == 30, "run was shortened")
        check(f"{label} recovery", row.get("recovery", {}).get("pass") is True, "recovery did not pass")
        check(
            f"{label} durable acknowledgements",
            539_000 <= row.get("durable_acks", 0) <= 542_000,
            f"count {row.get('durable_acks')}",
        )
        client = row.get("client", {})
        check(f"{label} lease loss", client.get("leases_lost") == 0, f"lost {client.get('leases_lost')}")
        check(f"{label} diff nacks", client.get("diff_nacks") == 0, f"saw {client.get('diff_nacks')}")
        check(
            f"{label} duplicate acknowledgements",
            client.get("duplicate_durable_acks") == 0,
            f"saw {client.get('duplicate_durable_acks')}",
        )
        histogram = row.get("journal_commit_histogram_us", {})
        histogram_n = sum(histogram.values()) if isinstance(histogram, dict) else -1
        series_n = row.get("series", {}).get("journal_commit_ms", {}).get("n")
        tail_n = row.get("journal_commit_tail", {}).get("n")
        check(
            f"{label} histogram",
            histogram_n == series_n == tail_n,
            f"histogram {histogram_n}, series {series_n}, tail {tail_n}",
        )
        stage = row.get("journal_stage", {})
        for field in required_stage:
            check(f"{label} stage field {field}", field in stage, "barrier-shape evidence missing")
        check(f"{label} on-disk bytes", row.get("on_disk_bytes", 0) > 700_000_000, "byte count missing")

    fjall = [row for row in runs if row.get("backend") == "fjall"]
    raw = [row for row in runs if row.get("backend") == "raw"]
    for row in fjall:
        label = row["label"]
        check(f"{label} fjall gate", row.get("gate") == "fail", "Fjall did not record the measured failure")
        check(
            f"{label} fjall root cause",
            "journal_commit_ms" in row.get("root_causes", []),
            f"root causes were {row.get('root_causes')}",
        )
        check(
            f"{label} fjall p99",
            row.get("series", {}).get("journal_commit_ms", {}).get("p99_us", 0) > 2_000,
            "Fjall no longer misses the 2 ms budget",
        )
        check(
            f"{label} fjall >2ms tail",
            row.get("journal_commit_tail", {}).get("pct_over_2ms", 0) > 3.0,
            "Fjall tail mass no longer matches the measurement",
        )
        check(
            f"{label} fjall >15ms tail",
            row.get("journal_commit_tail", {}).get("pct_over_15ms", 0) > 0,
            "Fjall's long tail disappeared",
        )
    for row in raw:
        label = row["label"]
        check(f"{label} raw gate", row.get("gate") == "pass", "journal-raw did not pass")
        check(f"{label} raw artifact", row.get("artifact_written") is True, "pass artifact missing")
        check(
            f"{label} raw p99",
            row.get("series", {}).get("journal_commit_ms", {}).get("p99_us", float("inf")) <= 2_000,
            "journal-raw misses the 2 ms budget",
        )
        check(
            f"{label} raw >2ms tail",
            row.get("journal_commit_tail", {}).get("pct_over_2ms", float("inf")) < 0.1,
            "journal-raw tail mass is no longer negligible",
        )
        check(
            f"{label} raw >15ms tail",
            row.get("journal_commit_tail", {}).get("pct_over_15ms") == 0,
            "journal-raw recorded commits over 15 ms",
        )

    if fjall and raw:
        byte_ratio = median(raw, lambda row: row["on_disk_bytes"]) / median(
            fjall, lambda row: row["on_disk_bytes"]
        )
        check(
            "on-disk comparability",
            0.75 <= byte_ratio <= 1.25,
            f"median raw/Fjall byte ratio {byte_ratio:.3f}; one arm did materially less work",
        )
    return failures


def report(runs: list[dict], device: dict) -> None:
    failures = validate(runs, device)
    if failures:
        raise SystemExit("evidence validation failed:\n  " + "\n  ".join(failures))

    host = device["host"]
    fio_jobs = device["fio"]["jobs"]
    print(f"journal-raw Phase 4 — {DATA.name}")
    print(f"  {host['machine_type']} / {host['zone']} / {host['nvme']}")
    print(
        "  fio job A: "
        f"{sum(job['iops'] for job in fio_jobs):.1f} barriers/s, "
        f"p99 {max(job['sync_p99_ms'] for job in fio_jobs):.3f} ms, "
        f"max {max(job['sync_max_ms'] for job in fio_jobs):.3f} ms — qualified\n"
    )

    print("  backend   gates  journal p99       >2 ms     >15 ms   sync max       on-disk bytes")
    for backend in ("fjall", "raw"):
        rows = [row for row in runs if row["backend"] == backend]
        p99 = [row["series"]["journal_commit_ms"]["p99_us"] / 1000 for row in rows]
        over_2 = [row["journal_commit_tail"]["pct_over_2ms"] for row in rows]
        over_15 = [row["journal_commit_tail"]["pct_over_15ms"] for row in rows]
        sync_max = [row["journal_sync_max_ms"] for row in rows]
        byte_counts = [row["on_disk_bytes"] for row in rows]
        passes = sum(row["gate"] == "pass" for row in rows)
        print(
            f"  {backend:<8} {passes}/5   "
            f"{statistics.median(p99):5.1f} ms [{min(p99):g}, {max(p99):g}]  "
            f"{statistics.median(over_2):7.3f}%  "
            f"{statistics.median(over_15):8.3f}%  "
            f"{statistics.median(sync_max):6.3f} ms [{max(sync_max):.3f}]  "
            f"{int(statistics.median(byte_counts)):,}"
        )

    ack_counts = [row["durable_acks"] for row in runs]
    print(
        "\n  correctness: 10/10 recovery proofs passed; "
        f"durable acknowledgements {min(ack_counts):,}–{max(ack_counts):,}; "
        "0 leases lost, 0 diff nacks, 0 duplicate durable acknowledgements"
    )
    print("  p99 values are bounded-histogram bucket upper bounds; tail percentages use the same buckets.")
    print("  Measurement only: this report makes no dependency or default-backend decision.")


def self_test(runs: list[dict], device: dict) -> int:
    failures = validate(runs, device)
    if failures:
        print("SELF-TEST FAILED")
        for failure in failures:
            print("  " + failure)
        return 1

    mutations: list[tuple[str, str, Callable[[list[dict], dict], None]]] = [
        ("population", "population", lambda rows, _dev: rows.pop()),
        ("backend population", "backend population", lambda rows, _dev: next(row for row in rows if row["backend"] == "raw").update(backend="fjall")),
        ("fio population", "fio population", lambda _rows, dev: dev["fio"]["jobs"].pop()),
        ("fio rate", "fio job 1 rate", lambda _rows, dev: dev["fio"]["jobs"][0].update(iops=0)),
        ("fio qualification", "fio job 1 qualification", lambda _rows, dev: dev["fio"]["jobs"][0].update(sync_max_ms=1.0)),
        ("alternation", "pair 1 order", lambda rows, _dev: rows[0].update(order=2)),
        ("order positions", "pair 1 order numbers", lambda rows, _dev: next(row for row in rows if row["pair"] == 1 and row["order"] == 2).update(order=3)),
        ("device pair count", "device pair count", lambda _rows, dev: dev["gate"].update(pairs=4)),
        ("device duration", "full gate duration", lambda _rows, dev: dev["gate"].update(duration_seconds=10)),
        ("recovery", "recovery", lambda rows, _dev: rows[0]["recovery"].__setitem__("pass", False)),
        ("raw gate", "raw gate", lambda rows, _dev: next(row for row in rows if row["backend"] == "raw").update(gate="fail")),
        ("fjall gate", "fjall gate", lambda rows, _dev: next(row for row in rows if row["backend"] == "fjall").update(gate="pass")),
        ("raw artifact", "raw artifact", lambda rows, _dev: next(row for row in rows if row["backend"] == "raw").update(artifact_written=False)),
        ("raw p99", "raw p99", lambda rows, _dev: next(row for row in rows if row["backend"] == "raw")["series"]["journal_commit_ms"].update(p99_us=3_000)),
        ("fjall p99", "fjall p99", lambda rows, _dev: next(row for row in rows if row["backend"] == "fjall")["series"]["journal_commit_ms"].update(p99_us=1_000)),
        ("raw tail", "raw >2ms tail", lambda rows, _dev: next(row for row in rows if row["backend"] == "raw")["journal_commit_tail"].update(pct_over_2ms=1.0)),
        ("raw long tail", "raw >15ms tail", lambda rows, _dev: next(row for row in rows if row["backend"] == "raw")["journal_commit_tail"].update(pct_over_15ms=0.1)),
        ("fjall tail", "fjall >2ms tail", lambda rows, _dev: next(row for row in rows if row["backend"] == "fjall")["journal_commit_tail"].update(pct_over_2ms=0)),
        ("fjall tail", "fjall >15ms tail", lambda rows, _dev: next(row for row in rows if row["backend"] == "fjall")["journal_commit_tail"].update(pct_over_15ms=0)),
        ("bytes", "on-disk comparability", lambda rows, _dev: [row.update(on_disk_bytes=1) for row in rows if row["backend"] == "raw"]),
        ("byte presence", "on-disk bytes", lambda rows, _dev: rows[0].update(on_disk_bytes=0)),
        ("duration", "duration", lambda rows, _dev: rows[0]["run"].update(duration_secs=10)),
        ("durable acks", "durable acknowledgements", lambda rows, _dev: rows[0].update(durable_acks=0)),
        ("lease loss", "lease loss", lambda rows, _dev: rows[0]["client"].update(leases_lost=1)),
        ("diff nacks", "diff nacks", lambda rows, _dev: rows[0]["client"].update(diff_nacks=1)),
        ("duplicate acks", "duplicate acknowledgements", lambda rows, _dev: rows[0]["client"].update(duplicate_durable_acks=1)),
        ("histogram", "histogram", lambda rows, _dev: rows[0]["journal_commit_histogram_us"].update({"999999": 1})),
        ("root cause", "fjall root cause", lambda rows, _dev: next(row for row in rows if row["backend"] == "fjall").update(root_causes=[])),
        ("device order", "device order", lambda _rows, dev: dev["gate"].update(order=[])),
    ]
    for field in (
        "sync_data_us_max",
        "sync_data_us_max_bytes",
        "sync_data_us_max_records",
        "slow_syncs",
        "flushes",
        "records",
        "bytes",
    ):
        mutations.append(
            (
                f"barrier shape {field}",
                f"stage field {field}",
                lambda rows, _dev, field=field: rows[0]["journal_stage"].pop(field),
            )
        )
    mutation_failures: list[str] = []
    for name, expected, mutate in mutations:
        mutated_runs, mutated_device = copy.deepcopy(runs), copy.deepcopy(device)
        mutate(mutated_runs, mutated_device)
        found = validate(mutated_runs, mutated_device)
        if not any(expected in failure for failure in found):
            mutation_failures.append(f"{name}: expected a failure containing {expected!r}, got {found}")

    if mutation_failures:
        print("SELF-TEST FAILED: mutation checks")
        for failure in mutation_failures:
            print("  " + failure)
        return 1
    print(
        f"SELF-TEST PASSED: {len(runs)} runs, five alternating pairs, "
        f"{len(mutations)} guarded-fact mutations rejected"
    )
    return 0


if __name__ == "__main__":
    loaded_runs, loaded_device = load()
    if "--self-test" in sys.argv:
        raise SystemExit(self_test(loaded_runs, loaded_device))
    report(loaded_runs, loaded_device)
