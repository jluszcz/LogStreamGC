use anyhow::{Context, Result, anyhow};
use aws_config::ConfigLoader;
use aws_config::retry::RetryConfig;
use aws_sdk_cloudwatchlogs::Client;
use aws_sdk_cloudwatchlogs::config::Region;
use aws_sdk_cloudwatchlogs::types::{LogGroup, LogStream};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use futures::stream::{self, FuturesUnordered, StreamExt};
use log::{debug, info, warn};
use regex::Regex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::task::JoinError;

pub const APP_NAME: &str = "log_stream_gc";

/// Throttling and transient errors are handled by the SDK's standard retry
/// (exponential backoff with jitter) rather than a hand-rolled retry loop.
const RETRY_MAX_ATTEMPTS: u32 = 10;

/// `DescribeLogGroups` rejects a `limit` outside 1-50.
const MAX_DESCRIBE_PAGE_SIZE: usize = 50;

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

impl Config {
    /// Clamps every field to the range the run actually depends on. `Config` is
    /// public with public fields, so a library caller can otherwise hand over a
    /// zero that divides by zero or a page size the API rejects.
    fn normalize(&mut self) {
        self.concurrency_limit = self.concurrency_limit.max(1);
        self.progress_interval = self.progress_interval.max(1);
        self.batch_size = self.batch_size.clamp(1, MAX_DESCRIBE_PAGE_SIZE);
    }
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

/// Attributes a panicked batch task to every log group it never got to, rather
/// than to a single group. Returns the number of groups charged.
fn record_batch_failure(failed_groups: &AtomicUsize, unprocessed_groups: &AtomicUsize) -> usize {
    let unprocessed = unprocessed_groups.load(Ordering::Relaxed);
    failed_groups.fetch_add(unprocessed, Ordering::Relaxed);
    unprocessed
}

fn join_batch(ctx: &GcContext, result: Result<(), JoinError>, unprocessed_groups: &AtomicUsize) {
    if let Err(e) = result {
        let unprocessed = record_batch_failure(&ctx.failed_groups, unprocessed_groups);
        warn!("Batch processing task failed with {unprocessed} log group(s) unprocessed: {e}");
    }
}

/// The date a log stream was last written to, which is what decides whether it
/// still holds unexpired data. `last_event_timestamp` is absent for a stream
/// that never received an event, so creation time is the fallback — deleting on
/// creation time alone destroys recent events in long-lived active streams.
fn last_activity_date(log_stream: &LogStream) -> Result<NaiveDate> {
    let timestamp = log_stream
        .last_event_timestamp()
        .or_else(|| log_stream.creation_time())
        .ok_or_else(|| anyhow!("Log stream has neither a last event time nor a creation time"))?;

    Ok(parse_timestamp(timestamp)?.date_naive())
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

    let log_stream_activity_date = last_activity_date(&log_stream).with_context(|| {
        format!("Failed to determine last activity for log stream: {log_stream_name}")
    })?;

    if log_stream_activity_date < *keep_from_date {
        debug!(
            "{} {log_group_name}/{log_stream_name} (last activity {log_stream_activity_date} < {keep_from_date})",
            if ctx.dry_run {
                "Would delete (Dry-Run)"
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
            "Keeping {log_group_name}/{log_stream_name} (last activity {log_stream_activity_date} >= {keep_from_date})"
        );
    }

    Ok(())
}

/// The oldest date whose data is still worth keeping: `today` minus the log
/// group's retention period scaled by the configured multiplier. Streams whose
/// last activity predates this are expendable.
fn keep_from_date(
    retention_period: i32,
    retention_multiplier: f64,
    today: NaiveDate,
) -> Result<NaiveDate> {
    let retention_days = (retention_period as f64 * retention_multiplier) as i64;

    let retention = Duration::try_days(retention_days)
        .ok_or_else(|| anyhow!("Failed to create duration for {retention_days} days"))?;

    today
        .checked_sub_signed(retention)
        .ok_or_else(|| anyhow!("Cutoff date is out of range for {retention_days} days"))
}

async fn gc_log_group(ctx: Arc<GcContext>, log_group: LogGroup) -> Result<()> {
    let log_group_name = log_group
        .log_group_name()
        .ok_or_else(|| anyhow!("Log group is missing a name"))?
        .to_string();

    // Invariant: callers filter via `should_process_log_group`, which guarantees positive retention.
    let log_group_retention_period = log_group
        .retention_in_days()
        .expect("log group passed should_process_log_group filter");

    let keep_from_date = keep_from_date(
        log_group_retention_period,
        ctx.config.retention_multiplier,
        Utc::now().date_naive(),
    )
    .with_context(|| format!("Failed to compute a cutoff date for {log_group_name}"))?;

    debug!(
        "Cleaning up {log_group_name} from before {keep_from_date} (retention: {log_group_retention_period}d * {})",
        ctx.config.retention_multiplier
    );

    let group_start = Instant::now();

    let mut pages = ctx
        .client
        .describe_log_streams()
        .log_group_name(&log_group_name)
        .into_paginator()
        .send();

    let mut log_stream_ct = 0;
    let mut error_ct = 0;
    let mut first_error = None;
    let mut next_progress_at = ctx.config.progress_interval;

    // Each page is processed as it arrives, so memory is bounded by the page
    // size rather than by the number of streams in the group, and the first
    // delete does not wait on the last page.
    while let Some(page) = pages.next().await {
        let page = page.with_context(|| {
            format!("Failed to describe log streams for log group: {log_group_name}")
        })?;

        let log_streams = page.log_streams.unwrap_or_default();
        if log_streams.is_empty() {
            continue;
        }

        let page_stream_ct = log_streams.len();
        log_stream_ct += page_stream_ct;
        ctx.total_streams
            .fetch_add(page_stream_ct, Ordering::Relaxed);

        let results: Vec<_> = stream::iter(log_streams)
            .map(|log_stream| gc_log_stream(&ctx, &keep_from_date, &log_group_name, log_stream))
            .buffer_unordered(ctx.config.concurrency_limit)
            .collect()
            .await;

        for error in results.into_iter().filter_map(Result::err) {
            error_ct += 1;
            first_error.get_or_insert(error);
        }

        let processed = ctx
            .processed_streams
            .fetch_add(page_stream_ct, Ordering::Relaxed)
            + page_stream_ct;

        if log_stream_ct > ctx.config.progress_threshold && log_stream_ct >= next_progress_at {
            next_progress_at = log_stream_ct + ctx.config.progress_interval;
            let rate = processed as f64 / ctx.start_time.elapsed().as_secs_f64();
            info!(
                "Processed {log_stream_ct} log stream(s) in {log_group_name} ({rate:.1} streams/sec overall)"
            );
        }
    }

    if let Some(first_error) = first_error {
        return Err(anyhow!(
            "Failed to process {error_ct} log streams in {log_group_name}: {first_error}"
        ));
    }

    debug!(
        "Completed processing {log_stream_ct} log stream(s) in {log_group_name} in {:.2}s",
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

    // Clamp once so every use of a limit (semaphore, buffer_unordered, batch cap,
    // progress modulus) agrees and stays in range.
    config.normalize();
    let concurrency_limit = config.concurrency_limit;
    let page_limit = config.batch_size as i32;

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

    let mut batches = FuturesUnordered::new();
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

        // Counts the groups this batch has not finished yet, so a panicked task
        // can be charged for exactly the groups it never processed.
        let unprocessed_groups = Arc::new(AtomicUsize::new(batch.len()));

        let handle = tokio::spawn({
            let ctx = Arc::clone(&ctx);
            let unprocessed_groups = Arc::clone(&unprocessed_groups);

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

                    unprocessed_groups.fetch_sub(1, Ordering::Relaxed);
                }
            }
        });
        batches.push(async move { (handle.await, unprocessed_groups) });

        if batches.len() >= concurrency_limit
            && let Some((result, unprocessed_groups)) = batches.next().await
        {
            join_batch(&ctx, result, &unprocessed_groups);
        }
    }

