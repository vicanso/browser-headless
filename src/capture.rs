//! HTTP-agnostic capture core.
//!
//! Holds the request params (`SummaryQuery`), the single + batch capture
//! engine, the result shapes, and URL/SSRF validation. It depends only on
//! `browser`, `pool`, `config`, and [`crate::error`] — **no axum** — so the
//! same engine can be driven by the HTTP layer ([`crate::http`]) or the queue
//! worker. [`capture_one`] is the unit of work both paths share.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
// Serialize is required so HTTP can enqueue SummaryQuery onto the Redis job stream.
use url::Url;

use crate::browser;
use crate::config::{
    self, checkout_wait_ms, clamp_settle_ms, clamp_timeout_ms, deadline_buffer_ms,
    default_timeout_ms,
};
use crate::error::{self, CaptureError};
use crate::pool::BrowserPool;
use crate::ssrf;

/// Shared, transport-agnostic capture context: the browser pool plus the SSRF
/// policy. Cheap to clone (an `Arc` + a bool). The HTTP `AppState` embeds one;
/// a worker would build its own.
#[derive(Clone)]
pub(crate) struct CaptureCtx {
    pub(crate) pool: Arc<BrowserPool>,
    pub(crate) allow_private_ips: bool,
}

/// How the entire response is delivered. Independent of `DataFormat`, which
/// controls only the `data` field's representation.
#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ResponseFormat {
    #[default]
    Json,
    Markdown,
}

/// Compact response for `content_only=true`. Deliberately tiny: just enough to
/// answer "did the page return content?" — the HTTP `status` of the final
/// document, the `final_url` after redirects (to catch unexpected landings),
/// the content size, and the content body itself.
#[derive(Serialize)]
pub(crate) struct ContentResponse {
    /// HTTP status of the final (post-redirect) main document. `0` when no
    /// Document response carried timing (fully-cached / unusual flow).
    pub(crate) status: u32,
    /// Final document URL after redirects. Equal to the requested URL when
    /// no redirect happened; compare against it to detect hijacks/landings.
    pub(crate) final_url: String,
    /// Unicode-scalar length of `data`. A near-zero count on a 200 is the
    /// signal for a blank / JS-skeleton page that failed to render content.
    pub(crate) char_count: usize,
    /// The page content in the caller's chosen `data_format` (`html`
    /// default / `text` / `markdown`).
    pub(crate) data: String,
}

