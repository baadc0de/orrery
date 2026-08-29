# =============================================================================
# #173 — the compute role: what #176's ephemeral qualified machines need.
# =============================================================================
#
# The trust policy says who; this says what. It is the second of the two roles
# #173 names, the one iam-cache-policy.tf deliberately deferred until now.
#
# ## What this role exists to do
#
# Launch one ephemeral EC2 instance in `eu-central-1`, let a gate run on it,
# and terminate that instance — nothing more. That is the whole of what #176
# needs from cloud IAM: the P2 kill-9 harness needs a device that passes D19's
# fio qualification, the qualification is performed *on* the machine, and the
# machine is thrown away afterwards (the owner's standing decision: gates
# provision infrastructure per run and discard it; there is no long-running
# reference anything).
#
# ## The trust policy, and why it is tighter than the cache role's
#
# The cache role may be assumed from any branch, a tag, or a same-repository
# pull request, because a build cache serves exactly those. This role is
# scoped to `ref:refs/heads/main` only:
#
#   `ref:refs/heads/main`  Pushes to main, `workflow_dispatch` on main, and
#                          the nightly `schedule`, which GitHub always runs on
#                          the default branch — all three carry exactly this
#                          subject. Everything that launches an instance costs
#                          money and leaves a running process behind if it is
#                          not torn down, so the population allowed to launch
#                          is the population whose code has passed review and
#                          merged. A feature branch cannot test-drive the
#                          ephemeral flow before merging; that is the price,
#                          stated rather than hidden. If it ever bites, widen
#                          `compute_allowed_subject_suffixes` consciously —
#                          it is one variable, not a console click.
#
# Deliberately absent, each for a stated reason:
#
#   `pull_request`        A pull request — fork OR same-repository — is where
#                         arbitrary code lands before review. The cache role's
#                         grant is bounded by a prefix; this role's grant
#                         creates billable machines, so unreviewed code does
#                         not get it at all. Fork pull requests are excluded
#                         twice over regardless: GitHub never mints the OIDC
#                         token for them, and their subjects name another
#                         repository anyway (see the immutable-prefix note in
#                         oidc.tf — the familiar repo:name form matches
#                         nothing here either).
#
#   `ref:refs/tags/*`     No release lane runs instances.
#
#   `environment:*`       Same trip-wire as the cache role: GitHub Environments
#                         REPLACE the ref in the subject, so a job moved under
#                         an Environment fails here until added on purpose —
#                         which, with required reviewers, is the right place
#                         to gate machine-launching if this ever feels too
#                         wide.
#
# ## The permission policy, derived from the lifecycle, not from a template
#
# An ephemeral machine has four moments: discover, launch tagged, read the
# tagged instance's serial-console evidence, terminate what you tagged. Each
# statement below is one of those moments, plus one explicit deny. The
# absences carry as much weight:
#
# **No StartInstances / StopInstances.** An ephemeral instance is launched,
# used and terminated. Pause/resume would let an idle machine accumulate cost
# indefinitely while every tool still calls it "running since Tuesday"; with
# no stop grant, every machine is either running work or terminating. #176 also
# sets instance-initiated-shutdown-behavior=terminate, so cloud-init's EXIT
# trap is independent of the controller's tagged teardown.
#
# **No CreateKeyPair, no ssm:*.** #176 resolves the access question without a
# login channel: user data starts a public-repository workload pinned to
# GITHUB_SHA, and one compact result comes back through GetConsoleOutput. The
# instance receives no AWS identity at all, so there is no profile to leak and
# no second credential path to audit.
#
# **No DeleteVolume, no DeleteNetworkInterface.** The root volume rides
# delete-on-termination and a NIC created by RunInstances is reaped with the
# instance, so cleanup is automatic; granting explicit deletes would buy
# nothing and add the one action class that makes a leaked credential able to
# destroy state it did not create.
#
# **Nothing outside EC2 describe-and-lifecycle.** No S3 (the cache role owns
# that), no IAM, no STS beyond the trust itself. In particular none of the 25
# pre-existing roles, 1 IAM user and 9 customer policies are reachable.
#
# ## The tag chain, which is the actual security argument
#
# Three conditions interlock, and removing any one of them breaks the design:
#
#   1. RunInstances requires `aws:RequestTag/orrery-ci-ephemeral = true`.
#      Nothing launchable by CI exists untagged — so there is no orphaned
#      instance CI cannot clean up after itself.
#   2. CreateTags is confined to `ec2:CreateAction = RunInstances`. Without
#      this guard, a compromised job could retag ANY instance in the region —
#      including one it did not create — with the ephemeral tag and then
#      terminate it through statement 3. The retag-and-kill chain is closed
#      at step 1 of that pair, not at the kill.
#   3. TerminateInstances requires `aws:ResourceTag/orrery-ci-ephemeral =
#      true`. CI can only kill what carries the tag, and by (1)+(2) only CI
#      can put the tag on a live instance during its own launch. Anything
#      pre-existing in the account is untouchable.
#
# Net property, worth saying plainly: everything CI can create, CI can delete;
# everything CI can delete, CI created. The blast radius of a leaked compute
# credential is the instances of one nightly run.
#
# ## What is and is not pinned down, stated for the reviewer
#
# **The AMI ids are pinned and reviewed.** `UseCanonicalUbuntuImage` names the
# exact images this credential may boot, from `var.compute_reviewed_image_ids`.
# This replaced an owner condition that did not work: `ec2:Owner` evaluates to
# an image's owner ALIAS when it has one, and Canonical's ubuntu-noble images
# carry `ImageOwnerAlias amazon` beside `OwnerId 099720109477`, so the
# condition matched nothing and every launch was denied. The pin costs a
# refresh when Canonical publishes a security update -- a stale pin fails
# closed, which is the right direction -- and it is strictly tighter than the
# owner condition was, so #173's licence-cost exposure stays closed.
#
# **Subnet/security-group resources are account-scoped, not topology-scoped.**
# Pinning them means reading pre-existing VPC data into this module, which
# infra/README.md rules out ("nothing pre-existing is read"), and pre-empts
# a topology decision into identity. The default VPC is the only network this
# account has today, so the marginal grant is near-empty.
#
# **Instance types are capability-derived, not preference-derived.** The list
# is #170's measured candidate set — every family in eu-central-1 reporting
# NvmeSupport=required — because D19 qualifies devices by measurement, and
# #176 measures the whole set (~$9 on-demand / ~$3 spot per hour-long pass)
# rather than reasoning families out first. AWS will add families; widening
# this variable after re-running #170's discovery query is the conscious act,
# and until then new families fail closed.
# =============================================================================

