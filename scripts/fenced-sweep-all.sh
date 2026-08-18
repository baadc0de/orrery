#!/usr/bin/env bash
# The whole fdb-off-bulk-path capacity study: rate leg and concurrency leg,
# before/after interleaved, two repeats each.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=fenced-sweep-env.sh
. "$here/fenced-sweep-env.sh"
exec "$here/fenced-sweep-driver.sh" "${REPEATS:-2}" \
  r20k:125:2 r40k:250:4 r60k:500:6 r80k:500:8 r120k:750:12 r160k:1000:16 \
  c250:250:2 c500:500:2 c1000:1000:2 c2000:2000:2