/// All capture knobs. Deserialized from the HTTP query/body and, for
/// `/summary/batch`, flattened as the shared template; fields are
/// `pub(crate)` so the HTTP layer can snapshot them for request logging.
#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct SummaryQuery {
    /// Defaulted so `/summary/batch` can flatten the shared params without a
    /// top-level `url` (each URL comes from the batch's `urls` list). The
    /// single endpoint validates it's non-empty — an empty/missing url → 400.
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default = "default_timeout_ms")]
    pub(crate) timeout_ms: u64,
    #[serde(default)]
    pub(crate) screenshot: bool,
    /// Format of the `data` field. Pairs with — and is independent of —
    /// `response_format` (which controls the response envelope).
    #[serde(default)]
    pub(crate) data_format: browser::DataFormat,
    /// Only meaningful for `data_format=markdown`. Default on.
    pub(crate) normalize_custom_elements: Option<bool>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) device_scale_factor: Option<f64>,
    pub(crate) user_agent: Option<String>,
    pub(crate) accept_language: Option<String>,
    pub(crate) cookie: Option<String>,

    /// CPU slowdown multiplier (`1.0` = native, `4.0` = 4× slower for
    /// low-end-device simulation). Values ≤ 1.0 are ignored.
    pub(crate) cpu_throttle: Option<f64>,

    /// Enable mobile-style touch event emulation. Pair with a small
    /// viewport for full mobile simulation (max touch points = 5).
    #[serde(default)]
    pub(crate) touch: bool,

    /// IANA timezone identifier override (e.g. `Asia/Shanghai`).
    pub(crate) timezone: Option<String>,
    /// BCP 47 locale override (e.g. `zh-CN`).
    pub(crate) locale: Option<String>,
    /// Geolocation override — must provide BOTH latitude and longitude to
    /// take effect (single value alone is ignored). `accuracy` is meters and
    /// defaults to 100.
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    pub(crate) accuracy: Option<f64>,

    /// Extra HTTP request headers (Authorization, X-Api-Key, custom IDs, …).
    /// JSON: `{"X-Api-Key": "abc", "Authorization": "Bearer xyz"}`.
    /// GET (less ergonomic): `headers[X-Api-Key]=abc` style if supported by
    /// your client — POST is recommended for header-heavy requests.
    #[serde(default)]
    pub(crate) headers: HashMap<String, String>,

    /// URL substrings to block at the network layer (wrapped as CDP wildcard
    /// `*pat*`). Drops trackers / ads / fonts before they hit the wire.
    /// Repeat the key: `?block_urls=google-analytics&block_urls=doubleclick`.
    #[serde(default)]
    pub(crate) block_urls: Vec<String>,

    /// Block requests by resource type — finer-grained than `block_urls`.
    /// Recognized: `document` / `stylesheet` (`css`) / `image` (`img`) /
    /// `media` / `font` / `script` (`js`) / `xhr` / `fetch` / `websocket` /
    /// `manifest` / `ping` / `other`. Unknown names are silently ignored.
    /// Uses CDP `Fetch.enable` interception (heavier than `block_urls`).
    #[serde(default)]
    pub(crate) block_resource_types: Vec<String>,

    /// Force-bypass the browser's HTTP cache (disk + memory) for this request.
    /// All resources will be re-fetched from origin; `from_cache` stays false.
    #[serde(default)]
    pub(crate) disable_cache: bool,

    /// Disable JavaScript execution. Page renders as plain HTML/CSS only.
    /// Useful for static-content scraping (much faster). SPAs will appear
    /// blank/skeleton.
    #[serde(default)]
    pub(crate) disable_javascript: bool,

    /// Capture a PDF render of the page (`Page.printToPDF`) into
    /// `stat.pdf` (base64). Combined with custom viewport this gives a
    /// long-form snapshot of the full page.
    #[serde(default)]
    pub(crate) pdf: bool,

    /// Emit a HAR 1.2 archive into `stat.har` derived from observed
    /// resources. Importable into Chrome DevTools / `har-viewer`.
    #[serde(default)]
    pub(crate) har: bool,

    /// Capture structured DOM + layout snapshot into `stat.dom_snapshot`
    /// via CDP `DOMSnapshot.captureSnapshot`. Heavier than HTML but
    /// includes per-node computed styles and layout rects.
    #[serde(default)]
    pub(crate) save_dom_snapshot: bool,

    /// Collect Core Web Vitals (LCP / CLS / TBT / TTFB / long-task count)
    /// into `stat.web_vitals`. Adds a small JS observer set installed
    /// before navigation.
    #[serde(default)]
    pub(crate) web_vitals: bool,

    /// Capture V8 heap + DOM counters + CPU time breakdown
    /// (`script_duration_ms` / `layout_duration_ms` / `recalc_style_duration_ms`
    /// / `task_duration_ms`) into `stat.metrics` via CDP
    /// `Performance.getMetrics`. One extra CDP call, negligible overhead.
    /// CPU durations are gold for regression detection — e.g. "LCP
    /// unchanged but script_duration_ms jumped 30% across deploys".
    #[serde(default)]
    pub(crate) metrics: bool,

    /// Extract page metadata (title / description / canonical / robots /
    /// lang / viewport / charset / theme-color / OG / Twitter) into
    /// `stat.metadata`. One extra `page.evaluate` call.
    #[serde(default)]
    pub(crate) metadata: bool,

    /// Identify head-level render-blocking resources (sync stylesheets,
    /// sync scripts without async/defer/module) into
    /// `stat.render_blocking_resources`. One extra `page.evaluate` call.
    #[serde(default)]
    pub(crate) render_blocking: bool,

    /// Capture Service Worker registration state into
    /// `stat.service_worker` via a `page.evaluate` call.
    #[serde(default)]
    pub(crate) service_worker: bool,

    /// Subscribe to `Network.requestWillBeSent` and attach an `initiator`
    /// object (type / url / line_number) to each `stat.resources[]` entry.
    /// Adds one event stream subscription for the request lifetime.
    #[serde(default)]
    pub(crate) initiators: bool,

    /// Collect `console.log/info/warn/error/debug` output into
    /// `stat.console_messages`. Default off — most pages produce noisy
    /// console output (analytics, framework dev warnings, large object
    /// dumps) that bloats the response without adding value. Enable when
    /// debugging or doing console-driven audits.
    #[serde(default)]
    pub(crate) console_messages: bool,

    /// Audit each `<img>`: decoded natural size vs laid-out display size,
    /// lazy/eager loading, viewport overlap, missing alt, and
    /// (server-side joined) transferred bytes + waste ratio. Output:
    /// `stat.image_sizing`. One extra `page.evaluate` call — reads
    /// already-decoded browser state, no extra IO.
    #[serde(default)]
    pub(crate) image_sizing: bool,

    /// Install a pre-navigation `MutationObserver` to count DOM mutations
    /// (additions, removals, attribute changes) during the full render
    /// window. Output: `stat.dom_mutations`. Useful for diagnosing
    /// render-thrash regressions in SPAs. Typical overhead <5ms.
    #[serde(default)]
    pub(crate) dom_mutations: bool,

    /// Include the full per-resource list (`stat.resources[]`) in the
    /// response. Default off — `resource_summary` aggregates + scalar
    /// `total_size` / `resource_count` are still emitted for functional
    /// validation. Enable only when you need per-request forensics.
    #[serde(default)]
    pub(crate) resources: bool,

    /// Emit `stat.http_errors`: failed_4xx / failed_5xx lists, network
    /// failures (DNS / TLS / connection refused / blocked — pulled from
    /// CDP `Network.loadingFailed`), final URL after redirects, and
    /// redirect chain length. Built for periodic health checks where
    /// the caller needs a single "is this page broken / hijacked /
    /// redirected somewhere weird" signal without parsing
    /// `resources[]`. Costs one extra event subscription when on; zero
    /// when off.
    #[serde(default)]
    pub(crate) http_errors: bool,

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
    pub(crate) coverage: bool,

    /// Audit declared `<link rel="preconnect">` and
    /// `<link rel="dns-prefetch">` hints against actually-loaded
    /// third-party hosts. Populates
    /// `resource_summary.resource_hints` with the declared origins
    /// and a gap list (hot third parties missing a hint = avoidable
    /// 100-300ms of DNS+TLS overhead per origin). One extra
    /// `page.evaluate` over `<head>` (~5ms). OR-merged with
    /// `all_metrics`.
    #[serde(default)]
    pub(crate) resource_hints: bool,

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
    pub(crate) font_audit: bool,

    /// Fetch the page's cookie jar at snapshot time into `stat.cookies`
    /// (`Page.getCookies`, one CDP round-trip). Default off — high-volume
    /// scraping rarely reads it, and the empty list renders as an absent
    /// section. Distinct from the `cookie` INPUT param above (cookies SET
    /// on the request); this one REPORTS the jar after the page ran (e.g.
    /// for session-continuation flows). OR-merged with `all_metrics`.
    #[serde(default)]
    pub(crate) cookies: bool,

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
    pub(crate) security_scan: bool,

    /// Convenience switch that turns ON every **analytical** flag:
    /// `web_vitals` / `metrics` / `metadata` / `render_blocking` /
    /// `service_worker` / `initiators` / `console_messages` /
    /// `image_sizing` / `dom_mutations` / `resources` / `http_errors` /
    /// `resource_hints` / `font_audit` / `security_scan` / `cookies`.
    /// Equivalent to setting all fifteen manually — saves long query
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
    pub(crate) all_metrics: bool,

    /// Lean content-only mode. When `true`:
    /// - the content is returned in the caller's chosen `data_format`
    ///   (`html` default / `text` / `markdown`) — this flag does NOT force
    ///   markdown; select the body format with `data_format` as usual;
    /// - every analytical flag, `all_metrics`, binary captures
    ///   (`screenshot` / `pdf` / `har` / `save_dom_snapshot`) and
    ///   `coverage` are suppressed, and `resource_summary` is not built —
    ///   nothing but the content is collected;
    /// - the response envelope is ALWAYS a compact JSON object
    ///   `{ status, final_url, char_count, data }` (the `format` /
    ///   `lang` params are ignored).
    ///
    /// Built for cheap "just give me the content" / render-correctness
    /// checks: `status` + a non-trivial `char_count` (and `final_url` not
    /// landing somewhere unexpected) answers "did this page actually return
    /// content" without shipping the full `WebPageStat`. JS still executes,
    /// so SPA content is captured; a blank/skeleton page shows up as a
    /// near-empty `data`.
    #[serde(default)]
    pub(crate) content_only: bool,

    /// Optional client-supplied request ID for tracing/log correlation.
    /// If absent, falls back to the `X-Request-ID` header; if that's also
    /// missing, an auto-generated UUID v4 is used. Every tracing log
    /// emitted for this request carries this ID as a span field.
    pub(crate) request_id: Option<String>,

    /// Optional CSS selector — wait for this element to appear before
    /// snapshotting. Polled with exponential backoff up to `timeout_ms`.
    pub(crate) wait_for_element: Option<String>,

    /// Optional JS expression polled until it returns a truthy value (per JS
    /// semantics). Evaluated after `wait_for_element`. More flexible than a
    /// selector — express any business condition.
    pub(crate) wait_for_function: Option<String>,

    /// Optional stabilization period (ms) applied after every deterministic
    /// gate (`wait_for_request`, the lifecycle gate selected by
    /// `wait_until_load`, `wait_for_element`, `wait_for_function`) and
    /// just before `data` extraction. Lets late JS render / CSS animation
    /// finish for cases the explicit waits can't express.
    pub(crate) settle_ms: Option<u64>,

    /// Optional JavaScript to evaluate in the page right after `settle_ms`,
    /// just before `data` is captured. Run via CDP `Runtime.evaluate`;
    /// returned Promises are awaited. Use cases: dismiss cookie banners,
    /// trigger lazy-load, scroll, set localStorage. Errors abort the request.
    pub(crate) script: Option<String>,

    /// Optional CSS selector — return only the matched element's content
    /// in `data` (outerHTML for `format=html`, innerText-style for
    /// `format=text`, normalized HTML→markdown for `format=markdown`).
    pub(crate) capture_element: Option<String>,

    /// Zero or more URL substrings — block `collect_summary` until a
    /// response whose URL contains each substring has arrived (ALL semantics,
    /// 4xx/5xx → 502). Repeat the key: `?wait_for_request=a&wait_for_request=b`.
    #[serde(default)]
    pub(crate) wait_for_request: Vec<String>,

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
    pub(crate) wait_until_load: bool,

    /// Response envelope format. `json` (default) returns `application/json`
    /// with all `WebPageStat` fields. `markdown` returns `text/markdown`
    /// rendered for LLM consumption (resources as prose). Independent of
    /// `data_format` which controls only the `data` field's representation.
    #[serde(default)]
    pub(crate) format: ResponseFormat,

    /// Language used for the **markdown rendering** when
    /// `format=markdown`. `en` (default) emits English section headings +
    /// prose; `zh` emits Chinese. The JSON envelope is **never** translated
    /// — all field names, enum tag values, and other machine-readable
    /// strings stay English regardless of `lang`, so downstream code that
    /// branches on them keeps working across languages. Ignored when
    /// `format=json`.
    #[serde(default)]
    pub(crate) lang: browser::Lang,

    /// Named flag preset applied **before** individual flags (OR-merged, so
    /// an explicit `true` still wins). See [`CaptureProfile`].
    /// Backwards-compatible: absent / unknown → no preset.
    #[serde(default)]
    pub(crate) profile: Option<CaptureProfile>,
}

