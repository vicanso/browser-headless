//! Queue worker mode — a consumer that is fully decoupled from the HTTP layer.
//!
//! It reads capture jobs from a Redis stream (consumer group), runs each on
//! the shared browser pool by calling [`capture::capture_one`], and writes the
//! result back to a per-job `result:{id}` key with a TTL. It never imports
//! `crate::http` and binds no HTTP port — its only dependencies are Redis and
//! the browser pool. Enable with `BROWSER_HEADLESS_MODE=worker`.
//!
//! Reliability: jobs are delivered at-least-once. A job is `XACK`ed only after
//! its result is written, so a worker that crashes mid-capture leaves the entry
//! pending; another worker reclaims it via `XAUTOCLAIM` once it has been idle
//! past the visibility timeout. A capture *error* still produces a result and
//! acks (it's a definitive outcome, not a transient failure).
//!
//! Message shape (one stream field `payload`, JSON):
//! `{ "id": "job-123", "url": "https://…", ...any /summary param… }`
//! (`id` optional — falls back to the Redis stream entry id).
//! Result at `result:{id}`: `{ "id", "status", "data"? , "error"? }`.

use std::time::Duration;

use futures::stream::StreamExt;
use redis::AsyncCommands;
use redis::aio::MultiplexedConnection;
use redis::streams::{
    StreamAutoClaimOptions, StreamAutoClaimReply, StreamId, StreamReadOptions, StreamReadReply,
};
use serde::{Deserialize, Serialize};

use crate::capture::{self, CaptureCtx, Captured, SummaryQuery};

/// Worker settings, resolved once from the environment.
#[derive(Clone)]
struct WorkerConfig {
    redis_url: String,
    stream: String,
    group: String,
    consumer: String,
    result_prefix: String,
    result_ttl_secs: u64,
    /// How long `XREADGROUP` blocks waiting for new jobs before we fall through
    /// to a reclaim pass.
    block_ms: usize,
    /// Min idle time before a pending entry is eligible for `XAUTOCLAIM` (i.e.
    /// the original worker is presumed dead).
    visibility_ms: u64,
}

impl WorkerConfig {
    fn from_env() -> Self {
        Self {
            redis_url: env_string("BROWSER_HEADLESS_REDIS_URL", "redis://127.0.0.1:6379"),
            stream: env_string("BROWSER_HEADLESS_JOBS_STREAM", "browser_headless:jobs"),
            group: env_string("BROWSER_HEADLESS_CONSUMER_GROUP", "workers"),
            consumer: env_string(
                "BROWSER_HEADLESS_CONSUMER_NAME",
                &format!("worker-{}", std::process::id()),
            ),
            result_prefix: env_string("BROWSER_HEADLESS_RESULT_PREFIX", "browser_headless:result:"),
            result_ttl_secs: env_u64("BROWSER_HEADLESS_RESULT_TTL_SECS", 3600).max(1),
            block_ms: env_u64("BROWSER_HEADLESS_JOB_BLOCK_MS", 5000).max(1) as usize,
            visibility_ms: env_u64("BROWSER_HEADLESS_JOB_VISIBILITY_MS", 120_000).max(1),
        }
    }
}

