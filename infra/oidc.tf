# =============================================================================
# #173 — the identity plane: GitHub's OIDC provider, and one federated role.
# =============================================================================

# -----------------------------------------------------------------------------
# The identity provider.
#
# `list-open-id-connect-providers` returned an empty list this session, so this
# creates the account's first one. It is account-global: once it exists, ANY
# role in account 590561279276 may name it as a trusted federated principal.
# That is the reason the trust policy below is where the care goes — the
# provider itself is not a security boundary, it is a statement that GitHub's
# signing keys are recognised. Deleting it would break every federated role in
# the account at once, so it is separated from the role by design and #176 will
# reference it rather than create a second one.
#
# `client_id_list` is the audience. `sts.amazonaws.com` is what
# `aws-actions/configure-aws-credentials` requests by default. Keeping the list
# to exactly one value means a token minted for some other audience — a
# different cloud, a third-party service the repo also federates to — cannot be
# replayed here.
#
# No `thumbprint_list`. Since 2023 AWS validates this issuer against its own
# trusted CA store and ignores the thumbprints; the provider's schema marks the
# argument Optional+Computed, so omitting it lets AWS supply the value and
# removes a hard-coded fingerprint that would otherwise rot on a CA rotation
# and look like a security control while doing nothing.
# -----------------------------------------------------------------------------
resource "aws_iam_openid_connect_provider" "github" {
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
}

# -----------------------------------------------------------------------------
# THE TRUST POLICY. Read this before anything else in the directory.
# -----------------------------------------------------------------------------
#
# ## The `sub` value, and why the obvious one is wrong here
#
# Every widely-copied GitHub-to-AWS OIDC snippet writes the subject as
# `repo:OWNER/REPO:...`. **For this repository that string never appears in a
# token, and a role trusting it could not be assumed at all.**
#
# GitHub changed the default subject format for repositories created after
# 2026-07-15 to embed immutable numeric ids
# (https://github.blog/changelog/2026-04-23-immutable-subject-claims-for-github-actions-oidc-tokens/).
# `baadc0de/orrery` was created 2026-08-12, so it is on the new format. This is
# not inferred from the date — it was read from the repository itself:
#
#     $ gh api repos/baadc0de/orrery/actions/oidc/customization/sub
#     {"use_default":true,"use_immutable_subject":false,
#      "sub_claim_prefix":"repo:baadc0de@15308543/orrery@1331921648"}
#
# `sub_claim_prefix` is authoritative and is what `var.github_subject_prefix`
# carries. (`use_immutable_subject: false` is the *opt-in toggle for older
# repositories*; it is false because this repository never had to opt in.)
#
# This lands well: the numeric ids are exactly the hardening we would otherwise
# want and cannot get. AWS does **not** expose GitHub's `repository_id` or
# `repository_owner_id` claims as IAM condition keys — the AWS IAM condition-key
# reference lists only `amr`, `aud`, `email`, `oaud` and `sub` for OIDC
# federation, so `token.actions.githubusercontent.com:repository_owner_id`,
# which several blog posts recommend, silently matches nothing and would deny
# every assumption. Here the ids ride inside `sub`, where they do work. A
# name-based subject would have been re-claimable: delete the `baadc0de`
# account, let a stranger register the login and a repository called `orrery`,
# and their tokens would be byte-identical to ours. The numeric form closes it.
#
# ## What is permitted
#
# Three subject suffixes, each with a reason it must be there:
#
#   `:ref:refs/heads/*`  Pushes and `workflow_dispatch` on any branch, and the
#                        nightly `schedule` event, which GitHub always runs on
#                        the default branch. Not narrowed to `main`: the cache
#                        exists to serve feature branches, and #175 will be
#                        tested by dispatching its workflow on a branch before
#                        it merges. Narrowing to `main` would make that fail
#                        with an AccessDenied several minutes into a job, which
#                        is the expensive kind of wrong. The population that can
#                        push a branch here is the population with write access
#                        to the repository — narrowing to `main` does not shrink
#                        it, it only shrinks where they can work.
#
#   `:pull_request`      Same-repository pull requests, which is where nearly
#                        every build actually happens. This is the value that
#                        the fork question turns on; see below.
#
#   `:ref:refs/tags/*`   Tag builds. None exist today. Included because their
#                        absence is a latent trap: a release lane added later
#                        would fail on a credential rather than on its own
#                        merits, and diagnosing it costs more than the marginal
#                        grant, which is again bounded by "can push a tag here".
#
# ## What is deliberately NOT permitted, and the consequences
#
#   `:environment:*`     Omitted. GitHub Environments **replace** the ref in the
#                        subject rather than adding to it, so the day a job
#                        gains `environment: production` its subject stops
#                        matching this policy and it fails with:
#
#                          Not authorized to perform sts:AssumeRoleWithWebIdentity
#
#                        That is a deliberate trip-wire, not an oversight. This
#                        repository has no Environments today; an Environment
#                        with required reviewers is the *right* place to gate
#                        cloud access, and adding one should be a conscious
#                        change to this list rather than something that
#                        inherits an existing grant. It is called out in
#                        infra/README.md so the failure is diagnosable in
#                        seconds.
#
#   `job_workflow_ref`   Not usable — see the AWS condition-key note above. If
#                        it were, scoping to a single reusable workflow would be
#                        the next tightening available.
#
# ## Blast radius, stated plainly
#
# A principal that satisfies this condition can read and write objects under
# `s3://orrery-kache/artifacts/*`. It cannot delete them, cannot touch the
# bucket's configuration, cannot see any other bucket (there are none), and has
# no permission of any kind outside S3 — importantly, none against the 25
# pre-existing roles, 1 IAM user and 9 customer policies this shared account
# carries for unrelated workloads.
#
# Who can become such a principal: anyone who can cause a workflow to run in
# `baadc0de/orrery` with `id-token: write`, i.e. anyone with write access to the
# repository. Since the repository is public, the fork case is the one that
# matters, and it has two independent answers. GitHub does not grant
# `id-token: write` to a workflow triggered by a pull request from a fork — the
# token is never minted, so there is nothing to present. And the repository's
# Actions setting requires approval for all outside collaborators
# (`.github/workflows/ci.yml:78-85`), so a fork workflow does not start
# unapproved. Note what ci.yml:78-85 says about layering: neither of these is
# the runner-label guard, and none of the three should be mistaken for the
# only one.
#
# The residual exposure a reviewer should weigh: a maintainer's compromised
# GitHub account, or a malicious commit merged to a branch, yields the ability
# to fill and read a build cache. Cache objects are content-addressed
# compiler outputs. Poisoning one requires a hash collision; reading them
# reveals compiled artifacts of a public repository. That is the whole grant.
# -----------------------------------------------------------------------------

