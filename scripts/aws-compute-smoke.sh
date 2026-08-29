#!/usr/bin/env bash
# #173's compute-credential proof: assume orrery-ci-compute, then show what
# the credential may do — and, just as hard, what it must refuse.
#
# ── Why this script exists twice-over ────────────────────────────────────────
#
# The nightly job `compute-identity-smoke` runs it in Actions, right after
# .github/actions/aws-compute-role assumes the role. scripts/gate-status.sh's
# trio for that job delegates to this same script rather than restating its
# logic inline — the determinism-soak lesson (AGENTS.md §gate-status): a gate
# whose body lives in a workflow is one edit away from drifting out of its own
# report.
#
# ── What it asserts ──────────────────────────────────────────────────────────
#
# Positive, each side-effect-free:
#   P1  the session really is assumed-role/orrery-ci-compute/* (sts)
#   P2  the #170 discovery query works (DescribeInstanceTypes), and returns a
#       plausible number of local-NVMe candidates — dozens in eu-central-1,
#       so a floor catches an empty result reading as success
#   P3  base-image resolution works (DescribeImages)
#
# Negative, each of which MUST be refused by IAM, and every one of which is
# side-effect-free *by construction*, not by good luck:
#   N1  s3api list-buckets               -> AccessDenied   (no S3 grant at all)
#         This has no bucket-existence precondition: a missing cache bucket
#         must not be able to turn an IAM proof into NoSuchBucket.
#   N2  iam list-roles                   -> AccessDenied   (no IAM grant)
#   N3  run-instances --dry-run t3.micro -> UnauthorizedOperation
#         The instance-type allow-list must refuse a family outside it.
#         --dry-run makes EC2 evaluate authorisation without executing, so
#         even a policy bug cannot launch anything here. The assertion is on
#         the error string, not the exit code: DryRunOperation or a NotFound
#         would mean the request was AUTHORISED, which is exactly the failure
#         this probe exists to catch.
#
# Policy document shape, not a live AWS probe:
#   S1  `TerminateTaggedOnly` permits `ec2:TerminateInstances` only when
#       aws:ResourceTag/orrery-ci-ephemeral=true.
#       This proves the checked-out Terraform policy still expresses the tag
#       guard. It does NOT prove how EC2 evaluates that condition against a
#       real untagged instance, nor that the checked-out policy is deployed.
#       #622 removed N4 because EC2 now resolves a synthetic instance's
#       existence before evaluating the tag condition; `NotFound` from that
#       request therefore proves neither an allow nor a denial.
#   S2  `TagOnlyAtLaunch` is the sole `ec2:CreateTags` grant and requires
#       ec2:CreateAction=RunInstances.
#       This likewise proves only the checked-out Terraform source. #679
#       removed the live retag-outside-launch probe because EC2 now resolves
#       its synthetic instance ID before evaluating that condition.
#
# Every figure lands in $COMPUTE_SMOKE_OUT/result.json, and the PASSED marker
# is written last — a run killed mid-way leaves no marker, and evidence
# readers treat its absence as failure.
#
# ── --self-test ──────────────────────────────────────────────────────────────
#
# The structural half, runnable per-commit with no AWS anything: the Terraform
# sources must still contain the clauses these probes assert at runtime, and
# the workflow plumbing (composite action, nightly job, gate-status trio,
# check.sh coverage) must still be wired to this script. Two rules from the
# house style are load-bearing here:
#
#   * the haystacks are OTHER files — infra/*.tf, the composite action, the
#     workflows, the two scripts — never this script's own body, so no clause
#     can pass by matching its own check line;
#   * Terraform is grepped with comment lines STRIPPED, because several
#     arguments (why pull_request is excluded, why metal is denied) are made
#     in prose, and a prose mention would satisfy a polarity-inverted check
#     without the code carrying it.
#
# Structural passing does not prove AWS enforces anything. N1–N3 prove their
# refusals nightly against the real service; S1–S2 prove only the Terraform
# policy shape for the two halves of the tag chain. The two kinds of check fail
# in different places by design.
set -euo pipefail

readonly NAME=aws-compute-smoke
die() { echo "$NAME: $*" >&2; exit 2; }

