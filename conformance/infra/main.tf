# SPDX-License-Identifier: Apache-2.0
# Conformance stack for microvms-agentd.
#
# The minimum AWS surface needed to build a MicroVM image and run one:
#   - an S3 bucket for the code artifact,
#   - the build role Lambda assumes during CreateMicrovmImage,
#   - an execution role so the daemon's stdout reaches CloudWatch.
#
# Everything is throwaway and force_destroy'd, because the conformance run
# destroys the stack whether it passes or fails.

terraform {
  required_version = ">= 1.6"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = ">= 3.0"
    }
  }
}

variable "region" {
  description = "AWS region. Must be a Lambda MicroVMs region."
  type        = string
  default     = "us-east-1"
}

variable "name_prefix" {
  description = "Prefix for all resource names, so a stray resource is traceable."
  type        = string
  default     = "agentd-conformance"
}

provider "aws" {
  region = var.region

  default_tags {
    tags = {
      "agentd:purpose" = "conformance"
      "agentd:managed" = "terraform"
    }
  }
}

data "aws_caller_identity" "current" {}

resource "random_id" "suffix" {
  byte_length = 4
}

locals {
  bucket_name = "${var.name_prefix}-${data.aws_caller_identity.current.account_id}-${random_id.suffix.hex}"
}

# ── artifact bucket ─────────────────────────────────────────────────────

resource "aws_s3_bucket" "artifacts" {
  # Ephemeral: versioning would defeat force_destroy, and access logging would
  # need a second bucket that outlives the thing it logs for.
  #checkov:skip=CKV_AWS_21:ephemeral conformance bucket; versioning defeats force_destroy
  #checkov:skip=CKV_AWS_18:access logging needs a second bucket; out of scope
  #checkov:skip=CKV_AWS_144:no cross-region replication for throwaway artifacts
  #checkov:skip=CKV2_AWS_61:stack is destroyed after each run
  #checkov:skip=CKV2_AWS_62:no event notifications needed
  #checkov:skip=CKV_AWS_145:SSE-S3 is sufficient; KMS adds key management to a throwaway stack
  bucket        = local.bucket_name
  force_destroy = true
}

resource "aws_s3_bucket_public_access_block" "artifacts" {
  bucket = aws_s3_bucket.artifacts.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "artifacts" {
  bucket = aws_s3_bucket.artifacts.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_policy" "artifacts_tls_only" {
  bucket     = aws_s3_bucket.artifacts.id
  depends_on = [aws_s3_bucket_public_access_block.artifacts]

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "DenyInsecureTransport"
        Effect    = "Deny"
        Principal = "*"
        Action    = "s3:*"
        Resource = [
          aws_s3_bucket.artifacts.arn,
          "${aws_s3_bucket.artifacts.arn}/*",
        ]
        Condition = {
          Bool = { "aws:SecureTransport" = "false" }
        }
      }
    ]
  })
}

# ── trust policy ────────────────────────────────────────────────────────
# The aws:SourceAccount condition is confused-deputy prevention, per the
# MicroVMs security documentation.

data "aws_iam_policy_document" "microvm_trust" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole", "sts:TagSession"]

    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [data.aws_caller_identity.current.account_id]
    }
  }
}

# ── image build role ────────────────────────────────────────────────────

resource "aws_iam_role" "build" {
  name               = "${var.name_prefix}-build-${random_id.suffix.hex}"
  assume_role_policy = data.aws_iam_policy_document.microvm_trust.json
}