/// Named capture presets. Each expands to a set of analytical / content
/// flags so callers don't have to remember long query strings.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CaptureProfile {
    /// Lean content fetch: `content_only=true`, `wait_until_load=true`.
    Content,
    /// Static scrape: `content_only` + `disable_javascript` + `wait_until_load`.
    Scrape,
    /// Full analytical suite (same OR-mask as `all_metrics=true`), no binaries.
    Audit,
    /// Audit + explicit `coverage` (instrumentation cost).
    Lighthouse,
}

/// RAII guard for the in-flight gauge: increments on construction, decrements
/// on drop. Drop runs on the normal path, on early return, and on future
/// cancellation / panic, so the gauge can't leak a phantom in-flight request.
struct InFlightGuard;
impl InFlightGuard {
    fn new() -> Self {
        metrics::gauge!("browser_headless_requests_in_flight").increment(1.0);
        InFlightGuard
    }
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!("browser_headless_requests_in_flight").decrement(1.0);
    }
}

/// Result of one capture before HTTP-envelope rendering. `Content` is the
/// compact content-only object; `Full` is the complete page snapshot (boxed —
/// `WebPageStat` is large, so an unboxed variant would bloat every result).
pub(crate) enum Captured {
    Content(ContentResponse),
    Full(Box<browser::WebPageStat>),
}

