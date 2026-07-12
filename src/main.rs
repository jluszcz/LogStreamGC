use anyhow::Result;
use clap::Parser;
use jluszcz_rust_utils::{Verbosity, set_up_logger};
use log::debug;
use log_stream_gc::{APP_NAME, Config, gc_log_streams};
use regex::Regex;

fn parse_non_zero_usize(s: &str) -> Result<usize, String> {
    let v: usize = s
        .parse()
        .map_err(|e: std::num::ParseIntError| e.to_string())?;
    if v == 0 {
        Err("must be greater than 0".into())
    } else {
        Ok(v)
    }
}

fn parse_positive_f64(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    if v <= 0.0 {
        Err("must be greater than 0".into())
    } else {
        Ok(v)
    }
}

fn parse_regex(s: &str) -> Result<Regex, String> {
    Regex::new(s).map_err(|e| e.to_string())
}

// The default_value strings below must match Config::default() in lib.rs.
#[derive(Debug, Parser)]
#[command(version, author, infer_long_args = true)]
struct Args {
    /// Verbose mode. Use -v for DEBUG, -vv for TRACE level logging.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Keeps all log streams, even if they would otherwise be deleted.
    #[arg(short = 'd', long, alias = "dry-run")]
    dryrun: bool,

    /// AWS region to run garbage collection in.
    #[arg(short = 'r', long, env = "AWS_REGION")]
    region: String,

    /// Maximum number of concurrent log stream deletions across all log groups.
    #[arg(
        short = 'c',
        long,
        value_name = "NUM",
        default_value = "10",
        value_parser = parse_non_zero_usize
    )]
    concurrency: usize,

    /// Minimum number of log streams before showing progress updates.
    #[arg(long, value_name = "NUM", default_value = "500")]
    progress_threshold: usize,

    /// Show progress every N log streams.
    #[arg(
        long,
        value_name = "NUM",
        default_value = "100",
        value_parser = parse_non_zero_usize
    )]
    progress_interval: usize,

    /// Multiplier for retention period (e.g., 2.0 = 2x retention period).
    #[arg(
        long,
        value_name = "NUM",
        default_value = "2.0",
        value_parser = parse_positive_f64
    )]
    retention_multiplier: f64,

    /// Log groups requested per AWS page (clamped to 1-50, the API limit).
    #[arg(
        long,
        value_name = "NUM",
        default_value = "50",
        value_parser = parse_non_zero_usize
    )]
    batch_size: usize,

    /// Only process log groups matching this regex pattern.
    #[arg(long, value_name = "REGEX", value_parser = parse_regex)]
    include_pattern: Option<Regex>,

    /// Skip log groups matching this regex pattern.
    #[arg(long, value_name = "REGEX", value_parser = parse_regex)]
    exclude_pattern: Option<Regex>,
}

fn parse_args() -> (Verbosity, bool, String, Config) {
    let args = Args::parse();

    let config = Config {
        concurrency_limit: args.concurrency,
        progress_threshold: args.progress_threshold,
        progress_interval: args.progress_interval,
        retention_multiplier: args.retention_multiplier,
        batch_size: args.batch_size,
        include_pattern: args.include_pattern,
        exclude_pattern: args.exclude_pattern,
    };

    (args.verbosity.into(), args.dryrun, args.region, config)
}

#[tokio::main]
async fn main() -> Result<()> {
    let (verbosity, dry_run, region, config) = parse_args();
    set_up_logger(APP_NAME, module_path!(), verbosity)?;
    debug!("region={region} dry_run={dry_run} config={config:?}");

    gc_log_streams(Some(region), config, dry_run).await
}
