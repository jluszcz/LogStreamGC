# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LogStreamGC is a Rust application that automatically deletes CloudWatch log streams after their retention period has
passed. It can run as both a standalone CLI tool and an AWS Lambda function.

## Architecture

The project has a dual-binary structure:

- **CLI binary** (`src/main.rs`, built as `log-stream-gc`): Command-line tool. Beyond region, dry-run, and verbosity
  it exposes the whole `Config` surface — `--concurrency`, `--progress-threshold`, `--progress-interval`,
  `--retention-multiplier`, `--batch-size`, `--include-pattern`, `--exclude-pattern`
- **Lambda binary** (`src/lambda.rs`, built as `lambda`): AWS Lambda handler that runs the garbage collection
  automatically with `Config::default()`
- **Core library** (`src/lib.rs`): Shared logic for both binaries containing the main `gc_log_streams` functionality
- **Infrastructure** (`log-stream-gc.tf`): Lambda, IAM, the EventBridge schedule, and the GitHub deploy role

Tests live inline in `src/lib.rs` under `#[cfg(test)] mod tests`; there is no `tests/` directory.

The core algorithm:

1. Enumerates all CloudWatch log groups in a region (optionally filtered by include/exclude regex)
2. For each log group, calculates a cutoff date (retention period × a configurable multiplier, default 2×)
3. Deletes log streams whose last event predates the cutoff date — falling back to creation time only for streams that
   never received an event — with a global concurrency limit on deletions
4. Uses the AWS SDK's standard retry configuration (10 max attempts) to handle throttling

## Development Commands

### Build and Test

- `cargo build` - Build the project
- `cargo fmt` - Format the source code
- `cargo test` - Run all tests
- `cargo check` - Check for compilation errors without building
- `cargo clippy --all-targets -- -D warnings` - Run Rust linter for code quality checks (includes test code)
- `pre-commit run --all-files` - Run the repo's commit gate (`.pre-commit-config.yaml`, which enforces `cargo fmt`
  plus `terraform fmt`/`validate`)

CI builds and tests on `ubuntu-24.04-arm` against `aarch64-unknown-linux-musl`; to reproduce that locally use
`cargo build --release --target aarch64-unknown-linux-musl` (needs `musl-tools` and the target installed).

`.github/workflows/ci.yml` also calls the shared `terraform-ci.yml`, which runs `terraform fmt -check -recursive` and `terraform validate` with `-backend=false` — the same checks the `terraform_fmt`/`terraform_validate` pre-commit hooks run locally. Terraform is never *applied* by CI.

## Conventions

- `Config::default()` in `src/lib.rs` and the clap `default_value` strings in `src/main.rs` are hand-synchronized.
  Changing a default in one file requires changing it in the other; both carry a comment saying so.
- `Config` is public with public fields, so `gc_log_streams` calls `Config::normalize()` to clamp every limit before
  use — including `retention_multiplier`, where a non-positive or non-finite value would put the cutoff at or after
  today and delete every stream in the group. Add new clamping there rather than relying on clap's validators, which
  only cover the CLI path.
- The `lambda` binary name is load-bearing: the shared `lambda-package.yml` workflow copies
  `target/<target>/release/lambda` to `bootstrap`.

## Dependencies

- Uses `jluszcz_rust_utils` for logging, the Lambda entry point (`lambda::run`), AWS SDK configuration
  (`aws::config`), and the shared clap verbosity argument (`cli::VerbosityArgs`)
- AWS SDK for CloudWatch Logs operations
- Lambda runtime for AWS Lambda execution
- Clap for CLI argument parsing
- Chrono for date/time calculations

## Environment Configuration

Terraform uses per-region workspaces. Source the matching env script (`. env-<region>`)
to export `TF_VAR_aws_region` and select the workspace before running `terraform`:

- `env-us_east_1` — region `us-east-1`, workspace `log-stream-gc_us-east-1`
- `env-us_east_2` — region `us-east-2`, workspace `log-stream-gc_us-east-2`

## Deployment

The project auto-deploys to multiple AWS regions (us-east-1, us-east-2) via GitHub Actions when changes are pushed to
main. The CI/CD pipeline builds for ARM64 architecture and creates Lambda deployment packages.
