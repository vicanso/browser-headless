//! HTTP/axum layer: router, handlers, API-key auth, request-shape logging,
//! and Prometheus exposition. All transport concerns live here; the actual
//! page capture is delegated to [`crate::capture`].

use std::sync::Arc;
use std::time::Instant;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRef, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_extra::extract::Query;
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::compression::CompressionLayer;
use tracing::Instrument;

use crate::capture::{
    self, BatchQuery, BatchResponse, CaptureCtx, Captured, ResponseFormat, SummaryQuery,
};
use crate::config::{self, max_batch_urls};
use crate::error::CaptureError;
use crate::jobs::{JobView, JobsBackend};
use crate::rate_limit::RateLimiter;

#[derive(Clone)]
pub(crate) struct AppState {
    /// Transport-agnostic capture context (browser pool + SSRF policy).
    pub(crate) ctx: CaptureCtx,
    /// Optional shared-secret API key. `None` → auth disabled (default, open).
    /// `Some` → every `/summary*` request must supply a matching `X-Api-Key`.
    pub(crate) api_key: Option<Arc<String>>,
    /// Prometheus exposition handle; `render()` produces the `/metrics` body.
    pub(crate) metrics_handle: PrometheusHandle,
    /// Process-wide rate limiter (may be a no-op when RPS is 0).
    pub(crate) rate_limiter: Arc<RateLimiter>,
    /// Async job backend for `POST /jobs` (local in-process or Redis stream).
    /// `None` when `BROWSER_HEADLESS_ASYNC_JOBS=false`.
    pub(crate) jobs: Option<JobsBackend>,
}

/// State for the probe/metrics routes — the pool (for readiness) plus the
/// Prometheus handle, without the API key. The full API router extracts it from
/// [`AppState`] via [`FromRef`]; the worker's health-only listener
/// ([`health_router`]) uses it directly, so worker mode exposes the same
/// `/healthz`, `/readyz`, `/metrics` without depending on the HTTP API.
#[derive(Clone)]
pub(crate) struct HealthState {
    pub(crate) ctx: CaptureCtx,
    pub(crate) metrics_handle: PrometheusHandle,
    /// When set and `protect_metrics` is on, metrics require this key.
    pub(crate) api_key: Option<Arc<String>>,
    pub(crate) protect_metrics: bool,
}

impl FromRef<AppState> for HealthState {
    fn from_ref(app: &AppState) -> HealthState {
        HealthState {
            ctx: app.ctx.clone(),
            metrics_handle: app.metrics_handle.clone(),
            api_key: app.api_key.clone(),
            protect_metrics: config::protect_metrics(),
        }
    }
}

/// Map a transport-neutral capture error to an axum response.
impl IntoResponse for CaptureError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, self.message).into_response()
    }
}

/// Build the axum router with all routes wired to `state`. The probe/metrics
/// handlers take `State<HealthState>`, extracted from `AppState` via `FromRef`.
pub(crate) fn router(state: AppState) -> Router {
    let body_limit = config::max_body_bytes();
    let mut r = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .route("/summary", get(summary_handler).post(summary_handler_post))
        .route("/summary/batch", post(summary_batch_handler))
        .route("/openapi.json", get(openapi_json));

    // Routes always registered when the backend is Some (init in main).
    if state.jobs.is_some() {
        r = r
            .route("/jobs", post(jobs_submit))
            .route("/jobs/{id}", get(jobs_get));
    }

    r.layer(DefaultBodyLimit::max(body_limit))
        // gzip/br/zstd response compression, negotiated per request via
        // Accept-Encoding. The capture payloads are large text (HTML /
        // markdown / WebPageStat JSON) that typically compresses 5-10×;
        // clients that don't send Accept-Encoding get identity as before.
        .layer(CompressionLayer::new())
        .with_state(state)
}