/// Capture a single URL end to end — validate + SSRF-check, check out a pool
/// slot, run the capture under the hard deadline, and shape the result into
/// [`Captured`]. Records the per-capture metrics (in-flight gauge, request
/// counter, duration histogram) so `/summary` and `/summary/batch` items are
/// metered the same way. Errors are returned (never panicked) so a batch can
/// report per-item failures.
pub(crate) async fn capture_one(
    ctx: &CaptureCtx,
    q: SummaryQuery,
) -> Result<Captured, CaptureError> {
    capture_one_metered(ctx, q, checkout_wait_ms()).await
}

/// [`capture_one`] variant for QUEUED (async-job) captures: the pool-slot
/// wait is bounded by the job TTL instead of `checkout_wait_ms()`. The
/// interactive admission cut (default 30s → 503) exists to shed *synchronous*
/// callers who are actively waiting on the response; an enqueued job is
/// expected to sit out a busy pool — that's the entire point of the queue.
/// Shedding it records a terminal error for work that was never attempted.
pub(crate) async fn capture_one_queued(
    ctx: &CaptureCtx,
    q: SummaryQuery,
) -> Result<Captured, CaptureError> {
    let wait_ms = config::job_ttl().as_millis() as u64;
    capture_one_metered(ctx, q, wait_ms).await
}

