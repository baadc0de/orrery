# `infra/` — the AWS identity plane and the kache cache bucket

Terraform for [#173](https://github.com/baadc0de/orrery/issues/173) (a GitHub
OIDC provider and **two** federated IAM roles), and
[#174](https://github.com/baadc0de/orrery/issues/174) (the S3 bucket kache uses
as its remote, with the lifecycle rule that replaces
`kache-prune-shared.timer`).

**Applied 2026-08-21.** All ten resources of the identity plane + cache bucket
exist in account `590561279276`, verified with `aws` directly rather than from
Terraform's own output, since state can drift from reality:

| | |
|---|---|
| bucket | `arn:aws:s3:::orrery-kache` (`eu-central-1`) |
| role | `arn:aws:iam::590561279276:role/orrery-ci-cache` |
| OIDC provider | `token.actions.githubusercontent.com` — the account's first |
| lifecycle | `expire-cache-objects`, 14 d on `artifacts/`, plus multipart abort |

The **compute role** (`orrery-ci-compute`, also #173) is in the tree since
2026-08-23 but **not yet applied** — see § The compute role below for its
three resources and what applying them costs (nothing per month; instances
bill only while a gate keeps one alive).

It was applied as an IAM principal, not as the account root user, and the plan
was read before the apply — `10 to add, 0 to change, 0 to destroy`, matching
what this file predicted.

**The state file is the record.** `infra/terraform.tfstate` is `.gitignore`d and
there is no remote backend, because a remote backend would be chicken-and-egg:
the only bucket this account has is the one this configuration creates. Keep it
somewhere that survives the machine it was applied from.

**What it is being used for, as of today:** the `clippy` CI lane pulls from this
bucket ([#194](https://github.com/baadc0de/orrery/issues/175)); the `test` lane
deliberately does not, because it was measured at \$16.70 per hour of wall-clock
saved against `clippy`'s \$1.15. See § What it costs below, and note the live
figure that matters is **egress, not storage**.

---

## What it creates — the whole list

Ten applied resources and three pending ones, all named `orrery-*` or
`orrery-kache`, so they are separable by name from the 25 roles, 1 IAM user,
9 customer policies and 1 instance profile this account already carries for
unrelated workloads. Nothing pre-existing is read, referenced or modified.

| Resource | Name | Status | Why |
|---|---|---|---|
| `aws_iam_openid_connect_provider` | `token.actions.githubusercontent.com` | applied | The account's first. Lets GitHub-signed tokens be presented to STS. |
| `aws_iam_role` | `orrery-ci-cache` | applied | Assumed from Actions. The trust policy is the reviewable artefact. |
| `aws_iam_policy` + attachment | `orrery-ci-cache-access` | applied | Four object actions on one prefix. No delete, no bucket admin. |
| `aws_iam_role` | `orrery-ci-compute` | **pending apply** | #176's ephemeral machines. Main-only trust; tag-chained lifecycle. |
| `aws_iam_policy` + attachment | `orrery-ci-compute-access` | **pending apply** | EC2 discovery, tagged launch/console/termination. One region. |
| `aws_s3_bucket` | `orrery-kache` | applied | The kache remote, `eu-central-1`. |
| `aws_s3_bucket_public_access_block` | — | applied | All four switches on. |
| `aws_s3_bucket_ownership_controls` | — | applied | `BucketOwnerEnforced`; ACLs off. |
| `aws_s3_bucket_server_side_encryption_configuration` | — | applied | SSE-S3 (AES256). |
| `aws_s3_bucket_lifecycle_configuration` | — | applied | Expiry + multipart abort. |
| `aws_s3_bucket_policy` | — | applied | Deny non-TLS. |

Not created: **no access keys, no IAM users, no KMS keys, no VPC changes, no
account-level settings.** #173's whole point is that there is no long-lived
credential anywhere in this design, and there is not one in this directory.
(EC2 *resources* are likewise not created here: instances exist only while a
gate launches one, per the standing no-long-running-infrastructure decision,
and launching is done by workflows assuming the compute role — never by this
module.)

---

## What it grants, in one paragraph

A GitHub Actions job running in `baadc0de/orrery`, on a branch, a tag, or a
same-repository pull request, can obtain a one-hour credential that reads and
writes objects under `s3://orrery-kache/artifacts/*`. It cannot delete them,
cannot change the bucket's configuration or its retention rule, cannot see any
other bucket, and has no permission of any kind outside S3 — in particular none
against anything that was in this account before.

### The trust condition

```json
"Condition": {
  "StringEquals": {
    "token.actions.githubusercontent.com:aud": "sts.amazonaws.com"
  },
  "StringLike": {
    "token.actions.githubusercontent.com:sub": [
      "repo:baadc0de@15308543/orrery@1331921648:ref:refs/heads/*",
      "repo:baadc0de@15308543/orrery@1331921648:pull_request",
      "repo:baadc0de@15308543/orrery@1331921648:ref:refs/tags/*"
    ]
  }
}
```

**Read the `@15308543` and `@1331921648`.** They are not decoration and they are
not something we added — they are GitHub's immutable subject format, the default
for repositories created after 2026-07-15, and `baadc0de/orrery` was created
2026-08-12. The value was read from the repository itself, not inferred:

```console
$ gh api repos/baadc0de/orrery/actions/oidc/customization/sub
{"use_default":true,"use_immutable_subject":false,
 "sub_claim_prefix":"repo:baadc0de@15308543/orrery@1331921648"}
```

The consequence is worth stating because it inverts the usual review instinct:
**the familiar `repo:baadc0de/orrery:*` would be not merely too wide but
completely inert here** — no token this repository can mint contains that
string, so a role trusting it would be unassumable and the first demonstration
job would fail with `Not authorized to perform sts:AssumeRoleWithWebIdentity`
for a reason nothing in the error message hints at.

It also happens to be the strongest form available. AWS does **not** expose
GitHub's `repository_id` or `repository_owner_id` claims as IAM condition keys —
the IAM condition-key reference lists only `amr`, `aud`, `email`, `oaud` and
`sub` for OIDC federation, so the `token.actions.githubusercontent.com:repository_owner_id`
condition that several blog posts recommend matches nothing and would deny every
assumption. Here the ids ride inside `sub`, where they do work, and they close
the name-reuse hole: a name-based subject would be re-mintable by anyone who
could one day claim the login `baadc0de` and create a repository called
`orrery`.

Full reasoning for each permitted suffix, and for what is deliberately not
restricted, is in the comment block in [`oidc.tf`](oidc.tf).

### Blast radius

The population that can obtain this credential is the population with write
access to `baadc0de/orrery`. The repository is public, so the fork case is the
one that matters, and it has two independent answers:

1. GitHub does not grant `id-token: write` to a workflow triggered by a pull
   request from a fork. The token is never minted; there is nothing to present.
2. The repository requires approval for all outside collaborators before any
   fork workflow starts (`.github/workflows/ci.yml:78-85`).

Neither is the runner-label guard, and — following the argument ci.yml:78-85
already makes about itself — none of the three should be mistaken for the only
one.

The residual exposure a reviewer should weigh: a compromised maintainer account,
or a malicious commit merged to a branch, yields the ability to fill and read a
build cache. The objects are content-addressed compiler outputs of a public
repository. Poisoning one requires a hash collision; reading them reveals
compiled artefacts of code that is already public. That is the entire grant.

---

## The compute role — `orrery-ci-compute` (#173, for #176)

The second of the two roles #173 names: what #176's ephemeral qualified
machines assume they may do. It exists to launch one EC2 instance in
`eu-central-1`, let a gate run on it, and terminate that instance — nothing
more. Instances are per-run infrastructure by standing decision; nothing here
keeps one alive between runs.

### The trust policy

Rendered exactly as AWS receives it (`terraform -chdir=infra output -raw` has
no equivalent for trust documents; this is `data.aws_iam_policy_document.github_compute_trust.json`):

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Sid": "GitHubActionsWebIdentityCompute",
    "Effect": "Allow",
    "Action": "sts:AssumeRoleWithWebIdentity",
    "Principal": {
      "Federated": "arn:aws:iam::590561279276:oidc-provider/token.actions.githubusercontent.com"
    },
    "Condition": {
      "StringEquals": {
        "token.actions.githubusercontent.com:aud": "sts.amazonaws.com",
        "token.actions.githubusercontent.com:sub":
          "repo:baadc0de@15308543/orrery@1331921648:ref:refs/heads/main"
      }
    }
  }]
}
```

**Read why this is tighter than the cache role's.** The cache serves branches,
tags and same-repo PRs; machines cost money and can be left running if a job
dies, so the compute credential is granted to exactly one subject:

- **Permitted:** `…:ref:refs/heads/main`. Pushes to main, `workflow_dispatch`
  on main, and the nightly `schedule` (which GitHub always runs on the default
  branch) all carry precisely this string. The population that can mint it is
  the population whose code has passed review and merged.
- **Fork pull requests:** excluded three independent ways. Their subjects name
  another repository; GitHub never mints an OIDC token for a fork PR at all;
  and `.github/actions/aws-compute-role` refuses any `pull_request` event up
  front with a message naming the reason instead of an opaque STS denial.
- **Same-repo pull requests:** excluded on purpose. A PR is where unreviewed
  code lands before review, and unreviewed code must not launch billable
  machines. Testing the ephemeral flow before merge means merging first, or
  widening `compute_allowed_subject_suffixes` in variables.tf as a conscious,
  reviewable act.
- **Tags and `environment:*`:** absent with the same reasoning oidc.tf records
  for the cache role — Environments *replace* the ref in the subject, so the
  day a job gains one it fails here until added deliberately.

A reviewer can check the subject against reality without AWS:
`terraform -chdir=infra output -raw compute_allowed_oidc_subjects`, or read it
off a real token from any run's log line `assumed principal:`.

### The permission policy

Seven allow statements plus one explicit deny; every action is listed because the
lifecycle needs it, and the absences are argued in iam-compute-policy.tf's
header:

| Statement | Grants | Bounds |
|---|---|---|
| `NoMetalSizes` | — (**Deny**) | `ec2:RunInstances` refused for `*.metal`, cutting the most expensive shapes off every family glob below. |
| `DiscoveryReadOnly` | eight `ec2:Describe*` actions | Instance types (the #170 capability query), images, AZs, VPC/subnet/SG lookups, our instances' status. Resource `*` — Describe actions accept no narrower scope — but pinned to `eu-central-1` via `aws:RequestedRegion`. |
| `UseCanonicalUbuntuImage` | `ec2:RunInstances` | Public image/snapshot resources only, pinned by `ec2:Owner` to Canonical (`099720109477`). This closes the licensed-AMI exposure #173 recorded. |
| `UseVpcLaunchInputs` | `ec2:RunInstances` | Existing subnet/security-group inputs in this account and region; separated because launch inputs do not carry creation request tags. |
| `LaunchTaggedInstance` | `ec2:RunInstances` | Only instance types matching `compute_instance_type_patterns` (#170's local-NVMe candidate set); only created resources carrying `aws:RequestTag/orrery-ci-ephemeral=true`; only `eu-central-1`. |
| `ReadTaggedConsoleEvidence` | `ec2:GetConsoleOutput` | Only an instance carrying the ownership tag. This is the result channel: the instance has no AWS credential, key pair, SSM profile or bucket grant. |
| `TagOnlyAtLaunch` | `ec2:CreateTags` | Only while `ec2:CreateAction = RunInstances`. Tags can never be written onto an existing object afterwards. |
| `TerminateTaggedOnly` | `ec2:TerminateInstances` | Only objects carrying `aws:ResourceTag/orrery-ci-ephemeral=true`. |

**Not granted**, each deliberate: `StartInstances`/`StopInstances` (an idle
machine could accrue cost forever while looking ephemeral), `CreateKeyPair`
and `ssm:*` (session access — SSM vs SSH — is #170's open question and drags
an instance profile with it; granting half now would be scoping by guesswork),
every delete action (root volumes ride delete-on-termination and RunInstances'
NICs are reaped with the instance), S3/IAM/KMS/STS (nothing outside EC2).

#176 resolved the access question without either option: EC2 user data starts
the public-repository workload and one compact result returns through the
tag-guarded serial console. `infra/p2-ephemeral.py` records every launched id,
terminates in each worker's `finally`, traps cancellation, and the workflow has
an independent `if: always()` teardown step over the same id file.

**The tag chain is the security argument.** Launch requires the tag;
retagging outside a launch is impossible; termination requires the tag.
Together: *everything CI can create, CI can delete; everything CI can delete,
CI created.* A leaked compute credential's blast radius is the instances of
one nightly run, and nothing pre-existing in the account is reachable even
for reads beyond Describe*.

### The instance-type allow-list will rot, in one direction

The list in variables.tf is #170's measured candidate set — every family in
`eu-central-1` reporting `NvmeSupport=required` — because D19 qualifies devices
by measurement and #176 measures the whole set rather than reasoning families
out first. AWS will add families; until someone re-runs #170's discovery query
and adds them consciously, new families fail closed. That is the correct
default for a credential that creates billable machines. Metal sizes never
match anyway: the explicit deny removes them from every glob at once,
including shapes like `u-*.metal` and `mac*.metal` no family pattern covers.

### Proving it works, nightly

nightly.yml's `compute-identity-smoke` job assumes the role through
[`.github/actions/aws-compute-role`](../.github/actions/aws-compute-role/action.yml)
and runs [`scripts/aws-compute-smoke.sh`](../scripts/aws-compute-smoke.sh):
three positive probes (identity, discovery, image resolution), four live
refusals, and one Terraform policy-shape assertion. The launch probe uses
`--dry-run`, so not even a broken policy can start anything during the proof.
The termination assertion confirms the source policy's sole
`TerminateTaggedOnly` grant requires
`aws:ResourceTag/orrery-ci-ephemeral=true`; it does **not** claim EC2 denied a
real untagged instance, or that the checked-out source is the deployed policy.
That distinction is deliberate: #622 found that EC2 now resolves a synthetic
instance's existence before evaluating its tag condition, making the former
live negative probe incapable of proving a denial. The script's `--self-test`
runs per-commit in `check.sh`'s gates lane and also executes that named
policy-shape assertion.

---

## Retention: why age works here when it did not last time

`AGENTS.md` § Build cache records that the first pruning policy for the
filesystem remote keyed on **age since last read**, and that it **could not
fire**: every build re-reads the whole hot set, so nothing was ever untouched
for 24 hours. The remote reached **319 GiB in a single day** and the disk hit
94%. It did not prune too little — it pruned nothing, while looking healthy.

S3's `Expiration.Days` keys on **age since object creation**, which is a
different quantity, not a tighter one:

- Creation time is written once at PUT and is **monotone**. Reads never touch
  it.
- So an object created N days ago is deleted at N days regardless of traffic,
  and "the rule matches nothing" is not a reachable state. The exact failure
  mode of the 319 GiB incident is structurally unavailable.
- Steady state is closed-form and bounded for any finite fill rate:
  `steady_state = unique_bytes_uploaded_per_day × N`.

**The cost of that, stated rather than hidden:** creation-age expiry evicts hot
objects too, turning unbounded growth into re-uploads. Three things make it
acceptable, all on the record: a pruned object is a cache miss and nothing worse
(content-addressed and immutable — `AGENTS.md` § Build cache); expiry is a
rolling front rather than a cliff, because objects are created across the whole
window and die across it too; and a re-upload resets the clock, so a genuinely
hot object costs one miss per N days.

**N = 14 days, and it is a starting value, not a measured one.** The fill rate
cannot be measured, because the remote does not exist yet — `kache doctor` on
the shared box still ends "no remote cache or planner configured". Substituting
a confident number for an absent measurement is precisely what produced the
24-hour policy, so instead, the bracket:

| assumed new unique content/day | steady state at N=14 | S3 Standard storage, eu-central-1 |
|---|---|---|
| 3 GiB | ~42 GiB | ~$1.03/month |
| 10 GiB | ~140 GiB | ~$3.43/month |
| 319 GiB (the 2026 incident rate) | ~4.4 TiB | ~$109/month |

The last row is the one that matters: it is the reason `cache_expiration_days`
is a variable and the reason **#175 must re-derive N from the fill rate it
measures and re-apply.** S3 lifecycle has no size trigger, and #174 forbids a
bespoke pruning timer to supply one — that is the whole point of moving
retention into the storage layer.

A second rule aborts incomplete multipart uploads after 1 day, bucket-wide.
Uploaded parts of an unfinished multipart upload are billed at full storage
rates and are **invisible to `list-objects`**; only `list-multipart-uploads`
shows them, and nobody runs it. A cancelled workflow mid-PUT is all it takes,
and ci.yml sets `cancel-in-progress` on every non-`main` ref.

Versioning is **not** enabled, deliberately — see the comment block in
[`s3.tf`](s3.tf). Briefly: the content is content-addressed, so a "previous
version" is either identical or a hash collision; and with versioning on,
`Expiration.Days` frees no space at all, because it writes a delete marker and
leaves the object as a noncurrent version. That is the single most likely way to
reproduce the unbounded-growth failure on S3.

---

## What it costs

List prices, `eu-central-1`, checked against AWS's published rates when this was
written. Re-check before quoting them at anyone.

| Line | Rate | Realistic monthly |
|---|---|---|
| S3 Standard storage | $0.0245 / GB-month | **$1–4** at the plausible fill rates above; ~$109 at the 2026 incident rate |
| PUT / POST / LIST | $0.0054 / 1,000 | a cold lane writing ~50k objects is ~$0.27; **cents** |
| GET | $0.00043 / 1,000 | **cents** |
| **Data transfer out to internet** | first 100 GB/month free, then **$0.09 / GB** | **this is the real bill** |
| IAM roles, policies, OIDC provider | free | $0 |
| Lifecycle rule execution | free | $0 |

**The finding a reviewer should take from that table is that storage is not the
cost — egress is.** GitHub-hosted runners live outside AWS, so every byte a lane
pulls from the cache is billed at $0.09/GB once the account's 100 GB/month free
allowance is used. A hosted lane pulling 5 GiB of cache costs about $0.46 in
egress; 100 such runs in a month is roughly **$45**, an order of magnitude above
the storage line, and it scales with cache *hits* — i.e. with the mechanism
working. #175's published comparison should therefore measure **GiB pulled per
run**, not only wall-clock, or the cache will be evaluated on the cheap axis.

(#176's ephemeral machines are in `eu-central-1` with the bucket, so their cache
traffic is intra-region and free. That co-location is why the region variable is
what it is.)

Total if nothing is ever used: **effectively $0** — an empty bucket, an OIDC
provider and IAM objects are all free.

---

## How to apply it

Prerequisites, and the first one is not optional:

1. **Stop using the root access key.** #170's smoke test ran as the account root
   user with a static key and no MFA (`AccountMFAEnabled: 0`,
   `AccountAccessKeysPresent: 1`). That was acceptable for a read-only smoke
   test and is not acceptable for creating a federated identity plane. Put MFA
   on root, create an IAM principal for this work, apply as that principal, and
   retire the root key. **The apply that creates the account's OIDC provider
   should not itself be the last thing root's key ever does.**
2. Terraform ≥ 1.6. Verified with 1.15.9 and `hashicorp/aws` 6.61.0.
3. `AWS_REGION=eu-central-1` and credentials for account `590561279276`. A
   postcondition in `providers.tf` fails the plan if the account id is anything
   else.

```console
$ terraform -chdir=infra init
$ terraform -chdir=infra fmt -check -recursive     # passes
$ terraform -chdir=infra validate                  # passes
$ terraform -chdir=infra plan                      # 10 to add, 0 to change, 0 to destroy
$ terraform -chdir=infra apply
```

Read the plan. The three things worth reading closely are the `sub` list in
`data.aws_iam_policy_document.github_cache_trust`, the action list in
`data.aws_iam_policy_document.cache_access`, and the two lifecycle rules.

### Applying the compute role (pending as of 2026-08-23)

The cache half above is applied; `iam-compute-policy.tf` is not. From this
directory, on an already-initialized checkout:

```console
$ terraform -chdir=infra fmt -check -recursive     # passes
$ terraform -chdir=infra validate                  # passes
$ scripts/aws-compute-smoke.sh --self-test         # structural clauses hold (no AWS needed)
$ python3 infra/p2-ephemeral.py self-test           # #176 stages hold (no AWS needed)
$ terraform -chdir=infra plan                      # 3 to add, 0 to change, 0 to destroy
$ terraform -chdir=infra apply
```

The plan's three adds are exactly: `aws_iam_role.github_compute`,
`aws_iam_policy.compute_access`, and its attachment. Read two things closely:
the rendered subject list in
`data.aws_iam_policy_document.github_compute_trust` — it must end in
`:ref:refs/heads/main`, character for character — and the action lists in
`data.aws_iam_policy_document.compute_access`, including Canonical's owner pin,
the ownership tag on launch/console/termination, and the explicit metal deny.
Nothing else in the plan may change; if a fourth add appears, stop and read why.

Then prove the credential path from Actions:

```console
$ gh workflow run nightly.yml        # dispatch runs on main; the trust policy permits exactly that
$ gh run watch                       # or watch https://github.com/baadc0de/orrery/actions
```

and read the `compute-identity-smoke` job: it prints the assumed principal,
exercises the discovery query #176 will start from, and requires five IAM
refusals (S3, IAM, off-list launch under `--dry-run`, untagged termination,
retag-outside-launch). A green run there is #173's acceptance evidence.

Then hand the outputs onward rather than transcribing them — a mistyped role
ARN fails as an opaque `AccessDenied` several minutes into a job:

```console
$ terraform -chdir=infra output -raw cache_role_arn
$ terraform -chdir=infra output -raw compute_role_arn
$ terraform -chdir=infra output -json kache_env
```

`kache_env` contains no credentials and needs none. kache honours
`KACHE_S3_ACCESS_KEY`/`KACHE_S3_SECRET_KEY` only when both are set, and
otherwise falls through to reqsign's default AWS chain, which reaches
`AssumeRoleWithWebIdentity` — so a role assumed by
`aws-actions/configure-aws-credentials` is picked up with no kache-specific
credential configuration at all.

### State

Local `terraform.tfstate`, and `.gitignore`d. A remote S3 backend would be
chicken-and-egg: the only bucket this account will have is the one this
configuration creates. **The state file is the record of what was created — keep
it.** If a shared backend is wanted later, create a separate small
`orrery-tfstate` bucket by hand (or in a second root module) and
`terraform init -migrate-state`; do not put the state in `orrery-kache`, where
the lifecycle rule would eventually expire it.

---

## Troubleshooting the first run

| Symptom | Cause |
|---|---|
| `Not authorized to perform sts:AssumeRoleWithWebIdentity` | The job's `sub` does not match. Compare it against `terraform output allowed_oidc_subjects`, character by character. The two likely causes are below. |
| …and the job uses a GitHub Environment | Environments **replace** the ref in the subject. `environment:*` is deliberately not permitted; add it to `allowed_subject_suffixes` on purpose, ideally with required reviewers. |
| …and the repository was renamed or transferred | Re-read `gh api repos/<owner>/<repo>/actions/oidc/customization/sub` and update `github_subject_prefix`. |
| The job never gets a token at all | `permissions: id-token: write` is missing on that job. The workflow default is `contents: read` (`ci.yml:50-51`) and must stay that way — widen it per-job. |
| `AccessDenied` on a List, while Get and Put work | The `s3:prefix` condition in `iam-cache-policy.tf` is too tight for the caller's list request. kache's build path does not list, so this degrades `sync`/`doctor` rather than the cache; widen the condition values or drop the condition. |
| `BucketAlreadyExists` | Bucket names are global. Set `cache_bucket_name`. (`orrery-kache` was free on 2026-08-21.) |
| A transition rule to IA/Glacier tiers almost nothing | The account reports `TransitionDefaultMinimumObjectSize: all_storage_classes_128K` — transitions skip objects under 128 KB, which is most of a build cache. Override it or do not add transitions. There are none today. |
| `compute-identity-smoke` fails with `Not authorized to perform sts:AssumeRoleWithWebIdentity` | Almost always the ref: this role accepts **only** `ref:refs/heads/main`, so a dispatch on a branch is refused by design. Compare the token's `sub` against `terraform output -raw compute_allowed_oidc_subjects` character by character. A repository rename changes the prefix — see the row above. |
| …a live negative probe fails with anything but `UnauthorizedOperation`/`AccessDenied` | The live policy check has widened or the probe no longer reaches IAM. Treat it as a security regression, not flakiness; `scripts/aws-compute-smoke.sh --self-test` names the guarded clause. The termination guard is different after #622: it is a Terraform policy-shape assertion because a synthetic instance ID now yields `InvalidInstanceID.NotFound` before EC2 evaluates tags. |
| The composite action refuses with "pull_request events cannot assume…" | Working as designed — same-repo PRs cannot launch machines (fork PRs never even get an OIDC token). Merge, or widen `compute_allowed_subject_suffixes` deliberately. |

## Watching the size

The number that decides `cache_expiration_days` is free to read: CloudWatch
publishes `AWS/S3` `BucketSizeBytes` daily at no charge.

```console
$ aws cloudwatch get-metric-statistics \
    --namespace AWS/S3 --metric-name BucketSizeBytes \
    --dimensions Name=BucketName,Value=orrery-kache \
                 Name=StorageType,Value=StandardStorage \
    --start-time "$(date -u -d '30 days ago' +%FT%TZ)" \
    --end-time "$(date -u +%FT%TZ)" \
    --period 86400 --statistics Average
