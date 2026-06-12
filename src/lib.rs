use anyhow::{Context, Result, anyhow};
use aws_config::ConfigLoader;
use aws_config::retry::RetryConfig;
use aws_sdk_cloudwatchlogs::Client;
use aws_sdk_cloudwatchlogs::config::Region;
use aws_sdk_cloudwatchlogs::types::{LogGroup, LogStream};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use futures::stream::{self, StreamExt};
use log::{debug, info, warn};
use regex::Regex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::Semaphore;

pub const APP_NAME: &str = "log_stream_gc";

/// Throttling and transient errors are handled by the SDK's standard retry
/// (exponential backoff with jitter) rather than a hand-rolled retry loop.
const RETRY_MAX_ATTEMPTS: u32 = 10;

#[derive(Debug, Clone)]
pub struct Config {
    /// Bounds three kinds of parallelism in a run: the global cap on in-flight
    /// log stream deletions (the binding constraint, enforced by a semaphore),
    /// the number of streams examined concurrently within each log group, and
    /// the number of log group batches processed concurrently.
    pub concurrency_limit: usize,
    pub progress_threshold: usize,
    pub progress_interval: usize,
    pub retention_multiplier: f64,
    pub batch_size: usize,
    pub include_pattern: Option<Regex>,
    pub exclude_pattern: Option<Regex>,
}

impl Default for Config {
    // These defaults must match the clap default_value strings in main.rs.
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

/// State shared by every task in a single garbage collection run.
struct GcContext {
    client: Client,
    config: Config,
    dry_run: bool,
    /// Caps in-flight log stream deletions across all log groups.
    delete_semaphore: Semaphore,
    processed_streams: AtomicUsize,
    total_streams: AtomicUsize,
    processed_groups: AtomicUsize,
    failed_groups: AtomicUsize,
    start_time: Instant,
}

fn parse_timestamp(timestamp: i64) -> Result<DateTime<Utc>> {
    if timestamp < 0 {
        return Err(anyhow!("Invalid timestamp: {}", timestamp));
    }

    DateTime::from_timestamp_millis(timestamp)
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
    let mut log_streams = Vec::new();

    let mut pages = client
        .describe_log_streams()
        .log_group_name(log_group_name)
        .into_paginator()
        .send();

    while let Some(page) = pages.next().await {
        let page = page.with_context(|| {
            format!("Failed to describe log streams for log group: {log_group_name}")
        })?;
        log_streams.extend(page.log_streams.unwrap_or_default());
    }

    info!(
        "Found {} log stream(s) for {log_group_name}",
        log_streams.len()
    );
    Ok(log_streams)
}

async fn gc_log_stream(
    ctx: &GcContext,
    keep_from_date: &NaiveDate,
    log_group_name: &str,
    log_stream: LogStream,
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
            if ctx.dry_run {
                "Keeping (Dry-Run)"
            } else {
                "Deleting"
            }
        );

        if !ctx.dry_run {
            let _permit = ctx.delete_semaphore.acquire().await?;
            ctx.client
                .delete_log_stream()
                .log_group_name(log_group_name)
                .log_stream_name(log_stream_name)
                .send()
                .await
                .with_context(|| {
                    format!("Failed to delete log stream: {log_group_name}/{log_stream_name}")
                })?;
            debug!("Deleted {log_group_name}/{log_stream_name}");
        }
    } else {
        debug!(
            "Keeping {log_group_name}/{log_stream_name} (creation date {log_stream_creation_date} >= {keep_from_date})"
        );
    }

    Ok(())
}

async fn gc_log_group(ctx: Arc<GcContext>, log_group: LogGroup) -> Result<()> {
    let log_group_name = log_group
        .log_group_name()
        .ok_or_else(|| anyhow!("Log group is missing a name"))?
        .to_string();

    // Invariant: callers filter via `should_process_log_group`, which guarantees positive retention.
    let log_group_retention_period: i64 = log_group
        .retention_in_days()
        .expect("log group passed should_process_log_group filter")
        .into();

    let retention_days =
        (log_group_retention_period as f64 * ctx.config.retention_multiplier) as i64;
    let keep_from_date = Utc::now().date_naive()
        - Duration::try_days(retention_days)
            .ok_or_else(|| anyhow!("Failed to create duration for {} days", retention_days))?;

    debug!(
        "Cleaning up {log_group_name} from before {keep_from_date} (retention: {}d * {} = {}d)",
        log_group_retention_period, ctx.config.retention_multiplier, retention_days
    );

    let log_streams = describe_log_streams(&ctx.client, &log_group_name).await?;
    let log_stream_ct = log_streams.len();
    ctx.total_streams
        .fetch_add(log_stream_ct, Ordering::Relaxed);

    if log_stream_ct == 0 {
        debug!("No log streams found in {log_group_name}");
        return Ok(());
    }

    let group_start = Instant::now();
    let stream_futures = stream::iter(log_streams.into_iter().enumerate())
        .map(|(idx, log_stream)| {
            let ctx = Arc::clone(&ctx);
            let log_group_name = log_group_name.clone();

            async move {
                let result =
                    gc_log_stream(&ctx, &keep_from_date, &log_group_name, log_stream).await;

                let processed = ctx.processed_streams.fetch_add(1, Ordering::Relaxed) + 1;

                if log_stream_ct > ctx.config.progress_threshold
                    && (idx + 1) % ctx.config.progress_interval == 0
                {
                    let rate = processed as f64 / ctx.start_time.elapsed().as_secs_f64();
                    info!(
                        "Processed {}/{log_stream_ct} log streams in {log_group_name} ({rate:.1} streams/sec overall)",
                        idx + 1
                    );
                }

                result
            }
        })
        .buffer_unordered(ctx.config.concurrency_limit);

    let errors: Vec<_> = stream_futures
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .filter_map(Result::err)
        .collect();

    let error_count = errors.len();
    if let Some(first_error) = errors.into_iter().next() {
        return Err(anyhow!(
            "Failed to process {error_count} log streams in {log_group_name}: {first_error}"
        ));
    }

    debug!(
        "Completed processing {log_stream_ct} log streams in {log_group_name} in {:.2}s",
        group_start.elapsed().as_secs_f64()
    );

    Ok(())
}

