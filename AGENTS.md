# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

**Orrery** is an in-development set of Rust crates for the Bevy game engine (0.19):
peer-to-peer multiplayer (QUIC transport with NAT hole punching via iroh) and a
persistent-universe backend. The repository contains the accepted architecture,
active P0–P2 implementation, test tools, and incomplete milestone harnesses.

## Reading path (normative order)

The design is documented in `docs/`. Accepted ADRs are normative over the
README and every numbered expansion document.

Start with [docs/DECISIONS.md](docs/DECISIONS.md), the ADR index and governance
entry point. Decisions live independently under [docs/adr/](docs/adr/):

- For architecture-wide work, read all accepted ADRs in numeric order.
- For scoped work, read the index, the ADRs named by the relevant expansion
  document, and any ADR dependencies those records link.
- Never treat the index summary as a substitute for the applicable ADR text.
- A future change to an accepted decision is a new ADR that explicitly
  supersedes the old one; do not silently rewrite architectural history.

| Decision | ADR | Covers |
|---|---|---|
| D1 | [ADR-0001](docs/adr/0001-requirements.md) | Requirements |
| D2 | [ADR-0002](docs/adr/0002-simulation-model.md) | Simulation model |
| D3 | [ADR-0003](docs/adr/0003-transport.md) | iroh transport |
| D4 | [ADR-0004](docs/adr/0004-bevy-netcode-stack.md) | Bevy netcode stack |
| D5 | [ADR-0005](docs/adr/0005-spatial-model.md) | Spatial model and CellId |
| D6 | [ADR-0006](docs/adr/0006-population-adaptive-topology.md) | Island topology |
| D7 | [ADR-0007](docs/adr/0007-authority-and-leases.md) | Authority and leases |
| D8 | [ADR-0008](docs/adr/0008-prediction-rollback-interpolation.md) | Prediction and rollback |
| D9 | [ADR-0009](docs/adr/0009-verifiable-core.md) | Verifiable core |
| D10 | [ADR-0010](docs/adr/0010-witnessing.md) | Witnessing |
| D11 | [ADR-0011](docs/adr/0011-persistence.md) | Persistence |
| D12 | [ADR-0012](docs/adr/0012-backend-services.md) | Backend services |
| D13 | [ADR-0013](docs/adr/0013-physics-and-determinism.md) | Physics posture |
| D14 | [ADR-0014](docs/adr/0014-pinned-versions.md) | Pinned versions |
| D15 | [ADR-0015](docs/adr/0015-crate-set.md) | Crate set |
| D16 | [ADR-0016](docs/adr/0016-parameter-reference.md) | Parameter defaults |
| D17 | [ADR-0017](docs/adr/0017-risks-and-open-questions.md) | Risks and open questions |

After the applicable ADRs, use this expansion reading path:

| Order | Document | Covers |
|---|---|---|
| 1 | [docs/00-overview.md](docs/00-overview.md) | Goals, constraints, system diagram, subsystem tour, glossary |
| 2 | [docs/01-spatial-model.md](docs/01-spatial-model.md) | Grid, `CellId` encoding, `big_space`, AOI, hysteresis, hotspots |
| 3 | [docs/02-networking.md](docs/02-networking.md) | iroh, relays, islands, topology regimes, channels, bandwidth |
| 4 | [docs/03-replication.md](docs/03-replication.md) | replicon/lightyear stack, interest sets, delta compression, priority |
| 5 | [docs/04-authority.md](docs/04-authority.md) | Weak/strong claims, leases, handoff, orphans, promotion |
| 6 | [docs/05-prediction-rollback.md](docs/05-prediction-rollback.md) | Timelines, prediction sets, reconciliation, interpolation, hit validation |
| 7 | [docs/06-verifiable-core.md](docs/06-verifiable-core.md) | `Ruleset`, determinism scoping, signed input logs, replay harness |
| 8 | [docs/07-witnessing.md](docs/07-witnessing.md) | Threat model, discrepancy protocol, adjudication, strikes |
| 9 | [docs/08-persistence.md](docs/08-persistence.md) | Cell actors, journal, FDB schema, intents, terrain, event archive |
| 10 | [docs/09-services-and-ops.md](docs/09-services-and-ops.md) | Service inventory, deployment, scaling, failure modes, telemetry |
| 11 | [docs/10-crates.md](docs/10-crates.md) | Workspace layout, per-crate API sketches, dependency graph |
| 12 | [docs/11-roadmap.md](docs/11-roadmap.md) | Build phases (P0–P6), milestones, tracked risks |
| 13 | [docs/12-world-seeding.md](docs/12-world-seeding.md) | World seeder: TOML scenario runner, generator bank, content diff/patch (expands 08 §17) |
| 14 | [docs/13-chain-replication.md](docs/13-chain-replication.md) | Cross-process journal mirroring, reconnect, and recovery |
| 15 | [docs/references.md](docs/references.md) | Annotated bibliography, organized by topic |