    info!("Found {total_log_groups} log group(s) after filtering");

    debug!(
        "Waiting for {} batch processing tasks to complete",
        batches.len()
    );

    while let Some((result, unprocessed_groups)) = batches.next().await {
        join_batch(&ctx, result, &unprocessed_groups);
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

    fn make_log_stream(creation: Option<i64>, last_event: Option<i64>) -> LogStream {
        let mut builder = LogStream::builder().log_stream_name("stream");
        if let Some(c) = creation {
            builder = builder.creation_time(c);
        }
        if let Some(e) = last_event {
            builder = builder.last_event_timestamp(e);
        }
        builder.build()
    }

    /// 2024-01-01T00:00:00Z
    const CREATED_MS: i64 = 1_704_067_200_000;
    /// 2024-06-01T00:00:00Z
    const LAST_EVENT_MS: i64 = 1_717_200_000_000;

    #[test]
    fn activity_date_prefers_last_event_over_creation() {
        let stream = make_log_stream(Some(CREATED_MS), Some(LAST_EVENT_MS));
        assert_eq!(
            last_activity_date(&stream).unwrap().to_string(),
            "2024-06-01"
        );
    }

    #[test]
    fn activity_date_falls_back_to_creation_when_stream_has_no_events() {
        let stream = make_log_stream(Some(CREATED_MS), None);
        assert_eq!(
            last_activity_date(&stream).unwrap().to_string(),
            "2024-01-01"
        );
    }

    #[test]
    fn activity_date_errors_when_stream_has_no_timestamps() {
        let stream = make_log_stream(None, None);
        assert!(last_activity_date(&stream).is_err());
    }

    #[test]
    fn activity_date_errors_on_invalid_last_event_timestamp() {
        let stream = make_log_stream(Some(CREATED_MS), Some(-1));
        assert!(last_activity_date(&stream).is_err());
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
    fn a_failed_batch_counts_every_group_it_did_not_process() {
        let failed_groups = AtomicUsize::new(1);
        let unprocessed = AtomicUsize::new(3);

        assert_eq!(record_batch_failure(&failed_groups, &unprocessed), 3);
        assert_eq!(failed_groups.load(Ordering::Relaxed), 4);
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn keep_from_date_applies_the_retention_multiplier() {
        assert_eq!(
            keep_from_date(7, 2.0, date("2024-06-15")).unwrap(),
            date("2024-06-01")
        );
    }

    #[test]
    fn keep_from_date_truncates_fractional_days() {
        // 7 * 1.5 = 10.5 days, truncated to 10
        assert_eq!(
            keep_from_date(7, 1.5, date("2024-06-15")).unwrap(),
            date("2024-06-05")
        );
    }

    #[test]
    fn keep_from_date_with_a_tiny_multiplier_keeps_only_today() {
        assert_eq!(
            keep_from_date(7, 0.01, date("2024-06-15")).unwrap(),
            date("2024-06-15")
        );
    }

    #[test]
    fn keep_from_date_errors_when_the_duration_overflows() {
        assert!(keep_from_date(i32::MAX, 1e12, date("2024-06-15")).is_err());
    }

    #[test]
    fn keep_from_date_errors_when_the_date_underflows() {
        assert!(keep_from_date(i32::MAX, 1.0, date("2024-06-15")).is_err());
    }

    #[test]
    fn normalize_clamps_zero_progress_interval() {
        let mut config = Config {
            progress_interval: 0,
            ..Config::default()
        };
        config.normalize();
        assert_eq!(config.progress_interval, 1);
    }

    #[test]
    fn normalize_clamps_zero_concurrency_limit() {
        let mut config = Config {
            concurrency_limit: 0,
            ..Config::default()
        };
        config.normalize();
        assert_eq!(config.concurrency_limit, 1);
    }

    #[test]
    fn normalize_clamps_batch_size_to_the_api_page_limit() {
        let mut config = Config {
            batch_size: 500,
            ..Config::default()
        };
        config.normalize();
        assert_eq!(config.batch_size, 50);

        let mut config = Config {
            batch_size: 0,
            ..Config::default()
        };
        config.normalize();
        assert_eq!(config.batch_size, 1);
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
