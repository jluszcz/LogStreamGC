use anyhow::{Context, Result, anyhow};
use aws_config::ConfigLoader;
use aws_config::retry::RetryConfig;
use aws_sdk_cloudwatchlogs::Client;
use aws_sdk_cloudwatchlogs::config::Region;
use aws_sdk_cloudwatchlogs::types::{LogGroup, LogStream};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use futures::stream::{self, StreamExt};
use log::{debug, info, trace, warn};
use regex::Regex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::task;
use tokio::time::{Duration as TokioDuration, sleep};

pub const APP_NAME: &str = "log_stream_gc";

#[derive(Debug, Clone)]
pub struct Config {
    pub concurrency_limit: usize,
    pub progress_threshold: usize,
    pub progress_interval: usize,
    pub retention_multiplier: f64,
    pub batch_size: usize,
    pub include_pattern: Option<Regex>,
    pub exclude_pattern: Option<Regex>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            concurrency_limit: 10,
            progress_threshold: 500,
            progress_interval: 100,
            retention_multiplier: 2.0,
            batch_size: 50,
            include_pattern: None,
            exclude_pattern: None,
        }
    }
}

fn parse_timestamp(timestamp: i64) -> Result<DateTime<Utc>> {
    if timestamp < 0 {
        return Err(anyhow!("Invalid timestamp: {}", timestamp));
    }

    let secs = timestamp / 1000;
    let nsecs = ((timestamp % 1000) * 1_000_000) as u32;

    DateTime::<Utc>::from_timestamp(secs, nsecs)
        .ok_or_else(|| anyhow!("Failed to parse timestamp: {}", timestamp))
}

fn should_process_log_group(group: &LogGroup, config: &Config) -> bool {
    let Some(name) = group.log_group_name() else {
        return false;
    };

    // Skip log groups without a positive retention — there's no cutoff date to compute.
    if !matches!(group.retention_in_days(), Some(days) if days > 0) {
        return false;
    }

    let include_match = config
        .include_pattern
        .as_ref()
        .is_none_or(|pattern| pattern.is_match(name));
    if !include_match {
        return false;
    }

    let exclude_match = config
        .exclude_pattern
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(name));
    !exclude_match
}