Also read [README.md](README.md) — it summarizes the architecture, the status,
and the feature set.

## Ground rules

- **Accepted ADRs are normative.** The records in `docs/adr/` govern the
  README and numbered docs. If an expansion conflicts with an applicable ADR,
  the ADR wins.
- **Implementation is partial.** Inspect the current tree before assuming a
  designed crate or service exists. Code sketches in `docs/10-crates.md` are
  indicative of shape, not guaranteed to match landed APIs.
- **Pinned versions ([D14](docs/adr/0014-pinned-versions.md)).** All dependency
  versions reflect the ecosystem as of August 2026 and are re-validated when
  implementation starts. Don't bump them casually.
- **Roadmap gates ([D17](docs/adr/0017-risks-and-open-questions.md)).** Each
  phase (P0–P6) has a demo criterion that is a permanent regression harness and
  gates entry to the next phase. See [docs/11-roadmap.md](docs/11-roadmap.md).

## CI

`.github/workflows/ci.yml` runs on every push and pull request: `rustfmt`,
`clippy -D warnings`, the verifiable-core static gates
(`scripts/core-gates.sh`), the P2/P3 harness `--self-test` modes, the workspace
test suite, and the cross-platform determinism matrix.

Three things about it are worth knowing before you change anything it touches.

**`clippy` is enforced at `-D warnings`, over two feature sets.** The default
build and the `fdb` build compile different code, and the `fdb` half went
unlinted long enough for `orrery_seed/tests/fdb_gates.rs` to stop compiling
altogether. Both are gated now. `clippy` needs only metadata, so the `fdb` pass
runs with no `libfdb_c` on the runner. Vendored crates under `vendor/` are
excluded — their findings are upstream's to fix — and the run passes
`--no-deps`, without which `--exclude` does not actually spare them: they are
still path dependencies, and clippy lints those too. The workspace test job
excludes the same three: `bevy_replicon`'s own tests and doctests do not compile
under this workspace's feature unification, because `bevy/serialize` is off and
they need `Transform: Serialize`.

One thing to know while you are in there: **`[workspace.lints]` currently
reaches only the vendored crates.** `vendor/aeronet_iroh`,
`vendor/aeronet_tokio_runtime` and `vendor/bevy_replicon` are the only manifests
with `[lints] workspace = true`, so the `pedantic`/`nursery`/`missing_docs`/
`unwrap_used` levels configured at the workspace root apply to third-party code
and to none of `crates/*`. That is backwards, and adopting it across the
first-party crates is its own piece of work — a large one, since those levels
have never been enforced there. The CI gate gates what is enforceable today:
default `clippy` at `-D warnings`.

**Determinism is checked *across* platforms, not on each one.** `orrery_core`'s
own tests run an identical tick twice in-process and compare hashes, which
catches VC-4/VC-8 violations but only ever proves one platform agrees with
itself. `orrery_conformance` closes that: it runs a fixed corpus through a
reference ruleset on the four supported targets (x86_64 Linux/Windows,
aarch64 Linux/macOS — x86_64 macOS is one of docs/06 §8's five, deliberately
dropped as unsupported), emits a digest of per-tick state hashes, and a
final job compares them. Discrete state must be bit-identical; a mismatch is
localized to the first diverging `(tick, entity)` and quantified against the §5
bands so you can tell `libm` drift from a rules change.

