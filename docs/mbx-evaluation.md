# mbx (mr boxington) evaluation — do not adopt, for now

> Evaluated 2026-08-31 on `fortyninety` against `jdx/mr-boxington`, source at
> commit `e565eda6`, binary release **1.1.0** (checksum-verified). This document
> exists so the next person who finds <https://mr-boxington.jdx.dev/> does not
> have to redo the spike.

## Verdict

**Do not adopt — for the stated purpose.** The request was "integrate this to
alleviate our disk space issues". Measured head-to-head on this machine, mbx
**uses more disk per worktree than the kache setup it would replace**, because
its space-saving mechanism is reflink deduplication and this box's ext4
filesystem does not support reflinks.

**The single strongest piece of evidence** — the same crate, the same warm
cache, a second checkout, measured both ways:

| | wall time | `target/` size | hardlink count of cached artifacts |
|---|---|---|---|
| **mbx** | **6.3 s** | **459 MB** | **1** — every byte unique to this checkout |
| **kache** (today) | 17.7 s | 271 MB | **2–4** — shared inodes with the store |

mbx is nearly **3× faster** and costs **~1.7× the disk**, and its disk cost is
*unshared* where kache's is *shared*. Adopting it to save disk would trade away
disk to buy speed. That may well be a trade worth making — but it is the
opposite of the one that was asked for, so it should be made deliberately and
not under the heading of disk relief.

