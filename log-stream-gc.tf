terraform {
  backend "s3" {}
}

# Sourced from environment variables named TF_VAR_${VAR_NAME}
variable "aws_acct_id" {}

variable "aws_region" {}

variable "code_bucket" {}

provider "aws" {
  region = var.aws_region
}

data "aws_s3_bucket" "code_bucket" {
  bucket = var.code_bucket
}

// Run daily at 15:00 UTC
resource "aws_cloudwatch_event_rule" "schedule" {
  name                = "log-stream-gc-schedule"
  schedule_expression = "cron(0 15 * * ? *)"
}

resource "aws_cloudwatch_event_target" "schedule_target" {
  rule = aws_cloudwatch_event_rule.schedule.name
  arn  = aws_lambda_function.log_stream_gc.arn
}

resource "aws_lambda_permission" "cw_execution" {
  statement_id  = "LogStreamGC-AllowExecutionFromCloudWatch"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.log_stream_gc.arn
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.schedule.arn
}

data "aws_iam_policy_document" "cw_actions" {
  statement {
    actions   = ["logs:Describe*", "logs:DeleteLogStream"]
    resources = ["arn:aws:logs:${var.aws_region}:${var.aws_acct_id}:*"]
  }
}

data "aws_iam_policy_document" "assume_role" {
  statement {
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
    actions = ["sts:AssumeRole"]
  }
}

resource "aws_iam_policy" "role_policy" {
  name   = "log-stream-gc-${var.aws_region}"
  policy = data.aws_iam_policy_document.cw_actions.json
}

resource "aws_iam_role" "role" {
  name               = "log-stream-gc.lambda.${var.aws_region}"
  assume_role_policy = data.aws_iam_policy_document.assume_role.json
}

resource "aws_iam_role_policy_attachment" "role_attachment" {
  role       = aws_iam_role.role.name
  policy_arn = aws_iam_policy.role_policy.arn
}

resource "aws_iam_role_policy_attachment" "basic_execution_role_attachment" {
  role       = aws_iam_role.role.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_lambda_function" "log_stream_gc" {
  function_name = "log-stream-gc"
  s3_bucket     = "${data.aws_s3_bucket.code_bucket.bucket}"
  s3_key        = "log-stream-gc.zip"
  role          = aws_iam_role.role.arn
  architectures = ["arm64"]
  runtime       = "provided.al2023"
  handler       = "ignored"
  publish       = "false"
  description   = "Clean up older log streams"
  timeout       = 5
  memory_size   = 128
}

resource "aws_cloudwatch_log_group" "log_group" {
  name              = "/aws/lambda/log-stream-gc"
  retention_in_days = "7"
}

data "aws_iam_openid_connect_provider" "github" {
  url = "https://token.actions.githubusercontent.com"
}

data "aws_iam_policy_document" "github" {
  statement {
    actions = ["s3:PutObject"]
    resources = ["${data.aws_s3_bucket.code_bucket.arn}/log-stream-gc.zip"]
  }
}

resource "aws_iam_policy" "github" {
  name   = "log-stream-gc.github.${var.aws_region}"
  policy = data.aws_iam_policy_document.github.json
}

resource "aws_iam_role" "github" {
  name = "log-stream-gc.github.${var.aws_region}"

  assume_role_policy = jsonencode({
    Version = "2012-10-17",
    Statement = [
      {
        Effect = "Allow",
        Principal = {
          Federated = "${data.aws_iam_openid_connect_provider.github.arn}"
        },
        Action = "sts:AssumeRoleWithWebIdentity",
        Condition = {
          StringEquals = {
            "token.actions.githubusercontent.com:aud" : "sts.amazonaws.com"
          }
          StringLike = {
            "token.actions.githubusercontent.com:sub" : "repo:jluszcz/LogStreamGC:*"
          },
        }
      }
    ]
  })
}

resource "aws_iam_role_policy_attachment" "github" {
  role       = aws_iam_role.github.name
  policy_arn = aws_iam_policy.github.arn
}
