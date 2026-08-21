provider "aws" {
  region = var.region

  # This account is NOT greenfield. #170's account smoke test (2026-08-21)
  # found 1 pre-existing IAM user, 25 roles, 9 customer-managed policies and
  # 1 instance profile belonging to unrelated workloads. Nothing pre-existing
  # is ours to touch.
  #
  # Two consequences are enforced mechanically rather than by care:
  #
  #   1. Every resource here is named with an `orrery-` prefix, so an operator
  #      auditing the account can separate ours from theirs by name alone.
  #   2. Every resource carries these tags, so they can also be separated by
  #      tag — which is what a cost report groups on, and a name prefix is not.
  default_tags {
    tags = {
      Project   = "orrery"
      ManagedBy = "terraform"
      Source    = "github.com/baadc0de/orrery/tree/main/infra"
    }
  }
}

# Guard rail. If someone points this configuration at the wrong account —
# their personal account, a client's — `terraform plan` fails here rather than
# proposing to create a GitHub-federated role in it. Costs one read-only
# sts:GetCallerIdentity per plan.
data "aws_caller_identity" "current" {
  lifecycle {
    postcondition {
      condition     = self.account_id == var.expected_account_id
      error_message = "Refusing to operate on account ${self.account_id}; expected ${var.expected_account_id}. Set -var=expected_account_id=... only if you genuinely mean to target a different account."
    }
  }
}

data "aws_partition" "current" {}
