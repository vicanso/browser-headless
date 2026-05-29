mod browser;

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::extract::Query;
use chromiumoxide::Browser;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tracing::Instrument;
use url::{Host, Url};

#[derive(Clone)]
struct AppState {
    /// RwLock-wrapped so the supervisor can atomically swap in a fresh
    /// browser after a crash without breaking in-flight reads.
    browser: Arc<RwLock<BrowserHandle>>,
    /// Bounded-concurrency gate around `browser::capture`. Each in-flight
    /// request holds one permit for its full lifetime, so the active page
    /// count never exceeds the configured cap. Requests beyond the cap
    /// queue (no 429 — async wait is simpler client UX).
    permits: Arc<Semaphore>,
    /// When true, SSRF guard is disabled — private / loopback / link-local
    /// IPs are allowed. For internal deployments scraping LAN services.
    /// Default false. Set via `BROWSER_HEADLESS_ALLOW_PRIVATE_IPS=1`.
    allow_private_ips: bool,
    /// Optional shared-secret API key. `None` → auth disabled (default,
    /// preserves open access). `Some` → every `/summary` request must
    /// supply matching `X-Api-Key` header. Set via env
    /// `BROWSER_HEADLESS_API_KEY=<value>`.
    api_key: Option<Arc<String>>,
}

struct BrowserHandle {
    browser: Arc<Browser>,
    default_user_agent: Arc<String>,
    /// `false` while the supervisor is respawning a crashed browser.
    /// Handlers short-circuit with 503; `/readyz` reports unhealthy.
    healthy: bool,
}

/// How the entire `/summary` response is delivered. Independent of
/// `OutputFormat` which controls only the `data` field's representation.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ResponseFormat {
    #[default]
    Json,
    Markdown,
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,chromiumoxide::conn=off,chromiumoxide::handler=off".into()
            }),
        )
        .init();

    let worker_threads = num_cpus::get();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(worker_threads)
        .build()
        .unwrap_or_else(|e| panic!("failed to build tokio runtime: {}", e))
        .block_on(run());
}

