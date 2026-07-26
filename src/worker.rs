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

use std::sync::Arc;
use std::time::{Duration, Instant};

use redis::AsyncCommands;
use redis::streams::{
    StreamAutoClaimOptions, StreamAutoClaimReply, StreamId, StreamPendingReply, StreamReadOptions,
    StreamReadReply,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::capture::{self, CaptureCtx, Captured, SummaryQuery};
use crate::config::{env_bool, env_opt, env_string, env_u64};
use crate::redis_conn::{Backend, Conn, RedisConnOpts};

const M_WORKER_JOBS: &str = "browser_headless_worker_jobs_total";
const M_WORKER_JOB_DURATION: &str = "browser_headless_worker_job_duration_seconds";
const M_WORKER_RECLAIMED: &str = "browser_headless_worker_reclaimed_total";
const M_WORKER_RETRIES: &str = "browser_headless_worker_retries_total";
const M_WORKER_INFLIGHT: &str = "browser_headless_worker_jobs_in_flight";
const M_WORKER_STREAM_LEN: &str = "browser_headless_worker_stream_length";
const M_WORKER_PENDING: &str = "browser_headless_worker_pending";

/// Worker settings, resolved once from the environment.
#[derive(Clone)]
struct WorkerConfig {
    /// Single-node URL (`redis://…`), or comma-separated seed nodes when
    /// `cluster` is true.
    redis_url: String,
    /// Treat `redis_url` as Redis Cluster seed nodes
    /// (`BROWSER_HEADLESS_REDIS_CLUSTER`).
    cluster: bool,
    /// Path to a PEM CA certificate to verify the server (private CA). When
    /// `None`, the system trust store is used. Use a `rediss://` URL for TLS.
    ca_cert_path: Option<String>,
    /// Paths to a PEM client certificate + key for mutual TLS. Both must be set
    /// together (or neither).
    client_cert_path: Option<String>,
    client_key_path: Option<String>,
    stream: String,
    group: String,
    /// Start position when the consumer group is FIRST created
    /// (`BROWSER_HEADLESS_GROUP_START`): `0` consumes everything already in the
    /// stream (backlog enqueued before the worker started), `$` only consumes
    /// messages added after creation. Only affects the first creation — once
    /// the group exists it keeps its own position across restarts.
    group_start: String,
    consumer: String,
    result_prefix: String,
    result_ttl_secs: u64,
    /// Delete each entry from the stream once it's processed + acked (XACK only
    /// clears the pending list, so the stream would otherwise grow forever).
    /// Default on; set `BROWSER_HEADLESS_DELETE_ON_ACK=false` to retain entries
    /// (e.g. for replay) and cap the stream with MAXLEN yourself.
    delete_on_ack: bool,
    /// How long `XREADGROUP` blocks waiting for new jobs before we fall through
    /// to a reclaim pass (`BROWSER_HEADLESS_JOB_BLOCK_MS`, default 60_000).
    ///
    /// A long window does NOT delay job pickup — a blocking read returns the
    /// moment a new entry lands. It only sets the idle-churn rate: every
    /// timeout costs one `XREADGROUP` + one `XAUTOCLAIM`, which matters on
    /// command-count-limited cloud Redis (e.g. Upstash free tier). Page
    /// detection is not a real-time workload — minute-level idle cadence is
    /// plenty. The only trade-off is reclaim latency for crashed-worker
    /// entries (worst case ~1 block window on top of `visibility_ms`).
    block_ms: usize,
    /// Min idle time before a pending entry is eligible for `XAUTOCLAIM` (i.e.
    /// the original worker is presumed dead).
    visibility_ms: u64,
    /// TCP + TLS + auth handshake timeout when opening a connection
    /// (`BROWSER_HEADLESS_REDIS_CONNECT_TIMEOUT_MS`, default 5000). redis-rs's
    /// own default is only 1s, too tight for a high-latency / cross-region
    /// link (e.g. Upstash), which surfaces as `timed out` on connect.
    connect_timeout_ms: u64,
    /// How often the background sampler refreshes the queue-depth gauges
    /// (stream length / pending) via `XLEN` + `XPENDING`
    /// (`BROWSER_HEADLESS_METRICS_SAMPLE_SECS`, default 300). Each sample is
    /// 2 Redis commands, billed on command-count-limited cloud Redis; queue
    /// depth for a non-real-time detection workload doesn't need finer than
    /// 5-minute resolution. In-flight is tracked per-job, not sampled.
    metrics_sample_secs: u64,
    /// Publish each completed result to a Redis pub/sub channel named after the
    /// result key (`<result_prefix><id>`), carrying the same JSON, so clients
    /// can block on `SUBSCRIBE` instead of polling `GET`
    /// (`BROWSER_HEADLESS_RESULT_NOTIFY`, default true). The key is still
    /// written, so pollers and late subscribers keep working.
    result_notify: bool,
    /// Max times to retry a job whose capture returned a *transient* failure
    /// (408 / 502 / 503 / 504) before writing a terminal error result
    /// (`BROWSER_HEADLESS_JOB_MAX_RETRIES`, default 2). Definitive failures
    /// (400 / 401 / 403 / 404) are never retried.
    job_max_retries: u64,
    /// Backoff between transient-failure retries
    /// (`BROWSER_HEADLESS_JOB_RETRY_BACKOFF_MS`, default 500).
    job_retry_backoff_ms: u64,
    /// On shutdown (SIGTERM / SIGINT), how long to wait for in-flight jobs to
    /// finish before exiting anyway (`BROWSER_HEADLESS_WORKER_DRAIN_MS`,
    /// default 30000). Jobs still running at the deadline stay pending and are
    /// reclaimed by another worker.
    drain_ms: u64,
}

impl WorkerConfig {
    fn from_env() -> Self {
        Self {
            redis_url: env_string("BROWSER_HEADLESS_REDIS_URL", "redis://127.0.0.1:6379"),
            cluster: env_bool("BROWSER_HEADLESS_REDIS_CLUSTER", false),
            ca_cert_path: env_opt("BROWSER_HEADLESS_REDIS_CA_CERT"),
            client_cert_path: env_opt("BROWSER_HEADLESS_REDIS_CLIENT_CERT"),
            client_key_path: env_opt("BROWSER_HEADLESS_REDIS_CLIENT_KEY"),
            stream: env_string("BROWSER_HEADLESS_JOBS_STREAM", "browser_headless:jobs"),
            group: env_string("BROWSER_HEADLESS_CONSUMER_GROUP", "workers"),
            group_start: env_string("BROWSER_HEADLESS_GROUP_START", "0"),
            consumer: env_string(
                "BROWSER_HEADLESS_CONSUMER_NAME",
                &format!("worker-{}", std::process::id()),
            ),
            result_prefix: env_string("BROWSER_HEADLESS_RESULT_PREFIX", "browser_headless:result:"),
            result_ttl_secs: env_u64("BROWSER_HEADLESS_RESULT_TTL_SECS", 3600).max(1),
            delete_on_ack: env_bool("BROWSER_HEADLESS_DELETE_ON_ACK", true),
            block_ms: env_u64("BROWSER_HEADLESS_JOB_BLOCK_MS", 60_000).max(1) as usize,
            visibility_ms: env_u64("BROWSER_HEADLESS_JOB_VISIBILITY_MS", 120_000).max(1),
            connect_timeout_ms: env_u64("BROWSER_HEADLESS_REDIS_CONNECT_TIMEOUT_MS", 5000).max(1),
            metrics_sample_secs: env_u64("BROWSER_HEADLESS_METRICS_SAMPLE_SECS", 300).max(1),
            result_notify: env_bool("BROWSER_HEADLESS_RESULT_NOTIFY", true),
            job_max_retries: env_u64("BROWSER_HEADLESS_JOB_MAX_RETRIES", 2),
            job_retry_backoff_ms: env_u64("BROWSER_HEADLESS_JOB_RETRY_BACKOFF_MS", 500),
            drain_ms: env_u64("BROWSER_HEADLESS_WORKER_DRAIN_MS", 30_000).max(1),
        }
    }
}

/// Mask credentials in a Redis URL (or comma-separated cluster seed list) for
/// logging: keep scheme / host / port / path, replace any `user:pass@` with
/// `***@`. Never log the raw URL — it embeds the password (e.g. Upstash).
fn redact_redis_url(raw: &str) -> String {
    raw.split(',')
        .map(|node| {
            let node = node.trim();
            match url::Url::parse(node) {
                Ok(u) if !u.username().is_empty() || u.password().is_some() => {
                    let host = u.host_str().unwrap_or("");
                    match u.port() {
                        Some(p) => format!("{}://***@{host}:{p}{}", u.scheme(), u.path()),
                        None => format!("{}://***@{host}{}", u.scheme(), u.path()),
                    }
                }
                Ok(_) => node.to_string(),
                // Unparseable — don't risk echoing embedded credentials.
                Err(_) => "<redacted>".to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl WorkerConfig {
    /// Connection options for the shared [`crate::redis_conn`] plumbing. The
    /// response timeout MUST exceed the `XREADGROUP` block window: a blocking
    /// read on an idle stream should return empty (the server's BLOCK nil)
    /// rather than tripping a premature client-side timeout that the loop
    /// would mistake for a dead connection.
    fn conn_opts(&self) -> RedisConnOpts {
        RedisConnOpts {
            url: self.redis_url.clone(),
            cluster: self.cluster,
            ca_cert_path: self.ca_cert_path.clone(),
            client_cert_path: self.client_cert_path.clone(),
            client_key_path: self.client_key_path.clone(),
            connect_timeout: Duration::from_millis(self.connect_timeout_ms),
            response_timeout: Duration::from_millis(self.block_ms as u64 + 5_000),
        }
    }
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

/// Run the worker loop until `shutdown` flips to `true` (SIGTERM / SIGINT),
/// then drain in-flight jobs and return. Shares the browser pool via `ctx`;
/// reads its own Redis config from the environment.
pub(crate) async fn run(ctx: CaptureCtx, mut shutdown: watch::Receiver<bool>) {
    let cfg = Arc::new(WorkerConfig::from_env());
    let capacity = ctx.pool.capacity().max(1);
    describe_worker_metrics();
    tracing::info!(
        redis_url = %redact_redis_url(&cfg.redis_url),
        cluster = cfg.cluster,
        tls_custom_certs = cfg.ca_cert_path.is_some() || cfg.client_cert_path.is_some(),
        stream = %cfg.stream,
        group = %cfg.group,
        consumer = %cfg.consumer,
        concurrency = capacity,
        "worker mode starting"
    );

    let backend = Arc::new(Backend::new(&cfg.conn_opts()).expect("invalid redis config"));

    ensure_group(&backend, &cfg).await;

    // Separate connections: `read_conn` owns the blocking XREADGROUP; the
    // cloned `write_conn` (cloned again per job) handles concurrent result
    // writes + acks without being stalled behind the blocking read.
    let mut read_conn = connect(&backend, &cfg).await;
    let write_conn = connect(&backend, &cfg).await;

    // Bounds how many captures run at once — and therefore how many jobs we
    // pull from Redis — to the pool capacity. A permit is held for a job's
    // whole lifetime and released when its spawned task finishes, so a slow
    // job frees its slot independently instead of holding back a whole batch
    // (the old batch-at-a-time loop left fast slots idle until the slowest
    // job in the batch completed).
    let inflight = Arc::new(Semaphore::new(capacity));

    // Background queue-depth sampler (XLEN / XPENDING → gauges). In-flight is
    // tracked per-job by `InFlightGuard`, not sampled here.
    tokio::spawn(sample_queue_metrics(backend.clone(), cfg.clone()));

    let mut backoff = Duration::from_secs(1);
    loop {
        // Reserve one slot before reading. This blocks while all `capacity`
        // captures are busy — backpressure: we never pull jobs we can't run
        // now. `want` is everything currently free, so a burst of freed slots
        // is refilled in one batched read.
        let first = tokio::select! {
            biased;
            _ = shutdown_requested(&mut shutdown) => break,
            permit = inflight.clone().acquire_owned() => {
                permit.expect("inflight semaphore closed")
            }
        };
        let want = 1 + inflight.available_permits();

        // Block for new jobs, but abandon the read on shutdown (releasing the
        // reserved slot) rather than sitting in a multi-second XREADGROUP while
        // we're trying to drain.
        let read = tokio::select! {
            biased;
            _ = shutdown_requested(&mut shutdown) => {
                drop(first);
                break;
            }
            r = poll_new(&mut read_conn, &cfg, want) => r,
        };

        match read {
            Ok(entries) if entries.is_empty() => {
                // No new jobs — release the reserved slot and try to reclaim
                // entries abandoned by a crashed worker before blocking again.
                drop(first);
                backoff = Duration::from_secs(1);
                match reclaim(&mut read_conn, &cfg, want).await {
                    Ok(reclaimed) if !reclaimed.is_empty() => {
                        metrics::counter!(M_WORKER_RECLAIMED).increment(reclaimed.len() as u64);
                        tracing::info!(count = reclaimed.len(), "reclaimed stale jobs");
                        for entry in reclaimed {
                            let permit = inflight
                                .clone()
                                .acquire_owned()
                                .await
                                .expect("inflight semaphore closed");
                            spawn_job(ctx.clone(), cfg.clone(), write_conn.clone(), permit, entry);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "XAUTOCLAIM failed"),
                }
            }
            Ok(entries) => {
                backoff = Duration::from_secs(1);
                // Non-empty. Reuse the reserved permit for the first entry;
                // acquire one per remaining entry (all within `want`, so ready).
                let mut held = Some(first);
                for entry in entries {
                    let permit = match held.take() {
                        Some(p) => p,
                        None => inflight
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("inflight semaphore closed"),
                    };
                    spawn_job(ctx.clone(), cfg.clone(), write_conn.clone(), permit, entry);
                }
            }
            Err(e) if e.code() == Some("NOGROUP") => {
                // The stream / consumer group vanished while we were running
                // (deleted, evicted, or trimmed away). Recreate it and retry —
                // no need to churn the connection.
                drop(first);
                tracing::warn!(error = %e, "consumer group missing; recreating");
                ensure_group(&backend, &cfg).await;
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                drop(first);
                tracing::error!(error = %e, retry_in = ?backoff, "redis read failed; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                read_conn = connect(&backend, &cfg).await;
            }
        }
    }

    // Graceful drain: stop pulling new jobs, then wait for in-flight ones to
    // finish — each holds a permit, so acquiring all `capacity` means none are
    // left — bounded by `drain_ms`. Jobs still running at the deadline stay
    // pending in Redis and are reclaimed by another worker (at-least-once).
    tracing::info!("worker shutting down; draining in-flight jobs");
    match tokio::time::timeout(
        Duration::from_millis(cfg.drain_ms),
        inflight.acquire_many(capacity as u32),
    )
    .await
    {
        Ok(_) => tracing::info!("worker drained cleanly; exiting"),
        Err(_) => tracing::warn!(
            in_flight = capacity - inflight.available_permits(),
            "worker drain timed out; exiting with jobs still in flight (will be reclaimed)"
        ),
    }
}

/// Resolve once the shutdown flag flips to `true` (or the sender is dropped,
/// which shouldn't happen — it lives for the whole process).
async fn shutdown_requested(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

/// Create the consumer group (and the stream, via `MKSTREAM`) if absent.
/// Retries until Redis is reachable; a pre-existing group (`BUSYGROUP`) is fine.
async fn ensure_group(backend: &Backend, cfg: &WorkerConfig) {
    loop {
        match backend.connect(&cfg.conn_opts()).await {
            Ok(mut conn) => {
                let res: redis::RedisResult<()> = conn
                    .xgroup_create_mkstream(
                        cfg.stream.as_str(),
                        cfg.group.as_str(),
                        cfg.group_start.as_str(),
                    )
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

/// Open an async connection (single-node or cluster), retrying until Redis is
/// reachable.
async fn connect(backend: &Backend, cfg: &WorkerConfig) -> Conn {
    loop {
        match backend.connect(&cfg.conn_opts()).await {
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
    conn: &mut Conn,
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
    conn: &mut Conn,
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

/// RAII guard for the worker in-flight gauge — increments on construction,
/// decrements on drop. Counts captures actually running, NOT the read loop's
/// reserved permit (which is held during the idle blocking read), and survives
/// a task panic since drop runs during unwind.
struct InFlightGuard;
impl InFlightGuard {
    fn new() -> Self {
        metrics::gauge!(M_WORKER_INFLIGHT).increment(1.0);
        InFlightGuard
    }
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!(M_WORKER_INFLIGHT).decrement(1.0);
    }
}

/// Spawn a detached task that processes one entry and releases its pool slot
/// (`permit`) when it finishes. Detaching lets a slow capture run independently
/// of the read loop, so the worker keeps pulling and dispatching other jobs
/// while it runs.
fn spawn_job(
    ctx: CaptureCtx,
    cfg: Arc<WorkerConfig>,
    conn: Conn,
    permit: OwnedSemaphorePermit,
    entry: StreamId,
) {
    tokio::spawn(async move {
        // Held for the whole job; dropping it on task completion frees the slot.
        let _permit = permit;
        let _in_flight = InFlightGuard::new();
        process_one(&ctx, &cfg, conn, entry).await;
    });
}

/// Process one entry: parse → capture → write `result:{id}` → ack. A failed
/// capture still writes a result + acks (a definitive outcome). A failed Redis
/// write leaves the entry pending so a later reclaim retries it.
async fn process_one(ctx: &CaptureCtx, cfg: &WorkerConfig, mut conn: Conn, entry: StreamId) {
    let entry_id = entry.id.clone();
    let Some(payload) = entry.get::<String>("payload") else {
        tracing::warn!(entry = %entry_id, "job missing `payload` field; dropping");
        ack(&mut conn, cfg, &entry_id).await;
        return;
    };
    let job: WorkerJob = match serde_json::from_str(&payload) {
        Ok(job) => job,
        Err(e) => {
            tracing::warn!(entry = %entry_id, error = %e, "malformed job; dropping");
            ack(&mut conn, cfg, &entry_id).await;
            return;
        }
    };
    let id = job.id.unwrap_or_else(|| entry_id.clone());
    let url = job.query.url.clone();
    let started = Instant::now();
    tracing::info!(entry = %entry_id, id = %id, url = %url, "job started");

    // Progress marker: flip the result key to `running` and RESET its TTL.
    // Two jobs done at once: pollers see the true state instead of a stale
    // `queued`, and a job that sat in a backlog longer than the result TTL
    // no longer 404s ("expired") moments before its result appears — the
    // enqueue-time clock restarts when processing actually begins.
    // Unconditional SET is safe under at-least-once: if a redelivered entry
    // briefly overwrites an already-written result, the completion SETEX
    // below rewrites it — pollers transiently see `running`, never lose the
    // final result. Best-effort (failure only degrades the progress signal).
    let running = serde_json::json!({ "id": id, "phase": "running" }).to_string();
    let marker_key = format!("{}{}", cfg.result_prefix, id);
    let marked: redis::RedisResult<()> = conn
        .set_ex(&marker_key, &running, cfg.result_ttl_secs)
        .await;
    if let Err(e) = marked {
        tracing::debug!(entry = %entry_id, error = %e, "running marker write failed");
    }
    let result = capture_to_result(ctx, cfg, id, job.query).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    // Worker-queue metrics. `capture_one` already records the generic capture
    // counter/histogram; these are queue-specific: per-job outcome + the
    // end-to-end worker time (capture + bookkeeping).
    let outcome = if result.status < 400 { "ok" } else { "error" };
    metrics::counter!(M_WORKER_JOBS, "outcome" => outcome).increment(1);
    metrics::histogram!(M_WORKER_JOB_DURATION).record(started.elapsed().as_secs_f64());

    let json = match serde_json::to_string(&result) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(entry = %entry_id, error = %e, "result serialize failed");
            ack(&mut conn, cfg, &entry_id).await;
            return;
        }
    };
    let key = format!("{}{}", cfg.result_prefix, result.id);
    // Result write + subscriber notify in ONE pipelined round-trip (the
    // channel name == the result key, so cluster routing stays single-slot;
    // PUBLISH itself is slotless and broadcasts cluster-wide from whichever
    // node the pipeline lands on). The old serial `SETEX` → `PUBLISH` paid
    // two RTTs per job — real money on a cross-region Redis. PUBLISH only
    // fails when the connection is broken (it can't fail server-side), so
    // folding it into the write's error path loses nothing: any pipeline
    // error → leave the entry pending for reclaim, same as a write failure.
    let mut pipe = redis::pipe();
    pipe.set_ex(&key, &json, cfg.result_ttl_secs).ignore();
    if cfg.result_notify {
        pipe.publish(&key, &json).ignore();
    }
    let write: redis::RedisResult<()> = pipe.query_async(&mut conn).await;
    if let Err(e) = write {
        // Don't ack — the entry stays pending and gets reclaimed, so the result
        // isn't silently lost.
        tracing::warn!(entry = %entry_id, error = %e, "result write failed; leaving entry pending");
        return;
    }

    tracing::info!(entry = %entry_id, id = %result.id, status = result.status, duration_ms, "job done");
    ack(&mut conn, cfg, &entry_id).await;
}

/// Register HELP/TYPE descriptions for the worker-specific metrics. Safe to
/// call once at startup (the recorder is installed by `run_worker` / `run_all`
/// before the worker starts).
fn describe_worker_metrics() {
    use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};
    describe_counter!(
        M_WORKER_JOBS,
        "Worker jobs processed, labelled by outcome (ok / error)"
    );
    describe_histogram!(
        M_WORKER_JOB_DURATION,
        Unit::Seconds,
        "Per-job worker processing time (capture + result write)"
    );
    describe_counter!(
        M_WORKER_RECLAIMED,
        "Stale pending entries reclaimed from presumed-dead workers via XAUTOCLAIM"
    );
    describe_counter!(
        M_WORKER_RETRIES,
        "Transient capture failures retried before a terminal result"
    );
    describe_gauge!(M_WORKER_INFLIGHT, "Worker captures currently in flight");
    describe_gauge!(
        M_WORKER_STREAM_LEN,
        "Jobs stream length (XLEN) at last sample"
    );
    describe_gauge!(
        M_WORKER_PENDING,
        "Pending (delivered, unacked) entries for the consumer group at last sample"
    );
}

/// Periodically refresh the queue-depth gauges from Redis: stream length
/// (`XLEN`) and group pending count (`XPENDING` summary). In-flight is tracked
/// separately by [`InFlightGuard`] around each job. Owns its own connection so
/// it never contends with the blocking read loop; reconnects on error.
async fn sample_queue_metrics(backend: Arc<Backend>, cfg: Arc<WorkerConfig>) {
    let interval = Duration::from_secs(cfg.metrics_sample_secs);
    let mut conn = connect(&backend, &cfg).await;
    loop {
        tokio::time::sleep(interval).await;

        let len: redis::RedisResult<i64> = conn.xlen(cfg.stream.as_str()).await;
        match len {
            Ok(len) => metrics::gauge!(M_WORKER_STREAM_LEN).set(len as f64),
            Err(e) => {
                tracing::debug!(error = %e, "XLEN sample failed; reconnecting");
                conn = connect(&backend, &cfg).await;
                continue;
            }
        }

        match xpending_count(&mut conn, &cfg).await {
            Ok(pending) => metrics::gauge!(M_WORKER_PENDING).set(pending as f64),
            Err(e) => {
                tracing::debug!(error = %e, "XPENDING sample failed; reconnecting");
                conn = connect(&backend, &cfg).await;
            }
        }
    }
}

/// Pending-entry count for the consumer group (`XPENDING` summary form).
async fn xpending_count(conn: &mut Conn, cfg: &WorkerConfig) -> redis::RedisResult<u64> {
    let reply: StreamPendingReply = conn
        .xpending(cfg.stream.as_str(), cfg.group.as_str())
        .await?;
    Ok(match reply {
        StreamPendingReply::Data(data) => data.count as u64,
        // `Empty` and any future non-exhaustive variant → no pending entries.
        _ => 0,
    })
}

/// Run one capture and shape it into a `JobResult`, retrying *transient*
/// failures (408 / 502 / 503 / 504) up to `job_max_retries` times with a fixed
/// backoff. Definitive failures (400 / 401 / 403 / 404) and successes return
/// immediately.
async fn capture_to_result(
    ctx: &CaptureCtx,
    cfg: &WorkerConfig,
    id: String,
    query: SummaryQuery,
) -> JobResult {
    let mut attempt = 0u64;
    loop {
        // `capture_one` consumes the query; clone per attempt so a retry can
        // re-run it (cheap relative to a browser capture).
        match capture::capture_one(ctx, query.clone()).await {
            Ok(Captured::Content(content)) => {
                return JobResult::ok(id, serde_json::to_value(content));
            }
            Ok(Captured::Full(stat)) => return JobResult::ok(id, serde_json::to_value(stat)),
            Err(e) => {
                let status = e.status_u16();
                if is_retryable(status) && attempt < cfg.job_max_retries {
                    attempt += 1;
                    metrics::counter!(M_WORKER_RETRIES).increment(1);
                    tracing::warn!(
                        id = %id,
                        status,
                        attempt,
                        max = cfg.job_max_retries,
                        "transient capture failure; retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(cfg.job_retry_backoff_ms)).await;
                    continue;
                }
                return JobResult::err(id, status, e.message);
            }
        }
    }
}

/// Transient capture failures worth retrying — gateway / availability / timeout
/// class. Client/definitive errors (400 / 401 / 403 / 404) are never retried.
fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 502 | 503 | 504)
}

async fn ack(conn: &mut Conn, cfg: &WorkerConfig, entry_id: &str) {
    // XACK only clears the pending list — the entry stays in the stream, so
    // (by default) XDEL it too, or the stream grows without bound (the result
    // already lives in `result:{id}`). Both commands key on the stream (same
    // cluster slot), so they're pipelined into one round-trip instead of the
    // old serial pair. The reclaim path is unaffected: we only reach here
    // after a definitive outcome, so nothing reclaimable is ever deleted.
    let acked: redis::RedisResult<()> = if cfg.delete_on_ack {
        let mut pipe = redis::pipe();
        pipe.xack(cfg.stream.as_str(), cfg.group.as_str(), &[entry_id])
            .ignore();
        pipe.xdel(cfg.stream.as_str(), &[entry_id]).ignore();
        pipe.query_async(&mut *conn).await
    } else {
        conn.xack(cfg.stream.as_str(), cfg.group.as_str(), &[entry_id])
            .await
            .map(|_: i64| ())
    };
    if let Err(e) = acked {
        tracing::warn!(entry = %entry_id, error = %e, "ack failed");
    }
}