async fn describe_log_streams(client: &Client, log_group_name: &str) -> Result<Vec<LogStream>> {
    let mut next_token = None;
    let mut log_streams = Vec::new();

    loop {
        trace!("Describing log streams for {log_group_name} (next_token={next_token:?})");

        let describe_output = client
            .describe_log_streams()
            .log_group_name(log_group_name)
            .set_next_token(next_token)
            .send()
            .await
            .with_context(|| {
                format!("Failed to describe log streams for log group: {log_group_name}")
            })?;
        debug!("Described log streams for {log_group_name}");

        log_streams.extend(describe_output.log_streams.unwrap_or_default());

        next_token = describe_output.next_token;
        if next_token.is_none() {
            info!(
                "Found {} log stream(s) for {log_group_name}",
                log_streams.len()
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
        .ok_or_else(|| anyhow!("Log stream is missing a name"))?;

    let creation_time = log_stream
        .creation_time()
        .ok_or_else(|| anyhow!("Log stream {} is missing a creation time", log_stream_name))?;

    let log_stream_creation_date = parse_timestamp(creation_time)
        .with_context(|| {
            format!("Failed to parse creation time for log stream: {log_stream_name}")
        })?
        .date_naive();

    if log_stream_creation_date < *keep_from_date {
        debug!(
            "{} {log_group_name}/{log_stream_name} (creation date {log_stream_creation_date} < {keep_from_date})",
            if dry_run {
                "Keeping (Dry-Run)"
            } else {
                "Deleting"
            }
        );

        if !dry_run {
            delete_log_stream_with_retry(client, log_group_name, log_stream_name)
                .await
                .with_context(|| {
                    format!("Failed to delete log stream: {log_group_name}/{log_stream_name}")
                })?;
        }
    } else {
        debug!(
            "Keeping {log_group_name}/{log_stream_name} (creation date {log_stream_creation_date} >= {keep_from_date})"
        );
    }

    Ok(())
}

async fn delete_log_stream_with_retry(
    client: &Client,
    log_group_name: &str,
    log_stream_name: &str,
) -> Result<()> {
    const MAX_RETRIES: u32 = 5;
    const BASE_DELAY_MS: u64 = 100;

    let mut attempt: u32 = 0;
    loop {
        match client
            .delete_log_stream()
            .log_group_name(log_group_name)
            .log_stream_name(log_stream_name)
            .send()
            .await
        {
            Ok(_) => {
                debug!("Deleted {log_group_name}/{log_stream_name}");
                return Ok(());
            }
            Err(err) => {
                let error_message = err.to_string().to_lowercase();
                let is_throttle =
                    error_message.contains("throttl") || error_message.contains("rate");

                if !is_throttle || attempt >= MAX_RETRIES {
                    return Err(anyhow::Error::from(err));
                }

                let delay_ms = BASE_DELAY_MS * 2_u64.pow(attempt);
                warn!(
                    "Throttling detected for {log_group_name}/{log_stream_name}, retrying in {delay_ms}ms (attempt {}/{})",
                    attempt + 1,
                    MAX_RETRIES + 1
                );
                sleep(TokioDuration::from_millis(delay_ms)).await;
                attempt += 1;
            }
        }
    }
}

async fn gc_log_group(
    client: &Client,
    log_group: LogGroup,
    config: &Config,
    dry_run: bool,
    processed_counter: Arc<AtomicUsize>,
    total_streams: Arc<AtomicUsize>,
) -> Result<()> {
    let log_group_name = log_group
        .log_group_name()
        .ok_or_else(|| anyhow!("Log group is missing a name"))?
        .to_string();

    // Invariant: callers filter via `should_process_log_group`, which guarantees positive retention.
    let log_group_retention_period: i64 = log_group
        .retention_in_days()
        .expect("log group passed should_process_log_group filter")
        .into();

    let retention_days = (log_group_retention_period as f64 * config.retention_multiplier) as i64;
    let keep_from_date = Utc::now().date_naive()
        - Duration::try_days(retention_days)
            .ok_or_else(|| anyhow!("Failed to create duration for {} days", retention_days))?;

    debug!(
        "Cleaning up {log_group_name} from before {keep_from_date} (retention: {}d * {} = {}d)",
        log_group_retention_period, config.retention_multiplier, retention_days
    );

    let log_streams = describe_log_streams(client, &log_group_name).await?;
    let log_stream_ct = log_streams.len();
    total_streams.fetch_add(log_stream_ct, Ordering::Relaxed);

    if log_stream_ct == 0 {
        debug!("No log streams found in {log_group_name}");
        return Ok(());
    }

    let progress_threshold = config.progress_threshold;
    let progress_interval = config.progress_interval;
    let concurrency_limit = config.concurrency_limit;

    let start_time = Instant::now();
    let stream_futures = stream::iter(log_streams.into_iter().enumerate())
        .map(|(idx, log_stream)| {
            let client = client.clone();
            let log_group_name = log_group_name.clone();
            let processed_counter = processed_counter.clone();

            async move {
                let result = gc_log_stream(
                    &client,
                    &keep_from_date,
                    &log_group_name,
                    log_stream,
                    dry_run,
                )
                .await;

                let processed = processed_counter.fetch_add(1, Ordering::Relaxed) + 1;

                if log_stream_ct > progress_threshold && (idx + 1) % progress_interval == 0 {
                    let elapsed = start_time.elapsed();
                    let rate = processed as f64 / elapsed.as_secs_f64();
                    info!(
                        "Processed {}/{log_stream_ct} log streams in {log_group_name} ({:.1} streams/sec)",
                        idx + 1,
                        rate
                    );
                }

                result
            }
        })
        .buffer_unordered(concurrency_limit);

    let results: Vec<Result<()>> = stream_futures.collect().await;

    let mut errors = Vec::new();
    for result in results {
        if let Err(e) = result {
            errors.push(e);
        }
    }

    if !errors.is_empty() {
        let error_count = errors.len();
        let first_error = errors.into_iter().next().unwrap();
        return Err(anyhow!(
            "Failed to process {} log streams in {}: {}",
            error_count,
            log_group_name,
            first_error
        ));
    }

    let elapsed = start_time.elapsed();
    debug!(
        "Completed processing {} log streams in {} in {:.2}s",
        log_stream_ct,
        log_group_name,
        elapsed.as_secs_f64()
    );

    Ok(())
}

pub async fn gc_log_streams(region: Option<String>, config: Config, dry_run: bool) -> Result<()> {
    let mut aws_config = ConfigLoader::default();
    if let Some(region) = region {
        aws_config = aws_config.region(Region::new(region));
    }

    let aws_config = aws_config
        .retry_config(RetryConfig::standard())
        .load()
        .await;

    let client = Client::new(&aws_config);
    let config = Arc::new(config);

    let processed_counter = Arc::new(AtomicUsize::new(0));
    let total_streams = Arc::new(AtomicUsize::new(0));
    let processed_groups = Arc::new(AtomicUsize::new(0));
    let start_time = Instant::now();
    let mut batch_handles = Vec::new();
    let max_concurrent_batches = config.concurrency_limit.max(1);
    let page_limit = config.batch_size.clamp(1, 50) as i32;

    let mut next_token: Option<String> = None;
    let mut total_log_groups: usize = 0;
    let mut page_count: usize = 0;

    loop {
        trace!("Describing log groups (next_token={next_token:?})");

        let output = client
            .describe_log_groups()
            .limit(page_limit)
            .set_next_token(next_token)
            .send()
            .await
            .context("Failed to describe log groups")?;

        page_count += 1;

        let mut batch = output.log_groups.unwrap_or_default();
        let before_filter = batch.len();
        batch.retain(|g| should_process_log_group(g, &config));
        let filtered_out = before_filter - batch.len();

        debug!(
            "Described log groups page {page_count}: {} kept, {filtered_out} filtered",
            batch.len()
        );

        if !batch.is_empty() {
            total_log_groups += batch.len();

            let handle = tokio::spawn({
                let client = client.clone();
                let config = Arc::clone(&config);
                let processed_counter = processed_counter.clone();
                let total_streams = total_streams.clone();
                let processed_groups = processed_groups.clone();

                async move {
                    for log_group in batch {
                        let log_group_name =
                            log_group.log_group_name().unwrap_or("unknown").to_string();
                        let group_num = processed_groups.fetch_add(1, Ordering::Relaxed) + 1;

                        debug!("Processing log group {log_group_name} (group #{group_num})");

                        if let Err(e) = gc_log_group(
                            &client,
                            log_group,
                            &config,
                            dry_run,
                            processed_counter.clone(),
                            total_streams.clone(),
                        )
                        .await
                        {
                            warn!("Failed to process log group {log_group_name}: {e}");
                        }

                        task::yield_now().await;
                    }
                }
            });
            batch_handles.push(handle);

            if batch_handles.len() >= max_concurrent_batches {
                let (completed, _, remaining) = futures::future::select_all(batch_handles).await;
                if let Err(e) = completed {
                    warn!("Batch processing task failed: {e}");
                }
                batch_handles = remaining;
            }
        }

        next_token = output.next_token;
        if next_token.is_none() {
            break;
        }
    }

    info!("Found {total_log_groups} log group(s) after filtering");

    if total_log_groups == 0 {
        info!("No log groups found matching the specified criteria");
        return Ok(());
    }

    debug!(
        "Waiting for {} batch processing tasks to complete",
        batch_handles.len()
    );

    for handle in batch_handles {
        if let Err(e) = handle.await {
            warn!("Batch processing task failed: {e}");
        }
    }

    let total_processed = processed_counter.load(Ordering::Relaxed);
    let total_stream_count = total_streams.load(Ordering::Relaxed);
    let elapsed = start_time.elapsed();

    info!(
        "Garbage collection completed: processed {}/{} log streams across {} log groups in {:.2}s ({:.1} streams/sec)",
        total_processed,
        total_stream_count,
        total_log_groups,
        elapsed.as_secs_f64(),
        if elapsed.as_secs_f64() > 0.0 {
            total_processed as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        }
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_log_group(name: &str, retention: Option<i32>) -> LogGroup {
        let mut builder = LogGroup::builder().log_group_name(name);
        if let Some(r) = retention {
            builder = builder.retention_in_days(r);
        }
        builder.build()
    }

    #[test]
    fn parse_timestamp_valid() {
        // 2024-01-01T00:00:00Z = 1_704_067_200 seconds = 1_704_067_200_000 ms
        let dt = parse_timestamp(1_704_067_200_000).unwrap();
        assert_eq!(dt.timestamp(), 1_704_067_200);
        assert_eq!(dt.date_naive().to_string(), "2024-01-01");
    }

    #[test]
    fn parse_timestamp_zero() {
        let dt = parse_timestamp(0).unwrap();
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn parse_timestamp_negative() {
        assert!(parse_timestamp(-1).is_err());
    }

    #[test]
    fn parse_timestamp_preserves_millis() {
        let dt = parse_timestamp(1_704_067_200_123).unwrap();
        assert_eq!(dt.timestamp_subsec_millis(), 123);
    }

    #[test]
    fn filter_skips_log_group_without_retention() {
        let group = make_log_group("foo", None);
        assert!(!should_process_log_group(&group, &Config::default()));
    }

    #[test]
    fn filter_skips_log_group_with_zero_retention() {
        let group = make_log_group("foo", Some(0));
        assert!(!should_process_log_group(&group, &Config::default()));
    }

    #[test]
    fn filter_skips_log_group_with_negative_retention() {
        let group = make_log_group("foo", Some(-1));
        assert!(!should_process_log_group(&group, &Config::default()));
    }

    #[test]
    fn filter_keeps_log_group_with_positive_retention() {
        let group = make_log_group("foo", Some(7));
        assert!(should_process_log_group(&group, &Config::default()));
    }

    #[test]
    fn filter_applies_include_pattern() {
        let group = make_log_group("/aws/lambda/foo", Some(7));

        let mut config = Config::default();
        config.include_pattern = Some(Regex::new(r"^/aws/lambda/").unwrap());
        assert!(should_process_log_group(&group, &config));

        config.include_pattern = Some(Regex::new(r"^/aws/ecs/").unwrap());
        assert!(!should_process_log_group(&group, &config));
    }

    #[test]
    fn filter_applies_exclude_pattern() {
        let group = make_log_group("/aws/lambda/foo", Some(7));

        let mut config = Config::default();
        config.exclude_pattern = Some(Regex::new(r"^/aws/lambda/").unwrap());
        assert!(!should_process_log_group(&group, &config));
    }

    #[test]
    fn filter_exclude_overrides_include() {
        let group = make_log_group("/aws/lambda/foo", Some(7));

        let mut config = Config::default();
        config.include_pattern = Some(Regex::new(r"^/aws/").unwrap());
        config.exclude_pattern = Some(Regex::new(r"foo$").unwrap());
        assert!(!should_process_log_group(&group, &config));
    }
}