`crates/orrery_conformance/corpus/golden.json` is committed, and every platform
also checks against it inside the ordinary test suite. **If you change the
reference ruleset, bump `REFERENCE_RULESET.version` and regenerate the golden:**

```sh
cargo run -p orrery_conformance -- emit --out crates/orrery_conformance/corpus/golden.json --compact
```

Regenerating without bumping the version hides a rules change as a determinism
pass, which is the one failure this whole apparatus exists to prevent.

`orrery_games` — the reference games P4 measures against — rides the same
matrix and carries the same obligation. Its golden chains live in
`crates/orrery_games/src/golden.rs`, one per scenario per game, and every
target checks them inside the ordinary test suite. **If you change a game's
rules, its pilot, its spawns or its scenario table, bump that game's
`RulesetId` version and regenerate:**

```sh
cargo test -p orrery_games --test battery -- --ignored --nocapture emit_goldens
cargo fmt -p orrery_games
```

`scripts/core-gates.sh` scans `orrery_games` alongside `orrery_core` — the
determinism rules are about the rules code, so a `HashMap` or a
`SystemTime::now` inside a `Ruleset` fails the same gate it would in the core.

**`sccache` is cleared on GitHub-hosted runners, and deliberately not on the
self-hosted one.** `.cargo/config.toml` sets `build.rustc-wrapper = "sccache"`
for local worktrees; the workflows set `RUSTC_WRAPPER: ""` at the top because a
GitHub runner is ephemeral and has nothing to hit. The jobs that can land on
`orrery-hel1-1` set it back to `sccache`, because that box keeps a persistent
`target/` and a cache at `/var/cache/sccache` that is **shared with the dev
checkout on the same machine** — same dependency graph, so a CI build starts
warm off whatever was compiled by hand there, and vice versa.

**The heavy Linux jobs run on a self-hosted box.** `clippy`, `static gates` and
`workspace tests` run on `orrery-hel1-1` for pushes and same-repository pull
requests, and fall back to `ubuntu-latest` for fork pull requests; `p1-swarm`
and the determinism soak run there nightly. Measured on the workspace test
job: 305–549 s hosted, 182 s cold on the box, **48 s warm**.

**Three runners, and each job is pinned to one of them.** A runner takes one
job at a time, so a single runner would serialize the three heavy jobs that
GitHub used to run on three machines. Three runners share the box — but not a
`target/`, since cargo takes an exclusive lock on one. That is why the jobs are
*pinned* by label (`orrery-clippy`, `orrery-gates`, `orrery-tests`) rather than
left to land wherever: an unpinned job runs against a directory last used by a
different job and rebuilds most of it. Each runner caps `CARGO_BUILD_JOBS` at
8, mild oversubscription across three concurrent jobs on 16 threads, which
keeps a lone nightly job fast when it has the box to itself.

**Do not restart the runner services while a run is in flight** — the job dies
as `The operation was canceled`, which reads like a test failure and is not
one.

Three things to know before changing any of it. The repository is public, so
the security posture is layered and the in-workflow runner guard is the
*weakest* of the three layers — see the comment on the `runner` job in
`ci.yml`. The runner is an unprivileged `ci` user with **no sudo**, which is
why every `apt-get` step in those jobs is conditioned on
`runner.environment == 'github-hosted'` — and why a missing system library on
the box is an ssh-and-install away rather than a workflow edit. What it needs
beyond a stock Ubuntu: the Bevy build dependencies, `foundationdb-clients`, and
**`libclang-dev`** (`foundationdb-sys` runs bindgen, which the hosted images
happen to satisfy and a bare box does not). And `p2-kill9` and `p3-island` stay on
hosted runners on purpose: `scripts/fdb-dev.sh` hardcodes `127.0.0.1:4500` and
stops the cluster with `pkill -f "fdbserver.*:4500"`, while the box runs its own
`fdbserver` on that port for development. Teaching that harness to take its
port and data directory from the environment is what those two jobs are waiting
on.