async fn capture_one_metered(
    ctx: &CaptureCtx,
    q: SummaryQuery,
    checkout_wait: u64,
) -> Result<Captured, CaptureError> {
    let _in_flight = InFlightGuard::new();
    let started = Instant::now();
    let result = capture_one_unmetered(ctx, q, checkout_wait).await;
    let status = match &result {
        Ok(_) => 200u16,
        Err(e) => e.status_u16(),
    };
    let outcome = if result.is_ok() { "ok" } else { "error" };
    metrics::counter!("browser_headless_requests_total", "status" => status.to_string())
        .increment(1);
    metrics::histogram!("browser_headless_request_duration_seconds", "outcome" => outcome)
        .record(started.elapsed().as_secs_f64());
    result
}

async fn capture_one_unmetered(
    ctx: &CaptureCtx,
    mut q: SummaryQuery,
    checkout_wait: u64,
) -> Result<Captured, CaptureError> {
    apply_profile(&mut q);
    apply_clamps(&mut q);

    if config::disable_script() && q.script.is_some() {
        return Err(CaptureError::forbidden(
            "script parameter disabled by BROWSER_HEADLESS_DISABLE_SCRIPT",
        ));
    }

    // Cheap validation BEFORE permit acquisition — bad URLs shouldn't burn
    // queue slots. Reject non-http(s) schemes and private/loopback hosts
    // unless the operator explicitly opted out via env var.
    let parsed_url = validate_url(&q.url)?;
    if !ctx.allow_private_ips {
        check_ssrf(&parsed_url).await?;
    }

    // Check out a page slot on the least-loaded active instance. Blocks
    // (queues) while every instance is saturated; errors only when no
    // instance is active (all crashed / recycling). Held until function end —
    // covers the full capture lifecycle so concurrency stays bounded and the
    // instance's in-flight / served counters stay accurate.
    //
    // Admission control: bound the queue wait by `checkout_wait` (the
    // interactive `checkout_wait_ms()` for sync callers, the job TTL for
    // queued jobs — see `capture_one_queued`). Without it, demand above pool
    // capacity parks callers (and their futures) here indefinitely — under
    // load that is an unbounded backlog, not backpressure. On timeout we shed
    // the request with 503 so a saturated service fails fast. `0` disables
    // the bound (wait forever — original behaviour).
    let wait_ms = checkout_wait;
    let t_checkout = Instant::now();
    let checkout = {
        let acquire = ctx.pool.checkout();
        let acquired = if wait_ms == 0 {
            Ok(acquire.await)
        } else {
            tokio::time::timeout(Duration::from_millis(wait_ms), acquire).await
        };
        match acquired {
            Ok(Ok(checkout)) => checkout,
            Ok(Err(())) => {
                return Err(CaptureError::service_unavailable(
                    "browser pool unavailable; retry shortly",
                ));
            }
            Err(_elapsed) => {
                return Err(CaptureError::service_unavailable(format!(
                    "browser pool saturated; no slot within {wait_ms}ms, retry shortly"
                )));
            }
        }
    };
    metrics::histogram!("browser_headless_checkout_wait_seconds")
        .record(t_checkout.elapsed().as_secs_f64());

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

    // Lean content-only mode: keeps the caller's `data_format`, suppresses
    // every metric / binary capture, and reshapes the response (below).
    let lean = q.content_only;
    // Kept for the lean response's `final_url` fallback (q.url is moved into
    // the request below). `document_timing.url` is preferred when present.
    let requested_url = q.url.clone();
    let timeout_ms = q.timeout_ms;

    let req = browser::SummaryRequest {
        url: q.url,
        timeout: Duration::from_millis(timeout_ms),
        screenshot: q.screenshot && !lean,
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
        // Re-apply the SSRF blocklist to every in-page navigation / redirect
        // hop (the pre-flight `check_ssrf` only validates the initial URL).
        // Mirrors the pool-level policy: on unless private IPs are allowed.
        ssrf_guard: !ctx.allow_private_ips,
        disable_cache: q.disable_cache,
        wait_for_element: q.wait_for_element,
        wait_for_function: q.wait_for_function,
        settle: q.settle_ms.map(Duration::from_millis),
        script: q.script,
        capture_element: q.capture_element,
        // Content-only mode keeps the caller's chosen body format — it's a
        // "give me the content" switch, not a markdown switch.
        data_format: q.data_format,
        normalize_custom_elements: q.normalize_custom_elements.unwrap_or(true),
        disable_javascript: q.disable_javascript,
        content_only: lean,
        // Binary captures stay on explicit opt-in (intentionally NOT touched
        // by `all_metrics` — MB-scale payloads). `content_only` additionally
        // suppresses them — a content fetch needs the body, not megabytes of
        // PNG/PDF/HAR.
        pdf: q.pdf && !lean,
        har: q.har && !lean,
        save_dom_snapshot: q.save_dom_snapshot && !lean,
        // Analytical flags — `all_metrics` is a convenience OR-mask. Individual
        // `true` stays `true`; `content_only` short-circuits the whole block to
        // `false` (lean mode collects nothing but the content body).
        web_vitals: !lean && (q.web_vitals || q.all_metrics),
        metrics: !lean && (q.metrics || q.all_metrics),
        metadata: !lean && (q.metadata || q.all_metrics),
        render_blocking: !lean && (q.render_blocking || q.all_metrics),
        service_worker: !lean && (q.service_worker || q.all_metrics),
        initiators: !lean && (q.initiators || q.all_metrics),
        console_messages: !lean && (q.console_messages || q.all_metrics),
        image_sizing: !lean && (q.image_sizing || q.all_metrics),
        dom_mutations: !lean && (q.dom_mutations || q.all_metrics),
        resources: !lean && (q.resources || q.all_metrics),
        http_errors: !lean && (q.http_errors || q.all_metrics),
        // `coverage` is INTENTIONALLY NOT or-merged with `all_metrics` — it has
        // real V8 instrumentation cost. Keep it strictly per-request opt-in.
        coverage: q.coverage && !lean,
        resource_hints: !lean && (q.resource_hints || q.all_metrics),
        font_audit: !lean && (q.font_audit || q.all_metrics),
        security_scan: !lean && (q.security_scan || q.all_metrics),
        collect_cookies: !lean && (q.cookies || q.all_metrics),
    };

    // Hard upper bound on the whole capture lifecycle. `timeout_ms` already
    // caps page-internal waits; the buffer (default 10s, overridable via
    // BROWSER_HEADLESS_DEADLINE_BUFFER_MS) covers chromium overhead (context
    // create / page open / dispose). When this fires, the future is dropped,
    // the checkout is RAII-released, and we return 504.
    let buffer_ms = deadline_buffer_ms();
    let total_deadline = Duration::from_millis(timeout_ms) + Duration::from_millis(buffer_ms);
    let stat = tokio::time::timeout(
        total_deadline,
        browser::capture(checkout.browser(), checkout.default_user_agent(), req),
    )
    .await
    .map_err(|_| {
        CaptureError::gateway_timeout(format!(
            "total request deadline {}ms exceeded (timeout_ms={} + {}ms buffer)",
            total_deadline.as_millis(),
            timeout_ms,
            buffer_ms
        ))
    })?
    .map_err(error::from_browser)?;

    // Content-only mode → the compact content object; otherwise the full
    // snapshot (rendering to JSON / markdown is the HTTP layer's job). `status`
    // / `final_url` come from the always-on `document_timing` (final post-
    // redirect document); when absent (fully-cached / unusual flow) fall back
    // to status 0 and the requested URL. `char_count` counts Unicode scalars,
    // so a near-zero count flags a blank / skeleton page.
    if lean {
        let (status, final_url) = match &stat.document_timing {
            Some(dt) => (dt.status, dt.url.clone()),
            None => (0, requested_url),
        };
        return Ok(Captured::Content(ContentResponse {
            status,
            final_url,
            char_count: stat.data.chars().count(),
            data: stat.data,
        }));
    }

    Ok(Captured::Full(Box::new(stat)))
}