pub async fn gc_log_streams(
    region: Option<String>,
    mut config: Config,
    dry_run: bool,
) -> Result<()> {
    let mut aws_config = ConfigLoader::default();
    if let Some(region) = region {
        aws_config = aws_config.region(Region::new(region));
    }

    let aws_config = aws_config
        .retry_config(RetryConfig::standard().with_max_attempts(RETRY_MAX_ATTEMPTS))
        .load()
        .await;

    // Clamp once so every use of the limit (semaphore, buffer_unordered, batch cap) agrees.
    config.concurrency_limit = config.concurrency_limit.max(1);
    let concurrency_limit = config.concurrency_limit;
    let page_limit = config.batch_size.clamp(1, 50) as i32;

    let ctx = Arc::new(GcContext {
        client: Client::new(&aws_config),
        dry_run,
        delete_semaphore: Semaphore::new(concurrency_limit),
        processed_streams: AtomicUsize::new(0),
        total_streams: AtomicUsize::new(0),
        processed_groups: AtomicUsize::new(0),
        failed_groups: AtomicUsize::new(0),
        start_time: Instant::now(),
        config,
    });

    let mut batch_handles = Vec::new();
    let mut total_log_groups: usize = 0;
    let mut page_count: usize = 0;

    let mut pages = ctx
        .client
        .describe_log_groups()
        .limit(page_limit)
        .into_paginator()
        .send();

    while let Some(page) = pages.next().await {
        let output = page.context("Failed to describe log groups")?;
        page_count += 1;

        let mut batch = output.log_groups.unwrap_or_default();
        let before_filter = batch.len();
        batch.retain(|g| should_process_log_group(g, &ctx.config));
        let filtered_out = before_filter - batch.len();

        debug!(
            "Described log groups page {page_count}: {} kept, {filtered_out} filtered",
            batch.len()
        );

        if batch.is_empty() {
            continue;
        }
        total_log_groups += batch.len();

        let handle = tokio::spawn({
            let ctx = Arc::clone(&ctx);

            async move {
                for log_group in batch {
                    let log_group_name =
                        log_group.log_group_name().unwrap_or("unknown").to_string();
                    let group_num = ctx.processed_groups.fetch_add(1, Ordering::Relaxed) + 1;

                    debug!("Processing log group {log_group_name} (group #{group_num})");

                    if let Err(e) = gc_log_group(Arc::clone(&ctx), log_group).await {
                        ctx.failed_groups.fetch_add(1, Ordering::Relaxed);
                        warn!("Failed to process log group {log_group_name}: {e}");
                    }
                }
            }
        });
        batch_handles.push(handle);

        if batch_handles.len() >= concurrency_limit {
            let (completed, _, remaining) = futures::future::select_all(batch_handles).await;
            if let Err(e) = completed {
                ctx.failed_groups.fetch_add(1, Ordering::Relaxed);
                warn!("Batch processing task failed: {e}");
            }
            batch_handles = remaining;
        }
    }

    info!("Found {total_log_groups} log group(s) after filtering");

    debug!(
        "Waiting for {} batch processing tasks to complete",
        batch_handles.len()
    );

    for handle in batch_handles {
        if let Err(e) = handle.await {
            ctx.failed_groups.fetch_add(1, Ordering::Relaxed);
            warn!("Batch processing task failed: {e}");
        }
    }

    let total_processed = ctx.processed_streams.load(Ordering::Relaxed);
    let total_stream_count = ctx.total_streams.load(Ordering::Relaxed);
    let elapsed = ctx.start_time.elapsed();

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

    let failed_groups = ctx.failed_groups.load(Ordering::Relaxed);
    if failed_groups > 0 {
        return Err(anyhow!(
            "Failed to process {failed_groups} log group(s); see warnings above"
        ));
    }

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

        let config = Config {
            include_pattern: Some(Regex::new(r"^/aws/lambda/").unwrap()),
            ..Config::default()
        };
        assert!(should_process_log_group(&group, &config));

        let config = Config {
            include_pattern: Some(Regex::new(r"^/aws/ecs/").unwrap()),
            ..Config::default()
        };
        assert!(!should_process_log_group(&group, &config));
    }

    #[test]
    fn filter_applies_exclude_pattern() {
        let group = make_log_group("/aws/lambda/foo", Some(7));

        let config = Config {
            exclude_pattern: Some(Regex::new(r"^/aws/lambda/").unwrap()),
            ..Config::default()
        };
        assert!(!should_process_log_group(&group, &config));
    }

    #[test]
    fn filter_exclude_overrides_include() {
        let group = make_log_group("/aws/lambda/foo", Some(7));

        let config = Config {
            include_pattern: Some(Regex::new(r"^/aws/").unwrap()),
            exclude_pattern: Some(Regex::new(r"foo$").unwrap()),
            ..Config::default()
        };
        assert!(!should_process_log_group(&group, &config));
    }
}