/// Health-only router for worker mode: `/healthz`, `/readyz`, `/metrics` and
/// nothing else (no `/summary*`, no API-key auth). serve / all modes use
/// [`router`], which serves these same probes alongside the capture API.
pub(crate) fn health_router(
    ctx: CaptureCtx,
    metrics_handle: PrometheusHandle,
    api_key: Option<Arc<String>>,
) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        // Same negotiated compression as the API router — the Prometheus
        // exposition text compresses well and scrapers send Accept-Encoding.
        .layer(CompressionLayer::new())
        .with_state(HealthState {
            ctx,
            metrics_handle,
            api_key,
            protect_metrics: config::protect_metrics(),
        })
}

/// Install the global Prometheus recorder and return a handle whose `render()`
/// produces the `/metrics` exposition text. `install_recorder()` only sets the
/// global recorder (no background HTTP listener — we serve the payload through
/// axum). Also registers HELP/TYPE descriptions for the metrics we emit.
pub(crate) fn init_metrics() -> PrometheusHandle {
    use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("install prometheus recorder");
    describe_counter!(
        "browser_headless_requests_total",
        "Total /summary requests, labelled by final HTTP status code"
    );
    describe_histogram!(
        "browser_headless_request_duration_seconds",
        Unit::Seconds,
        "End-to-end /summary handling time, labelled by outcome (ok/error)"
    );
    describe_gauge!(
        "browser_headless_requests_in_flight",
        "/summary requests currently being processed"
    );
    describe_counter!(
        "browser_headless_browser_respawns_total",
        "Times a crashed chromium instance was respawned"
    );
    describe_gauge!(
        "browser_headless_pool_size",
        "Configured number of chromium instances in the pool"
    );
    describe_gauge!(
        "browser_headless_pool_active_instances",
        "Chromium instances currently Active (not draining / respawning)"
    );
    describe_counter!(
        "browser_headless_recycles_total",
        "Voluntary instance recycles, labelled by reason (age / count)"
    );
    describe_histogram!(
        "browser_headless_checkout_wait_seconds",
        Unit::Seconds,
        "Time spent waiting for a free browser-pool slot"
    );
    describe_histogram!(
        "browser_headless_stage_duration_seconds",
        Unit::Seconds,
        "Per-capture stage duration (apply / collect / capture / format)"
    );
    handle
}

/// Liveness probe — the process is alive and the HTTP server is responding.
/// Does NOT verify the browser is healthy (use `/readyz` for that).
async fn healthz() -> &'static str {
    "ok"
}

/// Prometheus scrape endpoint. Open (no `X-Api-Key`) like the health probes,
/// unless `BROWSER_HEADLESS_PROTECT_METRICS` is set — then the same API key
/// is required. Health probes stay open so k8s liveness keeps working.
async fn metrics_endpoint(
    State(state): State<HealthState>,
    headers: HeaderMap,
) -> Result<Response, CaptureError> {
    if state.protect_metrics {
        check_api_key(state.api_key.as_ref(), &headers)?;
    }
    use axum::http::header::CONTENT_TYPE;
    Ok((
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics_handle.render(),
    )
        .into_response())
}

/// Readiness probe — sends `Browser.getVersion` over CDP to confirm an active
/// pool instance is reachable. Returns 503 when no instance is active (all
/// crashed / recycling) or the CDP socket is broken.
async fn readyz(State(state): State<HealthState>) -> Result<&'static str, CaptureError> {
    let browser = state.ctx.pool.any_active_browser().await.ok_or_else(|| {
        CaptureError::service_unavailable("no active browser instance (pool recycling/respawning)")
    })?;
    browser
        .version()
        .await
        .map_err(|e| CaptureError::service_unavailable(format!("browser CDP unreachable: {e}")))?;
    Ok("ok")
}

/// GET wrapper — query params drive a single page summary capture.
async fn summary_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SummaryQuery>,
) -> Result<Response, CaptureError> {
    check_auth(&state, &headers)?;
    check_rate(&state)?;
    let request_id = resolve_request_id(q.request_id.as_deref(), &headers);
    summary(state, q, request_id).await
}

