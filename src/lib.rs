use anyhow::{anyhow, Result};
use aws_config::retry::RetryConfig;
use aws_config::ConfigLoader;
use aws_sdk_cloudwatchlogs::config::Region;
use aws_sdk_cloudwatchlogs::types::{LogGroup, LogStream};
use aws_sdk_cloudwatchlogs::Client;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use log::{debug, info, trace, LevelFilter};
use std::borrow::Cow;

const MIN_ITEMS_FOR_PROGRESS_UPDATE: usize = 500;
const ITEM_PROGRESS_INTERVAL: usize = 100;

pub fn set_up_logger<T>(calling_module: T, verbose: bool) -> Result<()>
where
    T: Into<Cow<'static, str>>,
{
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let _ = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] [{}] {}",
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(LevelFilter::Warn)
        .level_for("log_stream_gc", level)
        .level_for(calling_module, level)
        .chain(std::io::stdout())
        .apply();

    Ok(())
}

fn parse_timestamp(timestamp: i64) -> DateTime<Utc> {
    let secs = timestamp / 1000;
    let nsecs = (timestamp % 1000) as u32;

    DateTime::<Utc>::from_naive_utc_and_offset(
        NaiveDateTime::from_timestamp_opt(secs, nsecs).expect("Cannot create timestamp"),
        Utc,
    )
}

async fn describe_log_groups(client: &Client) -> Result<Vec<LogGroup>> {
    let mut next_token = None;
    let mut log_groups = Vec::new();

    loop {
        trace!("Describing log groups (next_token={:?})", next_token);

        let describe_output = client
            .describe_log_groups()
            .set_next_token(next_token)
            .send()
            .await?;
        debug!("Described log groups");

        log_groups.append(&mut describe_output.log_groups.unwrap_or_default());

        next_token = describe_output.next_token;
        if next_token.is_none() {
            info!("Found {} log group(s)", log_groups.len());
            break Ok(log_groups);
        }
    }
}

async fn describe_log_streams(client: &Client, log_group_name: &str) -> Result<Vec<LogStream>> {
    let mut next_token = None;
    let mut log_streams = Vec::new();

    loop {
        trace!(
            "Describing log streams for {} (next_token={:?})",
            log_group_name,
            next_token
        );

        let describe_output = client
            .describe_log_streams()
            .log_group_name(log_group_name)
            .set_next_token(next_token)
            .send()
            .await?;
        debug!("Described log streams for {}", log_group_name);

        log_streams.append(&mut describe_output.log_streams.unwrap_or_default());

        next_token = describe_output.next_token;
        if next_token.is_none() {
            info!(
                "Found {} log stream(s) for {}",
                log_streams.len(),
                log_group_name
            );
            break Ok(log_streams);
        }
    }
}

async fn gc_log_stream(
    client: &Client,
    keep_from_date: &NaiveDate,
    log_group_name: &str,
    log_stream: LogStream,
    dry_run: bool,
) -> Result<()> {
    let log_stream_name = log_stream
        .log_stream_name()
        .ok_or_else(|| anyhow!("{:#?} is missing a name", log_stream))?;

    let log_stream_creation_date = log_stream
        .creation_time()
        .map(parse_timestamp)
        .ok_or_else(|| anyhow!("{:#?} is missing a valid creation time", log_stream))?
        .date_naive();

    if log_stream_creation_date < *keep_from_date {
        debug!(
            "{} {}/{} (creation date {} < {})",
            if dry_run {
                "Keeping (Dry-Run)"
            } else {
                "Deleting"
            },
            log_group_name,
            log_stream_name,
            log_stream_creation_date,
            keep_from_date
        );

        if !dry_run {
            debug!("Deleting {}/{}", log_group_name, log_stream_name);
            client
                .delete_log_stream()
                .log_group_name(log_group_name)
                .log_stream_name(log_stream_name)
                .send()
                .await?;
            debug!("Deleted {}/{}", log_group_name, log_stream_name);
        }
    } else {
        debug!(
            "Keeping {}/{} (creation date {} >= {})",
            log_group_name, log_stream_name, log_stream_creation_date, keep_from_date
        );
    }

    Ok(())
}

async fn gc_log_group(client: &Client, log_group: LogGroup, dry_run: bool) -> Result<()> {
    let log_group_name = log_group
        .log_group_name()
        .ok_or_else(|| anyhow!("{:#?} is missing a name", log_group))?
        .to_string();

    let log_group_retention_period: i64 = log_group
        .retention_in_days()
        .ok_or_else(|| anyhow!("{:#?} is missing a retention period", log_group))?
        .into();

    let keep_from_date = Utc::now().date_naive() - Duration::days(2 * log_group_retention_period);

    debug!(
        "Cleaning up {} from before {}",
        log_group_name, keep_from_date
    );

    let log_streams = describe_log_streams(client, &log_group_name).await?;
    let log_stream_ct = log_streams.len();
    for (idx, log_stream) in log_streams.into_iter().enumerate() {
        gc_log_stream(
            client,
            &keep_from_date,
            &log_group_name,
            log_stream,
            dry_run,
        )
        .await?;

        if log_stream_ct > MIN_ITEMS_FOR_PROGRESS_UPDATE && (idx + 1) % ITEM_PROGRESS_INTERVAL == 0
        {
            info!(
                "Cleaned up {}/{} log streams in {}",
                idx + 1,
                log_stream_ct,
                log_group_name
            );
        }
    }

    Ok(())
}

pub async fn gc_log_streams(region: Option<String>, dry_run: bool) -> Result<()> {
    let mut config = ConfigLoader::default();
    if let Some(region) = region {
        config = config.region(Region::new(region));
    }

    // FIXME If there are many log streams to delete, throttle rates can get pretty high - until I figure out how to handle
    //  throttling errors properly in gc_log_stream, use a large max attempts here
    //
    let config = config
        .retry_config(RetryConfig::standard().with_max_attempts(25))
        .load()
        .await;

    let client = Client::new(&config);

    let log_groups = describe_log_groups(&client).await?;
    debug!("{} log group(s) to garbage collect", log_groups.len());

    for log_group in log_groups {
        gc_log_group(&client, log_group, dry_run).await?;
    }

    Ok(())
}