async fn run() {
    let (browser_inst, default_ua, disconnect_rx) =
        browser::launch().await.expect("failed to launch browser");

    let max_pages = std::env::var("BROWSER_HEADLESS_MAX_PAGES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(8)
        .max(1);
    tracing::info!(max_pages, "concurrency limit");

    let allow_private_ips = std::env::var("BROWSER_HEADLESS_ALLOW_PRIVATE_IPS")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if allow_private_ips {
        tracing::warn!("SSRF guard disabled — private/loopback IPs allowed");
    }

    let api_key = std::env::var("BROWSER_HEADLESS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Arc::new);
    if api_key.is_some() {
        tracing::info!("API key auth enabled (X-Api-Key header required)");
    } else {
        tracing::warn!("API key auth disabled — /summary is open to anyone");
    }

    let browser_handle = Arc::new(RwLock::new(BrowserHandle {
        browser: Arc::new(browser_inst),
        default_user_agent: Arc::new(default_ua),
        healthy: true,
    }));

    // Supervisor: respawn browser on disconnect with exponential backoff.
    // Holds its own clone of the shared handle so the swap is atomic from
    // every handler's perspective.
    tokio::spawn(supervise_browser(browser_handle.clone(), disconnect_rx));

    let state = AppState {
        browser: browser_handle,
        permits: Arc::new(Semaphore::new(max_pages)),
        allow_private_ips,
        api_key,
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/summary", get(summary_handler).post(summary_handler_post))
        .with_state(state);

    let addr = "0.0.0.0:3000";
    let listener = TcpListener::bind(addr).await.expect("failed to bind");
    tracing::info!(
        "listening on {addr} with {} worker threads",
        num_cpus::get()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    tracing::info!("server shut down cleanly");
}

/// Supervisor: keeps the browser alive across crashes.
///
/// 1. Awaits the current browser's disconnect notification.
/// 2. Marks `healthy = false` so handlers short-circuit with 503 and
///    `/readyz` reports unhealthy.
/// 3. Re-runs `browser::launch` with exponential backoff (1s → 60s cap)
///    until it succeeds.
/// 4. Atomically swaps the new browser into shared state; `healthy = true`.
/// 5. Loops, waiting on the new browser's disconnect notification.
///
/// Runs forever — spawned as a detached tokio task.
async fn supervise_browser(
    state: Arc<RwLock<BrowserHandle>>,
    initial_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let mut disconnect_rx = initial_rx;
    loop {
        let _ = disconnect_rx.await;
        tracing::error!("browser disconnected; entering respawn loop");
        state.write().await.healthy = false;

        let mut backoff = Duration::from_secs(1);
        let new_rx = loop {
            match browser::launch().await {
                Ok((new_browser, new_ua, new_rx)) => {
                    let mut h = state.write().await;
                    h.browser = Arc::new(new_browser);
                    h.default_user_agent = Arc::new(new_ua);
                    h.healthy = true;
                    tracing::info!(
                        "browser respawned successfully (UA: {})",
                        h.default_user_agent
                    );
                    drop(h);
                    break new_rx;
                }
                Err(e) => {
                    tracing::error!(error = %e, retry_in = ?backoff, "browser respawn failed");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        };
        disconnect_rx = new_rx;
    }
}

/// Resolves when the process receives SIGTERM (`docker stop`, k8s pod
/// eviction, systemd `Term=signal`) or SIGINT (Ctrl-C in a foreground shell).
/// Used with `axum::serve(...).with_graceful_shutdown(...)` so the server
/// stops accepting new connections and waits for in-flight requests to
/// complete before returning. After return, AppState drops, `Arc<Browser>`
/// reaches refcount 0, and chromiumoxide's Drop tears down the chrome
/// subprocess.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM, initiating graceful shutdown"),
        _ = sigint.recv() => tracing::info!("received SIGINT, initiating graceful shutdown"),
    }
}

/// Liveness probe — the process is alive and the HTTP server is responding.
/// Does NOT verify the browser is healthy (use `/readyz` for that).
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness probe — sends `Browser.getVersion` over CDP to confirm the
/// chromium subprocess is still reachable. Returns 503 if the browser is
/// dead or the CDP socket is broken (operator should restart the pod).
async fn readyz(State(state): State<AppState>) -> Result<&'static str, (StatusCode, String)> {
    let (browser, healthy) = {
        let h = state.browser.read().await;
        (h.browser.clone(), h.healthy)
    };
    if !healthy {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "browser respawn in progress".to_string(),
        ));
    }
    browser.version().await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("browser CDP unreachable: {e}"),
        )
    })?;
    Ok("ok")
}

#[derive(Deserialize)]
struct SummaryQuery {
    url: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    screenshot: bool,
    /// Format of the `data` field. Pairs with — and is independent of —
    /// `response_format` (which controls the response envelope).
    #[serde(default)]
    data_format: browser::DataFormat,
    /// Only meaningful for `data_format=markdown`. Default on.
    normalize_custom_elements: Option<bool>,
    width: Option<u32>,
    height: Option<u32>,
    device_scale_factor: Option<f64>,
    user_agent: Option<String>,
    accept_language: Option<String>,
    cookie: Option<String>,

    /// CPU slowdown multiplier (`1.0` = native, `4.0` = 4× slower for
    /// low-end-device simulation). Values ≤ 1.0 are ignored.
    cpu_throttle: Option<f64>,

    /// Enable mobile-style touch event emulation. Pair with a small
    /// viewport for full mobile simulation (max touch points = 5).
    #[serde(default)]
    touch: bool,

    /// IANA timezone identifier override (e.g. `Asia/Shanghai`).
    timezone: Option<String>,
    /// BCP 47 locale override (e.g. `zh-CN`).
    locale: Option<String>,
    /// Geolocation override — must provide BOTH latitude and longitude to
    /// take effect (single value alone is ignored). `accuracy` is meters and
    /// defaults to 100.
    latitude: Option<f64>,
    longitude: Option<f64>,
    accuracy: Option<f64>,

    /// Extra HTTP request headers (Authorization, X-Api-Key, custom IDs, …).
    /// JSON: `{"X-Api-Key": "abc", "Authorization": "Bearer xyz"}`.
    /// GET (less ergonomic): `headers[X-Api-Key]=abc` style if supported by
    /// your client — POST is recommended for header-heavy requests.
    #[serde(default)]
    headers: HashMap<String, String>,

    /// URL substrings to block at the network layer (wrapped as CDP wildcard
    /// `*pat*`). Drops trackers / ads / fonts before they hit the wire.
    /// Repeat the key: `?block_urls=google-analytics&block_urls=doubleclick`.
    #[serde(default)]
    block_urls: Vec<String>,

