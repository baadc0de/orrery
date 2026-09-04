#!/usr/bin/env python3
"""Fail when a telemetry field is only ever written where it is constructed.

    scripts/telemetry-liveness-gate.py              scan the tree
    scripts/telemetry-liveness-gate.py --self-test  the scanner's own fixtures

Why this exists
---------------

`OverlayMetrics` carries what a volunteer ships back after a session. Six of
its twenty-three fields were assigned once, in `OverlayMetrics::new`, and never
again: `rollbacks_per_minute`, `prediction_set_size`, `live_discrepancies`,
`adjudications_completed`, `adjudication_latency_p50_ms` and
`adjudication_latency_p99_ms`. A twelve-minute human session on 2026-09-04
recorded `rollbacks_per_minute: 0` and `prediction_set_size: 2` — the
constructor's literals — and they were read back as measurements. A missing
field is an obvious gap; a defaulted one is quoted as evidence, and this one
cost a playtest (#1029).

The rule the owner stated is "no telemetry should be left to defaults and never
updated". A convention will rot — six fields already did — so this is the
mechanical form of it.

What the gate checks
--------------------

For every field of every `Serialize`-deriving struct declared in
`clients/regolith/src/telemetry.rs`, some *production* Rust source under
`clients/regolith/src/` must assign to it through a receiver **bound to that
struct's type**: `x.field = ...`, a compound assignment, or `&mut x.field`
(`std::mem::take`'s form). A field whose only writer is the struct literal in
its constructor is not assigned, because a struct literal writes
`field: value` and never `.field`.

The receiver matters, and finding that out is what this gate cost to get
right. A first cut matched `.field =` anywhere, and deleting
`metrics.uplink_shed = runtime.uplink_shed()` from the client did not fail it:
`CampaignRuntime` has an `uplink_shed` field of its own and increments it, so
the *name* was still assigned somewhere. Field names collide across structs by
design — the runtime counter and the overlay field are meant to share one —
so the scan first collects the identifiers actually bound to the audited type
(`mut metrics: ResMut<OverlayMetrics>`, `&mut OverlayMetrics`,
`let m: OverlayMetrics`, `OverlayMetrics::new(..)`) and accepts a write only
through one of those, plus `self` inside an `impl OverlayMetrics` block. A
struct no production code names at all is itself a failure, since every field
would otherwise pass on an empty search.

"Production" excludes, by construction:

* every `#[cfg(test)]` item and every `#[test]` function, so a field a unit
  test assigns and the client never does still fails;
* `clients/regolith/tests/`, which is not scanned at all;
* comments and string literals, so naming a field in a doc comment or a format
  string is not a write.

A field that genuinely has no runtime writer may be exempted in `ALLOW` below,
which requires a reason in the same line. An exemption naming a field that no
longer exists is itself a failure — a stale exemption is how a list like this
grows to cover things nobody decided to exempt.

What escapes it, stated plainly
-------------------------------

This is a static text scan, not a call-graph analysis, so:

1. **Dead production code passes.** A field assigned inside a `fn` that nothing
   calls — or, the realistic case in a Bevy client, inside a system that is
   never added to a schedule — reads as wired. `dead_code` catches the unused
   private function; it does not catch a registered-but-unscheduled system.
2. **A fabricated constant passes.** `metrics.rollbacks_per_minute = 0;`
   satisfies this gate completely. The gate can see liveness and cannot see
   truth; nothing mechanical can. It narrows the failure from "never written"
   to "written with a lie", which is a smaller place to have to look.
3. **Telemetry declared elsewhere is not covered.** Only structs in
   `clients/regolith/src/telemetry.rs` are audited. A second telemetry struct
   put in another module is outside the gate until its file is added to
   `TARGETS`.
4. **A same-named field on a receiver of an unrelated type still counts, if
   that receiver's own type is never named in a form the binding scan
   recognises.** Binding discovery is textual: a receiver reached through a
   method chain (`session.metrics().field = ..`), a closure parameter with an
   inferred type, or a `let` with no annotation is not attributed to any
   struct, and such a write is ignored rather than credited — which fails
   safe, but means a field whose *only* writer takes that shape reads as
   unwired.
5. **Exotic writes are invisible.** A functional-update literal
   (`OverlayMetrics { field: v, ..old }`) outside the constructor, a write
   through a raw pointer, or a field mutated in another crate through a `&mut`
   to the whole struct would all be missed. None exists today; all three would
   read as a failure here rather than as a false pass, so the gate errs toward
   noise rather than silence.
6. **A `#[cfg(...)]` arm for a platform nobody builds passes.** The scan is
   textual and reads every arm.
"""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path

