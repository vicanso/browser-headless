//! Shared Redis Streams job queue — producer side used by HTTP `POST /jobs`
//! when the jobs backend is Redis, consumer side remains in [`crate::worker`].
//!
//! Wire format matches the worker:
//! * Enqueue: `XADD` stream field `payload` = JSON
//!   `{ "id": "…", "url": "…", …SummaryQuery… }`
//! * Result key: `<result_prefix><id>` TTL'd JSON
//!   `{ "id", "status": <http u16>, "data"?, "error"? }` on completion;
//!   intermediate enqueue marker `{ "id", "phase": "queued" }`.

use std::time::Duration;

use redis::{AsyncCommands, RedisResult};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capture::SummaryQuery;
use crate::config::{self, env_bool, env_opt, env_string, env_u64};
use crate::error::CaptureError;
use crate::jobs::{JobStatus, JobView};
use crate::redis_conn::{Backend, Conn, RedisConnOpts};

/// How HTTP `/jobs` is backed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobsBackendKind {
    /// In-process spawn (default when Redis is not configured).
    Local,
    /// XADD to the worker stream; GET result keys from Redis.
    Redis,
}

impl JobsBackendKind {
    /// `BROWSER_HEADLESS_JOBS_BACKEND`:
    /// * `local` — always in-process
    /// * `redis` — always Redis; the process REFUSES TO START if Redis is
    ///   unreachable (an explicit setting must never silently degrade to a
    ///   node-local store — jobs would bypass the worker fleet and ids would
    ///   only resolve on one node behind a load balancer)
    /// * `auto` (default) — Redis when `BROWSER_HEADLESS_REDIS_URL` is set,
    ///   otherwise local
    pub(crate) fn from_env() -> Self {
        match env_string("BROWSER_HEADLESS_JOBS_BACKEND", "auto")
            .to_ascii_lowercase()
            .as_str()
        {
            "local" => JobsBackendKind::Local,
            "redis" => JobsBackendKind::Redis,
            _ => {
                if std::env::var("BROWSER_HEADLESS_REDIS_URL")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .is_some()
                {
                    JobsBackendKind::Redis
                } else {
                    JobsBackendKind::Local
                }
            }
        }
    }
}

/// Redis connection settings shared with the worker (same env vars).
#[derive(Clone)]
pub(crate) struct QueueConfig {
    pub redis_url: String,
    pub cluster: bool,
    pub ca_cert_path: Option<String>,
    pub client_cert_path: Option<String>,
    pub client_key_path: Option<String>,
    pub stream: String,
    /// Consumer group to pre-create (idempotently) at startup, so jobs
    /// enqueued before any worker has ever run aren't lost to a later
    /// `GROUP_START=$` group creation.
    pub group: String,
    pub group_start: String,
    pub result_prefix: String,
    pub result_ttl_secs: u64,
    pub connect_timeout_ms: u64,
    /// `XADD MAXLEN ~ N` backstop (`BROWSER_HEADLESS_JOBS_MAXLEN`, default
    /// 100_000; `0` = unlimited). The worker's `delete_on_ack` is the primary
    /// trim; this caps growth when no worker is consuming at all.
    pub maxlen: u64,
}

impl QueueConfig {
    pub(crate) fn from_env() -> Self {
        Self {
            redis_url: env_string("BROWSER_HEADLESS_REDIS_URL", "redis://127.0.0.1:6379"),
            cluster: env_bool("BROWSER_HEADLESS_REDIS_CLUSTER", false),
            ca_cert_path: env_opt("BROWSER_HEADLESS_REDIS_CA_CERT"),
            client_cert_path: env_opt("BROWSER_HEADLESS_REDIS_CLIENT_CERT"),
            client_key_path: env_opt("BROWSER_HEADLESS_REDIS_CLIENT_KEY"),
            stream: env_string("BROWSER_HEADLESS_JOBS_STREAM", "browser_headless:jobs"),
            group: env_string("BROWSER_HEADLESS_CONSUMER_GROUP", "workers"),
            group_start: env_string("BROWSER_HEADLESS_GROUP_START", "0"),
            result_prefix: env_string("BROWSER_HEADLESS_RESULT_PREFIX", "browser_headless:result:"),
            result_ttl_secs: env_u64("BROWSER_HEADLESS_RESULT_TTL_SECS", 3600).max(1),
            connect_timeout_ms: env_u64("BROWSER_HEADLESS_REDIS_CONNECT_TIMEOUT_MS", 5000).max(1),
            maxlen: env_u64("BROWSER_HEADLESS_JOBS_MAXLEN", 100_000),
        }
    }