    /// Block requests by resource type — finer-grained than `block_urls`.
    /// Recognized: `document` / `stylesheet` (`css`) / `image` (`img`) /
    /// `media` / `font` / `script` (`js`) / `xhr` / `fetch` / `websocket` /
    /// `manifest` / `ping` / `other`. Unknown names are silently ignored.
    /// Uses CDP `Fetch.enable` interception (heavier than `block_urls`).
    #[serde(default)]
    block_resource_types: Vec<String>,

    /// Force-bypass the browser's HTTP cache (disk + memory) for this request.
    /// All resources will be re-fetched from origin; `from_cache` stays false.
    #[serde(default)]
    disable_cache: bool,

    /// Disable JavaScript execution. Page renders as plain HTML/CSS only.
    /// Useful for static-content scraping (much faster). SPAs will appear
    /// blank/skeleton.
    #[serde(default)]
    disable_javascript: bool,

    /// Capture a PDF render of the page (`Page.printToPDF`) into
    /// `stat.pdf` (base64). Combined with custom viewport this gives a
    /// long-form snapshot of the full page.
    #[serde(default)]
    pdf: bool,

    /// Emit a HAR 1.2 archive into `stat.har` derived from observed
    /// resources. Importable into Chrome DevTools / `har-viewer`.
    #[serde(default)]
    har: bool,

    /// Capture structured DOM + layout snapshot into `stat.dom_snapshot`
    /// via CDP `DOMSnapshot.captureSnapshot`. Heavier than HTML but
    /// includes per-node computed styles and layout rects.
    #[serde(default)]
    save_dom_snapshot: bool,

    /// Collect Core Web Vitals (LCP / CLS / TBT / TTFB / long-task count)
    /// into `stat.web_vitals`. Adds a small JS observer set installed
    /// before navigation.
    #[serde(default)]
    web_vitals: bool,

    /// Capture V8 heap + DOM counters + CPU time breakdown
    /// (`script_duration_ms` / `layout_duration_ms` / `recalc_style_duration_ms`
    /// / `task_duration_ms`) into `stat.metrics` via CDP
    /// `Performance.getMetrics`. One extra CDP call, negligible overhead.
    /// CPU durations are gold for regression detection — e.g. "LCP
    /// unchanged but script_duration_ms jumped 30% across deploys".
    #[serde(default)]
    metrics: bool,

    /// Extract page metadata (title / description / canonical / robots /
    /// lang / viewport / charset / theme-color / OG / Twitter) into
    /// `stat.metadata`. One extra `page.evaluate` call.
    #[serde(default)]
    metadata: bool,

    /// Identify head-level render-blocking resources (sync stylesheets,
    /// sync scripts without async/defer/module) into
    /// `stat.render_blocking_resources`. One extra `page.evaluate` call.
    #[serde(default)]
    render_blocking: bool,

    /// Capture Service Worker registration state into
    /// `stat.service_worker` via a `page.evaluate` call.
    #[serde(default)]
    service_worker: bool,

    /// Subscribe to `Network.requestWillBeSent` and attach an `initiator`
    /// object (type / url / line_number) to each `stat.resources[]` entry.
    /// Adds one event stream subscription for the request lifetime.
    #[serde(default)]
    initiators: bool,

    /// Collect `console.log/info/warn/error/debug` output into
    /// `stat.console_messages`. Default off — most pages produce noisy
    /// console output (analytics, framework dev warnings, large object
    /// dumps) that bloats the response without adding value. Enable when
    /// debugging or doing console-driven audits.
    #[serde(default)]
    console_messages: bool,

    /// Audit each `<img>`: decoded natural size vs laid-out display size,
    /// lazy/eager loading, viewport overlap, missing alt, and
    /// (server-side joined) transferred bytes + waste ratio. Output:
    /// `stat.image_sizing`. One extra `page.evaluate` call — reads
    /// already-decoded browser state, no extra IO.
    #[serde(default)]
    image_sizing: bool,

    /// Install a pre-navigation `MutationObserver` to count DOM mutations
    /// (additions, removals, attribute changes) during the full render
    /// window. Output: `stat.dom_mutations`. Useful for diagnosing
    /// render-thrash regressions in SPAs. Typical overhead <5ms.
    #[serde(default)]
    dom_mutations: bool,

