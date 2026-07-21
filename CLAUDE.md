# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

LogStreamGC is a Rust application that automatically deletes CloudWatch log streams after their retention period has
passed. It can run as both a standalone CLI tool and an AWS Lambda function.

## Architecture

The project has a dual-binary structure:

- **CLI binary** (`src/main.rs`): Command-line tool with arguments for region, dry-run mode, and verbosity
- **Lambda binary** (`src/lambda.rs`): AWS Lambda handler that runs the garbage collection automatically
- **Core library** (`src/lib.rs`): Shared logic for both binaries containing the main `gc_log_streams` functionality

The core algorithm:

1. Enumerates all CloudWatch log groups in a region (optionally filtered by include/exclude regex)
2. For each log group, calculates a cutoff date (retention period × a configurable multiplier, default 2×)
3. Deletes log streams created before the cutoff date, with a global concurrency limit on deletions
4. Uses the AWS SDK's standard retry configuration (10 max attempts) to handle throttling

## Development Commands

### Build and Test

- `cargo build` - Build the project
- `cargo fmt` - Format the source code
- `cargo test` - Run all tests
- `cargo check` - Check for compilation errors without building
- `cargo clippy --all-targets -- -D warnings` - Run Rust linter for code quality checks (includes test code)

## Dependencies

- Uses `jluszcz_rust_utils` for logging utilities
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