REGION=${ORRERY_AWS_REGION:-eu-central-1}
ROLE_NAME=${ORRERY_COMPUTE_ROLE_NAME:-orrery-ci-compute}
OUT=${COMPUTE_SMOKE_OUT:-target/compute-identity-smoke}

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# #622's replacement for the former live "terminate untagged instance" probe.
#
# EC2 now looks up a synthetic instance ID before it evaluates the
# aws:ResourceTag condition, so that call returns NotFound regardless of
# whether the tag guard exists. Widening its accepted errors would be a green
# check that asserts nothing. Read the policy document instead, and say
# precisely what that means: this checks the source policy shape, not EC2's
# evaluation order or the currently deployed policy.
assert_termination_policy_shape() {
  local tf="$ROOT/infra/iam-compute-policy.tf"
  local code termination terminate_action_count

  [[ -r $tf ]] || die "policy-shape check: $tf is not readable"
  code=$(grep -v '^[[:space:]]*#' "$tf")

  # Pull exactly one Terraform `statement` block, rather than grepping the
  # whole document: ReadTaggedConsoleEvidence carries the same resource-tag
  # condition, which must not make a missing termination condition pass.
  if ! termination=$(awk '
    /^[[:space:]]*statement[[:space:]]*\{/ {
      in_statement = 1
      depth = 0
      matches_sid = 0
      block = ""
    }
    in_statement {
      block = block $0 ORS
      if ($0 ~ /sid[[:space:]]*=[[:space:]]*"TerminateTaggedOnly"/) {
        matches_sid = 1
      }
      opens = gsub(/\{/, "{")
      closes = gsub(/\}/, "}")
      depth += opens - closes
      if (depth == 0) {
        if (matches_sid) {
          print block
          found = 1
          exit
        }
        in_statement = 0
      }
    }
    END { if (!found) exit 1 }
  ' <<<"$code"); then
    die 'policy-shape check: TerminateTaggedOnly statement is missing — cannot prove termination is tag-guarded'
  fi

  terminate_action_count=$(grep -Fc '"ec2:TerminateInstances"' <<<"$code" || true)
  [[ $terminate_action_count == 1 ]] || die "policy-shape check: found $terminate_action_count ec2:TerminateInstances grants; expected exactly one tag-guarded grant"

  termination_has() { # needle explanation
    if ! grep -Fq -- "$1" <<<"$termination"; then
      die "policy-shape check: TerminateTaggedOnly lacks '$1' — $2"
    fi
  }

  termination_has 'effect    = "Allow"' \
    'termination is no longer an explicit allow statement'
  termination_has 'actions   = ["ec2:TerminateInstances"]' \
    'the one termination grant has changed shape'
  termination_has 'variable = "aws:ResourceTag/${local.ephemeral_tag_key}"' \
    'termination is no longer conditioned on the ephemeral ownership tag'
  termination_has 'values   = [local.ephemeral_tag_value]' \
    'termination no longer requires the ephemeral ownership-tag value'

  echo "$NAME: POLICY-SHAPE PASSED: TerminateTaggedOnly is the sole termination grant and requires aws:ResourceTag/orrery-ci-ephemeral=true (Terraform source only; not EC2 evaluation or deployed-policy proof)"
}

# #679's replacement for the former live "create-tags outside launch" probe.
#
# EC2 now looks up that probe's synthetic instance ID before it evaluates the
# ec2:CreateAction condition, so InvalidInstanceID.NotFound says nothing about
# whether tagging an existing object is allowed. Accepting NotFound would make
# the check green while asserting nothing. Read exactly the CreateTags statement
# in the policy instead; as with the termination assertion above, this proves
# source shape, not EC2 evaluation order or the currently deployed policy.
assert_create_tags_policy_shape() {
  local tf="$ROOT/infra/iam-compute-policy.tf"
  local code create_tags create_tags_action_count

  [[ -r $tf ]] || die "policy-shape check: $tf is not readable"
  code=$(grep -v '^[[:space:]]*#' "$tf")

  # Pull exactly one Terraform `statement` block, rather than grepping the
  # whole document: another statement could carry an ec2:CreateAction
  # condition and must not make a missing TagOnlyAtLaunch condition pass.
  if ! create_tags=$(awk '
    /^[[:space:]]*statement[[:space:]]*\{/ {
      in_statement = 1
      depth = 0
      matches_sid = 0
      block = ""
    }
    in_statement {
      block = block $0 ORS
      if ($0 ~ /sid[[:space:]]*=[[:space:]]*"TagOnlyAtLaunch"/) {
        matches_sid = 1
      }
      opens = gsub(/\{/, "{")
      closes = gsub(/\}/, "}")
      depth += opens - closes
      if (depth == 0) {
        if (matches_sid) {
          print block
          found = 1
          exit
        }
        in_statement = 0
      }
    }
    END { if (!found) exit 1 }
  ' <<<"$code"); then
    die 'policy-shape check: TagOnlyAtLaunch statement is missing — cannot prove CreateTags is confined to RunInstances'
  fi

  create_tags_action_count=$(grep -Fc '"ec2:CreateTags"' <<<"$code" || true)
  [[ $create_tags_action_count == 1 ]] || die "policy-shape check: found $create_tags_action_count ec2:CreateTags grants; expected exactly one launch-guarded grant"

  create_tags_has() { # needle explanation
    if ! grep -Fq -- "$1" <<<"$create_tags"; then
      die "policy-shape check: TagOnlyAtLaunch lacks '$1' — $2"
    fi
  }

  create_tags_has 'effect    = "Allow"' \
    'CreateTags is no longer an explicit allow statement'
  create_tags_has 'actions   = ["ec2:CreateTags"]' \
    'the one CreateTags grant has changed shape'
  create_tags_has 'variable = "ec2:CreateAction"' \
    'CreateTags is no longer confined to a creation action'
  create_tags_has 'values   = ["RunInstances"]' \
    'CreateTags is no longer confined to RunInstances'

  echo "$NAME: POLICY-SHAPE PASSED: TagOnlyAtLaunch is the sole CreateTags grant and requires ec2:CreateAction=RunInstances (Terraform source only; not EC2 evaluation or deployed-policy proof)"
}

# ─────────────────────────────────────────────────────────────────────────────
# --self-test: structure, no cloud.
# ─────────────────────────────────────────────────────────────────────────────

self_test() {
  local tf="$ROOT/infra/iam-compute-policy.tf"
  local vars="$ROOT/infra/variables.tf"
  local action="$ROOT/.github/actions/aws-compute-role/action.yml"
  local nightly="$ROOT/.github/workflows/nightly.yml"

  [[ -r $tf ]] || die "self-test: $tf is not readable"
  [[ -r $vars ]] || die "self-test: $vars is not readable"
  [[ -r $action ]] || die "self-test: $action is not readable"
  [[ -r $nightly ]] || die "self-test: $nightly is not readable"

  # Code only: the trust/policy ARGUMENTS live in comments, and a check on
  # commented prose would pass whether or not the code carried the clause.
  local code
  code=$(grep -v '^[[:space:]]*#' "$tf")

  has_code() { grep -Fq -- "$1" <<<"$code" || die "self-test: iam-compute-policy.tf no longer contains '$1' — $2"; }
  # If/fi rather than `grep && die`: in the good case grep fails, and a bare
  # &&-list makes this function RETURN 1 — which then kills self_test under
  # set -e as a failed simple command, silently. Measured, on the first run.
  lacks_code() {
    if grep -Fq -- "$1" <<<"$code"; then
      die "self-test: iam-compute-policy.tf contains '$1' — $2"
    fi
  }

  # The trust policy still binds audience and subject.
  has_code 'token.actions.githubusercontent.com:aud' \
    'a token minted for any audience could be presented'
  has_code '"sts.amazonaws.com"' \
    'the audience pin is gone'
  has_code 'local.allowed_compute_subjects' \
    'the sub condition no longer comes from the rendered subject list'
  has_code '${var.github_subject_prefix}' \
    'the repository-pinning prefix is gone from the subject rendering'

  # What must never be permitted to assume it.
  lacks_code 'pull_request' \
    'pull requests may assume the compute role; unreviewed code must not launch machines'
  lacks_code 'refs/tags' \
    'tag builds may assume the compute role'

  # The tag chain: launch forces the tag, tagging stays inside launch,
  # termination accepts only tagged objects. Remove any leg and the property
  # "everything CI creates, CI can delete; nothing else is touchable" breaks.
  has_code '"ec2:RunInstances"' \
    'there is no launch grant'
  has_code 'aws:RequestTag/' \
    'launch no longer forces the ownership tag, so CI could create instances it cannot kill'
  assert_termination_policy_shape
  assert_create_tags_policy_shape

  # Least privilege, negative space.
  has_code '"*.metal"' \
    'the metal-size deny is gone; the most expensive shapes match the family globs again'
  has_code 'var.compute_instance_type_patterns' \
    'the launch grant no longer consults the instance-type allow-list'
  has_code 'aws:RequestedRegion' \
    'the region bound is gone'
  # These two are inline &&-lists rather than helpers on purpose: set -e
  # spares a failed non-final member of an && list, so "grep finds nothing"
  # falls through cleanly here — wrap them in a helper and the helper would
  # return 1 instead, killing self_test as a failed simple command.
  grep -Eq '"(s3|iam|ssm|kms):[A-Za-z]+"' <<<"$code" \
    && die 'self-test: iam-compute-policy.tf grants outside EC2 — the compute credential is not compute-only any more'
  grep -Eq '"ec2:(Start|Stop)Instances"' <<<"$code" \
    && die 'self-test: iam-compute-policy.tf grants start/stop — an idle machine could accrue cost indefinitely'

  # The allow-list variable itself. The subjects' DEFAULT list is sliced out
  # of variables.tf rather than grepped whole, for two reasons: the variable's
  # own description argues about pull_request in prose (a whole-file search
  # could neither find absence nor tolerate presence), and widening the list
  # happens exactly where this check looks — the trust policy renders FROM
  # these values, so this slice is the guarded stage for "who may assume".
  local subj
  subj=$(sed -n '/^variable "compute_allowed_subject_suffixes"/,/^}/p' "$vars" \
    | sed -n '/default = \[/,/^\]/p')
  [[ -n $subj ]] \
    || die 'self-test: could not locate the compute_allowed_subject_suffixes default in variables.tf'
  grep -q '"ref:refs/heads/main"' <<<"$subj" \
    || die 'self-test: variables.tf no longer defaults the compute subjects to ref:refs/heads/main'
  if grep -Eq '"(pull_request|ref:refs/tags/\*)"' <<<"$subj"; then
    die 'self-test: compute subjects widened to pull requests or tag builds — unreviewed code must not launch machines'
  fi
  grep -q 'compute_instance_type_patterns' "$vars" \
    || die 'self-test: variables.tf no longer carries the instance-type allow-list'

  # Workflow plumbing: the composite action, the nightly job, the reporter
  # and the per-commit lane must all still reach each other.
  grep -q 'configure-aws-credentials' "$action" \
    || die 'self-test: the composite action no longer configures AWS credentials'
  grep -q 'pull_request' "$action" \
    || die 'self-test: the composite action no longer refuses pull_request events up front'
  grep -q '^  compute-identity-smoke:' "$nightly" \
    || die 'self-test: nightly.yml no longer declares the compute-identity-smoke job'
  grep -q '\./\.github/actions/aws-compute-role' "$nightly" \
    || die 'self-test: nightly.yml no longer routes the job through the composite action'
  grep -q 'id-token: write' "$nightly" \
    || die 'self-test: nightly.yml no longer grants id-token: write; no token can be minted'
  grep -q 'gate_compute_identity_smoke_run' "$ROOT/scripts/gate-status.sh" \
    || die 'self-test: gate-status.sh lost the compute-identity-smoke trio; the job would report UNKNOWN'
  grep -q 'run scripts/aws-compute-smoke.sh --self-test' "$ROOT/scripts/check.sh" \
    || die 'self-test: check.sh gates lane stopped running this self-test; a structural regression would go unnoticed'

  # Every probe below runs against real AWS, so an argument this CLI cannot
  # parse never reaches the policy at all: `aws` exits 2 with "Unknown options"
  # and the probe fails while proving *nothing*. That is exactly how #176's
  # first live run failed — `--min-count`/`--max-count` are AWS CLI **v1**
  # spellings and v2 wants `--count`. The structural clauses above all passed
  # while that was true, because they assert the probes exist rather than that
  # they are runnable.
  #
  # Search the executable body only: these spellings appear in this clause too,
  # and a whole-file search would match its own source and pass vacuously.
  probe_body="$(sed -n '/^echo "== positive probes =="/,$p' "$0" | grep -v '^[[:space:]]*#')"
  for v1_only in --min-count --max-count; do
    grep -Fq -- "$v1_only" <<<"$probe_body" \
      && die "self-test: probe uses the AWS CLI v1 option '$v1_only'; v2 rejects it at parse time, so the probe would fail without ever reaching the policy"
  done

  # The same failure one layer down: EC2 validates resource-id *syntax* before
  # it evaluates authorisation, so a dummy id in a known-malformed form answers
  # InvalidAMIID.Malformed / InvalidInstanceID.Malformed and masks the verdict
  # the probe asserts on. These two forms are confirmed malformed; the ids in
  # use are confirmed well-formed.
  for malformed in ami-00000000000000000 i-00000000000000000 ami-0123456789abcdef0; do
    grep -Fq -- "$malformed" <<<"$probe_body" \
      && die "self-test: probe uses '$malformed', which EC2 rejects as malformed before authorisation; the probe would report a syntax error instead of the policy verdict"
  done

  echo "$NAME: self-test passed"
}

# ─────────────────────────────────────────────────────────────────────────────

case "${1:-}" in
  --self-test) self_test; exit 0 ;;
  "") ;;
  *) die "usage: $NAME [--self-test]" ;;
esac

# ─────────────────────────────────────────────────────────────────────────────
# Live mode: prove the credential against the real service.
# ─────────────────────────────────────────────────────────────────────────────

mkdir -p "$OUT"
RESULT="$OUT/result.json"
: >"$RESULT"
rm -f "$OUT/PASSED"

principal='' account='' candidates=-1 images=-1
positives=0 denials=0 policy_assertions=0

positive() { # label
  echo "PASS  $1"
  positives=$((positives + 1))
}
denied() { # label detail
  echo "DENIED(as intended)  $1 ($2)"
  denials=$((denials + 1))
}

# A negative probe: run cmd; it must FAIL and its output must contain want.
expect_denied() { # label want cmd...
  local label=$1 want=$2
  shift 2
  local out rc=0
  out=$("$@" 2>&1) || rc=$?
  if ((rc == 0)); then
    die "negative probe '$label' SUCCEEDED; the policy is broader than this design claims. Output: $out"
  fi
  if ! grep -q "$want" <<<"$out"; then
    die "negative probe '$label' failed, but not with '$want'; got: $out"
  fi
  denied "$label" "$want"
}

# S1–S2 — #622/#679's policy-document assertions. Unlike the live probes
# below, these deliberately do not claim calls against real untagged or
# pre-existing instances were denied; EC2's synthetic-ID evaluation order no
# longer makes such claims testable without separate privileged machinery.
echo "== Terraform policy-shape assertions =="
assert_termination_policy_shape
policy_assertions=$((policy_assertions + 1))
assert_create_tags_policy_shape
policy_assertions=$((policy_assertions + 1))

echo "== positive probes =="

# P1 — who we are. The composite action already asserted this; re-derive it
# here so gate-status runs of the bare script carry their own evidence.
ident=$(aws sts get-caller-identity --output json)
principal=$(jq -r .Arn <<<"$ident")
account=$(jq -r .Account <<<"$ident")
case "$principal" in
  *"assumed-role/$ROLE_NAME/"*) ;;
  *) die "session principal is $principal, not assumed-role/$ROLE_NAME/* — assume the role first" ;;