    /// Include the full per-resource list (`stat.resources[]`) in the
    /// response. Default off — `resource_summary` aggregates + scalar
    /// `total_size` / `resource_count` are still emitted for functional
    /// validation. Enable only when you need per-request forensics.
    #[serde(default)]
    resources: bool,

    /// Emit `stat.http_errors`: failed_4xx / failed_5xx lists, network
    /// failures (DNS / TLS / connection refused / blocked — pulled from
    /// CDP `Network.loadingFailed`), final URL after redirects, and
    /// redirect chain length. Built for periodic health checks where
    /// the caller needs a single "is this page broken / hijacked /
    /// redirected somewhere weird" signal without parsing
    /// `resources[]`. Costs one extra event subscription when on; zero
    /// when off.
    #[serde(default)]
    http_errors: bool,

    /// Capture CSS / JS coverage (Lighthouse "Reduce unused CSS / JS"
    /// feed) into `stat.coverage`. Enables CDP `Profiler` precise
    /// coverage + `CSS` rule-usage tracking pre-navigation; computes
    /// per-file used / unused bytes and a top-10 wasteful-files list.
    ///
    /// **Explicitly NOT enabled by `all_metrics=true`** — coverage
    /// instrumentation disables some V8 optimisations and keeps style-
    /// engine state for the full load, so it stays per-request opt-in
    /// even when the caller asks for "every analytical signal". Set
    /// `coverage=true` explicitly when you actually want it.
    #[serde(default)]
    coverage: bool,

    /// Audit declared `<link rel="preconnect">` and
    /// `<link rel="dns-prefetch">` hints against actually-loaded
    /// third-party hosts. Populates
    /// `resource_summary.resource_hints` with the declared origins
    /// and a gap list (hot third parties missing a hint = avoidable
    /// 100-300ms of DNS+TLS overhead per origin). One extra
    /// `page.evaluate` over `<head>` (~5ms). OR-merged with
    /// `all_metrics`.
    #[serde(default)]
    resource_hints: bool,

    /// Audit `@font-face` declarations + `document.fonts` FontFaceSet
    /// for FOIT (Flash of Invisible Text) risk. Populates
    /// `stat.font_audit` with `font-display` distribution, the
    /// `missing_swap[]` list (per-face FOIT offenders), preload
    /// coverage count, and a CORS blind-spot counter
    /// (`unreadable_stylesheets` — cross-origin sheets without
    /// `crossorigin` can't be inspected, so the audit is honest
    /// about what it couldn't see). One extra `page.evaluate`
    /// (~3–8ms depending on stylesheet count). OR-merged with
    /// `all_metrics`.
    #[serde(default)]
    font_audit: bool,

    /// Deep client-side security scan into `stat.security_scan`:
    /// Subresource-Integrity coverage on cross-origin `<script>` /
    /// `<link>`, `target=_blank` links missing `rel=noopener`
    /// (reverse-tabnabbing), form security (cleartext `action` / password
    /// fields on non-HTTPS pages), JS library + version fingerprint
    /// (jQuery / React / Vue / …), and passively-detected CORS
    /// `Access-Control-Allow-Origin: *`-with-credentials
    /// misconfigurations. One extra `page.evaluate` DOM walk (~2–5ms)
    /// plus a pure server-side CORS derive. OR-merged with `all_metrics`.
    #[serde(default)]
    security_scan: bool,

    /// Convenience switch that turns ON every **analytical** flag:
    /// `web_vitals` / `metrics` / `metadata` / `render_blocking` /
    /// `service_worker` / `initiators` / `console_messages` /
    /// `image_sizing` / `dom_mutations` / `resources` / `http_errors`.
    /// Equivalent to setting all eleven manually — saves long query
    /// strings in AI comparison / regression-audit workflows.
    ///
    /// **Does NOT enable binary captures** (`screenshot` / `pdf` / `har`
    /// / `save_dom_snapshot`) — those produce MB-scale payloads and are
    /// kept on explicit opt-in so a stray `all_metrics=true` can't
    /// accidentally balloon a response by 10×.
    ///
    /// **Does NOT enable `coverage`** either — coverage has real
    /// per-request instrumentation cost, so callers must set it
    /// explicitly even when using `all_metrics`.
    ///
    /// Combine semantics: OR with each individual flag (anything already
    /// `true` stays `true`). When `false` (default), individual flags
    /// behave exactly as before — fully backwards compatible.
    #[serde(default)]
    all_metrics: bool,

