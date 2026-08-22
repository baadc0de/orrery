#!/usr/bin/env python3
"""Report D32 clause (e)'s promotion evidence for every enforcement control.

    python3 scripts/ramp-report.py
    python3 scripts/ramp-report.py --self-test

The predicate this renders is the ADR's, verbatim:

    promote(C) ⟺ production leg ∧ sensitivity leg ∧ review gate
                 ∧ (C = C3 ⟹ auditor live)

    production leg ⟺ W ≥ 30 days ∧ fp_count(H, C, W) = 0
                     ∧ coverage(H, C, W) ≥ 0.999 ∧ |H| ≥ 100

**Every figure here is read, never re-derived.** `fp_count`, `coverage`, its
numerator and its denominator, `|H|` and `W` are all computed by
`orrery_persistd::intent::ramp::RampMeter::snapshot`, from counters incremented
at two points in the admission path. This script compares them against the
floors above and prints them. That division of labour is `AGENTS.md`'s rule for
`gate-status.sh` — "a figure this script computed itself would be a second
implementation of the gate, and the two would disagree exactly when it
mattered" — and it is why `validate()` below checks the artifact's numbers
against *each other* and never against a recomputation of one of them.

**The denominator is the point.** D32: "a false-positive rate of 0 over a
cohort nobody watched is not evidence, it is blindness with a clean
conscience." So `fp_count` is never printed without the population it was
counted over, and a coverage whose denominator is zero prints as absent rather
than as a number — `0` would-have-acted out of `10 000` observed and `0` out of
`0` are opposite findings, and a report that renders them alike is the failure
the whole clause exists to prevent.
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
        "RAMP_REPORT_DATA",
        ROOT / "docs/data/ramp-shadow-2026-08-22.json",
    )
)

# The schema `RampArtifact` writes. A reader that guesses at a shape it was not
# written for reports numbers that are wrong rather than absent.
SCHEMA = "orrery.ramp.report/1"

# D32 clause (e)'s production leg. Every one of these is a dial the record
# states without deriving, and each is named here so lowering one is a visible
# edit rather than a drifting default.
WINDOW_FLOOR_DAYS = 30.0
COVERAGE_FLOOR = 0.999
COHORT_FLOOR = 100
FP_CEILING = 0

# D32 clause (f)'s auto-suspend spread term, for the same artifact's counters.
# The rate half needs a trailing 7-day hourly median the artifact does not
# carry, so only the spread is evaluated and the report says so.
SUSPEND_SPREAD = 8

# Clause (c)'s inventory. All five must be accounted for, measured or absent:
# a control missing from the artifact entirely would read as a control with
# nothing to report.
CONTROLS = (
    "attestation_quorum",
    "quarantine_validation",
    "write_annulment",
    "authority_correction",
    "strikes",
)


def load() -> dict:
    return json.loads(DATA.read_text())


# ── validation ───────────────────────────────────────────────────────────────
#
# Guarded facts about the artifact, each of which a mutation in `self_test`
# breaks on purpose. Nothing here recomputes a gate figure; the arithmetic
# clauses hold the artifact's own counters against each other, which is what a
# reader owes an artifact it did not produce.


def validate(artifact: dict) -> list[str]:
    failures: list[str] = []

    def check(name: str, condition: bool, detail: str) -> None:
        if not condition:
            failures.append(f"{name}: {detail}")

    check(
        "schema",
        artifact.get("schema") == SCHEMA,
        f"expected {SCHEMA!r}, found {artifact.get('schema')!r}",
    )

    provenance = artifact.get("provenance") or {}
    check(
        "provenance",
        provenance.get("traffic") in {"harness", "production"},
        "every artifact must say what traffic produced it; clause (e)'s production "
        f"leg is a claim about a fleet, and this one says {provenance.get('traffic')!r}",
    )
    check(
        "provenance source",
        bool(provenance.get("source")),
        "an artifact with no stated source cannot be cited in a promotion note",
    )

    controls = artifact.get("controls") or []
    absent = artifact.get("absent") or []
    named = {control["control"] for control in controls} | {
        control["control"] for control in absent
    }
    check(
        "control inventory",
        named == set(CONTROLS),
        f"D32 clause (c) inventories five controls; this artifact names {sorted(named)}",
    )
    for entry in absent:
        check(
            "absent reason",
            bool(entry.get("reason")),
            f"{entry.get('control')} is absent with no stated reason, which reads "
            "as a control with nothing to report",
        )

    for control in controls:
        name = control.get("control", "?")
        cohort = control.get("cohort") or {}

        # The denominator clause. Not "coverage is present" — a rate can be
        # present and unsupported. What must be present is the pair it was
        # computed from, because that is what a reader checks a rate against.
        check(
            f"{name} denominator",
            "qualifying" in cohort and "observed" in cohort,
            "clause (e)'s coverage is a ratio of two counts and the artifact must "
            "carry both; a rate without its denominator is not evidence",
        )

        # The 0-of-0 clause, in the artifact rather than in the rendering. An
        # empty denominator has no rate, and writing one anyway — 0.0 or 1.0,
        # it does not matter which — invents a measurement.
        if cohort.get("qualifying") == 0:
            check(
                f"{name} empty denominator",
                cohort.get("coverage") is None,
                "no qualifying cohort activity was observed, so coverage is undefined; "
                f"the artifact claims {cohort.get('coverage')!r}",
            )
        else:
            check(
                f"{name} coverage present",
                cohort.get("coverage") is not None,
                "a nonempty denominator must carry the rate computed from it",
            )

        check(
            f"{name} cohort arithmetic",
            cohort.get("observed", 0) + cohort.get("unevaluated", 0)
            <= cohort.get("qualifying", 0),
            "observed and unevaluated activity are both subsets of qualifying "
            "activity; more of them than of it means the two counting points "
            f"disagree ({cohort.get('observed')} + {cohort.get('unevaluated')} "
            f"> {cohort.get('qualifying')})",
        )
        check(
            f"{name} fp bound",
            cohort.get("fp_count", 0) <= cohort.get("observed", 0),
            "a false positive is an observation, so fp_count cannot exceed the "
            "number of observations it was drawn from",
        )
        check(
            f"{name} cohort size",
            cohort.get("active", 0) <= cohort.get("size", 0),
            "more active cohort members than cohort members",
        )
        check(
            f"{name} cohort halves",
            cohort.get("size", 0) <= cohort.get("armed", 0) + cohort.get("natural", 0),
            "|H| exceeds the sum of its two halves, which the union cannot",
        )
        check(
            f"{name} cause split",
            sum((cohort.get("by_cause") or {}).values()) == cohort.get("fp_count", 0),
            "the per-cause split must account for every false positive, or the "
            "shadow report and the rejection log join on a subset",
        )

        check(
            f"{name} fleet arithmetic",
            control.get("observed", 0) + control.get("unevaluated", 0)
            <= control.get("qualifying", 0),
            "the same subset relation, fleet-wide",
        )
        check(
            f"{name} verdict split",
            sum((control.get("by_verdict") or {}).values())
            == control.get("observed", 0) + control.get("unevaluated", 0),
            "every recorded verdict must appear in the outcome split; a split "
            "that is short is a split with an unnamed exit",
        )
        check(
            f"{name} spread bound",
            control.get("accounts_would_act", 0) <= control.get("accounts_observed", 0)
            <= control.get("accounts_qualifying", 0),
            "account cardinality must nest the way the event counts do",
        )
        check(
            f"{name} truncation",
            control.get("accounts_truncated", 0) == 0,
            f"{control.get('accounts_truncated')} accounts were folded into the "
            "meter's overflow bucket, so account spread and the cohort denominator "
            "are both understated and neither can be cited",
        )

    return failures


# ── the predicate ────────────────────────────────────────────────────────────


def production_leg(artifact: dict, control: dict) -> list[tuple[str, str, bool]]:
    """Clause (e)'s four production-leg terms, each rendered with its evidence."""
    cohort = control["cohort"]
    traffic = (artifact.get("provenance") or {}).get("traffic")
    coverage = cohort.get("coverage")

    terms: list[tuple[str, str, bool]] = [
        (
            "traffic is production",
            f"{traffic}",
            traffic == "production",
        ),
        (
            f"W ≥ {WINDOW_FLOOR_DAYS:.0f} days",
            f"{control['window_days']:.4f} days",
            control["window_days"] >= WINDOW_FLOOR_DAYS,
        ),
        (
            f"fp_count(H, C, W) = {FP_CEILING}",
            f"{cohort['fp_count']} of {cohort['observed']} observed",
            cohort["fp_count"] <= FP_CEILING,
        ),
        (
            f"coverage(H, C, W) ≥ {COVERAGE_FLOOR}",
            rate(coverage, cohort["observed"], cohort["qualifying"]),
            coverage is not None and coverage >= COVERAGE_FLOOR,
        ),
        (
            f"|H| ≥ {COHORT_FLOOR}",
            f"{cohort['size']} ({cohort['armed']} armed, {cohort['natural']} natural, "
            f"{cohort['active']} active)",
            cohort["size"] >= COHORT_FLOOR,
        ),
    ]
    return terms