NAME = "telemetry-liveness-gate"

ROOT = Path(__file__).resolve().parent.parent

# (source root scanned for writers, telemetry file declaring the fields).
TARGETS = [("clients/regolith/src", "clients/regolith/src/telemetry.rs")]

# Fields with no runtime writer, and why each is not a defect. Keyed
# `Struct.field`. The reason is not decoration: it is what a later reader needs
# in order to decide whether the exemption still holds.
ALLOW = {
    "OverlayMetrics.session_record_path": (
        "resolved once at startup by `paths` and handed to the constructor; "
        "the client never re-resolves it, so a row that reported it changing "
        "would be reporting something that did not happen"
    ),
}


def die(message: str) -> None:
    print(f"{NAME}: {message}", file=sys.stderr)
    raise SystemExit(1)


def strip_comments_and_strings(src: str) -> str:
    """Blank out comments and string/char literals, preserving offsets.

    Offsets are preserved (everything is replaced by spaces or newlines) so
    brace matching downstream still lines up with the original text.
    """
    out = []
    i = 0
    n = len(src)
    while i < n:
        two = src[i : i + 2]
        if two == "//":
            while i < n and src[i] != "\n":
                out.append(" ")
                i += 1
            continue
        if two == "/*":
            depth = 1
            out.append("  ")
            i += 2
            while i < n and depth:
                if src[i : i + 2] == "/*":
                    depth += 1
                    out.append("  ")
                    i += 2
                elif src[i : i + 2] == "*/":
                    depth -= 1
                    out.append("  ")
                    i += 2
                else:
                    out.append("\n" if src[i] == "\n" else " ")
                    i += 1
            continue
        # Raw strings: r"...", r#"..."#, br##"..."##.
        raw = re.match(r'(?:b?r)(#*)"', src[i:])
        if raw and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            hashes = raw.group(1)
            close = '"' + hashes
            end = src.find(close, i + raw.end())
            end = n if end < 0 else end + len(close)
            for ch in src[i:end]:
                out.append("\n" if ch == "\n" else " ")
            i = end
            continue
        if src[i] == '"':
            out.append(" ")
            i += 1
            while i < n:
                if src[i] == "\\":
                    out.append("  ")
                    i += 2
                    continue
                if src[i] == '"':
                    out.append(" ")
                    i += 1
                    break
                out.append("\n" if src[i] == "\n" else " ")
                i += 1
            continue
        # Char literals, but not lifetimes ('a) or labels ('outer:).
        if src[i] == "'":
            lit = re.match(r"'(?:\\.|[^\\'])'", src[i:])
            if lit:
                out.append(" " * lit.end())
                i += lit.end()
                continue
        out.append(src[i])
        i += 1
    return "".join(out)


def _skip_item(src: str, start: int) -> int:
    """End offset of the item beginning at `start` (a `{...}` block or a `;`)."""
    i = start
    n = len(src)
    while i < n:
        if src[i] == ";":
            return i + 1
        if src[i] == "{":
            depth = 0
            while i < n:
                if src[i] == "{":
                    depth += 1
                elif src[i] == "}":
                    depth -= 1
                    if depth == 0:
                        return i + 1
                i += 1
            return n
        i += 1
    return n


def strip_test_items(src: str) -> str:
    """Remove every `#[cfg(test)]` and `#[test]` item, attribute included."""
    pattern = re.compile(r"#\[\s*(?:cfg\s*\(\s*test\s*\)|test)\s*\]")
    while True:
        found = pattern.search(src)
        if not found:
            return src
        end = _skip_item(src, found.end())
        src = src[: found.start()] + src[end:]


def serialize_structs(telemetry_src: str) -> dict[str, list[str]]:
    """`{struct name: [field names]}` for `Serialize`-deriving declarations."""
    src = strip_comments_and_strings(telemetry_src)
    structs: dict[str, list[str]] = {}
    for found in re.finditer(r"pub\s+struct\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{", src):
        head = src[: found.start()]
        derive = re.search(r"#\[derive\(([^)]*)\)\][^{}]*$", head, re.DOTALL)
        if not derive or "Serialize" not in derive.group(1):
            continue
        body_end = _skip_item(src, found.end() - 1)
        body = src[found.end() : body_end - 1]
        depth = 0
        fields = []
        for token in re.finditer(r"[<>(){}\[\]]|pub\s+([A-Za-z_][A-Za-z0-9_]*)\s*:", body):
            if token.group(1) is not None:
                if depth == 0:
                    fields.append(token.group(1))
                continue
            depth += 1 if token.group(0) in "<({[" else -1
        structs[found.group(1)] = fields
    return structs


