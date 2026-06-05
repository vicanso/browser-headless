//! HTTP/axum layer: router, handlers, API-key auth, request-shape logging,
//! and Prometheus exposition. All transport concerns live here; the actual
//! page capture is delegated to [`crate::capture`].

use std::sync::Arc;
use std::time::Instant;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_extra::extract::Query;
use metrics_exporter_prometheus::PrometheusHandle;
use tracing::Instrument;

use crate::capture::{
    self, BatchQuery, BatchResponse, CaptureCtx, Captured, ResponseFormat, SummaryQuery,
};
use crate::config::max_batch_urls;

#[derive(Clone)]
pub(crate) struct AppState {
    /// Transport-agnostic capture context (browser pool + SSRF policy).
    pub(crate) ctx: CaptureCtx,
    /// Optional shared-secret API key. `None` → auth disabled (default, open).
    /// `Some` → every `/summary*` request must supply a matching `X-Api-Key`.
    pub(crate) api_key: Option<Arc<String>>,
    /// Prometheus exposition handle; `render()` produces the `/metrics` body.
    pub(crate) metrics_handle: PrometheusHandle,
}

/// Build the axum router with all routes wired to `state`.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .route("/summary", get(summary_handler).post(summary_handler_post))
        .route("/summary/batch", post(summary_batch_handler))
        .with_state(state)
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
    handle
}

/// Liveness probe — the process is alive and the HTTP server is responding.
/// Does NOT verify the browser is healthy (use `/readyz` for that).
async fn healthz() -> &'static str {
    "ok"
}

/// Prometheus scrape endpoint. Open (no `X-Api-Key`) like the health probes,
/// so an in-cluster Prometheus can scrape it without sharing the API key;
/// restrict at the network layer if the operational metrics are sensitive.
async fn metrics_endpoint(State(state): State<AppState>) -> Response {
    use axum::http::header::CONTENT_TYPE;
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics_handle.render(),
    )
        .into_response()
}

/// Readiness probe — sends `Browser.getVersion` over CDP to confirm an active
/// pool instance is reachable. Returns 503 when no instance is active (all
/// crashed / recycling) or the CDP socket is broken.
async fn readyz(State(state): State<AppState>) -> Result<&'static str, (StatusCode, String)> {
    let browser = state.ctx.pool.any_active_browser().await.ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "no active browser instance (pool recycling/respawning)".to_string(),
    ))?;
    browser.version().await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("browser CDP unreachable: {e}"),
        )
    })?;
    Ok("ok")
}

/// GET wrapper — query params drive a single page summary capture.
async fn summary_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SummaryQuery>,
) -> Result<Response, (StatusCode, String)> {
    check_auth(&state, &headers)?;
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
) -> Result<Response, (StatusCode, String)> {
    check_auth(&state, &headers)?;
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
) -> Result<Response, (StatusCode, String)> {
    check_auth(&state, &headers)?;
    if batch.urls.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "`urls` must not be empty".to_string(),
        ));
    }
    let max = max_batch_urls();
    if batch.urls.len() > max {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many urls ({}, max {max})", batch.urls.len()),
        ));
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

/// Shared-secret API key check. No-op when `state.api_key` is `None`
/// (auth disabled). When enabled, compares the `X-Api-Key` header against
/// the configured key in **constant time** (`subtle::ConstantTimeEq`), so a
/// correct prefix can't be recovered byte-by-byte from response-timing
/// differences. (Length still short-circuits, so use a fixed-length,
/// high-entropy key — 32+ random bytes.)
fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    use subtle::ConstantTimeEq;
    let Some(required) = &state.api_key else {
        return Ok(());
    };
    match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        Some(provided) if bool::from(provided.as_bytes().ct_eq(required.as_bytes())) => Ok(()),
        Some(_) => Err((StatusCode::UNAUTHORIZED, "invalid X-Api-Key".to_string())),
        None => Err((
            StatusCode::UNAUTHORIZED,
            "missing X-Api-Key header".to_string(),
        )),
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
) -> Result<Response, (StatusCode, String)> {
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
            Err((code, msg)) => tracing::warn!(
                duration_ms,
                status = code.as_u16(),
                error = %msg,
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
async fn summary_inner(state: AppState, q: SummaryQuery) -> Result<Response, (StatusCode, String)> {
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
