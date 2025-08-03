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

async fn describe_log_groups_batched(
    client: &Client,
    config: &Config,
    mut callback: impl FnMut(Vec<LogGroup>) -> Result<()>,
) -> Result<usize> {
    let mut next_token = None;
    let mut total_processed = 0;
    let mut batch_count = 0;

    loop {
        trace!("Describing log groups (next_token={next_token:?})");

        let describe_output = client
            .describe_log_groups()
            .set_next_token(next_token)
            .send()
            .await
            .context("Failed to describe log groups")?;

        batch_count += 1;
        debug!("Described log groups batch {}", batch_count);

        let mut batch_groups = describe_output.log_groups.unwrap_or_default();

        // Apply filtering
        batch_groups.retain(|group| {
            if let Some(name) = group.log_group_name() {
                let include_match = config
                    .include_pattern
                    .as_ref()
                    .map_or(true, |pattern| pattern.is_match(name));

                let exclude_match = config
                    .exclude_pattern
                    .as_ref()
                    .map_or(false, |pattern| pattern.is_match(name));

                include_match && !exclude_match
            } else {
                false
            }
        });

        if !batch_groups.is_empty() {
            total_processed += batch_groups.len();
            callback(batch_groups)?;
        }

        next_token = describe_output.next_token;
        if next_token.is_none() {
            info!("Found {} log group(s) after filtering", total_processed);
            break Ok(total_processed);
        }
    }
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
                format!(
                    "Failed to describe log streams for log group: {}",
                    log_group_name
                )
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
            format!(
                "Failed to parse creation time for log stream: {}",
                log_stream_name
            )
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
                    format!(
                        "Failed to delete log stream: {}/{}",
                        log_group_name, log_stream_name
                    )
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

    for attempt in 0..=MAX_RETRIES {
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
                if attempt == MAX_RETRIES {
                    return Err(anyhow::Error::from(err));
                }

                // Check if it's a throttling error that we should retry
                let error_message = err.to_string().to_lowercase();
                if error_message.contains("throttl") || error_message.contains("rate") {
                    let delay_ms = BASE_DELAY_MS * 2_u64.pow(attempt);
                    warn!(
                        "Throttling detected for {}/{}, retrying in {}ms (attempt {}/{})",
                        log_group_name,
                        log_stream_name,
                        delay_ms,
                        attempt + 1,
                        MAX_RETRIES + 1
                    );
                    sleep(TokioDuration::from_millis(delay_ms)).await;
                } else {
                    // For non-throttling errors, fail immediately
                    return Err(anyhow::Error::from(err));
                }
            }
        }
    }

    unreachable!("Loop should have returned by now")
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

    let log_group_retention_period: i64 = log_group
        .retention_in_days()
        .ok_or_else(|| anyhow!("Log group {} is missing a retention period", log_group_name))?
        .into();

    if log_group_retention_period <= 0 {
        return Err(anyhow!(
            "Invalid retention period {} for log group {}",
            log_group_retention_period,
            log_group_name
        ));
    }

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

    let start_time = Instant::now();
    let stream_futures = stream::iter(log_streams.into_iter().enumerate())
        .map(|(idx, log_stream)| {
            let client = client.clone();
            let keep_from_date = keep_from_date;
            let log_group_name = log_group_name.clone();
            let processed_counter = processed_counter.clone();
            let config = config.clone();

            async move {
                let result = gc_log_stream(
                    &client,
                    &keep_from_date,
                    &log_group_name,
                    log_stream,
                    dry_run,
                ).await;

                let processed = processed_counter.fetch_add(1, Ordering::Relaxed) + 1;

                if log_stream_ct > config.progress_threshold && (idx + 1) % config.progress_interval == 0 {
                    let elapsed = start_time.elapsed();
                    let rate = processed as f64 / elapsed.as_secs_f64();
                    info!(
                        "Processed {}/{log_stream_ct} log streams in {log_group_name} ({:.1} streams/sec)",
                        idx + 1, rate
                    );
                }

                result
            }
        })
        .buffer_unordered(config.concurrency_limit);

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

pub async fn gc_log_streams(dry_run: bool) -> Result<()> {
    let config = Config::default();
    inner_gc_log_streams(None, &config, dry_run).await
}

pub async fn gc_log_streams_in_region(region: String, dry_run: bool) -> Result<()> {
    let config = Config::default();
    inner_gc_log_streams(Some(region), &config, dry_run).await
}

pub async fn gc_log_streams_with_config(
    region: Option<String>,
    config: &Config,
    dry_run: bool,
) -> Result<()> {
    inner_gc_log_streams(region, config, dry_run).await
}

async fn inner_gc_log_streams(
    region: Option<String>,
    config: &Config,
    dry_run: bool,
) -> Result<()> {
    let mut aws_config = ConfigLoader::default();
    if let Some(region) = region {
        aws_config = aws_config.region(Region::new(region));
    }

    // Use standard retry config now that we handle throttling properly in delete_log_stream_with_retry
    let aws_config = aws_config
        .retry_config(RetryConfig::standard())
        .load()
        .await;

    let client = Client::new(&aws_config);

    let processed_counter = Arc::new(AtomicUsize::new(0));
    let total_streams = Arc::new(AtomicUsize::new(0));
    let processed_groups = Arc::new(AtomicUsize::new(0));
    let start_time = Instant::now();
    let mut batch_handles = Vec::new();
    let max_concurrent_batches = config.concurrency_limit.max(1);

    // Process log groups in batches to reduce memory usage
    let total_log_groups = describe_log_groups_batched(&client, config, |batch| {
        let client = client.clone();
        let config = config.clone();
        let processed_counter = processed_counter.clone();
        let total_streams = total_streams.clone();
        let processed_groups = processed_groups.clone();

        // Process batch in chunks based on config.batch_size
        let batch_size = config.batch_size.min(batch.len()).max(1);
        for chunk in batch.chunks(batch_size) {
            let chunk = chunk.to_vec();
            let handle = tokio::spawn({
                let client = client.clone();
                let config = config.clone();
                let processed_counter = processed_counter.clone();
                let total_streams = total_streams.clone();
                let processed_groups = processed_groups.clone();

                async move {
                    for log_group in chunk {
                        let log_group_name =
                            log_group.log_group_name().unwrap_or("unknown").to_string();
                        let group_num = processed_groups.fetch_add(1, Ordering::Relaxed) + 1;

                        debug!(
                            "Processing log group {} (group #{})",
                            log_group_name, group_num
                        );

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
                            warn!("Failed to process log group {}: {}", log_group_name, e);
                        }

                        // Yield to allow other tasks to run
                        task::yield_now().await;
                    }
                }
            });
            batch_handles.push(handle);

            // Apply backpressure: if we have too many concurrent batches, wait for some to complete
            if batch_handles.len() >= max_concurrent_batches {
                let (completed, _, remaining) = futures::future::select_all(batch_handles).await;
                if let Err(e) = completed {
                    warn!("Batch processing task failed: {}", e);
                }
                batch_handles = remaining;
            }
        }

        Ok(())
    })
    .await?;

    if total_log_groups == 0 {
        info!("No log groups found matching the specified criteria");
        return Ok(());
    }

    debug!(
        "Waiting for {} batch processing tasks to complete",
        batch_handles.len()
    );

    // Wait for all batch processing tasks to complete
    for handle in batch_handles {
        if let Err(e) = handle.await {
            warn!("Batch processing task failed: {}", e);
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
