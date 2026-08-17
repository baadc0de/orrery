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
(`scripts/core-gates.sh`), every `--self-test` mode in `scripts/`, the
workspace test suite, the standalone tools' own test suites, and the
cross-platform determinism matrix.

### Running it here: `scripts/check.sh`

Four of those jobs — `fmt`, `clippy`, `gates`, `test` — have no command bodies
of their own any more. They install a toolchain, apt packages and the
FoundationDB client, restore a cache, set the rustc wrapper, and then invoke
one lane of `scripts/check.sh`. So the way to find out whether a change passes
is to run it, not to push and wait:

```
./scripts/check.sh              # every lane, in CI's order
./scripts/check.sh clippy       # one lane, exactly the commands the job runs
./scripts/check.sh --list       # what a lane would run, without running it
./scripts/check.sh --self-test  # the lane table and the self-test coverage hold
./scripts/check.sh doctor       # delegates to dev-cache.sh: is the cache working?
```

**Scope the claim, because overclaiming it is how it stops being trusted.**
The script is the body of those four jobs and nothing else. `determinism` and
`determinism-verdict` keep four more cargo commands — a cross-platform matrix
is not something one machine can reproduce — and `nightly.yml` carries six
cargo invocations of its own plus the four gate scripts it runs *for real*,
including the two heavy harnesses that need an FDB cluster and eight peer
processes. Neither workflow is reproduced here.

**Measured, on the shared 16-thread box at `CARGO_BUILD_JOBS=3` with two other
agents building concurrently: 13 min 50 s** into a fresh worktree — empty
`target/` directories in all eight workspaces, warm cache. The second run,
fully warm and with nothing changed, was **78 s**: `fmt` 3 s, `clippy` 1 s,
`gates` 10 s, `test` 64 s. So the honest shape of it is that the first run in a
new worktree costs a quarter of an hour and every run after it costs a minute,
and `fmt` costs 3 s either way — run that one before every commit regardless.

Those figures were measured under the previous build cache (sccache), before the
2026-08-17 move to kache. The shape holds — a cold worktree is dominated by the
dependency graph and a warm one by `test` — but treat the absolute numbers as
stale until someone re-measures them.

### Eight workspaces, and only one of them is "the" workspace

`cargo test --workspace` reaches the root workspace. Each standalone tool
declares its own `[workspace]` table, so it reaches none of *them* — three red
CIs in one week came from that blind spot. The inventory, which is also
`scripts/check.sh`'s lane table:

| Workspace | Role in the lanes |
|---|---|
| `.` (root, 14 first-party crates + 3 vendored) | `clippy` and `test` lanes; `fmt` like any other. 1820 tests |
| `p1-swarm` | `cargo test` in `gates` — 43 tests |
| `p2-load` | `cargo test` in `gates` — 28 tests |
| `p2-dashboard` | `cargo test` in `gates` — 9 tests |
| `p4-streams-bench` | `cargo test` in `gates` — 7 tests |
| `p0-nat-test` | `cargo check --all-targets` — no tests |
| `p0-dashboard` | `cargo check --all-targets` — no tests |
| `p3-island` | `cargo check --all-targets` — no tests; asserted by the nightly island gate |

The four tool suites are 87 tests between them, which is the number that would
go unrun if the `gates` lane stopped visiting them.

`--self-test` compares that table against the filesystem — every directory
whose `Cargo.toml` declares `[workspace]` must appear in it — so a ninth
workspace cannot be added and silently go unchecked. It is a two-source check
by construction: the table cannot match itself.

**`fmt` is where this actually bit.** `cargo fmt --all` means "every member of
*this* workspace", so the root-only invocation the workflow used to run reached
zero of the 27 `.rs` files under the seven tools. Widening it found exactly one
dirty workspace, `p0-nat-test`. Note the flip side at the root: the three
vendored crates *are* root members, so `cargo fmt --all` holds `vendor/` to
default rustfmt even though clippy deliberately excludes it.

### A `--self-test` nothing runs is not a check