def rate(value: float | None, numerator: int, denominator: int) -> str:
    """A rate, always with the pair it came from — and absent when it has none.

    The guarded stage of this whole file. `0 / 0` has no rate: printing one
    would make a control nobody ran indistinguishable from a control that ran
    clean over ten thousand intents, which is exactly D32's "blindness with a
    clean conscience". So the numerator and denominator are unconditional and
    the rate itself is conditional, never the other way round.
    """
    pair = f"({numerator} / {denominator})"
    if value is None or denominator == 0:
        return f"undefined {pair} — nothing was observed, so there is no rate"
    return f"{value:.6f} {pair}"


# ── rendering ────────────────────────────────────────────────────────────────


def render(artifact: dict) -> list[str]:
    lines: list[str] = []
    provenance = artifact["provenance"]
    lines.append("D32 clause (e) — enforcement ramp promotion evidence")
    lines.append(f"  traffic     {provenance['traffic']}")
    lines.append(f"  source      {provenance['source']}")
    if provenance.get("note"):
        lines.append(f"  note        {provenance['note']}")
    lines.append("")

    for control in artifact["controls"]:
        cohort = control["cohort"]
        lines.append(f"  {control['control']}")
        lines.append(
            f"    window            {control['window_days']:.4f} days "
            f"({control['observed_from_ms']} … {control['observed_to_ms']} ms)"
        )
        lines.append(
            f"    admission decisions {control['qualifying']:>8,}   "
            f"across {control['accounts_qualifying']:,} accounts"
        )
        lines.append(
            f"    observed            {control['observed']:>8,}   "
            f"across {control['accounts_observed']:,} accounts"
        )
        lines.append(
            f"    unevaluated         {control['unevaluated']:>8,}   "
            "(recorded, never a would-have-acted event)"
        )
        lines.append(
            f"    would have acted    {control['would_act']:>8,}   "
            f"across {control['accounts_would_act']:,} accounts"
        )
        if control["unattributed"]["qualifying"]:
            lines.append(
                f"    unattributed        {control['unattributed']['qualifying']:>8,}   "
                "(no session; outside H by construction)"
            )
        lines.append("")
        lines.append("    outcome split")
        for label, count in sorted(control["by_verdict"].items()):
            lines.append(f"      {label:<32} {count:>8,}")
        lines.append("")
        lines.append("    over the known-honest cohort H")
        lines.append(
            f"      fp_count          {cohort['fp_count']:>8,}   "
            f"of {cohort['observed']:,} observed, across "
            f"{cohort['accounts_would_act']:,} accounts"
        )
        lines.append(
            f"      coverage          {rate(cohort['coverage'], cohort['observed'], cohort['qualifying'])}"
        )
        lines.append(
            f"      |H|               {cohort['size']:>8,}   "
            f"{cohort['armed']:,} armed, {cohort['natural']:,} natural, "
            f"{cohort['active']:,} active"
        )
        for label, count in sorted((cohort["by_cause"] or {}).items()):
            lines.append(f"      cause {label:<26} {count:>8,}")
        lines.append("")

        lines.append("    production leg")
        met = True
        for term, evidence, holds in production_leg(artifact, control):
            met = met and holds
            lines.append(f"      [{'x' if holds else ' '}] {term:<28} {evidence}")
        lines.append(
            f"      production leg: {'MET' if met else 'NOT MET'}"
            + ("" if met else " — this control may not be promoted")
        )
        lines.append("")
        lines.append(
            "      Not evaluated here, and not implied by the above: the "
            "sensitivity leg (#222's"
        )
        lines.append(
            "      gate leg), the pre-live review gate, and — for write_annulment "
            "— clause (g)'s"
        )
        lines.append("      auditor liveness. The production leg is one conjunct of four.")
        lines.append("")

        spread = control["accounts_would_act"]
        lines.append("    clause (f) auto-suspend, spread term only")
        lines.append(
            f"      spread ≥ {SUSPEND_SPREAD}          {spread} distinct accounts "
            f"— {'over' if spread >= SUSPEND_SPREAD else 'under'} the bound"
        )
        lines.append(
            "      The rate term needs a trailing 7-day hourly median this artifact "
            "does not carry,"
        )
        lines.append(
            "      so no trip is evaluated. Spread is a conjunct, not the trigger."
        )
        lines.append("")

    lines.append("  controls with nothing measured")
    for entry in artifact["absent"]:
        lines.append(f"    {entry['control']}")
        lines.append(f"      {entry['reason']}")
    return lines


