# Measurement: an S3 kache remote on CI's `static gates` lane

**Status: measurement, non-normative.** It supplies the number
[#585](https://github.com/baadc0de/orrery/issues/585) asked for and that
`.github/workflows/ci.yml`'s `gates:` block said was missing. It does **not**
overturn that block's stance; it ends the block's "no measurement of its own
either way" clause by supplying one.

**Date:** 2026-08-27. **Method:** [#175](https://github.com/baadc0de/orrery/issues/175)'s,
reproduced so the figures are comparable with its `clippy` (\$1.15 per hour of
wall-clock saved, kept) and `test` (\$16.70, removed) results.

**Verdict: outcome 2 — it costs like `test`. Leave `gates` cold.**

---

## 1. Result in one table

| | `clippy` (#175, keeps its remote) | `test` (#175, removed) | **`gates` (this measurement)** |
|---|---|---|---|
| cold wall-clock | 241.5 s (2 samples) | 677.5 s (2 samples) | **1276 s** (22 samples, 2026-08-27) |
| warm wall-clock | 148 / 131 / 121 s | 562 / 690 / 637 s | TBD |
| saving vs cold | −45 % | −7 % | TBD |
| hit rate | 99.9 / 99.9 / 100.0 % | 75.2 / 63.7 / 89.4 % | TBD |
| pulled per run | 0.461 GiB | 2.30 GiB mean | TBD |
| **\$ per hour of wall-clock saved** | **\$1.15** | **\$16.70** | **TBD** |

---

## 2. What was run, and what it cost to find out

TBD

## 3. The cold baseline is not 656 s any more

`gates:` and #171 both record **656 s**, from a single `ubuntu-latest` sample on
2026-08-20 ([run 32468444495](https://github.com/baadc0de/orrery/actions/runs/32468444495)),
and #171's own table shows that figure was obtained with `awk` pointed at mawk
for one leg because gawk 5.2.1 broke `p3-island-gate.sh --self-test` (#195 later
fixed that properly).

Measured again on 2026-08-27 over **22 real pull-request runs of the lane**, all
cold and cacheless as the job is configured today:

| | s |
|---|---|
| mean | 1276 |
| median | 1320 |
| min | 1004 |
| max | 1380 |
| population sd | 112 |

**The lane has roughly doubled since #171 measured it**, which is consistent
with #585's own observation that it now runs 20–22 minutes and is the last check
to finish. Anything in the tree still quoting 656 s as this lane's cold cost is
stale; the number to compare a cache against is 1276 s.

Note the second half of that table, because it decides how the cache has to be
read: **the cacheless lane's own spread is 376 s (sd 112 s)**. `test`'s cacheless
samples agreed to within 1 %, which is what let #175 attribute that lane's 128 s
spread to the cache. Here the runner's own noise is already larger than any
saving a cache is likely to produce, so a warm/cold mean difference of less than
about 200 s cannot be separated from it with three samples.

## 4. Where the misses are, and whether linking is the reason

TBD

## 5. Cost

TBD

## 6. `package-client.yml`

TBD

## 7. Recommendation

TBD
