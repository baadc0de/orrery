# Working alongside other agents

> Split out of `AGENTS.md` on 2026-08-30 to keep that file readable. The lane rules stay in AGENTS.md; the mechanics live here.
> It is the same text, relocated; `AGENTS.md` keeps the rules and points here
> for the reasoning, the measurements and the incidents behind them.

## Working alongside other agents

Several agents work this repository at once, each in its own git worktree. The
worktrees isolate the filesystem and nothing else: there is one `.git`, one
developer-local build cache, one disk, one FoundationDB dev cluster, one set of
harness ports, and one GitHub remote. CI's S3 cache is separate infrastructure,
configured only by the CI job; it is not shared state on this workstation.
Everything below exists because of the remaining local asymmetry.

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
instance it did not start.

**What exists where, since the 2026-08-22 decision on #176: gates provision
FoundationDB per run and discard it with the runner, and there is no long-running
reference cluster anywhere.** The composite action
[`.github/actions/foundationdb`](.github/actions/foundationdb/action.yml)
installs the client always and, under `server: "true"`, a throwaway single-node
cluster whose server package self-configures `/etc/foundationdb/fdb.cluster`;
that is how `nightly.yml`'s four FDB jobs named above get theirs. What persists
between runs is local convenience only: this workstation (`fortyninety`) keeps a
native `fdbserver` — the `.fdb-dev/` instance described next. The Docker
container that used to be the reference cluster on `orrery-hel1-1` was retired
by that decision; when last checked (2026-08-22) it was still listening there,
awaiting teardown —

```
ssh orrery-hel1-1.distopik.com docker ps -a | grep -i fdb   # still up? a leftover, not a reference
```

— and nothing new may point at it.

**`start` does work here.** An earlier revision of this section said it could
not — that there was no `fdbserver` binary, only `foundationdb-clients`, and
that the process in `ps` was root-owned and lived in a container. All three
claims are wrong, and they were repeated into agent briefings for a day before
anyone checked. Verify for yourself rather than trusting either version:

```
hostname                            # fortyninety — these answers describe THIS box
which fdbserver                     # /usr/bin/fdbserver — the server package IS installed
ss -lntp | grep 4500                # served by fdbserver, not a container
ps -o user= -p <that pid>           # owned by the dev user, not root
docker ps | grep -i fdb             # no fdb container exists
```

So an agent needing a cluster it can clobber should start its **own
`fdbserver`** on a non-default port with its own data directory — which is
exactly what `fdb-dev.sh` is parameterised for — rather than standing up a
container. The shared instance on `127.0.0.1:4500` is still shared: take the
`fdb-dev` lease before writing to it, and never `stop`, `reset` or `pkill` it.

The shared dev cluster serves `127.0.0.1:4500`, with its data and cluster file
under the *main* checkout's `.fdb-dev/`. Tools that look beside their own
checkout — `scripts/fdb-dev.sh`'s `$ROOT/.fdb-dev` default, for one — do not
find it from a worktree, so the route to it is its cluster file:

```
export ORRERY_FDB_CLUSTER_FILE="$(git rev-parse --path-format=absolute --git-common-dir)/../.fdb-dev/fdb.cluster"
fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'status minimal'   # dev:test@127.0.0.1:4500
```

Set it explicitly, and do not rely on the fallback: the fdb-gated tests discover
their cluster by walking up from the crate directory looking for any
`.fdb-dev/fdb.cluster`. From a worktree under `.claude/worktrees/` that walk
climbs out of the worktree and lands in the main checkout — an unset variable
does not fail safe here, it quietly aims the run at the shared instance below.
From a checkout with no `.fdb-dev` above it, the other failure mode fires: the
tests `eprintln!("skipping: …")` and pass — green assertions about nothing,
which is exactly the trap `scripts/fdb-tests.sh` exists to close. That script
refuses to default the variable at all.

**Never `stop`, `reset` or `pkill` any of it.** One native `fdbserver` serves
every agent on this box and the tests' default fallback, and it is a *shared
development database*: whatever you write stays. Take the `fdb-dev` lease before
you write to it. If you need a cluster you can clobber, start your own instance
on another port **and in its own directory** — the directory does not follow
the port, and a second instance pointed at the shared data dir is not isolated —