def report(artifact: dict) -> None:
    for line in render(artifact):
        print(line)


# ── self-test ────────────────────────────────────────────────────────────────


def synthetic(qualifying: int, observed: int, fp_count: int) -> dict:
    """An artifact with one control and known numbers, for the pair of cases
    clause (e)'s coverage term exists to keep apart."""
    coverage = None if qualifying == 0 else observed / qualifying
    return {
        "schema": SCHEMA,
        "provenance": {
            "traffic": "harness",
            "source": "ramp-report.py --self-test",
            "note": "",
        },
        "controls": [
            {
                "control": "attestation_quorum",
                "observed_from_ms": 0,
                "observed_to_ms": 0,
                "window_days": 0.0,
                "qualifying": qualifying,
                "observed": observed,
                "unevaluated": 0,
                "would_act": fp_count,
                "accounts_qualifying": 1 if qualifying else 0,
                "accounts_observed": 1 if observed else 0,
                "accounts_would_act": 1 if fp_count else 0,
                "accounts_truncated": 0,
                "unattributed": {"qualifying": 0, "observed": 0, "would_act": 0},
                "by_verdict": {"would_admit": observed} if observed else {},
                "by_cause": {"threshold_not_met": fp_count} if fp_count else {},
                "cohort": {
                    "armed": 1,
                    "natural": 0,
                    "size": 1,
                    "active": 1 if qualifying else 0,
                    "qualifying": qualifying,
                    "observed": observed,
                    "unevaluated": 0,
                    "coverage": coverage,
                    "fp_count": fp_count,
                    "accounts_would_act": 1 if fp_count else 0,
                    "by_cause": {"threshold_not_met": fp_count} if fp_count else {},
                },
            }
        ],
        "absent": [
            {"control": name, "reason": "synthetic"}
            for name in CONTROLS
            if name != "attestation_quorum"
        ],
    }