The heavy harnesses — P2's kill-9 gate, which needs a real FoundationDB
cluster, and P3's island gate, which needs eight peer processes and a real
`kill -9` — cannot gate a pull request. They run nightly and on demand in
`.github/workflows/nightly.yml`, alongside a soak that repeats the corpus ten
times in one process to catch per-process nondeterminism.

## Build cache and target directories

Agents work in parallel git worktrees, and a Rust `target/` is enormous: one
worktree here reached **77 GiB**, a second checkout **182 GiB**, and 17 GiB of
that was incremental-compilation scratch alone. Left alone this fills the disk,
and a build that dies with `No space left on device` costs more than it saves.

The arrangement is: **every worktree keeps its own `target/`, and they all share
one object cache.**

Sharing a `CARGO_TARGET_DIR` instead would look tempting and be wrong — cargo
takes an exclusive lock on a target directory, so two agents building at once
would serialize, one waiting on the other for the whole build. The object cache
has no such contention: identical `rustc` invocations, which is nearly the
entire dependency graph, are compiled once per machine and reused everywhere.

### What is configured, and where

| Setting | Location | Committed? |
|---|---|---|
| `build.rustc-wrapper = "sccache"` | `.cargo/config.toml` | yes — worktrees each get a copy of tracked files, so this is the only way a setting reaches all of them |
| `build.incremental = false` | `.cargo/config.toml` | yes |
| cache directory and size cap | `~/.config/sccache/config` | no — machine-local |

The standalone tools (`p2-load`, `p3-island`, `p0-*`) each declare their own
`[workspace]`, so each has its own `target/`. They still inherit the repo's
`.cargo/config.toml`, because cargo walks up from the working directory — do
not add a per-tool `.cargo/config.toml`, which would shadow it and silently
drop that tool back to uncached builds.

**Incremental compilation is off deliberately, and the two reasons compound.**
sccache cannot cache an incremental unit — it marks them non-cacheable — so
leaving it on would defeat the cache for exactly the crates being worked on,
while still writing the artifacts to disk. If you are a human tight-looping
edits on one crate, that trade is not in your favour: use `CARGO_INCREMENTAL=1`
for that session, which overrides the file.

### This makes sccache a build prerequisite

If it is missing, install it (`pacman -S sccache`, or
`cargo install sccache --locked`) or opt out for one command with an empty
wrapper, which takes precedence over the config file:

```
RUSTC_WRAPPER= cargo build
```

### Working with it

```
./scripts/dev-cache.sh doctor   # is the cache wired up and actually taking effect?
./scripts/dev-cache.sh stats    # hit rate, cache size
./scripts/dev-cache.sh disk     # what every target/ in this checkout costs
./scripts/dev-cache.sh prune    # delete every target/ — safe, and meant to be used
```

`prune` is the lever to pull when disk gets tight, and pulling it is cheap:
sources are in git and the rebuild refills from the cache. Measured on the
`p3-island` tool, deleting its whole `target/` and rebuilding: **100% cache hit
rate, 304/304 units**, 21 s cold versus 14 s warm — the residual is linking and
cargo's own bookkeeping, which no object cache can remove.

Two things follow for agents sharing this machine:

- **Prune freely, and prune your own worktree before a long build.** You are not
  destroying anyone's work; you are dropping a derived artifact that another
  agent's build already paid to compute.
- **Do not read `Non-cacheable calls` in the stats as breakage.** Linking,
  `build.rs` executions, and a few binary crate-type units are not cacheable by
  design. A healthy report has a high hit *rate* with a non-zero non-cacheable
  count.

## Working alongside other agents

Several agents work this repository at once, each in its own git worktree. The
worktrees isolate the filesystem and nothing else: there is one `.git`, one
sccache, one disk, one FoundationDB dev cluster, one set of harness ports, one
GitHub remote. Everything below exists because of that asymmetry.

**Be clear about what a collision actually is.** Two agents editing the same
file in two worktrees do *not* clobber each other — separate checkouts, separate
inodes, and neither can see the other's buffer. What they produce is a merge
conflict, discovered later and further from the decision that caused it. That is
worth knowing about in advance, but it is not a reason to stop. The things that
genuinely cannot be shared are elsewhere, and they are the ones worth blocking
on: the `.fdb-dev/` cluster, a harness's fixed ports, `git push` and branch
deletion, `git worktree add/remove`, and the disk itself.