```
ORRERY_FDB_DEV_PORT=4501 ORRERY_FDB_DEV_DIR=/tmp/opencode/fdb-4501 \
  scripts/fdb-dev.sh start        # dev4501:test4501@127.0.0.1:4501, verified 2026-08-22
```

— then point `ORRERY_FDB_CLUSTER_FILE` at `/tmp/opencode/fdb-4501/fdb.cluster`.
An agent running its own instance needs no lease, and should not take one;
`scripts/fdb-dev.sh stop` with the same two variables tears it down and cannot
reach any other instance.

So this arrangement is deliberately two-speed: **lanes are advisory, leases are
exclusive.**

### Never mutate `~/.cargo/registry` (2026-08-31)

The extracted crate sources under
`~/.cargo/registry/src/index.crates.io-*/` are shared by **every build on the
machine** and are invisible to `git status` in every worktree. A change there
silently alters what every concurrent lane compiles, and nothing in this
repository will tell you.

This is not hypothetical. On 2026-08-31 a lane needed to break a dependency to
demonstrate a mutation check and edited
`lightyear_prediction-0.29.0/src/rollback.rs` in place, injecting a
`type_name::<C>().ends_with("Vitality")` special case. It was left there. A
registry-wide audit — every extracted crate, compared against its own
`.cargo-ok` extraction timestamp — found exactly one modified file, which was
restored byte-identical from the cached `.crate` tarball. A `check.sh` had been
running through the window, and was restarted for that reason.

**To mutate a dependency, copy it and patch the copy:**

```bash
cp -r ~/.cargo/registry/src/index.crates.io-*/<crate>-<ver> /tmp/mycopy
chmod -R u+w /tmp/mycopy
# add to the workspace Cargo.toml, then revert when done:
#   [patch.crates-io]
#   <crate> = { path = "/tmp/mycopy" }
```

That is contained, visible in `git diff`, and cannot escape the worktree.

**To audit**, if you suspect the registry has been touched:

```bash
R=~/.cargo/registry/src/index.crates.io-*/
for d in $R*/; do ok="$d.cargo-ok"; [ -f "$ok" ] || continue
  find "$d" -type f ! -name '.cargo-ok' -newer "$ok"; done
```

Any output is a file written after its crate was extracted. Restore from
`~/.cargo/registry/cache/index.crates.io-*/<crate>-<ver>.crate`.

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
scripts/agent-lane.sh register --task "..." --paths crates/orrery_witness/,gates/p1-swarm/
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
  --paths crates/orrery_witness/,gates/p1-swarm/,docs/03-replication.md
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

### Codex delegation — live again (2026-08-20)

The weekly quota reset, so routing work to Codex is back on. With opencode
(below) there are now **three** providers with independent quotas, so a wide
fan-out should be **level-loaded** across all of them rather than queued entirely
on one — no single provider's limit is then a hard stop on the whole queue.

Rough division by what each is actually good at here, rather than round-robin:
**Claude** for judgement against an unbuilt design, and for anything that must
commit, push or open a PR; **Codex** for well-specified crate work it can build
and verify, remembering it cannot write to `.git`; **opencode** for read-heavy
investigation and precise citation.

The binary is `codex` (`/usr/bin/codex`). **There is no `cx` wrapper**; earlier
notes naming one are stale. Auth is a ChatGPT account (`codex login status`).

| Model | Use |
|---|---|
| `gpt-5.6-terra` | General coding. Also the configured default in `~/.codex/config.toml`, so a bare `codex exec` already uses it. |
| `gpt-5.6-sol` | Demanding frontier work. |

**Pass the full `gpt-5.6-*` id.** The bare names `terra` and `sol` are rejected —
`The 'terra' model is not supported when using Codex with a ChatGPT account` —
behind a `Model metadata for 'terra' not found` warning that looks like the cause
and is not. The account is fine; the id is wrong.