locals {
  # Rendered subjects, so the plan output shows the literal strings a reviewer
  # can compare against a real token's `sub` — the same convention oidc.tf uses
  # for the cache role.
  allowed_compute_subjects = [
    for suffix in var.compute_allowed_subject_suffixes :
    "${var.github_subject_prefix}:${suffix}"
  ]

  # One spelling of the ownership tag, used by all three statements of the
  # chain above. A drift between "what launch requires" and "what terminate
  # accepts" would strand an instance nobody's credential could kill.
  ephemeral_tag_key   = "orrery-ci-ephemeral"
  ephemeral_tag_value = "true"
}

data "aws_iam_policy_document" "github_compute_trust" {
  statement {
    sid     = "GitHubActionsWebIdentityCompute"
    effect  = "Allow"
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.github.arn]
    }

    # Audience, StringEquals, exactly as the cache role pins it: without this,
    # a token minted for ANY audience by ANY GitHub repository could be
    # presented here.
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }

    # Subject. StringLike for symmetry with the cache role, but note there is
    # no wildcard in any permitted value: the single suffix is exact, so this
    # is effectively StringEquals over one literal string. The repository-
    # pinning prefix carries the numeric ids and contains no metacharacters.
    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values   = local.allowed_compute_subjects
    }
  }
}

