# Terraform and provider pinning for Orrery's build infrastructure.
#
# Why Terraform rather than CloudFormation, CDK or a runbook of `aws` calls:
# the deliverables of #173 and #174 are a *trust policy* and a *lifecycle rule*,
# and both are documents a reviewer has to be able to read, diff and re-read a
# year later. Terraform keeps them as text in the repository, which is what
# #173 asks for ("the provider, the trust conditions and the policy documents
# must be written down in the repository, not left only in a console"). It also
# gives a genuine dry run: `terraform plan` shows the exact API calls before
# anyone consents to them, which a runbook of `aws` invocations does not.
#
# State: local by default, deliberately. A remote S3 backend would be
# chicken-and-egg — the only bucket this account will have is the one this
# configuration creates. See infra/README.md § State.

terraform {
  required_version = ">= 1.6.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }
}