A second clause covers the gate scripts themselves.
`scripts/p4-accumulate.sh` and `scripts/p4-ledger.sh` each grew a `--self-test` that only `nightly.yml` ever
called, and the cost was not hypothetical: `p4-ledger.sh --self-test` counted
ledger lines with `wc -l`, BSD `wc` pads its output to a fixed width, and the
nightly's macOS leg failed on `[[ "       1" == 1 ]]` on every run until it was
found. No per-commit check invoked it, so nothing could have caught it earlier.

So `check.sh --self-test` now `find`s every script in `scripts/` that
*dispatches* on `--self-test` and asserts the `gates` lane invokes each one.
The two sides are the filesystem and the text of `lane_gates`, which is what
makes it a check rather than a restatement — a clause reading both out of one
list would pass on exactly the script nothing runs. Adding a gate script with a
self-test and forgetting to wire it up now fails the gate.

**Writing one that can fail.** Structural self-tests grep the script's own body
for the stages that make it a proof, and the failure mode is subtle: a pattern
that also appears somewhere harmless — a `${VAR:?usage}` message, an output
path template, another leg of the same harness — passes on a script that has
lost the stage entirely. The rule is to mutation-check each clause: break the
*guarded stage*, confirm the self-test fails, restore. Breaking the stage and
the check line together proves nothing, which is how vacuous clauses survive.

**`CARGO_TARGET_DIR` is never exported by the script.** An already-set value
always wins, and `--isolate` (per-lane directories, local use only) is opt-in.
Two reasons, both live: each pinned self-hosted runner keeps one warm `target/`
for its one job, and relocating it makes the first post-merge run cold on all
three; and an agent harness sets one per task, so an unconditional export would
collapse isolated lanes onto a single exclusively-locked directory — they would
queue, not merely share.

Five things about the workflow itself are worth knowing before you change
anything it touches.

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

One thing to know while you are in there: **`[workspace.lints]` still reaches
only the vendored crates.** `vendor/aeronet_iroh`, `vendor/aeronet_tokio_runtime`
and `vendor/bevy_replicon` are the only manifests with `[lints] workspace = true`,
so the `pedantic`/`nursery`/`missing_docs`/`unwrap_used` levels configured at the
workspace root apply to third-party code and to none of `crates/*` through that
mechanism. That is backwards, and adopting the table wholesale is still its own
piece of work.

But do not read it as "no lint levels apply to `crates/*`", because one of them
does and it bites. All fourteen first-party crates set
`#![warn(missing_docs)]` in their own `lib.rs`, and CI runs `clippy --workspace
--all-targets --no-deps -- -D warnings`, which promotes that warning to an
error. **An undocumented
public item fails CI today.** What is genuinely unadopted is the rest:
`pedantic`, `nursery` and `unwrap_used` have never been enforced on first-party
code, and turning them on is the large piece of work.

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

`scripts/core-gates.sh` scans `orrery_games` and `orrery_conformance`
alongside `orrery_core` — the determinism rules are about the rules code, so a
`HashMap` or a `SystemTime::now` inside a `Ruleset` fails the same gate it would
in the core. A fourth clause, scoped to the two ruleset crates, refuses a live
neighbour read: cross-entity effects travel as events, because the adjudicator
installs exactly one entity and a neighbour read is always `None` at replay.

