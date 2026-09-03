# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

**Orrery** is an in-development set of Rust crates for the Bevy game engine (0.19):
peer-to-peer multiplayer (QUIC transport with NAT hole punching via iroh) and a
persistent-universe backend. The repository contains the accepted architecture,
active P0–P4 implementation — P4 landed as construction and open as
*measurement*, with witnessing in shadow mode — test tools, and incomplete
milestone harnesses.

## Reading path (normative order)

The design is documented in `docs/`. **Accepted ADRs are normative** over the
README and every numbered expansion document.

Start with [docs/DECISIONS.md](docs/DECISIONS.md) — the ADR index and
governance entry point. The records live under [docs/adr/](docs/adr/).

- For architecture-wide work, read all accepted ADRs in numeric order.
- For scoped work, read the index, the ADRs named by the relevant expansion
  document, and any ADR dependencies those records link.
- Never treat the index summary as a substitute for the applicable ADR text.
- A change to an accepted decision is a **new ADR that explicitly supersedes**
  the old one. Do not silently rewrite architectural history.

This file deliberately does **not** list the ADRs. It used to, and the copy
went stale at D30 while the tree reached D50 — an agent reading it would have
concluded the later records did not exist. `docs/DECISIONS.md` is the index;
there is one of them.

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
| 13 | [docs/12-world-seeding.md](docs/12-world-seeding.md) | World seeder: TOML scenario runner, generator bank, content diff/patch |
| 14 | [docs/13-chain-replication.md](docs/13-chain-replication.md) | Cross-process journal mirroring, reconnect, and recovery |
| 15 | [docs/14-capacity.md](docs/14-capacity.md) | Measured single-box capacity envelope |
| 16 | [docs/references.md](docs/references.md) | Annotated bibliography, organized by topic |

Working in this repository, rather than reading about it:

| Document | Covers |
|---|---|
| [docs/ci-and-gates.md](docs/ci-and-gates.md) | What CI runs, the twelve workspaces, self-test clauses, the determinism matrix, `gate-status.sh`, and the hosted measurements |
| [docs/build-cache.md](docs/build-cache.md) | `kache`, target directories, and why `CARGO_TARGET_DIR` is never exported |
| [docs/agent-lanes.md](docs/agent-lanes.md) | Lane registration, leases, the hooks, and delegating to codex or opencode |

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


## CI, and the one rule that matters most

`.github/workflows/ci.yml` runs on every push and pull request: `rustfmt`,
`clippy -D warnings`, the verifiable-core static gates (`scripts/core-gates.sh`),
every `--self-test` mode in `scripts/`, the workspace test suite, the standalone
tools' own suites, and the cross-platform determinism matrix.

Four of those jobs — `fmt`, `clippy`, `gates`, `test` — have no command bodies
of their own. They invoke one lane of `scripts/check.sh`, so the way to find out
whether a change passes is to run it, not to push and wait:

```
./scripts/check.sh              # every lane, in CI's order
./scripts/check.sh clippy       # one lane, exactly the commands the job runs
./scripts/check.sh --list       # what a lane would run, without running it
./scripts/check.sh --self-test  # the lane table and the self-test coverage hold
./scripts/check.sh doctor       # delegates to dev-cache.sh: is the cache working?
```

The `clippy` lane also carries the Windows cross-check (`cargo check -p
orrery_ipc_transport --target x86_64-pc-windows-gnu`, #1020): `#[cfg(windows)]`
code compiled nowhere else per commit. Without the target installed the stage
skips with a NOTE and the run still passes; `rustup target add
x86_64-pc-windows-gnu` enables it.

### The push is the gate

**`./scripts/check.sh` is always run before pushing a branch for a pull
request.** Cutting commits locally without it is fine — the gate is the push,
not the commit, and not the merge.

The only exception is a documentation chore: prose, a plan, an ADR update.
Those need no lane.

This is a rule rather than a habit because **two of pull-request CI's checks do
not exist**. `static gates` and `workspace tests` moved to nightly on
2026-08-28 to cut roughly twenty minutes of merge latency, and they are the only
lanes that build `clients/regolith` and the standalone gate workspaces. **A pull
request can be green on every required check while `main` does not compile.**

That happened twice on 2026-08-30: once from a targeted `cargo test -p` standing
in for the script (#718, fixed by #719), and once structurally, when adding a
public field to a struct in `crates/orrery_games` broke a consumer in
`clients/regolith` that the writing lane was correctly scoped away from and
could not have seen (#728, fixed by #729).

Read the script's final line. `check: fmt clippy gates test passed` is the
claim; anything else is not.

**Scope the claim, because overclaiming it is how it stops being trusted.** The
script is the body of those four jobs and nothing else — not `determinism`, not
`nightly.yml`.

### Thirteen workspaces, and only one of them is "the" workspace

`cargo test --workspace` reaches the root workspace only. Each standalone tool
declares its own `[workspace]` table, so it reaches none of *them* — three red
CIs in one week came from that blind spot, and `cargo fmt --all` had the same
hole. `./scripts/check.sh --list` is the executable inventory; do not hand-copy
it. `--self-test` compares the lane table against the filesystem, so a
fourteenth workspace cannot be added and silently go unchecked.

Everything else about CI — the full workspace table, the self-test clauses, the
determinism matrix, `gate-status.sh`, and the hosted timing and disk
measurements — is in [docs/ci-and-gates.md](docs/ci-and-gates.md).

## Build cache

`kache` is a build prerequisite: `.cargo/config.toml` routes rustc through it,
and cargo treats a missing wrapper as a hard error rather than a fallback. The
script never exports `CARGO_TARGET_DIR`; an already-set value always wins, and
per-lane isolation is opt-in via `--isolate`.

`target/` directories are the disk problem on this box, not the cache: 202 GiB
across fifteen of them, and nothing ever reclaimed one until #781.
`scripts/dev-cache.sh prune` is the deliberate manual lever; `reclaim` is the
automatic one, and it runs on session end. It deletes an agent worktree that
`git worktree list` no longer knows about, and the `target/` of one whose branch
has landed — but only when no build process is rooted in the tree, resolved
through `/proc/<pid>/cwd`. Run `./scripts/dev-cache.sh reclaim` to see what it
would take without taking it.

Details, including what exists on the developer box and in CI, are in
[docs/build-cache.md](docs/build-cache.md).

## Working alongside other agents

Several agents work this repository at once, each in its own git worktree. The
worktrees isolate the filesystem and **nothing else**: one `.git`, one build
cache, one disk, one FoundationDB dev cluster, one set of harness ports, one
GitHub remote.

- **Before editing, run `scripts/agent-lane.sh check <path>`.** If another live
  lane claims it, coordinate rather than proceed.
- **Take a lease before a shared resource** — the FDB cluster, harness ports, a
  push.
- **A lane owns files, not intentions.** Two lanes must never share a file.

The hooks register a lane, heartbeat it, warn on a claimed path, and release it
at session end. What is *not* automatic is the useful part: naming the task, and
taking leases. Mechanics and delegation to codex or opencode are in
[docs/agent-lanes.md](docs/agent-lanes.md).