    /// Optional client-supplied request ID for tracing/log correlation.
    /// If absent, falls back to the `X-Request-ID` header; if that's also
    /// missing, an auto-generated UUID v4 is used. Every tracing log
    /// emitted for this request carries this ID as a span field.
    request_id: Option<String>,

    /// Optional CSS selector — wait for this element to appear before
    /// snapshotting. Polled with exponential backoff up to `timeout_ms`.
    wait_for_element: Option<String>,

    /// Optional JS expression polled until it returns a truthy value (per JS
    /// semantics). Evaluated after `wait_for_element`. More flexible than a
    /// selector — express any business condition.
    wait_for_function: Option<String>,

    /// Optional stabilization period (ms) applied after every deterministic
    /// gate (`wait_for_request`, the lifecycle gate selected by
    /// `wait_until_load`, `wait_for_element`, `wait_for_function`) and
    /// just before `data` extraction. Lets late JS render / CSS animation
    /// finish for cases the explicit waits can't express.
    settle_ms: Option<u64>,

    /// Optional JavaScript to evaluate in the page right after `settle_ms`,
    /// just before `data` is captured. Run via CDP `Runtime.evaluate`;
    /// returned Promises are awaited. Use cases: dismiss cookie banners,
    /// trigger lazy-load, scroll, set localStorage. Errors abort the request.
    script: Option<String>,

    /// Optional CSS selector — return only the matched element's content
    /// in `data` (outerHTML for `format=html`, innerText-style for
    /// `format=text`, normalized HTML→markdown for `format=markdown`).
    capture_element: Option<String>,

    /// Zero or more URL substrings — block `collect_summary` until a
    /// response whose URL contains each substring has arrived (ALL semantics,
    /// 4xx/5xx → 502). Repeat the key: `?wait_for_request=a&wait_for_request=b`.
    #[serde(default)]
    wait_for_request: Vec<String>,

    /// Wait-gate strategy for the collect stage:
    /// - `true`  → return shortly after the `load` (onload) lifecycle
    ///   event. Faster + more deterministic on pages with long-tail
    ///   analytics / websocket traffic that never quiesce. Pair with
    ///   `settle_ms` if late JS still needs to run before capture.
    /// - `false` (default) → return shortly after Chrome's `networkIdle`
    ///   lifecycle event (≥500ms with zero in-flight requests). Use this
    ///   when you need every late-firing response recorded in `resources`.
    ///
    /// Independent of `wait_for_element` / `wait_for_function` /
    /// `wait_for_request`, which run / match regardless of which gate is
    /// active. Caller must explicitly opt in — the gate choice is not
    /// inferred from the other wait flags.
    #[serde(default)]
    wait_until_load: bool,

    /// Response envelope format. `json` (default) returns `application/json`
    /// with all `WebPageStat` fields. `markdown` returns `text/markdown`
    /// rendered for LLM consumption (resources as prose). Independent of
    /// `data_format` which controls only the `data` field's representation.
    #[serde(default)]
    format: ResponseFormat,