    fn result_key(&self, id: &str) -> String {
        format!("{}{id}", self.result_prefix)
    }
}

/// Stored result shape (worker + HTTP enqueue marker).
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct StoredJobResult {
    pub id: String,
    /// Present on intermediate enqueue markers (`queued` / `running`).
    #[serde(default)]
    pub phase: Option<String>,
    /// HTTP status on terminal results (worker always writes this).
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
}

impl StoredJobResult {
    pub(crate) fn into_job_view(self, age_ms: u64) -> JobView {
        // Terminal evidence (`status` / `data` / `error`) ALWAYS wins over a
        // `phase` hint: a stale or replayed "queued"/"running" marker must
        // never mask a result that is already present in the document. Phase
        // is only consulted when no terminal field exists.
        let (status, http_status) = match (
            self.phase.as_deref(),
            self.status,
            self.data.is_some(),
            self.error.is_some(),
        ) {
            (_, Some(s), true, _) if s < 400 => (JobStatus::Done, Some(s)),
            (_, Some(s), _, true) => (JobStatus::Error, Some(s)),
            (_, Some(s), _, _) if s >= 400 => (JobStatus::Error, Some(s)),
            (_, Some(s), _, _) => (JobStatus::Done, Some(s)),
            (_, None, _, true) => (JobStatus::Error, Some(500)),
            (_, None, true, _) => (JobStatus::Done, Some(200)),
            (Some("running"), None, _, _) => (JobStatus::Running, None),
            // "queued" marker or anything unrecognized without terminal data.
            _ => (JobStatus::Queued, None),
        };
        JobView {
            id: self.id,
            status,
            http_status,
            data: self.data,
            error: self.error,
            age_ms,
        }
    }
}

impl QueueConfig {
    /// Connection options for the shared [`crate::redis_conn`] plumbing.
    /// The producer only issues fast commands (XADD / SET / GET), so its
    /// operational response timeout is `connect + 5s` headroom — no blocking
    /// reads to span, unlike the worker.
    fn conn_opts(&self) -> RedisConnOpts {
        RedisConnOpts {
            url: self.redis_url.clone(),
            cluster: self.cluster,
            ca_cert_path: self.ca_cert_path.clone(),
            client_cert_path: self.client_cert_path.clone(),
            client_key_path: self.client_key_path.clone(),
            connect_timeout: Duration::from_millis(self.connect_timeout_ms),
            response_timeout: Duration::from_millis(self.connect_timeout_ms + 5_000),
        }
    }
}

/// Producer handle for HTTP job enqueue / result lookup.
///
/// Holds ONE cached connection (opened at startup, cloned per call —
/// multiplexed/cluster connections are designed for concurrent shared use)
/// instead of dialing TCP+TLS+AUTH per request: the documented usage is
/// *polling* `GET /jobs/{id}`, which would otherwise be a handshake storm.
/// On a command error the cache is invalidated and the next call reconnects.
#[derive(Clone)]
pub(crate) struct QueueClient {
    backend: std::sync::Arc<Backend>,
    /// Arc'd so the client stays pointer-sized (it lives inside the
    /// `JobsBackend` enum cloned into every request's state).
    cfg: std::sync::Arc<QueueConfig>,
    conn: std::sync::Arc<tokio::sync::Mutex<Option<Conn>>>,
}

impl QueueClient {
    pub(crate) async fn connect() -> Result<Self, CaptureError> {
        let cfg = QueueConfig::from_env();
        let backend = Backend::new(&cfg.conn_opts())
            .map_err(|e| CaptureError::service_unavailable(format!("redis config invalid: {e}")))?;
        // Prove connectivity once at startup; keep the connection for reuse.
        let mut conn = backend
            .connect(&cfg.conn_opts())
            .await
            .map_err(|e| CaptureError::service_unavailable(format!("redis connect failed: {e}")))?;
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(|e| CaptureError::service_unavailable(format!("redis PING failed: {e}")))?;
        // Pre-create the consumer group (and the stream via MKSTREAM),
        // idempotently — same call the worker makes. Without it, jobs
        // enqueued before the FIRST worker ever runs are permanently
        // invisible when that worker creates the group with `GROUP_START=$`.
        let created: RedisResult<()> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(&cfg.stream)
            .arg(&cfg.group)
            .arg(&cfg.group_start)
            .arg("MKSTREAM")
            .query_async(&mut conn)
            .await;
        match created {
            Ok(()) => tracing::info!(group = %cfg.group, "created consumer group"),
            Err(e) if e.code() == Some("BUSYGROUP") => {}
            // Non-fatal: the worker's own ensure_group retries; log and go.
            Err(e) => tracing::warn!(error = %e, "XGROUP CREATE failed"),
        }
        tracing::info!(
            stream = %cfg.stream,
            result_prefix = %cfg.result_prefix,
            "HTTP jobs backend: redis stream (shared with workers)"
        );
        Ok(Self {
            backend: std::sync::Arc::new(backend),
            cfg: std::sync::Arc::new(cfg),
            conn: std::sync::Arc::new(tokio::sync::Mutex::new(Some(conn))),
        })
    }

