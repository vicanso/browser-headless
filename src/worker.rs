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

use std::time::{Duration, Instant};

use futures::stream::StreamExt;
use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::cluster::{ClusterClient, ClusterClientBuilder};
use redis::cluster_async::ClusterConnection;
use redis::streams::{
    StreamAutoClaimOptions, StreamAutoClaimReply, StreamId, StreamReadOptions, StreamReadReply,
};
use redis::{
    AsyncCommands, AsyncConnectionConfig, ClientTlsConfig, Cmd, ErrorKind, Pipeline, RedisFuture,
    RedisResult, TlsCertificates, Value,
};
use serde::{Deserialize, Serialize};

use crate::capture::{self, CaptureCtx, Captured, SummaryQuery};

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
    /// to a reclaim pass.
    block_ms: usize,
    /// Min idle time before a pending entry is eligible for `XAUTOCLAIM` (i.e.
    /// the original worker is presumed dead).
    visibility_ms: u64,
    /// TCP + TLS + auth handshake timeout when opening a connection
    /// (`BROWSER_HEADLESS_REDIS_CONNECT_TIMEOUT_MS`, default 5000). redis-rs's
    /// own default is only 1s, too tight for a high-latency / cross-region
    /// link (e.g. Upstash), which surfaces as `timed out` on connect.
    connect_timeout_ms: u64,
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
            block_ms: env_u64("BROWSER_HEADLESS_JOB_BLOCK_MS", 5000).max(1) as usize,
            visibility_ms: env_u64("BROWSER_HEADLESS_JOB_VISIBILITY_MS", 120_000).max(1),
            connect_timeout_ms: env_u64("BROWSER_HEADLESS_REDIS_CONNECT_TIMEOUT_MS", 5000).max(1),
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

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
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

/// Read a PEM file into bytes for TLS configuration, mapping IO errors into a
/// `RedisError` that names the path (the worker panics on it at startup).
fn read_pem(path: &str) -> RedisResult<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        redis::RedisError::from((
            ErrorKind::Io,
            "failed to read TLS PEM file",
            format!("{path}: {e}"),
        ))
    })
}

/// Build `TlsCertificates` from the configured PEM paths, or `None` when no
/// custom TLS material is set (system trust store / plain `rediss://`). mTLS
/// requires BOTH client cert and key.
fn build_tls_certificates(cfg: &WorkerConfig) -> RedisResult<Option<TlsCertificates>> {
    if cfg.ca_cert_path.is_none() && cfg.client_cert_path.is_none() && cfg.client_key_path.is_none()
    {
        return Ok(None);
    }
    let root_cert = cfg.ca_cert_path.as_deref().map(read_pem).transpose()?;
    let client_tls = match (&cfg.client_cert_path, &cfg.client_key_path) {
        (Some(cert), Some(key)) => Some(ClientTlsConfig {
            client_cert: read_pem(cert)?,
            client_key: read_pem(key)?,
        }),
        (None, None) => None,
        _ => {
            return Err(redis::RedisError::from((
                ErrorKind::InvalidClientConfig,
                "incomplete mTLS config",
                "set BOTH BROWSER_HEADLESS_REDIS_CLIENT_CERT and \
                 BROWSER_HEADLESS_REDIS_CLIENT_KEY (or neither)"
                    .to_string(),
            )));
        }
    };
    Ok(Some(TlsCertificates {
        client_tls,
        root_cert,
    }))
}

/// A Redis connection that is either single-node (multiplexed) or cluster.
/// Both inner types implement [`ConnectionLike`], so `AsyncCommands` — and thus
/// every stream command the worker issues — works uniformly through this enum.
/// All worker commands are single-key (the stream, or one `result:{id}`), so
/// cluster routing never hits a cross-slot error.
#[derive(Clone)]
enum Conn {
    Single(MultiplexedConnection),
    Cluster(ClusterConnection),
}

impl ConnectionLike for Conn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Conn::Single(c) => c.req_packed_command(cmd),
            Conn::Cluster(c) => c.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Conn::Single(c) => c.req_packed_commands(cmd, offset, count),
            Conn::Cluster(c) => c.req_packed_commands(cmd, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Conn::Single(c) => c.get_db(),
            Conn::Cluster(c) => c.get_db(),
        }
    }
}

/// Client-side response timeout for worker connections. It MUST exceed the
/// `XREADGROUP` block window: a blocking read on an idle stream should return
/// empty (the server's BLOCK nil) rather than tripping a premature client-side
/// timeout that the loop would mistake for a dead connection. Writes are fast,
/// so the generous ceiling is only ever hit on a genuinely hung connection.
fn read_response_timeout(cfg: &WorkerConfig) -> Duration {
    Duration::from_millis(cfg.block_ms as u64 + 5_000)
}

/// Handshake (TCP + TLS + auth) timeout for opening a connection.
fn connect_timeout(cfg: &WorkerConfig) -> Duration {
    Duration::from_millis(cfg.connect_timeout_ms)
}

/// The Redis client — single-node or cluster — that hands out [`Conn`]s.
enum Backend {
    Single(redis::Client),
    Cluster(ClusterClient),
}