data "aws_iam_policy_document" "compute_access" {
  # ---------------------------------------------------------------------------
  # Explicit deny first: metal sizes are the most expensive shapes matching
  # any family glob below (`c6id.metal-32xl` and friends are dollars per hour,
  # not cents). A Deny outranks every Allow, so the allow-list stays readable
  # as globs while the expensive corner is cut off outright. This also covers
  # u-*.metal and mac*.metal, which no family pattern would have matched
  # anyway — they were never launchable here.
  # ---------------------------------------------------------------------------
  statement {
    sid       = "NoMetalSizes"
    effect    = "Deny"
    actions   = ["ec2:RunInstances"]
    resources = ["*"]

    condition {
      test     = "StringLike"
      variable = "ec2:InstanceType"
      values   = ["*.metal"]
    }
  }

  # ---------------------------------------------------------------------------
  # Discovery. Read-only, and the part #176's qualification procedure runs
  # first: the capability filter query from #170 that produces the candidate
  # set lives entirely inside DescribeInstanceTypes. DescribeImages resolves
  # whatever base image #176 picks; DescribeVPC/Subnet/SecurityGroup locate
  # the default-VPC pieces a RunInstances call must name. None of these
  # actions accepts a resource restriction, hence "*"; the region condition
  # keeps even reads from wandering outside eu-central-1.
  # ---------------------------------------------------------------------------
  statement {
    sid    = "DiscoveryReadOnly"
    effect = "Allow"

    actions = [
      "ec2:DescribeInstanceTypes",
      "ec2:DescribeImages",
      "ec2:DescribeAvailabilityZones",
      "ec2:DescribeVpcs",
      "ec2:DescribeSubnets",
      "ec2:DescribeSecurityGroups",
      "ec2:DescribeInstances",
      "ec2:DescribeInstanceStatus",
    ]

    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }

  # ---------------------------------------------------------------------------
  # Launch, and the two conditions that make it least-privilege in fact
  # rather than in intention: the type allow-list bounds WHAT can run, and
  # the required tag guarantees the thing can be killed again by the same
  # credential (statement TerminateTaggedOnly) — see the tag-chain argument
  # in the header.
  #
  # Resource list follows AWS's own RunInstances example: the instance plus
  # every subordinate object the call creates. snapshot/* is present because
  # AMI block-device mappings resolve through snapshots, and omitting it turns
  # some AMI choices into an opaque AccessDenied; alone it grants nothing.
  # Subnet/security-group/image stay account-scoped rather than id-pinned —
  # the reasons are in the header, under "honestly NOT pinned down".
  # ---------------------------------------------------------------------------
  # RunInstances evaluates every named resource separately. Public AMIs and
  # their snapshots do not carry our ownership request tag, so they have their
  # own grant, pinned to Canonical's publisher account. This also closes #173's
  # recorded licensed-AMI exposure.
  statement {
    sid     = "UseCanonicalUbuntuImage"
    effect  = "Allow"
    actions = ["ec2:RunInstances"]

    # Reviewed ids, not an owner condition. `ec2:Owner` evaluates to the
    # image's owner ALIAS when it has one, and Canonical's ubuntu-noble
    # images report `ImageOwnerAlias amazon` beside `OwnerId 099720109477` --
    # so `ec2:Owner = "099720109477"` silently matched nothing and the gate
    # was denied for five nights. Accepting `amazon` instead would have
    # granted every Amazon-aliased public image.
    #
    # Snapshots stay a wildcard: RunInstances authorises the backing snapshot
    # separately, its id is not knowable from the AMI id without a lookup this
    # module may not make, and a snapshot cannot be booted on its own.
    resources = concat(
      [for id in var.compute_reviewed_image_ids : "arn:aws:ec2:*::image/${id}"],
      ["arn:aws:ec2:*::snapshot/*"],
    )

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }

  # Existing default-VPC objects are inputs to the launch, not things it
  # creates. They cannot carry aws:RequestTag, so keep this grant separate from
  # the created-resource statement below.
  statement {
    sid     = "UseVpcLaunchInputs"
    effect  = "Allow"
    actions = ["ec2:RunInstances"]

    resources = [
      "arn:aws:ec2:*:${var.expected_account_id}:security-group/*",
      "arn:aws:ec2:*:${var.expected_account_id}:subnet/*",
    ]

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }

  statement {
    sid     = "LaunchTaggedInstance"
    effect  = "Allow"
    actions = ["ec2:RunInstances"]

    # **instance/* only.** RunInstances authorises every resource it touches
    # separately, and `ec2:InstanceType` exists in the request context only for
    # the instance itself. A statement carrying that condition therefore cannot
    # match `volume/*`, `network-interface/*` or `spot-instances-request/*` --
    # the key is absent, StringLike cannot match, the statement does not apply,
    # and the launch dies on an implicit deny with an empty matchedStatements.
    #
    # That is not hypothetical: it is how #176's first live run failed. All 47
    # candidates errored, including every family inside the allow-list, and the
    # decoded authorization message read
    #   action RunInstances, resource network-interface/*, matchedStatements {}
    # The allow-list was never the problem; this statement simply did not reach
    # the supporting resources.
    resources = ["arn:aws:ec2:*:${var.expected_account_id}:instance/*"]

    # The candidate set: local-NVMe families measured in eu-central-1, sizes
    # wildcarded. Provenance and the widening procedure are documented on the
    # variable in variables.tf.
    condition {
      test     = "StringLike"
      variable = "ec2:InstanceType"
      values   = var.compute_instance_type_patterns
    }

    # Tag at creation or do not create. Every created resource type in the
    # controller's TagSpecifications carries this tag.
    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${local.ephemeral_tag_key}"
      values   = [local.ephemeral_tag_value]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }

  # ---------------------------------------------------------------------------
  # The supporting resources a launch creates alongside the instance: its root
  # volume, its ENI, and the spot request that asked for it.
  #
  # Same ownership tag, same region, and deliberately **no** `ec2:InstanceType`
  # condition -- see the statement above for why including it here denies the
  # whole launch. The type allow-list is not weakened by its absence: the
  # instance itself is still gated on it by `LaunchTaggedInstance`, and
  # `NoMetalSizes` denies outright, so no instance of an unlisted type can be
  # created no matter what these resources permit. A volume or ENI cannot exist
  # without an instance to attach to.
  # ---------------------------------------------------------------------------
  statement {
    sid     = "LaunchTaggedSupportingResources"
    effect  = "Allow"
    actions = ["ec2:RunInstances"]

    resources = [
      "arn:aws:ec2:*:${var.expected_account_id}:volume/*",
      "arn:aws:ec2:*:${var.expected_account_id}:network-interface/*",
      "arn:aws:ec2:*:${var.expected_account_id}:spot-instances-request/*",
    ]

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/${local.ephemeral_tag_key}"
      values   = [local.ephemeral_tag_value]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }

  # ---------------------------------------------------------------------------
  # Evidence return. The instance has no SSH key, SSM profile, AWS credential
  # or bucket grant. Its cloud-init process emits one compact, base64-encoded
  # result to the serial console; the controller may read that console only on
  # an instance carrying the same ownership tag termination requires.
  # ---------------------------------------------------------------------------
  statement {
    sid       = "ReadTaggedConsoleEvidence"
    effect    = "Allow"
    actions   = ["ec2:GetConsoleOutput"]
    resources = ["arn:aws:ec2:*:${var.expected_account_id}:instance/*"]

    condition {
      test     = "StringEquals"
      variable = "aws:ResourceTag/${local.ephemeral_tag_key}"
      values   = [local.ephemeral_tag_value]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }

  # ---------------------------------------------------------------------------
  # Tagging, confined to the moment of launch. ec2:CreateAction is the
  # condition key that closes the retag-and-kill chain described in the
  # header: tags may only be created BY a RunInstances call, never onto an
  # existing object afterwards.
  # ---------------------------------------------------------------------------
  statement {
    sid       = "TagOnlyAtLaunch"
    effect    = "Allow"
    actions   = ["ec2:CreateTags"]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = ["RunInstances"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }

  # ---------------------------------------------------------------------------
  # Teardown, guarded by ownership. Only objects carrying the ephemeral tag
  # are terminable, and by the two statements above only a RunInstances call
  # from this very policy could have applied it. Note the action name is the
  # plural TerminateInstances — the singular form silently authorises nothing.
  # ---------------------------------------------------------------------------
  statement {
    sid       = "TerminateTaggedOnly"
    effect    = "Allow"
    actions   = ["ec2:TerminateInstances"]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "aws:ResourceTag/${local.ephemeral_tag_key}"
      values   = [local.ephemeral_tag_value]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestedRegion"
      values   = [var.region]
    }
  }
}

resource "aws_iam_role" "github_compute" {
  name        = "orrery-ci-compute"
  description = "Launch, tag and terminate #176's ephemeral qualified machines. Assumed from GitHub Actions OIDC, refs/heads/main only; see infra/iam-compute-policy.tf for the trust conditions and the tag chain."

  assume_role_policy = data.aws_iam_policy_document.github_compute_trust.json

  # One hour, pinned like the cache role's: the longest consumer is p2-kill9,
  # whose job timeout is also 60 minutes, so a credential cannot outlive the
  # work it was minted for. If a future harness needs longer, raise both
  # together and say why.
  max_session_duration = 3600

  # No permissions boundary — same judgement as the cache role: the attached
  # policy IS the whole grant, so a boundary would restate it. Revisit if a
  # second policy is ever attached.
}

resource "aws_iam_policy" "compute_access" {
  name        = "orrery-ci-compute-access"
  description = "EC2 discovery, tagged launch, tagged console evidence and tagged termination, in one region. No start/stop, no deletes, no keys, no SSM."
  policy      = data.aws_iam_policy_document.compute_access.json
}

resource "aws_iam_role_policy_attachment" "compute_access" {
  role       = aws_iam_role.github_compute.name
  policy_arn = aws_iam_policy.compute_access.arn
}