This is a "not for this reason, not on this filesystem" — not "mbx is bad". It
is a well-built tool and it **passes the safety test this repository would most
expect it to fail** (Q2 below). The one change that would flip this verdict
completely is a reflink-capable filesystem; see [What would change
this](#what-would-change-this-verdict).

## The disk problem is real, and bigger than the brief said

True regardless of mbx, and worth recording. Measured with `du`, hardlinks
counted once, 2026-08-31:

| Thing | Size |
|---|---|
| All 15 `target/` directories in the checkout | **202.2 GiB** |
| The same, measured *together* so any sharing counts once | **202.2 GiB** |
| `~/.cache/kache` | 53 GiB |
| All `target/` directories **and** the kache store together | 250.1 GiB |
| Root filesystem | 1.6 T, 78 % used at the start of this spike |

**59 GiB is reclaimable right now, with no new tooling.**
`.claude/worktrees/cx-capacity/target` is 59 GiB and `cx-capacity` does **not**
appear in `git worktree list` — the directory outlived its registration.
Nothing can ever ask for those outputs again. Deleting that one directory
recovers more disk than adopting mbx would.

That is the actual shape of the disk problem: not insufficient deduplication,
but **unbounded accumulation that nobody prunes**. `./scripts/dev-cache.sh
prune` already exists for exactly this and reclaims all 202.2 GiB. The gap is
that it is manual. A timer around the existing script addresses the real
problem at a fraction of mbx's cost.

## Q1. What mbx actually is

A Cargo wrapper — `mbx build`, `mbx test`, or an installed Cargo shim. Per
`docs/how-it-works.md`, each invocation resolves the workspace via Cargo
metadata, starts an **in-process** cache agent (explicitly "no persistent
daemon"), and runs Cargo with its own shim as `RUSTC_WRAPPER`, plus `HOST_CC` /
`HOST_CXX` shims for build scripts. Each shim content-addresses its compiler
invocation, restores outputs on a hit, compiles and publishes on a miss.

It is the same *category* of tool as kache, and says so: mbx's
`docs/compared.md` calls kache "the closest tool to mbx", which "predates mbx
and directly inspired its design".

Two things go beyond kache:

- **Managed target directories.** `target/` becomes a symlink into mbx's cache
  root, pruned automatically.
- **Machine-wide compile scheduling.** Every compiler takes a permit from one
  machine-wide, memory-weighted pool, so concurrent builds share one CPU and
  memory budget instead of each sizing `-j` to the whole machine.

## Q2. Does it keep per-checkout outputs separate? — **yes, decisively**

This was the go/no-go, and mbx passes it empirically. The hazard behind
`docs/build-cache.md` is **two concurrent builds colliding over the same linked
output**. mbx does not create that hazard.

**Separate target directories, keyed per checkout.** Two checkouts of identical
sources resolved to different directories:

```text
wtA/target -> <cache>/targets/v1/e738ba7f2a5020f1…
wtB/target -> <cache>/targets/v1/df8d1ed6a854c893…
```

**Linked binaries and test executables are separate files.** They are
byte-identical — same content, restored from the same cache entry — but they
are *not the same inode*:

| Artifact | wtA | wtB | sha256 |
|---|---|---|---|
| `debug/collide` (binary) | inode 33313744 | inode 33313780 | identical |
| `deps/it-b18f…` (test exe) | inode 33313740 | inode 33313772 | identical |

Different inodes means each checkout owns its own copy. A build that writes
into its `target/` cannot affect any other checkout, and two builds cannot
contend for one output path. mbx's docs state this as a deliberate design rule
— where it cannot reflink it copies, "still never a hardlink" — and the
measured link count of 1 on every restored artifact confirms it.

Note the contrast: **kache *does* hardlink outputs into `target/`** (link counts
2–4 above), so under kache a shared inode genuinely is reachable from a
worktree. mbx is the *safer* of the two on this specific axis.

**They do not serialize.** Four concurrent cold builds, each running `mbx test`
in its own checkout:

```text
wtA rc=0 dur=2.84s   wtB rc=0 dur=2.93s
wtC rc=0 dur=2.89s   wtD rc=0 dur=2.85s
TOTAL wall: 2.94s
```

Four builds completed in the wall time of one, all tests passing, no errors.
Serialization would have taken ~11 s. The builds actively *cooperated*: in a
two-way run, one compiled `regex_automata`/`regex_syntax`/`regex` while the
other compiled `aho_corasick`/`memchr`, and each restored the other's results
in flight.

**So the shared-`CARGO_TARGET_DIR` prohibition does not reach mbx.** What is
shared is the content-addressed intermediate artifacts. What stays per-checkout
is the entire materialized `target/`, every linked binary and test executable
included, as its own inode. That is exactly the separation the rule exists to
protect, achieved without a shared directory and therefore without the lock.

`docs/build-cache.md` is left **unamended**, because nothing here is adopted.
If mbx is ever adopted, that document should be corrected rather than patched:
its stated reason (cargo's exclusive target-directory lock) is the *mechanism*
of the remedy, while the *hazard* is colliding linked outputs. Written as the
hazard, the next tool gets judged on what actually matters.

One real behavioural change, which is *not* the lock: mbx's scheduler would
throttle this box's parallel agent builds to one shared budget
(`scheduler.cpus` = logical CPUs, dividing 85 % of RAM). That is contention,
not serialization — cache hits never wait, and permits are kernel-released if a
process dies. For several agents building at once it is an improvement over
today, where each build believes it owns all 32 cores. It is the single most
attractive thing about mbx for this repository.

## Q3. Does it conflict with kache? — yes, and silently

`.cargo/config.toml` sets `build.rustc-wrapper = "kache"`. mbx sets
`RUSTC_WRAPPER` to its own shim. There is one such seat and both projects say
it cannot be shared — mbx's `docs/faq.md`: "Both wrap rustc through
`RUSTC_WRAPPER`, so they cannot be combined for the same build."

mbx has a guard for this, in `crates/mbx/src/session.rs`:

```rust
warn!(
    "RUSTC_WRAPPER is already set to {previous}; deferring to it, so this build is not cached"
);
```

**That guard does not protect this repository**, because it reads the
`RUSTC_WRAPPER` *environment variable* (`crates/mbx/src/cli/cargo.rs`,
`get_env("RUSTC_WRAPPER")`), and this repo configures the wrapper in
`.cargo/config.toml` instead — deliberately, since a committed cargo config is
the only way a setting reaches every worktree.

Verified twice. First, cargo's precedence, with two marker scripts:

| Configuration | Wrapper used |
|---|---|
| `build.rustc-wrapper` in `.cargo/config.toml` only | the config one |
| Same config **plus** `RUSTC_WRAPPER` in the environment | **the environment one** |

Then with real mbx 1.1.0, against a checkout carrying this repo's exact config
shape: **the config-file wrapper was never invoked once, and mbx emitted no
warning.** The build silently ran through mbx's cache instead.

So mbx installed on this box today would silently disable kache in every
worktree, and a machine with mbx and a machine without would build through
entirely different caches while both looked healthy. If mbx is ever adopted,
removing `rustc-wrapper` from `.cargo/config.toml` is mandatory, not optional —
leaving both in place is the worst of the three available states and would put
*two* multi-gigabyte stores on a disk we are trying to empty.

**CI is not affected, which is better than expected.** `ci.yml` sets a
workflow-wide `RUSTC_WRAPPER: ""` (line 80) and only the `clippy` job opts back
in, via `kunobi-ninja/kache-action` writing `RUSTC_WRAPPER=kache` to
`$GITHUB_ENV`; `package-client.yml` does the same at line 49. CI therefore does
not depend on the config-file setting at all, and mbx run in CI *would* see the
env var and correctly defer. The `orrery-kache` bucket, its OIDC role, and
`infra/s3.tf` need not be touched by a local-only adoption — the conflict is
confined to the developer workstation.

`scripts/check.sh doctor` and `scripts/dev-cache.sh` are kache-specific and
would need rewriting.

## Q4. Does it save disk here? — no; it costs disk

**Its deduplication mechanism does not function on this machine.** mbx restores
outputs by reflinking them from its store into each `target/`; from
`docs/how-it-works.md`, "When cloning is unavailable, mbx transparently copies
the bytes instead."

The root filesystem — holding `/home`, the checkout, and `~/.cache` — is
**ext4**, which has no reflink support. Verified at the real location:

```console
$ cp --reflink=always /home/baadc0de/.cache/rf-src /home/baadc0de/.cache/rf-dst
cp: failed to clone …: Operation not supported
```

mbx confirms it in its own reporting on every single build run during this
spike, without exception:

```text
mbx[cache]: materialization: 0 outputs (0 B) reflinked, 279 outputs (266.7 MiB) copied
```

Zero reflinks, always. So on `fortyninety` every cache hit writes a **full byte
copy** into `target/`: the store holds one copy and each checkout holds another,
unshared.

That produces the head-to-head at the top of this document. On the real orrery
workspace, `cargo check -p orrery_sim` in a second checkout against a warm
store: mbx 6.3 s and a 459 MB `target/` of entirely unique bytes; kache 17.7 s
and a 271 MB `target/` whose artifacts carry link counts of 2–4, i.e. shared
with the store rather than duplicated.

The copy fallback also costs time: that mbx run spent **13.5 s cumulative in
materialization** copying 266.7 MiB. It still won on wall clock, but a large
part of what reflinks are for was paid in full.

What mbx *would* give is **bounding**. Its budgets scale with the disk
(`crates/mbx/src/config.rs`: `MAX_SCALED_BUDGET = 100 GiB`, `BUDGET_INCREMENT =
5 GiB`), and on this box it reports them itself:

```text
mbx[setup]: cache is …, shared by every checkout and worktree, pruned to 75.0 GiB
mbx[setup]: target/ is managed: deleted when its checkout is gone, unused for 30 days, or over 100.0 GiB total
```

175 GiB steady state against today's unbounded 250.1 GiB. That is a real
property and the honest best case for mbx here — but it is **pruning, not
sharing**, and per worktree it is pruning *more* bytes than kache would have
created. The same bound is available today from `dev-cache.sh prune` plus a
timer, without replacing the build cache.

## Q5. Failure mode if mbx is missing or broken

Roughly neutral, with one improvement and one new edge.

Better: mbx would remove a hard prerequisite. Today `.cargo/config.toml` makes
kache mandatory — a missing binary fails every build with `could not execute
process kache`, which is why both workflows carry `RUSTC_WRAPPER: ""`. mbx is
invoked as a command rather than baked into committed config, so without it
`cargo build` still works, merely uncached.

Worse, and specific to this repo: as shown in Q3 an mbx that is *present*
silently suppresses kache with no warning. The failure is not a broken build but
an invisible divergence between machines, surfacing as an unexplained hit-rate
collapse. And because mbx replaces `target/` with a symlink into its cache root,
a broken mbx also leaves `target/` pointing into a store nothing is managing;
`MBX_TARGET_VIEWS=0` and `mbx cache remove` both require a working mbx.

## What was tested, and what was not

Tested empirically on this box: reflink support at the real path; cargo's
wrapper precedence; mbx's silent displacement of a config-file wrapper; two- and
four-way concurrent cold builds with tests; per-checkout inode identity of
linked binaries and test executables; cold and warm `cargo check -p orrery_sim`
on the real orrery workspace under both mbx and kache, with sizes and link
counts.

Not tested:

- **mbx's remote cache.** Its protocol cache server and GitHub Action were read
  about, never run. Any CI claim here is documentary.
- **A full workspace build.** Everything was scoped to `orrery_sim` and one
  synthetic crate to respect the disk budget. mbx bypassed 28–29 compilations
  per run (`cc-unknown-flag`, `native-library`,
  `build-script-no-declared-inputs`, and others); on a full build of this
  workspace — which has FoundationDB and other native dependencies — that
  bypass set could be materially larger and was not characterised.
- **Long-run behaviour.** Collection, the 30-day age policy, and budget
  enforcement were never exercised; the store peaked at 274 MB.
- mbx is **1.1.0**, days old. All experiment artifacts, worktrees, and caches
  created for this spike were removed.

## What would change this verdict

Not "mbx improves" — the blockers are local to this machine:

1. **A reflink-capable filesystem** (btrfs, XFS with `reflink=1`, or ZFS) for
   the cache and target root. This flips the entire disk result: the 459 MB of
   unique bytes becomes shared extents, the 13.5 s materialization mostly
   disappears, and mbx becomes both faster *and* cheaper on disk than kache.
   `target.root` can be pointed at such a volume independently, which is the
   cheap way to run this experiment for real.
2. **Wanting mbx for speed and concurrency rather than disk.** It is ~3× faster
   on warm cross-checkout builds and ran four concurrent builds in the time of
   one. If that is the goal, the case is strong and should be judged on its own
   terms — the required migration is smaller than it first appears, since CI is
   already decoupled.

Until then the disk lever is the documented one: prune — starting with the
59 GiB in `cx-capacity` that no worktree claims.
