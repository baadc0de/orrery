# Build cache and target directories

> Split out of `AGENTS.md` on 2026-08-30 to keep that file readable. It is operational detail an agent needs once, not on every read.
> It is the same text, relocated; `AGENTS.md` keeps the rules and points here
> for the reasoning, the measurements and the incidents behind them.

## Build cache and target directories

Agents work in parallel git worktrees, and a Rust `target/` is enormous. Every
worktree keeps its own `target/`; kache stores compiled objects separately.
Do not turn this into a shared `CARGO_TARGET_DIR`.

Sharing a `CARGO_TARGET_DIR` instead would look tempting and be wrong — cargo
takes an exclusive lock on a target directory, so two agents building at once
would serialize, one waiting on the other for the whole build. The local object
cache has no such contention: identical `rustc` invocations can be reused by
worktrees run by the same user.

### What is configured, and where

| Setting | Location | Committed? |
|---|---|---|
| `build.rustc-wrapper = "kache"` | `.cargo/config.toml` | yes — worktrees each get a copy of tracked files, so this is the only way a setting reaches all of them |
| `build.incremental = false` | `.cargo/config.toml` | yes |
| developer kache store | `~/.cache/kache` | no — local to that Unix user; it has no shared remote on this workstation |
| CI remote | `clippy`'s `kunobi-ninja/kache-action` inputs and its job environment | yes — `s3-bucket: orrery-kache`, `s3-region: eu-central-1`, `s3-prefix: artifacts`, and a 20 GiB local cap |

The standalone tools (`gates/p2-load`, `gates/p3-island`, `p0-*`) each declare their own
`[workspace]`, so each has its own `target/`. They still inherit the repo's
`.cargo/config.toml`, because cargo walks up from the working directory — do
not add a per-tool `.cargo/config.toml`, which would shadow it and silently
drop that tool back to uncached builds.

**Incremental compilation is off deliberately.** An incremental unit is not
cacheable by a plain rustc wrapper, so leaving it on writes artifacts while
defeating the cache for the crate being edited. For a local tight edit loop,
`CARGO_INCREMENTAL=1` overrides the file for that command.

### This makes kache a build prerequisite