    /// Language used for the **markdown rendering** when
    /// `format=markdown`. `en` (default) emits English section headings +
    /// prose; `zh` emits Chinese. The JSON envelope is **never** translated
    /// — all field names, enum tag values, and other machine-readable
    /// strings stay English regardless of `lang`, so downstream code that
    /// branches on them keeps working across languages. Ignored when
    /// `format=json`.
    #[serde(default)]
    lang: browser::Lang,
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

/// Shared-secret API key check. No-op when `state.api_key` is `None`
/// (auth disabled). When enabled, compares `X-Api-Key` header byte-for-byte
/// — not constant-time, so use a high-entropy key (32+ random bytes) where
/// timing attacks are infeasible.
fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let Some(required) = &state.api_key else {
        return Ok(());
    };
    match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        Some(provided) if provided == required.as_str() => Ok(()),
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
    // Snapshot request shape BEFORE moving `q` into `summary_inner` so the
    // end-of-request log can dump them without holding onto the whole query.
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

async fn summary_inner(state: AppState, q: SummaryQuery) -> Result<Response, (StatusCode, String)> {
    // Cheap validation BEFORE permit acquisition — bad URLs shouldn't burn
    // queue slots. Reject non-http(s) schemes and private/loopback hosts
    // unless the operator explicitly opted out via env var.
    let parsed_url = validate_url(&q.url)?;
    if !state.allow_private_ips {
        check_ssrf(&parsed_url).await?;
    }

    // Block (queue) until a page slot is free. Permit released at function
    // end — covers the full capture lifecycle so memory is bounded.
    let _permit = state.permits.acquire().await.expect("semaphore closed");

    let cookies = q
        .cookie
        .as_deref()
        .map(parse_cookie_header)
        .unwrap_or_default();

    let geolocation = match (q.latitude, q.longitude) {
        (Some(latitude), Some(longitude)) => Some(browser::Geolocation {
            latitude,
            longitude,
            accuracy: q.accuracy.unwrap_or(100.0),
        }),
        _ => None,
    };

    let req = browser::SummaryRequest {
        url: q.url,
        timeout: Duration::from_millis(q.timeout_ms),
        screenshot: q.screenshot,
        wait_for_request: q.wait_for_request,
        wait_until_load: q.wait_until_load,
        width: q.width,
        height: q.height,
        device_scale_factor: q.device_scale_factor,
        user_agent: q.user_agent,
        accept_language: q.accept_language,
        cookies,
        timezone: q.timezone,
        locale: q.locale,
        geolocation,
        cpu_throttle: q.cpu_throttle,
        touch: q.touch,
        headers: q.headers,
        block_urls: q.block_urls,
        block_resource_types: q.block_resource_types,
        disable_cache: q.disable_cache,
        wait_for_element: q.wait_for_element,
        wait_for_function: q.wait_for_function,
        settle: q.settle_ms.map(Duration::from_millis),
        script: q.script,
        capture_element: q.capture_element,
        data_format: q.data_format,
        normalize_custom_elements: q.normalize_custom_elements.unwrap_or(true),
        disable_javascript: q.disable_javascript,
        // Binary captures stay on explicit opt-in (intentionally NOT
        // touched by `all_metrics` — MB-scale payloads). Caller still
        // sets `pdf=true` / `screenshot=true` etc. when they really want
        // them.
        pdf: q.pdf,
        har: q.har,
        save_dom_snapshot: q.save_dom_snapshot,
        // Analytical flags — `all_metrics` is a convenience OR-mask over
        // every "indicator" feature. Individual `true` stays `true`
        // (already-set flags are unaffected); the only effect is
        // bringing untouched defaults UP to true when the master switch
        // is on. Backwards compatible: `all_metrics=false` (default)
        // leaves every flag exactly as the caller wrote it.
        web_vitals: q.web_vitals || q.all_metrics,
        metrics: q.metrics || q.all_metrics,
        metadata: q.metadata || q.all_metrics,
        render_blocking: q.render_blocking || q.all_metrics,
        service_worker: q.service_worker || q.all_metrics,
        initiators: q.initiators || q.all_metrics,
        console_messages: q.console_messages || q.all_metrics,
        image_sizing: q.image_sizing || q.all_metrics,
        dom_mutations: q.dom_mutations || q.all_metrics,
        resources: q.resources || q.all_metrics,
        http_errors: q.http_errors || q.all_metrics,
        // `coverage` is INTENTIONALLY NOT or-merged with `all_metrics`
        // — coverage has real V8 instrumentation cost (precise
        // coverage disables some optimisations) and CSS rule-usage
        // tracking. Keep it strictly per-request opt-in.
        coverage: q.coverage,
        resource_hints: q.resource_hints || q.all_metrics,
        font_audit: q.font_audit || q.all_metrics,
        security_scan: q.security_scan || q.all_metrics,
    };

    // Snapshot the current browser handle out from under the RwLock so the
    // supervisor can swap a new one in mid-capture without blocking us.
    let (browser_arc, default_ua) = {
        let h = state.browser.read().await;
        if !h.healthy {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "browser respawning; retry shortly".to_string(),
            ));
        }
        (h.browser.clone(), h.default_user_agent.clone())
    };

    // Hard upper bound on the whole capture lifecycle. `timeout_ms` already
    // caps page-internal waits; the +10s buffer covers chromium overhead
    // (context create / page open / dispose). When this fires, the future
    // is dropped, the permit is RAII-released, and we return 504. (A
    // mid-flight context may leak briefly; the browser GCs it eventually.)
    let total_deadline = Duration::from_millis(q.timeout_ms) + Duration::from_secs(10);
    let stat = tokio::time::timeout(
        total_deadline,
        browser::capture(&browser_arc, default_ua.as_str(), req),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "total request deadline {}ms exceeded (timeout_ms={} + 10s buffer)",
                total_deadline.as_millis(),
                q.timeout_ms
            ),
        )
    })?
    .map_err(browser_error)?;

    let response = match q.format {
        ResponseFormat::Json => Json(stat).into_response(),
        ResponseFormat::Markdown => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            stat.to_markdown(q.lang),
        )
            .into_response(),
    };
    Ok(response)
}