esac
positive "session principal is assumed-role/$ROLE_NAME/* (account $account)"

# P2 — the discovery query #176's qualification procedure starts from.
candidates=$(aws ec2 describe-instance-types --region "$REGION" \
  --filters Name=instance-storage-supported,Values=true \
  --query 'length(InstanceTypes)')
((candidates >= 20)) || die "DescribeInstanceTypes returned $candidates candidates; expected the dozens #170 measured in $REGION"
positive "discovery query returned $candidates local-storage instance types in $REGION"

# P3 — base-image resolution.
images=$(aws ec2 describe-images --region "$REGION" --owners amazon \
  --filters Name=name,Values=al2023-ami-*-x86_64 \
  --query 'length(Images)')
((images >= 1)) || die "DescribeImages returned no Amazon Linux 2023 images in $REGION"
positive "image resolution returned $images candidate base images"

echo "== negative probes (each of these SHOULD be refused) =="

# N1 — no S3 at all: the cache belongs to the other role. ListBuckets has no
# target resource, so a missing bucket cannot answer before IAM does.
expect_denied "s3 list-buckets" "AccessDenied" \
  aws s3api list-buckets --region "$REGION"

# N2 — no IAM.
expect_denied "iam list-roles" "AccessDenied" \
  aws iam list-roles --region "$REGION" --max-items 1