/// POST wrapper — accepts the same parameter set as `summary_handler` but in
/// a JSON request body. Useful when the cookie / wait_for_request list / URL
/// would exceed practical query-string lengths.
async fn summary_handler_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(q): Json<SummaryQuery>,
) -> Result<Response, CaptureError> {
    check_auth(&state, &headers)?;
    check_rate(&state)?;
    let request_id = resolve_request_id(q.request_id.as_deref(), &headers);
    summary(state, q, request_id).await
}

/// POST `/summary/batch` — capture many URLs in one request. Returns a JSON
/// array of per-item results and never fails the whole batch on a single bad
/// URL. The envelope is always JSON: `content_only` items yield the compact
/// content object, others the full `WebPageStat`. `format=markdown` is ignored
/// (the batch is a JSON array). Concurrency is bounded by the pool, so a large
/// batch queues internally; the connection is held until all items finish.
async fn summary_batch_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<BatchQuery>,
) -> Result<Response, CaptureError> {
    check_auth(&state, &headers)?;
    check_rate(&state)?;
    if batch.urls.is_empty() {
        return Err(CaptureError::bad_request("`urls` must not be empty"));
    }
    let max = max_batch_urls();
    if batch.urls.len() > max {
        return Err(CaptureError::bad_request(format!(
            "too many urls ({}, max {max})",
            batch.urls.len()
        )));
    }

    let started = Instant::now();
    let total = batch.urls.len();
    let results = capture::run_batch(&state.ctx, batch).await;
    let failed = results.iter().filter(|r| r.status >= 400).count();
    tracing::info!(
        total,
        failed,
        duration_ms = started.elapsed().as_millis() as u64,
        "batch complete"
    );
    Ok(Json(BatchResponse {
        count: results.len(),
        results,
    })
    .into_response())
}

/// POST `/jobs` — enqueue an async capture; returns `{ id }` immediately.
async fn jobs_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(q): Json<SummaryQuery>,
) -> Result<Json<serde_json::Value>, CaptureError> {
    check_auth(&state, &headers)?;
    check_rate(&state)?;
    if q.url.is_empty() {
        return Err(CaptureError::bad_request("`url` must not be empty"));
    }
    let jobs = state
        .jobs
        .as_ref()
        .ok_or_else(|| CaptureError::service_unavailable("async jobs disabled"))?;
    let id = jobs.submit(state.ctx.clone(), q).await?;
    Ok(Json(serde_json::json!({ "id": id, "status": "queued" })))
}

/// GET `/jobs/:id` — poll an async job result.
async fn jobs_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<JobView>, CaptureError> {
    check_auth(&state, &headers)?;
    let jobs = state
        .jobs
        .as_ref()
        .ok_or_else(|| CaptureError::service_unavailable("async jobs disabled"))?;
    jobs.get(&id)
        .await?
        .map(Json)
        .ok_or_else(|| CaptureError::not_found(format!("job `{id}` not found or expired")))
}

/// Minimal OpenAPI 3 document for the public surface (not a full schema of
/// every `WebPageStat` field — that lives in the README).
async fn openapi_json() -> Response {
    let body = include_str!("openapi.json");
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

fn check_rate(state: &AppState) -> Result<(), CaptureError> {
    if state.rate_limiter.try_acquire() {
        Ok(())
    } else {
        Err(CaptureError::too_many_requests(
            "rate limit exceeded; retry shortly",
        ))
    }
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), CaptureError> {
    check_api_key(state.api_key.as_ref(), headers)
}

/// Shared-secret API key check. No-op when `api_key` is `None` (auth disabled).
/// When enabled, compares the `X-Api-Key` header against the configured key in
/// **constant time** (`subtle::ConstantTimeEq`).
fn check_api_key(api_key: Option<&Arc<String>>, headers: &HeaderMap) -> Result<(), CaptureError> {
    use subtle::ConstantTimeEq;
    let Some(required) = api_key else {
        return Ok(());
    };
    match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        Some(provided) if bool::from(provided.as_bytes().ct_eq(required.as_bytes())) => Ok(()),
        Some(_) => Err(CaptureError::unauthorized("invalid X-Api-Key")),
        None => Err(CaptureError::unauthorized("missing X-Api-Key header")),
    }
}

