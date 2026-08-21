# `infra/` — the AWS identity plane and the kache cache bucket

Terraform for [#173](https://github.com/baadc0de/orrery/issues/173) (a GitHub
OIDC provider and one federated IAM role) and
[#174](https://github.com/baadc0de/orrery/issues/174) (the S3 bucket kache uses
as its remote, with the lifecycle rule that replaces
`kache-prune-shared.timer`).

**Applied 2026-08-21.** All ten resources exist in account `590561279276`,
verified with `aws` directly rather than from Terraform's own output, since
state can drift from reality:

| | |
|---|---|
| bucket | `arn:aws:s3:::orrery-kache` (`eu-central-1`) |
| role | `arn:aws:iam::590561279276:role/orrery-ci-cache` |
| OIDC provider | `token.actions.githubusercontent.com` — the account's first |
| lifecycle | `expire-cache-objects`, 14 d on `artifacts/`, plus multipart abort |

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

Ten resources, all named `orrery-*` or `orrery-kache`, so they are separable by
name from the 25 roles, 1 IAM user, 9 customer policies and 1 instance profile
this account already carries for unrelated workloads. Nothing pre-existing is
read, referenced or modified.

| Resource | Name | Why |
|---|---|---|
| `aws_iam_openid_connect_provider` | `token.actions.githubusercontent.com` | The account's first. Lets GitHub-signed tokens be presented to STS. |
| `aws_iam_role` | `orrery-ci-cache` | Assumed from Actions. The trust policy is the reviewable artefact. |
| `aws_iam_policy` + attachment | `orrery-ci-cache-access` | Four object actions on one prefix. No delete, no bucket admin. |
| `aws_s3_bucket` | `orrery-kache` | The kache remote, `eu-central-1`. |
| `aws_s3_bucket_public_access_block` | — | All four switches on. |
| `aws_s3_bucket_ownership_controls` | — | `BucketOwnerEnforced`; ACLs off. |
| `aws_s3_bucket_server_side_encryption_configuration` | — | SSE-S3 (AES256). |
| `aws_s3_bucket_lifecycle_configuration` | — | Expiry + multipart abort. |
| `aws_s3_bucket_policy` | — | Deny non-TLS. |

Not created: **no access keys, no IAM users, no KMS keys, no EC2, no VPC
changes, no account-level settings.** #173's whole point is that there is no
long-lived credential anywhere in this design, and there is not one in this
directory.

Also not created: **#176's compute role.** Its least-privilege shape depends on
which instance family passes D19's fio qualification and on whether access is by
SSM or a key pair — none of which is decided. Writing an `ec2:RunInstances`
grant now would mean scoping it by guesswork. The OIDC provider it needs is
here, and its ARN is an output.

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

Then hand the outputs to #175 rather than transcribing them — a mistyped role
ARN fails as an opaque `AccessDenied` several minutes into a job:

```console
$ terraform -chdir=infra output -raw cache_role_arn
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
role in the account at once. Today only `orrery-ci-cache` uses it; once #176
adds a compute role, or if any unrelated workload adopts it, `terraform destroy`
here becomes a wider action than it looks. Check with
`aws iam list-open-id-connect-providers` and audit who trusts it before
destroying.

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
| `oidc.tf` | **#173.** The provider, and the trust policy with its full argument. |
| `iam-cache-policy.tf` | **#173.** The permissions policy, derived from kache's request set. |
| `s3.tf` | **#174.** The bucket, its defaults, and the lifecycle rule. |
| `outputs.tf` | What #175 consumes. |
| `terraform.tfvars.example` | Overrides, none of which an ordinary apply needs. |