impl Backend {
    fn from_cfg(cfg: &WorkerConfig) -> RedisResult<Self> {
        let certs = build_tls_certificates(cfg)?;
        if cfg.cluster {
            // Comma-separated seed nodes, e.g.
            // "redis://10.0.0.1:6379,redis://10.0.0.2:6379".
            let nodes: Vec<String> = cfg
                .redis_url
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // The cluster client bakes the timeouts into every connection (no
            // per-connection setter), so set them here.
            let mut builder = ClusterClientBuilder::new(nodes)
                .connection_timeout(connect_timeout(cfg))
                .response_timeout(read_response_timeout(cfg));
            if let Some(certs) = certs {
                builder = builder.certs(certs);
            }
            Ok(Backend::Cluster(builder.build()?))
        } else {
            match certs {
                Some(certs) => Ok(Backend::Single(redis::Client::build_with_tls(
                    cfg.redis_url.clone(),
                    certs,
                )?)),
                None => Ok(Backend::Single(redis::Client::open(cfg.redis_url.clone())?)),
            }
        }
    }

    /// Open a connection. `connect_timeout` bounds the TCP+TLS+auth handshake
    /// and is used as the handshake-command response timeout; `response_timeout`
    /// is then applied for normal operation (long, to span `XREADGROUP BLOCK`).
    /// Both args are for single-node connections — cluster bakes its timeouts in
    /// at build time, so they're unused there.
    async fn connect(
        &self,
        connect_timeout: Duration,
        response_timeout: Duration,
    ) -> redis::RedisResult<Conn> {
        match self {
            Backend::Single(c) => {
                // Raise the connect + handshake-response timeout above redis-rs's
                // 1s / 500ms defaults so a slow link finishes authenticating.
                let conn_cfg = AsyncConnectionConfig::new()
                    .set_connection_timeout(Some(connect_timeout))
                    .set_response_timeout(Some(connect_timeout));
                let mut conn = c
                    .get_multiplexed_async_connection_with_config(&conn_cfg)
                    .await?;
                // Switch to the long operational response timeout for blocking reads.
                conn.set_response_timeout(response_timeout);
                Ok(Conn::Single(conn))
            }
            Backend::Cluster(c) => Ok(Conn::Cluster(c.get_async_connection().await?)),
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

/// Run the worker loop forever (until the process is signalled). Shares the
/// browser pool via `ctx`; reads its own Redis config from the environment.
pub(crate) async fn run(ctx: CaptureCtx) {
    let cfg = WorkerConfig::from_env();
    let capacity = ctx.pool.capacity().max(1);
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

    let backend = Backend::from_cfg(&cfg).expect("invalid redis config");

    ensure_group(&backend, &cfg).await;

    // Separate connections: `read_conn` owns the blocking XREADGROUP; the
    // cloned `write_conn` handles concurrent result writes + acks without being
    // stalled behind the blocking read.
    let mut read_conn = connect(&backend, &cfg).await;
    let write_conn = connect(&backend, &cfg).await;

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
            Err(e) if e.code() == Some("NOGROUP") => {
                // The stream / consumer group vanished while we were running
                // (deleted, evicted, or trimmed away). Recreate it and retry —
                // no need to churn the connection.
                tracing::warn!(error = %e, "consumer group missing; recreating");
                ensure_group(&backend, &cfg).await;
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::error!(error = %e, retry_in = ?backoff, "redis read failed; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                read_conn = connect(&backend, &cfg).await;
            }
        }
    }
}

/// Create the consumer group (and the stream, via `MKSTREAM`) if absent.
/// Retries until Redis is reachable; a pre-existing group (`BUSYGROUP`) is fine.
async fn ensure_group(backend: &Backend, cfg: &WorkerConfig) {
    loop {
        match backend
            .connect(connect_timeout(cfg), read_response_timeout(cfg))
            .await
        {
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
        match backend
            .connect(connect_timeout(cfg), read_response_timeout(cfg))
            .await
        {
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

/// Capture each entry concurrently (bounded by pool capacity), write its
/// result, then ack. A failed capture still writes a result + acks; a failed
/// Redis write leaves the entry pending for later reclaim.
async fn process(ctx: &CaptureCtx, cfg: &WorkerConfig, write_conn: &Conn, entries: Vec<StreamId>) {
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
                let url = job.query.url.clone();
                let started = Instant::now();
                tracing::info!(entry = %entry_id, id = %id, url = %url, "job started");
                let result = capture_to_result(&ctx, id, job.query).await;
                let duration_ms = started.elapsed().as_millis() as u64;

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
                tracing::info!(entry = %entry_id, id = %result.id, status = result.status, duration_ms, "job done");
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

async fn ack(conn: &mut Conn, cfg: &WorkerConfig, entry_id: &str) {
    let acked: redis::RedisResult<i64> = conn
        .xack(cfg.stream.as_str(), cfg.group.as_str(), &[entry_id])
        .await;
    if let Err(e) = acked {
        tracing::warn!(entry = %entry_id, error = %e, "XACK failed");
        return;
    }
    // XACK only clears the pending list — the entry stays in the stream. Delete
    // it so the stream doesn't grow without bound (the result already lives in
    // `result:{id}`). The reclaim path is unaffected: we only reach here after a
    // definitive outcome, so nothing reclaimable is ever deleted.
    if cfg.delete_on_ack {
        let deleted: redis::RedisResult<i64> = conn.xdel(cfg.stream.as_str(), &[entry_id]).await;
        if let Err(e) = deleted {
            tracing::warn!(entry = %entry_id, error = %e, "XDEL failed");
        }
    }
}
