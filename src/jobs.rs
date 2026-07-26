//! Async job store for `POST /jobs` + `GET /jobs/:id`.
//!
//! Two backends (selected by `BROWSER_HEADLESS_JOBS_BACKEND`):
//! * **Local** — in-process spawn on the shared browser pool (default when
//!   Redis is not configured).
//! * **Redis** — `XADD` to the same stream workers consume, poll
//!   `result:{id}` keys (horizontal scale with `MODE=worker`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::capture::{self, CaptureCtx, Captured, SummaryQuery};
use crate::config;
use crate::error::CaptureError;
use crate::queue::QueueClient;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JobStatus {
    Queued,
    Running,
    Done,
    Error,
}

#[derive(Clone, Serialize)]
pub(crate) struct JobView {
    pub(crate) id: String,
    pub(crate) status: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// Age of the job in milliseconds (from submit). Redis backend may
    /// report `0` when age is not tracked server-side.
    pub(crate) age_ms: u64,
}

struct JobRecord {
    status: JobStatus,
    http_status: Option<u16>,
    data: Option<serde_json::Value>,
    error: Option<String>,
    created: Instant,
    expires: Instant,
}

/// Process-local job map. Cheap to clone (`Arc`).
#[derive(Clone, Default)]
pub(crate) struct JobStore {
    inner: Arc<RwLock<HashMap<String, JobRecord>>>,
}

impl JobStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn submit(
        &self,
        ctx: CaptureCtx,
        query: SummaryQuery,
    ) -> Result<String, CaptureError> {
        let max = config::max_async_jobs();
        let id = Uuid::new_v4().to_string();
        let now = Instant::now();
        let ttl = config::job_ttl();
        {
            // One write lock for the whole check → evict → insert sequence, so
            // concurrent submits can't all pass the cap check (the old split
            // read-then-write was a TOCTOU that overshot the cap).
            let mut map = self.inner.write().await;
            map.retain(|_, rec| rec.expires > now);

            // The cap bounds ACTIVE work (queued + running) only. Counting
            // every retained record — the old behaviour — throttled
            // throughput to `max` jobs per TTL window: 256 jobs that finished
            // in seconds blocked all submits for the rest of the hour.
            let active = map
                .values()
                .filter(|r| matches!(r.status, JobStatus::Queued | JobStatus::Running))
                .count();
            if active >= max {
                return Err(CaptureError::service_unavailable(format!(
                    "async job queue full ({active}/{max} in flight); retry later or raise BROWSER_HEADLESS_MAX_ASYNC_JOBS"
                )));
            }

            // Memory stays bounded at the same `max` records as before: make
            // room by evicting the OLDEST COMPLETED result instead of
            // rejecting the submit. `active < max` guarantees a completed
            // record exists whenever the map is full, so this always finds
            // one; a poller that comes back after its result was evicted gets
            // the same 404 an expired TTL would have produced.
            while map.len() >= max {
                let Some(oldest) = map
                    .iter()
                    .filter(|(_, r)| matches!(r.status, JobStatus::Done | JobStatus::Error))
                    .min_by_key(|(_, r)| r.created)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                map.remove(&oldest);
            }

            map.insert(
                id.clone(),
                JobRecord {
                    status: JobStatus::Queued,
                    http_status: None,
                    data: None,
                    error: None,
                    created: now,
                    expires: now + ttl,
                },
            );
        }

        let store = self.clone();
        let job_id = id.clone();
        tokio::spawn(async move {
            store.run_job(job_id, ctx, query).await;
        });
        Ok(id)
    }

    async fn run_job(&self, id: String, ctx: CaptureCtx, query: SummaryQuery) {
        {
            let mut map = self.inner.write().await;
            if let Some(rec) = map.get_mut(&id) {
                rec.status = JobStatus::Running;
            }
        }

        // Queued-capture variant: pool-slot wait bounded by the job TTL, not
        // the interactive 30s admission cut — an async job is supposed to
        // wait out a busy pool instead of being shed with a terminal 503.
        let result = capture::capture_one_queued(&ctx, query).await;
        let mut map = self.inner.write().await;
        let Some(rec) = map.get_mut(&id) else {
            return;
        };
        rec.expires = Instant::now() + config::job_ttl();
        match result {
            Ok(Captured::Content(c)) => match serde_json::to_value(c) {
                Ok(v) => {
                    rec.status = JobStatus::Done;
                    rec.http_status = Some(200);
                    rec.data = Some(v);
                }
                Err(e) => {
                    rec.status = JobStatus::Error;
                    rec.http_status = Some(500);
                    rec.error = Some(format!("serialize: {e}"));
                }
            },
            Ok(Captured::Full(stat)) => match serde_json::to_value(stat) {
                Ok(v) => {
                    rec.status = JobStatus::Done;
                    rec.http_status = Some(200);
                    rec.data = Some(v);
                }
                Err(e) => {
                    rec.status = JobStatus::Error;
                    rec.http_status = Some(500);
                    rec.error = Some(format!("serialize: {e}"));
                }
            },
            Err(e) => {
                rec.status = JobStatus::Error;
                rec.http_status = Some(e.status_u16());
                rec.error = Some(e.message);
            }
        }
    }

    pub(crate) async fn get(&self, id: &str) -> Option<JobView> {
        self.purge_expired().await;
        let map = self.inner.read().await;
        let rec = map.get(id)?;
        Some(JobView {
            id: id.to_string(),
            status: rec.status,
            http_status: rec.http_status,
            data: rec.data.clone(),
            error: rec.error.clone(),
            age_ms: rec.created.elapsed().as_millis() as u64,
        })
    }

    async fn purge_expired(&self) {
        let now = Instant::now();
        let mut map = self.inner.write().await;
        map.retain(|_, rec| rec.expires > now);
    }

    pub(crate) fn spawn_sweeper(self, interval: Duration) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                self.purge_expired().await;
            }
        });
    }
}

/// Unified jobs backend for the HTTP layer.
#[derive(Clone)]
pub(crate) enum JobsBackend {
    Local(JobStore),
    Redis(QueueClient),
}

impl JobsBackend {
    pub(crate) async fn submit(
        &self,
        ctx: CaptureCtx,
        query: SummaryQuery,
    ) -> Result<String, CaptureError> {
        match self {
            JobsBackend::Local(store) => store.submit(ctx, query).await,
            // Redis path: workers own the pool; HTTP only enqueues.
            JobsBackend::Redis(q) => {
                let _ = ctx; // pool unused for enqueue
                q.enqueue(query).await
            }
        }
    }

    pub(crate) async fn get(&self, id: &str) -> Result<Option<JobView>, CaptureError> {
        match self {
            JobsBackend::Local(store) => Ok(store.get(id).await),
            JobsBackend::Redis(q) => q.get(id).await,
        }
    }
}