def writer_text(src_dir: Path) -> str:
    """Every production Rust source under `src_dir`, comments and tests gone."""
    chunks = []
    for path in sorted(src_dir.rglob("*.rs")):
        chunks.append(strip_test_items(strip_comments_and_strings(path.read_text())))
    return "\n".join(chunks)


def bindings_for(struct: str, src: str) -> set[str]:
    """Identifiers production code binds to `struct`, plus `self` in its impls.

    Textual, and deliberately generous: a name that reaches this set only ever
    *permits* a write to be counted, and a name missed here makes the gate
    stricter rather than laxer.
    """
    names: set[str] = set()
    ident = r"([A-Za-z_][A-Za-z0-9_]*)"
    wrappers = r"(?:&\s*(?:mut\s+)?|(?:ResMut|Res|Mut|Ref|Box|Arc|Rc|Option)\s*<\s*)*"
    # `mut metrics: ResMut<OverlayMetrics>`, `metrics: &mut OverlayMetrics`,
    # `let m: OverlayMetrics`, and struct-typed fields.
    for found in re.finditer(
        rf"\b(?:mut\s+)?{ident}\s*:\s*{wrappers}{re.escape(struct)}\b", src
    ):
        names.add(found.group(1))
    # `let mut metrics = OverlayMetrics::new(..)`, `= OverlayMetrics {`.
    for found in re.finditer(
        rf"\blet\s+(?:mut\s+)?{ident}\s*(?::[^=;]*)?=\s*(?:&\s*mut\s+)?"
        rf"{re.escape(struct)}\s*(?:::|\{{)",
        src,
    ):
        names.add(found.group(1))
    names.discard("_")
    return names


def impl_bodies(struct: str, src: str) -> str:
    """The concatenated bodies of every `impl ... StructName {` block."""
    bodies = []
    for found in re.finditer(rf"\bimpl\b[^;{{]*\b{re.escape(struct)}\b[^;{{]*\{{", src):
        end = _skip_item(src, found.end() - 1)
        bodies.append(src[found.end() : end])
    return "\n".join(bodies)


# `x.field =`, `x.field += ...`, and `&mut x.field`, for a receiver `x` known
# to hold the audited struct. `=(?!=)` keeps `==` out; `>=`, `<=` and `!=`
# cannot match because `>`, `<` and `!` are not operators in the alternation.
def _assignment(field: str, receivers: set[str]) -> re.Pattern[str]:
    ops = r"(?:\+|-|\*|/|%|\||&|\^|<<|>>)?"
    who = "|".join(sorted(re.escape(name) for name in receivers))
    # A receiver may be reached through plain field access on the binding
    # (`self.metrics.field`), so allow a dotted tail after the bound name.
    recv = rf"\b(?:{who})(?:\s*\.\s*[A-Za-z_][A-Za-z0-9_]*)*\s*\.\s*{re.escape(field)}"
    return re.compile(rf"{recv}\s*{ops}=(?!=)|&\s*mut\s+{recv}\b")


def audit(root: Path, targets, allow) -> list[str]:
    """Failure sentences, empty when every declared field has a live writer."""
    failures: list[str] = []
    declared: set[str] = set()
    for src_rel, telemetry_rel in targets:
        src_dir = root / src_rel
        telemetry = root / telemetry_rel
        if not telemetry.is_file():
            failures.append(f"{telemetry_rel} does not exist; the target list has drifted")
            continue
        structs = serialize_structs(telemetry.read_text())
        if not structs:
            failures.append(
                f"{telemetry_rel} declares no Serialize struct; "
                "the scan would pass by finding nothing"
            )
            continue
        haystack = writer_text(src_dir)
        for struct, fields in sorted(structs.items()):
            if not fields:
                failures.append(f"{struct} parsed with no fields; the scan has drifted")
            receivers = bindings_for(struct, haystack)
            # `self` is only a receiver for this struct *inside its own impl
            # blocks*, so it is searched against those bodies and never
            # against the tree: `CampaignRuntime` also has an `uplink_shed`
            # and writes `self.uplink_shed += 1`, and crediting that to
            # `OverlayMetrics` is the exact false pass this scoping removes.
            body = impl_bodies(struct, haystack)
            if not receivers and not body:
                failures.append(
                    f"no production source under {src_rel} binds a {struct}, so "
                    "every one of its fields would pass on an empty search"
                )
                continue
            for field in fields:
                key = f"{struct}.{field}"
                declared.add(key)
                if key in allow:
                    continue
                written = receivers and _assignment(field, receivers).search(haystack)
                written = written or (body and _assignment(field, {"self"}).search(body))
                if not written:
                    failures.append(
                        f"{key} is never assigned outside its constructor: "
                        f"no production source under {src_rel} writes "
                        f"`.{field} =` through a {struct}. "
                        "Wire it to the runtime value it names, or delete it — "
                        "a field that always reports its initial literal reads "
                        "as a measurement and is not one"
                    )
    for key, reason in sorted(allow.items()):
        if key not in declared:
            failures.append(
                f"the exemption for {key} names a field that no longer exists "
                f"(reason on file: {reason})"
            )
    return failures