locals {
  # Rendered subjects, so the plan output shows the literal strings a reviewer
  # can compare against a real token's `sub`.
  allowed_subjects = [
    for suffix in var.allowed_subject_suffixes :
    "${var.github_subject_prefix}:${suffix}"
  ]
}

data "aws_iam_policy_document" "github_cache_trust" {
  statement {
    sid     = "GitHubActionsWebIdentity"
    effect  = "Allow"
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.github.arn]
    }

    # Audience. Without this, a token minted for ANY audience by ANY GitHub
    # repository could be presented; `aud` is what binds the token to AWS. It is
    # StringEquals, never StringLike.
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }

    # Subject. StringLike because two of the three permitted values contain a
    # `*`. The wildcard only ever appears *after* the repository-pinning prefix
    # — `${var.github_subject_prefix}` is a literal with no metacharacters — so
    # the wildcard can never widen the match past this one repository.
    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values   = local.allowed_subjects
    }
  }
}

resource "aws_iam_role" "github_cache" {
  name        = "orrery-ci-cache"
  description = "Read/write the Orrery kache S3 remote. Assumed from GitHub Actions OIDC; see infra/oidc.tf for the trust conditions."

  assume_role_policy = data.aws_iam_policy_document.github_cache_trust.json

  # An hour. `configure-aws-credentials` requests session credentials for the
  # job; the longest lane measured in #171 was 674 s cold, so an hour is ample
  # with room for the cache to make jobs slower before it makes them faster.
  # The default is also an hour; it is pinned so that a future AWS default
  # change cannot silently lengthen a CI credential's life.
  max_session_duration = 3600

  # No permissions boundary. Stated rather than omitted: the attached policy is
  # already the whole grant (four object actions on one prefix), so a boundary
  # would restate it without constraining anything. If this role ever grows a
  # second policy, revisit that judgement.
}