# N3 — the type allow-list, and *only* the type allow-list.
#
# Three details, each learned by watching this probe fail for the wrong reason:
#
#   * `--count`, not `--min-count`/`--max-count`. Those were AWS CLI **v1**
#     spellings; v2 rejects them at argument-parse time with "Unknown options",
#     which never reaches AWS at all — so the probe failed while proving
#     nothing about the policy.
#   * A **real** image id. EC2 validates the AMI id's syntax before it
#     evaluates authorisation, so any dummy id answers
#     `InvalidAMIID.Malformed` and masks the verdict this probe asserts on.
#     `ami-00000000000000000` and `ami-0123456789abcdef0` are both rejected as
#     malformed.
#   * Canonical's image and the launch tag, both **correct**. The policy grants
#     RunInstances only for owner 099720109477 and only with
#     `aws:RequestTag/orrery-ci-ephemeral=true`. If either were wrong here the
#     refusal could come from that condition instead, and the probe would pass
#     while the type allow-list was wide open.
#
# So every condition is satisfied except the instance type. `UnauthorizedOperation`
# can then only mean the allow-list refused it, which is what N3 exists to prove.
# `--dry-run` keeps this side-effect-free by construction: EC2 evaluates
# authorisation without executing, so even a broken policy launches nothing.
probe_ami=$(aws ec2 describe-images --region "$REGION" --owners 099720109477 \
  --filters 'Name=name,Values=ubuntu/images/hvm-ssd*/ubuntu-*-24.04-amd64-server-*' \
            'Name=state,Values=available' \
  --query 'sort_by(Images,&CreationDate)[-1].ImageId' --output text)
[[ $probe_ami == ami-* ]] \
  || die "could not resolve a Canonical base image for the type-allow-list probe; got '$probe_ami'"

expect_denied "run-instances t3.micro, Canonical image, correct tag (dry-run)" "UnauthorizedOperation" \
  aws ec2 run-instances --region "$REGION" --dry-run \
    --image-id "$probe_ami" --instance-type t3.micro --count 1 \
    --tag-specifications 'ResourceType=instance,Tags=[{Key=orrery-ci-ephemeral,Value=true}]'

jq -n \
  --arg principal "$principal" \
  --arg account "$account" \
  --arg region "$REGION" \
  --argjson candidates "$candidates" \
  --argjson images "$images" \
  --argjson positives "$positives" \
  --argjson denials "$denials" \
  --argjson policy_assertions "$policy_assertions" \
  '{principal: $principal, account: $account, region: $region,
    candidates_found: $candidates, images_found: $images,
    positives_passed: $positives, denials_proved: $denials,
    policy_assertions_passed: $policy_assertions,
    passed: true}' >"$RESULT"

echo "$NAME: $positives positive probes passed, $denials live least-privilege denials proved, $policy_assertions Terraform policy-shape assertion passed; report at $RESULT"
touch "$OUT/PASSED"