    /// Clone the cached connection, reconnecting lazily if a previous call
    /// invalidated it. The mutex is held only for the clone/connect, never
    /// across a command.
    async fn conn(&self) -> Result<Conn, CaptureError> {
        let mut guard = self.conn.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let c = self
            .backend
            .connect(&self.cfg.conn_opts())
            .await
            .map_err(|e| CaptureError::service_unavailable(format!("redis connect failed: {e}")))?;
        *guard = Some(c.clone());
        Ok(c)
    }

    /// Drop the cached connection so the next call redials. Called on any
    /// command error — a multiplexed connection does not self-heal.
    async fn invalidate(&self) {
        *self.conn.lock().await = None;
    }

    /// Enqueue a job; returns the job id.
    ///
    /// The id is ALWAYS server-minted (UUID v4). It must never come from the
    /// caller: the id is the Redis result-key suffix, so a caller-chosen id
    /// lets one client overwrite / poll-hijack another's result (and two
    /// honest clients reusing a stable trace id would silently collide).
    /// `request_id` stays what it is documented to be — an opaque correlation
    /// field inside the payload — and defaults to the job id when absent so
    /// worker-side logs still correlate.
    pub(crate) async fn enqueue(&self, mut query: SummaryQuery) -> Result<String, CaptureError> {
        let id = Uuid::new_v4().to_string();
        if query.request_id.as_deref().is_none_or(str::is_empty) {
            query.request_id = Some(id.clone());
        }

        #[derive(Serialize)]
        struct Payload<'a> {
            id: &'a str,
            #[serde(flatten)]
            query: SummaryQuery,
        }
        let payload = serde_json::to_string(&Payload { id: &id, query })
            .map_err(|e| CaptureError::internal(format!("job serialize: {e}")))?;

        let mut conn = self.conn().await?;

        // Two SEPARATE commands, marker first — deliberately NOT a MULTI/EXEC
        // pipeline: the stream and the result key hash to different cluster
        // slots, so an atomic pipeline is a guaranteed CROSSSLOT error under
        // BROWSER_HEADLESS_REDIS_CLUSTER (the worker's own pipelines are
        // single-key and unaffected). Ordering closes the race the old
        // atomicity targeted: the marker lands BEFORE the job becomes visible
        // to workers, so a fast worker result can never be clobbered by a
        // late marker write. `NX` makes the marker write non-destructive as a
        // final belt-and-braces (a v4 UUID collision is astronomically
        // unlikely; if it ever fires, refuse rather than overwrite).
        let marker = serde_json::json!({ "id": id, "phase": "queued" }).to_string();
        let key = self.cfg.result_key(&id);
        let set: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg(&marker)
            .arg("NX")
            .arg("EX")
            .arg(self.cfg.result_ttl_secs)
            .query_async(&mut conn)
            .await
            .map_err(|e| self.fail("redis marker write failed", e))?;
        if set.is_none() {
            return Err(CaptureError::internal(format!(
                "job id collision on `{id}`; not overwriting"
            )));
        }
        // `MAXLEN ~` backstop so the stream can't grow unbounded when no
        // worker is consuming (the worker's delete_on_ack is the real trim).
        let mut xadd = redis::cmd("XADD");
        xadd.arg(&self.cfg.stream);
        if self.cfg.maxlen > 0 {
            xadd.arg("MAXLEN").arg("~").arg(self.cfg.maxlen);
        }
        let added: Result<String, redis::RedisError> = xadd
            .arg("*")
            .arg("payload")
            .arg(&payload)
            .query_async(&mut conn)
            .await;
        if let Err(e) = added {
            // Best-effort marker cleanup so the failed submit doesn't linger
            // as a phantom `queued` job until TTL.
            let _: RedisResult<i64> = conn.del(&key).await;
            return Err(self.fail("redis enqueue failed", e));
        }

