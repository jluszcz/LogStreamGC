#  LogStreamGC

Delete CloudWatch log streams after their current retention period has passed.

[![Status Badge](https://github.com/jluszcz/LogStreamGC/actions/workflows/build-and-deploy.yml/badge.svg)](https://github.com/jluszcz/LogStreamGC/actions/workflows/build-and-deploy.yml)

## Overview

LogStreamGC is a Rust tool that garbage-collects old CloudWatch log streams. For each log group that has a retention
policy set, it deletes any log streams whose creation date is older than a configurable multiple of that retention
period (default: 2×).

Log groups without a retention policy are skipped.

## How It Works

1. Enumerate all CloudWatch log groups in the target region (optionally filtered by regex)
2. For each log group, compute a cutoff date: `now - (retention_period × retention_multiplier)`
3. Delete all log streams whose creation time is before the cutoff date
4. Throttling errors on deletion are retried with exponential backoff (up to 5 attempts)

## Usage

### CLI

```
log-stream-gc [OPTIONS] --region <REGION>
```

| Option | Short | Default | Description |
|---|---|---|---|
| `--region <REGION>` | `-r` | *(required)* | AWS region to run garbage collection in. Also reads `AWS_REGION`. |
| `--dryrun` | `-d` | `false` | Log what would be deleted without actually deleting anything. |
| `--concurrency <NUM>` | `-c` | `10` | Number of concurrent log stream deletions per log group. |
| `--retention-multiplier <NUM>` | | `2.0` | Multiplier applied to the log group's retention period to determine the cutoff. |
| `--batch-size <NUM>` | | `50` | Number of log groups to process per batch. |
| `--include-pattern <REGEX>` | | | Only process log groups whose name matches this regex. |
| `--exclude-pattern <REGEX>` | | | Skip log groups whose name matches this regex. |
| `--progress-threshold <NUM>` | | `500` | Minimum number of log streams in a group before showing progress updates. |
| `--progress-interval <NUM>` | | `100` | Show a progress update every N log streams (when above threshold). |
| `-v` / `-vv` | | | Enable DEBUG / TRACE logging. |

**Examples**

```sh
# Dry-run in us-east-1
log-stream-gc --region us-east-1 --dryrun

# Only clean up /aws/lambda/* log groups
log-stream-gc --region us-east-2 --include-pattern '^/aws/lambda/'

# Use a 3× retention multiplier
log-stream-gc --region us-east-1 --retention-multiplier 3.0
```

### Lambda

The Lambda binary reads the AWS region from the execution environment and runs a single garbage collection pass with
default settings. It is triggered daily at **15:00 UTC** via an EventBridge (CloudWatch Events) schedule.

## Architecture

The project contains two binaries backed by a shared library:

| Binary | Source | Description |
|---|---|---|
| `main` | `src/main.rs` | CLI tool with full argument support |
| `lambda` | `src/lambda.rs` | AWS Lambda handler for scheduled runs |

Core logic lives in `src/lib.rs` (`gc_log_streams` and related functions).

## IAM Permissions

The Lambda execution role requires:

```json
{
  "logs:Describe*": "arn:aws:logs:<region>:<account>:*",
  "logs:DeleteLogStream": "arn:aws:logs:<region>:<account>:*",
  "cloudwatch:PutMetricData": "*" (scoped to namespace "log_stream_gc")
}
```

Plus `AWSLambdaBasicExecutionRole` for CloudWatch Logs write access.

## Deployment

Pushes to `main` that touch `src/**`, `Cargo*`, or `.github/workflows/**` trigger the CI/CD pipeline:

1. **Build & test** on `ubuntu-24.04-arm` targeting `aarch64-unknown-linux-musl`
2. **Lint** with `cargo clippy -- -D warnings`
3. **Package** the `lambda` binary as `log-stream-gc.zip`
4. **Deploy** to `us-east-1` and `us-east-2` in parallel via `deploy-lambda.yml`

Infrastructure is managed with Terraform (`log-stream-gc.tf`). Lambda runs on `provided.al2023`, ARM64, with 128 MB
memory and a 5-second timeout.

## Development

```sh
cargo build       # Build
cargo test        # Run tests
cargo fmt         # Format
cargo check       # Check without building
cargo clippy -- -D warnings  # Lint
```