/// Expand a named [`CaptureProfile`] into individual flags (OR-merge).
fn apply_profile(q: &mut SummaryQuery) {
    let Some(profile) = q.profile else {
        return;
    };
    match profile {
        CaptureProfile::Content => {
            q.content_only = true;
            q.wait_until_load = true;
        }
        CaptureProfile::Scrape => {
            q.content_only = true;
            q.disable_javascript = true;
            q.wait_until_load = true;
        }
        CaptureProfile::Audit => {
            q.all_metrics = true;
        }
        CaptureProfile::Lighthouse => {
            q.all_metrics = true;
            q.coverage = true;
        }
    }
}

/// Clamp timeout / settle to process-wide ceilings so a single request can't
/// occupy a pool slot for hours.
fn apply_clamps(q: &mut SummaryQuery) {
    let t = clamp_timeout_ms(q.timeout_ms);
    if t != q.timeout_ms {
        tracing::warn!(
            requested = q.timeout_ms,
            clamped = t,
            "timeout_ms clamped by BROWSER_HEADLESS_MAX_TIMEOUT_MS"
        );
        q.timeout_ms = t;
    }
    if let Some(s) = q.settle_ms {
        let c = clamp_settle_ms(s);
        if c != s {
            tracing::warn!(
                requested = s,
                clamped = c,
                "settle_ms clamped by BROWSER_HEADLESS_MAX_SETTLE_MS"
            );
            q.settle_ms = Some(c);
        }
    }
}