**The rustc wrapper is cleared on GitHub-hosted runners, and deliberately not
on the self-hosted one.** `.cargo/config.toml` sets
`build.rustc-wrapper = "kache"` for local worktrees; the workflows set
`RUSTC_WRAPPER: ""` at the top because a GitHub-hosted runner is ephemeral and
has nothing to hit. The jobs that can land on `orrery-hel1-1` set it back to
`kache`, because that box keeps a persistent `target/` and a shared cache at
`/var/cache/kache/shared` that **both build identities publish to and restore
from** — same dependency graph, so a CI build starts warm off whatever was
compiled by hand in the dev checkout, and vice versa.

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
happen to satisfy and a bare box does not). And the jobs that need a
FoundationDB *server* stay on GitHub-hosted runners — `p2-kill9` and the fdb
test job — because provisioning a throwaway cluster means `sudo dpkg -i` on the
server package, which that user cannot do. The box does run an `fdbserver` — in
a Docker container, see [Working alongside other agents](#working-alongside-other-agents)
— and that is exactly the cluster those jobs must not be pointed at: it is a
shared development database, and both of them write into whatever cluster they
are given.

`p3-island` used to be pinned there for the same stated reason and never had
one — `scripts/p3-island-gate.sh` contains no FoundationDB reference at all,
binds every listener on `127.0.0.1:0` and runs persistd with
`--allow-volatile-leases` — so it now runs on the box with the other nightly
jobs.

The heavy harnesses — P2's kill-9 gate, which needs a real FoundationDB
cluster, and P3's island gate, which needs eight peer processes and a real
`kill -9` — cannot gate a pull request. They run nightly and on demand in
`.github/workflows/nightly.yml`, alongside a soak that repeats the corpus ten
times in one process to catch per-process nondeterminism.

**The `fdb` feature has its own nightly test job, and a wrapper script is the
reason it means anything.** `orrery_persistd` and `orrery_seed` carry a tier of
tests that only compile under `--features fdb`, and every one of them opens
with a guard that `eprintln!("skipping: …")` and returns `Ok` when it cannot
find a cluster. That guard is right for a developer's `cargo test` and a trap
for CI: `cargo test --features fdb` on a runner with no cluster is 27 passes
that assert nothing. So `scripts/fdb-tests.sh` runs them — both packages in one
invocation, per C-8 — captures stderr with `--nocapture`, fails on any
`skipping:` line, and asserts a floor on how many tests actually executed. Its
`--self-test` proves those assertions against six synthetic logs and runs
per-commit, alongside every other self-test in `scripts/`.

**The standalone tools are tested per-commit too.** Each declares its own
`[workspace]`, so `cargo test --workspace` reaches none of them; the `gates`
lane runs `cargo test` in `p1-swarm`, `p2-load`, `p2-dashboard` and
`p4-streams-bench`, and `cargo check --all-targets` in the three that have no
tests at all — see the inventory above, which is the table the lane iterates.
`p2-load` takes `orrery_persistd` with `features = ["fdb"]`, which is why that
job installs the FoundationDB *client* on the hosted path.

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
| `build.rustc-wrapper = "kache"` | `.cargo/config.toml` | yes — worktrees each get a copy of tracked files, so this is the only way a setting reaches all of them |
| `build.incremental = false` | `.cargo/config.toml` | yes |
| local store size cap, shared remote | `~/.config/kache/config.toml` | no — machine-local, one per build identity |
| the cache daemon | `kache@<user>.service` (systemd) | no — machine-local |

The standalone tools (`p2-load`, `p3-island`, `p0-*`) each declare their own
`[workspace]`, so each has its own `target/`. They still inherit the repo's
`.cargo/config.toml`, because cargo walks up from the working directory — do
not add a per-tool `.cargo/config.toml`, which would shadow it and silently
drop that tool back to uncached builds.

**Incremental compilation is off deliberately, and the two reasons compound.**
An incremental unit is not cacheable by a plain rustc wrapper, so
leaving it on would defeat the cache for exactly the crates being worked on,
while still writing the artifacts to disk. If you are a human tight-looping
edits on one crate, that trade is not in your favour: use `CARGO_INCREMENTAL=1`
for that session, which overrides the file.

### This makes kache a build prerequisite

If it is missing, install it (see
[kache](https://github.com/kunobi-ninja/kache)) or opt out for one command with
an empty wrapper, which takes precedence over the config file:

```
RUSTC_WRAPPER= cargo build
```

### How it is set up on this box

Two build identities compile here: the dev user (you, and every agent worktree)
and `ci`, which the three GitHub Actions runners run as. They share one cache
through a content-addressed directory:

| Piece | Where |
|---|---|
| shared cache | `/var/cache/kache/shared`, group `kache`, `2775` + a default ACL granting the group `rwx` |
| local store | `~/.cache/kache` per identity, capped at 25 GiB |
| daemon | `kache@<user>.service`, a systemd **system** unit, one instance per identity |
| shared-cache pruning | `kache-prune-shared.timer`, daily |

`cache.local_max_size` is the size cap — **not** `max_size`, which kache ignores
silently, leaving you on the 50 GiB default while your config claims otherwise.
`cache.auto_gc` is on by default and enforces the cap opportunistically, so the
local stores look after themselves.

The shared remote does not: `kache gc` evicts the local stores only, so a
filesystem remote grows without bound. `kache-prune-shared.timer` drops objects
untouched for 21 days. Deleting them is always safe — they are content-addressed
and immutable, so a pruned object is a cache miss and nothing worse.

The default ACL is the part worth understanding: it makes every new object
group-writable **regardless of the writing process's umask**. Without it a
runner with `umask 022` would publish objects the dev user could not overwrite,
and the sharing would rot silently in one direction.

A systemd *system* unit rather than kache's own `kache daemon install`, which
writes a **user** unit: a user unit needs lingering and a D-Bus session, and
`ci` is a service account with neither.

Two `kache doctor` checks are expected to fail here and are not problems: it
reports the daemon service as "not installed" because it only recognises its own
user unit, and it counts daemon processes machine-wide rather than per uid, so
it sees the other identity's daemon and reports one too many.

**Why not sccache**, since the repo used it until 2026-08-17.

The intermittent CI failures — `Connection reset by peer (os error 104)`,
`Failed to read response header` — had a specific cause, and it was not cache
corruption. All three runners run as `ci` and shared **one** sccache server.
Whichever job's client spawned it owned that process inside *that runner's*
process tree, so when that job finished, the runner's cleanup killed it —
`Terminate orphan process: pid (N) (sccache)` — and every in-flight compile in
the other two jobs died with it. Across 15 retrieved failures the correlation is
exact: each one begins 15–50 ms after a *different* job on a *different* runner
logged that kill. (PR #52's `SCCACHE_IGNORE_SERVER_IO_ERROR=1` did not help; one
of the 15 failed fatally with it set.)

kache is immune for two independent reasons. Its daemon is a systemd system
unit in `system.slice`, not a descendant of any job, so a runner's orphan reaper
never sees it. And because kache compiles **in the invoking process**, a dead
daemon costs remote lookups, not builds: with `kache@baadc0de` stopped
outright, a full rebuild of `p3-island` from an empty `target/` took 3.45 s
against 3.28 s with it running.

Two further sccache problems made the shared cache worth leaving anyway. It runs
the compiler inside its server, so the server's uid owns the output objects and
it panics outright if it cannot stat the calling user's toolchain — which it
cannot, because `/home/<user>` is `0750`. And it writes cache entries `0600`, so
`/var/cache/sccache` was never actually shared: 20,520 entries readable only by
the dev user, 4,965 only by `ci`, each server's LRU evicting files it could not
read. Upstream documents that arrangement as unsupported — *"The local storage
only supports a single sccache server at a time. Multiple concurrent servers
will race and cause spurious build failures."*

### Working with it

```
./scripts/dev-cache.sh doctor   # is the cache wired up and actually taking effect?
./scripts/dev-cache.sh stats    # hit rate, cache size
./scripts/dev-cache.sh disk     # what every target/ in this checkout costs
./scripts/dev-cache.sh prune    # delete every target/ — safe, and meant to be used
```

`prune` is the lever to pull when disk gets tight, and pulling it is cheap:
sources are in git and the rebuild refills from the cache. Measured 2026-08-17
on the `p3-island` tool, deleting its whole `target/` and rebuilding: **25 s
with a cold cache, 3.3 s warm** (330 cache hits). The residual is linking and
cargo's own bookkeeping, which no object cache can remove. For comparison, the
same measurement under sccache was 21 s cold and **14 s** warm — the warm case
is where the difference shows, because sccache's entries were unreadable across
build identities and a warm cache was warm for one user only.

Two things follow for agents sharing this machine:

- **Prune freely, and prune your own worktree before a long build.** You are not
  destroying anyone's work; you are dropping a derived artifact that another
  agent's build already paid to compute.
- **Do not read a less-than-100% hit rate as breakage.** Linking, `build.rs`
  executions and a few binary crate-type units are not cacheable by design, and
  the crate you are actively editing is *supposed* to miss. `kache why-miss
  <crate>` explains any individual miss, which beats guessing.
- **A miss on the crate you are editing is not a miss on its dependencies.**
  The dependency graph is the part the shared cache pays for, and it is the
  overwhelming majority of a cold build.

## Working alongside other agents

Several agents work this repository at once, each in its own git worktree. The
worktrees isolate the filesystem and nothing else: there is one `.git`, one
build cache, one disk, one FoundationDB dev cluster, one set of harness ports,
one GitHub remote. Everything below exists because of that asymmetry.

**Be clear about what a collision actually is.** Two agents editing the same
file in two worktrees do *not* clobber each other — separate checkouts, separate
inodes, and neither can see the other's buffer. What they produce is a merge
conflict, discovered later and further from the decision that caused it. That is
worth knowing about in advance, but it is not a reason to stop. The things that
genuinely cannot be shared are elsewhere, and they are the ones worth blocking
on: the `.fdb-dev/` cluster, a harness's fixed ports, `git push` and branch
deletion, `git worktree add/remove`, and the disk itself.

The `.fdb-dev/` cluster is on that list because agents share *one* of it, and on
this machine there is no second one to stand up on a whim.
`scripts/fdb-dev.sh` is written as if there were: `ORRERY_FDB_DEV_PORT`,
`ORRERY_FDB_DEV_DIR`, the cluster description, the memory sizes and the
`FDBSERVER` path all come from the environment, and an instance is identified by
its data directory rather than by its port, so `stop` can never reach an
instance it did not start. **But `start` cannot run here: there is no
`fdbserver` binary on this box.** `foundationdb-clients` is installed — that is
`fdbcli`, `fdbbackup` and `libfdb_c`, which is what the builds and
`foundationdb-sys`'s bindgen actually need — and the *server* package is not.
The `fdbserver` you can see in `ps` is root-owned and lives in a container.

The dev cluster is that container: **`orrery-fdb`, image
`foundationdb/foundationdb:7.3.63`, host networking, serving `127.0.0.1:4500`,
with the main checkout's `.fdb-dev/data` bind-mounted at `/var/fdb/data`.** So
the route to it is its cluster file, not the script — and that file lives in the
*main checkout*, which is why a worktree cannot find it by looking around:

```
export ORRERY_FDB_CLUSTER_FILE="$(git rev-parse --path-format=absolute --git-common-dir)/../.fdb-dev/fdb.cluster"
fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'status minimal'   # docker:docker@127.0.0.1:4500
```

Set it explicitly, and do not rely on the fallback: most fdb-gated tests look
for a `.fdb-dev/fdb.cluster` by walking up from the crate directory, a walk that
finds nothing from a worktree. The tests then `eprintln!("skipping: …")` and
pass — green assertions about nothing, which is exactly the trap
`scripts/fdb-tests.sh` exists to close. That script refuses to default the
variable at all.

**Never `stop`, `reset` or `pkill` any of it.** One container serves every agent
on this box and the tests' default fallback, and it is a *shared development
database*: whatever you write stays. Take the `fdb-dev` lease before you write
to it. If you need a cluster you can clobber, run a second container on another
port and point `ORRERY_FDB_CLUSTER_FILE` at its cluster file — an agent running
its own instance needs no lease, and should not take one.

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

The `fdb-dev` lease is about the *default* instance — the one at `.fdb-dev/` on
port 4500 that every suite falls back to. An agent running its own instance on
its own port needs no lease, and should not take one.

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
