use anyhow::Result;
use clap::{Arg, ArgAction, Command, value_parser};
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

fn parse_args() -> Result<(Verbosity, bool, String, Config)> {
    // The default_value strings below must match Config::default() in lib.rs.
    let matches = Command::new("log-stream-gc")
        .version(env!("CARGO_PKG_VERSION"))
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
                .value_parser(parse_non_zero_usize)
                .help("Maximum number of concurrent log stream deletions across all log groups."),
        )
        .arg(
            Arg::new("progress-threshold")
                .long("progress-threshold")
                .value_name("NUM")
                .default_value("500")
                .value_parser(value_parser!(usize))
                .help("Minimum number of log streams before showing progress updates."),
        )
        .arg(
            Arg::new("progress-interval")
                .long("progress-interval")
                .value_name("NUM")
                .default_value("100")
                .value_parser(parse_non_zero_usize)
                .help("Show progress every N log streams."),
        )
        .arg(
            Arg::new("retention-multiplier")
                .long("retention-multiplier")
                .value_name("NUM")
                .default_value("2.0")
                .value_parser(parse_positive_f64)
                .help("Multiplier for retention period (e.g., 2.0 = 2x retention period)."),
        )
        .arg(
            Arg::new("batch-size")
                .long("batch-size")
                .value_name("NUM")
                .default_value("50")
                .value_parser(parse_non_zero_usize)
                .help("Log groups requested per AWS page (clamped to 1-50, the API limit)."),
        )
        .arg(
            Arg::new("include-pattern")
                .long("include-pattern")
                .value_name("REGEX")
                .value_parser(parse_regex)
                .help("Only process log groups matching this regex pattern."),
        )
        .arg(
            Arg::new("exclude-pattern")
                .long("exclude-pattern")
                .value_name("REGEX")
                .value_parser(parse_regex)
                .help("Skip log groups matching this regex pattern."),
        )
        .get_matches();

    let verbosity = matches.get_count("verbosity").into();
    let dry_run = matches.get_flag("dryrun");
    let region = matches.get_one::<String>("region").unwrap().to_string();

    let config = Config {
        concurrency_limit: *matches.get_one("concurrency").unwrap(),
        progress_threshold: *matches.get_one("progress-threshold").unwrap(),
        progress_interval: *matches.get_one("progress-interval").unwrap(),
        retention_multiplier: *matches.get_one("retention-multiplier").unwrap(),
        batch_size: *matches.get_one("batch-size").unwrap(),
        include_pattern: matches.get_one::<Regex>("include-pattern").cloned(),
        exclude_pattern: matches.get_one::<Regex>("exclude-pattern").cloned(),
    };

    Ok((verbosity, dry_run, region, config))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (verbosity, dry_run, region, config) = parse_args()?;
    set_up_logger(APP_NAME, module_path!(), verbosity)?;
    debug!("region={region} dry_run={dry_run} config={config:?}");

    gc_log_streams(Some(region), config, dry_run).await
}