```
codex exec -m gpt-5.6-sol -s workspace-write -C <dir> "<prompt>"
```

`-s` is `read-only`, `workspace-write` or `danger-full-access`; add `--json` for
JSONL events and `-o <file>` to capture the final message.

**One caveat that matters here: a Codex agent does not inherit this repository's
hooks.** The `SessionStart` lane registration and the `PreToolUse` collision check
in `.claude/settings.json` do not run for it, so it is invisible to
[the lane ledger](#working-alongside-other-agents) unless someone registers it, and
nothing warns it when it edits into another agent's paths. Register its lane on its
behalf, or give it paths that overlap nobody.

### opencode delegation (2026-08-21)

A third provider, alongside Claude and Codex, and free at time of writing. The
binary is `opencode` (`~/.opencode/bin/opencode`, v1.18.20). It needs **no
credentials** — `~/.local/share/opencode/auth.json` is empty and the
`opencode/*-free` models run anyway. `opencode models` lists what is available;
`opencode/x-preview-f-free` is the capable one.

**It is very good at reading code and citing it.** Across four tasks it produced
six `file:line` citations that were checked against the source and every one was
exact, including a correction nobody asked for: `AGENTS.md`'s claim that
`max_size` is silently ignored is true *as a TOML key*, but `KACHE_MAX_SIZE`
does work as an environment variable. Route investigation, grooming and
review-style work to it.

#### The three traps, all of which look like a broken model

Every one of these was hit here before the tool worked, and each cost real time:

```
opencode.jsonc      an explicit permission block, per project
--format json       structured events on stdout
nohup + patience    it does NOT stream; output appears at exit
```

1. **The default renderer needs a TTY.** Redirect it and you get zero bytes.
   Always pass `--format json`, then filter with
   `jq -r 'select(.type=="text")|.part.text'`.
2. **Tool permissions default to asking, and a non-interactive run auto-rejects.**
   The symptom is a silent stall, not an error. `--auto` works but is a blanket
   grant (`auto-approve permissions that are not explicitly denied
   (dangerous!)`); prefer a project `opencode.jsonc` with an explicit block,
   which is scoped and reviewable:

   ```jsonc
   { "$schema": "https://opencode.ai/config.json",
     "permission": { "bash": "allow", "edit": "allow", "read": "allow",
                     "glob": "allow", "grep": "allow", "list": "allow",
                     "lsp": "allow", "task": "allow", "todowrite": "allow",
                     "external_directory": "allow" } }
   ```

   Every key takes `ask`, `allow` or `deny`; the full list is in the schema.
   Note `external_directory` — without it, reading anything outside the project
   (a vendored crate under `~/.cargo/registry`, say) stalls silently.
3. **It buffers output and writes at exit.** A run killed before it finishes
   produces *nothing*, which is indistinguishable from a hang. Multi-step turns
   are slow — a tool call returns quickly but the follow-up model step was
   measured at ~55 s — so a real task runs for many minutes. Launch it detached
   (`nohup … &`) with a generous timeout and read the file afterwards. **Do not
   conclude it has hung because the log is empty.**

That third trap produced three separate wrong diagnoses here, including a
"bootstrap hangs on this repository" conclusion that was false — bootstrap
completes in well under a second, as `--print-logs --log-level DEBUG` shows. The
control that appeared to confirm it (a one-file repo that answered fine) did not
control for elapsed time, which was the variable that actually differed.

#### Reviewing its work

It cannot be trusted more than any other agent, and the same rule applies: **read
the line it cites before repeating the claim.** It has earned that trust on
citations so far; it has not yet been proven on code changes, because every
attempt to test that here was killed by the harness rather than by the model.

### Device-local memory

Durable, machine-local context lives in `.agents/memory/` — a symlink into the
shared store, git-ignored, never committed. Check its `INDEX.md` for notes on
decisions, project state, environment quirks, and open threads. Add or update
entries there (dated, one file per topic) rather than losing context between
sessions. Never store secrets in it.

Notes written there are now read by every agent on this machine, which is the
point, and worth a sentence of care: write what a peer would need, not what you
would need.