/// Resolve the effective request ID for this call:
/// 1. explicit `request_id` query/body param (caller-supplied for trace
///    correlation with their own systems)
/// 2. `X-Request-ID` header (proxy / gateway-supplied)
/// 3. auto-generated UUID v4
fn resolve_request_id(param: Option<&str>, headers: &HeaderMap) -> String {
    param
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Tracing + structured-logging wrapper around a single capture. Snapshots the
/// request shape before the capture so the end-of-request log can dump it.
async fn summary(
    state: AppState,
    q: SummaryQuery,
    request_id: String,
) -> Result<Response, CaptureError> {
    let span = tracing::info_span!(
        "summary",
        request_id = %request_id,
        url = %q.url,
    );
    let data_format = q.data_format;
    let response_format = q.format;
    let timeout_ms = q.timeout_ms;
    let capture_element = q.capture_element.clone();
    let wait_for_element = q.wait_for_element.clone();
    let wait_for_request_count = q.wait_for_request.len();
    let has_script = q.script.is_some();
    let has_cookie = q.cookie.is_some();
    let has_headers = !q.headers.is_empty();
    let disable_javascript = q.disable_javascript;
    let disable_cache = q.disable_cache;

    async move {
        let started = Instant::now();
        let result = summary_inner(state, q).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => tracing::info!(
                duration_ms,
                data_format = ?data_format,
                response_format = ?response_format,
                timeout_ms,
                capture_element = ?capture_element.as_deref(),
                wait_for_element = ?wait_for_element.as_deref(),
                wait_for_request_count,
                has_script,
                has_cookie,
                has_headers,
                disable_javascript,
                disable_cache,
                "request complete"
            ),
            Err(e) => tracing::warn!(
                duration_ms,
                status = e.status_u16(),
                error = %e.message,
                data_format = ?data_format,
                response_format = ?response_format,
                timeout_ms,
                capture_element = ?capture_element.as_deref(),
                wait_for_element = ?wait_for_element.as_deref(),
                wait_for_request_count,
                has_script,
                has_cookie,
                has_headers,
                disable_javascript,
                disable_cache,
                "request failed"
            ),
        }
        result
    }
    .instrument(span)
    .await
}

/// Render the HTTP envelope from a capture: the compact content object for
/// `content_only`, otherwise the full snapshot as JSON or markdown per
/// `format` / `lang`.
async fn summary_inner(state: AppState, q: SummaryQuery) -> Result<Response, CaptureError> {
    if q.url.is_empty() {
        return Err(CaptureError::bad_request("`url` must not be empty"));
    }
    let format = q.format;
    let lang = q.lang;
    match capture::capture_one(&state.ctx, q).await? {
        Captured::Content(body) => Ok(Json(body).into_response()),
        Captured::Full(stat) => Ok(match format {
            ResponseFormat::Json => Json(stat).into_response(),
            ResponseFormat::Markdown => (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/markdown; charset=utf-8",
                )],
                stat.to_markdown(lang),
            )
                .into_response(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn openapi_document_is_valid_json() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("openapi.json")).expect("openapi.json parses");
        assert!(v["paths"]["/summary"].is_object());
        assert!(v["paths"]["/jobs"].is_object());
        assert_eq!(v["openapi"], "3.0.3");
    }

    #[test]
    fn capture_error_maps_status_codes() {
        use axum::response::IntoResponse;
        let resp = CaptureError::too_many_requests("slow down").into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let resp = CaptureError::forbidden("blocked").into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Router-level integration without a real browser pool: only the static
    /// `/openapi.json` route is exercised (no CDP).
    #[tokio::test]
    async fn openapi_route_serves_json() {
        let app = Router::new().route("/openapi.json", get(openapi_json));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["paths"]["/healthz"].is_object());
    }
}