So this arrangement is deliberately two-speed: **lanes are advisory, leases are
exclusive.**

### Where the shared state lives

In the git *common* directory — `$(git rev-parse --path-format=absolute
--git-common-dir)`, which every worktree of this clone resolves to the same
absolute path, and which is never committed. Nowhere else has both properties: a
tracked path is copied per worktree and eventually committed by accident, and a
git-ignored path is per-worktree too.

That last point was a live bug rather than a hypothetical. `.agents/memory/` is
git-ignored, so it existed only in whichever checkout created it and was
invisible from every other worktree — machine-local memory that was really
main-checkout-local memory. It now lives in the common directory, with
`.agents/memory` as a symlink into it. Run `scripts/agent-lane.sh init` once in
a new worktree to create that link.

### The driver

`scripts/agent-lane.sh` is committed, so every worktree gets a copy — the same
reason `.cargo/config.toml` is committed.

```
scripts/agent-lane.sh register --task "..." --paths crates/orrery_witness/,p1-swarm/
scripts/agent-lane.sh list                   # who else is working, on what, where
scripts/agent-lane.sh check <path>...        # does anyone else claim this?
scripts/agent-lane.sh lease acquire fdb-dev  # exclusive; fails if someone holds it
scripts/agent-lane.sh lease release fdb-dev
```

A lane goes stale after 45 minutes without a heartbeat and is reaped
automatically, taking any lease it held with it. A lease that outlives its
holder is the failure mode that makes the next agent wait on nobody, so releases
are not left to good manners.

### What is automatic

`.claude/settings.json` wires four hooks through `scripts/agent-lane-hook.sh`:

| Hook | Does |
|---|---|
| `SessionStart` | registers the lane and injects the current lane table into context |
| `UserPromptSubmit` | heartbeats |
| `PreToolUse` on `Edit`/`Write`/`NotebookEdit` | if another live lane claims the path, asks before proceeding |
| `SessionEnd` | releases the lane and its leases |

The pre-edit hook returns `ask`, never `deny`, for the reason above: the edit is
safe, the merge is the question, and that judgement is not a hook's to make.

Every hook is best-effort and exits zero on any failure. A coordination ledger
that can block work is worse than no ledger.

**What is not automatic is the useful part.** The hook registers a lane with no
task and no paths, which tells a peer nothing. Declare them yourself once you
know what you are doing:

```
scripts/agent-lane.sh register --task "P4: bound witness bandwidth at 32 peers" \
  --paths crates/orrery_witness/,p1-swarm/,docs/03-replication.md
```

### Talking to another agent directly

Sessions on this machine can message each other natively — `ListAgents` to see
them, `SendMessage` to write to one by name. Use it when the ledger is not
enough: you need a decision from whoever holds a lease, you are about to change
an interface they are building against, or their lane says they are somewhere
you are heading.

Prefer the ledger for anything a peer can read at their own pace. A message
interrupts; a lane does not.

### Handing work to a subagent

Within one session, use the `Agent` tool and its worktree isolation rather than
inventing a protocol. Subagents inherit this repository's hooks, so a subagent
that edits into another agent's lane is caught by the same check.

### Codex delegation — suspended

The `cx` tool and Codex-to-Codex delegation are **out of credits and disabled**.
Do not route work to Codex, and do not add a fallback that tries. Revisit after
Thursday evening; until then this section is the whole story, and work that
would have been delegated is done here.

### Device-local memory

Durable, machine-local context lives in `.agents/memory/` — a symlink into the
shared store, git-ignored, never committed. Check its `INDEX.md` for notes on
decisions, project state, environment quirks, and open threads. Add or update
entries there (dated, one file per topic) rather than losing context between
sessions. Never store secrets in it.

Notes written there are now read by every agent on this machine, which is the
point, and worth a sentence of care: write what a peer would need, not what you
would need.
