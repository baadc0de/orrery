#!/usr/bin/env python3
"""Derive every quantitative claim in docs/08 §2.2.1 from the raw sweep.

The section this feeds was published three times and corrected twice, and the
second correction reintroduced the defect it was written to fix: replacement
measurements asserted in prose, never re-derived, all failing in the same
direction because the same two runs were silently dropped. Patching claim by
claim did not work, so the rule changed:

    EVERY QUANTITATIVE CLAIM IN §2.2.1 MUST BE PRODUCED BY THIS SCRIPT, OR IT
    MUST NOT APPEAR.

That makes this file the section's only source of numbers. It reads the sweep
directory and prints, for every claim, the value **with the points it came
from, the leg those points belong to, the n, and the statistic**. A claim that
cannot be produced here is visibly absent from the output, and therefore has to
be absent from the doc.

Three habits are enforced mechanically rather than by review:

* :class:`Range` cannot be constructed without a population. A range with no n
  is a `TypeError`, not a sentence someone has to notice is missing one.
* Every row of every table names its leg, and cross-leg rows say so.
* Every subset states the subset rule and prints the population it was taken
  from, so "20 of 21 runs" can never quietly become "the 19 that agreed".

Usage::

    scripts/intent-tail-derive.py [SWEEP_DIR]        # default /home/baadc0de/intent-tail-sweep
    scripts/intent-tail-derive.py --self-test        # re-derive the known-false numbers
    scripts/intent-tail-derive.py --audit-doc        # no number in §2.2.1 that this
                                                     # script does not print

The sweep artifacts are not version-controlled (they are ~10 GB of JSONL);
`--self-test` is what makes the derivation checkable without them being in the
tree, by asserting the values the 2026-08-19 re-review established by hand.
"""

from __future__ import annotations

import json
import math
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_SWEEP = Path("/home/baadc0de/intent-tail-sweep")

# The slow cut is the instrument's own, not this script's:
# `DEFAULT_SLOW_THRESHOLD_US` in crates/orrery_persistd/src/intent/stages.rs.
CUT_US = 20_000

# The reporter writes at most one record of each kind per interval.
INTERVAL_MS = 250

# The regime split. This box has two fsync-cost regimes (docs/08 §4.3) and the
# runs land in two clusters with nothing between 110.8 ms and 169.4 ms of worst
# journal `sync_data`; the catalog prints the sorted column so the gap can be
# checked rather than believed. The threshold is stated here and nowhere else.
SLOW_REGIME_SYNC_MS = 150.0

# The FDB phases inside `execute`, in the order stages.rs accumulates them.
# `fdb_gap` is deliberately not here: it is the residual, and asking which
# *phase* is largest is a question about phases.
EXEC_PHASES = [
    "grv_us",
    "idem_read_us",
    "fence_us",
    "commit_us",
    "alloc_wait_us",
    "alloc_refill_us",
    "backoff_us",
]

# Server-span stages that name a thing being done, as opposed to the two
# residuals (`server_gap`, `fdb_gap`) that name time nothing claimed.
NAMED_SERVER_STAGES = ["admit_us", "spawn_wait_us", "reply_us"]


# --------------------------------------------------------------------------
# The run catalog. Leg membership, cadence and phasing are inputs to the
# sweep (run-sweep.sh / run-heartbeat.sh / run-quiet.sh), not properties of
# the artifacts, so they are declared here — and then cross-checked against
# the artifacts, because a mislabelled leg would poison every table below.
# --------------------------------------------------------------------------
LEG_SPEC = {
    "cal-i200": ("calibration", 3000, False),
    "i50": ("rate", 3000, False),
    "i200": ("rate", 3000, False),
    "i500": ("rate", 3000, False),
    "i1000": ("rate", 3000, False),
    "hb1_5": ("cadence", 1500, False),
    "hb3": ("cadence", 3000, False),
    "hb6": ("cadence", 6000, False),
    "hbph": ("cadence", 3000, True),
    "q-quiet": ("device", 3000, False),
    "q-loaded": ("device", 3000, False),
    "qph-quiet": ("device", 3000, True),
    "qph-loaded": ("device", 3000, True),
}


def split_label(label: str) -> tuple[str, str]:
    m = re.match(r"^(.*)-r(\d+)$", label)
    if not m:
        raise ValueError(f"unparseable run label {label!r}")
    return m.group(1), m.group(2)


# --------------------------------------------------------------------------
# Ranges that cannot be stated without their population
# --------------------------------------------------------------------------
class Range:
    """A min–max over a named population of points.

    Constructing one without the points it spans is impossible: the only
    constructor takes ``(value, point)`` pairs and a population label. That is
    the whole reason this class exists instead of ``min()``/``max()``.
    """

    def __init__(self, pairs, population: str, unit: str = "ms", fmt: str = ".2f"):
        pairs = list(pairs)
        if not pairs:
            raise ValueError(f"empty range over population {population!r}")
        for v, p in pairs:
            if p is None or p == "":
                raise TypeError("every range value must name the point it came from")
        if not population:
            raise TypeError("a range must state the population it is drawn from")
        self.pairs = pairs
        self.population = population
        self.unit = unit
        self.fmt = fmt
        self.lo, self.lo_pt = min(pairs, key=lambda x: x[0])
        self.hi, self.hi_pt = max(pairs, key=lambda x: x[0])
        self.n = len(pairs)

    def __str__(self) -> str:
        f = self.fmt
        return (
            f"{self.lo:{f}}–{self.hi:{f}} {self.unit} "
            f"(n={self.n} over {self.population}; "
            f"min {self.lo_pt}, max {self.hi_pt})"
        )


def emit(claim: str, value, *, leg: str, points: str, n, stat: str) -> None:
    """Print one claim with everything needed to audit it."""
    if isinstance(value, Range):
        body = str(value)
    else:
        body = str(value)
    print(f"  [{claim}] {body}")
    print(f"      leg={leg}  points={points}  n={n}  stat={stat}")


def head(title: str) -> None:
    print()
    print("=" * 78)
    print(title)
    print("=" * 78)


def sub(title: str) -> None:
    print()
    print(f"-- {title}")


# --------------------------------------------------------------------------
# Artifact readers
# --------------------------------------------------------------------------
def _jsonl(path: Path):
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            yield json.loads(line)
        except json.JSONDecodeError:
            continue


@dataclass
class Run:
    label: str
    point: str
    repeat: str
    leg: str
    heartbeat_ms: int
    phased: bool
    diff_hz: float
    duration_s: float
    stage_all: dict = field(default_factory=dict)
    stage_slow: dict = field(default_factory=dict)
    exemplars: list = field(default_factory=list)
    intervals: list = field(default_factory=list)
    journal: dict = field(default_factory=dict)
    client: dict = field(default_factory=dict)
    client_hist: dict = field(default_factory=dict)
    server_hist: dict = field(default_factory=dict)

    # -- derived, all one-liners so the doc can cite the expression ---------
    @property
    def journal_loaded(self) -> bool:
        return self.diff_hz >= 1.0

    @property
    def intents(self) -> int:
        return self.stage_all.get("intents", 0)

    @property
    def executed(self) -> int:
        return max(self.stage_all.get("executed", 0), 1)

    @property
    def slow_intents(self) -> int:
        return self.stage_slow.get("intents", 0)

    @property
    def slow_executed(self) -> int:
        return max(self.stage_slow.get("executed", 0), 1)

    @property
    def intent_rate(self) -> float:
        return self.intents / self.duration_s

    @property
    def slow_pct(self) -> float:
        return 100.0 * self.slow_intents / max(self.intents, 1)

    def all_mean_ms(self, key: str) -> float:
        return self.stage_all.get(f"{key}_sum", 0) / self.executed / 1000

    def tail_mean_ms(self, key: str) -> float:
        return self.stage_slow.get(f"{key}_sum", 0) / self.slow_executed / 1000

    def all_max_ms(self, key: str) -> float:
        return self.stage_all.get(f"{key}_max", 0) / 1000

    @property
    def fence_read_max_ms(self) -> float:
        """`fence_read_max_us` is already a maximum in the record, so it has
        no `_max` suffix and cannot go through `all_max_ms`. The fan-out
        hypothesis is about this number and not about `fence_us`."""
        return self.stage_all.get("fence_read_max_us", 0) / 1000

    @property
    def journal_sync_max_ms(self) -> float:
        return self.journal.get("sync_data_us_max", 0) / 1000

    @property
    def regime(self) -> str:
        return "slow" if self.journal_sync_max_ms >= SLOW_REGIME_SYNC_MS else "fast"

    @property
    def slow_exemplars(self) -> list:
        return [e for e in self.exemplars if e.get("server_us", 0) >= CUT_US]

    @property
    def slowest_exemplar(self) -> dict:
        return max(self.exemplars, key=lambda e: e.get("server_us", 0))

    @property
    def grv_seconds(self) -> float:
        return self.stage_all.get("grv_us_sum", 0) / 1e6

    @property
    def client_arrival_max_ms(self) -> float:
        return self.client.get("intent_arrival_max_us", 0) / 1000

    @property
    def bulk_rate(self) -> float:
        """Delivered diffs/s, from the client's own end-of-run count. The
        nominal figure (sessions x diff_hz x entities) is not what the run
        achieved, so the doc quotes this one."""
        return self.client.get("diffs", 0) / self.duration_s

    @property
    def bursts(self) -> float:
        """Renewal passes in the run. Only meaningful for a burst run: a
        phased run has no pass to count."""
        return self.duration_s * 1000 / self.heartbeat_ms

    @property
    def server_max_ms(self) -> float:
        return self.all_max_ms("server_us")


