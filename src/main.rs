use anyhow::Result;
use clap::{App, Arg};
use log::debug;
use log_stream_gc::{gc_log_streams, set_up_logger};

#[derive(Debug)]
struct Args {
    verbose: bool,
    dry_run: bool,
    region: String,
}

fn parse_args() -> Args {
    let matches = App::new("log-stream-gc")
        .version("0.1")
        .author("Jacob Luszcz")
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .help("Verbose mode. Outputs DEBUG and higher log messages."),
        )
        .arg(
            Arg::new("dryrun")
                .short('d')
                .long("dryrun")
                .alias("dry-run")
                .help("Keeps all log streams, even if they would otherwise be deleted."),
        )
        .arg(
            Arg::new("region")
                .short('r')
                .long("region")
                .required(true)
                .takes_value(true)
                .env("AWS_REGION")
                .help("AWS region to run garbage collection in."),
        )
        .get_matches();

    let verbose = matches.is_present("verbose");
    let dry_run = matches.is_present("dryrun");

    let region = matches
        .value_of("region")
        .expect("region is required")
        .to_string();

    Args {
        verbose,
        dry_run,
        region,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    set_up_logger(module_path!(), args.verbose)?;
    debug!("{:?}", args);

    gc_log_streams(Some(args.region), args.dry_run).await
}