data "aws_iam_policy_document" "build" {
  statement {
    sid       = "ReadCodeArtifact"
    effect    = "Allow"
    actions   = ["s3:GetObject"]
    resources = ["${aws_s3_bucket.artifacts.arn}/*"]
  }

  statement {
    sid    = "WriteBuildLogs"
    effect = "Allow"
    actions = [
      "logs:CreateLogGroup",
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    # The service writes build logs to /aws/lambda-microvms/<image-name>, NOT
    # /aws/lambda/microvms/*. Granting the wrong prefix does not fail the build:
    # it silently discards every log line, and each failure then reports
    # reason=unknown, which reads as the service withholding stateReason when it
    # is really this policy. Measured 2026-08-05; see docs/PLATFORM.md.
    resources = [
      "arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda-microvms/*",
    ]
  }

  # A task image pulled from a same-account ECR repository fails to build
  # outright without these. Harmless when the base image is public.
  statement {
    sid    = "PullBaseImageFromEcr"
    effect = "Allow"
    actions = [
      "ecr:GetAuthorizationToken",
      "ecr:BatchCheckLayerAvailability",
      "ecr:GetDownloadUrlForLayer",
      "ecr:BatchGetImage",
    ]
    resources = ["*"]
    #checkov:skip=CKV_AWS_355:GetAuthorizationToken is account-scoped and takes no resource
    #checkov:skip=CKV_AWS_356:same
  }
}

resource "aws_iam_role_policy" "build" {
  name   = "build"
  role   = aws_iam_role.build.id
  policy = data.aws_iam_policy_document.build.json
}

# ── execution role ──────────────────────────────────────────────────────
# Attached at RunMicrovm so the daemon's JSON logs reach CloudWatch. Those logs
# are the only channel for anything the daemon reports outside an HTTP response,
# which is what makes the loopback-origin measurement possible.

resource "aws_iam_role" "execution" {
  name               = "${var.name_prefix}-exec-${random_id.suffix.hex}"
  assume_role_policy = data.aws_iam_policy_document.microvm_trust.json
}

data "aws_iam_policy_document" "execution" {
  statement {
    sid    = "WriteRuntimeLogs"
    effect = "Allow"
    actions = [
      "logs:CreateLogGroup",
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    resources = [
      "arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:*",
    ]
  }
}

resource "aws_iam_role_policy" "execution" {
  name   = "execution"
  role   = aws_iam_role.execution.id
  policy = data.aws_iam_policy_document.execution.json
}

# ── build log read policy ───────────────────────────────────────────────
# The build and execution roles above only *write* logs. Reading the build log
# group — what `microvm logs` prints the `aws logs tail` invocation for — is
# done by the caller's own identity, which this module cannot know (it may be a
# user, a role, or an SSO session). So the read grant ships as a standalone
# managed policy: attach it to whatever identity runs `aws logs tail`, via the
# `logs_read_policy_arn` output. Without it, a fresh install's tail fails with
# AccessDeniedException even though the group exists and holds the build logs.

data "aws_iam_policy_document" "logs_read" {
  statement {
    sid    = "ReadBuildLogs"
    effect = "Allow"
    actions = [
      "logs:FilterLogEvents",
      "logs:GetLogEvents",
      "logs:DescribeLogStreams",
    ]
    # Same prefix hazard as WriteBuildLogs above: the service writes to
    # /aws/lambda-microvms/<image-name>, NOT /aws/lambda/microvms/*. The second
    # resource form is the log-stream ARN shape GetLogEvents authorizes against.
    resources = [
      "arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda-microvms/*",
      "arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda-microvms/*:log-stream:*",
    ]
  }
}

resource "aws_iam_policy" "logs_read" {
  name        = "${var.name_prefix}-logs-read-${random_id.suffix.hex}"
  description = "Read MicroVM image build log groups (/aws/lambda-microvms/*) — attach to the identity that runs `aws logs tail`"
  policy      = data.aws_iam_policy_document.logs_read.json
}

# ── outputs ─────────────────────────────────────────────────────────────

output "region" {
  value = var.region
}

output "s3_bucket" {
  value = aws_s3_bucket.artifacts.bucket
}

output "build_role_arn" {
  value = aws_iam_role.build.arn
}

output "execution_role_arn" {
  value = aws_iam_role.execution.arn
}

output "logs_read_policy_arn" {
  value = aws_iam_policy.logs_read.arn
}