# ── Self-test ───────────────────────────────────────────────────────────────
#
# Fixtures rather than the tree, in both directions: a clean fixture must pass,
# and each planted defect must fail *by name*. A gate whose failure path is
# never exercised is a gate that reports success on a broken scan.

_CLEAN_TELEMETRY = """\
//! Fixture.
use serde::Serialize;

/// Not serialized, so not audited: `ghost` is deliberately never assigned.
#[derive(Debug)]
pub struct NotTelemetry {
    pub ghost: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Fixture {
    /// A live counter.
    pub alive: u64,
    /// Taken through `&mut`.
    pub taken: u64,
    /// Compound assignment only.
    pub bumped: u64,
    /// Generic type with a comma, to exercise the depth counter.
    pub pair: Option<(u64, u64)>,
    /// Exempt in the fixture allowlist.
    pub frozen: u64,
}

impl Fixture {
    pub fn new() -> Self {
        Self { alive: 0, taken: 0, bumped: 0, pair: None, frozen: 7 }
    }
}
"""

_CLEAN_WRITERS = """\
use super::Fixture;

pub fn drive(f: &mut Fixture, runtime: &Runtime) {
    f.alive = runtime.alive();
    let _ = std::mem::take(&mut f.taken);
    f.bumped += 1;
    f.pair = runtime.pair();
    // A comparison is not a write: f.frozen == 0
    let _ = f.frozen;
}
"""


def _write_fixture(root: Path, telemetry: str, writers: str) -> None:
    src = root / "src"
    src.mkdir(parents=True, exist_ok=True)
    (src / "telemetry.rs").write_text(telemetry)
    (src / "drive.rs").write_text(writers)