```

If it is still climbing after `cache_expiration_days` days have elapsed since the
first upload, the fill rate is higher than assumed and N should come down. If it
plateaus well below budget, N can go up and buy a higher hit rate.

---

## How to remove it

```console
# The bucket will not delete while it holds objects, and there is no
# force_destroy. Empty it first, and note this also clears any orphaned
# multipart parts, which `rm --recursive` alone does not.
$ aws s3 rm s3://orrery-kache --recursive
$ aws s3api list-multipart-uploads --bucket orrery-kache   # expect no Uploads key

$ terraform -chdir=infra destroy
```

Two warnings.

**The OIDC provider is account-global.** Destroying it breaks every federated
role in the account at once. Both `orrery-ci-cache` and `orrery-ci-compute`
trust it, and any unrelated workload that adopts it would too, so
`terraform destroy` here becomes a wider action than it looks. Check with
`aws iam list-open-id-connect-providers` and audit who trusts it before
destroying. (Destroying the compute role itself is safe the way deleting a
key is: nightly's `compute-identity-smoke` goes red on the next run, loudly,
at the point of use.)

**Deleting the cache is safe; deleting the identity plane is not, silently.**
Cache objects are content-addressed, so an emptied bucket costs one cold build.
A removed role, by contrast, fails every workflow that assumes it, at the point
of use.

---

## Files

| File | Contents |
|---|---|
| `versions.tf` | Terraform and provider pinning; why Terraform. |
| `providers.tf` | Region, default tags, and the wrong-account guard. |
| `variables.tf` | Every input, each with its justification. |
| `oidc.tf` | **#173.** The provider, and the cache trust policy with its full argument. |
| `iam-cache-policy.tf` | **#173.** The cache permissions policy, derived from kache's request set. |
| `iam-compute-policy.tf` | **#173.** The compute role: main-only trust, the tag-chained EC2 lifecycle, and the argued absences. |
| `s3.tf` | **#174.** The bucket, its defaults, and the lifecycle rule. |
| `outputs.tf` | What #175 consumes. |
| `terraform.tfvars.example` | Overrides, none of which an ordinary apply needs. |