If it is missing, install it (see
[kache](https://github.com/kunobi-ninja/kache)) or opt out for one command with
an empty wrapper, which takes precedence over the config file:

```
RUSTC_WRAPPER= cargo build
```

### What exists on the developer box and CI

**`fortyninety` is deliberately local-only.** `.cargo/config.toml` routes
rustc through kache and its store is the invoking user's `~/.cache/kache`, but
the shared S3 bucket is not configured there. As verified on 2026-08-23,
`kache doctor` ends with:

```
All checks passed.
Daemon checks are informational: no remote cache or planner configured (the daemon is optional for local-only use).
```

That is expected, not an instruction to add AWS credentials or a remote to a
developer machine. `./scripts/dev-cache.sh doctor`, `stats`, and `disk` report
the local arrangement; they cannot validate CI's ephemeral S3 credentials.

**The configured machine is the CI `clippy` runner.** For a same-repository
run, that job assumes `orrery-ci-cache` with GitHub OIDC, then
`kunobi-ninja/kache-action` installs kache 0.14.2 and exports the S3 settings.
The 2026-08-21 CI run printed this resolved remote:

```
✓ Remote          s3://orrery-kache/artifacts
✗ Daemon service  not installed
                  → kache daemon install

1 issue(s) found.
```

The daemon-service line is informational on an ephemeral runner; the workflow
asserts the exact `s3://orrery-kache/artifacts` line, rather than treating
`kache doctor`'s exit status as the remote check. Fork pull requests do not
receive an OIDC token and run cold. `fmt`, `gates`, `test`, the determinism
matrix, and nightly jobs also use the workflow-level empty `RUSTC_WRAPPER` or
their own `actions/cache` setup unless a job explicitly installs kache. In
particular, `gates` and `test` have no S3 kache remote by a measured decision,
not an omission.

There are no self-hosted runners: `gh api repos/baadc0de/orrery/actions/runners
--paginate --jq '.total_count'` returned `0` on 2026-08-23. Do not administer
the retired `orrery-hel1-1` runner services or rely on its cache paths.

**Remote retention lives in S3, not on a host.**
`infra/s3.tf` provisions the private `orrery-kache` bucket with all public
access blocked and AES256 SSE-S3, then its
`aws_s3_bucket_lifecycle_configuration.kache` expires the `artifacts/` prefix
at `var.cache_expiration_days`. The current value is 14 days since object
creation, so the bound is `unique bytes uploaded per day × 14`; reads cannot
extend it. A second, bucket-wide rule aborts incomplete multipart uploads one
day after initiation (`var.multipart_abort_days`). There is no bespoke pruning
timer anywhere in this arrangement. Adjusting the retention window means
changing and applying the Terraform variable after measuring upload rate; it
does not mean adding a cron job.

### Working with it

```
./scripts/dev-cache.sh doctor   # is the cache wired up and actually taking effect?
./scripts/dev-cache.sh stats    # hit rate, cache size
./scripts/dev-cache.sh disk     # what every target/ in this checkout costs
./scripts/dev-cache.sh prune    # delete every target/ — safe, and meant to be used
```

`prune` is the lever to pull when disk gets tight: sources are in git and it
removes only this checkout's derived `target/` directories. The developer's
local store may accelerate the rebuild; the CI S3 remote is configured only by
the CI job, so it is not a local fallback.

Two things follow for agents sharing this machine:

- **Prune freely, and prune your own worktree before a long build.** You are not
  destroying anyone's work; you are dropping a derived artifact. The local
  store may accelerate rebuilding it, but this box has no remote fallback.
- **Do not read a less-than-100% hit rate as breakage.** Linking, `build.rs`
  executions and a few binary crate-type units are not cacheable by design, and
  the crate you are actively editing is *supposed* to miss. `kache why-miss
  <crate>` explains any individual miss, which beats guessing.
- **A miss on the crate you are editing is not a miss on its dependencies.**
  The dependency graph is the part kache can pay for, and it is the overwhelming
  majority of a cold build.


### Reclaiming dead lanes (#781)

`prune` is the deliberate, manual lever and stays exactly as it is. It is also
*only accidentally safe*: it deletes every `target/` its `find -maxdepth 3`
returns, with no liveness check at all, and the worktree targets it would
otherwise delete out from under a running build sit at depth 4. Widening that
depth would be a disk-space fix that corrupts four concurrent builds.

`reclaim` is the automatic half, and it earns the wider scope rather than
inheriting the accident:

```console
$ ./scripts/dev-cache.sh reclaim              # report only
$ ./scripts/dev-cache.sh reclaim --apply      # and delete
```

It looks only at `.claude/worktrees/`, and every tree passes two independent
gates before anything is deleted. Both fail closed: anything unreadable,
unresolvable or unfetchable is reported and kept.

- **Liveness.** No `cargo`/`rustc`/`kache`/linker process has its working
  directory inside the tree, and no process at all holds an open descriptor
  under its `target/` (cargo keeps `target/*/.cargo-lock` open for the whole of
  a build, which catches helpers this script has never heard of). The working
  directory is read from `/proc/<pid>/cwd`. It is deliberately **not** a grep of
  the command line: `pgrep -f <path>` matches a string in `argv`, which says
  nothing about where a process is running, and a build in tree A whose argv
  happens to name tree B is attributed to B and missed in A.
  Process liveness is the signal — a finished lane and a stalled one have
  identical file counts and identical mtimes.
- **Content on main.** Deliberately **not** `git merge-base --is-ancestor`. PRs
  here are squash-merged, so a lane's own commits are never ancestors of `main`
  and an ancestry test reports every landed lane as unmerged — a reclaimer that
  reclaims nothing. Instead the files the branch changed are compared against a
  freshly fetched `main` by content. The question is asked **from the primary
  checkout**, never from inside the worktree: an orphaned worktree's own
  `origin` can point at a local filesystem path, so its `origin/main` is stale
  and `git log origin/main..HEAD` run there reports landed work as unlanded.

What it then does depends on what the tree is:

| tree | reclaimed |
|---|---|
| **orphan** — a directory under `.claude/worktrees/` that `git worktree list` does not know about | the whole directory. No lane can build there again, and `git worktree remove` refuses it, which is why both of #781's 100 GiB incidents survived. |
| **registered** — a live worktree whose branch content has landed | its `target/` only. The checkout belongs to an agent and is not this script's to remove. |

A registered worktree with a warm heartbeat in the agent-lane ledger is also
left alone — not a safety gate (liveness is), but deleting a live agent's
`target/` between two of its builds costs it a cold rebuild for nothing.

**The trigger is `SessionEnd`**, via `scripts/agent-lane-hook.sh`, which invokes
the *primary* checkout's copy detached from the hook's 10 s budget. A lane whose
PR has landed no longer needs its target, and session end is when a lane stops
needing its outputs. It is not a systemd timer: a timer fires on wall-clock
time, which is unrelated to whether anything landed, and it would have to live
outside this repository as a user unit nobody clones.

Every deletion appends a line to `<git-common-dir>/dev-cache-reclaim.log`.
`./scripts/dev-cache.sh --self-test` pins both gates functionally, in both
directions, and runs per commit in `check.sh gates`.