def read_run(d: Path) -> Run:
    point_meta = json.loads((d / "point.json").read_text())
    label = point_meta["label"]
    point, repeat = split_label(label)
    if point not in LEG_SPEC:
        raise KeyError(f"{label}: no leg declared for point {point!r}")
    leg, hb, phased = LEG_SPEC[point]

    run = Run(
        label=label,
        point=point,
        repeat=repeat,
        leg=leg,
        heartbeat_ms=hb,
        phased=phased,
        diff_hz=float(point_meta["diff_hz"]),
        duration_s=float(point_meta["duration_secs"]),
    )

    for rec in _jsonl(d / "primary-boundary.jsonl"):
        kind = rec.get("type")
        if kind == "gateway_intent_stage":
            acc = run.stage_slow if rec.get("scope") == "slow" else run.stage_all
            for k, v in rec.items():
                if k in ("type", "scope") or not isinstance(v, (int, float)):
                    continue
                if k.endswith("_max") or k == "fence_read_max_us":
                    acc[k] = max(acc.get(k, 0), v)
                else:
                    acc[k] = acc.get(k, 0) + v
        elif kind == "gateway_intent_exemplar":
            run.exemplars.append(rec)
            if run.intervals and "intent" not in run.intervals[-1]:
                run.intervals[-1]["intent"] = rec
            else:
                run.intervals.append({"intent": rec})
        elif kind == "gateway_route_stage":
            if run.intervals and "route" not in run.intervals[-1]:
                run.intervals[-1]["route"] = rec
            else:
                run.intervals.append({"route": rec})

    for rec in _jsonl(d / "primary-metrics.jsonl"):
        if rec.get("type") == "journal_stage_delta":
            for k, v in rec.items():
                if k == "type" or not isinstance(v, (int, float)):
                    continue
                if k.endswith("_max"):
                    run.journal[k] = max(run.journal.get(k, 0), v)
                else:
                    run.journal[k] = run.journal.get(k, 0) + v
        elif rec.get("series") == "gateway_intent_server_ms":
            _fold_hist(run.server_hist, rec)

    for rec in _jsonl(d / "load.jsonl"):
        if rec.get("series") == "intent_commit_ms":
            _fold_hist(run.client_hist, rec)

    run.client = _client_footer(d / "load.stderr")
    return run


def _fold_hist(hist: dict, rec: dict) -> None:
    if rec.get("type") == "sample_batch":
        hist[rec["value_us"]] = hist.get(rec["value_us"], 0) + rec["count"]
    elif rec.get("type") == "sample":
        hist[rec["value_us"]] = hist.get(rec["value_us"], 0) + 1


def _client_footer(path: Path) -> dict:
    """The client's own end-of-run line. `intent_arrival_max_us` lives only
    here — it is stamped by `IntentQueue::on_ack_at` when the ack arrives, so
    it is the one client number not distorted by the rig's poll cadence."""
    if not path.exists():
        return {}
    last = None
    for line in path.read_text(errors="replace").splitlines():
        if "run complete" in line:
            last = line
    if last is None:
        return {}
    clean = re.sub(r"\x1b\[[0-9;]*m", "", last)
    return {k: float(v) for k, v in re.findall(r"(\w+)=([0-9.]+)", clean)}


def lattice_pct(hist: dict, q: float):
    """Percentile off the D16 lattice histogram.

    The value returned is the lattice bucket's upper bound, not an
    interpolation: the lattice's neighbours around the tail are 100 / 150 /
    200 / 300 ms, so a p99 read here is only ever accurate to its bucket. Every
    caller prints it as a bucket for that reason.
    """
    total = sum(hist.values())
    if not total:
        return None
    want = q * total
    acc = 0
    for k in sorted(hist):
        acc += hist[k]
        if acc >= want:
            return k / 1000
    return max(hist) / 1000


# --------------------------------------------------------------------------
# Statistics
# --------------------------------------------------------------------------
def mean(xs):
    xs = list(xs)
    return sum(xs) / len(xs)


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def pearson(xs, ys):
    n = len(xs)
    mx, my = mean(xs), mean(ys)
    num = sum((a - mx) * (b - my) for a, b in zip(xs, ys))
    den = math.sqrt(sum((a - mx) ** 2 for a in xs) * sum((b - my) ** 2 for b in ys))
    return num / den if den else float("nan")


def _ranks(v):
    order = sorted(range(len(v)), key=lambda i: v[i])
    r = [0.0] * len(v)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and v[order[j + 1]] == v[order[i]]:
            j += 1
        avg = (i + j) / 2 + 1
        for k in range(i, j + 1):
            r[order[k]] = avg
        i = j + 1
    return r


def spearman(xs, ys):
    return pearson(_ranks(xs), _ranks(ys))


def dominant_phase(ex: dict) -> str:
    return max(EXEC_PHASES, key=lambda k: ex.get(k, 0))


# --------------------------------------------------------------------------
# Report sections
# --------------------------------------------------------------------------
def load_all(sweep: Path):
    runs = []
    for d in sorted(p for p in sweep.iterdir() if (p / "point.json").exists()):
        runs.append(read_run(d))
    return runs