def self_test() -> None:
    targets = [("src", "src/telemetry.rs")]
    allow = {"Fixture.frozen": "fixture exemption"}
    checks = 0

    def run(label, telemetry, writers, allow_map, expect):
        nonlocal checks
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write_fixture(root, telemetry, writers)
            failures = audit(root, targets, allow_map)
            if expect is None:
                if failures:
                    die(f"self-test: {label} should be clean, got: {failures}")
            else:
                if not any(expect in f for f in failures):
                    die(
                        f"self-test: {label} should have failed naming "
                        f"{expect!r}, got: {failures}"
                    )
            checks += 1

    # The parse itself: five fields found, and the non-Serialize struct's
    # `ghost` not among them. Without this a scan that found nothing would
    # report every fixture below as clean and this file would prove nothing.
    structs = serialize_structs(_CLEAN_TELEMETRY)
    if sorted(structs) != ["Fixture"]:
        die(f"self-test: expected only Fixture to be audited, got {sorted(structs)}")
    if structs["Fixture"] != ["alive", "taken", "bumped", "pair", "frozen"]:
        die(f"self-test: field parse drifted: {structs['Fixture']}")
    checks += 1

    run("a fully wired struct", _CLEAN_TELEMETRY, _CLEAN_WRITERS, allow, None)

    # 1. The defect itself: declared, constructed, never assigned.
    run(
        "a field with no writer at all",
        _CLEAN_TELEMETRY.replace("pub pair: Option<(u64, u64)>,", "pub pair: Option<(u64, u64)>,\n    pub orphan: u64,").replace(
            "pair: None,", "pair: None, orphan: 0,"
        ),
        _CLEAN_WRITERS,
        allow,
        "Fixture.orphan",
    )

    # 2. Assigned only by a unit test in a production file. This is the clause
    #    that makes the gate mean "the client writes it" rather than "the
    #    string appears somewhere".
    run(
        "a field only a #[cfg(test)] module assigns",
        _CLEAN_TELEMETRY,
        _CLEAN_WRITERS.replace("    f.alive = runtime.alive();\n", "")
        + """
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assigns_it() {
        let mut f = Fixture::new();
        f.alive = 9;
        assert_eq!(f.alive, 9);
    }
}
""",
        allow,
        "Fixture.alive",
    )

    # 3. Assigned only by a `#[test]` function sitting in a production module
    #    with no enclosing `#[cfg(test)] mod`.
    run(
        "a field only a bare #[test] fn assigns",
        _CLEAN_TELEMETRY,
        _CLEAN_WRITERS.replace("    f.bumped += 1;\n", "")
        + """
#[test]
fn bumps() {
    let mut f = Fixture::new();
    f.bumped += 1;
}
""",
        allow,
        "Fixture.bumped",
    )

    # 4. Named in prose and in a format string, written nowhere. The 2026-09-04
    #    session's rows are exactly this shape: the field is all over the HUD
    #    code and assigned by none of it.
    run(
        "a field only mentioned in a comment and a format string",
        _CLEAN_TELEMETRY,
        _CLEAN_WRITERS.replace(
            "    f.taken = ",
            "    // f.taken = 0;\n    f.taken_NOT = ",
        ).replace(
            "let _ = std::mem::take(&mut f.taken);",
            'println!("taken {}", f.taken); // f.taken = 1;',
        ),
        allow,
        "Fixture.taken",
    )

    # 5. The struct literal in the constructor is not an assignment. Deleting
    #    every writer must fail even though `new` sets all five.
    run(
        "a constructor that sets every field and no writer that does",
        _CLEAN_TELEMETRY,
        "pub fn drive() {}\n",
        allow,
        "Fixture.alive",
    )

    # 6. The collision this gate was nearly shipped without catching: another
    #    struct in the same tree carries a field of the same name and writes
    #    it. Removing the telemetry write must still fail. `metrics.uplink_shed
    #    = runtime.uplink_shed()` deleted from the real client passed the first
    #    version of this scanner, because `CampaignRuntime::uplink_shed` is
    #    incremented on a real failure path and the name was therefore still
    #    "assigned somewhere".
    run(
        "a field whose only writer assigns the same name on another type",
        _CLEAN_TELEMETRY,
        _CLEAN_WRITERS.replace("    f.alive = runtime.alive();\n", "")
        + """
pub struct Runtime {
    pub alive: u64,
}

impl Runtime {
    pub fn count(&mut self) {
        self.alive += 1;
    }
}

pub fn also(r: &mut Runtime) {
    r.alive = 3;
}
""",
        allow,
        "Fixture.alive",
    )

    # 7. And the other direction, so the scoping above is not simply strict:
    #    a struct that writes its own fields from inside its own impl is
    #    wired, and must not be reported.
    run(
        "a struct that assigns its own fields through self",
        _CLEAN_TELEMETRY.replace(
            """impl Fixture {""",
            """impl Fixture {
    pub fn refresh(&mut self, n: u64) {
        self.alive = n;
    }
""",
        ),
        _CLEAN_WRITERS.replace("    f.alive = runtime.alive();\n", ""),
        allow,
        None,
    )

    # 8. A stale exemption. The exemption list is the one part of this gate a
    #    human edits, so it gets the same treatment check.sh gives its own.
    run(
        "an exemption for a field that does not exist",
        _CLEAN_TELEMETRY,
        _CLEAN_WRITERS,
        {**allow, "Fixture.departed": "fixture exemption"},
        "Fixture.departed",
    )

    # 9. A telemetry file that parses to nothing must fail rather than pass
    #    vacuously — the failure mode a scanner is likeliest to acquire.
    run(
        "a telemetry file the parser finds no struct in",
        "//! Fixture with nothing in it.\n",
        _CLEAN_WRITERS,
        {},
        "declares no Serialize struct",
    )

    print(f"{NAME}: self-test passed ({checks} checks)")


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        self_test()
        return 0
    if len(argv) > 1:
        die(f"unrecognized argument: {argv[1]}")
    failures = audit(ROOT, TARGETS, ALLOW)
    if failures:
        for failure in failures:
            print(f"{NAME}: {failure}", file=sys.stderr)
        die(
            "a telemetry field written only where it is constructed reports its "
            "initial literal forever, and a reader cannot tell that from a "
            "measurement (#1029)"
        )
    audited = sum(len(f) for _, t in TARGETS for f in serialize_structs((ROOT / t).read_text()).values())
    print(f"{NAME}: passed ({audited} fields audited, {len(ALLOW)} exempted)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
