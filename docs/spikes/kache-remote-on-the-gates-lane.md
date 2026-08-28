# Measurement: an S3 kache remote on CI's `static gates` lane

**Status: measurement, non-normative.** It supplies the number
[#585](https://github.com/baadc0de/orrery/issues/585) asked for and that
`.github/workflows/ci.yml`'s `gates:` block said did not exist. It does **not**
overturn that block's stance on its own; it ends its "no measurement of its own
either way" clause.

**Date:** 2026-08-27. **Method:**
[#175](https://github.com/baadc0de/orrery/issues/175)'s, reproduced so the
figures are comparable with its `clippy` result (\$1.15 per hour of wall-clock
saved — kept) and its `test` result (\$16.70 — removed).

**Nothing here is wired.** The rig lived on a throwaway branch,
`spike/585-gates-kache-measurement` (draft PR
[#586](https://github.com/baadc0de/orrery/pull/586)), which is not proposed for
merge and was closed when the runs finished. `gates` is unchanged apart from the
figures recorded in its comment.

---

## 1. The answer

| | `clippy` (#175, kept) | `test` (#175, removed) | **`gates` (here)** |
|---|---|---|---|
| cold wall-clock | 241.5 s (n=2) | 677.5 s (n=2) | **1276 s (n=22)** |
| warm wall-clock | 148 / 131 / 121 s | 562 / 690 / 637 s | **637 / 719 / 848 s** |
| warm mean vs cold | −45 % | −7 % | **−42 %** |
| warm spread | 27 s | 128 s | **211 s** |
| hit rate | 99.9 / 99.9 / 100.0 % | 75.2 / 63.7 / 89.4 % | **96.8 / 97.4 / 98.2 %** |
| pulled per run | 0.461 GiB | 2.30 GiB | **3.15 GiB** |
| **\$ per hour of wall-clock saved** | **\$1.15** | **\$16.70** | **\$2.02** |
| monthly egress bill at this repo's run rate | ~\$28 | ~\$200 | **~\$228** |

**This is not `test`'s result and it is not `clippy`'s.** By the metric #175
decided on — dollars per hour of wall-clock saved — `gates` passes comfortably:
\$2.02 against `clippy`'s \$1.15 and eight times better than the \$16.70 that
sank `test`. The hit rate is stable to 1.4 percentage points on an unchanged
tree, nothing like `test`'s 25.7-point swing, and the 211 s warm spread is 39 %
of the 541 s saving rather than 267 % of it.

**And it still should not be wired on this evidence, for two reasons neither of
#175's lanes showed.** First, across three runs of an *identical* tree the hit
rate rose monotonically 96.8 → 97.4 → 98.2 % **and the wall-clock rose with it**,
637 → 719 → 848 s: the cache got warmer and the lane got slower, because the
per-hit cross-cloud round trip dominates and it scales with the hits (§ 5).
Second, a fourth arm nobody asked for turns out to matter more than the third:
**kache with no remote at all takes 176 s (14 %) off the lane for \$0**, because
three quarters of the hits are same-run duplicates across the twelve workspaces
and never touch the network. Against that free baseline the S3 remote's marginal
price is **\$3.00 per hour**, not \$2.02.

This is #585's **outcome 3, ambiguous** — stated as such rather than rounded
toward a change. § 7 says which way each metric points and what to do about it.

---

## 2. What was run, and how much CI it cost

Six executions of the lane, on a throwaway branch, plus a free baseline:

| # | What | Run |
|---|---|---|
| — | **cold baseline** — 22 real pull-request executions of the lane already on the record for 2026-08-27, cacheless as the job ships. Cost: nothing. | various |
| 1 | cache-warming run: kache installed, local store empty, bucket holding only `clippy`-era objects | [33102954252 attempt 1](https://github.com/baadc0de/orrery/actions/runs/33102954252/attempts/1) |
| 2–4 | **three warm runs on an unchanged tree**, re-running the single `gates` job so nothing else was re-billed | attempts 2, 3, 4 of the same run |
| 5 | one warm run with **one first-party crate's cache key invalidated**, which is what a real pull request does | [33107788013](https://github.com/baadc0de/orrery/actions/runs/33107788013) |
| 6 | one run with **kache installed and no remote at all**, on the same unchanged tree | [33109070779](https://github.com/baadc0de/orrery/actions/runs/33109070779) |

Runs 2–4 are #175's exact shape: same commit, same job, three attempts, so the
only variable is cache state. Re-running the one job rather than the workflow
kept the other five lanes from being paid for three times. Runs 5 and 6 are
additions, and each answers a question the three-run protocol structurally
cannot: 2–4 measure the best case (nothing changed) while every real pull request
is run 5's case, and 2–5 all price the remote against *nothing* rather than
against the free arrangement run 6 measures. Both turned out to change the
answer, which is why they were worth about \$0.60 of egress between them.

The cold side was **not** re-run. The lane already executes cold on every pull
request, so 22 same-day samples were free and n=22 beats n=3. That is a
deliberate deviation from the brief's "three runs each" and it makes the
baseline stronger, not weaker.

---

## 3. The cold baseline is not 656 s any more

`gates:` and #171 both record **656 s**, from one `ubuntu-latest` sample on
2026-08-20 ([run 32468444495](https://github.com/baadc0de/orrery/actions/runs/32468444495)) —
and #171's own text notes that figure needed `awk` pointed at mawk for one leg,
because gawk 5.2.1 broke `p3-island-gate.sh --self-test` until #195 fixed it
properly.

Measured again on 2026-08-27 over 22 real pull-request executions of the lane,
all cold and cacheless exactly as configured:

| | s |
|---|---|
| mean | **1276** |
| median | 1320 |
| min | 1004 |
| max | 1380 |
| population sd | 112 |

**The lane has roughly doubled since #171 measured it**, which matches #585's own
observation that it now takes 20–22 minutes and finishes last on nearly every
pull request. Anything still quoting 656 s as this lane's cold cost is stale.

The spread matters as much as the mean, and it is the first place this lane
differs from the two #175 measured. #171's two cacheless `test` samples agreed
to within 1 %, which is what let #175 attribute that lane's warm variance to the
cache. Here the **cacheless** lane already spreads 376 s (sd 112 s). Any warm
figure has to be read against that, not against a quiet baseline.

---

## 4. The runs

`bytes_down` is read from `kache report --format json`, not inferred. It is the
billed direction: hosted runners are on Azure, the bucket is `eu-central-1`, so
every cache **hit** is egress. `bytes_up` is recorded before kache-action's post
step, so it undercounts uploads — which does not matter, because uploads are
free.

| run | wall | hit rate | weighted | pulled | pushed | store | free disk at peak | download avg |
|---|---|---|---|---|---|---|---|---|
| 1 warming | 1086 s | 83.8 % | 59.0 % | 0.812 GiB | 3.134 GiB | 18.1 / 20 GiB (91 %) | 38.54 GiB | 359 ms |
| 2 warm | **637 s** | 96.8 % | 91.9 % | 3.114 GiB | 0.524 GiB | 18.1 / 20 GiB (91 %) | 37.30 GiB | 341 ms |
| 3 warm | **719 s** | 97.4 % | 93.4 % | 3.139 GiB | 0.579 GiB | 18.0 / 20 GiB (90 %) | 38.81 GiB | 401 ms |
| 4 warm | **848 s** | 98.2 % | 91.3 % | 3.191 GiB | 0.017 GiB | 17.9 / 20 GiB (89 %) | 36.43 GiB | 476 ms |
| 5 warm, **one core crate changed** | 835 s | 98.8 % | 91.8 % | 3.031 GiB | 1.024 GiB | 17.9 / 20 GiB (89 %) | 39.08 GiB | 507 ms |
| 6 **no remote at all** | 1100 s | 64.8 % | 48.2 % | 0 | 0 | 18.0 / 20 GiB (90 %) | 39.65 GiB | — |

Warm mean over runs 2–4 (#175's protocol, unchanged tree): **735 s** against a
cold mean of 1276 s — a saving of **541 s (−42 %)**.

**Run 5 is the one the three-run protocol cannot produce, and it is reassuring.**
Every real pull request changes first-party code, so the best case measured by
2–4 is not the case the lane actually runs. Invalidating `orrery_core`'s cache
key — the crate the most of this tree depends on — cost 30 recompiled units and
1.02 GiB of *upload*, and changed neither the hit rate (98.8 %, the highest of
the five) nor the wall-clock (835 s, inside the 637–848 s warm band). The reason
is § 5's: this lane's cacheable mass is the third-party dependency graph
compiled twelve times over, and a first-party crate is a rounding error against
it. **A pull request does not degrade this cache.**

Two things the table should not be read past.

**The disk.** #171 measured this lane cacheless with **68.64 GiB free at peak**.
With the cache it is 36–39 GiB — the headroom is roughly halved, because the
runner's root filesystem is ext4 without reflink (#175's most reusable finding),
so kache's local store is a second *physical* copy of the cacheable part of
`target/`. The store sits pinned at 89–91 % of its 20 GiB cap and evicts on
every run. It fits. It fits with about half the margin the job's comment claims.

**Nothing was ever prefetched.** kache-action starts a daemon to warm-prefetch
from the previous run's manifest, and on this lane it downloaded 1.1–1.3 GiB per
run and used **0 %** of it every time — `0 advisory / 20 fallback plans`. On
`clippy` prefetch is part of why the remote pays. Here it is pure waste, and the
lane's shape is why: twelve separate workspaces mean twelve build graphs, and
the single-manifest planner cannot predict them.

---

## 5. Linking, and what actually limits this lane

The job's comment says the lane is "many small standalone-workspace builds, and
it does link", and that linking is where object caching stops paying. **The data
half-confirms it: linking sets the ceiling, but it is not what stops the cache
paying.**

What linking does cost, identically in all six runs:

- **851 passthroughs and 171 probes** — rustc and cc invocations kache declines
  to cache at all. The largest classes are `cc unsupported flag(s):
  -mno-omit-leaf-frame-pointer` (709), query/probe invocations (141), `existing
  output path requires compiler write semantics` (68) and build-script probes
  (62). None of these is a cache failure; they are outside the cache by
  construction (`AGENTS.md` § Build cache: "Linking, `build.rs` executions and a
  few binary crate-type units are not cacheable by design").
- **The residual warm misses are link units, and they are the biggest units in
  the tree.** Run 4's 73 misses are led by `orrery_regolith_client` three times
  over (1.2 / 1.6 / 1.4 GB), `campaign_joins_host_fixture` (1.3 GB),
  `live_campaign_materialised_routing` (1.3 GB) and `build_info` — binary and
  integration-test units, i.e. exactly the ones that link. That is why 98.2 % is
  the ceiling and not 100 %, and why `clippy` — which never links — reaches
  100 %.

But those misses are only **5–14 min of compile work against ~1.2 h avoided**.
Linking caps the hit rate; it does not decide the economics. What decides them
is this:

> `Aggregate remote open/setup latency (~8min) exceeds read/transfer time
> (~5min) — check the remote path, storage latency, or read fan-out`

— kache's own diagnostic on run 4, and the same sentence #175 quoted when it
took the remote off `test`. Across runs 2, 3 and 4 the aggregate open/setup grew
**5 → 6 → 8 minutes**, average download time grew **341 → 401 → 476 ms**, and
failed downloads grew **9 → 23 → 24**. Every one of those scales with the number
of *successful* remote hits, so as the cache warms the lane pays more for it:

| | run 2 | run 3 | run 4 |
|---|---|---|---|
| hit rate | 96.8 % | 97.4 % | 98.2 % |
| remote hits | 1116 | 1146 | 1179 |
| aggregate open/setup | ~5 min | ~6 min | ~8 min |
| **wall-clock** | **637 s** | **719 s** | **848 s** |

That is the finding this lane contributes that neither of #175's did: on
`test` the cache was unstable, here it is stable and *anti-correlated with
speed*. Three points is three points, and the mechanism is not in doubt — it is
a cross-cloud round trip per artifact, ~312 ms when #175 measured it on `test`
and 341–476 ms here — but the slope is measured over three samples and should
not be extrapolated far.

### The number that reframes the whole question

Of run 4's 4,844 cacheable units, **3,578 were served by the *local* store** and
only **1,179 by the remote**. The lane compiles the same bevy/iroh dependency
graph up to twelve times, once per standalone workspace, into twelve separate
`target/` directories — and kache deduplicates that *within a single run*,
before any network is involved.

Three quarters of this lane's cache *hits* therefore need no bucket, no OIDC
role and no egress at all. **Run 6 priced that arm separately**, with
kache installed, `github-cache: false` and no S3 configuration whatsoever, so
same-run duplicates were the only hits available
([run 33109070779](https://github.com/baadc0de/orrery/actions/runs/33109070779)):

| | cold, cacheless | **kache, no remote** | kache + S3 remote |
|---|---|---|---|
| wall-clock | 1276 s | **1100 s** | 735 s |
| vs cold | — | **−176 s (−14 %)** | −541 s (−42 %) |
| hit rate | — | 64.8 % (48.2 % weighted) | 96.8–98.2 % |
| hits | — | 3139 local, 0 remote | 3578 local, 1179 remote |
| cache-hit overhead | — | ~4 min, 76 ms/hit | ~14–18 min, 176–225 ms/hit |
| ROI (compile avoided / hit overhead) | — | **9.9x** | 4.1–5.4x |
| bytes over the wire | 0 | **0** | 3.38 GB pulled |
| cost | \$0 | **\$0** | \$0.304/run |

**A third of the saving is free**, and it is the *efficient* third: 76 ms per
hit off local disk against 176–225 ms per hit across two clouds, and a 9.9x
return against 4.1–5.4x. The remote's marginal contribution is the other
365 s a run, and that costs \$0.304 — **\$3.00 per hour of marginal wall-clock
saved**, not the \$2.02 the headline rate suggests.

---

## 6. Cost

List price, `eu-central-1`, as `infra/README.md` § What it costs records it:
egress to the internet is \$0.09/GB past 100 GB/month free, and storage and
requests are cents.

- **Pulled per warm run:** 3.343 / 3.370 / 3.427 GB (mean **3.380 GB**), i.e.
  **\$0.304 a run**.
- **Saving per warm run:** 541 s.
- **\$0.304 × 3600 / 541 = \$2.02 per hour of wall-clock saved.** `clippy` is
  \$1.15 and keeps its remote; `test` was \$16.70 and does not.
- **Monthly:** the lane ran **26 times a day** on pull requests over 2026-08-25
  to 08-27 (33 / 22 / 23), close to the ~29/day #175 assumed. 26 × 30 ×
  3.380 GB = 2,636 GB, minus the 100 GB free tier, at \$0.09 = **\$228/month**
  (\$256 at 29/day).

The 100 GB free tier is **per account, not per lane**, so the three monthly
figures in § 1 cannot all claim it — `clippy` already consumes it. Charged
against `gates` on top of `clippy`'s existing traffic the figure is
2,636 × \$0.09 = **\$237/month**, and the comparison in § 1 is generous to this
lane rather than to the argument against it.

So the rate is `clippy`-like and the absolute bill is the largest of the three
lanes — larger than the ~\$200/month that helped remove `test`'s. Both readings
are true and they are not in conflict: `gates` buys far more time than `test`
did (541 s a run against 48 s), which is exactly why it can be eight times
cheaper per hour and still cost more per month.

---

## 7. Recommendation

**Outcome 3 of #585's three: ambiguous — and here is exactly which way each
metric points, rather than a verdict rounded toward a change.**

**Do not wire the S3 remote on this evidence.** Not because it fails #175's
test — it passes it, at \$2.02 per hour of wall-clock saved against `clippy`'s
accepted \$1.15 and `test`'s rejected \$16.70, with a hit rate stable to 1.4
points and a warm spread that is 39 % of the saving rather than 267 % of it.
Three separate things say wait anyway:

1. **The bill is the largest of the three lanes.** \$228–256/month, against a
   repository whose entire recorded S3 line today is \$1–4/month of storage plus
   `clippy`'s ~\$28/month of egress. #175 established that spending this money is
   the owner's call and not an executor's; a rate that passes does not make a
   ~9x increase in the cloud bill somebody else's decision to take.
2. **The trend across three identical runs points the wrong way.** 637 → 719 →
   848 s as the hit rate rose 96.8 → 97.4 → 98.2 %, with aggregate remote
   open/setup growing 5 → 6 → 8 minutes and failed downloads 9 → 23 → 24 (§ 5).
   The −42 % is the best reading of a series that is degrading, and three points
   cannot say where it settles. Whoever decides this should get three more
   samples first; they cost about \$1.
3. **It halves the disk headroom** — 68.64 GiB free at peak cacheless (#171)
   against 36–39 GiB with the store, which sits pinned at ~90 % of its 20 GiB
   cap and evicts every run. It fits, but the job's "3.9x headroom" sentence
   stops being true.

**Do consider the free arm instead.** kache installed with no remote at all
takes 176 s (14 %) off the lane, needs no bucket, no OIDC grant, no egress and
no credential on the fork path — and it is the *efficient* part of the saving
(§ 5's table). It is not this issue's deliverable and it is not wired here
either; it is the cheapest change available to the lane and it deserves its own
decision.

**Re-measure when the runners move into `eu-central-1`.** That is the trigger
`test`'s block already records, and it applies here for the same two reasons and
more strongly: intra-region traffic takes the \$228/month to \$0, and the
cross-cloud round trip that is *this* lane's dominant download phase (37–47 % of
it) goes with it. At that point the remote is very likely right; prove it, do
not assume it.

---

## 8. `package-client.yml`, in one paragraph

`package-client.yml` sets `RUSTC_WRAPPER: ""` with the note "Unlike `ci.yml`
there is no kache-installing job here to put it back" — a fact, not a rationale.
**This measurement supports leaving it uncached, and supplies the rationale it
lacks.** Three of the numbers above transfer directly. First, the free arm buys
that workflow nothing: `gates`' 176 s of no-cost saving came *entirely* from
compiling one dependency graph twelve times into twelve `target/` directories,
and `package-client` compiles one workspace once per platform — there are no
same-run duplicates to deduplicate, so the arrangement that is free here is
worth zero there. Second, its slow leg is a **cold Windows Bevy build**
(1832/1856/1876 s over the last three releases), which is a target triple
nothing else in this repository ever compiles: its cache namespace would be
populated by nothing but its own previous release, so on a release-triggered,
rare workflow it is cold or nearly cold every time it matters — #175's "a
remote nothing has ever written to has a 0 % hit rate" condition, made permanent
by the trigger rather than by a warming pass. Third, the arithmetic never gets
off the ground: `gates` justifies its bill by saving ~117 hours a month across
26 runs a day, while `package-client` runs a handful of times per release, so
even at `clippy`'s \$1.15 rate the total saving is under an hour a month —
against wiring an OIDC grant and a third-party action into a workflow whose own
header says it "will eventually hold the private-assets credential" and that PR
code must never run with that authority. Leave it as it is.
