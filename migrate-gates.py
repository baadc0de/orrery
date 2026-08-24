#!/usr/bin/env python3
"""Move root p{N}-* directories under gates/ and repoint every reference.

Three transformations, and the second and third are where the danger is:

1. `git mv p{N}-name gates/p{N}-name` for each phase directory.

2. Relative escapes *inside* the moved trees get one level deeper. Each
   p-dir's Cargo.toml carries `path = "../crates/..."` style deps (21 lines
   across the repo) which become `../../crates/...`. Sibling references such
   as gates/p0-nat-test -> ../gates/p0-nat-lab keep working untouched, because both ends
   move together.

3. External references are rewritten ONLY where the token names the directory.
   23 files in scripts/ share the p{N}- prefix without being directories --
   p1-swarm-gate.sh, p2-abba-load.sh, p4-ledger.sh and friends. A naive
   substitution renames those to gates/p1-swarm-gate.sh and silently breaks
   every caller, so the pattern refuses to match when the name is followed by
   another word or hyphen.
"""
import re, subprocess, sys, pathlib

DRY = "--apply" not in sys.argv
root = pathlib.Path(".").resolve()
dirs = sorted(p.name for p in root.iterdir()
              if p.is_dir() and re.fullmatch(r"p\d-[a-z-]+", p.name))

SKIP_DIRS = {".git", "target", "vendor", "node_modules"}
TEXT_EXT = {".sh", ".yml", ".yaml", ".toml", ".md", ".rs", ".py", ".txt"}

def files():
    for p in root.rglob("*"):
        if not p.is_file() or p.suffix not in TEXT_EXT:
            continue
        if any(part in SKIP_DIRS for part in p.relative_to(root).parts):
            continue
        yield p

# (3) the guarded pattern: the directory name NOT followed by a word char or
# hyphen, and not already prefixed by gates/
pat = re.compile(r"(?<!gates/)\b(" + "|".join(re.escape(d) for d in dirs) + r")(?![-\w])")

changed, hits = {}, 0
for f in files():
    try:
        s = f.read_text()
    except UnicodeDecodeError:
        continue
    new, n = pat.subn(lambda m: "gates/" + m.group(1), s)
    if n:
        changed[f] = new
        hits += n

# (2) depth fix for files that are themselves moving
depth_fixed = {}
for d in dirs:
    for f in (root / d).rglob("*"):
        if not f.is_file() or f.suffix not in TEXT_EXT:
            continue
        try:
            s = changed.get(f) or f.read_text()
        except UnicodeDecodeError:
            continue
        # `path = "../X"` -> `path = "../../X"`; `include_str!("../../X")` -> one deeper
        new = re.sub(r'(path\s*=\s*")\.\./(?!\.)', r'\1../../', s)
        new = re.sub(r'(include_str!\(")\.\./\.\./', r'\1../../../', new)
        if new != s:
            depth_fixed[f] = new

print(f"  directories to move : {len(dirs)}")
for d in dirs: print(f"      {d}  ->  gates/{d}")
print(f"  files with references: {len(changed)}   (total {hits} substitutions)")
print(f"  files needing depth fix: {len(depth_fixed)}")
print(f"  scripts/ p-prefixed files protected from rename: "
      f"{len([p for p in (root/'scripts').iterdir() if re.match(r'p\\d-', p.name)])}")

if DRY:
    print("\n  DRY RUN — nothing written. Re-run with --apply")
    sys.exit(0)

(root / "gates").mkdir(exist_ok=True)
for d in dirs:
    subprocess.run(["git", "mv", d, f"gates/{d}"], check=True)
for f, s in changed.items():
    tgt = f
    rel = f.relative_to(root)
    if rel.parts and rel.parts[0] in dirs:
        tgt = root / "gates" / rel
    tgt.write_text(s)
for f, s in depth_fixed.items():
    rel = f.relative_to(root)
    tgt = root / "gates" / rel if rel.parts[0] in dirs else f
    tgt.write_text(s)
print("  applied")
