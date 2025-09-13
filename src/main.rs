use anyhow::Result;
use clap::{Arg, ArgAction, Command};
use jluszcz_rust_utils::{Verbosity, set_up_logger};
use log::debug;
use log_stream_gc::{APP_NAME, Config, gc_log_streams_with_config};
use regex::Regex;

#[derive(Debug)]
struct Args {
    verbosity: Verbosity,
    dry_run: bool,
    region: String,
    concurrency: usize,
    progress_threshold: usize,
    progress_interval: usize,
    retention_multiplier: f64,
    batch_size: usize,
    include_pattern: Option<String>,
    exclude_pattern: Option<String>,
}

fn parse_args() -> Result<Args> {
    let matches = Command::new("log-stream-gc")
        .version("0.1")
        .author("Jacob Luszcz")
        .arg(
            Arg::new("verbosity")
                .short('v')
                .action(ArgAction::Count)
                .help("Verbose mode. Use -v for DEBUG, -vv for TRACE level logging."),
        )
        .arg(
            Arg::new("dryrun")
                .short('d')
                .long("dryrun")
                .alias("dry-run")
                .action(ArgAction::SetTrue)
                .help("Keeps all log streams, even if they would otherwise be deleted."),
        )
        .arg(
            Arg::new("region")
                .short('r')
                .long("region")
                .required(true)
                .env("AWS_REGION")
                .help("AWS region to run garbage collection in."),
        )
        .arg(
            Arg::new("concurrency")
                .short('c')
                .long("concurrency")
                .value_name("NUM")
                .default_value("10")
                .help("Number of concurrent log stream deletions."),
        )
        .arg(
            Arg::new("progress-threshold")
                .long("progress-threshold")
                .value_name("NUM")
                .default_value("500")
                .help("Minimum number of log streams before showing progress updates."),
        )
        .arg(
            Arg::new("progress-interval")
                .long("progress-interval")
                .value_name("NUM")
                .default_value("100")
                .help("Show progress every N log streams."),
        )
        .arg(
            Arg::new("retention-multiplier")
                .long("retention-multiplier")
                .value_name("NUM")
                .default_value("2.0")
                .help("Multiplier for retention period (e.g., 2.0 = 2x retention period)."),
        )
        .arg(
            Arg::new("batch-size")
                .long("batch-size")
                .value_name("NUM")
                .default_value("50")
                .help("Number of log groups to process in each batch."),
        )
        .arg(
            Arg::new("include-pattern")
                .long("include-pattern")
                .value_name("REGEX")
                .help("Only process log groups matching this regex pattern."),
        )
        .arg(
            Arg::new("exclude-pattern")
                .long("exclude-pattern")
                .value_name("REGEX")
                .help("Skip log groups matching this regex pattern."),
        )
        .get_matches();

    let verbosity = matches.get_count("verbosity").into();
    let dry_run = matches.get_flag("dryrun");

    let region = matches
        .get_one::<String>("region")
        .expect("region is required")
        .to_string();

    let concurrency = matches
        .get_one::<String>("concurrency")
        .unwrap()
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid concurrency value"))?;

    let progress_threshold = matches
        .get_one::<String>("progress-threshold")
        .unwrap()
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid progress-threshold value"))?;

    let progress_interval = matches
        .get_one::<String>("progress-interval")
        .unwrap()
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid progress-interval value"))?;

    let retention_multiplier = matches
        .get_one::<String>("retention-multiplier")
        .unwrap()
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("Invalid retention-multiplier value"))?;

    let batch_size = matches
        .get_one::<String>("batch-size")
        .unwrap()
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid batch-size value"))?;

    let include_pattern = matches.get_one::<String>("include-pattern").cloned();
    let exclude_pattern = matches.get_one::<String>("exclude-pattern").cloned();

    // Validate numeric values
    if concurrency == 0 {
        return Err(anyhow::anyhow!("Concurrency must be greater than 0"));
    }
    if progress_interval == 0 {
        return Err(anyhow::anyhow!("Progress interval must be greater than 0"));
    }
    if retention_multiplier <= 0.0 {
        return Err(anyhow::anyhow!(
            "Retention multiplier must be greater than 0"
        ));
    }
    if batch_size == 0 {
        return Err(anyhow::anyhow!("Batch size must be greater than 0"));
    }

    Ok(Args {
        verbosity,
        dry_run,
        region,
        concurrency,
        progress_threshold,
        progress_interval,
        retention_multiplier,
        batch_size,
        include_pattern,
        exclude_pattern,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    set_up_logger(APP_NAME, module_path!(), args.verbosity)?;
    debug!("{args:?}");

    // Build regex patterns if provided
    let include_pattern = if let Some(pattern) = args.include_pattern {
        Some(
            Regex::new(&pattern)
                .map_err(|e| anyhow::anyhow!("Invalid include pattern regex: {}", e))?,
        )
    } else {
        None
    };

    let exclude_pattern = if let Some(pattern) = args.exclude_pattern {
        Some(
            Regex::new(&pattern)
                .map_err(|e| anyhow::anyhow!("Invalid exclude pattern regex: {}", e))?,
        )
    } else {
        None
    };

    let config = Config {
        concurrency_limit: args.concurrency,
        progress_threshold: args.progress_threshold,
        progress_interval: args.progress_interval,
        retention_multiplier: args.retention_multiplier,
        batch_size: args.batch_size,
        include_pattern,
        exclude_pattern,
    };

    gc_log_streams_with_config(Some(args.region), &config, args.dry_run).await
}