def zero_of_zero_is_distinguishable() -> list[str]:
    """The functional half, and the one the issue asks for by name.

    Two artifacts that agree on every number a numerator can carry — both
    report `fp_count = 0` — and differ only in whether anything was watched.
    The check is on the *rendered* output rather than on a helper's return
    value, because rendering is where the distinction is either kept or thrown
    away, and a report that throws it away is the exact failure D32 names.
    """
    failures: list[str] = []
    watched = render(synthetic(qualifying=10_000, observed=10_000, fp_count=0))
    unwatched = render(synthetic(qualifying=0, observed=0, fp_count=0))

    def coverage_line(lines: list[str]) -> str:
        matching = [line for line in lines if "coverage" in line and "≥" not in line]
        return matching[0].strip() if matching else ""

    watched_line = coverage_line(watched)
    unwatched_line = coverage_line(unwatched)

    if not watched_line or not unwatched_line:
        failures.append("0-of-0: the report renders no coverage line at all")
        return failures
    if watched_line == unwatched_line:
        failures.append(
            "0-of-0: '0 would-have-acted out of 10 000 observed' and '0 out of 0' "
            f"render identically as {watched_line!r}"
        )
    if "10000" not in watched_line.replace(",", "").replace(" ", ""):
        failures.append(
            f"0-of-0: the observed denominator is missing from {watched_line!r}; "
            "a rate without its denominator is not evidence"
        )
    if "undefined" not in unwatched_line:
        failures.append(
            f"0-of-0: an empty denominator rendered a rate anyway: {unwatched_line!r}"
        )
    for line in watched + unwatched:
        stripped = line.strip()
        if stripped.startswith("coverage") and "(" not in stripped:
            failures.append(
                f"0-of-0: a coverage line was rendered with no numerator/denominator "
                f"pair: {stripped!r}"
            )

    # And the predicate must refuse the unwatched one on the coverage term
    # specifically, not merely on the window.
    unwatched_terms = dict(
        (term, holds)
        for term, _evidence, holds in production_leg(
            synthetic(0, 0, 0), synthetic(0, 0, 0)["controls"][0]
        )
    )
    coverage_term = next(term for term in unwatched_terms if term.startswith("coverage"))
    if unwatched_terms[coverage_term]:
        failures.append(
            "0-of-0: the production leg's coverage term held over an empty cohort"
        )
    return failures