        Ok(id)
    }

    pub(crate) async fn get(&self, id: &str) -> Result<Option<JobView>, CaptureError> {
        let mut conn = self.conn().await?;
        let key = self.cfg.result_key(id);
        let raw: Option<String> = match conn.get(&key).await {
            Ok(v) => v,
            Err(e) => return Err(self.fail("redis GET failed", e)),
        };
        let Some(raw) = raw else {
            return Ok(None);
        };
        let stored: StoredJobResult = serde_json::from_str(&raw)
            .map_err(|e| CaptureError::internal(format!("job result decode: {e}")))?;
        // age_ms is not tracked server-side for the Redis backend; report 0
        // (documented as "unknown") rather than spending a second round-trip
        // per poll on TTL arithmetic.
        Ok(Some(stored.into_job_view(0)))
    }

    /// Map a command error to a 503 and schedule a reconnect for the next
    /// call. Fire-and-forget: invalidation only needs to happen before the
    /// next `conn()` acquires the lock.
    fn fail(&self, what: &str, e: redis::RedisError) -> CaptureError {
        let this = self.clone();
        tokio::spawn(async move { this.invalidate().await });
        CaptureError::service_unavailable(format!("{what}: {e}"))
    }
}

/// Resolve jobs backend at startup; `None` means async jobs disabled.
pub(crate) async fn init_jobs_backend() -> Option<crate::jobs::JobsBackend> {
    if !config::async_jobs_enabled() {
        return None;
    }
    match JobsBackendKind::from_env() {
        JobsBackendKind::Local => {
            let store = crate::jobs::JobStore::new();
            store.clone().spawn_sweeper(Duration::from_secs(60));
            tracing::info!("HTTP jobs backend: local (in-process)");
            Some(crate::jobs::JobsBackend::Local(store))
        }
        JobsBackendKind::Redis => match QueueClient::connect().await {
            Ok(client) => Some(crate::jobs::JobsBackend::Redis(client)),
            // NO silent fallback to local. A node that degrades to an
            // in-process store bypasses the worker fleet, runs captures on
            // its own (typically minimal) pool, and mints ids that resolve
            // only on itself — behind a load balancer that's jobs randomly
            // 404ing with /readyz still green. When the operator explicitly
            // chose `redis` (or set REDIS_URL under `auto`), refuse to start
            // so the failure is visible at deploy time.
            Err(e) => panic!(
                "jobs backend is `redis` but Redis is unreachable: {e}. \
                 Fix connectivity or set BROWSER_HEADLESS_JOBS_BACKEND=local \
                 (or BROWSER_HEADLESS_ASYNC_JOBS=false)."
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_job_queued_phase() {
        let v = StoredJobResult {
            id: "a".into(),
            phase: Some("queued".into()),
            status: None,
            data: None,
            error: None,
        }
        .into_job_view(1);
        assert_eq!(v.status, JobStatus::Queued);
    }

    #[test]
    fn stored_job_worker_ok() {
        let v = StoredJobResult {
            id: "a".into(),
            phase: None,
            status: Some(200),
            data: Some(serde_json::json!({"x": 1})),
            error: None,
        }
        .into_job_view(0);
        assert_eq!(v.status, JobStatus::Done);
        assert_eq!(v.http_status, Some(200));
    }

    // Regression: a stale/replayed `queued` phase must not mask a result
    // that is already present in the same document (terminal wins).
    #[test]
    fn stored_job_terminal_beats_stale_phase() {
        let v = StoredJobResult {
            id: "a".into(),
            phase: Some("queued".into()),
            status: Some(200),
            data: Some(serde_json::json!({"x": 1})),
            error: None,
        }
        .into_job_view(0);
        assert_eq!(v.status, JobStatus::Done);
        assert_eq!(v.http_status, Some(200));
    }

    #[test]
    fn stored_job_running_phase() {
        let v = StoredJobResult {
            id: "a".into(),
            phase: Some("running".into()),
            status: None,
            data: None,
            error: None,
        }
        .into_job_view(0);
        assert_eq!(v.status, JobStatus::Running);
    }

    #[test]
    fn stored_job_worker_err() {
        let v = StoredJobResult {
            id: "a".into(),
            phase: None,
            status: Some(502),
            data: None,
            error: Some("upstream".into()),
        }
        .into_job_view(0);
        assert_eq!(v.status, JobStatus::Error);
        assert_eq!(v.http_status, Some(502));
    }
}
