# =============================================================================
# #173 — what the federated role may actually do.
# =============================================================================
#
# The trust policy says who; this says what. #173 rejects `s3:*` explicitly, so
# every action below is here because kache 0.14.2 issues the corresponding
# request, and the interesting entries are the ones that are absent.
#
# ## Derived from kache's actual request set, not from a template
#
# kache's S3 remote is OpenDAL's `opendal-service-s3`
# (kache-0.14.2 Cargo.toml:212-214, recorded in docs/spikes/kache-remote-backend.md
# on branch `docs/kache-backend-record`). A build cache does four things:
#
#   probe an object      HEAD Object            -> s3:GetObject
#   fetch an object      GET Object             -> s3:GetObject
#   store an object      PUT Object,            -> s3:PutObject
#                        or Create/UploadPart/
#                        CompleteMultipartUpload
#   clean up a failed    AbortMultipartUpload   -> s3:AbortMultipartUpload
#   upload
#
# All three multipart calls are authorised by `s3:PutObject`; only the abort has
# its own action name. `s3:ListMultipartUploadParts` is included with it because
# a resumed or retried upload lists the parts it already sent, and its absence
# turns a retry into an opaque 403.
#
# ## The absences, which are the point
#
# **No `s3:DeleteObject`.** Nothing in CI should delete a cached object. `kache
# gc` evicts the *local* store only (AGENTS.md § Build cache), and remote
# retention here is the S3 lifecycle rule in s3.tf, which S3 executes with its
# own service identity and not with this role's. So the delete grant would be
# unused — and withholding it means a credential leaked out of a workflow log
# cannot destroy the cache, only add to and read from it. Worst case is a
# storage bill, which the lifecycle rule bounds anyway.
#
# **No bucket administration.** No `s3:PutBucketPolicy`, `s3:PutLifecycle*`,
# `s3:PutBucketPublicAccessBlock`, `s3:DeleteBucket`. CI must not be able to
# switch off its own retention rule or unblock public access. Those belong to
# whoever runs `terraform apply`.
#
# **No `s3:PutObjectAcl`, no ACL grants.** The bucket sets
# BucketOwnerEnforced, which disables ACLs entirely, so an ACL call would fail
# regardless; not granting it keeps the policy honest about that.
#
# **Nothing outside S3.** No EC2, no IAM, no KMS (SSE-S3 needs none — see
# s3.tf). This account carries 25 unrelated pre-existing roles and an IAM user;
# this policy cannot see, read or modify any of them.
#
# **Scoped to one bucket and one prefix**, by ARN, not by convention. There are
# zero other buckets in the account today, but that is a fact about today.
# =============================================================================

data "aws_iam_policy_document" "cache_access" {
  # ---------------------------------------------------------------------------
  # Object-level. This is the statement kache lives on.
  # ---------------------------------------------------------------------------
  statement {
    sid    = "CacheObjectReadWrite"
    effect = "Allow"

    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:AbortMultipartUpload",
      "s3:ListMultipartUploadParts",
    ]

    resources = ["${aws_s3_bucket.kache.arn}/${var.cache_prefix}/*"]
  }

  # ---------------------------------------------------------------------------
  # Bucket-level listing, constrained to the cache prefix.
  #
  # `s3:ListBucket` is a *bucket* action, so its resource is the bucket ARN and
  # the prefix restriction has to be expressed as a condition on `s3:prefix`.
  # Without the condition the role could enumerate every key in the bucket,
  # including a future non-cache prefix (#176's fio qualification output, say)
  # that it has no object permissions on — a small leak, but a free one to
  # close.
  #
  # Both `${prefix}` and `${prefix}/*` are listed: the first matches a request
  # that lists the prefix itself, the second everything under it, including the
  # bare `artifacts/` a lister sends.
  #
  # THIS IS THE CONDITION MOST LIKELY TO BE TOO TIGHT. If a `kache sync` or
  # `kache doctor` against the real bucket returns AccessDenied on a List while
  # Get and Put work, this condition is the first suspect; infra/README.md
  # § Troubleshooting says what to do. kache's build path does not list at all —
  # it addresses objects by content hash — so a failure here degrades
  # diagnostics rather than the cache.
  # ---------------------------------------------------------------------------
  statement {
    sid       = "CacheListPrefixOnly"
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.kache.arn]

    condition {
      test     = "StringLike"
      variable = "s3:prefix"
      values = [
        var.cache_prefix,
        "${var.cache_prefix}/*",
      ]
    }
  }

  # ---------------------------------------------------------------------------
  # Region discovery. The AWS SDK issues GetBucketLocation when it has to
  # resolve or correct a bucket's region; it returns nothing but a region
  # string. Granted so that a region typo in kache's config surfaces as a clear
  # redirect rather than as an AccessDenied that looks like a policy bug.
  # ---------------------------------------------------------------------------
  statement {
    sid       = "BucketRegionDiscovery"
    effect    = "Allow"
    actions   = ["s3:GetBucketLocation"]
    resources = [aws_s3_bucket.kache.arn]
  }
}

resource "aws_iam_policy" "cache_access" {
  name        = "orrery-ci-cache-access"
  description = "Object read/write on the Orrery kache prefix. No delete, no bucket administration."
  policy      = data.aws_iam_policy_document.cache_access.json
}

resource "aws_iam_role_policy_attachment" "cache_access" {
  role       = aws_iam_role.github_cache.name
  policy_arn = aws_iam_policy.cache_access.arn
}

# -----------------------------------------------------------------------------
# NOT PROVISIONED HERE: #176's compute role.
#
# #173's scope names two roles, a cache role and a compute role for the
# ephemeral qualified machines. Only the cache role is here, deliberately.
#
# The compute role's least-privilege shape depends on decisions #176 has not
# made and this PR must not pre-empt: which instance family passes D19's fio
# qualification (the candidate set spans arm64 and x86 and is chosen by
# measurement, per #170), whether access is by SSM Session Manager or a key
# pair (neither exists in the account), and what the launch template and
# instance profile look like. An `ec2:RunInstances` grant written before those
# answers exist would be scoped by guesswork, and #173 says plainly that
# `ec2:*` is not an acceptable answer.
#
# What #176 inherits from this PR is the expensive part: the OIDC provider
# exists, its ARN is an output, and this file is the worked example of the
# scoping standard its policy has to meet. Adding the compute role is then one
# file, not a re-derivation.
# -----------------------------------------------------------------------------
