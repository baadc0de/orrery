variable "region" {
  description = <<-EOT
    Region for the cache bucket. eu-central-1 because #170's account smoke test
    (2026-08-21) verified it usable end to end there — all three AZs available,
    64 on-demand and 64 spot Standard vCPU quota, default VPC present — and
    because #176's ephemeral qualified machines will live in the same region,
    where S3 traffic is free rather than $0.09/GiB. GitHub-hosted runners are
    outside AWS wherever the bucket is, so runner latency does not discriminate
    between EU regions; the co-location with #176 does.
  EOT
  type        = string
  default     = "eu-central-1"
}

variable "expected_account_id" {
  description = "Account this configuration is allowed to touch. Enforced by a postcondition in providers.tf."
  type        = string
  default     = "590561279276"

  validation {
    condition     = can(regex("^[0-9]{12}$", var.expected_account_id))
    error_message = "An AWS account id is exactly 12 digits."
  }
}

variable "github_repository" {
  description = "The repository this identity plane serves, `owner/name`. Documentation and outputs only; the trust policy matches on github_subject_prefix."
  type        = string
  default     = "baadc0de/orrery"
}

variable "github_subject_prefix" {
  description = <<-EOT
    The literal, repository-pinning prefix of the OIDC `sub` claim. Every
    permitted subject in the trust policy is this string plus one of
    allowed_subject_suffixes.

    DO NOT guess this value, and in particular do not write the familiar
    `repo:baadc0de/orrery`. Read it from the repository:

        gh api repos/baadc0de/orrery/actions/oidc/customization/sub

    which returned, this session:

        {"use_default":true,"use_immutable_subject":false,
         "sub_claim_prefix":"repo:baadc0de@15308543/orrery@1331921648"}

    The `@<id>` segments are GitHub's immutable subject format, default for
    repositories created after 2026-07-15; this one was created 2026-08-12.
    The full argument, including why this is a security improvement rather than
    an annoyance, is in the comment block in oidc.tf.

    Re-read the value after any repository rename or transfer: renames after
    2026-07-15 also adopt the immutable format, and the ids are stable across
    them, but the surrounding names in the string are not.
  EOT
  type        = string
  default     = "repo:baadc0de@15308543/orrery@1331921648"

  validation {
    condition     = startswith(var.github_subject_prefix, "repo:") && !strcontains(var.github_subject_prefix, "*")
    error_message = "The subject prefix must start with `repo:` and must contain no wildcard — the wildcard belongs in allowed_subject_suffixes, after the repository is pinned."
  }
}

variable "allowed_subject_suffixes" {
  description = <<-EOT
    The part of the `sub` claim after the repository prefix. Each entry is
    argued individually in oidc.tf; the short form:

      ref:refs/heads/*  pushes, workflow_dispatch, and the nightly schedule
      pull_request      same-repository pull requests
      ref:refs/tags/*   tag builds (none today; included so a release lane does
                        not fail on a credential)

    `environment:*` is deliberately absent. Adding a GitHub Environment to a job
    REPLACES the ref in its subject, so such a job will fail to assume the role
    until the environment is added here on purpose.
  EOT
  type        = list(string)
  default = [
    "ref:refs/heads/*",
    "pull_request",
    "ref:refs/tags/*",
  ]

  validation {
    condition     = length(var.allowed_subject_suffixes) > 0
    error_message = "At least one subject suffix must be permitted, or the role is unassumable."
  }
}

variable "cache_bucket_name" {
  description = <<-EOT
    S3 bucket for the kache remote. S3 bucket names are global across all AWS
    accounts, so this can collide with a stranger's bucket; `aws s3api
    head-bucket --bucket orrery-kache` returned 404 (does not exist anywhere)
    this session, so the name is free. If a future apply fails with
    BucketAlreadyExists, set this variable rather than editing the default —
    the outputs feed #175, so the name must not be hand-copied anywhere.
  EOT
  type        = string
  default     = "orrery-kache"
}

variable "cache_prefix" {
  description = <<-EOT
    Object key prefix inside the bucket. Matches kache's own default
    (`RemoteConfig.prefix`, kache 0.14.2 config.rs:336-341) and the value
    #170's config sketch spells out explicitly.

    The lifecycle expiry rule and the IAM policy are both scoped to this
    prefix, so a future non-cache use of the same bucket (say, a fio
    qualification artifact from #176) would land outside both and would neither
    be expired nor be writable by the CI role — which is the intended
    behaviour, not an oversight.

    No per-toolchain or per-target sub-prefix is imposed. kache keys objects by
    content hash, and the hash already covers the compiler, the target triple
    and the flags; adding a prefix layer would only partition the cache and
    lower the hit rate. Stated here because #174 asks for the layout to be
    chosen rather than inherited silently.
  EOT
  type        = string
  default     = "artifacts"
}

variable "cache_expiration_days" {
  description = <<-EOT
    Age, in days since object CREATION, at which a cached object is deleted.
    The full size-versus-age argument is in s3.tf next to the rule and in
    infra/README.md § Retention; the short version is that S3 lifecycle offers
    no size trigger, creation-age provably fires where the last-read policy
    that produced the 319 GiB incident provably could not, and the resulting
    steady-state size is `daily unique upload volume x this number`.

    14 is a starting value, not a measured one. #175 is the issue that will
    measure the fill rate; when it does, change this number and re-apply.
  EOT
  type        = number
  default     = 14

  validation {
    condition     = var.cache_expiration_days >= 1 && var.cache_expiration_days <= 365
    error_message = "Expiration must be between 1 and 365 days."
  }
}

variable "multipart_abort_days" {
  description = <<-EOT
    Age at which incomplete multipart uploads are aborted. One day. An
    interrupted kache upload — a cancelled workflow, a runner reclaimed
    mid-PUT — leaves parts that are billed at full storage rates and are
    invisible to `list-objects`; the only way to see them is
    `list-multipart-uploads`, which nobody runs. This is the quiet version of
    the same unbounded-growth problem #174 exists to solve.

    One day rather than seven because no legitimate kache upload spans more
    than a few minutes.
  EOT
  type        = number
  default     = 1

  validation {
    condition     = var.multipart_abort_days >= 1
    error_message = "AbortIncompleteMultipartUpload requires at least 1 day."
  }
}