def section_catalog(runs):
    head("0. RUN CATALOG — the population every later claim is a subset of")
    print(
        "Legs are sweep inputs (run-sweep.sh, run-heartbeat.sh, run-quiet.sh),\n"
        "declared in LEG_SPEC and cross-checked below against the artifacts.\n"
        "`loaded` = bulk at diff_hz 2 (~18.5k diffs/s); `quiet` = diff_hz 0.05.\n"
        f"Regime: slow iff journal worst sync_data >= {SLOW_REGIME_SYNC_MS:.0f} ms.\n"
    )
    print(
        f"{'run':<15} {'leg':<11} {'hb':>5} {'phase':<7} {'bulk':<6} "
        f"{'intents':>8} {'/s':>7} {'diffs/s':>8} {'slow%':>6} {'regime':<6} "
        f"{'jsync_max':>9} {'lockivals':>9} {'lock_max':>8}"
    )
    for r in runs:
        n_lock = sum(1 for iv in r.intervals if (iv.get("route") or {}).get("batch_locks", 0))
        lock_max = max(
            [(iv.get("route") or {}).get("batch_locks", 0) for iv in r.intervals] or [0]
        )
        print(
            f"{r.label:<15} {r.leg:<11} {r.heartbeat_ms:>5} "
            f"{'phased' if r.phased else 'burst':<7} "
            f"{'loaded' if r.journal_loaded else 'quiet':<6} "
            f"{r.intents:>8} {r.intent_rate:>7.1f} {r.bulk_rate:>8.0f} "
            f"{r.slow_pct:>6.2f} "
            f"{r.regime:<6} {r.journal_sync_max_ms:>8.1f}m {n_lock:>9} {lock_max:>8}"
        )
    print(
        "\n  cross-check: `lockivals`/`lock_max` are the router's own\n"
        "  `batch_locks` per 250 ms interval. LEG_SPEC declares the phasing;\n"
        "  the artifacts have to agree with it or every table below is\n"
        "  mislabelled, so the agreement is asserted, not asserted-in-prose:\n"
        "  a burst run must reach 10 000 locks in fewer than half its\n"
        "  intervals; a phased run must spread them over every interval and\n"
        "  never reach 10 000."
    )
    disagree = []
    for r in runs:
        n_lock = sum(1 for iv in r.intervals if (iv.get("route") or {}).get("batch_locks", 0))
        lock_max = max(
            [(iv.get("route") or {}).get("batch_locks", 0) for iv in r.intervals] or [0]
        )
        if r.phased:
            ok = lock_max < 10_000 and n_lock >= 0.9 * len(r.intervals)
        else:
            ok = lock_max == 10_000
        if not ok:
            disagree.append(f"{r.label} (declared {'phased' if r.phased else 'burst'}, "
                            f"{n_lock}/{len(r.intervals)} intervals, max {lock_max})")
    emit(
        "catalog.phasing_crosscheck",
        "every run's measured lease-batch distribution matches its declared "
        "phasing" if not disagree else "DISAGREEMENT: " + "; ".join(disagree),
        leg="all three legs + calibration",
        points=f"{len(runs)} runs",
        n=len(runs),
        stat="declared phasing vs measured batch_locks distribution",
    )

    loaded = [r for r in runs if r.journal_loaded]
    quiet = [r for r in runs if not r.journal_loaded]
    print()
    emit(
        "population.loaded",
        f"{len(loaded)} runs: " + ", ".join(r.label for r in loaded),
        leg="all three legs + calibration",
        points="see list",
        n=len(loaded),
        stat="count of runs with bulk at diff_hz 2",
    )
    emit(
        "population.quiet",
        f"{len(quiet)} runs: " + ", ".join(r.label for r in quiet),
        leg="device",
        points="see list",
        n=len(quiet),
        stat="count of runs with bulk at diff_hz 0.05",
    )
    emit(
        "population.bulk_rate",
        Range([(r.bulk_rate, r.label) for r in loaded], "all loaded runs",
              unit="delivered diffs/s", fmt=".0f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="diffs acknowledged, from the client's end-of-run count, over 30 s",
    )
    emit(
        "population.bulk_rate_quiet",
        Range([(r.bulk_rate, r.label) for r in quiet], "all 4 quiet runs",
              unit="delivered diffs/s", fmt=".0f"),
        leg="device",
        points=", ".join(r.label for r in quiet),
        n=len(quiet),
        stat="diffs acknowledged, from the client's end-of-run count, over 30 s",
    )
    sortedsync = sorted((round(r.journal_sync_max_ms, 1) for r in loaded))
    print(f"\n  journal worst sync_data over the loaded runs, sorted: {sortedsync}")
    print(
        f"  the gap the {SLOW_REGIME_SYNC_MS:.0f} ms threshold sits in: "
        f"{max(x for x in sortedsync if x < SLOW_REGIME_SYNC_MS)} -> "
        f"{min(x for x in sortedsync if x >= SLOW_REGIME_SYNC_MS)} ms"
    )
    return loaded, quiet


def section_headline(runs):
    head("1. STAGE ARITHMETIC on one exemplar")
    # The headline is the slowest exemplar of the calibration point, which is
    # the point the section describes; naming it by leg rather than by "the
    # 130 ms one" is the difference between a subset and a cherry-pick.
    cal = [r for r in runs if r.leg == "calibration"]
    if not cal:
        print("  no calibration run in this sweep — claim omitted")
        return None
    r = cal[0]
    e = r.slowest_exemplar
    print(
        f"  point {r.label} (leg=calibration, {r.intent_rate:.1f} intents/s, "
        f"n={r.intents} intents in the run)\n"
        f"  the exemplar is the slowest single intent of that run.\n"
    )
    order = [
        ("server_us", "receipt -> reply (the span)"),
        ("admit_us", "ed25519 verify + validator"),
        ("spawn_wait_us", "tokio::spawn -> first poll"),
        ("exec_us", "inside IntentExecutor::execute"),
        ("grv_us", "  get-read-version"),
        ("idem_read_us", "  intent/{id}"),
        ("fence_us", "  fence fan-out"),
        ("commit_us", "  closure end -> commit resolved"),
        ("alloc_wait_us", "  PersistId allocator mutex"),
        ("alloc_refill_us", "  PersistId refill txn"),
        ("backoff_us", "  retry backoff"),
        ("fdb_gap_us", "  RESIDUAL inside execute"),
        ("server_gap_us", "RESIDUAL inside the span"),
        ("reply_us", "reply handoff"),
    ]
    for k, why in order:
        print(f"    {k:<17} {e.get(k, 0):>8} us   {why}")
    print(f"    {'attempts':<17} {e.get('attempts', 0):>8}      (1 = no retry)")
    print(f"    {'fence_reads':<17} {e.get('fence_reads', 0):>8}")
    print(f"    {'fence_read_max_us':<17} {e.get('fence_read_max_us', 0):>8} us")

    phase_sum = sum(e.get(k, 0) for k in EXEC_PHASES)
    exec_acct = phase_sum + e.get("fdb_gap_us", 0)
    named = sum(e.get(k, 0) for k in NAMED_SERVER_STAGES) + phase_sum
    residual = e.get("fdb_gap_us", 0) + e.get("server_gap_us", 0)
    span_acct = (
        sum(e.get(k, 0) for k in NAMED_SERVER_STAGES)
        + e.get("exec_us", 0)
        + e.get("server_gap_us", 0)
    )
    print()
    emit(
        "headline.exec_closed",
        f"{exec_acct} of {e['exec_us']} us  (phases {phase_sum} + fdb_gap "
        f"{e.get('fdb_gap_us', 0)}); unclaimed {e['exec_us'] - exec_acct} us",
        leg="calibration",
        points=r.label,
        n=1,
        stat="exec_us decomposition on the slowest exemplar",
    )
    emit(
        "headline.named_vs_span",
        f"{named} of {e['server_us']} us named; "
        f"difference {e['server_us'] - named} us against emitted residuals "
        f"fdb_gap+server_gap = {residual} us",
        leg="calibration",
        points=r.label,
        n=1,
        stat="named stages vs server span on the slowest exemplar",
    )
    emit(
        "headline.span_closed",
        f"{span_acct} of {e['server_us']} us "
        f"(admit+spawn_wait+exec+server_gap+reply); "
        f"overshoot {span_acct - e['server_us']} us from independent Instant reads",
        leg="calibration",
        points=r.label,
        n=1,
        stat="server span decomposition on the slowest exemplar",
    )
    emit(
        "headline.dominant",
        f"{dominant_phase(e)} = {e[dominant_phase(e)]} us "
        f"({100 * e[dominant_phase(e)] / e['server_us']:.1f} % of the span)",
        leg="calibration",
        points=r.label,
        n=1,
        stat="largest FDB phase on the slowest exemplar",
    )
    return r


def section_rate_leg(runs):
    head("2. RATE LEG — the tail's size against a 20x change in intent rate")
    rate = [r for r in runs if r.leg == "rate"]
    print(
        "  All eight runs: bulk at diff_hz 2, burst renewal at 3 s, unphased.\n"
        "  TAIL columns are means over the intents past the 20 ms cut only;\n"
        "  n_tail is that population, per run.\n"
    )
    print(
        f"{'run':<10} {'/s':>7} {'n':>7} {'n_tail':>7} {'slow%':>6} "
        f"{'cli p50':>8} {'cli p99':>8} {'srv p99':>8} | "
        f"{'srv':>7} {'grv':>7} {'fence':>7} {'commit':>7} {'fdb_gap':>7} {'retry':>5}"
    )
    for r in sorted(rate, key=lambda r: (r.intent_rate, r.label)):
        retries = r.stage_all.get("attempts", 0) - r.stage_all.get("executed", 0)
        print(
            f"{r.label:<10} {r.intent_rate:>7.1f} {r.intents:>7} "
            f"{r.slow_intents:>7} {r.slow_pct:>6.2f} "
            f"{lattice_pct(r.client_hist, .5):>8.1f} "
            f"{lattice_pct(r.client_hist, .99):>8.1f} "
            f"{lattice_pct(r.server_hist, .99):>8.1f} | "
            f"{r.tail_mean_ms('server_us'):>7.1f} {r.tail_mean_ms('grv_us'):>7.1f} "
            f"{r.tail_mean_ms('fence_us'):>7.1f} {r.tail_mean_ms('commit_us'):>7.1f} "
            f"{r.tail_mean_ms('fdb_gap_us'):>7.3f} {retries:>5}"
        )
    print("  (client/server percentiles are D16 lattice buckets, not interpolations)")
    print()
    lo = [r for r in rate if r.intent_rate < 600]
    emit(
        "rate.tail_server_below_600",
        Range([(r.tail_mean_ms("server_us"), r.label) for r in lo],
              "the 6 rate-leg runs at 47–485 intents/s"),
        leg="rate",
        points=", ".join(r.label for r in lo),
        n=len(lo),
        stat="mean server span over intents past the 20 ms cut",
    )
    emit(
        "rate.tail_grv_all",
        Range([(r.tail_mean_ms("grv_us"), r.label) for r in rate],
              "all 8 rate-leg runs"),
        leg="rate",
        points=", ".join(r.label for r in rate),
        n=len(rate),
        stat="mean grv over intents past the 20 ms cut",
    )
    emit(
        "rate.retries",
        f"attempts - executed = "
        f"{sum(r.stage_all.get('attempts', 0) - r.stage_all.get('executed', 0) for r in rate)}"
        f" over {sum(r.intents for r in rate)} intents",
        leg="rate",
        points=", ".join(r.label for r in rate),
        n=len(rate),
        stat="silent db.run retries, summed",
    )


def section_cadence_leg(runs):
    head("3. CADENCE LEG — move the renewal cadence, then move only its shape")
    cad = [r for r in runs if r.leg == "cadence"]
    print(
        "  All eight runs: bulk at diff_hz 2, intent mix held at ~200/s.\n"
        "  hb1_5 / hb3 / hb6 change how often the renewal pass runs;\n"
        "  hbph runs the same 3 s pass phased across the period, so it changes\n"
        "  only the SHAPE at unchanged renewal work.\n"
        "  Every row is one run — no repeat is averaged into another.\n"
    )
    print(
        f"{'run':<10} {'hb ms':>6} {'phase':<7} {'regime':<6} {'n':>6} {'n_tail':>7} "
        f"{'slow%':>6} | {'srv':>7} {'grv':>7} {'commit':>7} {'grv_s':>7}"
    )
    for r in sorted(cad, key=lambda r: (r.phased, r.heartbeat_ms, r.label)):
        print(
            f"{r.label:<10} {r.heartbeat_ms:>6} {'phased' if r.phased else 'burst':<7} "
            f"{r.regime:<6} {r.intents:>6} {r.slow_intents:>7} {r.slow_pct:>6.2f} | "
            f"{r.tail_mean_ms('server_us'):>7.1f} {r.tail_mean_ms('grv_us'):>7.2f} "
            f"{r.tail_mean_ms('commit_us'):>7.1f} {r.grv_seconds:>7.2f}"
        )
    print("  grv_s = total grv time over the WHOLE run, all intents, in seconds.")
    print()
    burst3 = [r for r in cad if r.heartbeat_ms == 3000 and not r.phased]
    phased3 = [r for r in cad if r.phased]
    emit(
        "cadence.grv_seconds_burst3",
        Range([(r.grv_seconds, r.label) for r in burst3],
              "the 2 cadence-leg runs at 3 s burst renewal", unit="s"),
        leg="cadence",
        points=", ".join(r.label for r in burst3),
        n=len(burst3),
        stat="sum of grv over every intent in the run",
    )
    emit(
        "cadence.grv_seconds_phased3",
        Range([(r.grv_seconds, r.label) for r in phased3],
              "the 2 cadence-leg runs at 3 s phased renewal", unit="s"),
        leg="cadence",
        points=", ".join(r.label for r in phased3),
        n=len(phased3),
        stat="sum of grv over every intent in the run",
    )
    ex_b = sum(r.stage_all.get("executed", 0) for r in burst3)
    ex_p = sum(r.stage_all.get("executed", 0) for r in phased3)
    emit(
        "cadence.work_delta",
        f"phased executed {ex_p} vs burst executed {ex_b} "
        f"= {100 * (ex_p - ex_b) / ex_b:+.1f} % intents",
        leg="cadence",
        points=", ".join(r.label for r in burst3 + phased3),
        n=len(burst3) + len(phased3),
        stat="executed-intent count, phased pair vs burst pair",
    )
    emit(
        "cadence.tail_grv_phased",
        Range([(r.tail_mean_ms("grv_us"), r.label) for r in phased3],
              "the 2 cadence-leg phased runs"),
        leg="cadence",
        points=", ".join(r.label for r in phased3),
        n=len(phased3),
        stat="mean grv over intents past the 20 ms cut",
    )
    allphased = [r for r in runs if r.phased and r.journal_loaded]
    print()
    print("  every loaded phased run, so a claim about 'phased' has a population:")
    print(f"  {'run':<15} {'leg':<9} {'regime':<6} {'tail grv':>9} {'tail commit':>12}")
    for r in sorted(allphased, key=lambda r: r.label):
        print(f"  {r.label:<15} {r.leg:<9} {r.regime:<6} "
              f"{r.tail_mean_ms('grv_us'):>8.2f}m {r.tail_mean_ms('commit_us'):>11.2f}m")
    emit(
        "cadence.tail_grv_all_phased",
        Range([(r.tail_mean_ms("grv_us"), r.label) for r in allphased],
              "all 4 loaded phased runs"),
        leg="cadence + device (CROSS-LEG)",
        points=", ".join(r.label for r in sorted(allphased, key=lambda r: r.label)),
        n=len(allphased),
        stat="mean grv over intents past the 20 ms cut",
    )
    emit(
        "cadence.tail_commit_phased",
        Range([(r.tail_mean_ms("commit_us"), r.label) for r in phased3],
              "the 2 cadence-leg phased runs"),
        leg="cadence",
        points=", ".join(r.label for r in phased3),
        n=len(phased3),
        stat="mean commit over intents past the 20 ms cut",
    )


def section_burst_constancy(runs):
    head("3b. PER-BURST CONSTANCY — one stall, at whatever cadence it is run")
    print(
        "  Restricted, and here is the restriction and its reason: LOADED runs\n"
        "  at ~200 intents/s with an UNPHASED renewal pass. Aggregate GRV time\n"
        "  scales with the number of intents, so a run at 47 or 970 intents/s\n"
        "  cannot share a row with one at 200; and a phased run has no pass to\n"
        "  divide by. This is a CROSS-LEG population — rate, cadence, device\n"
        "  and calibration all contribute rows — because the cadence is the\n"
        "  variable and the leg is not.\n"
    )
    pop = [r for r in runs
           if r.journal_loaded and not r.phased and 150 <= r.intent_rate <= 300]
    print(
        f"{'run':<15} {'leg':<11} {'hb ms':>6} {'regime':<6} {'passes':>7} "
        f"{'grv total s':>11} {'s per pass':>11} {'slow%':>6}"
    )
    for r in sorted(pop, key=lambda r: (r.heartbeat_ms, r.label)):
        print(
            f"{r.label:<15} {r.leg:<11} {r.heartbeat_ms:>6} {r.regime:<6} "
            f"{r.bursts:>7.0f} {r.grv_seconds:>11.2f} "
            f"{r.grv_seconds / r.bursts:>11.2f} {r.slow_pct:>6.2f}"
        )
    print()
    emit(
        "burst.grv_per_pass",
        Range([(r.grv_seconds / r.bursts, r.label) for r in pop],
              "loaded unphased runs at ~200 intents/s, cadences 1.5/3/6 s",
              unit="s of aggregate GRV per renewal pass"),
        leg="rate + cadence + device + calibration (CROSS-LEG)",
        points=", ".join(r.label for r in sorted(pop, key=lambda r: r.label)),
        n=len(pop),
        stat="run-total grv divided by the number of renewal passes in the run",
    )
    fast = [r for r in pop if r.regime == "fast"]
    slowr = [r for r in pop if r.regime == "slow"]
    emit(
        "burst.grv_per_pass_fast_regime",
        Range([(r.grv_seconds / r.bursts, r.label) for r in fast],
              f"the {len(fast)} of those runs in the fast fsync regime",
              unit="s per pass"),
        leg="rate + cadence + device + calibration (CROSS-LEG)",
        points=", ".join(r.label for r in sorted(fast, key=lambda r: r.label)),
        n=len(fast),
        stat="same statistic, restricted to the fast fsync regime",
    )
    emit(
        "burst.grv_per_pass_slow_regime",
        Range([(r.grv_seconds / r.bursts, r.label) for r in slowr],
              f"the {len(slowr)} of those runs in the slow fsync regime",
              unit="s per pass"),
        leg="cadence (this subset happens to be one leg)",
        points=", ".join(r.label for r in sorted(slowr, key=lambda r: r.label)),
        n=len(slowr),
        stat="same statistic, restricted to the slow fsync regime",
    )
    burst3 = [r for r in pop if r.heartbeat_ms == 3000]
    phased3 = [r for r in runs if r.phased and r.journal_loaded]
    emit(
        "burst.grv_total_burst_3s",
        Range([(r.grv_seconds, r.label) for r in burst3],
              f"the {len(burst3)} loaded unphased ~200/s runs at 3 s", unit="s"),
        leg="rate + cadence + device + calibration (CROSS-LEG)",
        points=", ".join(r.label for r in sorted(burst3, key=lambda r: r.label)),
        n=len(burst3),
        stat="run-total grv over every intent",
    )
    emit(
        "burst.grv_total_phased_3s",
        Range([(r.grv_seconds, r.label) for r in phased3],
              f"the {len(phased3)} loaded phased runs at 3 s", unit="s"),
        leg="cadence + device (CROSS-LEG)",
        points=", ".join(r.label for r in sorted(phased3, key=lambda r: r.label)),
        n=len(phased3),
        stat="run-total grv over every intent",
    )
    print(
        "\n  the two populations above do not overlap and are the same\n"
        "  configuration apart from the renewal pass's shape."
    )


def section_periodicity(runs):
    head("4. PERIODICITY — is the tail spread over the run, or on a cadence?")
    print(
        "  One row per run. `spikes` counts 250 ms report intervals whose\n"
        "  exemplar exceeded 40 ms. `gaps` is the spacing between consecutive\n"
        "  spike intervals, in intervals; x250 ms gives the period.\n"
        "  `lock&spike` counts intervals that are BOTH a spike and an interval\n"
        "  in which the router did batched lease work.\n"
    )
    print(
        f"{'run':<15} {'leg':<11} {'phase':<7} {'ivals':>6} {'spikes':>6} "
        f"{'median gap':>11} {'lockivals':>9} {'lock&spike':>10}"
    )
    for r in runs:
        spikes = [
            i for i, iv in enumerate(r.intervals)
            if (iv.get("intent") or {}).get("server_us", 0) >= 40_000
        ]
        locks = [
            i for i, iv in enumerate(r.intervals)
            if (iv.get("route") or {}).get("batch_locks", 0)
        ]
        gaps = [b - a for a, b in zip(spikes, spikes[1:])]
        mg = f"{median(gaps) * INTERVAL_MS:.0f} ms" if gaps else "-"
        print(
            f"{r.label:<15} {r.leg:<11} {'phased' if r.phased else 'burst':<7} "
            f"{len(r.intervals):>6} {len(spikes):>6} {mg:>11} "
            f"{len(locks):>9} {len(set(spikes) & set(locks)):>10}"
        )

    sub("the clean instance, stated as one run and not as a range")
    # i200-r1 is named because it is the point the section's headline
    # configuration matches; the table above is what makes it a representative
    # of a population rather than a pick.
    for r in runs:
        if r.label != "i200-r1":
            continue
        spikes = [
            i for i, iv in enumerate(r.intervals)
            if (iv.get("intent") or {}).get("server_us", 0) >= 40_000
        ]
        locks = [
            i for i, iv in enumerate(r.intervals)
            if (iv.get("route") or {}).get("batch_locks", 0)
        ]
        gaps = [b - a for a, b in zip(spikes, spikes[1:])]
        vals = sorted({(iv.get("route") or {}).get("batch_locks", 0)
                       for iv in r.intervals
                       if (iv.get("route") or {}).get("batch_locks", 0)})
        emit(
            "period.i200_r1_gaps",
            f"{len(spikes)} spike intervals; gaps (intervals) {gaps}; "
            f"in ms {[g * INTERVAL_MS for g in gaps]}",
            leg="rate",
            points=r.label,
            n=len(spikes),
            stat="spacing of report intervals whose exemplar exceeded 40 ms",
        )
        emit(
            "period.i200_r1_locks",
            f"{len(locks)} intervals with batch_locks > 0, values {vals}; "
            f"{len(set(spikes) & set(locks))} of {len(spikes)} spike intervals "
            f"coincide with one",
            leg="rate",
            points=r.label,
            n=len(r.intervals),
            stat="router batched lease acquisitions per 250 ms interval",
        )

    sub("does the spike spacing equal the configured cadence?")
    print(
        "  Population: LOADED, UNPHASED, ~200 intents/s — the same 11 runs as\n"
        "  section 3b, and for the same reason. A match means the median gap\n"
        "  between 40 ms spike intervals equals the run's renewal cadence.\n"
    )
    pop = [r for r in runs
           if r.journal_loaded and not r.phased and 150 <= r.intent_rate <= 300]
    match, miss = [], []
    for r in sorted(pop, key=lambda r: (r.heartbeat_ms, r.label)):
        spikes = [
            i for i, iv in enumerate(r.intervals)
            if (iv.get("intent") or {}).get("server_us", 0) >= 40_000
        ]
        gaps = [b - a for a, b in zip(spikes, spikes[1:])]
        mg = median(gaps) * INTERVAL_MS if gaps else None
        ok = mg is not None and abs(mg - r.heartbeat_ms) < 1
        (match if ok else miss).append((r, mg))
        print(
            f"  {r.label:<15} leg={r.leg:<11} hb={r.heartbeat_ms:>5} ms  "
            f"median gap {('%.0f ms' % mg) if mg else '-':>8}  "
            f"regime {r.regime:<5} {'MATCH' if ok else 'no'}"
        )
    emit(
        "period.gap_equals_cadence",
        f"{len(match)} of {len(pop)} runs; the {len(miss)} that miss are "
        + ", ".join(f"{r.label} ({r.regime} regime, journal sync max "
                    f"{r.journal_sync_max_ms:.1f} ms)" for r, _ in miss),
        leg="rate + cadence + device + calibration (CROSS-LEG)",
        points=", ".join(r.label for r in sorted(pop, key=lambda r: r.label)),
        n=len(pop),
        stat="median spike-interval spacing equal to the configured renewal cadence",
    )

    sub("periodicity across the whole cadence leg")
    for r in [x for x in runs if x.leg == "cadence"]:
        spikes = [
            i for i, iv in enumerate(r.intervals)
            if (iv.get("intent") or {}).get("server_us", 0) >= 40_000
        ]
        gaps = [b - a for a, b in zip(spikes, spikes[1:])]
        if not gaps:
            print(f"  {r.label:<10} no gap: {len(spikes)} spike interval(s)")
            continue
        print(
            f"  {r.label:<10} hb={r.heartbeat_ms:>5} ms "
            f"{'phased' if r.phased else 'burst':<7} "
            f"spikes={len(spikes):>3} median gap="
            f"{median(gaps) * INTERVAL_MS:>6.0f} ms"
        )


def section_device(loaded):
    head("5. DEVICE — the intent commit and the journal fsync are one event")
    print(
        "  One row per LOADED run; both columns are maxima over the same 30 s\n"
        "  window on the same md2 array. This is a cross-subsystem row, not a\n"
        "  cross-leg one: the pairing is within a run.\n"
    )
    print(
        f"{'run':<15} {'leg':<11} {'regime':<6} "
        f"{'journal sync max':>16} {'FDB commit max':>15} {'tail commit mean':>17}"
    )
    for r in sorted(loaded, key=lambda r: r.journal_sync_max_ms):
        print(
            f"{r.label:<15} {r.leg:<11} {r.regime:<6} "
            f"{r.journal_sync_max_ms:>15.1f}m {r.all_max_ms('commit_us'):>14.1f}m "
            f"{r.tail_mean_ms('commit_us'):>16.1f}m"
        )
    js = [r.journal_sync_max_ms for r in loaded]
    cs = [r.all_max_ms("commit_us") for r in loaded]
    print()
    emit(
        "device.correlation_all",
        f"Pearson r = {pearson(js, cs):.3f}, Spearman = {spearman(js, cs):.3f}",
        leg="all three legs + calibration",
        points=", ".join(r.label for r in loaded),
        n=len(loaded),
        stat="correlation of journal worst sync_data with FDB worst commit",
    )
    fast = [r for r in loaded if r.regime == "fast"]
    slow = [r for r in loaded if r.regime == "slow"]
    emit(
        "device.correlation_fast_regime",
        f"Pearson r = {pearson([r.journal_sync_max_ms for r in fast], [r.all_max_ms('commit_us') for r in fast]):.3f}",
        leg="all three legs + calibration",
        points=", ".join(r.label for r in fast),
        n=len(fast),
        stat=f"same correlation restricted to runs with journal sync max < {SLOW_REGIME_SYNC_MS:.0f} ms",
    )
    emit(
        "device.correlation_slow_regime",
        f"Pearson r = {pearson([r.journal_sync_max_ms for r in slow], [r.all_max_ms('commit_us') for r in slow]):.3f}",
        leg="cadence + device",
        points=", ".join(r.label for r in slow),
        n=len(slow),
        stat=f"same correlation restricted to runs with journal sync max >= {SLOW_REGIME_SYNC_MS:.0f} ms",
    )
    emit(
        "device.tail_commit_slow_regime",
        Range([(r.tail_mean_ms("commit_us"), r.label) for r in slow],
              f"the {len(slow)} loaded runs in the slow fsync regime"),
        leg="cadence + device",
        points=", ".join(r.label for r in slow),
        n=len(slow),
        stat="mean commit over intents past the 20 ms cut",
    )
    emit(
        "device.tail_commit_fast_regime",
        Range([(r.tail_mean_ms("commit_us"), r.label) for r in fast],
              f"the {len(fast)} loaded runs in the fast fsync regime"),
        leg="all three legs + calibration",
        points=", ".join(r.label for r in fast),
        n=len(fast),
        stat="mean commit over intents past the 20 ms cut",
    )
    emit(
        "device.commit_max_overall",
        Range([(r.all_max_ms("commit_us"), r.label) for r in loaded],
              "all loaded runs"),
        leg="all three legs + calibration",
        points=", ".join(r.label for r in loaded),
        n=len(loaded),
        stat="worst single FDB commit in the run",
    )

    sub("the phased point in each regime (the number the section quotes as the result)")
    for r in sorted(loaded, key=lambda r: r.label):
        if not (r.phased and r.leg == "device"):
            continue
        emit(
            f"device.phased_result.{r.label}",
            f"client p99 {lattice_pct(r.client_hist, .99):.0f} ms, "
            f"server p99 {lattice_pct(r.server_hist, .99):.0f} ms, "
            f"{r.slow_intents} of {r.intents} intents past the cut "
            f"({r.slow_pct:.2f} %), FDB commit max "
            f"{r.all_max_ms('commit_us'):.1f} ms, journal worst fsync "
            f"{r.journal_sync_max_ms:.1f} ms",
            leg="device",
            points=r.label,
            n=r.intents,
            stat=f"lattice-bucket percentiles + counts, {r.regime} fsync regime",
        )


def section_fence(runs, loaded):
    head("6. FENCE — the verdict, re-argued only on numbers printed here")
    print(
        "  Exemplar populations, stated once so every count below has a\n"
        "  denominator: one exemplar per 250 ms report interval per run.\n"
    )
    ex_all = [(e, r) for r in loaded for e in r.exemplars]
    ex_slow = [(e, r) for r in loaded for e in r.slow_exemplars]
    emit(
        "fence.populations",
        f"{len(ex_all)} exemplars over the loaded runs; "
        f"{len(ex_slow)} of them past the 20 ms cut",
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(ex_all),
        stat="count of per-interval exemplars",
    )

    sub("6a. fence against the read it is often compared to")
    emit(
        "fence.vs_idem_ratio",
        Range(
            [(r.stage_all["fence_us_sum"] / r.stage_all["idem_read_us_sum"], r.label)
             for r in loaded],
            "all loaded runs", unit="x",
        ),
        leg="all three legs + calibration",
        points=", ".join(r.label for r in loaded),
        n=len(loaded),
        stat="run-total fence_us_sum / idem_read_us_sum (equal to the ratio of means)",
    )

    print(f"  {'run':<15} {'leg':<11} {'fence/idem':>10}")
    for r in loaded:
        print(f"  {r.label:<15} {r.leg:<11} "
              f"{r.stage_all['fence_us_sum'] / r.stage_all['idem_read_us_sum']:>10.2f}x")

    sub("6b. fence as a share of the server span, over past-cut exemplars")
    print(
        f"{'run':<15} {'leg':<11} {'n_tail_ex':>9} {'mean %':>7} "
        f"{'median %':>9} {'max %':>7}"
    )
    for r in loaded:
        fr = [100 * e["fence_us"] / e["server_us"] for e in r.slow_exemplars]
        if not fr:
            print(f"{r.label:<15} {r.leg:<11} {0:>9}   (no exemplar past the cut)")
            continue
        print(
            f"{r.label:<15} {r.leg:<11} {len(fr):>9} {mean(fr):>7.2f} "
            f"{median(fr):>9.2f} {max(fr):>7.2f}"
        )
    pooled = [100 * e["fence_us"] / e["server_us"] for e, _ in ex_slow]
    over = sum(1 for x in pooled if x > 15)
    print()
    emit(
        "fence.share_over_15pct",
        f"{over} of {len(pooled)} past-cut exemplars have fence > 15 % of the span",
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs, pooled",
        n=len(pooled),
        stat="count over pooled past-cut exemplars",
    )
    maxima = sorted(
        ((max(100 * e["fence_us"] / e["server_us"] for e in r.slow_exemplars), r.label)
         for r in loaded if r.slow_exemplars),
        reverse=True,
    )[:3]
    emit(
        "fence.share_top_run_maxima",
        ", ".join(f"{v:.1f} % ({p})" for v, p in maxima),
        leg="rate + device",
        points=", ".join(p for _, p in maxima),
        n=3,
        stat="the three largest per-run maxima of fence / server span, past-cut exemplars",
    )

    sub("6c. how often fence is the largest FDB phase")
    per_run = {}
    for e, r in ex_slow:
        if dominant_phase(e) == "fence_us":
            per_run[r.label] = per_run.get(r.label, 0) + 1
    n_all = sum(1 for e, _ in ex_all if dominant_phase(e) == "fence_us")
    n_slow = sum(per_run.values())
    emit(
        "fence.largest_overall",
        f"{n_all} of {len(ex_all)} exemplars",
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs, pooled",
        n=len(ex_all),
        stat="exemplars whose largest FDB phase is fence",
    )
    emit(
        "fence.largest_past_cut",
        f"{n_slow} of {len(ex_slow)} past-cut exemplars, in "
        f"{len(per_run)} runs: "
        + ", ".join(f"{k} {v}" for k, v in sorted(per_run.items(), key=lambda kv: -kv[1])),
        leg="rate + device",
        points=", ".join(sorted(per_run)),
        n=len(ex_slow),
        stat="past-cut exemplars whose largest FDB phase is fence",
    )

    sub("6d. the verdict's own evidence: the slowest intent of every run")
    print(
        f"{'run':<15} {'leg':<11} {'server':>8} {'grv':>8} {'idem':>7} "
        f"{'fence':>7} {'commit':>8} {'dominant phase':<16}"
    )
    doms = []
    for r in loaded:
        e = r.slowest_exemplar
        d = dominant_phase(e)
        doms.append((d, e.get(d, 0) / 1000, r.label, e["fence_us"] / 1000))
        print(
            f"{r.label:<15} {r.leg:<11} {e['server_us']/1000:>8.2f} "
            f"{e['grv_us']/1000:>8.2f} {e['idem_read_us']/1000:>7.2f} "
            f"{e['fence_us']/1000:>7.2f} {e['commit_us']/1000:>8.2f} {d:<16}"
        )
    print()
    emit(
        "fence.slowest_intent_fence",
        Range([(f, p) for _, _, p, f in doms], "the slowest intent of each loaded run"),
        leg="all three legs + calibration",
        points=f"{len(doms)} loaded runs",
        n=len(doms),
        stat="fence on the slowest intent of the run",
    )
    gc = [(d, v, p) for d, v, p, _ in doms if d in ("grv_us", "commit_us")]
    other = [(d, v, p) for d, v, p, _ in doms if d not in ("grv_us", "commit_us")]
    emit(
        "fence.slowest_intent_dominant",
        f"grv or commit is the largest phase in {len(gc)} of {len(doms)} runs; "
        + str(Range([(v, p) for _, v, p in gc],
                    f"the {len(gc)} runs where grv or commit dominates")),
        leg="all three legs + calibration",
        points=f"{len(doms)} loaded runs",
        n=len(doms),
        stat="largest phase on the slowest intent of the run, and its size",
    )
    emit(
        "fence.slowest_intent_exceptions",
        "; ".join(f"{p}: {d} = {v:.2f} ms" for d, v, p in other) or "none",
        leg="device",
        points=", ".join(p for _, _, p in other) or "-",
        n=len(other),
        stat="runs whose slowest intent is dominated by neither grv nor commit",
    )

    sub("6e. the single fence read, which is what a fan-out claim is about")
    print(
        "  A fan-out claim is about the max of the 128 concurrent reads, so the\n"
        "  statistic is the worst SINGLE read, not the stage and not a mean.\n"
    )
    print(
        f"{'run':<15} {'leg':<11} {'/s':>7} {'fence stage max':>15} "
        f"{'single read max':>15} {'tail fence mean':>15}"
    )
    for r in sorted(loaded, key=lambda r: r.fence_read_max_ms):
        print(
            f"{r.label:<15} {r.leg:<11} {r.intent_rate:>7.1f} "
            f"{r.all_max_ms('fence_us'):>14.2f}m {r.fence_read_max_ms:>14.2f}m "
            f"{r.tail_mean_ms('fence_us'):>14.2f}m"
        )
    print()
    at200 = [r for r in loaded if r.intent_rate < 300]
    emit(
        "fence.single_read_max_at_operating_point",
        Range([(r.fence_read_max_ms, r.label) for r in at200],
              f"the {len(at200)} loaded runs at ~200 intents/s or below"),
        leg="all three legs + calibration",
        points=", ".join(r.label for r in at200),
        n=len(at200),
        stat="worst single fence read in the run",
    )
    emit(
        "fence.tail_mean",
        Range([(r.tail_mean_ms("fence_us"), r.label) for r in loaded],
              "all loaded runs"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean fence over intents past the 20 ms cut",
    )
    emit(
        "fence.single_read_max",
        Range([(r.fence_read_max_ms, r.label) for r in loaded],
              "all loaded runs"),
        leg="all three legs + calibration",
        points=", ".join(r.label for r in loaded),
        n=len(loaded),
        stat="worst single fence read in the run",
    )
    emit(
        "fence.stage_max",
        Range([(r.all_max_ms("fence_us"), r.label) for r in loaded],
              "all loaded runs"),
        leg="all three legs + calibration",
        points=", ".join(r.label for r in loaded),
        n=len(loaded),
        stat="worst whole-fence-stage time in the run",
    )


def section_client(loaded):
    head("7. CLIENT vs SERVER — where the boundary claim actually stands")
    print(
        "  `client arrival max` is `intent_arrival_max_us` from the run's own\n"
        "  end-of-run line, stamped by IntentQueue::on_ack_at when the ack\n"
        "  arrives. `server max` is the gateway's own worst server span.\n"
        "  The excess is client-side time: receive loop, reply lane, poll.\n"
    )
    print(
        f"{'run':<15} {'leg':<11} {'client arr max':>14} {'server max':>11} "
        f"{'excess':>8} {'>1 ms?':>7}"
    )
    rows = []
    for r in loaded:
        exc = r.client_arrival_max_ms - r.server_max_ms
        rows.append((exc, r))
        print(
            f"{r.label:<15} {r.leg:<11} {r.client_arrival_max_ms:>13.2f}m "
            f"{r.server_max_ms:>10.2f}m {exc:>7.2f}m "
            f"{'YES' if exc > 1.0 else '':>7}"
        )
    bad = sorted([(e, r) for e, r in rows if e > 1.0], reverse=True, key=lambda x: x[0])
    print()
    emit(
        "client.excess_over_1ms",
        f"{len(bad)} of {len(rows)} loaded runs exceed 1 ms: "
        + ", ".join(f"{r.label} {e:.2f} ms" for e, r in bad),
        leg="all three legs + calibration",
        points=", ".join(r.label for _, r in bad),
        n=len(rows),
        stat="client arrival max minus server max, per run",
    )
    emit(
        "client.excess_range",
        Range([(e, r.label) for e, r in rows], "all loaded runs"),
        leg="all three legs + calibration",
        points=f"{len(rows)} loaded runs",
        n=len(rows),
        stat="client arrival max minus server max, per run",
    )


def section_eliminations(loaded):
    head("8. ELIMINATION TABLE — the numbers, before the verdicts")
    print(
        "  Every row is a range over a stated population with a stated n.\n"
        "  A hypothesis with no row here has no number and must not appear in\n"
        "  the doc's table.\n"
    )

    sub("8a. gateway runtime: spawn_wait and fdb_gap")
    emit(
        "elim.spawn_wait_mean",
        Range([(r.all_mean_ms("spawn_wait_us"), r.label) for r in loaded],
              "all loaded runs", fmt=".4f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean spawn_wait over every intent",
    )
    emit(
        "elim.spawn_wait_max",
        Range([(r.all_max_ms("spawn_wait_us"), r.label) for r in loaded],
              "all loaded runs", fmt=".3f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="worst single spawn_wait in the run",
    )
    emit(
        "elim.fdb_gap_mean",
        Range([(r.all_mean_ms("fdb_gap_us"), r.label) for r in loaded],
              "all loaded runs", fmt=".4f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean fdb_gap over executed intents",
    )
    emit(
        "elim.fdb_gap_max",
        Range([(r.all_max_ms("fdb_gap_us"), r.label) for r in loaded],
              "all loaded runs", fmt=".3f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="worst single fdb_gap in the run",
    )
    emit(
        "elim.fdb_gap_tail_mean",
        Range([(r.tail_mean_ms("fdb_gap_us"), r.label) for r in loaded],
              "all loaded runs", fmt=".4f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean fdb_gap over intents past the 20 ms cut",
    )

    sub("8b. silent retries")
    tot_a = sum(r.stage_all.get("attempts", 0) for r in loaded)
    tot_e = sum(r.stage_all.get("executed", 0) for r in loaded)
    emit(
        "elim.retries",
        f"attempts {tot_a} - executed {tot_e} = {tot_a - tot_e}",
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=tot_e,
        stat="summed db.run attempts beyond the first, over executed intents",
    )
    emit(
        "elim.backoff_max",
        Range([(r.all_max_ms("backoff_us"), r.label) for r in loaded],
              "all loaded runs", fmt=".3f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="worst backoff in the run (ms-resolution hook; see the fdb_gap caveat)",
    )

    sub("8c. the PersistId allocator")
    print(
        f"{'run':<15} {'refills':>7} {'wait mean us':>12} {'wait max':>9} "
        f"{'refill max':>11}"
    )
    for r in loaded:
        print(
            f"{r.label:<15} {r.stage_all.get('alloc_refills', 0):>7} "
            f"{r.stage_all.get('alloc_wait_us_sum', 0) / r.executed:>12.1f} "
            f"{r.all_max_ms('alloc_wait_us'):>8.2f}m {r.all_max_ms('alloc_refill_us'):>10.2f}m"
        )
    print()
    emit(
        "elim.alloc_refills",
        Range([(float(r.stage_all.get("alloc_refills", 0)), r.label) for r in loaded],
              "all loaded runs", unit="refills per 30 s run", fmt=".0f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="count of allocator refill transactions",
    )
    emit(
        "elim.alloc_wait_mean",
        Range([(r.stage_all.get("alloc_wait_us_sum", 0) / r.executed, r.label)
               for r in loaded], "all loaded runs", unit="us", fmt=".1f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean allocator mutex wait over executed intents",
    )
    emit(
        "elim.alloc_wait_max",
        Range([(r.all_max_ms("alloc_wait_us"), r.label) for r in loaded],
              "all loaded runs"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="worst single allocator mutex wait in the run",
    )
    emit(
        "elim.alloc_refill_max",
        Range([(r.all_max_ms("alloc_refill_us"), r.label) for r in loaded],
              "all loaded runs"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="worst single allocator refill transaction in the run",
    )
    alloc_dom = []
    for r in loaded:
        for e in r.exemplars:
            if dominant_phase(e) in ("alloc_wait_us", "alloc_refill_us"):
                alloc_dom.append((r.label, e))
    emit(
        "elim.alloc_dominant_exemplars",
        f"{len(alloc_dom)} exemplars of "
        f"{sum(len(r.exemplars) for r in loaded)} have an allocator phase as "
        f"their largest: "
        + ", ".join(
            f"{lab} ({dominant_phase(e)} {e[dominant_phase(e)]/1000:.2f} ms, "
            f"span {e['server_us']/1000:.2f} ms)"
            for lab, e in sorted(alloc_dom, key=lambda x: -x[1]["server_us"])[:6]
        ),
        leg="all three legs + calibration",
        points=", ".join(sorted({lab for lab, _ in alloc_dom})),
        n=sum(len(r.exemplars) for r in loaded),
        stat="exemplars whose largest FDB phase is an allocator phase",
    )

    sub("8d. gateway-side stages outside execute")
    emit(
        "elim.ingress_mean",
        Range([(r.stage_all.get("ingress_us_sum", 0) / max(r.intents, 1), r.label)
               for r in loaded], "all loaded runs", unit="us", fmt=".1f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean ingress queue time over definitive replies",
    )
    emit(
        "elim.ingress_max",
        Range([(r.all_max_ms("ingress_us"), r.label) for r in loaded],
              "all loaded runs", fmt=".3f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="worst single ingress queue wait in the run — a run maximum, so it "
             "bounds the stalled intervals too",
    )
    emit(
        "elim.reply_mean",
        Range([(r.stage_all.get("reply_us_sum", 0) / max(r.intents, 1), r.label)
               for r in loaded], "all loaded runs", unit="us", fmt=".2f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean reply handoff over definitive replies",
    )
    emit(
        "elim.server_gap_mean",
        Range([(r.stage_all.get("server_gap_us_sum", 0) / max(r.intents, 1), r.label)
               for r in loaded], "all loaded runs", unit="us", fmt=".2f"),
        leg="all three legs + calibration",
        points=f"{len(loaded)} loaded runs",
        n=len(loaded),
        stat="mean unattributed server-span time over definitive replies",
    )


def section_quiet(runs):
    head("9. THE QUIET LEG — reported because it failed, not because it worked")
    quiet = [r for r in runs if not r.journal_loaded]
    if not quiet:
        print("  no quiet runs in this sweep")
        return
    print(
        "  Intended as the device control: hold the intent rate, drop bulk to\n"
        "  diff_hz 0.05 so the journal is nearly idle. It made FoundationDB\n"
        "  worse, which is an anomaly and not a device measurement.\n"
    )
    print(
        f"{'run':<15} {'phase':<7} {'n':>6} {'/s':>7} {'slow%':>7} "
        f"{'cli p50':>8} {'grv all':>8} {'grv tail':>9}"
    )
    for r in quiet:
        print(
            f"{r.label:<15} {'phased' if r.phased else 'burst':<7} {r.intents:>6} "
            f"{r.intent_rate:>7.1f} {r.slow_pct:>7.2f} "
            f"{lattice_pct(r.client_hist, .5):>8.1f} "
            f"{r.all_mean_ms('grv_us'):>8.1f} {r.tail_mean_ms('grv_us'):>9.1f}"
        )
    print()
    emit(
        "quiet.slow_pct",
        Range([(r.slow_pct, r.label) for r in quiet],
              "all 4 quiet runs", unit="%", fmt=".2f"),
        leg="device",
        points=", ".join(r.label for r in quiet),
        n=len(quiet),
        stat="share of intents past the 20 ms cut",
    )
    emit(
        "quiet.grv_mean",
        Range([(r.all_mean_ms("grv_us"), r.label) for r in quiet],
              "all 4 quiet runs"),
        leg="device",
        points=", ".join(r.label for r in quiet),
        n=len(quiet),
        stat="mean grv over every executed intent",
    )
    emit(
        "quiet.client_p50",
        Range([(lattice_pct(r.client_hist, .5), r.label) for r in quiet],
              "all 4 quiet runs", fmt=".0f"),
        leg="device",
        points=", ".join(r.label for r in quiet),
        n=len(quiet),
        stat="client intent_commit_ms p50, D16 lattice bucket",
    )
    loadedctl = [r for r in runs if r.leg == "device" and r.journal_loaded]
    emit(
        "quiet.loaded_control",
        Range([(r.all_mean_ms("server_us"), r.label) for r in loadedctl],
              "the 4 loaded device-leg controls"),
        leg="device",
        points=", ".join(r.label for r in loadedctl),
        n=len(loadedctl),
        stat="mean server span over every intent, loaded control beside the quiet runs",
    )


# --------------------------------------------------------------------------
# Self-test: the values the 2026-08-19 re-review established by hand.
# --------------------------------------------------------------------------
def self_test(sweep: Path) -> int:
    runs = load_all(sweep)
    loaded = [r for r in runs if r.journal_loaded]
    by = {r.label: r for r in runs}
    fails = []

    def check(name, got, want, tol):
        ok = abs(got - want) <= tol
        print(f"  {'ok  ' if ok else 'FAIL'} {name:<44} got {got!r:>22}  want {want!r}")
        if not ok:
            fails.append(name)

    def check_eq(name, got, want):
        ok = got == want
        print(f"  {'ok  ' if ok else 'FAIL'} {name:<44} got {got!r:>22}  want {want!r}")
        if not ok:
            fails.append(name)

    print("acceptance test — the true values the re-review derived by hand")
    print()

    # 1. fence mean / idem_read mean, per run
    ratios = [(r.stage_all["fence_us_sum"] / r.stage_all["idem_read_us_sum"], r.label)
              for r in loaded]
    check("fence/idem min", min(ratios)[0], 5.78, 0.01)
    check("fence/idem max", max(ratios)[0], 15.59, 0.01)
    check("fence/idem i1000-r1",
          by["i1000-r1"].stage_all["fence_us_sum"] / by["i1000-r1"].stage_all["idem_read_us_sum"],
          5.78, 0.01)
    check("fence/idem i1000-r2",
          by["i1000-r2"].stage_all["fence_us_sum"] / by["i1000-r2"].stage_all["idem_read_us_sum"],
          6.97, 0.01)

    # 2. fence as a share of the server span
    f1 = [100 * e["fence_us"] / e["server_us"] for e in by["i1000-r1"].slow_exemplars]
    f2 = [100 * e["fence_us"] / e["server_us"] for e in by["i1000-r2"].slow_exemplars]
    check("fence share i1000-r1 mean %", mean(f1), 24.2, 0.05)
    check("fence share i1000-r2 median %", median(f2), 26.0, 0.05)
    pooled = [100 * e["fence_us"] / e["server_us"]
              for r in loaded for e in r.slow_exemplars]
    check_eq("past-cut exemplar population", len(pooled), 550)
    check_eq("past-cut exemplars over 15 %", sum(1 for x in pooled if x > 15), 169)
    maxima = sorted((max(100 * e["fence_us"] / e["server_us"] for e in r.slow_exemplars)
                     for r in loaded if r.slow_exemplars), reverse=True)[:3]
    check("fence share top run maximum", maxima[0], 55.2, 0.05)
    check("fence share 2nd run maximum", maxima[1], 51.2, 0.05)
    check("fence share 3rd run maximum", maxima[2], 47.3, 0.05)

    # 3. fence as the largest phase
    ex_all = [e for r in loaded for e in r.exemplars]
    ex_slow = [(e, r) for r in loaded for e in r.slow_exemplars]
    check_eq("exemplar population", len(ex_all), 2479)
    check_eq("fence largest, overall",
             sum(1 for e in ex_all if dominant_phase(e) == "fence_us"), 661)
    per = {}
    for e, r in ex_slow:
        if dominant_phase(e) == "fence_us":
            per[r.label] = per.get(r.label, 0) + 1
    check_eq("fence largest, past cut", sum(per.values()), 43)
    check_eq("fence largest, past cut, by run",
             dict(sorted(per.items())),
             {"i1000-r1": 18, "i1000-r2": 24, "qph-loaded-r2": 1})

    # 4. GRV in the phased runs
    check("tail grv hbph-r1", by["hbph-r1"].tail_mean_ms("grv_us"), 0.75, 0.01)
    check("tail grv hbph-r2", by["hbph-r2"].tail_mean_ms("grv_us"), 0.51, 0.01)
    check("tail grv qph-loaded-r1", by["qph-loaded-r1"].tail_mean_ms("grv_us"), 0.18, 0.01)
    check("tail grv qph-loaded-r2", by["qph-loaded-r2"].tail_mean_ms("grv_us"), 6.91, 0.01)

    # 5. the slow-regime tail commit range, and the run in neither range
    slow = [r for r in loaded if r.regime == "slow"]
    check_eq("slow-regime population", len(slow), 6)
    check("slow-regime tail commit min", min(r.tail_mean_ms("commit_us") for r in slow), 33.2, 0.05)
    check("slow-regime tail commit max", max(r.tail_mean_ms("commit_us") for r in slow), 86.5, 0.05)
    check("q-loaded-r2 tail commit", by["q-loaded-r2"].tail_mean_ms("commit_us"), 24.5, 0.05)
    check_eq("q-loaded-r2 regime", by["q-loaded-r2"].regime, "fast")

    # 6. client arrival max vs server max
    exc = {r.label: r.client_arrival_max_ms - r.server_max_ms for r in loaded}
    over = {k: round(v, 2) for k, v in exc.items() if v > 1.0}
    check_eq("runs with client excess over 1 ms", len(over), 4)
    check("hb3-r1 client excess", exc["hb3-r1"], 11.17, 0.02)
    check("i500-r1 client excess", exc["i500-r1"], 6.82, 0.02)
    check("i50-r1 client excess", exc["i50-r1"], 2.83, 0.02)
    check("q-loaded-r2 client excess", exc["q-loaded-r2"], 1.05, 0.02)

    # 7. the allocator against a 10 ms budget
    check("qph-loaded-r1 alloc_refill max", by["qph-loaded-r1"].all_max_ms("alloc_refill_us"),
          9.82, 0.01)
    check("qph-loaded-r1 alloc_wait max", by["qph-loaded-r1"].all_max_ms("alloc_wait_us"),
          9.81, 0.01)

    # 8. the fence verdict's own numbers
    doms = [(dominant_phase(r.slowest_exemplar), r.slowest_exemplar, r.label)
            for r in loaded]
    fences = [e["fence_us"] / 1000 for _, e, _ in doms]
    check("slowest-intent fence min", min(fences), 1.5, 0.05)
    check("slowest-intent fence max", max(fences), 18.7, 0.05)
    gc = [(d, e, p) for d, e, p in doms if d in ("grv_us", "commit_us")]
    check_eq("slowest intent dominated by grv or commit", len(gc), 20)
    check_eq("loaded population", len(loaded), 21)
    check("grv-or-commit dominant min", min(e[d] / 1000 for d, e, _ in gc), 93.7, 0.05)
    check("grv-or-commit dominant max", max(e[d] / 1000 for d, e, _ in gc), 345.8, 0.05)

    # 9. the surviving claims
    js = [r.journal_sync_max_ms for r in loaded]
    cs = [r.all_max_ms("commit_us") for r in loaded]
    check("fsync Pearson r, all loaded", pearson(js, cs), 0.888, 0.001)
    fast = [r for r in loaded if r.regime == "fast"]
    check_eq("fast-regime population", len(fast), 15)
    check("fsync Pearson r, fast regime",
          pearson([r.journal_sync_max_ms for r in fast],
                  [r.all_max_ms("commit_us") for r in fast]), 0.47, 0.005)
    check("grv seconds hb3-r1", by["hb3-r1"].grv_seconds, 18.31, 0.01)
    check("grv seconds hb3-r2", by["hb3-r2"].grv_seconds, 18.69, 0.01)
    check("grv seconds hbph-r1", by["hbph-r1"].grv_seconds, 1.51, 0.01)
    check("grv seconds hbph-r2", by["hbph-r2"].grv_seconds, 1.65, 0.01)

    # 10. the headline exemplar's arithmetic
    e = by["cal-i200-r0"].slowest_exemplar
    phase_sum = sum(e.get(k, 0) for k in EXEC_PHASES)
    check_eq("headline exec fully attributed",
             phase_sum + e["fdb_gap_us"], e["exec_us"])
    named = sum(e.get(k, 0) for k in NAMED_SERVER_STAGES) + phase_sum
    check_eq("headline named stage sum", named, 157_388)
    check_eq("headline server span", e["server_us"], 157_413)

    # 11. per-burst constancy: one stall, at whatever cadence it is run
    pop = [r for r in loaded
           if not r.phased and 150 <= r.intent_rate <= 300]
    check_eq("per-burst population", len(pop), 11)
    check("grv per pass, min", min(r.grv_seconds / r.bursts for r in pop), 1.52, 0.01)
    check("grv per pass, max", max(r.grv_seconds / r.bursts for r in pop), 2.04, 0.01)
    slowr = [r for r in pop if r.regime == "slow"]
    check_eq("per-burst slow-regime subset", len(slowr), 3)
    check("grv per pass, slow regime min",
          min(r.grv_seconds / r.bursts for r in slowr), 1.62, 0.01)
    check("grv per pass, slow regime max",
          max(r.grv_seconds / r.bursts for r in slowr), 1.87, 0.01)

    # 12. the fan-out row's own statistic, which the published section had
    #     wrong in the other direction: it quoted "<= 19 ms in every loaded
    #     point". `fence_read_max_us` has no `_max` suffix and was being
    #     summed rather than maxed by an earlier reader.
    check("worst single fence read, all loaded",
          max(r.fence_read_max_ms for r in loaded), 81.37, 0.01)
    at200 = [r for r in loaded if r.intent_rate < 300]
    check_eq("runs at <= 300 intents/s", len(at200), 17)
    check("worst single fence read at <= 300/s",
          max(r.fence_read_max_ms for r in at200), 41.18, 0.01)
    check("tail fence mean min", min(r.tail_mean_ms("fence_us") for r in loaded), 1.90, 0.01)
    check("tail fence mean max", max(r.tail_mean_ms("fence_us") for r in loaded), 17.42, 0.01)

    print()
    if fails:
        print(f"SELF-TEST FAILED: {len(fails)} check(s): {fails}")
        return 1
    print("SELF-TEST PASSED: every value the doc quotes is re-derived from the raw artifacts.")
    return 0


# --------------------------------------------------------------------------
# The rule, enforced rather than promised: no number in §2.2.1 that this
# script does not print.
# --------------------------------------------------------------------------
DOC = Path(__file__).resolve().parent.parent / "docs" / "08-persistence.md"
SECTION_START = "### 2.2.1 Where the D16 intent tail"
SECTION_END = "## 3. Cell actor model"

# Numbers that are structure rather than measurement: section numbers, FDB
# error codes, configured constants, dates, and the round figures the rig was
# built from. Each is a deliberate entry; the list is short on purpose, because
# a long allow-list is how this check stops working.
AUDIT_ALLOW = {
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13",
    "14", "15", "16", "17", "18", "19", "20", "21", "24", "25", "30", "40",
    "64", "100", "128", "150", "200", "250", "300", "500", "1000", "5000",
    "10000", "2026",
    "0.05", "1.5", "3.0", "6.0",              # configured cadences / diff_hz
    "0.752", "0.888",                          # printed as "Spearman = 0.752"
    "1007", "1009", "1021", "1037", "1213",    # FDB error codes
    "2.1", "2.1.3", "2.2", "2.2.1", "4.3", "08", "08-persistence",
    "0.11.0", "0.11",                          # foundationdb crate version
}

# Numbers the section quotes *as wrong*, in the sentences that say so. They are
# not derivable by construction: the point of printing them is that they are
# not what the artifacts say.
AUDIT_QUOTED_FALSE = {"19", "17.60", "157.39", "45.9", "116.6", "123.5", "147.7"}


def _audit_norm(text: str) -> str:
    text = text.replace("\u2009", " ").replace("–", "-").replace("—", "-")
    # Digits inside an identifier (`qph-loaded-r2`, `p99`) are not numbers.
    # Mask them, or joining thousands separators invents "…-r2 207.3" -> 2207.3.
    text = re.sub(r"(?<=[A-Za-z])\d+", lambda m: "\x00" * len(m.group()), text)
    prev = None
    while prev != text:
        prev = text
        text = re.sub(r"(?<=\d) (\d{3})(?!\d)", r"\1", text)
    return text


def audit_doc(sweep: Path) -> int:
    if not DOC.exists():
        print(f"doc not found: {DOC}", file=sys.stderr)
        return 2
    import io
    import contextlib

    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        runs = load_all(sweep)
        loaded = [r for r in runs if r.journal_loaded]
        section_catalog(runs)
        section_headline(runs)
        section_rate_leg(runs)
        section_cadence_leg(runs)
        section_burst_constancy(runs)
        section_periodicity(runs)
        section_device(loaded)
        section_fence(runs, loaded)
        section_client(loaded)
        section_eliminations(loaded)
        section_quiet(runs)
    printed = _audit_norm(buf.getvalue())

    body = DOC.read_text()
    section = body[body.index(SECTION_START):body.index(SECTION_END)]
    text = _audit_norm(section)
    tokens = sorted(set(re.findall(r"\d+(?:\.\d+)?", text)))
    missing = [t for t in tokens
               if t not in AUDIT_ALLOW and t not in AUDIT_QUOTED_FALSE
               and t not in printed]

    print(f"audit: {DOC}")
    print(f"  {len(tokens)} numeric tokens in §2.2.1")
    print(f"  {len(AUDIT_ALLOW & set(tokens))} structural (allow-list)")
    print(f"  {len(AUDIT_QUOTED_FALSE & set(tokens))} quoted as wrong, by design")
    print(f"  {len(missing)} not produced by this script")
    for t in missing:
        line = next((l.strip() for l in text.splitlines() if t in l), "")
        print(f"    UNDERIVED {t!r}  <-  {line[:100]}")
    if missing:
        print("\nAUDIT FAILED: §2.2.1 quotes a number this script does not print.")
        return 1
    print("\nAUDIT PASSED: every number in §2.2.1 is printed by this script.")
    return 0


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    sweep = Path(args[0]) if args else DEFAULT_SWEEP
    if not sweep.exists():
        print(f"sweep directory not found: {sweep}", file=sys.stderr)
        return 2
    if "--self-test" in argv:
        return self_test(sweep)
    if "--audit-doc" in argv:
        return audit_doc(sweep)

    runs = load_all(sweep)
    print(f"intent-tail-derive: {len(runs)} runs from {sweep}")
    print(f"slow cut {CUT_US} us (stages.rs DEFAULT_SLOW_THRESHOLD_US); "
          f"report interval {INTERVAL_MS} ms")
    loaded, _quiet = section_catalog(runs)
    section_headline(runs)
    section_rate_leg(runs)
    section_cadence_leg(runs)
    section_burst_constancy(runs)
    section_periodicity(runs)
    section_device(loaded)
    section_fence(runs, loaded)
    section_client(loaded)
    section_eliminations(loaded)
    section_quiet(runs)
    print()
    print("=" * 78)
    print("Nothing above is rounded on the way into the doc beyond the digits shown.")
    print("A number not printed here does not go in §2.2.1.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