/// Body for `POST /summary/batch`. `urls` are captured concurrently (bounded
/// by the pool); every other field is the shared capture template applied to
/// each URL — a flattened top-level `url`, if present, is ignored. All
/// `/summary` params work here; `content_only` + `data_format=markdown` is the
/// typical "validate a batch of pages" shape.
#[derive(Deserialize)]
pub(crate) struct BatchQuery {
    pub(crate) urls: Vec<String>,
    #[serde(flatten)]
    base: SummaryQuery,
}

/// One slot in a `/summary/batch` response. `status` is 200 on success or the
/// per-item error status; exactly one of `data` / `error` is populated.
#[derive(Serialize)]
pub(crate) struct BatchItem {
    /// Echoes the requested URL so callers can correlate by value, not index.
    url: String,
    pub(crate) status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl BatchItem {
    fn success(url: String, data: Result<serde_json::Value, serde_json::Error>) -> Self {
        match data {
            Ok(value) => BatchItem {
                url,
                status: 200,
                data: Some(value),
                error: None,
            },
            // Serializing our own stat should never fail; if it somehow does,
            // surface it as a per-item 500 rather than poisoning the batch.
            Err(e) => BatchItem {
                url,
                status: 500,
                data: None,
                error: Some(format!("serialize result: {e}")),
            },
        }
    }

    fn failure(url: String, status: u16, error: String) -> Self {
        BatchItem {
            url,
            status,
            data: None,
            error: Some(error),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct BatchResponse {
    pub(crate) count: usize,
    pub(crate) results: Vec<BatchItem>,
}

/// Drive a batch with bounded concurrency = pool capacity, preserving input
/// order in the output. Each URL inherits the shared `base` params (with its
/// own `url` substituted). Per-item failures become `BatchItem` errors rather
/// than failing the whole request.
pub(crate) async fn run_batch(ctx: &CaptureCtx, batch: BatchQuery) -> Vec<BatchItem> {
    let BatchQuery { urls, base } = batch;
    let concurrency = ctx.pool.capacity().min(urls.len()).max(1);
    let mut slots: Vec<Option<BatchItem>> = (0..urls.len()).map(|_| None).collect();

    let mut stream = futures::stream::iter(urls.into_iter().enumerate())
        .map(|(idx, url)| {
            let ctx = ctx.clone();
            let mut q = base.clone();
            q.url = url.clone();
            async move {
                let item = match capture_one(&ctx, q).await {
                    Ok(Captured::Content(content)) => {
                        BatchItem::success(url, serde_json::to_value(content))
                    }
                    Ok(Captured::Full(stat)) => BatchItem::success(url, serde_json::to_value(stat)),
                    Err(e) => BatchItem::failure(url, e.status_u16(), e.message),
                };
                (idx, item)
            }
        })
        .buffer_unordered(concurrency);

    while let Some((idx, item)) = stream.next().await {
        slots[idx] = Some(item);
    }
    slots
        .into_iter()
        .map(|slot| slot.expect("every batch slot filled"))
        .collect()
}

/// Parse + scheme-restrict the incoming URL. Reject anything other than
/// `http` / `https` (no `file:` / `chrome:` / `javascript:` / etc.).
fn validate_url(raw: &str) -> Result<Url, CaptureError> {
    let url =
        Url::parse(raw).map_err(|e| CaptureError::bad_request(format!("invalid URL: {e}")))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(CaptureError::bad_request(format!(
            "scheme `{other}` not allowed; only http/https"
        ))),
    }
}

/// SSRF guard for the **initial** URL — a cheap pre-flight before a pool slot
/// is checked out. Delegates host classification to [`crate::ssrf`] and maps
/// its error to a transport status (`NoHost` / `DnsFailed` → 400, blocked →
/// 403).
///
/// Redirects are covered separately: `browser`'s Fetch interception re-applies
/// the same blocklist to every navigation hop (so a public URL can't 3xx-bounce
/// into an internal host). For DNS-rebinding-grade threats, still combine with
/// an egress firewall / proxy.
async fn check_ssrf(url: &Url) -> Result<(), CaptureError> {
    use ssrf::SsrfError;
    ssrf::check_url(url).await.map_err(|e| match e {
        SsrfError::NoHost | SsrfError::DnsFailed(_) => CaptureError::bad_request(e.to_string()),
        SsrfError::Blocked(_) => CaptureError::forbidden(e.to_string()),
    })
}

/// Parse a standard HTTP `Cookie` header into `(name, value)` pairs.
/// Whitespace around `;` and `=` is trimmed; entries without `=` are skipped.
pub(crate) fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cookie_header_basic() {
        let pairs = parse_cookie_header("a=1; b=two; c=");
        assert_eq!(
            pairs,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "two".into()),
                ("c".into(), "".into()),
            ]
        );
    }

    #[test]
    fn parse_cookie_skips_malformed() {
        let pairs = parse_cookie_header("novalue; ok=1;;");
        assert_eq!(pairs, vec![("ok".into(), "1".into())]);
    }

    #[test]
    fn validate_url_rejects_file() {
        let err = validate_url("file:///etc/passwd").unwrap_err();
        assert_eq!(err.status_u16(), 400);
    }

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_url("https://example.com/path").is_ok());
    }

    #[test]
    fn apply_profile_content() {
        let mut q = SummaryQuery {
            url: "https://x".into(),
            profile: Some(CaptureProfile::Content),
            ..empty_query()
        };
        apply_profile(&mut q);
        assert!(q.content_only);
        assert!(q.wait_until_load);
    }

    #[test]
    fn apply_profile_lighthouse_enables_coverage() {
        let mut q = SummaryQuery {
            url: "https://x".into(),
            profile: Some(CaptureProfile::Lighthouse),
            ..empty_query()
        };
        apply_profile(&mut q);
        assert!(q.all_metrics);
        assert!(q.coverage);
    }

    fn empty_query() -> SummaryQuery {
        // Deserialize empty JSON so defaults match the HTTP path.
        serde_json::from_value(serde_json::json!({})).expect("defaults")
    }
}
