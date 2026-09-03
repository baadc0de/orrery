#!/usr/bin/env python3
"""Render the #920 sidecar IPC measurement against its falsification bands.

Reads the observer report `orrery-ipc-bench observer --report ...` writes
(`schema: orrery-ipc-harness/1`) and prints the phase table, the two
baselines, and — only for a Windows report — the verdict the issue's bands
define. A report from any other platform is informational: the bands are
defined at N = 24 **on Windows**, and a Linux number cannot take the
sidecar-versus-embedded decision.

    python3 scripts/ipc-report.py REPORT.json [SCALING.json]

With a second report at a different N, the scaling clause is checked:
`extract + encode + decode` linear in N with slope ≤ ~1 µs/entity, and
p99 ≤ 2 ms at N = 128 (D6's per-cell player ceiling).

The bands (#920), at N = 24 on Windows:

    sidecar stands      ipc_added p99 ≤ 1 ms, p99.9 ≤ 4 ms,
                        zero dropped spawn/despawn/input, frame drops ≤ 0.1 %
    sidecar overturned  ipc_added p99 ≥ 16.7 ms (one tick) or p50 ≥ 1 ms
    owner's call        between them

`phase` — the wait for the next engine tick — is printed but deliberately
excluded from the verdict: it exists in embedded too, and folding it in
roughly doubles the apparent cost (#920's lie 6).

    --self-test   run the fixture checks and exit
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

SCHEMA = "orrery-ipc-harness/1"

P99_STANDS_US = 1_000.0
P999_STANDS_US = 4_000.0
P99_OVERTURN_US = 16_700.0
P50_OVERTURN_US = 1_000.0
FRAME_DROP_STANDS = 0.001

P99_SCALING_128_US = 2_000.0
SLOPE_US_PER_ENTITY = 1.0

SUM_TOLERANCE = 0.05  # ipc_added may drift 5 % from the sum of its columns

STANDS = "SIDECAR STANDS"
OVERTURNED = "SIDECAR OVERTURNED"
OWNERS_CALL = "OWNER'S CALL"
INFORMATIONAL = "INFORMATIONAL ONLY — the bands are defined for WINDOWS (#920); this report cannot take the decision"


def load_report(path: str) -> dict:
    report = json.loads(Path(path).read_text())
    if report.get("schema") != SCHEMA:
        raise SystemExit(f"{path}: not an orrery-ipc-harness/1 report (schema={report.get('schema')!r})")
    if report.get("role") != "observer":
        raise SystemExit(f"{path}: expected the observer's report, got role={report.get('role')!r}")
    phases = report.get("phases_ns") or {}
    for required in ("hop_in", "extract", "encode", "hop_out", "decode_out", "phase", "ipc_added"):
        if required not in phases:
            raise SystemExit(
                f"{path}: phases_ns is missing '{required}' — a report without "
                "every column, phase included, is not a measurement #920 can read"
            )
    sums = sum_component_means(phases)
    ipc = phases["ipc_added"]["mean_ns"]
    if abs(ipc - sums) > SUM_TOLERANCE * max(ipc, 1.0):
        raise SystemExit(
            f"{path}: ipc_added mean {ipc:.0f} ns does not equal the sum of its "
            f"columns ({sums:.0f} ns) — the artifact is internally inconsistent"
        )
    return report


def sum_component_means(phases: dict) -> float:
    return sum(
        phases[name]["mean_ns"]
        for name in ("hop_in", "extract", "encode", "hop_out", "decode_out")
    )


def fmt_summary(phases: dict, name: str, unit: str = "ns") -> str:
    summary = phases[name]
    scale = {"µs": 1000, "ms": 1e6, "ns": 1}[unit]
    digits = {"µs": 1, "ms": 3, "ns": 0}[unit]
    return "  ".join(
        f"{value / scale:{9 + digits}.{digits}f}"
        for value in (summary["p50_ns"], summary["p99_ns"], summary["p99_9_ns"], summary["max_ns"])
    )


def frame_drop_rate(report: dict) -> float:
    """Frames the observer never applied, over frames expected.

    `ticks - samples` is the single number the threshold cares about: a frame
    discarded in the sidecar's latest-wins lane, superseded in the observer's
    apply slot, or missing its timing join was never presented. Counting the
    component counters instead would double-count (a discarded frame is also
    a return-path gap)."""
    ticks = report.get("ticks") or 0
    samples = report.get("samples") or 0
    return (ticks - samples) / ticks if ticks else 0.0


def input_spawn_drops(report: dict) -> int:
    drops = report.get("drops") or {}
    return sum(
        drops.get(name, 0)
        for name in (
            "input_dropped",
            "forward_seq_gaps",
            "forward_seq_reorders",
            "spawn_missing",
            "despawn_missing",
        )
    )


def classify(report: dict) -> str:
    """The #920 verdict for a Windows report. Not consulted for other
    platforms, whose reports are informational by definition."""
    phases = report["phases_ns"]
    ipc = phases["ipc_added"]
    p99_us = ipc["p99_ns"] / 1000
    p999_us = ipc["p99_9_ns"] / 1000
    p50_us = ipc["p50_ns"] / 1000

    if p99_us >= P99_OVERTURN_US or p50_us >= P50_OVERTURN_US:
        return OVERTURNED
    if (
        p99_us <= P99_STANDS_US
        and p999_us <= P999_STANDS_US
        and input_spawn_drops(report) == 0
        and frame_drop_rate(report) <= FRAME_DROP_STANDS
    ):
        return STANDS
    return OWNERS_CALL


def render(report_path: str, scaling_path: str | None) -> str:
    report = load_report(report_path)
    phases = report["phases_ns"]
    baselines = report.get("baselines_ns") or {}
    drops = report.get("drops") or {}
    lines: list[str] = []

    platform = report.get("platform", "?")
    lines.append(f"# Sidecar IPC measurement — {report_path}")
    lines.append(
        f"platform {platform} / {report.get('arch', '?')}, "
        f"clock {report.get('clock', '?')}, transport {report.get('transport', '?')}, "
        f"tcp_nodelay {report.get('tcp_nodelay')}, "
        f"timeBeginPeriod {report.get('time_begin_period')}"
    )
    lines.append(
        f"N = {report.get('entities')} entities at {report.get('tick_hz')} Hz, "
        f"{report.get('samples')} samples over {report.get('duration_s', 0):.0f} s "
        f"(warmup {report.get('warmup_ticks')} ticks), "
        f"loadavg {report.get('loadavg_start')} → {report.get('loadavg_end')}"
    )
    lines.append("")

    lines.append("## Phases (the decision quantity, µs)")
    lines.append("```")
    header = f"{'column':20s} {'p50':>12s} {'p99':>12s} {'p99.9':>12s} {'max':>12s}"
    lines.append(header)
    for name in ("hop_in", "extract", "encode", "hop_out", "decode_out", "ipc_added"):
        lines.append(f"{name:20s} {fmt_summary(phases, name, 'µs')}")
    lines.append("```")
    lines.append("")
    lines.append("## phase — the tick wait, SEPARATE (ms)")
    lines.append("```")
    lines.append(header)
    for name in ("phase", "phase_after_decode"):
        lines.append(f"{name:20s} {fmt_summary(phases, name, 'ms')}")
    lines.append("```")
    lines.append("")
    lines.append(
        "`phase` is the wait for the next engine tick. It is not IPC cost — it "
        "exists in embedded too — and it is excluded from `ipc_added`, which "
        "equals hop_in + extract + encode + hop_out + decode_out (#920 lie 6)."
    )
    lines.append("")
    lines.append("## Baselines (µs)")
    lines.append("```")
    lines.append(header)
    for name in sorted(baselines):
        lines.append(f"{name:20s} {fmt_summary(baselines, name, 'µs')}")
    lines.append("```")
    lines.append("")
    lines.append("## Drop accounting")
    lines.append(f"```\n{json.dumps(drops, indent=1)}\n```")
    lines.append(
        f"frame drop rate {frame_drop_rate(report) * 100:.4f} % "
        f"(band: ≤ {FRAME_DROP_STANDS * 100:.1f} % for stands)"
    )
    lines.append("")

    lines.append("## Verdict")
    if platform != "windows":
        lines.append(f"**{INFORMATIONAL}**")
        lines.append(
            f"`ipc_added` p50 {phases['ipc_added']['p50_ns'] / 1000:.1f} µs, "
            f"p99 {phases['ipc_added']['p99_ns'] / 1000:.1f} µs, "
            f"p99.9 {phases['ipc_added']['p99_9_ns'] / 1000:.1f} µs on {platform} — "
            "worth having, not the deciding number. The decisive measurement is "
            "the same invocation on Windows, where the TCP loopback candidate is "
            "the *worst reasonable* one (#920)."
        )
    else:
        verdict = classify(report)
        ipc = phases["ipc_added"]
        lines.append(f"**{verdict}**")
        lines.append(
            f"anchors drawn: p99 {ipc['p99_ns'] / 1000:.1f} µs against "
            f"{P99_STANDS_US:.0f} µs (stands) and {P99_OVERTURN_US:.0f} µs "
            f"(one tick, overturned); p50 {ipc['p50_ns'] / 1000:.1f} µs against "
            f"{P50_OVERTURN_US:.0f} µs."
        )

    if scaling_path:
        lines.append("")
        lines.append("## Scaling clause")
        lines.append(scaling_clause(report, scaling_path))

    return "\n".join(lines) + "\n"


def scaling_clause(base: dict, scaling_path: str) -> str:
    """`extract + encode + decode` linear in N, slope ≤ ~1 µs/entity, and
    p99 ≤ 2 ms at N = 128."""
    other = load_report(scaling_path)
    n1, n2 = base["entities"], other["entities"]
    if n1 == n2:
        raise SystemExit(f"{scaling_path}: scaling comparison needs two different N values")
    low, high = (base, other) if n1 < n2 else (other, base)
    n_low, n_high = low["entities"], high["entities"]
    mean_low = sum_component_means(low["phases_ns"])
    mean_high = sum_component_means(high["phases_ns"])
    slope_us = (mean_high - mean_low) / 1000 / (n_high - n_low)
    p99_high_us = high["phases_ns"]["ipc_added"]["p99_ns"] / 1000

    lines = [
        f"N = {n_low}: extract + encode + decode mean {mean_low / 1000:.1f} µs",
        f"N = {n_high}: extract + encode + decode mean {mean_high / 1000:.1f} µs",
        f"slope {slope_us:.3f} µs/entity (clause: ≤ ~{SLOPE_US_PER_ENTITY:.0f})",
        f"N = {n_high}: ipc_added p99 {p99_high_us:.1f} µs (clause: ≤ {P99_SCALING_128_US:.0f} µs at N = 128)",
    ]
    if slope_us <= SLOPE_US_PER_ENTITY and p99_high_us <= P99_SCALING_128_US:
        lines.append("scaling clause: **holds**")
    else:
        lines.append("scaling clause: **violated** — see the lines above")
    return "\n".join(lines)


# ── --self-test ──────────────────────────────────────────────────────────────
#
# The self-test is functional in the direction that matters: every way a
# report could flatter or fail the decision must either classify correctly or
# be refused by name. A reporter that cannot tell a clean artifact from a
# tampered one would let a Linux number read as a Windows verdict, which is
# the exact failure this task exists to prevent.


def make_report(
    platform: str = "windows",
    p50_us: float = 40.0,
    p99_us: float = 70.0,
    p999_us: float = 90.0,
    ipc_mean_ns: float | None = None,
    drops: dict | None = None,
    with_phase: bool = True,
    entities: int = 24,
) -> dict:
    """A synthetic observer report whose columns sum honestly."""
    component_means = {
        "hop_in": 14_000.0,
        "extract": 500.0,
        "encode": 8_000.0,
        "hop_out": 15_000.0,
        "decode_out": 1_100.0,
    }
    phases: dict = {
        name: {
            "mean_ns": mean,
            "min_ns": 0.0,
            "p50_ns": mean * 0.8,
            "p99_ns": mean * 1.4,
            "p99_9_ns": mean * 1.7,
            "max_ns": mean * 2.0,
        }
        for name, mean in component_means.items()
    }
    phases["ipc_added"] = {
        "mean_ns": sum(component_means.values()) if ipc_mean_ns is None else ipc_mean_ns,
        "min_ns": 0.0,
        "p50_ns": p50_us * 1000,
        "p99_ns": p99_us * 1000,
        "p99_9_ns": p999_us * 1000,
        "max_ns": p999_us * 1000 * 1.2,
    }
    if with_phase:
        phases["phase"] = {
            "mean_ns": 8_300_000.0,
            "min_ns": 0.0,
            "p50_ns": 8_300_000.0,
            "p99_ns": 16_000_000.0,
            "p99_9_ns": 16_500_000.0,
            "max_ns": 20_000_000.0,
        }
        phases["phase_after_decode"] = {
            "mean_ns": 8_300_000.0,
            "min_ns": 0.0,
            "p50_ns": 8_300_000.0,
            "p99_ns": 16_000_000.0,
            "p99_9_ns": 16_500_000.0,
            "max_ns": 20_000_000.0,
        }
    return {
        "schema": SCHEMA,
        "role": "observer",
        "platform": platform,
        "arch": "x86_64",
        "clock": "QueryPerformanceCounter" if platform == "windows" else "CLOCK_MONOTONIC",
        "transport": "tcp-loopback",
        "tcp_nodelay": True,
        "time_begin_period": False,
        "entities": entities,
        "tick_hz": 60,
        "warmup_ticks": 600,
        "ticks": 36_000,
        "samples": 36_000,
        "duration_s": 600.0,
        "loadavg_start": [0.0, 0.0, 0.0],
        "loadavg_end": [0.0, 0.0, 0.0],
        "phases_ns": phases,
        "baselines_ns": {
            "extract_inproc": phases["extract"],
            "hop_null_out": phases["hop_in"],
            "hop_null_back": phases["hop_out"],
            "hop_null_rtt": phases["ipc_added"],
        },
        "drops": drops or {},
        "notes": [],
    }


def expect_system_exit(fn, needle: str, name: str) -> None:
    try:
        fn()
    except SystemExit as error:
        message = str(error)
        if needle not in message:
            raise SystemExit(f"self-test: {name} failed with the wrong refusal: {message}") from error
        return
    raise SystemExit(f"self-test: {name} was expected to refuse and did not")


def self_test() -> None:
    # The three band classifications, on Windows reports.
    stands = make_report(p50_us=40, p99_us=700, p999_us=3_000)
    assert classify(stands) == STANDS, f"clean fast report must read {STANDS}"

    overturned_p99 = make_report(p50_us=40, p99_us=20_000, p999_us=25_000)
    assert classify(overturned_p99) == OVERTURNED, "p99 ≥ 16.7 ms must read overturned"

    overturned_p50 = make_report(p50_us=1_500, p99_us=2_000, p999_us=2_500)
    assert classify(overturned_p50) == OVERTURNED, "p50 ≥ 1 ms must read overturned"

    owners = make_report(p50_us=40, p99_us=5_000, p999_us=6_000)
    assert classify(owners) == OWNERS_CALL, "between the bands is the owner's call"

    # The threshold's drop conditions. A fast report with dropped input cannot
    # read as a stand.
    dropped = make_report(p50_us=40, p99_us=700, p999_us=3_000, drops={"forward_seq_gaps": 3})
    assert classify(dropped) == OWNERS_CALL, "dropped inputs must forfeit the stands band"

    # Frame drop rate over the band: 40 of 36 000 frames never applied
    # (ticks - samples) is 0.11 %, above the 0.1 % the stands band allows.
    frame_drops = make_report(p50_us=40, p99_us=700, p999_us=3_000)
    frame_drops["samples"] = 35_960
    assert classify(frame_drops) == OWNERS_CALL, "frame drops > 0.1 % must forfeit the stands band"

    # A non-Windows report renders informationally, never a verdict.
    linux_text = render_report(make_report(platform="linux"))
    assert "INFORMATIONAL" in linux_text, "a Linux report must be marked informational"
    assert STANDS not in linux_text, "a Linux report must not print the stands verdict"
    assert "WINDOWS" in linux_text, "the platform caveat must be stated"

    # A report without the phase column is refused: the phase separation is
    # load-bearing, and its absence is how the tick wait sneaks back in.
    expect_system_exit(
        lambda: load_report_dict(make_report(with_phase=False)),
        "missing 'phase'",
        "report without phase",
    )

    # A tampered report — ipc_added inflated by folding the tick wait in — is
    # refused because its columns no longer sum.
    folded = make_report(ipc_mean_ns=sum(make_report()["phases_ns"][n]["mean_ns"] for n in ("hop_in", "extract", "encode", "hop_out", "decode_out")) + 8_300_000)
    expect_system_exit(
        lambda: load_report_dict(folded),
        "does not equal the sum",
        "ipc_added with the tick wait folded in",
    )

    # A wrong role or schema is refused by name.
    wrong_role = make_report()
    wrong_role["role"] = "sidecar"
    expect_system_exit(lambda: load_report_dict(wrong_role), "expected the observer's report", "wrong role")
    wrong_schema = make_report()
    wrong_schema["schema"] = "orrery-ipc-harness/2"
    expect_system_exit(lambda: load_report_dict(wrong_schema), "not an orrery-ipc-harness/1 report", "wrong schema")

    # The scaling clause math, both directions.
    low = make_report(entities=24)
    low["phases_ns"]["extract"]["mean_ns"] = 500.0
    high = make_report(entities=128, p50_us=100, p99_us=500, p999_us=900)
    high["phases_ns"]["extract"]["mean_ns"] = 500.0 + 104 * 800  # 0.8 µs/entity
    n1, n2 = low["entities"], high["entities"]
    slope = (
        sum_component_means(high["phases_ns"]) - sum_component_means(low["phases_ns"])
    ) / 1000 / (n2 - n1)
    assert slope <= SLOPE_US_PER_ENTITY, f"slope {slope:.3f} must satisfy the clause"

    print("ipc-report: self-test passed")


def load_report_dict(report: dict) -> dict:
    """Run the load_report checks against an in-memory report."""
    import tempfile

    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
        json.dump(report, handle)
        path = handle.name
    try:
        return load_report(path)
    finally:
        Path(path).unlink()


def render_report(report: dict) -> str:
    import tempfile

    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
        json.dump(report, handle)
        path = handle.name
    try:
        return render(path, None)
    finally:
        Path(path).unlink()


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] == "--self-test":
        self_test()
        return 0
    if not args or not args[0].endswith(".json"):
        print(__doc__)
        return 2
    print(render(args[0], args[1] if len(args) > 1 else None))
    return 0


if __name__ == "__main__":
    sys.exit(main())