/// Parse + scheme-restrict the incoming URL. Reject anything other than
/// `http` / `https` (no `file:` / `chrome:` / `javascript:` / etc.).
fn validate_url(raw: &str) -> Result<Url, (StatusCode, String)> {
    let url =
        Url::parse(raw).map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid URL: {e}")))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err((
            StatusCode::BAD_REQUEST,
            format!("scheme `{other}` not allowed; only http/https"),
        )),
    }
}

/// SSRF guard. For literal-IP hosts checks the address directly; for
/// domains performs a DNS lookup and rejects if ANY resolved address is in
/// a blocked range. Covers IPv4 loopback / private / link-local /
/// broadcast / unspecified, and IPv6 equivalents + ULA + IPv4-mapped.
///
/// Limitation: doesn't defend against DNS rebinding (browser may resolve
/// the host again at navigation time and get a different IP). For high-
/// stakes deployments combine with egress firewall / proxy.
async fn check_ssrf(url: &Url) -> Result<(), (StatusCode, String)> {
    let host = url
        .host()
        .ok_or((StatusCode::BAD_REQUEST, "URL has no host".to_string()))?;
    let port = url.port_or_known_default().unwrap_or(80);

    match host {
        Host::Ipv4(ip) => {
            if is_blocked_ipv4(&ip) {
                return Err((StatusCode::FORBIDDEN, format!("blocked IPv4 host: {ip}")));
            }
        }
        Host::Ipv6(ip) => {
            if is_blocked_ipv6(&ip) {
                return Err((StatusCode::FORBIDDEN, format!("blocked IPv6 host: {ip}")));
            }
        }
        Host::Domain(name) => {
            let addrs = tokio::net::lookup_host(format!("{name}:{port}"))
                .await
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("dns resolution failed for `{name}`: {e}"),
                    )
                })?;
            for addr in addrs {
                let blocked = match addr.ip() {
                    std::net::IpAddr::V4(v4) => is_blocked_ipv4(&v4),
                    std::net::IpAddr::V6(v6) => is_blocked_ipv6(&v6),
                };
                if blocked {
                    return Err((
                        StatusCode::FORBIDDEN,
                        format!("`{name}` resolves to blocked IP {}", addr.ip()),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn is_blocked_ipv4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback()           // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254/16 (incl. cloud metadata 169.254.169.254)
        || ip.is_broadcast()    // 255.255.255.255
        || ip.is_unspecified()  // 0.0.0.0
        || ip.octets()[0] == 0 // 0.0.0.0/8 reserved
}

fn is_blocked_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    let segments = ip.segments();
    // fe80::/10 link-local
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // fc00::/7 ULA (unique local addresses)
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // IPv4-mapped (::ffff:x.x.x.x) — check embedded v4
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(&v4);
    }
    false
}

/// Parse a standard HTTP `Cookie` header into `(name, value)` pairs.
/// Whitespace around `;` and `=` is trimmed; entries without `=` are skipped.
fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
    header
        .split(';')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (name, value) = pair.split_once('=')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn browser_error(e: browser::Error) -> (StatusCode, String) {
    let status = match &e {
        browser::Error::NotFound(_) => StatusCode::NOT_FOUND,
        browser::Error::Timeout(_) => StatusCode::REQUEST_TIMEOUT,
        browser::Error::UpstreamFailure { .. } => StatusCode::BAD_GATEWAY,
        browser::Error::InvalidInput(_) => StatusCode::BAD_REQUEST,
        browser::Error::Cdp(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}