fn env_string(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Incoming job: an optional `id` plus the full capture params (the same
/// `SummaryQuery` the HTTP endpoint accepts, flattened).
#[derive(Deserialize)]
struct WorkerJob {
    #[serde(default)]
    id: Option<String>,
    #[serde(flatten)]
    query: SummaryQuery,
}

/// Result written to `result:{id}`: 200 + `data` on success, the error status
/// + `error` on failure (mirrors the `/summary/batch` per-item shape).
#[derive(Serialize)]
struct JobResult {
    id: String,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl JobResult {
    fn ok(id: String, data: Result<serde_json::Value, serde_json::Error>) -> Self {
        match data {
            Ok(value) => JobResult {
                id,
                status: 200,
                data: Some(value),
                error: None,
            },
            Err(e) => JobResult {
                id,
                status: 500,
                data: None,
                error: Some(format!("serialize result: {e}")),
            },
        }
    }

    fn err(id: String, status: u16, error: String) -> Self {
        JobResult {
            id,
            status,
            data: None,
            error: Some(error),
        }
    }
}

/// Run the worker loop forever (until the process is signalled). Shares the
/// browser pool via `ctx`; reads its own Redis config from the environment.
pub(crate) async fn run(ctx: CaptureCtx) {
    let cfg = WorkerConfig::from_env();
    let capacity = ctx.pool.capacity().max(1);
    tracing::info!(
        redis_url = %cfg.redis_url,
        stream = %cfg.stream,
        group = %cfg.group,
        consumer = %cfg.consumer,
        concurrency = capacity,
        "worker mode starting"
    );

    let client =
        redis::Client::open(cfg.redis_url.clone()).expect("invalid BROWSER_HEADLESS_REDIS_URL");

    ensure_group(&client, &cfg).await;

    // Separate connections: `read_conn` owns the blocking XREADGROUP; the
    // cloned `write_conn` (multiplexed) handles concurrent result writes + acks
    // without being stalled behind the blocking read.
    let mut read_conn = connect(&client).await;
    let write_conn = connect(&client).await;

    let mut backoff = Duration::from_secs(1);
    loop {
        match poll_new(&mut read_conn, &cfg, capacity).await {
            Ok(entries) => {
                backoff = Duration::from_secs(1);
                if entries.is_empty() {
                    // No new jobs this cycle — try to reclaim entries abandoned
                    // by a crashed worker before looping back to the blocking read.
                    match reclaim(&mut read_conn, &cfg, capacity).await {
                        Ok(reclaimed) if !reclaimed.is_empty() => {
                            tracing::info!(count = reclaimed.len(), "reclaimed stale jobs");
                            process(&ctx, &cfg, &write_conn, reclaimed).await;
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "XAUTOCLAIM failed"),
                    }
                    continue;
                }
                process(&ctx, &cfg, &write_conn, entries).await;
            }
            Err(e) => {
                tracing::error!(error = %e, retry_in = ?backoff, "redis read failed; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                read_conn = connect(&client).await;
            }
        }
    }
}

/// Create the consumer group (and the stream, via `MKSTREAM`) if absent.
/// Retries until Redis is reachable; a pre-existing group (`BUSYGROUP`) is fine.
async fn ensure_group(client: &redis::Client, cfg: &WorkerConfig) {
    loop {
        match client.get_multiplexed_async_connection().await {
            Ok(mut conn) => {
                let res: redis::RedisResult<()> = conn
                    .xgroup_create_mkstream(cfg.stream.as_str(), cfg.group.as_str(), "$")
                    .await;
                match res {
                    Ok(()) => {
                        tracing::info!(group = %cfg.group, "created consumer group");
                        return;
                    }
                    Err(e) if e.code() == Some("BUSYGROUP") => return,
                    Err(e) => tracing::error!(error = %e, "XGROUP CREATE failed; retrying"),
                }
            }
            Err(e) => tracing::error!(error = %e, "redis connect failed; retrying"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Open a multiplexed async connection, retrying until Redis is reachable.
async fn connect(client: &redis::Client) -> MultiplexedConnection {
    loop {
        match client.get_multiplexed_async_connection().await {
            Ok(conn) => return conn,
            Err(e) => {
                tracing::error!(error = %e, "redis connect failed; retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Block up to `block_ms` for up to `count` brand-new jobs (`>` cursor).
async fn poll_new(
    conn: &mut MultiplexedConnection,
    cfg: &WorkerConfig,
    count: usize,
) -> redis::RedisResult<Vec<StreamId>> {
    let opts = StreamReadOptions::default()
        .group(&cfg.group, &cfg.consumer)
        .count(count)
        .block(cfg.block_ms);
    let reply: StreamReadReply = conn
        .xread_options(&[cfg.stream.as_str()], &[">"], &opts)
        .await?;
    Ok(reply.keys.into_iter().flat_map(|k| k.ids).collect())
}

/// Claim up to `count` pending entries idle longer than the visibility timeout
/// (their original consumer is presumed dead).
async fn reclaim(
    conn: &mut MultiplexedConnection,
    cfg: &WorkerConfig,
    count: usize,
) -> redis::RedisResult<Vec<StreamId>> {
    let opts = StreamAutoClaimOptions::default().count(count);
    let reply: StreamAutoClaimReply = conn
        .xautoclaim_options(
            cfg.stream.as_str(),
            cfg.group.as_str(),
            cfg.consumer.as_str(),
            cfg.visibility_ms,
            "0",
            opts,
        )
        .await?;
    Ok(reply.claimed)
}

/// Capture each entry concurrently (bounded by pool capacity), write its
/// result, then ack. A failed capture still writes a result + acks; a failed
/// Redis write leaves the entry pending for later reclaim.
async fn process(
    ctx: &CaptureCtx,
    cfg: &WorkerConfig,
    write_conn: &MultiplexedConnection,
    entries: Vec<StreamId>,
) {
    let capacity = ctx.pool.capacity().max(1);
    futures::stream::iter(entries)
        .for_each_concurrent(capacity, |entry| {
            let ctx = ctx.clone();
            let cfg = cfg.clone();
            let mut conn = write_conn.clone();
            async move {
                let entry_id = entry.id.clone();
                let Some(payload) = entry.get::<String>("payload") else {
                    tracing::warn!(entry = %entry_id, "job missing `payload` field; dropping");
                    ack(&mut conn, &cfg, &entry_id).await;
                    return;
                };
                let job: WorkerJob = match serde_json::from_str(&payload) {
                    Ok(job) => job,
                    Err(e) => {
                        tracing::warn!(entry = %entry_id, error = %e, "malformed job; dropping");
                        ack(&mut conn, &cfg, &entry_id).await;
                        return;
                    }
                };
                let id = job.id.unwrap_or_else(|| entry_id.clone());
                let result = capture_to_result(&ctx, id, job.query).await;

                let json = match serde_json::to_string(&result) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::error!(entry = %entry_id, error = %e, "result serialize failed");
                        ack(&mut conn, &cfg, &entry_id).await;
                        return;
                    }
                };
                let key = format!("{}{}", cfg.result_prefix, result.id);
                let write: redis::RedisResult<()> =
                    conn.set_ex(&key, &json, cfg.result_ttl_secs).await;
                if let Err(e) = write {
                    // Don't ack — the entry stays pending and gets reclaimed,
                    // so the result isn't silently lost.
                    tracing::warn!(entry = %entry_id, error = %e, "result write failed; leaving entry pending");
                    return;
                }
                tracing::info!(entry = %entry_id, id = %result.id, status = result.status, "job done");
                ack(&mut conn, &cfg, &entry_id).await;
            }
        })
        .await;
}

/// Run one capture and shape it into a `JobResult`.
async fn capture_to_result(ctx: &CaptureCtx, id: String, query: SummaryQuery) -> JobResult {
    match capture::capture_one(ctx, query).await {
        Ok(Captured::Content(content)) => JobResult::ok(id, serde_json::to_value(content)),
        Ok(Captured::Full(stat)) => JobResult::ok(id, serde_json::to_value(stat)),
        Err((code, msg)) => JobResult::err(id, code.as_u16(), msg),
    }
}

async fn ack(conn: &mut MultiplexedConnection, cfg: &WorkerConfig, entry_id: &str) {
    let acked: redis::RedisResult<i64> = conn
        .xack(cfg.stream.as_str(), cfg.group.as_str(), &[entry_id])
        .await;
    if let Err(e) = acked {
        tracing::warn!(entry = %entry_id, error = %e, "XACK failed");
    }
}
