use anyhow::Result;
use clap::{Arg, ArgAction, Command};
use log::debug;
use log_stream_gc::{gc_log_streams, set_up_logger};

#[derive(Debug)]
struct Args {
    verbose: bool,
    dry_run: bool,
    region: String,
}

fn parse_args() -> Args {
    let matches = Command::new("log-stream-gc")
        .version("0.1")
        .author("Jacob Luszcz")
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("Verbose mode. Outputs DEBUG and higher log messages."),
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
        .get_matches();

    let verbose = matches.get_flag("verbose");
    let dry_run = matches.get_flag("dryrun");

    let region = matches
        .get_one::<String>("region")
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
