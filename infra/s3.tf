# =============================================================================
# #174 — the kache remote: one private bucket, and the lifecycle rule that
# replaces `kache-prune-shared.timer`.
# =============================================================================

resource "aws_s3_bucket" "kache" {
  bucket = var.cache_bucket_name

  # No `force_destroy`. `terraform destroy` will refuse while objects remain,
  # which is the correct default for a bucket someone might one day point at
  # something that is not a cache. The tear-down runbook in infra/README.md
  # empties it explicitly, so the refusal is a speed bump and not a trap.

  tags = {
    Purpose = "kache-remote"
    Issue   = "174"
  }
}

# -----------------------------------------------------------------------------
# Public access: blocked, all four switches.
#
# The bucket holds compiled artifacts of a public repository, so the
# confidentiality stake is low — but the *integrity* and *billing* stakes are
# not. A world-writable cache is a supply-chain injection point, and a
# world-readable one is an open egress bill at $0.09/GiB paid by us.
#
# All four settings, not two: the ACL pair and the policy pair block different
# routes to the same outcome, and the account-level equivalent is not set (this
# account predates us and carries unrelated workloads, so we do not touch
# account-wide settings).
# -----------------------------------------------------------------------------
resource "aws_s3_bucket_public_access_block" "kache" {
  bucket = aws_s3_bucket.kache.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# -----------------------------------------------------------------------------
# Ownership: BucketOwnerEnforced, i.e. ACLs disabled entirely.
#
# The modern S3 default and the one that makes the IAM policy the single source
# of truth for access. With ACLs off there is no second, per-object grant
# mechanism a reviewer has to also audit — which is why iam-cache-policy.tf can
# claim to be the whole grant.
# -----------------------------------------------------------------------------
resource "aws_s3_bucket_ownership_controls" "kache" {
  bucket = aws_s3_bucket.kache.id

  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

# -----------------------------------------------------------------------------
# Encryption at rest: SSE-S3 (AES256), with a bucket key.
#
# SSE-KMS was considered and declined, on cost and on blast radius:
#
#   - A build cache is millions of small objects. SSE-KMS bills a KMS request
#     per object operation ($0.03 per 10,000), which on a cache is a charge
#     proportional to the cache's whole reason for existing. `bucket_key_enabled`
#     mitigates it heavily but does not remove it, and SSE-S3 costs nothing at
#     all.
#   - SSE-KMS would require the CI role to hold `kms:Decrypt` and
#     `kms:GenerateDataKey`, widening iam-cache-policy.tf beyond S3 for no gain
#     against the threat that matters here (integrity, not confidentiality —
#     these are compiler outputs of a public repository).
#
# `bucket_key_enabled` is set anyway: it is free, has no effect under SSE-S3,
# and means a later switch to SSE-KMS does not also have to remember it.
# -----------------------------------------------------------------------------
resource "aws_s3_bucket_server_side_encryption_configuration" "kache" {
  bucket = aws_s3_bucket.kache.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
    bucket_key_enabled = true
  }
}

# -----------------------------------------------------------------------------
# Versioning: NOT enabled, and the omission is the decision.
#
# There is no `aws_s3_bucket_versioning` resource here on purpose; S3's default
# for a new bucket is unversioned, and the provider's "Disabled" status is only
# accepted for buckets that have never had versioning, so the resource would
# encode a fragile assertion in place of a plain default.
#
# Why decline it:
#
#   - The content is content-addressed and immutable. A key's bytes are a
#     function of its name, so a "previous version" is either identical or the
#     product of a hash collision. There is nothing to roll back to.
#   - An overwrite of an existing key is a no-op in content terms, so versioning
#     would accumulate byte-identical copies and bill for every one.
#   - It would actively fight the retention rule. With versioning on,
#     `Expiration.Days` deletes nothing: it writes a delete marker and the real
#     object becomes a noncurrent version that survives until a *separate*
#     `NoncurrentVersionExpiration` rule removes it. The single most likely way
#     to reproduce #174's unbounded-growth failure on S3 is to enable
#     versioning and then wonder why the expiry rule freed no space.
#   - `AGENTS.md` § Build cache: deleting a cached object is always safe — it
#     becomes a cache miss and nothing worse. Recovery is a rebuild.
#
# If versioning is ever enabled, a NoncurrentVersionExpiration rule must be
# added in the same change, and the retention arithmetic below redone.
# -----------------------------------------------------------------------------

# -----------------------------------------------------------------------------
# THE LIFECYCLE RULE — #174's central deliverable, and its size-versus-age
# argument.
# -----------------------------------------------------------------------------
#
# ## The failure this must not repeat
#
# `AGENTS.md` § Build cache records that the first `kache-prune-shared.timer`
# policy keyed on **age since last read** with a 24-hour window, and that it
# **could not fire**: every build re-reads the whole hot set, so no object was
# ever untouched for 24 hours. The filesystem remote reached **319 GiB in a
# single day** and the disk hit 94%. The policy did not prune too little; it
# pruned *nothing*, and the monitoring said it was working.
#
# ## Why it structurally cannot recur here
#
# S3's `Expiration.Days` keys on **age since object creation**. That is not a
# tighter version of the failed quantity, it is a different quantity, and the
# difference is the whole argument:
#
#   - Creation time is written once, at PUT, and is **monotone**. Reads do not
#     touch it. A GET, ten thousand GETs, or none at all leave it identical.
#   - Therefore an object created N days ago is deleted at N days, whatever its
#     traffic. The set the rule matches grows without bound if uploads continue,
#     so "matches nothing" is not a reachable state. The exact failure mode of
#     the 319 GiB incident — a predicate that a busy cache can never satisfy —
#     is unavailable to a creation-age rule.
#   - The steady-state size is closed-form:
#
#         steady_state_bytes = unique_bytes_uploaded_per_day x expiration_days
#
#     It is bounded for any finite fill rate, which is exactly the property the
#     filesystem remote lacked.
#
# ## What it costs instead, stated rather than hidden
#
# Creation-age expiry evicts hot objects too. An object read on every build
# still dies on its Nth day and is re-uploaded on the next miss. So the trade is
# real, and it is: unbounded growth, converted into one cache miss per object
# per N days.
#
# Three things make that acceptable here, and all three are on the record:
#
#   1. A miss is only slow. Objects are content-addressed and immutable, so a
#      pruned object is a cache miss and nothing worse (AGENTS.md § Build
#      cache). No correctness surface exists to get wrong.
#   2. The eviction is a rolling front, not a cliff. Objects are created when
#      they are first needed, so their expiry dates are spread across the whole
#      window in the same distribution. The cache never empties at once.
#   3. Re-uploading resets the clock — a new PUT is a new creation time — so a
#      genuinely hot object costs one miss per N days and is then warm again.
#
# ## Choosing N, honestly
#
# **N = 14 days (`var.cache_expiration_days`), and it is a starting value, not a
# measured one.** #174 asks for the window to be sized against a measured fill
# rate; that measurement does not exist, because the remote does not exist —
# `kache doctor` on the shared box still reports "no remote cache or planner
# configured". Substituting a confident number for an absent measurement is what
# produced the 24-hour policy. So instead, the bracket:
#
#   - **Plausible case.** #171 measured the heaviest cold lane's `target/` peak
#     at 46.92 GiB, which is uncompressed build output, of which cacheable
#     compiler artifacts are a fraction. The dependency graph is stable across
#     PRs, so it is uploaded once and hit thereafter; the *new unique* content
#     per day is roughly what changed. At 3 GiB/day of new unique objects,
#     14 days is ~42 GiB, about **$1.03/month** of S3 Standard storage in
#     eu-central-1. At 10 GiB/day it is 140 GiB, about **$3.43/month**.
#   - **Worst historical case.** The shared box's 319 GiB/day, which was
#     multi-identity churn with zero eviction and is almost certainly not
#     representative of hosted CI lanes. At that rate 14 days is ~4.4 TiB, about
#     **$109/month**. That is the number that matters: it is the one that says
#     N must be re-derived from measurement rather than left at 14 forever.
#
# The lever is `var.cache_expiration_days` and nothing else — S3 lifecycle has
# no size trigger, and #174 forbids a bespoke pruning timer, cron or script to
# supply one, which is the whole point of moving retention into the storage
# layer. So the operational contract is: **#175 measures the fill rate when it
# wires the remote into the hosted lanes; whoever lands it re-derives N from
# that number and re-applies.** infra/README.md § Watching the size gives the
# free CloudWatch metric to read it from.
#
# ## Two smaller decisions inside the rule
#
# - **Expiry, not transition.** #170's smoke test found the account reports
#   `TransitionDefaultMinimumObjectSize: all_storage_classes_128K`, so a
#   *transition* rule silently skips objects under 128 KB — most of a build
#   cache. No transition rule is written here, and anyone adding one (Glacier,
#   IA) must override that setting or it will tier almost nothing while
#   appearing to work. Expiry is unaffected by it.
# - **Prefix-filtered.** Expiry applies to `${var.cache_prefix}/` only, so a
#   future non-cache object elsewhere in the bucket is not silently deleted on
#   a schedule chosen for compiler output.
# -----------------------------------------------------------------------------
resource "aws_s3_bucket_lifecycle_configuration" "kache" {
  bucket = aws_s3_bucket.kache.id

  # Ordering only: the configuration replaces the bucket's rules wholesale, and
  # ACLs/ownership must settle first so nothing races the first PUT.
  depends_on = [aws_s3_bucket_ownership_controls.kache]

  # ---------------------------------------------------------------------------
  # Rule 1 — the retention rule that replaces `kache-prune-shared.timer`.
  # ---------------------------------------------------------------------------
  rule {
    id     = "expire-cache-objects"
    status = "Enabled"

    filter {
      prefix = "${var.cache_prefix}/"
    }

    expiration {
      days = var.cache_expiration_days
    }
  }

  # ---------------------------------------------------------------------------
  # Rule 2 — abort incomplete multipart uploads. Bucket-wide, on purpose.
  #
  # #174 calls these "the quiet version of the same unbounded-growth problem",
  # and that is precise: uploaded parts of an unfinished multipart upload are
  # billed at full storage rates and are **invisible to `list-objects`**. Only
  # `list-multipart-uploads` shows them, and nobody runs it. So the store can
  # grow, be billed for, and read as empty. A cancelled workflow or a reclaimed
  # runner mid-PUT is all it takes, and CI cancels runs routinely — ci.yml sets
  # `cancel-in-progress` on every non-main ref.
  #
  # The filter is empty rather than prefix-scoped: an orphaned part is garbage
  # wherever it is, and unlike an expiry there is no case in which someone wants
  # to keep one. Note that `AbortIncompleteMultipartUpload` cannot be combined
  # with `Expiration` in one rule, which is why this is a second rule and not a
  # second clause.
  #
  # `filter {}` — an explicitly empty filter — rather than omitting the block:
  # the S3 API requires each rule to carry a filter or a (deprecated) prefix,
  # and the provider errors on a rule with neither.
  # ---------------------------------------------------------------------------
  rule {
    id     = "abort-incomplete-multipart-uploads"
    status = "Enabled"

    filter {}

    abort_incomplete_multipart_upload {
      days_after_initiation = var.multipart_abort_days
    }
  }
}

# -----------------------------------------------------------------------------
# Bucket policy: refuse anything not over TLS.
#
# IAM already governs who may call what; this governs *how*. A `Deny` with
# `aws:SecureTransport = false` is the one control that IAM policies cannot
# express on the caller's behalf, because it constrains every principal
# including ones added later and including the account root. Cheap, standard,
# and the thing an auditor looks for first.
#
# Note the `Deny` applies to the root user too. That is intentional and is not a
# lockout: it forbids plaintext HTTP, not access. Every AWS SDK, the CLI and
# kache's OpenDAL backend all use HTTPS by default, so nothing legitimate
# notices.
#
# It depends on the public access block: applying a bucket policy before
# `block_public_policy` is in place would leave a window, however brief, in
# which a policy could be public.
# -----------------------------------------------------------------------------
data "aws_iam_policy_document" "kache_bucket" {
  statement {
    sid    = "DenyInsecureTransport"
    effect = "Deny"

    principals {
      type        = "*"
      identifiers = ["*"]
    }

    actions = ["s3:*"]

    resources = [
      aws_s3_bucket.kache.arn,
      "${aws_s3_bucket.kache.arn}/*",
    ]

    condition {
      test     = "Bool"
      variable = "aws:SecureTransport"
      values   = ["false"]
    }
  }
}

resource "aws_s3_bucket_policy" "kache" {
  bucket = aws_s3_bucket.kache.id
  policy = data.aws_iam_policy_document.kache_bucket.json

  depends_on = [aws_s3_bucket_public_access_block.kache]
}