def self_test(artifact: dict) -> int:
    failures = validate(artifact)
    if failures:
        print("SELF-TEST FAILED")
        for failure in failures:
            print("  " + failure)
        return 1

    def control(doc: dict) -> dict:
        return doc["controls"][0]

    mutations: list[tuple[str, str, Callable[[dict], None]]] = [
        ("schema", "schema", lambda doc: doc.update(schema="orrery.ramp.report/99")),
        (
            "traffic",
            "provenance",
            lambda doc: doc["provenance"].update(traffic="probably fine"),
        ),
        (
            "source",
            "provenance source",
            lambda doc: doc["provenance"].update(source=""),
        ),
        ("inventory", "control inventory", lambda doc: doc["absent"].pop()),
        (
            "absent reason",
            "absent reason",
            lambda doc: doc["absent"][0].update(reason=""),
        ),
        (
            "denominator",
            "denominator",
            lambda doc: control(doc)["cohort"].pop("qualifying"),
        ),
        (
            "invented rate",
            "empty denominator",
            lambda doc: control(doc)["cohort"].update(
                qualifying=0, observed=0, unevaluated=0, coverage=0.0, fp_count=0,
                by_cause={},
            ),
        ),
        (
            "missing rate",
            "coverage present",
            lambda doc: control(doc)["cohort"].update(coverage=None),
        ),
        (
            "cohort arithmetic",
            "cohort arithmetic",
            lambda doc: control(doc)["cohort"].update(qualifying=1),
        ),
        (
            "fp bound",
            "fp bound",
            lambda doc: control(doc)["cohort"].update(fp_count=10**9),
        ),
        (
            "cohort size",
            "cohort size",
            lambda doc: control(doc)["cohort"].update(active=10**9),
        ),
        (
            "cohort halves",
            "cohort halves",
            lambda doc: control(doc)["cohort"].update(armed=0, natural=0),
        ),
        (
            "cause split",
            "cause split",
            lambda doc: control(doc)["cohort"].update(by_cause={"invented": 3}),
        ),
        (
            "fleet arithmetic",
            "fleet arithmetic",
            lambda doc: control(doc).update(qualifying=1),
        ),
        (
            "verdict split",
            "verdict split",
            lambda doc: control(doc)["by_verdict"].popitem(),
        ),
        (
            "spread",
            "spread bound",
            lambda doc: control(doc).update(accounts_would_act=10**9),
        ),
        (
            "truncation",
            "truncation",
            lambda doc: control(doc).update(accounts_truncated=1),
        ),
    ]

    mutation_failures: list[str] = []
    for name, expected, mutate in mutations:
        mutated = copy.deepcopy(artifact)
        mutate(mutated)
        found = validate(mutated)
        if not any(expected in failure for failure in found):
            mutation_failures.append(
                f"{name}: expected a failure containing {expected!r}, got {found}"
            )

    mutation_failures.extend(zero_of_zero_is_distinguishable())

    if mutation_failures:
        print("SELF-TEST FAILED: guarded facts")
        for failure in mutation_failures:
            print("  " + failure)
        return 1

    measured = artifact["controls"][0]
    print(
        f"SELF-TEST PASSED: {len(artifact['controls'])} measured control(s), "
        f"{len(artifact['absent'])} absent, "
        f"fp_count {measured['cohort']['fp_count']} over "
        f"{measured['cohort']['observed']:,} observed, "
        f"coverage {rate(measured['cohort']['coverage'], measured['cohort']['observed'], measured['cohort']['qualifying'])}, "
        f"{len(mutations)} guarded-fact mutations rejected, "
        "0-of-0 distinguishable from 0-of-10000"
    )
    return 0


if __name__ == "__main__":
    loaded = load()
    if "--self-test" in sys.argv:
        raise SystemExit(self_test(loaded))
    report(loaded)
