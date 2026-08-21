# Everything #175 needs to wire the workflows, so nothing is hand-copied.
#
#   terraform -chdir=infra output -raw cache_role_arn
#   terraform -chdir=infra output -json kache_env
#
# The point of these being outputs rather than facts in a README is that a
# transcription error in a role ARN fails as an opaque AccessDenied several
# minutes into a job.

output "cache_role_arn" {
  description = "Pass to aws-actions/configure-aws-credentials as `role-to-assume`."
  value       = aws_iam_role.github_cache.arn
}

output "cache_role_name" {
  description = "IAM role name, for console lookups and CloudTrail filters."
  value       = aws_iam_role.github_cache.name
}

output "github_oidc_provider_arn" {
  description = "The account's GitHub OIDC provider. #176's compute role must reference this one rather than create a second."
  value       = aws_iam_openid_connect_provider.github.arn
}

output "cache_bucket_name" {
  description = "kache's `[cache.remote] bucket` / KACHE_S3_BUCKET."
  value       = aws_s3_bucket.kache.id
}

output "cache_bucket_arn" {
  description = "Bucket ARN, for anyone writing a further policy against it."
  value       = aws_s3_bucket.kache.arn
}

output "cache_region" {
  description = "kache's `[cache.remote] region` / KACHE_S3_REGION, and the region configure-aws-credentials should use."
  value       = var.region
}

output "cache_prefix" {
  description = "kache's `[cache.remote] prefix` / KACHE_S3_PREFIX."
  value       = var.cache_prefix
}

output "allowed_oidc_subjects" {
  description = <<-EOT
    The exact `sub` values the trust policy accepts, rendered. Paste these into
    #173 and diff them against a real token's `sub`: the fastest way to diagnose
    "Not authorized to perform sts:AssumeRoleWithWebIdentity" is to compare the
    two strings character by character.
  EOT
  value       = local.allowed_subjects
}

output "kache_env" {
  description = <<-EOT
    The complete kache remote configuration as environment variables, which is
    the form #175 wants: hosted runners are ephemeral, so an env block beats
    writing ~/.config/kache/config.toml on every job.

    Deliberately contains NO credentials, and none are needed. kache honours
    KACHE_S3_ACCESS_KEY/KACHE_S3_SECRET_KEY only when both are set and otherwise
    falls through to reqsign's default AWS chain, which reaches
    AssumeRoleWithWebIdentity — so a role assumed by
    aws-actions/configure-aws-credentials is picked up with no kache-specific
    credential configuration at all (docs/spikes/kache-remote-backend.md §2, on
    branch `docs/kache-backend-record`).

    There is no KACHE_S3_ENDPOINT here and there must not be: its presence is
    what selects the S3-emulation path that choosing native AWS S3 exists to
    avoid.

    Note also KACHE_S3_BUCKET set-but-empty DISABLES the remote rather than
    falling back to a config file — so a workflow that computes this value must
    fail loudly if it computes an empty string.
  EOT
  value = {
    KACHE_S3_BUCKET = aws_s3_bucket.kache.id
    KACHE_S3_REGION = var.region
    KACHE_S3_PREFIX = var.cache_prefix
  }
}
