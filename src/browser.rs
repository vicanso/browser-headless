//! Browser orchestration layer. Wraps chromiumoxide CDP calls in an
//! HTTP-agnostic API with a dedicated `Error` enum that callers can map onto
//! their own response types.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::css::{
    EnableParams as CssEnableParams, EventStyleSheetAdded, RuleUsage as CssRuleUsage,
    StartRuleUsageTrackingParams, StopRuleUsageTrackingParams,
};
use chromiumoxide::cdp::browser_protocol::dom::EnableParams as DomEnableParams;
use chromiumoxide::cdp::browser_protocol::dom_snapshot::CaptureSnapshotParams;
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetCpuThrottlingRateParams, SetDeviceMetricsOverrideParams, SetGeolocationOverrideParams,
    SetLocaleOverrideParams, SetScriptExecutionDisabledParams, SetTimezoneOverrideParams,
    SetTouchEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams as FetchEnableParams, EventRequestPaused,
    FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::{
    BlockPattern, Cookie as CdpCookie, CookieParam, EnableParams as NetworkEnableParams,
    ErrorReason, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
    EventResponseReceived, GetCookiesParams, Headers, Initiator as CdpInitiator,
    ResourceTiming as CdpResourceTiming, ResourceType, SecurityDetails as CdpSecurityDetails,
    SetBlockedUrLsParams, SetCacheDisabledParams, SetCookiesParams, SetExtraHttpHeadersParams,
    SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotParams,
    EnableParams as PageEnableParams, EventLifecycleEvent, PrintToPdfParams,
    SetLifecycleEventsEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::performance::{
    EnableParams as PerformanceEnableParams, GetMetricsParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams, DisposeBrowserContextParams,
};
use chromiumoxide::cdp::js_protocol::profiler::{
    EnableParams as ProfilerEnableParams, ScriptCoverage, StartPreciseCoverageParams,
    StopPreciseCoverageParams, TakePreciseCoverageParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    EnableParams as RuntimeEnableParams, EventConsoleApiCalled, EventExceptionThrown,
};
use chromiumoxide::{Browser, BrowserConfig};
use futures::stream::{self, BoxStream, StreamExt};
use htmd::HtmlToMarkdown;
use serde::{Deserialize, Serialize};

pub use chromiumoxide::Page;

#[derive(Debug)]
pub enum Error {
    Cdp(String),
    NotFound(String),
    Timeout(String),
    UpstreamFailure { status: i64, url: String },
    InvalidInput(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Cdp(s) => write!(f, "{s}"),
            Error::NotFound(s) => write!(f, "not found: {s}"),
            Error::Timeout(s) => write!(f, "timed out waiting for {s}"),
            Error::UpstreamFailure { status, url } => {
                write!(f, "upstream request failed (status {status}): {url}")
            }
            Error::InvalidInput(s) => write!(f, "invalid input: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<chromiumoxide::error::CdpError> for Error {
    fn from(e: chromiumoxide::error::CdpError) -> Self {
        Error::Cdp(e.to_string())
    }
}

const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const DEFAULT_SCALE: f64 = 1.0;

/// Default `User-Agent` advertised by every page unless the caller
/// overrides via `SummaryRequest.user_agent`. Pinned to a recent
/// stable Chrome string on macOS so requests don't carry the literal
/// `HeadlessChrome` token — many WAFs (Cloudflare, Akamai, custom
/// enterprise gateways) blanket-block that token, returning 403 / no
/// CORS headers and breaking otherwise-valid scrapes.
///
/// The actual Chromium binary version is logged once at launch
/// (`chromium launched` event) so admins can still tell what's
/// running; this constant only controls what pages see over the wire.
/// If you need a different default per deployment, override on every
/// request via the `user_agent` query param.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";

/// Custom tracing target for the per-resource diagnostic logs
/// (`request_will_be_sent` / `response_received` / `loading_failed` /
/// `resource has status=0 ...`). Independent target means callers can
/// enable just these without flooding stdout with every other debug
/// log in the crate. Enable with:
///
/// ```sh
/// RUST_LOG=warn,browser_headless::diag::resource=debug
/// ```
///
/// — sets default to `warn` (quiet) and only this target to `debug`.
/// Default crate-wide log filter still picks up the `warn!` walker
/// because warn-level passes any `warn`-or-higher filter.
const DIAG_RESOURCE: &str = "browser_headless::diag::resource";

/// Launch Chromium, spawn the watcher task that keeps the CDP connection
/// alive, and return the browser handle, its default user-agent string,
/// and a `oneshot::Receiver` that fires once when the CDP stream ends
/// (chromium subprocess died, websocket dropped, etc.). The supervisor
/// uses that signal to respawn — see `main::supervise_browser`.
pub async fn launch() -> Result<(Browser, String, tokio::sync::oneshot::Receiver<()>), Error> {
    // --no-sandbox: required when running as non-root inside a container
    //   without user-namespace mapping (the default Docker config).
    //   Wired through chromiumoxide's `.no_sandbox()` builder rather than
    //   `.arg("--no-sandbox")` — `.arg(...)` stores the string verbatim
    //   and `ArgsBuilder` prepends `--`, so a literal `--no-sandbox`
    //   becomes `----no-sandbox` (quadruple-dash, ignored by Chrome) and
    //   the sandbox stays enforced. `.no_sandbox()` sets the internal
    //   flag, which chromiumoxide expands to the correctly-formatted
    //   `--no-sandbox --disable-setuid-sandbox` pair.
    // disable-dev-shm-usage: containers ship a 64MB /dev/shm by default,
    //   which Chrome fills up under load and then crashes; switch to /tmp.
    //   Pass WITHOUT leading dashes for the same reason — chromiumoxide
    //   prepends `--` itself.
    // Safe defaults for an internal scraping service. Remove them if
    // exposing this to untrusted URLs in a multi-tenant context.
    // Record the explicit Chromium flags we requested. chromiumoxide doesn't
    // expose the final flattened argv (DEFAULT_ARGS + sandbox flags + our
    // extras + headless flags) once the subprocess is spawned, so this log
    // is "what WE asked for" not "what Chromium received". Still essential
    // for diagnosing future arg-mangling bugs (cf. the `--no-sandbox` →
    // `----no-sandbox` quadruple-dash trap we just fixed): if a future
    // refactor passes a flag a different way, you can compare intent vs.
    // observed behavior side-by-side from the logs.
    let requested_args: Vec<&'static str> = vec!["disable-dev-shm-usage"];
    tracing::info!(
        no_sandbox = true,
        args = ?requested_args,
        "launching chromium",
    );

    let config = BrowserConfig::builder()
        .no_sandbox()
        .arg("disable-dev-shm-usage")
        .build()
        .map_err(|e| Error::InvalidInput(format!("browser config: {e}")))?;
    let (browser, mut handler) = Browser::launch(config).await?;

    let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();

    // Watcher: forwards transient errors as debug logs, fires the disconnect
    // notification when the handler stream ends, then exits.
    tokio::spawn(async move {
        let mut notify_tx = Some(notify_tx);
        loop {
            match handler.next().await {
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "browser handler error");
                }
                None => {
                    tracing::error!("browser handler stream ended; signalling supervisor");
                    if let Some(tx) = notify_tx.take() {
                        let _ = tx.send(());
                    }
                    return;
                }
            }
        }
    });

    let version = browser.version().await?;
    // Full version snapshot — `product` carries the Chromium binary
    // identity (e.g. "HeadlessChrome/119.0.6045.105"), `revision` is
    // the build hash, `js_version` is the V8 version. Together they
    // pin down exactly what binary started up, which is what you want
    // when a CDP feature behaves differently in two environments.
    tracing::info!(
        product = %version.product,
        revision = %version.revision,
        protocol_version = %version.protocol_version,
        js_version = %version.js_version,
        binary_user_agent = %version.user_agent,
        default_user_agent = DEFAULT_USER_AGENT,
        "chromium launched",
    );
    // Return the pinned default UA (NOT the raw Chromium binary UA).
    // The binary UA contains the literal `HeadlessChrome` token, which
    // is the single most common reason WAFs reject scrapes. Pages see
    // `DEFAULT_USER_AGENT` unless the caller overrides per-request.
    Ok((browser, DEFAULT_USER_AGENT.to_string(), notify_rx))
}

pub async fn apply_viewport(
    page: &Page,
    width: Option<u32>,
    height: Option<u32>,
    device_scale_factor: Option<f64>,
) -> Result<(), Error> {
    if width.is_none() && height.is_none() && device_scale_factor.is_none() {
        return Ok(());
    }
    let params = SetDeviceMetricsOverrideParams::builder()
        .width(width.unwrap_or(DEFAULT_WIDTH) as i64)
        .height(height.unwrap_or(DEFAULT_HEIGHT) as i64)
        .device_scale_factor(device_scale_factor.unwrap_or(DEFAULT_SCALE))
        .mobile(false)
        .build()
        .map_err(|e| Error::InvalidInput(format!("viewport: {e}")))?;
    page.execute(params).await?;
    Ok(())
}

/// Map a user-supplied resource type name (case-insensitive, common
/// aliases) to a CDP `ResourceType`. Unknown names return `None` and are
/// silently skipped by the caller.
fn parse_resource_type(s: &str) -> Option<ResourceType> {
    match s.to_ascii_lowercase().as_str() {
        "document" | "html" => Some(ResourceType::Document),
        "stylesheet" | "css" => Some(ResourceType::Stylesheet),
        "image" | "img" => Some(ResourceType::Image),
        "media" | "video" | "audio" => Some(ResourceType::Media),
        "font" => Some(ResourceType::Font),
        "script" | "js" | "javascript" => Some(ResourceType::Script),
        "xhr" => Some(ResourceType::Xhr),
        "fetch" => Some(ResourceType::Fetch),
        "websocket" | "ws" => Some(ResourceType::WebSocket),
        "manifest" => Some(ResourceType::Manifest),
        "ping" => Some(ResourceType::Ping),
        "other" => Some(ResourceType::Other),
        _ => None,
    }
}

/// Enable `Fetch` interception scoped to the given resource types and
/// spawn a drain task that fails matching requests, continues the rest.
/// Returns a `oneshot::Sender` — drop it (or send) to signal the task to
/// exit cleanly when the page is done.
pub async fn apply_block_resource_types(
    page: &Page,
    types: &[String],
) -> Result<Option<tokio::sync::oneshot::Sender<()>>, Error> {
    let blocked: Vec<ResourceType> = types
        .iter()
        .filter_map(|s| parse_resource_type(s))
        .collect();
    if blocked.is_empty() {
        return Ok(None);
    }

    let patterns: Vec<RequestPattern> = blocked
        .iter()
        .map(|rt| RequestPattern {
            url_pattern: Some("*".to_string()),
            resource_type: Some(rt.clone()),
            request_stage: None,
        })
        .collect();

    page.execute(FetchEnableParams {
        patterns: Some(patterns),
        handle_auth_requests: Some(false),
    })
    .await?;

    let mut stream = page.event_listener::<EventRequestPaused>().await?;
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let page_clone = page.clone();
    let blocked_set = blocked;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                ev = stream.next() => match ev {
                    Some(event) => {
                        let request_id = event.request_id.clone();
                        let drop_it = blocked_set.iter().any(|t| t == &event.resource_type);
                        let result = if drop_it {
                            page_clone
                                .execute(FailRequestParams::new(
                                    request_id,
                                    ErrorReason::BlockedByClient,
                                ))
                                .await
                                .map(|_| ())
                        } else {
                            page_clone
                                .execute(ContinueRequestParams::new(request_id))
                                .await
                                .map(|_| ())
                        };
                        if let Err(e) = result {
                            tracing::debug!(error = %e, "fetch handler error");
                        }
                    }
                    None => break,
                }
            }
        }
    });

    Ok(Some(stop_tx))
}

/// Enable touch event emulation (`ontouchstart` etc. become dispatchable;
/// `navigator.maxTouchPoints` reports >0). Pair with a small viewport
/// override for full mobile simulation. Max touch points fixed at 5.
pub async fn apply_touch_emulation(page: &Page, enabled: bool) -> Result<(), Error> {
    if !enabled {
        return Ok(());
    }
    page.execute(SetTouchEmulationEnabledParams {
        enabled: true,
        max_touch_points: Some(5),
    })
    .await?;
    Ok(())
}

/// Apply CPU throttling. `rate` is a slowdown multiplier (1.0 = native,
/// 4.0 = 4× slower). Values ≤ 1.0 are skipped (CDP doesn't speed up).
pub async fn apply_cpu_throttle(page: &Page, rate: Option<f64>) -> Result<(), Error> {
    if let Some(r) = rate
        && r > 1.0
    {
        page.execute(SetCpuThrottlingRateParams::new(r)).await?;
    }
    Ok(())
}

/// Override the page's timezone via IANA tz id. Must be set before
/// navigation to affect initial JS Date / Intl behavior.
pub async fn apply_timezone(page: &Page, timezone: Option<&str>) -> Result<(), Error> {
    if let Some(tz) = timezone {
        page.execute(SetTimezoneOverrideParams::new(tz)).await?;
    }
    Ok(())
}

/// Override the page's locale (BCP 47). Affects `navigator.language` and
/// Intl APIs. Must be set before navigation.
pub async fn apply_locale(page: &Page, locale: Option<&str>) -> Result<(), Error> {
    if let Some(loc) = locale {
        page.execute(SetLocaleOverrideParams {
            locale: Some(loc.to_string()),
        })
        .await?;
    }
    Ok(())
}

/// Override the page's geolocation. Setting this also auto-grants the
/// `geolocation` permission. Must be set before any JS that reads it.
pub async fn apply_geolocation(page: &Page, geo: Option<&Geolocation>) -> Result<(), Error> {
    if let Some(g) = geo {
        page.execute(SetGeolocationOverrideParams {
            latitude: Some(g.latitude),
            longitude: Some(g.longitude),
            accuracy: Some(g.accuracy),
            ..Default::default()
        })
        .await?;
    }
    Ok(())
}

/// JS that walks `document.head` and projects the standard SEO / social /
/// rendering metadata into a flat JSON object matching `PageMetadata`.
const PAGE_METADATA_JS: &str = r#"
(function() {
  function meta(sel) {
    const el = document.head.querySelector(sel);
    return el ? (el.getAttribute('content') || '').trim() || null : null;
  }
  function link(rel) {
    const el = document.head.querySelector(`link[rel="${rel}"]`);
    return el ? (el.getAttribute('href') || '').trim() || null : null;
  }
  const og = {};
  document.head.querySelectorAll('meta[property^="og:"]').forEach((el) => {
    const k = (el.getAttribute('property') || '').slice(3);
    const v = (el.getAttribute('content') || '').trim();
    if (k && v) og[k] = v;
  });
  const twitter = {};
  document.head.querySelectorAll('meta[name^="twitter:"]').forEach((el) => {
    const k = (el.getAttribute('name') || '').slice(8);
    const v = (el.getAttribute('content') || '').trim();
    if (k && v) twitter[k] = v;
  });
  const charsetEl = document.head.querySelector('meta[charset]');
  return {
    title: document.title || '',
    description: meta('meta[name="description"]'),
    canonical: link('canonical'),
    robots: meta('meta[name="robots"]'),
    lang: document.documentElement.lang || null,
    viewport: meta('meta[name="viewport"]'),
    charset: charsetEl ? charsetEl.getAttribute('charset') : null,
    theme_color: meta('meta[name="theme-color"]'),
    og: og,
    twitter: twitter,
  };
})()
"#;

pub async fn collect_page_metadata(page: &Page) -> Result<PageMetadata, Error> {
    let eval = page.evaluate(PAGE_METADATA_JS).await?;
    eval.into_value()
        .map_err(|e| Error::Cdp(format!("metadata decode: {e}")))
}

/// JS that queries the page's Service Worker registration.
const SERVICE_WORKER_JS: &str = r#"
(async () => {
  if (!('serviceWorker' in navigator)) {
    return { controlled: false, scope: null, active_script: null, waiting: false, installing: false };
  }
  try {
    const reg = await navigator.serviceWorker.getRegistration();
    return {
      controlled: !!navigator.serviceWorker.controller,
      scope: reg ? reg.scope : null,
      active_script: reg && reg.active ? reg.active.scriptURL : null,
      waiting: reg ? !!reg.waiting : false,
      installing: reg ? !!reg.installing : false,
    };
  } catch (e) {
    return { controlled: false, scope: null, active_script: null, waiting: false, installing: false };
  }
})()
"#;

pub async fn collect_service_worker(page: &Page) -> Result<ServiceWorkerStatus, Error> {
    let eval = page.evaluate(SERVICE_WORKER_JS).await?;
    eval.into_value()
        .map_err(|e| Error::Cdp(format!("service_worker decode: {e}")))
}

/// Project CDP `SecurityDetails` into our compact `TlsInfo`. `days_remaining`
/// is computed at capture time from wall clock; negative if expired.
/// `host` identifies which origin the certificate was observed on.
/// `remote_ip` / `remote_port` come from the same Network.responseReceived —
/// the IP the browser actually connected to (already resolved, no extra DNS
/// lookup needed on our side; safe from SSRF surface since it's observation).
fn extract_tls_info(
    sd: &CdpSecurityDetails,
    host: String,
    remote_ip: Option<String>,
    remote_port: Option<u16>,
) -> TlsInfo {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as f64)
        .unwrap_or(0.0);
    // CDP's valid_to is `TimeSinceEpoch` (seconds since unix epoch as f64).
    let valid_to = *sd.valid_to.inner();
    let valid_from = *sd.valid_from.inner();
    let days_remaining = ((valid_to - now_secs) / 86400.0).floor() as i64;
    TlsInfo {
        host,
        remote_ip,
        remote_port,
        protocol: sd.protocol.clone(),
        cipher: sd.cipher.clone(),
        key_exchange: if sd.key_exchange.is_empty() {
            None
        } else {
            Some(sd.key_exchange.clone())
        },
        subject_name: sd.subject_name.clone(),
        issuer: sd.issuer.clone(),
        valid_from,
        valid_to,
        days_remaining,
        san_list: sd.san_list.clone(),
    }
}

/// Format the resolved remote IP suffix for a TLS section header.
/// Returns `" → 198.51.100.42"` (port hidden if standard 443), or empty
/// string when CDP didn't report an IP (cached responses, local schemes).
fn format_remote_ip(tls: &TlsInfo) -> String {
    match (&tls.remote_ip, tls.remote_port) {
        (Some(ip), Some(443)) | (Some(ip), None) => format!(" → `{ip}`"),
        (Some(ip), Some(p)) => format!(" → `{ip}:{p}`"),
        (None, _) => String::new(),
    }
}

/// Format certificate expiry as human-readable string with severity markers.
/// Negative days = already expired. <30 days = warning. Used by markdown
/// rendering for both the main-document section and the per-host table.
fn format_tls_expiry(days_remaining: i64) -> String {
    if days_remaining < 0 {
        format!("**EXPIRED {} days ago**", -days_remaining)
    } else if days_remaining < 30 {
        format!("**expires in {days_remaining} days ⚠️**")
    } else {
        format!("expires in {days_remaining} days")
    }
}

/// Extract the security-relevant headers from a response's header map.
/// Returns None when no security headers are present.
/// Build the `SecurityAudit` scorecard from already-captured data.
/// `headers` is the curated main-document header map (`None` when no
/// Document response was ever observed — same shape as
/// `WebPageStat.security_headers`). `cookies` is the page's full jar.
///
/// Pure derive — runs in O(headers + cookies), no IO.
fn build_security_audit(
    headers: Option<&HashMap<String, String>>,
    cookies: &[Cookie],
) -> SecurityAudit {
    let mut h = SecurityHeadersCheck::default();
    let has = |name: &str| -> bool { headers.is_some_and(|m| m.contains_key(name)) };
    h.hsts = has("Strict-Transport-Security");
    h.csp = has("Content-Security-Policy");
    h.csp_report_only = has("Content-Security-Policy-Report-Only");
    h.x_frame_options = has("X-Frame-Options");
    h.x_content_type_options = has("X-Content-Type-Options");
    h.referrer_policy = has("Referrer-Policy");
    h.permissions_policy = has("Permissions-Policy");
    h.coop = has("Cross-Origin-Opener-Policy");
    h.coep = has("Cross-Origin-Embedder-Policy");

    let mut missing = Vec::new();
    for &name in CORE_SECURITY_HEADERS {
        if !has(name) {
            missing.push(name.to_string());
        }
    }
    h.present_count = (CORE_SECURITY_HEADERS.len() - missing.len()) as u32;
    h.missing = missing;

    let mut c = CookieSecurityCheck {
        total: cookies.len() as u32,
        ..Default::default()
    };
    let mut header_bytes: u64 = 0;
    for cookie in cookies {
        if cookie.secure {
            c.secure += 1;
        }
        if cookie.http_only {
            c.http_only += 1;
        }
        if let Some(ss) = cookie.same_site.as_deref() {
            c.same_site_set += 1;
            // SameSite=None without Secure is rejected by modern browsers
            // outright. Case-insensitive match — CDP returns "None" but
            // origin headers may differ.
            if ss.eq_ignore_ascii_case("None") && !cookie.secure {
                c.same_site_none_without_secure += 1;
            }
        }
        // Estimate the on-the-wire `Cookie:` header contribution:
        // `name=value` for the cookie, plus `"; "` separator between
        // cookies. Subtract the trailing separator at the end.
        header_bytes += cookie.name.len() as u64;
        header_bytes += 1; // '='
        header_bytes += cookie.value.len() as u64;
        header_bytes += 2; // "; "
    }
    if header_bytes >= 2 {
        header_bytes -= 2; // drop the trailing "; " after the last cookie
    }
    c.header_bytes = header_bytes;

    SecurityAudit {
        headers: h,
        cookies: c,
    }
}

fn extract_security_headers(headers: &Headers) -> Option<HashMap<String, String>> {
    let obj = headers.inner().as_object()?;
    let mut out = HashMap::new();
    for &name in SECURITY_HEADER_NAMES {
        for (k, v) in obj {
            if k.eq_ignore_ascii_case(name) {
                if let Some(s) = v.as_str() {
                    out.insert(name.to_string(), s.to_string());
                }
                break;
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Case-insensitive single-header lookup. Used for per-resource header
/// extraction (e.g. `content-encoding`) where we want one value, not the
/// curated security-headers map.
fn lookup_header(headers: &Headers, name: &str) -> Option<String> {
    let obj = headers.inner().as_object()?;
    for (k, v) in obj {
        if k.eq_ignore_ascii_case(name) {
            return v.as_str().map(str::to_string);
        }
    }
    None
}

/// Project chromiumoxide's `Initiator` into our compact `RequestInitiator`.
/// Stack trace and column info are dropped.
fn map_initiator(init: &CdpInitiator) -> RequestInitiator {
    // Type debug-formats to PascalCase variant name; lowercase for stable
    // comparison strings.
    let t = format!("{:?}", init.r#type).to_lowercase();
    RequestInitiator {
        r#type: t,
        url: init.url.clone(),
        line_number: init.line_number.map(|n| n.max(0.0) as u32),
    }
}

/// JS that scans `document.head` for render-blocking resources.
/// Render-blocking criteria:
/// - `<link rel="stylesheet">` without a non-matching media query
/// - `<script>` without `async` / `defer` / `type="module"` (both external
///   and inline)
const RENDER_BLOCKING_JS: &str = r#"
(function() {
  const blockers = [];
  for (const el of document.head.children) {
    const tag = el.tagName.toLowerCase();
    if (tag === 'link') {
      const rel = (el.rel || '').toLowerCase();
      if (rel !== 'stylesheet' || el.disabled) continue;
      const media = (el.media || 'all').toLowerCase();
      // Conservative: only `print`-only or `not screen` queries don't block.
      // Anything matching screen (default, 'all', 'screen', or screen+...) blocks.
      const printOnly = media === 'print' || media.indexOf('print') === 0 && media.indexOf('screen') < 0;
      if (printOnly) continue;
      blockers.push({ tag: 'link', url: el.href || '', why: 'sync stylesheet' });
    } else if (tag === 'script') {
      if (el.async || el.defer || (el.type || '').toLowerCase() === 'module') continue;
      if (el.src) {
        blockers.push({ tag: 'script', url: el.src, why: 'no async/defer' });
      } else if ((el.textContent || '').trim()) {
        blockers.push({ tag: 'script', url: '(inline)', why: 'inline blocking script' });
      }
    }
  }
  return blockers;
})()
"#;

pub async fn collect_render_blocking(page: &Page) -> Result<Vec<RenderBlocker>, Error> {
    let eval = page.evaluate(RENDER_BLOCKING_JS).await?;
    eval.into_value()
        .map_err(|e| Error::Cdp(format!("render_blocking decode: {e}")))
}

/// Scan `<head>` for declared `<link rel="preconnect">` and
/// `<link rel="dns-prefetch">` hints. Returns each hint's resolved
/// href (the browser normalizes relative/protocol-relative URLs for
/// us). The Rust side later parses these into hosts to compare
/// against actually-loaded third-party domains.
///
/// We don't filter `disabled` here (matches real browser behavior:
/// disabled `<link>`s don't trigger preconnect/dns-prefetch).
const RESOURCE_HINTS_JS: &str = r#"
(function() {
  const preconnect = [];
  const dnsPrefetch = [];
  for (const el of document.head.querySelectorAll('link[rel]')) {
    if (el.disabled) continue;
    const rel = (el.rel || '').toLowerCase();
    const href = el.href || '';
    if (!href) continue;
    if (rel.indexOf('preconnect') >= 0) {
      preconnect.push(href);
    } else if (rel.indexOf('dns-prefetch') >= 0) {
      dnsPrefetch.push(href);
    }
  }
  return { preconnect, dns_prefetch: dnsPrefetch };
})()
"#;

#[derive(Debug, Clone, Default, Deserialize)]
struct RawResourceHints {
    preconnect: Vec<String>,
    dns_prefetch: Vec<String>,
}

pub async fn collect_resource_hints(page: &Page) -> Result<RawResourceHintsPublic, Error> {
    let eval = page.evaluate(RESOURCE_HINTS_JS).await?;
    let raw: RawResourceHints = eval
        .into_value()
        .map_err(|e| Error::Cdp(format!("resource_hints decode: {e}")))?;
    Ok(RawResourceHintsPublic {
        preconnect: raw.preconnect,
        dns_prefetch: raw.dns_prefetch,
    })
}

/// Exported shape for the raw resource-hint scrape — kept distinct from
/// the final `ResourceHints` (which carries the computed `gap` derived
/// against the resource list). Pub-by-necessity so the format stage
/// can `try_join!` the call.
#[derive(Debug, Clone, Default)]
pub struct RawResourceHintsPublic {
    pub preconnect: Vec<String>,
    pub dns_prefetch: Vec<String>,
}

/// Build the final `ResourceHints` with computed `gap`. Compares
/// already-built `top_third_party_domains` (computed earlier in
/// `build_resource_summary`) against the declared hint origins.
/// Hosts that account for `< gap_floor_bytes` bytes are skipped to
/// avoid noisy gaps from tiny one-off fetches (telemetry beacons,
/// 1x1 pixels) where the preconnect cost can outweigh the gain.
fn build_resource_hints(
    raw: RawResourceHintsPublic,
    top_third_party_domains: &[DomainBytes],
) -> ResourceHints {
    const GAP_FLOOR_BYTES: u64 = 4096;
    // Normalize declared hint URLs → host strings for comparison.
    // We accept either full URL (preconnect: `https://cdn.example.com`)
    // or host-only DNS hints. Stripping the scheme + trailing path
    // keeps the comparison uniform.
    let to_host = |s: &String| -> Option<String> {
        url::Url::parse(s)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .or_else(|| {
                // Bare-host form (no scheme) — chrome's `link.href`
                // usually resolves to absolute, but be defensive.
                let trimmed = s.trim().trim_start_matches("//");
                trimmed
                    .split('/')
                    .next()
                    .filter(|h| !h.is_empty() && h.contains('.'))
                    .map(str::to_string)
            })
    };
    let declared: std::collections::HashSet<String> = raw
        .preconnect
        .iter()
        .chain(raw.dns_prefetch.iter())
        .filter_map(to_host)
        .collect();

    let gap: Vec<ResourceHintGap> = top_third_party_domains
        .iter()
        .filter(|d| d.bytes >= GAP_FLOOR_BYTES)
        .filter(|d| !declared.contains(&d.host))
        .map(|d| ResourceHintGap {
            host: d.host.clone(),
            bytes: d.bytes,
            count: d.count,
        })
        .collect();

    ResourceHints {
        declared_preconnect: raw.preconnect,
        declared_dns_prefetch: raw.dns_prefetch,
        gap,
    }
}

/// Walk `document.styleSheets` for `@font-face` rules + read the
/// `document.fonts` FontFaceSet for counts. Two outputs the AI cares
/// about most: distribution of `font-display` values, and the per-face
/// "missing swap" list (FOIT risk — invisible-text-during-load).
///
/// CORS caveat: cross-origin stylesheets without `crossorigin` + CORS
/// headers raise on `sheet.cssRules` access. We catch and increment
/// `unreadable_stylesheets` so the audit is honest about its blind
/// spots. Same-origin + properly-CORS'd third-party stylesheets work.
///
/// The `src` URL extraction is regex-based on the `src:` descriptor's
/// string form because CSS Typed OM doesn't expose `format()` /
/// `url()` cleanly. We take the first `url(...)` token only — most
/// `@font-face` blocks list one URL + optional `local()` fallbacks
/// and several format hints, but the first URL is the one the
/// browser would actually fetch when the local fonts are absent.
const FONT_AUDIT_JS: &str = r#"
(function() {
  const out = {
    font_count: 0,
    loaded_count: 0,
    display_distribution: {},
    missing_swap: [],
    declared_preload_count: 0,
    unreadable_stylesheets: 0,
  };

  // Walk every @font-face rule across all readable stylesheets.
  // CSSFontFaceRule.style is a CSSStyleDeclaration; `font-display` may
  // be missing (defaults to `auto` per CSS spec → FOIT risk).
  const fontFaces = [];
  for (const sheet of document.styleSheets) {
    let rules;
    try {
      rules = sheet.cssRules;
    } catch (e) {
      // Cross-origin without CORS — the browser blocks rule access as
      // a side-channel mitigation. Count and skip so the caller knows
      // the audit isn't complete.
      out.unreadable_stylesheets++;
      continue;
    }
    if (!rules) continue;
    for (const rule of rules) {
      // CSSRule.FONT_FACE_RULE === 5 in the legacy enum; instanceof
      // covers modern engines where the numeric type is deprecated.
      const isFontFace =
        (typeof CSSFontFaceRule !== 'undefined' && rule instanceof CSSFontFaceRule) ||
        rule.type === 5;
      if (!isFontFace) continue;
      const style = rule.style;
      // Strip wrapping quotes from family — declared as
      //   font-family: 'Inter';
      // the property value comes back as `'Inter'` literally.
      const familyRaw = style.getPropertyValue('font-family') || '';
      const family = familyRaw.replace(/^\s*['"]|['"]\s*$/g, '').trim();
      const src = style.getPropertyValue('src') || '';
      const display = (style.getPropertyValue('font-display') || '').trim();
      fontFaces.push({ family, src, display });
    }
  }

  // Tally `font-display` values + flag faces likely to cause FOIT.
  // `swap` (text visible immediately with fallback, swap when ready)
  // and `optional` (use only if cached) both avoid FOIT. `auto`
  // (= the default) and `block` are the FOIT-prone cases. `fallback`
  // has a 100ms invisible window — short but still a FOIT, flag it.
  for (const ff of fontFaces) {
    const d = ff.display || 'auto';
    out.display_distribution[d] = (out.display_distribution[d] || 0) + 1;
    if (d !== 'swap' && d !== 'optional') {
      // Extract first url() from `src` — string parse rather than CSS
      // Typed OM because the latter doesn't expose `format()` tokens.
      let sourceUrl = null;
      const m = ff.src.match(/url\(\s*['"]?([^'")]+)['"]?\s*\)/);
      if (m && m[1]) {
        try {
          sourceUrl = new URL(m[1], document.baseURI).href;
        } catch (e) {
          sourceUrl = m[1];
        }
      }
      out.missing_swap.push({
        family: ff.family,
        source_url: sourceUrl,
        display: d || null,
      });
    }
  }

  // FontFaceSet counts. `document.fonts` iterates every FontFace the
  // engine knows about (including the ones from @font-face above,
  // plus any added programmatically via `document.fonts.add(...)`).
  if (document.fonts) {
    document.fonts.forEach((f) => {
      out.font_count++;
      if (f.status === 'loaded') out.loaded_count++;
    });
  }

  // Preload coverage — single scalar count of `<link rel="preload"
  // as="font">`. Per-font preload gap analysis is intentionally NOT
  // done: preloading every font is itself an anti-pattern (eager
  // render-blocking fetch), and the "preload only above-fold fonts"
  // suggestion requires viewport text analysis. Keep this honest:
  // tell the caller whether they bothered to preload anything,
  // leave deciding which fonts deserve preload to the AI.
  for (const link of document.head.querySelectorAll('link[rel="preload"][as="font"]')) {
    if (!link.disabled && link.href) out.declared_preload_count++;
  }

  return out;
})()
"#;

#[derive(Debug, Clone, Default, Deserialize)]
struct RawFontAudit {
    font_count: u32,
    loaded_count: u32,
    display_distribution: HashMap<String, u32>,
    missing_swap: Vec<FontIssue>,
    declared_preload_count: u32,
    unreadable_stylesheets: u32,
}

pub async fn collect_font_audit(page: &Page) -> Result<FontAudit, Error> {
    let eval = page.evaluate(FONT_AUDIT_JS).await?;
    let raw: RawFontAudit = eval
        .into_value()
        .map_err(|e| Error::Cdp(format!("font_audit decode: {e}")))?;
    Ok(FontAudit {
        font_count: raw.font_count,
        loaded_count: raw.loaded_count,
        display_distribution: raw.display_distribution,
        missing_swap: raw.missing_swap,
        declared_preload_count: raw.declared_preload_count,
        unreadable_stylesheets: raw.unreadable_stylesheets,
    })
}

/// Per-image sizing collector. Reads `naturalWidth/Height` (decoded pixel
/// dimensions, populated by the browser as a side effect of decoding the
/// image bytes — no extra IO on our side) and the laid-out display
/// dimensions via `getBoundingClientRect()`. Both are already-computed
/// browser state, so the cost is just a DOM walk — typically <2ms for 100
/// images. `currentSrc` reflects the URL the browser actually picked from
/// any `srcset`/`sizes` candidates.
///
/// Filters:
/// - Skips `naturalWidth === 0 && !loading==='lazy'`: image failed to load.
///   Lazy images outside viewport legitimately have 0×0 naturals and are
///   kept with `loaded: false` so the caller can audit them separately.
/// - `in_viewport` is computed against `innerWidth/Height` so the caller
///   can distinguish above-the-fold waste (high-impact) from below-the-fold.
const IMAGE_SIZING_JS: &str = r#"
(function() {
  const vw = window.innerWidth || 0;
  const vh = window.innerHeight || 0;
  // DPR reflects whatever the page is actually rendering at — including
  // any device_scale_factor we set via Emulation.setDeviceMetricsOverride.
  // Used server-side to compute the effective device-pixel display size
  // (the number of pixels the browser actually needs to draw crisply),
  // which is what natural dimensions should be compared against.
  const dpr = window.devicePixelRatio || 1;
  const out = [];
  for (const img of document.images) {
    const rect = img.getBoundingClientRect();
    const nw = img.naturalWidth | 0;
    const nh = img.naturalHeight | 0;
    const loaded = nw > 0 && nh > 0;
    const loading = img.loading || 'eager';
    // Hidden images (display:none / visibility:hidden / zero size) are noise.
    // Lazy images below the fold are legitimately not-yet-loaded; report
    // them but mark loaded=false so they don't pollute "waste" metrics.
    if (!loaded && loading !== 'lazy') continue;
    const inViewport =
      rect.bottom > 0 && rect.top < vh && rect.right > 0 && rect.left < vw;
    // Lighthouse "image" four-pack inputs — we capture presence of
    // the relevant <img> attributes here, then the server-side
    // enrichment buckets them into ImageAudit.
    //   `width` / `height` attrs — when both are missing the image
    //     contributes to CLS (browser can't reserve layout space
    //     until decode). We check the IDL property `attributes` so a
    //     CSS-set width counts as "no attribute" (it's what causes
    //     CLS, not the visual size).
    //   `srcset` — without it there's no responsive variant; the
    //     same source ships to every viewport / DPR.
    const hasWidthAttr = img.hasAttribute('width');
    const hasHeightAttr = img.hasAttribute('height');
    const hasSrcset = img.hasAttribute('srcset') && (img.srcset || '').trim().length > 0;
    out.push({
      url: img.currentSrc || img.src || '',
      natural_width: nw,
      natural_height: nh,
      display_width: Math.round(rect.width),
      display_height: Math.round(rect.height),
      device_pixel_ratio: dpr,
      loaded,
      loading,
      decoding: img.decoding || 'auto',
      in_viewport: inViewport,
      alt_missing: !img.alt,
      has_width_attr: hasWidthAttr,
      has_height_attr: hasHeightAttr,
      has_srcset: hasSrcset,
    });
  }
  return out;
})()
"#;

pub async fn collect_image_sizing(page: &Page) -> Result<Vec<ImageSizing>, Error> {
    let eval = page.evaluate(IMAGE_SIZING_JS).await?;
    eval.into_value()
        .map_err(|e| Error::Cdp(format!("image_sizing decode: {e}")))
}

/// Group `cls_entries.sources` by element identity (selector) and rank by
/// total contributed shift. Each shift entry's `value` is split equally
/// across its source elements — CDP doesn't report per-source impact, and
/// equal split keeps fractions adding up to ~100% of total CLS for
/// intuitive reading.
fn aggregate_cls_sources(vitals: &mut WebVitals) {
    use std::collections::HashMap;
    // Per-selector accumulator: (total_shift, shift_count, max_distance_px).
    // Max-distance is tracked so the AI can give concrete "reserve N px"
    // suggestions instead of just "fix CLS" — the biggest single jump is
    // the lower bound for `min-height` / layout reservation.
    let mut by_selector: HashMap<String, (f64, u32, f64)> = HashMap::new();

    for entry in &vitals.cls_entries {
        if entry.sources.is_empty() {
            continue;
        }
        let per_source = entry.value / entry.sources.len() as f64;
        for src in &entry.sources {
            let selector = format_cls_selector(src);
            let agg = by_selector.entry(selector).or_insert((0.0, 0, 0.0));
            agg.0 += per_source;
            agg.1 += 1;
            if src.distance_px > agg.2 {
                agg.2 = src.distance_px;
            }
        }
    }

    // Use total CLS as denominator; fall back to small epsilon to avoid /0.
    let total_cls = if vitals.cls > 0.0 { vitals.cls } else { 1.0 };
    let mut sources: Vec<ClsTopSource> = by_selector
        .into_iter()
        .map(
            |(selector, (total_shift, shift_count, max_distance_px))| ClsTopSource {
                selector,
                total_shift,
                fraction: total_shift / total_cls,
                shift_count,
                max_distance_px,
            },
        )
        .collect();
    sources.sort_by(|a, b| {
        b.total_shift
            .partial_cmp(&a.total_shift)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    vitals.cls_top_sources = sources;
}

fn format_cls_selector(src: &ClsSourceElement) -> String {
    if !src.id.is_empty() {
        format!("{}#{}", src.tag, src.id)
    } else if !src.class.is_empty() {
        // Use first class token for selector uniqueness; full class string
        // is preserved in raw ClsEntry.sources for detailed inspection.
        let first = src.class.split_whitespace().next().unwrap_or("");
        format!("{}.{}", src.tag, first)
    } else {
        src.tag.clone()
    }
}

/// Aggregate raw LoAF entries into the `WebVitals` summary fields. Computes
/// scalar totals (`loaf_count`, `loaf_total_blocking_duration`) and groups
/// the per-script breakdowns across all frames by `source_url`, returning
/// the top 5 offenders ranked by `total_duration_ms` desc.
///
/// `source_url` is the grouping key — same code from the same script will
/// typically appear in many LoAF frames (e.g. a re-rendering callback),
/// and grouping makes it visible. Empty source URLs (inline / eval /
/// browser-internal) are kept as a single `""` bucket so they don't get
/// blamed on every script equally.
fn aggregate_loaf(vitals: &mut WebVitals, raw: &[LoafRawEntry]) {
    use std::collections::HashMap;

    vitals.loaf_count = raw.len() as u32;
    vitals.loaf_total_blocking_duration = raw.iter().map(|e| e.blocking_duration).sum();

    let mut by_source: HashMap<String, LoafOffender> = HashMap::new();
    for entry in raw {
        for s in &entry.scripts {
            let agg = by_source
                .entry(s.source_url.clone())
                .or_insert_with(|| LoafOffender {
                    source_url: s.source_url.clone(),
                    source_function_name: s.source_function_name.clone(),
                    invoker_type: s.invoker_type.clone(),
                    ..Default::default()
                });
            agg.total_duration_ms += s.duration;
            agg.total_forced_style_layout_ms += s.forced_style_and_layout_duration;
            agg.invocation_count += 1;
            // Refresh function_name with last-seen value (not aggregated —
            // same source URL can host multiple functions; this gives at
            // least one concrete callsite for the offender).
            if !s.source_function_name.is_empty() {
                agg.source_function_name = s.source_function_name.clone();
            }
        }
    }

    let mut offenders: Vec<LoafOffender> = by_source.into_values().collect();
    offenders.sort_by(|a, b| {
        b.total_duration_ms
            .partial_cmp(&a.total_duration_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    offenders.truncate(5);
    vitals.loaf_top_offenders = offenders;
}

/// Aggregate raw longtask entries into the `WebVitals` summary. Groups by
/// best-effort "source" (preferring `attribution[0].container_src`, else
/// `attribution[0].container_name`, else the task's `name`) and returns
/// the top 5 by `total_duration_ms` desc.
///
/// `PerformanceLongTaskTiming.attribution[].containerSrc` is informative
/// for cross-frame longtasks (iframe URL) but typically empty for
/// same-frame tasks. We capture both signals — the function name and
/// the iframe URL — and degrade gracefully. The Long Animation Frame
/// API (see `aggregate_loaf`) is more precise for same-frame attribution
/// when available; longtask attribution is the broader-compat fallback.
fn aggregate_long_tasks(vitals: &mut WebVitals, raw: &[LongTaskRawEntry]) {
    use std::collections::HashMap;
    if raw.is_empty() {
        return;
    }
    let mut by_source: HashMap<String, LongTaskOffender> = HashMap::new();
    for entry in raw {
        // Pick the most actionable label available. Empty attribution
        // (or all-empty fields) falls back to the task name; a final
        // `"(same-page)"` keeps the bucket labeled even when every
        // signal is empty (older Chromium, weird embedders).
        let source = entry
            .attribution
            .iter()
            .find_map(|a| {
                if !a.container_src.is_empty() {
                    Some(a.container_src.clone())
                } else if !a.container_name.is_empty() {
                    Some(a.container_name.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                if !entry.name.is_empty() {
                    entry.name.clone()
                } else {
                    "(same-page)".to_string()
                }
            });
        let agg = by_source
            .entry(source.clone())
            .or_insert_with(|| LongTaskOffender {
                source,
                ..Default::default()
            });
        agg.total_duration_ms += entry.duration;
        if entry.duration > agg.max_duration_ms {
            agg.max_duration_ms = entry.duration;
        }
        agg.task_count += 1;
    }
    let mut offenders: Vec<LongTaskOffender> = by_source.into_values().collect();
    offenders.sort_by(|a, b| {
        b.total_duration_ms
            .partial_cmp(&a.total_duration_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    offenders.truncate(5);
    vitals.long_task_top_offenders = offenders;
}

/// Server-side enrichment for `image_sizing` entries: join `transferred_bytes`
/// from the captured `resources` (matched by URL) and compute `waste_ratio`.
/// Then sort worst-waste-first with unknown ratios trailing for a useful
/// "top offenders" view.
///
/// **DPR-correct comparison**: on a 2x DPR screen, an `<img>` displayed at
/// 400×300 CSS pixels actually needs 800×600 source pixels to render
/// crisply, so the effective display area is `display × DPR`. A naive
/// CSS-pixel-only comparison would flag a perfectly-sized 2x image as 75%
/// waste. We multiply `display_*` by `device_pixel_ratio` before computing
/// the ratio so the waste signal stays correct across DPR settings —
/// including any `device_scale_factor` emulation the request applied.
///
/// `waste_ratio` is intentionally `None` (not 0) only when either source
/// dimension is 0 (not loaded / lazy off-screen). When the image is
/// already at-size or under-size we emit `Some(0.0)` so callers can still
/// filter on it.
fn enrich_image_sizing(images: &mut [ImageSizing], resources: &[WebPageResource]) -> ImageAudit {
    // Build URL → content_size map once. Resources can repeat (redirects),
    // last write wins which is fine — final response is what mattered.
    let mut bytes_by_url: HashMap<&str, u64> = HashMap::with_capacity(resources.len());
    for r in resources {
        if r.content_size > 0 {
            bytes_by_url.insert(r.url.as_str(), r.content_size);
        }
    }

    for img in images.iter_mut() {
        if !img.url.is_empty()
            && let Some(b) = bytes_by_url.get(img.url.as_str())
        {
            img.transferred_bytes = Some(*b);
        }
        // Use floats throughout: integer math truncates the DPR scaling
        // when display_* are small (e.g. 50×50 @ 2x = 10000 effective px
        // but rounded int math could drift). Cap DPR ≥ 0.0 just to be
        // defensive; modern Chromium always reports ≥ 1 unless emulating.
        let dpr = if img.device_pixel_ratio > 0.0 {
            img.device_pixel_ratio
        } else {
            1.0
        };
        let np = (img.natural_width as f64) * (img.natural_height as f64);
        let effective_dw = img.display_width as f64 * dpr;
        let effective_dh = img.display_height as f64 * dpr;
        let dp = effective_dw * effective_dh;
        img.waste_ratio = if np == 0.0 || dp == 0.0 {
            None
        } else if dp >= np {
            // At-size or being upscaled (under-resolution) — not waste.
            // Under-resolution is a separate visual-quality issue, not a
            // bandwidth one; not flagged here.
            Some(0.0)
        } else {
            // (natural - effective_display) / natural; clamp [0, 1].
            let w = 1.0 - (dp / np);
            Some(w.clamp(0.0, 1.0))
        };
    }

    // Worst offenders first; unknown (None) waste sinks to the bottom.
    images.sort_by(|a, b| {
        let ar = a.waste_ratio.unwrap_or(-1.0);
        let br = b.waste_ratio.unwrap_or(-1.0);
        br.partial_cmp(&ar).unwrap_or(std::cmp::Ordering::Equal)
    });

    build_image_audit(images)
}

/// Build the Lighthouse-aligned "image" four-pack from already-enriched
/// `ImageSizing` entries. Each list independently sorted + capped at 20.
///
/// Filters:
/// - All categories skip the LCP / hero pre-image case (data: URLs, empty
///   URLs) — they're not actionable as URL references.
/// - `oversized` requires the natural-vs-effective-display ratio > 2.0.
///   The 2× threshold (vs Lighthouse's stricter 1.0) suppresses the
///   "off by a few percent at fractional DPR" noise.
/// - `missing_dimensions` skips unloaded lazy images — natural size
///   isn't known yet, and the fix (add width/height attrs) applies to
///   them in the same way; we just list once they've loaded.
/// - `missing_lazy` requires `in_viewport=false` AND `loading!="lazy"`.
///   Above-the-fold images legitimately load eagerly.
/// - `missing_srcset` lists every img without srcset over a minimal
///   display-area floor (32×32) to suppress 1px tracking pixels.
fn build_image_audit(images: &[ImageSizing]) -> ImageAudit {
    let mut oversized: Vec<ImageIssue> = Vec::new();
    let mut missing_dimensions: Vec<ImageIssue> = Vec::new();
    let mut missing_lazy: Vec<ImageIssue> = Vec::new();
    let mut missing_srcset: Vec<ImageIssue> = Vec::new();

    const SRCSET_AREA_FLOOR: u32 = 32 * 32;

    for img in images.iter() {
        if img.url.is_empty() || img.url.starts_with("data:") {
            continue;
        }
        let dpr = if img.device_pixel_ratio > 0.0 {
            img.device_pixel_ratio
        } else {
            1.0
        };
        let np = (img.natural_width as f64) * (img.natural_height as f64);
        let dp = (img.display_width as f64 * dpr) * (img.display_height as f64 * dpr);
        let oversize_ratio = if dp > 0.0 && np > 0.0 { np / dp } else { 0.0 };
        let display_area = img.display_width.saturating_mul(img.display_height);

        if oversize_ratio > 2.0 {
            oversized.push(ImageIssue {
                url: img.url.clone(),
                display_width: img.display_width,
                display_height: img.display_height,
                in_viewport: img.in_viewport,
                ratio: oversize_ratio,
            });
        }
        if img.loaded && !(img.has_width_attr && img.has_height_attr) && display_area > 0 {
            missing_dimensions.push(ImageIssue {
                url: img.url.clone(),
                display_width: img.display_width,
                display_height: img.display_height,
                in_viewport: img.in_viewport,
                ratio: 0.0,
            });
        }
        if !img.in_viewport && img.loading != "lazy" {
            missing_lazy.push(ImageIssue {
                url: img.url.clone(),
                display_width: img.display_width,
                display_height: img.display_height,
                in_viewport: false,
                ratio: 0.0,
            });
        }
        if !img.has_srcset && display_area >= SRCSET_AREA_FLOOR {
            missing_srcset.push(ImageIssue {
                url: img.url.clone(),
                display_width: img.display_width,
                display_height: img.display_height,
                in_viewport: img.in_viewport,
                ratio: 0.0,
            });
        }
    }

    // `oversized` ranks by ratio desc — biggest waste first.
    oversized.sort_by(|a, b| {
        b.ratio
            .partial_cmp(&a.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    oversized.truncate(20);
    // Other lists rank by display area desc — bigger images cost more
    // to fix in shifted layout / fetched bytes / wrong-sized decode.
    let area_desc = |a: &ImageIssue, b: &ImageIssue| {
        let aa = (b.display_width as u64).saturating_mul(b.display_height as u64);
        let bb = (a.display_width as u64).saturating_mul(a.display_height as u64);
        aa.cmp(&bb)
    };
    missing_dimensions.sort_by(area_desc);
    missing_dimensions.truncate(20);
    missing_lazy.sort_by(area_desc);
    missing_lazy.truncate(20);
    missing_srcset.sort_by(area_desc);
    missing_srcset.truncate(20);

    ImageAudit {
        oversized,
        missing_dimensions,
        missing_lazy,
        missing_srcset,
    }
}

/// Aggregate `resources` into comparable scalars. Pure derive — no browser
/// interaction. MIME mapped to a small bucket set for stable bucket names
/// across deploys.
/// Parse `max-age=N` out of a `Cache-Control` header. Returns the
/// numeric value when found (clamped to u32; values like `0` and
/// `no-cache` legitimately mean "must revalidate" and are returned
/// as `Some(0)`). Returns `None` when no `max-age` directive is
/// present, when the value is malformed, or when an `s-maxage`-only
/// header was supplied (shared-cache only — irrelevant to browser
/// caching). Case-insensitive. Whitespace-tolerant around `=`.
fn parse_max_age(cache_control: &str) -> Option<u32> {
    for token in cache_control.split(',') {
        let t = token.trim();
        let lower = t.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("max-age") {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix('=')?.trim_start();
            return rest
                .parse::<u64>()
                .ok()
                .map(|v| v.min(u32::MAX as u64) as u32);
        }
    }
    None
}

/// True when the URL path looks fingerprinted — i.e. contains a
/// hex token of length ≥ 8 (typical webpack / vite / rollup output:
/// `app.4f7c2a91.js`, `main-9d3b8e2f.css`, `chunk.a1b2c3d4e5f6.js`).
/// Length ≥ 8 dodges short hex words like `cafe` or `dead` that
/// might legitimately appear in non-versioned URLs. We scan the
/// *path* segments of the URL (so query strings and host don't
/// confuse the detector) and require the hex run to be flanked by
/// non-hex characters so the path itself doesn't need to be all hex.
fn is_hashed_url_path(parsed: &url::Url) -> bool {
    let path = parsed.path();
    let bytes = path.as_bytes();
    let is_hex =
        |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || (b'A'..=b'F').contains(&b);
    let mut run: usize = 0;
    for &b in bytes {
        if is_hex(b) {
            run += 1;
            if run >= 8 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// True iff the MIME bucket represents a static asset whose cache
/// policy should be tightly controlled (long max-age, `immutable`
/// for hashed URLs). HTML / JSON / XHR / generic "other" all
/// legitimately use short or no-store policies and are excluded so
/// `cache_policy_issues` stays signal-only.
fn is_static_asset_bucket(bucket: &str) -> bool {
    matches!(bucket, "javascript" | "css" | "image" | "font")
}

fn build_resource_summary(resources: &[WebPageResource], target_url: &str) -> ResourceSummary {
    let target_host = url::Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();

    let mut summary = ResourceSummary::default();
    let mut largest: Option<(String, u64)> = None;
    let mut cache_hits: u32 = 0;
    let mut hosts: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Per-host (bytes, count) for ranking top third-party domains. Only
    // populated for hosts that differ from `target_host` so the page's
    // own origin doesn't show up in the third-party list.
    let mut third_party_by_host: HashMap<String, (u64, u32)> = HashMap::new();
    let mut modern_protocol_hits: u32 = 0;
    let mut real_network_responses: u32 = 0;

    for r in resources {
        let bucket = mime_bucket(&r.mime_type);
        *summary.bytes_by_type.entry(bucket.to_string()).or_insert(0) += r.content_size;
        *summary.count_by_type.entry(bucket.to_string()).or_insert(0) += 1;

        // Cache-Control coverage: counted across ALL responses (cached
        // hits too — the header is a property of the original origin
        // response, and CDP carries the cached headers forward).
        if r.cache_control.is_some() {
            summary.cache_control_present += 1;
        } else {
            summary.cache_control_missing += 1;
        }

        // Image-format buckets — case-insensitive prefix match against
        // the canonical IANA strings. SVG and other vector / unusual
        // formats are intentionally excluded (no conversion target).
        let m = r.mime_type.to_ascii_lowercase();
        if m == "image/jpeg" || m == "image/png" || m == "image/gif" {
            summary.legacy_image_bytes += r.content_size;
        } else if m == "image/webp" || m == "image/avif" {
            summary.modern_image_bytes += r.content_size;
        }

        // Source-map coverage — only JS / CSS resources contribute. Use
        // mime-bucket so this stays consistent with the rest of the
        // type aggregates (e.g. `application/javascript`,
        // `text/javascript`, and `text/css` all map cleanly).
        if bucket == "javascript" || bucket == "css" {
            if r.has_source_map {
                summary.source_maps_present += 1;
            } else {
                summary.source_maps_missing += 1;
            }
        }

        let status_bucket = match r.status {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            500..=599 => "5xx",
            _ => "other",
        };
        *summary
            .status_distribution
            .entry(status_bucket.to_string())
            .or_insert(0) += 1;

        if r.from_cache {
            cache_hits += 1;
            summary.cached_bytes += r.content_size;
        } else {
            real_network_responses += 1;
            // Real network responses only — cache hits never touched the
            // wire so their `connection_reused` flag is meaningless.
            if r.connection_reused {
                summary.connections_reused += 1;
            } else {
                summary.connections_new += 1;
            }
            // Protocol distribution: skip cache (no real protocol) and
            // normalise empty → "unknown" so missing values are visible
            // rather than silently dropped.
            let proto = if r.protocol.is_empty() {
                "unknown".to_string()
            } else {
                r.protocol.to_lowercase()
            };
            // Modern protocol = h2 (HTTP/2) or any h3 variant (h3,
            // h3-29, etc.). Plain "http/1.1" and "unknown" don't count.
            if proto == "h2" || proto.starts_with("h3") {
                modern_protocol_hits += 1;
            }
            *summary.protocol_distribution.entry(proto).or_insert(0) += 1;

            // Compression audit: track compressed vs missed-opportunity
            // for compressible text-y MIME types. Image / video / font /
            // wasm are already compressed at the format level so the
            // absence of Content-Encoding isn't a finding for them.
            //
            // `compression_breakdown` only buckets text-compressible
            // resources (one row per algorithm + "none") — keeps the
            // map small and focused on the actionable signal. The
            // first algorithm in `Content-Encoding` wins when multiple
            // codings are layered (e.g. `gzip, br` is rare but valid).
            if is_text_compressible(&r.mime_type) {
                let algo = r
                    .content_encoding
                    .as_deref()
                    .map(|e| e.split(',').next().unwrap_or(e).trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "none".to_string());
                *summary.compression_breakdown.entry(algo).or_insert(0) += 1;
            }
            if r.content_encoding.is_some() {
                summary.compressed_count += 1;
            } else if is_text_compressible(&r.mime_type) {
                summary.uncompressed_text_count += 1;
                summary.uncompressed_text_bytes += r.content_size;
            }
        }

        // Third-party = different host from the target URL.
        // Also collect unique hosts (DNS lookup approximation) — done
        // here once per resource regardless of cache state. Uses the
        // `parsed_url` cache populated in `collect_summary`; avoids a
        // second per-resource `Url::parse` call.
        if let Some(h) = r.parsed_url.as_ref().and_then(|u| u.host_str()) {
            hosts.insert(h.to_string());
            if !target_host.is_empty() && h != target_host {
                summary.third_party_bytes += r.content_size;
                let slot = third_party_by_host.entry(h.to_string()).or_insert((0, 0));
                slot.0 += r.content_size;
                slot.1 += 1;
            }
        }

        match largest.as_ref() {
            None => largest = Some((r.url.clone(), r.content_size)),
            Some((_, sz)) if r.content_size > *sz => {
                largest = Some((r.url.clone(), r.content_size))
            }
            _ => {}
        }
    }

    if !resources.is_empty() {
        summary.cache_hit_ratio = cache_hits as f64 / resources.len() as f64;
    }
    summary.largest_resource = largest;
    summary.unique_hosts = hosts.len() as u32;

    // Top-10 third-party hosts by bytes (ties broken by host asc for
    // stable output across captures). All-zero ratio is fine — `0.0`
    // when no real network responses were observed (full cache).
    let mut by_bytes: Vec<DomainBytes> = third_party_by_host
        .into_iter()
        .map(|(host, (bytes, count))| DomainBytes { host, bytes, count })
        .collect();
    by_bytes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.host.cmp(&b.host)));
    by_bytes.truncate(10);
    summary.top_third_party_domains = by_bytes;

    summary.modern_protocol_share = if real_network_responses > 0 {
        modern_protocol_hits as f64 / real_network_responses as f64
    } else {
        0.0
    };

    summary.duplicate_resources = build_duplicate_resources(resources);
    summary.mixed_content = build_mixed_content(resources, target_url);
    summary.max_initiator_chain_depth = compute_initiator_chain_depth(resources);

    // Top-N largest resources per static-asset MIME bucket. Restricted
    // to JS / CSS / image / font — the four types where "this one file
    // is huge" is directly actionable. Skip `from_cache=true` AND
    // `content_size=0` (cache hits with no real size) to avoid noise.
    // Each bucket capped at 5 to keep the JSON / markdown compact.
    {
        let mut by_bucket: HashMap<&'static str, Vec<&WebPageResource>> = HashMap::new();
        for r in resources {
            let bucket = mime_bucket(&r.mime_type);
            let bucket_static: &'static str = match bucket {
                "javascript" => "javascript",
                "css" => "css",
                "image" => "image",
                "font" => "font",
                _ => continue,
            };
            // Skip cache-hits with zero bytes — they convey no size info.
            if r.from_cache && r.content_size == 0 {
                continue;
            }
            by_bucket.entry(bucket_static).or_default().push(r);
        }
        for (bucket, mut group) in by_bucket {
            group.sort_by_key(|r| std::cmp::Reverse(r.content_size));
            let top: Vec<LargestResource> = group
                .into_iter()
                .take(5)
                .map(|r| LargestResource {
                    url: r.url.clone(),
                    bytes: r.content_size,
                    mime_type: r.mime_type.clone(),
                    from_cache: r.from_cache,
                })
                .collect();
            if !top.is_empty() {
                summary.top_largest_by_type.insert(bucket.to_string(), top);
            }
        }
    }

    // Uncompressed text resources — the offender list. Already counted
    // by the scalar pair `uncompressed_text_count` / `_bytes`; this
    // surfaces the specific URLs. Filter mirrors the scalar derive:
    // real-network responses only (cache hits never paid wire cost),
    // text-compressible MIME types, no `Content-Encoding`. Floor at
    // 1KB to avoid noise from tiny placeholders.
    {
        let mut candidates: Vec<UncompressedResource> = resources
            .iter()
            .filter(|r| {
                !r.from_cache
                    && r.content_encoding.is_none()
                    && is_text_compressible(&r.mime_type)
                    && r.content_size >= 1024
            })
            .map(|r| UncompressedResource {
                url: r.url.clone(),
                mime_type: r.mime_type.clone(),
                bytes: r.content_size,
            })
            .collect();
        candidates.sort_by_key(|e| std::cmp::Reverse(e.bytes));
        candidates.truncate(20);
        summary.uncompressed_text_resources = candidates;
    }

    // Cache-policy anti-patterns on static assets. Two reason codes:
    //   `short_max_age` — `max-age` parsed below 60s on JS/CSS/img/font.
    //   `missing_immutable` — fingerprinted URL with cache-control set
    //     but missing the `immutable` directive (each hard refresh
    //     triggers a revalidation round-trip that could be skipped).
    // Cap at 20 worst entries (by raw content_size desc — biggest waste
    // first). HTML / JSON / API responses excluded — their headers
    // reflect business rules, not asset misconfig.
    {
        let mut findings: Vec<(u64, CachePolicyIssue)> = Vec::new();
        for r in resources {
            let bucket = mime_bucket(&r.mime_type);
            if !is_static_asset_bucket(bucket) {
                continue;
            }
            let Some(cc) = r.cache_control.as_deref() else {
                continue;
            };
            let cc_lower = cc.to_ascii_lowercase();
            // Skip explicit no-store / no-cache — those declare a
            // policy and aren't an oversight (rare on static assets,
            // but valid for one-off cache busters).
            if cc_lower.contains("no-store") || cc_lower.contains("no-cache") {
                continue;
            }
            if let Some(max_age) = parse_max_age(cc)
                && max_age < 60
            {
                findings.push((
                    r.content_size,
                    CachePolicyIssue {
                        url: r.url.clone(),
                        mime_type: r.mime_type.clone(),
                        cache_control: cc.to_string(),
                        reason: "short_max_age".to_string(),
                    },
                ));
                continue;
            }
            // Missing-immutable check — only for fingerprinted URLs
            // (otherwise `immutable` would be unsafe). The parsed_url
            // cache means no extra Url::parse cost here.
            if let Some(parsed) = r.parsed_url.as_ref()
                && is_hashed_url_path(parsed)
                && !cc_lower.contains("immutable")
            {
                findings.push((
                    r.content_size,
                    CachePolicyIssue {
                        url: r.url.clone(),
                        mime_type: r.mime_type.clone(),
                        cache_control: cc.to_string(),
                        reason: "missing_immutable".to_string(),
                    },
                ));
            }
        }
        findings.sort_by_key(|(sz, _)| std::cmp::Reverse(*sz));
        findings.truncate(20);
        summary.cache_policy_issues = findings.into_iter().map(|(_, e)| e).collect();
    }

    summary
}

/// Detect HTTPS-page-loading-HTTP-resource ("mixed content") findings.
/// When the main `target_url` is not HTTPS this returns the default
/// (no detection applies — every resource is plain HTTP and the page
/// itself is too). Otherwise scans `resources[]` for `http://` URLs
/// and returns the top-10 offenders by `content_size` desc.
fn build_mixed_content(resources: &[WebPageResource], target_url: &str) -> MixedContent {
    if !target_url.starts_with("https://") {
        return MixedContent::default();
    }
    let mut offenders: Vec<MixedContentResource> = resources
        .iter()
        .filter(|r| r.url.starts_with("http://"))
        .map(|r| MixedContentResource {
            url: r.url.clone(),
            content_size: r.content_size,
            kind: mime_bucket(&r.mime_type).to_string(),
        })
        .collect();
    let total_count = offenders.len() as u32;
    if total_count == 0 {
        return MixedContent::default();
    }
    offenders.sort_by_key(|e| std::cmp::Reverse(e.content_size));
    offenders.truncate(10);
    MixedContent {
        detected: true,
        total_count,
        resources: offenders,
    }
}

/// Walk `initiator.url` chains backwards from each resource to find
/// the maximum depth. Returns `None` when no resource has initiator
/// data (caller didn't request `initiators=true`), `Some(0)` when
/// every resource was initiated directly by the parser at depth 1.
///
/// Approximates Lighthouse "Avoid chaining critical requests" —
/// without the explicit "render-blocking only" filter, so this is
/// the upper bound on chain length across all initiator types.
/// Defended against cycles (rare but possible in degenerate
/// initiator graphs) by a visited-URL set; capped at 100 hops to
/// guarantee termination on pathological inputs.
fn compute_initiator_chain_depth(resources: &[WebPageResource]) -> Option<u32> {
    if !resources.iter().any(|r| r.initiator.is_some()) {
        return None;
    }
    let mut parent: HashMap<&str, &str> = HashMap::new();
    for r in resources {
        if let Some(init) = r.initiator.as_ref()
            && let Some(p) = init.url.as_deref()
            && !p.is_empty()
        {
            parent.insert(r.url.as_str(), p);
        }
    }
    let mut max_depth: u32 = 0;
    for r in resources {
        let mut depth: u32 = 0;
        let mut current: &str = r.url.as_str();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        seen.insert(current);
        while let Some(&p) = parent.get(current) {
            if !seen.insert(p) {
                break; // cycle guard
            }
            depth += 1;
            current = p;
            if depth > 100 {
                break; // pathological-input safety cap
            }
        }
        max_depth = max_depth.max(depth);
    }
    Some(max_depth)
}

/// Extract the basename (final path segment) from a URL, stripping
/// query string and fragment. `None` for URLs whose path ends in `/`
/// (no real filename) or that don't parse.
fn url_basename(url: &str) -> Option<&str> {
    let no_query = url.split('?').next().unwrap_or(url);
    let no_frag = no_query.split('#').next().unwrap_or(no_query);
    no_frag
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && !s.contains(':'))
}

/// Detect duplicate resources via two complementary passes. Pure
/// derive; runs in `O(N log N)` worst case for the sorts.
///
/// Pass 1 — **exact URL**: group resources by URL, keep groups with
/// `count ≥ 2`. Sort each group's content_size desc; waste = sum of
/// all but the largest size (so a `[fresh, cache-hit]` pair reports
/// `wasted = 0` while `[fresh, fresh]` reports `wasted = fresh_size`).
///
/// Pass 2 — **basename + size**: group by `(basename, content_size)`,
/// dedupe URLs within each group. Keep groups with ≥2 distinct URLs.
/// Skips entries with `content_size == 0` (cache hits and empty
/// responses — no meaningful size signal). Skips entries with no
/// extractable basename (path ending in `/`).
///
/// Both lists capped at top 10 sorted by `wasted_bytes` desc.
fn build_duplicate_resources(resources: &[WebPageResource]) -> DuplicateResources {
    let mut by_url: HashMap<&str, Vec<&WebPageResource>> = HashMap::new();
    for r in resources {
        by_url.entry(r.url.as_str()).or_default().push(r);
    }
    let mut exact_url: Vec<DuplicateEntry> = Vec::new();
    let mut wasted_total: u64 = 0;
    for (url, copies) in &by_url {
        if copies.len() < 2 {
            continue;
        }
        let mut sizes: Vec<u64> = copies.iter().map(|r| r.content_size).collect();
        sizes.sort_by(|a, b| b.cmp(a));
        let bytes_each = sizes.first().copied().unwrap_or(0);
        let wasted: u64 = sizes.iter().skip(1).sum();
        wasted_total += wasted;
        exact_url.push(DuplicateEntry {
            key: (*url).to_string(),
            urls: vec![(*url).to_string()],
            count: copies.len() as u32,
            bytes_each,
            wasted_bytes: wasted,
        });
    }
    exact_url.sort_by_key(|e| std::cmp::Reverse(e.wasted_bytes));
    exact_url.truncate(10);

    // Pass 2: basename + size. Skip cache hits and empty responses —
    // they have no meaningful size for comparison.
    let mut by_name_size: HashMap<(&str, u64), Vec<&WebPageResource>> = HashMap::new();
    for r in resources {
        if r.content_size == 0 {
            continue;
        }
        let Some(name) = url_basename(&r.url) else {
            continue;
        };
        by_name_size
            .entry((name, r.content_size))
            .or_default()
            .push(r);
    }
    let mut likely_same_file: Vec<DuplicateEntry> = Vec::new();
    for ((name, size), copies) in &by_name_size {
        let mut urls: Vec<String> = copies.iter().map(|r| r.url.clone()).collect();
        urls.sort();
        urls.dedup();
        if urls.len() < 2 {
            continue;
        }
        let count = urls.len() as u32;
        let bytes_each = *size;
        let wasted = (count as u64 - 1) * bytes_each;
        wasted_total += wasted;
        likely_same_file.push(DuplicateEntry {
            key: format!("{name}|{bytes_each}"),
            urls,
            count,
            bytes_each,
            wasted_bytes: wasted,
        });
    }
    likely_same_file.sort_by_key(|e| std::cmp::Reverse(e.wasted_bytes));
    likely_same_file.truncate(10);

    DuplicateResources {
        exact_url,
        likely_same_file,
        wasted_bytes: wasted_total,
    }
}

/// Whether a MIME type benefits from text compression (gzip/br/zstd).
/// Image/video/font/wasm are already compressed at the format level —
/// double-compressing them wastes CPU. Used to flag "uses-text-compression"
/// audit candidates.
fn is_text_compressible(mime: &str) -> bool {
    let m = mime.to_ascii_lowercase();
    m.starts_with("text/")
        || m.contains("javascript")
        || m.contains("ecmascript")
        || m.contains("json")
        || m.contains("xml")
        || m.contains("svg")
        || m.contains("html")
}

/// Map a MIME type string to a short stable bucket name. Unknown / empty
/// MIME maps to "other".
fn mime_bucket(mime: &str) -> &'static str {
    let m = mime.to_ascii_lowercase();
    if m.starts_with("image/") {
        "image"
    } else if m.starts_with("video/") || m.starts_with("audio/") {
        "media"
    } else if m.starts_with("font/")
        || m.contains("woff")
        || m.contains("opentype")
        || m.contains("truetype")
    {
        "font"
    } else if m.contains("javascript") || m.contains("ecmascript") {
        "javascript"
    } else if m.contains("css") {
        "css"
    } else if m.contains("html") {
        "html"
    } else if m.contains("xml") {
        "xml"
    } else if m.contains("json") {
        "json"
    } else {
        "other"
    }
}

/// Read CDP `Performance.getMetrics` and project the well-known counters
/// into `PageMetrics`. Memory + DOM counts are sourced directly; CPU
/// durations are converted from CDP seconds to ms. Unknown metric names
/// are ignored.
///
/// **Precondition:** `apply_performance_enable(page, true)` must have been
/// called earlier in the request (handled by `capture()` in the apply
/// stage when `req.metrics = true`). Without it, CDP returns the error
/// "Performance domain is not enabled" and this function fails.
pub async fn collect_page_metrics(page: &Page) -> Result<PageMetrics, Error> {
    let resp = page.execute(GetMetricsParams::default()).await?;

    let mut m = PageMetrics::default();
    for metric in &resp.result.metrics {
        let v = metric.value;
        match metric.name.as_str() {
            "JSHeapUsedSize" => m.js_heap_used = v.max(0.0) as u64,
            "JSHeapTotalSize" => m.js_heap_total = v.max(0.0) as u64,
            "Documents" => m.documents = v.max(0.0) as u32,
            "Frames" => m.frames = v.max(0.0) as u32,
            "Nodes" => m.nodes = v.max(0.0) as u32,
            "JSEventListeners" => m.js_event_listeners = v.max(0.0) as u32,
            // CDP returns seconds; convert to ms for consistency with the
            // rest of the codebase (timeout_ms, settle_ms, etc.).
            "ScriptDuration" => m.script_duration_ms = v.max(0.0) * 1000.0,
            "LayoutDuration" => m.layout_duration_ms = v.max(0.0) * 1000.0,
            "RecalcStyleDuration" => m.recalc_style_duration_ms = v.max(0.0) * 1000.0,
            "TaskDuration" => m.task_duration_ms = v.max(0.0) * 1000.0,
            _ => {}
        }
    }
    Ok(m)
}

/// Setup script registered via `Page.addScriptToEvaluateOnNewDocument` so
/// it runs **before** any page script on the next navigation. Installs
/// three `PerformanceObserver`s into `window.__web_vitals`:
/// - LCP: max startTime of `largest-contentful-paint` entries
/// - CLS: sum of `layout-shift` entry values where `!hadRecentInput`
/// - TBT: sum of `(longtask.duration - 50)` for each long task
///   `buffered: true` backfills entries that fired before observer install.
const WEB_VITALS_SETUP_JS: &str = r#"
(function() {
  if (window.__web_vitals_initialized) return;
  window.__web_vitals_initialized = true;
  window.__web_vitals = {
    lcp: 0, cls: 0, tbt: 0, ttfb: 0, long_tasks: 0,
    lcp_element: null,
    cls_entries: [],
  };
  const MAX_CLS_ENTRIES = 50;
  function describeNode(n) {
    if (!n || n.nodeType !== 1) return null;
    return {
      tag: (n.tagName || '').toLowerCase(),
      id: n.id || '',
      class: typeof n.className === 'string' ? n.className : '',
    };
  }
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        // LCP can update multiple times (each new larger element wins);
        // always overwrite with the latest.
        if (e.startTime >= window.__web_vitals.lcp) {
          window.__web_vitals.lcp = e.startTime;
          const node = e.element;
          if (node && node.nodeType === 1) {
            const desc = describeNode(node);
            // Image-like: prefer currentSrc, fall back to src / poster.
            const isImg = desc.tag === 'img' || desc.tag === 'video';
            const url = isImg
              ? (node.currentSrc || node.src || node.poster || e.url || null)
              : null;
            // Text-like: snapshot first 120 chars.
            const text = !isImg
              ? (node.textContent || '').trim().slice(0, 120) || null
              : null;
            // Image natural dimensions (decoded pixel size). Lets the AI
            // detect "image is N× the display size" without a separate
            // image_sizing fetch. `videoWidth`/`videoHeight` for <video>.
            let nw = 0, nh = 0;
            if (desc.tag === 'img') {
              nw = node.naturalWidth | 0; nh = node.naturalHeight | 0;
            } else if (desc.tag === 'video') {
              nw = node.videoWidth | 0; nh = node.videoHeight | 0;
            }
            window.__web_vitals.lcp_element = {
              tag: desc.tag, id: desc.id, class: desc.class,
              url: url, text_preview: text,
              // Computed area of the LCP candidate in CSS px². Always set.
              size: e.size || 0,
              // For images: when the resource finished loading
              // (ms from nav start). `0` for text LCP candidates.
              load_time: e.loadTime || 0,
              // When the element was actually painted. May be `0` on
              // cross-origin images without `Timing-Allow-Origin` —
              // browser hides exact paint time as a side-channel guard.
              render_time: e.renderTime || 0,
              natural_width: nw, natural_height: nh,
            };
          }
        }
      }
    }).observe({ type: 'largest-contentful-paint', buffered: true });
  } catch (e) {}
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (e.hadRecentInput) continue;
        window.__web_vitals.cls += e.value;
        if (window.__web_vitals.cls_entries.length < MAX_CLS_ENTRIES) {
          // Each source carries `previousRect` / `currentRect` (DOMRectReadOnly).
          // Capture both so the server can compute Euclidean movement
          // distance and AI can suggest concrete fixes ("reserve 240px
          // for #promo-banner" rather than just "fix CLS").
          const sources = (e.sources || [])
            .map((s) => {
              const node = describeNode(s.node);
              if (!node) return null;
              const pr = s.previousRect || {};
              const cr = s.currentRect || {};
              const dx = (cr.x || 0) - (pr.x || 0);
              const dy = (cr.y || 0) - (pr.y || 0);
              return {
                tag: node.tag, id: node.id, class: node.class,
                previous_rect: {
                  x: pr.x || 0, y: pr.y || 0,
                  width: pr.width || 0, height: pr.height || 0,
                },
                current_rect: {
                  x: cr.x || 0, y: cr.y || 0,
                  width: cr.width || 0, height: cr.height || 0,
                },
                distance_px: Math.sqrt(dx * dx + dy * dy),
              };
            })
            .filter((x) => x !== null);
          window.__web_vitals.cls_entries.push({
            time_ms: e.startTime,
            value: e.value,
            sources: sources,
          });
        }
      }
    }).observe({ type: 'layout-shift', buffered: true });
  } catch (e) {}
  // Long Task entries — also fed into the TBT scalar above. We keep a
  // capped list of per-task records (start / duration / attribution) so
  // the server can aggregate top offending sources. Attribution comes
  // from `PerformanceLongTaskTiming.attribution[]`, which lists the
  // containing frame / object. `containerSrc` is most useful for
  // cross-frame tasks (iframe src) — for same-frame work the field is
  // usually empty and `entry.name` (`"self"` etc.) carries the only
  // signal. We capture both; server-side aggregator falls back gracefully.
  window.__web_vitals.long_task_entries = [];
  const MAX_LONGTASKS = 100;
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (e.duration > 50) window.__web_vitals.tbt += e.duration - 50;
        window.__web_vitals.long_tasks++;
        if (window.__web_vitals.long_task_entries.length < MAX_LONGTASKS) {
          const attribution = (e.attribution || []).map((a) => ({
            container_type: a.containerType || '',
            container_src: a.containerSrc || '',
            container_id: a.containerId || '',
            container_name: a.containerName || '',
          }));
          window.__web_vitals.long_task_entries.push({
            name: e.name || '',
            start_time: e.startTime,
            duration: e.duration,
            attribution: attribution,
          });
        }
      }
    }).observe({ type: 'longtask', buffered: true });
  } catch (e) {}
  // INP (Interaction to Next Paint) — 2024 Core Web Vital, replaced FID.
  // Only `event` entries with an `interactionId` count as user interactions
  // (filters out passive events like scroll). In pure headless scraping
  // this stays 0 since no user input fires; becomes meaningful when the
  // request's `script` param simulates click()/dispatchEvent() etc.
  window.__web_vitals.inp = 0;
  window.__web_vitals.interaction_count = 0;
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (!e.interactionId) continue;
        window.__web_vitals.interaction_count++;
        if (e.duration > window.__web_vitals.inp) {
          window.__web_vitals.inp = e.duration;
        }
      }
    }).observe({ type: 'event', buffered: true, durationThreshold: 16 });
  } catch (e) {}
  // Long Animation Frames (LoAF) — Chrome 123+. More precise jank signal
  // than `longtask`: per-frame breakdown of script / style / layout /
  // paint with attributable script sources. Stored as a capped list;
  // server side aggregates top offenders by source URL.
  window.__web_vitals.loaf_entries = [];
  const MAX_LOAF = 100;
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (window.__web_vitals.loaf_entries.length >= MAX_LOAF) break;
        // Per-script breakdown: only meaningful scripts (skip <5ms noise),
        // cap per frame to keep payload bounded.
        const scripts = (e.scripts || [])
          .filter((s) => s.duration > 5)
          .slice(0, 5)
          .map((s) => ({
            invoker_type: s.invokerType || '',
            source_url: s.sourceURL || '',
            source_function_name: s.sourceFunctionName || '',
            duration: s.duration,
            forced_style_and_layout_duration:
              s.forcedStyleAndLayoutDuration || 0,
          }));
        window.__web_vitals.loaf_entries.push({
          start_time: e.startTime,
          duration: e.duration,
          blocking_duration: e.blockingDuration || 0,
          render_start: e.renderStart || 0,
          style_and_layout_start: e.styleAndLayoutStart || 0,
          scripts: scripts,
        });
      }
    }).observe({ type: 'long-animation-frame', buffered: true });
  } catch (e) {}
  // FPS — rAF-driven frame counter. Streaming aggregation so memory
  // stays constant regardless of observation window length. Caveats:
  //   1. The rAF loop itself drives frame production. A page with no
  //      animation would normally be idle (compositor stopped); our
  //      loop forces ~60fps even then. So `avg_fps` on a static page
  //      reads near-target, not zero — meaningful for animation /
  //      scroll-heavy pages but biased high on static ones.
  //   2. The most actionable signals are `jank_ratio` (frames slower
  //      than 16.67ms) and `longest_frame_ms` — those reflect main-
  //      thread blocking regardless of why the loop is running.
  //   3. In headless / VM: software rasterization → numbers are not
  //      comparable to user-device measurements, but consistent for
  //      regression detection on the same harness.
  window.__web_vitals.fps_frames = 0;
  window.__web_vitals.fps_jank = 0;
  window.__web_vitals.fps_longest_ms = 0;
  window.__web_vitals.fps_first_ts = 0;
  window.__web_vitals.fps_last_ts = 0;
  const FPS_JANK_THRESHOLD_MS = 1000 / 60;  // 16.67ms — 60fps target
  let __wv_prev_frame_ts = 0;
  function __wv_fps_tick(ts) {
    if (__wv_prev_frame_ts > 0) {
      const dt = ts - __wv_prev_frame_ts;
      window.__web_vitals.fps_frames++;
      if (dt > FPS_JANK_THRESHOLD_MS) window.__web_vitals.fps_jank++;
      if (dt > window.__web_vitals.fps_longest_ms) {
        window.__web_vitals.fps_longest_ms = dt;
      }
      if (window.__web_vitals.fps_first_ts === 0) {
        window.__web_vitals.fps_first_ts = __wv_prev_frame_ts;
      }
      window.__web_vitals.fps_last_ts = ts;
    }
    __wv_prev_frame_ts = ts;
    requestAnimationFrame(__wv_fps_tick);
  }
  requestAnimationFrame(__wv_fps_tick);
})();
"#;

/// Read accumulated `window.__web_vitals`, enrich with TTFB from the
/// Navigation Timing API, and derive FPS scalars from the raw frame
/// counters maintained by the rAF loop.
const WEB_VITALS_READ_JS: &str = r#"
(function() {
  const v = window.__web_vitals || { lcp: 0, cls: 0, tbt: 0, ttfb: 0, long_tasks: 0 };
  const nav = performance.getEntriesByType('navigation')[0];
  if (nav) v.ttfb = nav.responseStart;
  // FPS derivation. Compute window from the first vs last frame's
  // timestamps, so we measure during the period frames were actually
  // observed (not from page nav, which would skew avg with the lead-in
  // before the first rAF callback fires).
  const fpsFrames = v.fps_frames || 0;
  const fpsJank = v.fps_jank || 0;
  const fpsLongest = v.fps_longest_ms || 0;
  const fpsWindowMs = (v.fps_last_ts || 0) - (v.fps_first_ts || 0);
  v.fps_frame_count = fpsFrames;
  v.fps_avg = fpsWindowMs > 0 ? (fpsFrames * 1000 / fpsWindowMs) : 0;
  v.fps_jank_ratio = fpsFrames > 0 ? (fpsJank / fpsFrames) : 0;
  v.fps_longest_frame_ms = fpsLongest;
  // Strip the raw counters — the public shape only carries the four
  // derived scalars above. Keeps the JSON envelope tight.
  delete v.fps_frames;
  delete v.fps_jank;
  delete v.fps_longest_ms;
  delete v.fps_first_ts;
  delete v.fps_last_ts;
  return v;
})()
"#;

/// Install pre-navigation observer scripts in a single CDP call.
///
/// Each enabled flag contributes its IIFE-wrapped setup block to a combined
/// payload, which is then injected via **one** `Page.addScriptToEvaluateOnNewDocument`.
/// Saves one CDP RTT vs registering each script separately — the registered
/// payload still runs before any user / framework script on every new
/// document, preserving the "observer in place before initial paint /
/// hydration" guarantee.
///
/// Safe to concatenate: every setup block is a self-contained IIFE
/// terminated with `})();`, so two IIFEs joined directly parse as two
/// independent statements with no scope cross-talk. All blocks also include
/// idempotency guards (`if (window.__xxx_initialized) return;`) so even
/// double-injection from buggy callers wouldn't double-instrument.
///
/// Returns `Ok(())` without any CDP call when both flags are `false` —
/// callers don't need to pre-check.
pub async fn apply_observers_setup(
    page: &Page,
    web_vitals: bool,
    dom_mutations: bool,
) -> Result<(), Error> {
    if !web_vitals && !dom_mutations {
        return Ok(());
    }
    // Borrow `&'static str` directly when only one is requested; pay the
    // String allocation only for the merge case.
    let combined: std::borrow::Cow<'static, str> = match (web_vitals, dom_mutations) {
        (true, true) => format!("{WEB_VITALS_SETUP_JS}{DOM_MUTATIONS_SETUP_JS}").into(),
        (true, false) => WEB_VITALS_SETUP_JS.into(),
        (false, true) => DOM_MUTATIONS_SETUP_JS.into(),
        (false, false) => unreachable!(), // handled above
    };
    page.execute(AddScriptToEvaluateOnNewDocumentParams::new(
        combined.into_owned(),
    ))
    .await?;
    Ok(())
}

/// Enable the CDP `Performance` domain pre-navigation, **only** when the
/// request actually wants metrics. CDP requires the domain to be enabled
/// before `Performance.getMetrics` will return a payload (otherwise it
/// errors with "Performance domain is not enabled"). Hoisting the enable
/// into the parallel apply stage saves a serial CDP RTT later in the
/// format stage — `collect_page_metrics` then only needs the single
/// `getMetrics` call.
///
/// Skipped entirely when `enabled=false` so feature-off requests pay
/// nothing. Counters (`ScriptDuration`, `LayoutDuration`, etc.) are
/// tracked by Chrome regardless of domain enable state, so enabling
/// early doesn't add page overhead — it only unlocks the read endpoint.
pub async fn apply_performance_enable(page: &Page, enabled: bool) -> Result<(), Error> {
    if !enabled {
        return Ok(());
    }
    page.execute(PerformanceEnableParams::default()).await?;
    Ok(())
}

/// Pre-navigation setup for CSS / JS coverage collection. Runs only when
/// `enabled=true` — when off, this is a single bool test and zero CDP
/// traffic.
///
/// What runs (in order, all required before `page.goto`):
///   1. `Profiler.enable` + `Profiler.startPreciseCoverage`
///      (`call_count=false, detailed=true, allowTriggeredUpdates=false`)
///      — block-granularity coverage with the minimum data we need.
///      `call_count=false` keeps payload small (we only care whether a
///      byte was executed, not how many times).
///   2. `DOM.enable` then `CSS.enable` (CSS domain depends on DOM).
///   3. `CSS.startRuleUsageTracking` — starts the per-stylesheet
///      rule-execution tracking that `stopRuleUsageTracking` later
///      drains.
///
/// Both Profiler and CSS keep instrumentation state for the full page
/// lifetime; they're stopped + drained in `collect_summary`'s finalize
/// stage when coverage is requested.
pub async fn apply_coverage_setup(page: &Page, enabled: bool) -> Result<(), Error> {
    if !enabled {
        return Ok(());
    }
    // Profiler first — JS coverage starts before any script can run.
    page.execute(ProfilerEnableParams::default()).await?;
    let start = StartPreciseCoverageParams {
        call_count: Some(false),
        detailed: Some(true),
        allow_triggered_updates: Some(false),
    };
    page.execute(start).await?;
    // CSS requires DOM; both must be enabled before rule-usage tracking.
    page.execute(DomEnableParams::default()).await?;
    page.execute(CssEnableParams::default()).await?;
    page.execute(StartRuleUsageTrackingParams::default())
        .await?;
    Ok(())
}

/// DOM mutation hotspot collector. Installs a `MutationObserver` before any
/// user script runs, so initial hydration / SSR ↔ CSR mount / framework
/// reconciliation all get counted.
///
/// **Carefully tuned for low overhead:**
/// - Only reads cached node properties (`nodeName`, `attributeName`) — never
///   triggers layout (no `getBoundingClientRect`, no `offsetWidth`).
/// - `characterData: false` — React/Vue text interpolation produces *huge*
///   numbers of these (every `{name}` interpolation), drowning out signal.
/// - Aggregates into two small maps (by tag + by attribute name) + scalar
///   counters; never stores raw `MutationRecord` objects (avoids memory
///   blow-up — a single batch can deliver thousands).
/// - `attributeOldValue: false` keeps the browser from snapshotting old
///   strings just so we can throw them away.
///
/// Typical cost: <5ms total observation overhead on a heavy SPA with 10k+
/// mutations. The DOM mutations themselves were going to happen anyway —
/// we just count them.
const DOM_MUTATIONS_SETUP_JS: &str = r#"
(function() {
  if (window.__dom_mutations) return;
  const start = performance.now();
  const byTag = Object.create(null);
  const byAttr = Object.create(null);
  let added = 0, removed = 0, attrCount = 0;
  const obs = new MutationObserver(records => {
    for (let i = 0; i < records.length; i++) {
      const r = records[i];
      if (r.type === 'childList') {
        const t = r.target && r.target.nodeName ? r.target.nodeName.toLowerCase() : '?';
        const a = r.addedNodes.length, rem = r.removedNodes.length;
        added += a;
        removed += rem;
        byTag[t] = (byTag[t] || 0) + a + rem;
      } else if (r.type === 'attributes') {
        attrCount++;
        const n = r.attributeName || '?';
        byAttr[n] = (byAttr[n] || 0) + 1;
      }
    }
  });
  // observe(document, ...) is valid before <html> exists; observer is
  // queued and starts firing once the document has any children.
  obs.observe(document, {
    subtree: true,
    childList: true,
    attributes: true,
    characterData: false,
    attributeOldValue: false,
    characterDataOldValue: false,
  });
  window.__dom_mutations = {
    start_ms: start,
    by_tag: byTag,
    by_attr: byAttr,
    counts: () => ({ added, removed, attr: attrCount }),
  };
})();
"#;

/// Drain the mutation accumulator. Returns raw maps + counters; server-side
/// code sorts and trims to top-N. Run as late as possible so observation
/// covers the full render window.
const DOM_MUTATIONS_READ_JS: &str = r#"
(function() {
  const m = window.__dom_mutations;
  if (!m) return null;
  const c = m.counts();
  return {
    total_added_nodes: c.added,
    total_removed_nodes: c.removed,
    total_attribute_changes: c.attr,
    observation_window_ms: Math.round(performance.now() - m.start_ms),
    by_tag: m.by_tag,
    by_attr: m.by_attr,
  };
})()
"#;

/// Read mutation accumulator and project the raw JS object into our typed
/// `DomMutations` (sorted, trimmed top-N).
pub async fn collect_dom_mutations(page: &Page) -> Result<Option<DomMutations>, Error> {
    let eval = page.evaluate(DOM_MUTATIONS_READ_JS).await?;
    // Raw shape from JS: maps as Object<string, number>.
    #[derive(Deserialize)]
    struct Raw {
        total_added_nodes: u64,
        total_removed_nodes: u64,
        total_attribute_changes: u64,
        observation_window_ms: u64,
        by_tag: HashMap<String, u64>,
        by_attr: HashMap<String, u64>,
    }
    let raw: Option<Raw> = eval
        .into_value()
        .map_err(|e| Error::Cdp(format!("dom_mutations decode: {e}")))?;
    Ok(raw.map(|r| {
        let top_tags = top_n_counts(r.by_tag, 10);
        let top_attrs = top_n_counts(r.by_attr, 10);
        DomMutations {
            total_added_nodes: r.total_added_nodes,
            total_removed_nodes: r.total_removed_nodes,
            total_attribute_changes: r.total_attribute_changes,
            observation_window_ms: r.observation_window_ms,
            top_tags_by_mutation_count: top_tags,
            top_attributes_changed: top_attrs,
        }
    }))
}

/// Sort a `name → count` map by count desc (ties broken by name asc for
/// stable output), take top `limit`. Used by both tag and attribute tables.
fn top_n_counts(map: HashMap<String, u64>, limit: usize) -> Vec<MutationCount> {
    let mut v: Vec<(String, u64)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v.into_iter()
        .map(|(name, count)| MutationCount { name, count })
        .collect()
}

/// Disable JS execution in the page. Subsequent navigations render only the
/// static HTML/CSS — no JS runs, no SPA hydration. Useful for fast static
/// scraping. Must be called before navigation.
pub async fn apply_disable_javascript(page: &Page, disabled: bool) -> Result<(), Error> {
    if !disabled {
        return Ok(());
    }
    page.execute(SetScriptExecutionDisabledParams::new(true))
        .await?;
    Ok(())
}

/// Toggle the browser's HTTP cache for this page. When `disabled = true`,
/// every subsequent request bypasses both disk and memory caches (and the
/// served response's `from_cache` flag stays false). Per-page scope — does
/// not affect other pages or global cache state. Must be called before the
/// navigation whose requests should bypass cache.
pub async fn set_cache_disabled(page: &Page, disabled: bool) -> Result<(), Error> {
    // Network domain must be enabled for setCacheDisabled to take effect.
    page.execute(NetworkEnableParams::default()).await?;
    page.execute(SetCacheDisabledParams::new(disabled)).await?;
    Ok(())
}

/// Inject cookies into the browser's cookie jar **before navigation**. Each
/// pair becomes a `Network.CookieParam` with `url = target_url`, letting CDP
/// derive `domain`, `path`, `secure`, `sameSite` from that URL.
pub async fn set_cookies(
    page: &Page,
    cookies: &[(String, String)],
    target_url: &str,
) -> Result<(), Error> {
    if cookies.is_empty() {
        return Ok(());
    }
    let params: Vec<CookieParam> = cookies
        .iter()
        .map(|(name, value)| {
            let mut p = CookieParam::new(name.clone(), value.clone());
            p.url = Some(target_url.to_string());
            p
        })
        .collect();
    page.execute(SetCookiesParams::new(params)).await?;
    Ok(())
}

/// Inject extra HTTP request headers (Authorization, X-Api-Key, custom
/// tracing IDs, etc.) into every subsequent request from this page.
/// Per-page scope. Must be called before navigation.
pub async fn apply_extra_headers(
    page: &Page,
    headers: &std::collections::HashMap<String, String>,
) -> Result<(), Error> {
    if headers.is_empty() {
        return Ok(());
    }
    page.execute(NetworkEnableParams::default()).await?;
    let value = serde_json::to_value(headers)
        .map_err(|e| Error::InvalidInput(format!("headers serialize: {e}")))?;
    page.execute(SetExtraHttpHeadersParams::new(Headers::new(value)))
        .await?;
    Ok(())
}

/// Block any request whose URL contains any of the given substrings. Each
/// substring is wrapped as `*<pattern>*` for CDP wildcard matching, so it
/// behaves like `String::contains`. Useful for stripping ads/analytics/
/// trackers, both speeding up loads and reducing noise in `stat.resources`.
/// Per-page scope. Must be called before navigation.
pub async fn apply_blocked_urls(page: &Page, patterns: &[String]) -> Result<(), Error> {
    if patterns.is_empty() {
        return Ok(());
    }
    page.execute(NetworkEnableParams::default()).await?;
    let url_patterns: Vec<BlockPattern> = patterns
        .iter()
        .map(|p| BlockPattern::new(format!("*{p}*"), true))
        .collect();
    page.execute(SetBlockedUrLsParams {
        url_patterns: Some(url_patterns),
    })
    .await?;
    Ok(())
}

pub async fn apply_user_agent(
    page: &Page,
    user_agent: Option<&str>,
    accept_language: Option<&str>,
    default_user_agent: &str,
) -> Result<(), Error> {
    // Always apply — even when the caller didn't override anything —
    // because the `default_user_agent` we hold (see `DEFAULT_USER_AGENT`)
    // is a WAF-safe pinned string that intentionally differs from the
    // raw Chromium binary UA (which contains `HeadlessChrome` and trips
    // most production WAFs). Skipping the CDP call here would leave the
    // page on the raw binary UA, defeating the whole point.
    //
    // Cost: one extra `Network.setUserAgentOverride` CDP RTT per page
    // when both fields are absent (~5ms over local loopback, absorbed
    // into the parallel apply-stage `try_join!`). Cheap insurance.
    let ua = user_agent
        .map(String::from)
        .unwrap_or_else(|| default_user_agent.to_string());
    let mut params = SetUserAgentOverrideParams::new(ua);
    params.accept_language = accept_language.map(String::from);
    page.execute(params).await?;
    Ok(())
}

/// Poll `document.querySelector(selector)` with exponential backoff
/// (10ms → ×2 → cap 200ms) until it matches or `timeout` elapses.
pub async fn wait_for_selector(
    page: &Page,
    selector: &str,
    timeout: Duration,
) -> Result<(), Error> {
    const INITIAL_INTERVAL: Duration = Duration::from_millis(10);
    const MAX_INTERVAL: Duration = Duration::from_millis(200);

    let deadline = Instant::now() + timeout;
    let mut interval = INITIAL_INTERVAL;
    loop {
        if page.find_element(selector).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout(format!(
                "selector {selector} (after {}ms)",
                timeout.as_millis()
            )));
        }
        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(MAX_INTERVAL);
    }
}

/// Poll a JavaScript expression until it returns a truthy value (per JS
/// semantics: `true`, non-zero number, non-empty string/array/object) or
/// `timeout` elapses. More flexible than `wait_for_selector` — express any
/// business condition: cart non-empty, item count >= N, custom JS flag, etc.
/// Exception in the expression is treated as falsy (keep polling).
pub async fn wait_for_function(
    page: &Page,
    expression: &str,
    timeout: Duration,
) -> Result<(), Error> {
    const INITIAL_INTERVAL: Duration = Duration::from_millis(10);
    const MAX_INTERVAL: Duration = Duration::from_millis(200);

    let deadline = Instant::now() + timeout;
    let mut interval = INITIAL_INTERVAL;
    loop {
        if let Ok(result) = page.evaluate(expression).await
            && let Ok(value) = result.into_value::<serde_json::Value>()
            && is_truthy(&value)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout(format!(
                "function (after {}ms)",
                timeout.as_millis()
            )));
        }
        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(MAX_INTERVAL);
    }
}

fn is_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// Read a string-valued DOM property (`outerHTML`, `innerText`, ...) from the
/// first element matching `selector`. `property` is interpolated into JS as
/// an identifier — caller must guarantee it is safe (never user-derived).
pub async fn capture_property(
    page: &Page,
    selector: &str,
    property: &str,
    timeout: Duration,
) -> Result<String, Error> {
    wait_for_selector(page, selector, timeout).await?;
    let escaped = serde_json::to_string(selector)
        .map_err(|e| Error::InvalidInput(format!("selector escape: {e}")))?;
    let expr = format!(
        "(() => {{ const el = document.querySelector({escaped}); return el ? el.{property} : null; }})()"
    );
    let evaluation = page.evaluate(expr).await?;
    let value: Option<String> = evaluation
        .into_value()
        .map_err(|e| Error::Cdp(format!("decode: {e}")))?;
    value.ok_or_else(|| Error::NotFound(selector.to_string()))
}

/// DOM walker — extracts visible text, inserting newlines between elements
/// whose computed `display` is block-like. Handles flex/grid layouts where
/// `innerText` alone collapses everything into one line.
const TEXT_EXTRACTOR_JS: &str = r#"(function(sel) {
    const SKIP = new Set(['SCRIPT','STYLE','NOSCRIPT','IFRAME','SVG','TEMPLATE','HEAD']);
    const BLOCK = /^(block|flex|grid|list-item|table|flow-root)/;
    const root = sel ? document.querySelector(sel) : document.body;
    if (!root) return null;
    let out = '';
    function walk(node) {
        if (node.nodeType === 3) {
            out += node.textContent.replace(/\s+/g, ' ');
            return;
        }
        if (node.nodeType !== 1) return;
        if (SKIP.has(node.tagName)) return;
        const style = getComputedStyle(node);
        if (style.visibility === 'hidden' || style.display === 'none') return;
        const isBlock = BLOCK.test(style.display);
        if (isBlock && out && !out.endsWith('\n')) out += '\n';
        for (const child of node.childNodes) walk(child);
        if (isBlock && !out.endsWith('\n')) out += '\n';
    }
    walk(root);
    return out
        .replace(/[ \t]+/g, ' ')
        .replace(/ ?\n ?/g, '\n')
        .replace(/\n{3,}/g, '\n\n')
        .trim();
})"#;

pub async fn extract_text(
    page: &Page,
    selector: Option<&str>,
    timeout: Duration,
) -> Result<String, Error> {
    if let Some(sel) = selector {
        wait_for_selector(page, sel, timeout).await?;
    }
    let sel_arg = match selector {
        Some(s) => serde_json::to_string(s)
            .map_err(|e| Error::InvalidInput(format!("selector escape: {e}")))?,
        None => "null".to_string(),
    };
    let expr = format!("{TEXT_EXTRACTOR_JS}({sel_arg})");
    let evaluation = page.evaluate(expr).await?;
    let value: Option<String> = evaluation
        .into_value()
        .map_err(|e| Error::Cdp(format!("decode: {e}")))?;
    value.ok_or_else(|| Error::NotFound(selector.unwrap_or("document.body").to_string()))
}

/// Serializes the DOM to an HTML string, rewriting any custom element (tag
/// name containing `-`) into `<div>` or `<span>` based on its computed
/// `display`. Attributes and children preserved. The live DOM is not mutated.
const DOM_NORMALIZE_JS: &str = r#"(function(sel) {
    const SKIP = new Set(['SCRIPT','STYLE','NOSCRIPT','IFRAME','SVG','HEAD','TEMPLATE']);
    const BLOCK = /^(block|flex|grid|list-item|table|flow-root)/;
    const VOID = new Set(['area','base','br','col','embed','hr','img','input','link','meta','source','track','wbr']);
    const root = sel ? document.querySelector(sel) : document.body;
    if (!root) return null;
    function escText(s) {
        return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
    }
    function escAttr(s) {
        return s.replace(/&/g,'&amp;').replace(/"/g,'&quot;');
    }
    function attrs(node) {
        let s = '';
        for (const a of node.attributes) s += ' ' + a.name + '="' + escAttr(a.value) + '"';
        return s;
    }
    let out = '';
    function walk(node) {
        if (node.nodeType === 3) { out += escText(node.textContent); return; }
        if (node.nodeType !== 1) return;
        if (SKIP.has(node.tagName)) return;
        let tag = node.tagName.toLowerCase();
        if (tag.includes('-')) {
            const display = getComputedStyle(node).display;
            tag = BLOCK.test(display) ? 'div' : 'span';
        }
        out += '<' + tag + attrs(node) + '>';
        if (!VOID.has(tag)) {
            for (const child of node.childNodes) walk(child);
            out += '</' + tag + '>';
        }
    }
    walk(root);
    return out;
})"#;

pub async fn normalize_dom(
    page: &Page,
    selector: Option<&str>,
    timeout: Duration,
) -> Result<String, Error> {
    if let Some(sel) = selector {
        wait_for_selector(page, sel, timeout).await?;
    }
    let sel_arg = match selector {
        Some(s) => serde_json::to_string(s)
            .map_err(|e| Error::InvalidInput(format!("selector escape: {e}")))?,
        None => "null".to_string(),
    };
    let expr = format!("{DOM_NORMALIZE_JS}({sel_arg})");
    let evaluation = page.evaluate(expr).await?;
    let value: Option<String> = evaluation
        .into_value()
        .map_err(|e| Error::Cdp(format!("decode: {e}")))?;
    value.ok_or_else(|| Error::NotFound(selector.unwrap_or("document.body").to_string()))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WebPageStat {
    pub total_size: u64,
    /// Number of network resources observed during load. Always populated
    /// (functional-validation signal) even when `resources` is opted out
    /// of being serialised — preserves "page loaded N resources" without
    /// shipping the detailed array.
    pub resource_count: u64,
    pub fcp_time: u32,
    pub dcl_time: u32,
    pub load_time: u32,
    /// Page content. `collect_summary` populates this with raw HTML from
    /// `page.content()`; callers may overwrite with text/markdown afterwards.
    pub data: String,
    /// Raw uncaught exception strings (one per `Runtime.exceptionThrown`),
    /// formatted as `<line>:<col> <text> | <description>`. Kept for
    /// forensic detail; for an AI-scannable count + per-class breakdown
    /// see `js_exceptions`.
    pub exceptions: Vec<String>,
    /// Counted / classified rollup of `exceptions[]` for monitoring.
    /// Always populated (`total: 0`, `by_name: []` when no exceptions).
    pub js_exceptions: JsExceptions,
    /// `console.log/info/warn/error/debug` calls observed during the page
    /// lifecycle, formatted as `[<level>] <args>`. Distinct from uncaught
    /// runtime exceptions, which live in `exceptions` / `js_exceptions`.
    pub console_messages: Vec<String>,
    pub resources: Vec<WebPageResource>,
    /// Cookies in the page's cookie jar at snapshot time (scoped to the
    /// target URL). Useful for session-continuation workflows.
    pub cookies: Vec<Cookie>,
    pub screenshot: Option<Screenshot>,
    /// Base64-encoded PDF render of the page (CDP `Page.printToPDF`).
    /// Populated only when `SummaryRequest.pdf = true`.
    pub pdf: Option<Pdf>,
    /// HAR 1.2 archive derived from `resources` + page lifecycle timings.
    /// Populated only when `SummaryRequest.har = true`. Some fields are
    /// best-effort (request headers / method default to GET, body sizes
    /// approximate) because we don't capture `requestWillBeSent` payloads.
    pub har: Option<serde_json::Value>,
    /// `DOMSnapshot.captureSnapshot` result (documents + string table).
    /// Populated only when `SummaryRequest.save_dom_snapshot = true`.
    pub dom_snapshot: Option<serde_json::Value>,
    /// Core Web Vitals (LCP / CLS / TBT / TTFB) + long-task count.
    /// Populated only when `SummaryRequest.web_vitals = true`.
    pub web_vitals: Option<WebVitals>,
    /// V8 heap + DOM counters + CPU time breakdown from CDP
    /// `Performance.getMetrics`. Populated only when
    /// `SummaryRequest.metrics = true`.
    pub metrics: Option<PageMetrics>,
    /// Aggregated stats derived server-side from `resources`. Always
    /// populated (free to compute — no extra CDP calls). Gives AI
    /// comparison-friendly scalars: bytes by type, count by type, status
    /// distribution, cache hit ratio, etc.
    pub resource_summary: ResourceSummary,
    /// Page metadata (title / meta / OG / canonical / robots / ...).
    /// Populated only when `SummaryRequest.metadata = true`. SEO and
    /// correctness regression signal.
    pub metadata: Option<PageMetadata>,
    /// Resources in `<head>` that block the initial render: sync
    /// stylesheets and sync (non-async/defer/module) scripts. Populated
    /// only when `SummaryRequest.render_blocking = true`.
    pub render_blocking_resources: Option<Vec<RenderBlocker>>,
    /// Security-related response headers from the main document
    /// (last-seen Document-type response). Filtered to a fixed set
    /// (CSP, HSTS, X-Frame-Options, Referrer-Policy, etc.). Always
    /// populated when at least one Document response was observed.
    pub security_headers: Option<HashMap<String, String>>,
    /// AI-scannable security scorecard derived from the already-captured
    /// `security_headers` and `cookies`. Always populated — when
    /// there's nothing to check (HTTP page, no cookies) every count is
    /// `0` and every bool is `false`, which is itself a meaningful
    /// signal. See `SecurityAudit` doc for what's covered and what's
    /// intentionally out of scope.
    pub security_audit: SecurityAudit,
    /// Service Worker / PWA registration state. Populated only when
    /// `SummaryRequest.service_worker = true`.
    pub service_worker: Option<ServiceWorkerStatus>,
    /// TLS / certificate info of the main document. Populated when the
    /// response actually used TLS (HTTPS) and CDP reported
    /// `securityDetails`. `None` for HTTP, file://, or unsupported.
    pub tls_info: Option<TlsInfo>,
    /// Certificates for **all** HTTPS hosts encountered while loading the
    /// page (main document + JS/CSS/font/image CDNs etc.), deduplicated by
    /// host. Sorted by `days_remaining` ascending so soonest-to-expire
    /// certificates appear first — useful for security/expiry audit across
    /// third-party CDNs. Includes the main document's host as well.
    pub tls_certificates: Vec<TlsInfo>,
    /// Per-`<img>` sizing audit. Each entry compares decoded source
    /// dimensions vs laid-out display dimensions, optionally joined with
    /// the network resource's transferred byte count. Populated only when
    /// `SummaryRequest.image_sizing = true`. Sorted by `waste_ratio` desc
    /// (worst offenders first), with unknown ratios trailing.
    pub image_sizing: Option<Vec<ImageSizing>>,
    /// Image-audit roll-up — Lighthouse-aligned "image" four-pack:
    /// `oversized` (natural / effective-display > 2×), `missing_dimensions`
    /// (no `width`/`height` attr → CLS), `missing_lazy` (below-the-fold
    /// fetched eagerly), `missing_srcset` (no responsive variants).
    /// Populated alongside `image_sizing` (derived from the same data,
    /// no extra browser interaction). `None` when `image_sizing` is off.
    pub image_audit: Option<ImageAudit>,
    /// Font-loading audit: `font-display` distribution, FOIT-risk
    /// `@font-face` list, preload coverage scalar, CORS blind-spot count.
    /// Populated only when `SummaryRequest.font_audit = true` (OR-merged
    /// with `all_metrics`). One extra `page.evaluate` over CSSOM (~3–8ms
    /// depending on stylesheet count).
    pub font_audit: Option<FontAudit>,
    /// DOM mutation hotspot summary captured via pre-navigation
    /// `MutationObserver`. Populated only when
    /// `SummaryRequest.dom_mutations = true`. Useful for diagnosing
    /// render thrash / over-eager reconciliation regressions.
    pub dom_mutations: Option<DomMutations>,
    /// HTTP error rollup (4xx / 5xx lists, network failures, final URL,
    /// redirect count). Populated only when `SummaryRequest.http_errors =
    /// true`. The fastest path to "is this page broken / hijacked /
    /// redirected somewhere unexpected" for monitoring use cases.
    pub http_errors: Option<HttpErrors>,
    /// CSS / JS code coverage — Lighthouse "Reduce unused CSS/JS" feed.
    /// Populated only when `SummaryRequest.coverage = true`. NOT enabled
    /// by `all_metrics=true` (coverage stays opt-in due to its
    /// instrumentation cost).
    pub coverage: Option<CoverageReport>,
    /// Phase-by-phase timing for the main document (DNS / TCP / TLS /
    /// TTFB). Always emitted when a Document-type response with timing
    /// data was observed — `None` for full-cache or unusual flows.
    /// Surfaced top-level so AI / monitors can do "server slow vs
    /// frontend slow" first-triage without scanning `resources[]`.
    pub document_timing: Option<DocumentTiming>,
}

/// TLS / certificate snapshot for a single host. `days_remaining` is
/// derived at capture time from `valid_to` minus wall-clock `now`; negative
/// when the cert is already expired.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsInfo {
    /// Hostname this certificate was observed on (e.g. `cdn.example.com`).
    /// Lets callers attribute a cert to its origin when multiple appear in
    /// `tls_certificates`.
    pub host: String,
    /// Remote IP address the browser actually connected to for this host,
    /// as reported by `Network.responseReceived.response.remoteIPAddress`.
    /// Useful for diffing DNS/CDN routing across captures (cert says
    /// `*.example.com` but resolves to an unexpected IP/region → MITM /
    /// hijack signal). `None` for cached responses or local schemes.
    pub remote_ip: Option<String>,
    /// Remote port (almost always 443 for HTTPS, but recorded for
    /// completeness). `None` when CDP didn't report it.
    pub remote_port: Option<u16>,
    /// TLS protocol version, e.g. `"TLS 1.3"`.
    pub protocol: String,
    /// Cipher suite name.
    pub cipher: String,
    /// Key-exchange algorithm (often empty in TLS 1.3 where it's negotiated).
    pub key_exchange: Option<String>,
    /// Subject CN of the leaf certificate.
    pub subject_name: String,
    /// Issuer CN (CA name).
    pub issuer: String,
    /// Cert "not before" — Unix epoch seconds.
    pub valid_from: f64,
    /// Cert "not after" — Unix epoch seconds.
    pub valid_to: f64,
    /// `(valid_to - now) / 86400`. Negative when expired.
    pub days_remaining: i64,
    /// Subject Alternative Names.
    pub san_list: Vec<String>,
}

/// Standard set of security-relevant response headers we extract from the
/// main document. Names are matched case-insensitively; output preserves
/// canonical capitalisation from this list.
const SECURITY_HEADER_NAMES: &[&str] = &[
    "Content-Security-Policy",
    "Content-Security-Policy-Report-Only",
    "Strict-Transport-Security",
    "X-Frame-Options",
    "X-Content-Type-Options",
    "Referrer-Policy",
    "Permissions-Policy",
    "Cross-Origin-Embedder-Policy",
    "Cross-Origin-Opener-Policy",
    "Cross-Origin-Resource-Policy",
    "X-XSS-Protection",
];

/// The "core enforced" subset of the above — headers that actually block
/// something (clickjacking, MIME sniffing, mixed content, popup-tab
/// isolation). Absence of any one is treated as a finding by
/// `SecurityAudit.headers.missing`. Excludes:
///   - `Content-Security-Policy-Report-Only` — report-only doesn't block.
///   - `X-XSS-Protection` — deprecated; modern browsers ignore it.
///   - `Cross-Origin-Embedder-Policy` / `Cross-Origin-Resource-Policy` —
///     situational (only relevant for cross-origin isolation use cases).
const CORE_SECURITY_HEADERS: &[&str] = &[
    "Strict-Transport-Security",
    "Content-Security-Policy",
    "X-Frame-Options",
    "X-Content-Type-Options",
    "Referrer-Policy",
    "Permissions-Policy",
    "Cross-Origin-Opener-Policy",
];

/// Security-config scorecard for AI / monitoring. Pure derive from
/// already-captured `WebPageStat.security_headers` + `cookies` — no
/// extra browser interaction. Always populated: when both inputs are
/// empty (HTTP page with no cookies), every count is `0` and every
/// header bool is `false`, which is itself a useful signal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityAudit {
    pub headers: SecurityHeadersCheck,
    pub cookies: CookieSecurityCheck,
}

/// Boolean presence flags for the most commonly-required security
/// response headers on the main document. `present_count` and `missing`
/// give an AI-scannable summary that doesn't require parsing the full
/// header map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityHeadersCheck {
    /// `Strict-Transport-Security` — enforces HTTPS-only for the
    /// configured `max-age`. Absence on an HTTPS site lets attackers
    /// downgrade subsequent connections.
    pub hsts: bool,
    /// `Content-Security-Policy` (enforcing variant). Absence means no
    /// browser-side XSS / inline-script mitigation.
    pub csp: bool,
    /// `Content-Security-Policy-Report-Only` — for monitoring CSP
    /// rollout without blocking. Exposed separately so `csp=false &&
    /// csp_report_only=true` (a common pre-enforcement state) is
    /// distinguishable from "no CSP at all".
    pub csp_report_only: bool,
    /// `X-Frame-Options` — clickjacking mitigation. Modern equivalent
    /// is CSP `frame-ancestors`; either one being present is a real
    /// signal, but this field tracks the legacy header specifically.
    pub x_frame_options: bool,
    /// `X-Content-Type-Options: nosniff` — blocks MIME sniffing of
    /// script/style content type. Header presence checked; value
    /// (`nosniff` is the only valid one) is not validated here.
    pub x_content_type_options: bool,
    /// `Referrer-Policy` — controls how much referrer data leaks
    /// cross-origin.
    pub referrer_policy: bool,
    /// `Permissions-Policy` (formerly `Feature-Policy`) — opts in/out
    /// of powerful browser features (camera / mic / geolocation / ...).
    pub permissions_policy: bool,
    /// `Cross-Origin-Opener-Policy` — isolates the page's browsing
    /// context from cross-origin popups (mitigates Spectre + tab-nabbing).
    pub coop: bool,
    /// `Cross-Origin-Embedder-Policy` — required for cross-origin
    /// isolation (e.g. `SharedArrayBuffer`). Optional / situational, so
    /// **not** counted in `present_count` / `missing`.
    pub coep: bool,
    /// Count of the 7 core enforced headers that were present
    /// (`hsts` + `csp` + `x_frame_options` + `x_content_type_options` +
    /// `referrer_policy` + `permissions_policy` + `coop`). Ranges
    /// `0..=7`. Single scalar so monitors can alert on a deploy that
    /// drops a header.
    pub present_count: u32,
    /// Canonical names of the core headers that were missing. Empty
    /// when all 7 are present.
    pub missing: Vec<String>,
}

/// Coverage statistics for cookie-security attributes across the page's
/// cookie jar. Use ratios (`secure / total`, etc.) to track rollout of
/// secure-cookie policies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CookieSecurityCheck {
    pub total: u32,
    /// Cookies with the `Secure` attribute — only sent over HTTPS.
    pub secure: u32,
    /// Cookies with `HttpOnly` — not exposed to `document.cookie`,
    /// limits XSS-driven session theft.
    pub http_only: u32,
    /// Cookies with a `SameSite` attribute set (any of `Strict` / `Lax`
    /// / `None`). "Not set" lets the browser fall back to its legacy
    /// default, which has shifted over time and is a fingerprint of
    /// stale cookie configuration.
    pub same_site_set: u32,
    /// Anti-pattern: `SameSite=None` without `Secure`. Chrome / Firefox
    /// reject these cookies outright — the page is shipping cookies
    /// that won't be accepted. Non-zero is always actionable.
    pub same_site_none_without_secure: u32,
    /// Estimated `Cookie:` request-header byte size when **every**
    /// cookie in the jar applies (worst-case same-origin GET). Sum
    /// of `name + "=" + value` plus `"; "` separators. Servers and
    /// CDNs typically cap inbound headers at 8 KB and many web
    /// frameworks at 4 KB — values approaching either are a real
    /// per-request tax (every navigation, every XHR pays this).
    pub header_bytes: u64,
}

/// Snapshot of the page's Service Worker registration. `None` for the
/// whole struct means navigator.serviceWorker isn't available (rare); the
/// individual fields go to None / false when no registration exists.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceWorkerStatus {
    /// True if a SW currently controls this page (`navigator.serviceWorker.controller`).
    pub controlled: bool,
    /// Registration scope URL.
    pub scope: Option<String>,
    /// Script URL of the active SW.
    pub active_script: Option<String>,
    /// True if a waiting SW exists (update pending activation).
    pub waiting: bool,
    /// True if an installing SW exists.
    pub installing: bool,
}

/// DOM mutation hotspot summary. Aggregates `MutationRecord` deltas observed
/// from before-navigation install through end-of-capture. Counts are
/// gross — `total_added_nodes + total_removed_nodes` over-counts churn
/// (the same `<li>` re-rendered N times is 2N), but that's precisely the
/// "render thrash" signal we want.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomMutations {
    /// Total `childList` additions across all observed mutations.
    pub total_added_nodes: u64,
    /// Total `childList` removals.
    pub total_removed_nodes: u64,
    /// Total `attributes` mutations (class/style/aria/etc.).
    pub total_attribute_changes: u64,
    /// Wall-clock time the observer was active, from setup → drain.
    /// Lets callers normalise (mutations/second) for comparison.
    pub observation_window_ms: u64,
    /// Tags with the most mutation count (added+removed touching them as
    /// the parent). Top 10. Helps identify "what kind of element churns".
    pub top_tags_by_mutation_count: Vec<MutationCount>,
    /// Attributes most frequently changed. Top 10. `class` and `style`
    /// dominating signals heavy animation / state toggle churn.
    pub top_attributes_changed: Vec<MutationCount>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MutationCount {
    /// Tag name (lowercased) or attribute name.
    pub name: String,
    pub count: u64,
}

/// Bucketed view of uncaught JS exceptions captured during the page
/// lifecycle. Derived from the same `Runtime.exceptionThrown` stream that
/// fills `WebPageStat.exceptions[]` — that field stays for forensic
/// detail; this one exists so AI / dashboards can scan a single scalar
/// (`total`) and a short ranked list (`by_name`) to spot regressions
/// like "today this page has 12 ReferenceErrors vs. 0 yesterday".
///
/// Always populated (no opt-in) — costs essentially nothing since the
/// exception stream is already always subscribed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JsExceptions {
    /// Total uncaught exceptions observed. Equal to `exceptions.len()`.
    /// Single scalar so monitors can alert on `>0` (or on
    /// deltas across captures) without parsing detail strings.
    pub total: u32,
    /// Per-exception-class roll-up, sorted by `count` descending (ties
    /// broken by `name` ascending). Class name comes from CDP
    /// `RemoteObject.className` (`TypeError`, `ReferenceError`,
    /// `SyntaxError`, custom error subclasses, ...). When CDP didn't
    /// provide one — e.g. `throw "literal string"` — the entry is
    /// classified as `"Other"`. Capped at the 10 most frequent classes
    /// to keep the payload bounded.
    pub by_name: Vec<JsExceptionCount>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JsExceptionCount {
    /// Exception class name (`TypeError` / `ReferenceError` / custom /
    /// `Other`).
    pub name: String,
    pub count: u32,
    /// First-seen message text for this class, truncated to 200 chars.
    /// Lets AI / humans see the actual error without expanding the full
    /// `exceptions[]` list. `None` when CDP returned no description.
    pub sample_message: Option<String>,
}

/// HTTP-layer health snapshot for AI-friendly anomaly detection / "is the
/// page broken?" checks. Populated only when `SummaryRequest.http_errors`
/// is true. Derived from the response stream + an extra
/// `Network.loadingFailed` listener — no `evaluate` calls, no per-resource
/// overhead.
///
/// Three signals callers typically act on:
///   - `failed_count > 0` → at least one resource didn't load cleanly.
///   - `network_failures` non-empty → DNS / TLS / connection-refused
///     errors (CDP never produced a `responseReceived` for these, so they
///     would be invisible from `resources[]` alone).
///   - `final_url != requested URL` (or `redirect_count > 0`) → the
///     response came from somewhere unexpected (hijack / login-wall /
///     CDN-level redirect).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HttpErrors {
    /// `failed_4xx.len() + failed_5xx.len() + network_failures.len()`.
    /// Single scalar so monitors can alert on `>0` without parsing the
    /// detail lists.
    pub failed_count: u32,
    /// HTTP 4xx responses observed during page load. Includes the main
    /// document if it returned 4xx (no implicit filter on resource type).
    pub failed_4xx: Vec<FailedRequest>,
    /// HTTP 5xx responses.
    pub failed_5xx: Vec<FailedRequest>,
    /// Requests that never produced a response — DNS failure, TLS handshake
    /// abort, connection refused, blocked by CSP/CORS/extension, etc.
    /// Sourced from CDP `Network.loadingFailed`; `error_text` is Chromium's
    /// `net::ERR_*` constant verbatim.
    pub network_failures: Vec<NetworkFailure>,
    /// Final document URL after any HTTP 3xx redirects were followed.
    /// Equal to the requested URL when no redirect happened. Use
    /// `final_url != requested URL` as a "landed somewhere unexpected"
    /// signal.
    pub final_url: String,
    /// Number of HTTP 3xx Document responses observed before the final
    /// landing page. `0` for direct navigation; `N` for an N-hop redirect
    /// chain.
    pub redirect_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailedRequest {
    pub url: String,
    pub status: u32,
    /// CDP `ResourceType` lowercased (`document`, `script`, `image`, ...).
    /// Lets monitors distinguish "main doc 404" from "missing favicon".
    pub resource_type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkFailure {
    pub url: String,
    /// Chromium net-error constant
    /// (`net::ERR_NAME_NOT_RESOLVED`, `net::ERR_CONNECTION_REFUSED`,
    /// `net::ERR_CERT_DATE_INVALID`, ...). Source list:
    /// <https://cs.chromium.org/chromium/src/net/base/net_error_list.h>.
    pub error_text: String,
    pub resource_type: String,
    /// True when the failure was a deliberate cancellation (e.g. navigation
    /// superseded by another, or a `block_urls` policy hit). Lets callers
    /// filter out "expected" failures when alerting on real ones.
    pub canceled: bool,
}

/// CSS / JS code-coverage report — the Lighthouse "Reduce unused CSS /
/// JS" feed. Populated only when `SummaryRequest.coverage = true`.
///
/// **Not** enabled by `all_metrics=true`: precise V8 coverage
/// instruments every script for the page lifetime (disables some
/// optimizations) and CSS rule-usage tracking keeps style-engine state
/// for the whole load. The cost is small per page but real, so
/// coverage stays explicitly opt-in even when the caller asks for
/// "every analytical signal".
///
/// JS bytes are computed via the standard innermost-wins sweep over
/// `Profiler.takePreciseCoverage` ranges (a byte is "used" iff its
/// smallest enclosing range has `count > 0`). CSS bytes come from
/// `CSS.stopRuleUsageTracking` (each `RuleUsage` has a `used` flag and
/// start/end offsets within its stylesheet); per-stylesheet totals
/// from the `length` field on `CSS.styleSheetAdded` headers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoverageReport {
    /// Aggregate JavaScript source bytes seen across every script the
    /// V8 isolate reported coverage for (inline + external).
    pub js_total_bytes: u64,
    /// JS bytes that V8 marked as executed at least once.
    pub js_used_bytes: u64,
    /// `js_total_bytes - js_used_bytes` — the Lighthouse "unused JS"
    /// figure.
    pub js_unused_bytes: u64,
    /// `js_unused_bytes / js_total_bytes`, `0.0..1.0`. `0.0` when no JS
    /// was observed; AI / monitors can alert on a single scalar.
    pub js_unused_ratio: f64,
    /// CSS aggregates — same semantics as the JS counters, sourced from
    /// rule-usage tracking instead of V8 precise coverage.
    pub css_total_bytes: u64,
    pub css_used_bytes: u64,
    pub css_unused_bytes: u64,
    pub css_unused_ratio: f64,
    /// Top files ranked by `unused_bytes` descending (mixed JS + CSS),
    /// capped at 10. Direct feed for AI "what should I trim first?"
    /// suggestions. Files with `total_bytes == 0` are excluded (anonymous
    /// inline scripts that V8 reports but have no measurable size).
    pub top_unused: Vec<CoverageEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoverageEntry {
    pub url: String,
    /// `"js"` or `"css"`.
    pub kind: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub unused_bytes: u64,
    /// `unused_bytes / total_bytes`, `0.0..1.0`.
    pub unused_ratio: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RenderBlocker {
    /// `script` or `link`.
    pub tag: String,
    /// `href` (link) or `src` (script). `"(inline)"` for inline scripts.
    pub url: String,
    /// Short human-readable reason: `"sync stylesheet"`, `"no async/defer"`,
    /// `"inline blocking script"`.
    pub why: String,
}

/// Per-image sizing audit entry. Captured browser-side from already-decoded
/// `HTMLImageElement` properties (no extra IO). Server-side post-processing
/// correlates `url` with the network `resources` to fill `transferred_bytes`
/// and compute `waste_ratio`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageSizing {
    /// `img.currentSrc` (winning srcset/sizes candidate) — what the browser
    /// actually fetched. Falls back to `img.src` if `currentSrc` is empty.
    pub url: String,
    /// Decoded source pixel width. `0` when the image failed/skipped load.
    pub natural_width: u32,
    pub natural_height: u32,
    /// Laid-out CSS pixel size on the page (rounded from `getBoundingClientRect`).
    pub display_width: u32,
    pub display_height: u32,
    /// `window.devicePixelRatio` at capture time. Reflects any
    /// `device_scale_factor` emulation we applied. Used server-side to
    /// scale `display_*` up to the actual device pixels the browser needs
    /// before computing `waste_ratio` — a 2x DPR screen needs 2× the
    /// natural pixels per CSS pixel to render crisply.
    pub device_pixel_ratio: f64,
    /// `false` only for lazy images outside the viewport (we still emit
    /// them so callers can audit lazy coverage), or genuinely broken images.
    pub loaded: bool,
    /// `"eager"` (default) or `"lazy"`.
    pub loading: String,
    /// `"auto"`, `"async"`, or `"sync"`.
    pub decoding: String,
    /// True if any part of the image's box overlaps the initial viewport.
    /// Above-the-fold waste matters more than below-the-fold.
    pub in_viewport: bool,
    /// True when `<img>` has no `alt` attribute — quick a11y signal.
    pub alt_missing: bool,
    /// True iff the `<img>` tag has a literal `width="..."` attribute
    /// (CSS-set width does NOT count). Missing both `width` and
    /// `height` attrs is what causes CLS — the browser can't reserve
    /// layout space until the bytes decode.
    #[serde(default)]
    pub has_width_attr: bool,
    #[serde(default)]
    pub has_height_attr: bool,
    /// True iff the `<img>` carries a non-empty `srcset` attribute.
    /// Missing srcset = no responsive variants, same source ships to
    /// every viewport / DPR. The `<picture>` parent's source-set
    /// counts: when a `<picture>` wraps the img, the browser writes
    /// the chosen candidate's URL back to `img.currentSrc` and we
    /// see `srcset` on the `<source>` not the `<img>` — that case
    /// is correctly handled by Chrome populating srcset on the
    /// inner img too in the "no picture, no srcset attr" sense.
    #[serde(default)]
    pub has_srcset: bool,
    /// Bytes actually downloaded for `url`, joined from the `resources` map
    /// server-side. `None` when no matching resource entry exists (data:
    /// URLs, cached without a fresh request, cross-context).
    pub transferred_bytes: Option<u64>,
    /// `1 - (display_pixels / natural_pixels)`, clamped to `[0, 1]`. Higher
    /// = more wasted decoded pixels. `None` when either dimension is 0 or
    /// the image is using device-pixel scaling that makes the ratio
    /// misleading (DPR > 1 with reasonable oversize).
    pub waste_ratio: Option<f64>,
}

/// Image-audit roll-up. Lighthouse-aligned "image" four-pack — each
/// list maps directly to one AI suggestion. Populated only when
/// `SummaryRequest.image_sizing = true`; derived from the same
/// `image_sizing` data so there's no extra browser interaction.
/// All four lists capped at 20 entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageAudit {
    /// Natural pixels / effective display pixels (display × DPR) > 2.0.
    /// The browser decoded much more than it needed to draw. Sorted by
    /// `ratio` desc so the worst offenders surface first. AI suggestion:
    /// "serve a smaller variant" / "add `srcset`".
    pub oversized: Vec<ImageIssue>,
    /// `<img>` without BOTH `width` AND `height` attributes set on the
    /// tag. CLS contributor. Sorted by display area desc — larger
    /// missing-dimensions images cause bigger shifts. AI suggestion:
    /// "set explicit width/height to reserve layout space".
    pub missing_dimensions: Vec<ImageIssue>,
    /// Below-the-fold images (`in_viewport=false`) fetched eagerly —
    /// `loading != "lazy"`. Wasted bytes during initial load. Sorted
    /// by display area desc as a proxy for "how much they cost to
    /// fetch and decode". AI suggestion: `loading="lazy"`.
    pub missing_lazy: Vec<ImageIssue>,
    /// `<img>` without `srcset`. No responsive variants. Sorted by
    /// display area desc — larger images benefit most from
    /// device-tailored variants. AI suggestion: add `srcset` (and
    /// `sizes` for art direction).
    pub missing_srcset: Vec<ImageIssue>,
}

/// One image-audit entry. Compact subset of `ImageSizing` carrying
/// just the fields needed to triage the issue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageIssue {
    pub url: String,
    /// Display size in CSS pixels (what the user actually sees).
    pub display_width: u32,
    pub display_height: u32,
    /// True if any part of the image overlaps the initial viewport.
    /// Above-the-fold issues matter more than below-the-fold.
    pub in_viewport: bool,
    /// Issue-specific numeric. For `oversized`: natural / effective-
    /// display ratio (always > 2.0 when listed here, so the AI can
    /// say "image is 3.4× the display size"). For other categories:
    /// `0.0` (the issue is categorical — the URL is the answer).
    pub ratio: f64,
}

/// Font-loading audit. Walks `@font-face` rules across all readable
/// stylesheets and the `document.fonts` FontFaceSet to surface the
/// signal AI optimisation cares about most:
/// **which fonts will cause FOIT** (Flash of Invisible Text — the
/// "blank text for 3 seconds during load" UX bug). Populated only
/// when `SummaryRequest.font_audit = true` (OR-merged with
/// `all_metrics`).
///
/// **CORS blind spot**: cross-origin stylesheets without
/// `crossorigin` + matching CORS headers raise on `cssRules` access.
/// `unreadable_stylesheets` is non-zero when the audit was incomplete;
/// treat the rest of the data as "of what's visible".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FontAudit {
    /// Total number of FontFace entries the browser knows about
    /// (`document.fonts.size`). Includes both `@font-face`-declared
    /// fonts and any added programmatically via `document.fonts.add`.
    pub font_count: u32,
    /// Of `font_count`, how many reached `status === "loaded"` by
    /// the time the audit ran. Unloaded ones may still be in flight
    /// (lazy fonts not yet used) — not necessarily an error.
    pub loaded_count: u32,
    /// Distribution of `font-display` descriptor values across all
    /// observed `@font-face` rules. Keys: `"auto"` / `"swap"` /
    /// `"block"` / `"fallback"` / `"optional"`. Missing
    /// `font-display` defaults to `"auto"` per CSS spec. A healthy
    /// page has `swap` dominating.
    pub display_distribution: HashMap<String, u32>,
    /// `@font-face` declarations with `font-display` set to anything
    /// other than `swap` / `optional` — i.e. likely to cause FOIT.
    /// Each entry names the family + resolved source URL so the AI
    /// can suggest the literal fix (`font-display: swap;`).
    pub missing_swap: Vec<FontIssue>,
    /// Count of `<link rel="preload" as="font">` declarations in
    /// `<head>`. Single scalar — per-font preload gap analysis
    /// would require above-the-fold font usage detection and is
    /// **intentionally not done** (preloading every font is itself
    /// an anti-pattern). Tells "did you bother to preload any
    /// fonts at all" — `0` is the common case and is itself a
    /// finding when the page uses critical web fonts.
    pub declared_preload_count: u32,
    /// Count of stylesheets the audit could NOT read due to CORS.
    /// Non-zero means the rest of the data is incomplete — the
    /// page's third-party fonts (Google Fonts, Adobe Fonts, etc.)
    /// often live in cross-origin sheets without the `crossorigin`
    /// attribute set on `<link>`. AI suggestion: "add `crossorigin`
    /// to the `<link>` so the audit can see your font config".
    pub unreadable_stylesheets: u32,
}

/// One `@font-face` finding in `FontAudit.missing_swap`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FontIssue {
    /// CSS font-family name from the `@font-face` block (quotes
    /// stripped). Empty when the declaration was malformed.
    pub family: String,
    /// Resolved absolute URL of the first `url(...)` in the `src:`
    /// descriptor — what the browser would actually fetch when
    /// local fallbacks miss. `None` when only `local()` sources
    /// were declared, or `src:` couldn't be parsed.
    pub source_url: Option<String>,
    /// Current `font-display` value verbatim from the declaration.
    /// `None` when the descriptor was absent — defaults to `"auto"`
    /// per CSS spec, which is what `missing_swap` would have been
    /// keyed off. The AI suggestion is the same either way:
    /// `font-display: swap;`.
    pub display: Option<String>,
}

/// Comparison-friendly aggregates over `resources`. All counts/bytes are
/// derived server-side; no extra browser interaction.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResourceSummary {
    /// Total content bytes grouped by top-level MIME type (e.g. "image",
    /// "javascript", "css", "font", "json", "html", "other").
    pub bytes_by_type: HashMap<String, u64>,
    /// Resource counts by the same MIME buckets as `bytes_by_type`.
    pub count_by_type: HashMap<String, u32>,
    /// Counts by HTTP status class: `"2xx"`, `"3xx"`, `"4xx"`, `"5xx"`, `"other"`.
    pub status_distribution: HashMap<String, u32>,
    /// Fraction of resources served from cache (0.0 .. 1.0).
    pub cache_hit_ratio: f64,
    /// Total content bytes that came from cache.
    pub cached_bytes: u64,
    /// Total content bytes from hosts other than the target URL's host.
    pub third_party_bytes: u64,
    /// `(url, bytes)` of the single largest resource — quick "what dominates"
    /// answer. None when `resources` is empty.
    pub largest_resource: Option<(String, u64)>,
    /// HTTP version distribution across non-cached responses. Keys are
    /// normalised protocol strings (`"h2"`, `"h3"`, `"http/1.1"`,
    /// `"unknown"` for missing). Surfaces incomplete HTTP/2|3 rollouts
    /// at a glance.
    pub protocol_distribution: HashMap<String, u32>,
    /// Resources that shipped with a `Content-Encoding` header
    /// (`gzip`/`br`/`deflate`/`zstd`).
    pub compressed_count: u32,
    /// Text-compressible resources (HTML/CSS/JS/JSON/SVG/XML) served
    /// **without** a `Content-Encoding` header — missed-compression
    /// candidates. Lighthouse "uses-text-compression" equivalent.
    pub uncompressed_text_count: u32,
    /// Wire bytes of those uncompressed text resources — the savings
    /// opportunity if compression were enabled.
    pub uncompressed_text_bytes: u64,
    /// Real network responses (`from_cache=false`) that reused an
    /// existing connection. High ratio = good HTTP/2 connection pooling.
    pub connections_reused: u32,
    /// Real network responses that opened a fresh TCP/TLS connection.
    /// Each one paid the handshake cost. High count on a single-origin
    /// site usually means HTTP/1.1 (no multiplexing) or DNS misconfig.
    pub connections_new: u32,
    /// Distinct hostnames across all observed resources. Approximates
    /// DNS lookup count (one per unique host). Lower is better for
    /// HTTP/2 connection coalescing.
    pub unique_hosts: u32,
    /// Distribution of `Content-Encoding` algorithms across real-network
    /// responses to **text-compressible** resources (HTML/CSS/JS/JSON/
    /// SVG/XML — see `is_text_compressible`). Keys: `"gzip"` / `"br"` /
    /// `"zstd"` / `"deflate"` / `"none"`. `"none"` means the response
    /// was a compressible text type but shipped uncompressed (each one
    /// also counted in `uncompressed_text_count`). Binary types
    /// (image/video/font/wasm) are excluded — they're already format-
    /// compressed and the absence of Content-Encoding isn't a finding.
    pub compression_breakdown: HashMap<String, u32>,
    /// Real-network responses that carried a `Cache-Control` header
    /// (any value, including `no-store` — the point is the origin made
    /// an explicit caching statement). Pairs with `cache_control_missing`
    /// to compute a coverage ratio.
    pub cache_control_present: u32,
    /// Real-network responses that shipped without `Cache-Control`. Each
    /// one falls back to browser heuristic freshness — typically a
    /// missed caching opportunity for static assets. Watch for spikes
    /// after deploys that add a new origin or CDN tier.
    pub cache_control_missing: u32,
    /// Top third-party hosts ranked by bytes shipped, capped at 10.
    /// Lets callers spot the heaviest external dependencies (analytics,
    /// ads, fonts, vendor CDNs) without parsing the per-resource list.
    /// Empty when the page loaded zero third-party content.
    pub top_third_party_domains: Vec<DomainBytes>,
    /// Fraction of real-network responses negotiated over HTTP/2 or
    /// HTTP/3 (`0.0 .. 1.0`). Single scalar so monitors can alert on
    /// regressions — e.g. `0.95 → 0.20` typically means a vendor CDN
    /// reverted to HTTP/1.1 or a misconfigured origin lost ALPN.
    /// `0.0` when every response was cached (no real protocol observed).
    pub modern_protocol_share: f64,
    /// Total content bytes shipped in **legacy** raster image formats
    /// (`image/jpeg`, `image/png`, `image/gif`). Pairs with
    /// `modern_image_bytes` — together they cover the bulk of
    /// image payload, and the ratio drives the Lighthouse "Serve
    /// images in next-gen formats" estimate. Vector formats (SVG)
    /// and non-image MIME types are excluded.
    pub legacy_image_bytes: u64,
    /// Total content bytes shipped in **next-gen** raster image
    /// formats (`image/webp`, `image/avif`). A high
    /// `modern / (modern + legacy)` ratio indicates good
    /// modernisation; `0` is itself a finding for image-heavy pages.
    pub modern_image_bytes: u64,
    /// Count of **JS or CSS** resources that shipped a sourcemap
    /// pointer header (`SourceMap` or `X-SourceMap`). Two
    /// orthogonal interpretations — useful for `coverage` analysis,
    /// risky for production exposure — left to the caller.
    pub source_maps_present: u32,
    /// JS / CSS resources WITHOUT a sourcemap pointer header. Pairs
    /// with `source_maps_present` as the coverage denominator.
    pub source_maps_missing: u32,
    /// Duplicate-resource detection: same URL loaded multiple times,
    /// plus same-basename + same-size loaded from different URLs.
    /// Always populated (empty lists when no duplicates).
    pub duplicate_resources: DuplicateResources,
    /// Mixed-content audit: plain HTTP resources on an HTTPS page.
    /// Always populated; `detected=false` for HTTP-served pages
    /// (where the check doesn't apply) and clean HTTPS pages.
    pub mixed_content: MixedContent,
    /// Max chain length walking backwards through `initiator.url`
    /// from any resource to a root (a resource with no initiator).
    /// Approximates Lighthouse's "critical request chain depth".
    /// `None` when `initiators=false` (the per-resource initiator
    /// data isn't captured); `Some(0)` when every resource was
    /// initiated by the parser at depth 1 (flat dependency graph).
    pub max_initiator_chain_depth: Option<u32>,
    /// Per-MIME-bucket "top largest resources" ranking. Keys are the
    /// same MIME buckets used in `bytes_by_type` but restricted to the
    /// four optimisation-relevant types: `"javascript"`, `"css"`,
    /// `"image"`, `"font"`. Each Vec is sorted by `bytes` desc and
    /// capped at 5 entries. Empty map when no resources matched any
    /// of those types.
    pub top_largest_by_type: HashMap<String, Vec<LargestResource>>,
    /// Compressible-text resources served WITHOUT `Content-Encoding`,
    /// the actual offender list (vs. the existing
    /// `uncompressed_text_count` / `_bytes` scalars). Sorted by `bytes`
    /// desc, capped at 20 — pins down which files to fix first.
    pub uncompressed_text_resources: Vec<UncompressedResource>,
    /// Cache-policy anti-patterns on **static assets only** (JS / CSS /
    /// image / font). Two reason codes:
    ///
    /// - `"short_max_age"` — `max-age` parsed below 60s on a static
    ///   asset. Usually a deploy-time misconfiguration; static files
    ///   should ship `max-age` in the hours-to-year range.
    /// - `"missing_immutable"` — URL looks fingerprinted (contains a
    ///   `[a-f0-9]{8,}` token) AND `Cache-Control` is present but
    ///   missing the `immutable` directive. Without `immutable`,
    ///   browsers still revalidate on hard refresh — pure waste.
    ///
    /// Capped at 20 entries (worst-first). HTML / JSON / API responses
    /// are excluded — their cache headers reflect business rules,
    /// not asset misconfiguration.
    pub cache_policy_issues: Vec<CachePolicyIssue>,
    /// Resource-hint audit (preconnect / dns-prefetch gaps vs the hot
    /// third-party hosts the page actually fetched from). `None` when
    /// the caller didn't request `resource_hints` (no head scrape
    /// performed); `Some(_)` otherwise — `declared_*` and `gap` may
    /// still be empty if the page either declares no hints or every
    /// hot third party is already covered.
    pub resource_hints: Option<ResourceHints>,
}

/// Mixed-content finding — HTTPS pages must not fetch sub-resources
/// over plain HTTP (browsers either block or auto-upgrade them, and
/// either way it's a configuration mistake the caller wants to
/// know about). When the main page itself is HTTP this check
/// doesn't apply (nothing to be "mixed" against).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MixedContent {
    /// True iff the main page was loaded over HTTPS AND at least one
    /// sub-resource came over plain HTTP. False on HTTP main pages
    /// or clean HTTPS pages.
    pub detected: bool,
    /// Total count of HTTP resources observed on the HTTPS page —
    /// **not** capped (the truncation only affects `resources`).
    pub total_count: u32,
    /// Up to 10 of the offending resources, sorted by `content_size`
    /// descending (largest payload first — fixing those has the
    /// biggest user-visible impact). Empty when `detected=false`.
    pub resources: Vec<MixedContentResource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MixedContentResource {
    pub url: String,
    pub content_size: u64,
    /// Top-level MIME bucket (`javascript` / `css` / `image` / ...),
    /// taken from the same `mime_bucket` mapping used elsewhere.
    pub kind: String,
}

/// Per-host byte/count tuple for the `top_third_party_domains` ranking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainBytes {
    pub host: String,
    pub bytes: u64,
    pub count: u32,
}

/// One entry of the per-type "largest resources" ranking. Used by
/// `ResourceSummary.top_largest_by_type` to surface, for each MIME
/// bucket (`javascript` / `css` / `image` / `font`), the few biggest
/// individual files. AI can use these as targeted "split this bundle"
/// or "compress this image" suggestions without having to scan the
/// full `resources[]` list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LargestResource {
    pub url: String,
    pub bytes: u64,
    pub mime_type: String,
    /// True if this entry was served from cache (`content_size` came from
    /// the cached response — no fresh wire transfer). Cached copies are
    /// still listed because they reflect what the page actually loaded;
    /// callers who want "wasted bandwidth" should filter by `!from_cache`.
    pub from_cache: bool,
}

/// One entry of the "compressible text resource served uncompressed"
/// list. `ResourceSummary` already exposes the count + total bytes for
/// the cohort; this list pins down *which* files to fix first (sorted
/// by size desc, top 20). MIME filter follows `is_text_compressible`
/// so binary types (images / video / fonts / wasm) never appear.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UncompressedResource {
    pub url: String,
    pub mime_type: String,
    pub bytes: u64,
}

/// One cache-policy finding. Detected against static-asset MIME types
/// (JS / CSS / image / font) only; HTML / JSON / XHR endpoints have
/// legitimate reasons to use `no-store` / short max-age and are
/// excluded so the list stays actionable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachePolicyIssue {
    pub url: String,
    pub mime_type: String,
    /// Verbatim `Cache-Control` header — useful when the issue is
    /// `missing_immutable` (so the AI can suggest the augmented value).
    pub cache_control: String,
    /// Short tag — `"short_max_age"` (max-age < 60s on a static asset)
    /// or `"missing_immutable"` (hashed/fingerprinted URL without the
    /// `immutable` directive — every revalidation is pure waste).
    pub reason: String,
}

/// Resource-hint audit. Compares the page's declared
/// `<link rel="preconnect">` / `<link rel="dns-prefetch">` hints
/// against the set of third-party origins actually hit during load.
/// Always populated when `resource_hints=true` (or when subsumed by
/// `all_metrics=true`); the `declared_*` lists may be empty if the
/// page declared no hints, and `gap` may be empty if every hot
/// third party is already covered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceHints {
    /// Origins (or hosts, for dns-prefetch) the page explicitly
    /// declared via `<link rel="preconnect">`. Stored as the link's
    /// `href` resolved — typically `https://example.com`.
    pub declared_preconnect: Vec<String>,
    /// Origins (or hosts) declared via `<link rel="dns-prefetch">`.
    /// These only resolve DNS, not connection setup; useful but
    /// strictly weaker than preconnect.
    pub declared_dns_prefetch: Vec<String>,
    /// Third-party hosts that were actually loaded with non-trivial
    /// bytes but have neither a preconnect nor a dns-prefetch hint.
    /// Each gap = one DNS lookup + TLS handshake of pure latency
    /// (typically 100-300ms on a cold connection). Ranked by bytes
    /// desc so the highest-impact fix surfaces first.
    pub gap: Vec<ResourceHintGap>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceHintGap {
    pub host: String,
    pub bytes: u64,
    pub count: u32,
}

/// Duplicate-resource report — catches the "same static file loaded
/// twice" pattern that wastes bandwidth and parse / compile time.
///
/// Two detection passes are run, both pure derives from `resources[]`:
///
///   1. **exact_url**: same URL appeared ≥2 times. Usually a real bug
///      (double-mount, hydration loop, accidental import). Chrome
///      normally dedupes via HTTP cache, so multiple entries means
///      cache was bypassed or the engine treated them as distinct
///      requests.
///   2. **likely_same_file**: same `basename` + same `content_size`
///      shipped from ≥2 different URLs. Catches "same library from
///      different CDNs" (jsdelivr + cdnjs) or "fingerprinted twice
///      with different hashes". Constraint on `content_size` cuts
///      most false positives from generic names (`app.js`,
///      `index.js`); same-name-different-size pairs are excluded.
///
/// `wasted_bytes` (top-level) sums the savings across both buckets —
/// the bytes you'd recover if dedup were fixed. For exact_url groups
/// with mixed cache + fresh copies, cache hits contribute `0` (no
/// wire transfer happened), so this scalar is conservative.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DuplicateResources {
    pub exact_url: Vec<DuplicateEntry>,
    pub likely_same_file: Vec<DuplicateEntry>,
    pub wasted_bytes: u64,
}

/// One bucket of duplicate-resource detection. For `exact_url` the
/// `key` is the URL and `urls` contains that one URL (the list is
/// just for symmetry with `likely_same_file`); for `likely_same_file`
/// the `key` is `"<basename>|<bytes_each>"` and `urls` is the ≥2
/// distinct origins.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DuplicateEntry {
    pub key: String,
    pub urls: Vec<String>,
    /// Total occurrences. For `exact_url` = how many times that URL
    /// was loaded. For `likely_same_file` = how many distinct URLs
    /// landed in this bucket.
    pub count: u32,
    /// Representative size of one copy. Same across all copies in a
    /// `likely_same_file` group (it's part of the grouping key); for
    /// `exact_url` it's the max content_size observed (so a fresh
    /// copy beats a cache-hit copy at `0`).
    pub bytes_each: u64,
    /// Bytes that wouldn't have been transferred if dedup worked —
    /// sum of `content_size` for all copies except the largest one.
    /// `0` when every copy was a cache hit (still surfaces the code
    /// bug, but acknowledges no wire cost was paid).
    pub wasted_bytes: u64,
}

/// Page-level metadata extracted from `<head>`. Comparison signals for SEO
/// and correctness regressions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageMetadata {
    pub title: String,
    pub description: Option<String>,
    pub canonical: Option<String>,
    pub robots: Option<String>,
    pub lang: Option<String>,
    pub viewport: Option<String>,
    pub charset: Option<String>,
    pub theme_color: Option<String>,
    /// All `<meta property="og:*">` tags keyed by the substring after `og:`.
    pub og: HashMap<String, String>,
    /// All `<meta name="twitter:*">` tags keyed by the substring after `twitter:`.
    pub twitter: HashMap<String, String>,
}

/// Per-page snapshot from `Performance.getMetrics` — memory, DOM counters,
/// and CPU time breakdown. The CPU durations (ms) are cumulative since
/// page start and are gold for regression detection: e.g. "LCP unchanged
/// but `script_duration_ms` jumped 30% across deploys" → JS regression.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PageMetrics {
    /// V8 heap currently in use, bytes.
    pub js_heap_used: u64,
    /// V8 heap allocated capacity, bytes.
    pub js_heap_total: u64,
    /// Number of HTML Document objects.
    pub documents: u32,
    /// Number of Frame objects (main + iframes).
    pub frames: u32,
    /// Total live DOM node count.
    pub nodes: u32,
    /// Registered JS event listeners across the page.
    pub js_event_listeners: u32,
    /// Cumulative JS execution time (ms).
    pub script_duration_ms: f64,
    /// Cumulative layout time (ms).
    pub layout_duration_ms: f64,
    /// Cumulative style recalculation time (ms).
    pub recalc_style_duration_ms: f64,
    /// Cumulative time spent on tasks the renderer ran (ms) — superset of
    /// the above per-phase durations.
    pub task_duration_ms: f64,
}

/// Subset of Web Vitals collectible in a headless environment. Skips FID
/// and INP because they require real user interaction (always 0 in
/// headless).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebVitals {
    /// Largest Contentful Paint — ms from navigation start.
    pub lcp: f64,
    /// Cumulative Layout Shift — unitless, sum of significant shifts.
    pub cls: f64,
    /// Total Blocking Time — sum of (long-task duration − 50ms), in ms.
    pub tbt: f64,
    /// Time to First Byte — ms from navigation start to first response byte.
    pub ttfb: f64,
    /// Number of long tasks (>50ms) observed.
    pub long_tasks: u32,
    /// The element that triggered LCP (image, text block, etc.).
    /// Knowing the element makes "why LCP changed" attributable.
    pub lcp_element: Option<LcpElement>,
    /// Layout-shift entries with their contributing source elements.
    /// Capped at 50 entries client-side to bound memory.
    pub cls_entries: Vec<ClsEntry>,
    /// Pre-aggregated top CLS offenders: which elements contributed the
    /// most layout shift, ranked desc by `total_shift`. Server-side
    /// derived from `cls_entries`. Use this for "find the worst offender"
    /// without iterating raw entries.
    #[serde(default)]
    pub cls_top_sources: Vec<ClsTopSource>,
    /// **INP (Interaction to Next Paint)** — 2024 Core Web Vital, replaces
    /// FID. Longest interaction duration in ms (event-start → next paint).
    /// In pure headless scraping this is `0` because no user input occurs;
    /// becomes meaningful when `script` triggers `.click()` / synthetic
    /// events. `interaction_count` tells you whether the value is
    /// meaningful — `0` interactions means `inp` is just a default zero,
    /// not "instant response".
    #[serde(default)]
    pub inp: f64,
    /// Number of interaction events observed (events with `interactionId`).
    /// `0` is normal for non-interactive scrapes — treat `inp` as N/A then.
    #[serde(default)]
    pub interaction_count: u32,
    /// **Long Animation Frames** (Chrome 123+). Total LoAF entries observed
    /// during the page render. Each LoAF is a frame that took >50ms to
    /// produce — a more precise jank signal than `long_tasks` because it
    /// covers the *entire* frame (script + style + layout + paint), not
    /// just the script portion.
    #[serde(default)]
    pub loaf_count: u32,
    /// Sum of `blockingDuration` across all observed LoAF entries.
    /// Analogous to TBT but covers the full rendering pipeline, not just
    /// long tasks. Higher = worse responsiveness during render.
    #[serde(default)]
    pub loaf_total_blocking_duration: f64,
    /// Server-side aggregated top offending scripts across all LoAF
    /// entries (grouped by `source_url`, ranked desc by `total_duration_ms`).
    /// Up to 5 entries. Empty when no LoAF observed or no attributable
    /// scripts (LoAF API unsupported on older Chromium).
    #[serde(default)]
    pub loaf_top_offenders: Vec<LoafOffender>,
    /// Server-side aggregated top offending longtask sources, grouped
    /// from `PerformanceLongTaskTiming.attribution[].container_src`
    /// (or `container_name` / task name when src is empty). Up to 5
    /// entries, ranked desc by `total_duration_ms`. Lets the AI say
    /// "3 longtasks, 800ms total, all from gtm.js" instead of just
    /// "long_tasks: 3". Empty when no longtasks observed OR every
    /// observed task lacked any attribution detail (rare).
    ///
    /// `long_tasks` (scalar count) and `tbt` (scalar blocking time)
    /// stay as the headline numbers; this list is the **why**.
    #[serde(default)]
    pub long_task_top_offenders: Vec<LongTaskOffender>,
    /// **FPS** — frames observed by a `requestAnimationFrame` loop
    /// installed pre-navigation, aggregated over the same observation
    /// window as the rest of `WebVitals`. `0` when `web_vitals=false`
    /// or when no frames were produced (window too short / page
    /// crashed before paint).
    ///
    /// Caveat: the rAF loop itself drives frame production. A page
    /// with no animation would normally be idle (compositor parked);
    /// the loop forces frames at the engine's natural cadence (~60Hz
    /// in headless, ~display refresh rate when headed). So `fps_avg`
    /// on a fully static page reads near-60 rather than zero. The
    /// most actionable signal for animation pages is `fps_jank_ratio`
    /// and `fps_longest_frame_ms` — those reflect real main-thread
    /// blocking regardless of why the loop is producing frames.
    ///
    /// Headless + VM caveat: headless Chrome uses software
    /// rasterization, so absolute numbers don't match user-device
    /// (GPU) performance. Comparable for **regression detection**
    /// on the same harness; not comparable across host setups.
    #[serde(default)]
    pub fps_avg: f64,
    /// Fraction of observed frames slower than 16.67ms (60fps target),
    /// `0.0 .. 1.0`. The headline jank signal for marketing pages —
    /// a value above `0.10` typically means visibly stuttery animation
    /// or scroll. `0.0` when `fps_frame_count == 0`.
    #[serde(default)]
    pub fps_jank_ratio: f64,
    /// Slowest single frame observed, in ms. Marks the worst-case
    /// pause the user could have perceived during the window. Values
    /// above ~100ms usually pair with a `loaf_top_offenders[]` entry
    /// that explains the source.
    #[serde(default)]
    pub fps_longest_frame_ms: f64,
    /// Total number of frames observed. `0` when `web_vitals=false`
    /// or the observation window closed before the second rAF
    /// callback (the first sets the baseline timestamp, the second
    /// is needed to measure a `dt`). Useful for sanity-checking
    /// `fps_avg` (very low frame count → small sample, noisy signal).
    #[serde(default)]
    pub fps_frame_count: u32,
}

/// Per-script contribution aggregated across all observed LoAF entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoafOffender {
    /// Script URL the slow code came from. May be empty for inline /
    /// `eval` / browser-internal callbacks.
    pub source_url: String,
    /// Last-seen function name for this source. Sampled, not aggregated.
    pub source_function_name: String,
    /// `"script"`, `"user-callback"`, `"event-listener"`, etc.
    pub invoker_type: String,
    /// Sum of `duration` across all invocations attributed to `source_url`.
    pub total_duration_ms: f64,
    /// Sum of `forcedStyleAndLayoutDuration` — synchronous layout / style
    /// recalc this script forced. Non-zero indicates layout thrashing.
    pub total_forced_style_layout_ms: f64,
    /// Number of LoAF entries this script appeared in.
    pub invocation_count: u32,
}

/// Raw LoAF entry as collected browser-side. Internal — only used to
/// receive the JSON before server-side aggregation; not in `WebVitals`
/// output (we expose the aggregated `loaf_top_offenders` + scalars).
#[derive(Debug, Clone, Default, Deserialize)]
struct LoafRawEntry {
    #[allow(dead_code)]
    start_time: f64,
    #[allow(dead_code)]
    duration: f64,
    blocking_duration: f64,
    #[allow(dead_code)]
    render_start: f64,
    #[allow(dead_code)]
    style_and_layout_start: f64,
    scripts: Vec<LoafRawScript>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LoafRawScript {
    invoker_type: String,
    source_url: String,
    source_function_name: String,
    duration: f64,
    forced_style_and_layout_duration: f64,
}

/// Raw `PerformanceLongTaskTiming` entry as captured browser-side.
/// Internal — receives the JSON before server-side aggregation; not
/// in the `WebVitals` output (we expose the aggregated
/// `long_task_top_offenders` + the scalar `long_tasks` count).
#[derive(Debug, Clone, Default, Deserialize)]
struct LongTaskRawEntry {
    /// `"self"` for same-frame tasks, otherwise the cross-frame
    /// container's name. Used as the fallback grouping key when no
    /// `attribution.container_src` is reported.
    name: String,
    #[allow(dead_code)]
    start_time: f64,
    duration: f64,
    attribution: Vec<LongTaskRawAttribution>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LongTaskRawAttribution {
    /// `"iframe"` / `"embed"` / `"object"` / etc. Empty for same-page.
    #[allow(dead_code)]
    container_type: String,
    /// URL of the embedded frame (iframe `src`). Most actionable —
    /// when present, names the third-party iframe responsible.
    container_src: String,
    #[allow(dead_code)]
    container_id: String,
    /// Human-readable name from `frame.name` / etc. Used as a
    /// fallback grouping key when `container_src` is empty.
    container_name: String,
}

/// One aggregated longtask offender: a single "source" (best-effort
/// derivation — `container_src` when available, else `container_name`,
/// else the task's `name`) and its total contribution across all
/// observed tasks. Ranked desc by `total_duration_ms` for AI scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LongTaskOffender {
    /// Best-effort source identifier. In priority order: the first
    /// `attribution[].container_src` if non-empty (iframe URL), else
    /// the first non-empty `container_name`, else the task's
    /// `name` (typically `"self"` for in-page tasks). When the result
    /// would be empty, falls back to `"(same-page)"` so the bucket is
    /// at least labeled.
    pub source: String,
    /// Sum of task durations (ms) attributed to this source.
    pub total_duration_ms: f64,
    /// Longest single task duration observed for this source. Lets
    /// AI distinguish "many small tasks" from "one giant task".
    pub max_duration_ms: f64,
    /// Number of distinct longtask entries grouped into this bucket.
    pub task_count: u32,
}

/// One aggregated CLS offender: a single element identity (tag + id /
/// class) and its total shift contribution across all observed shifts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClsTopSource {
    /// Best-effort CSS selector — `tag#id`, `tag.firstClass`, or just `tag`.
    pub selector: String,
    /// Sum of shift contributions attributed to this element.
    pub total_shift: f64,
    /// `total_shift` as a fraction of total CLS (0.0 .. ~1.0).
    pub fraction: f64,
    /// Number of distinct shift entries this element appeared in.
    pub shift_count: u32,
    /// Largest single movement (Euclidean distance in CSS px between
    /// `previous_rect` and `current_rect`) observed for this element.
    /// Useful for the "reserve N px of vertical space" suggestion:
    /// if an element shifted 240px once, the fix needs ≥240px of
    /// reserved height. `0.0` when no source-level geometry was
    /// available (older Chromium without `LayoutShiftAttribution`).
    #[serde(default)]
    pub max_distance_px: f64,
}

/// Identifying details of the element that triggered Largest Contentful Paint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LcpElement {
    pub tag: String,
    pub id: String,
    pub class: String,
    /// For `<img>` / `<video poster>`: the resource URL. None otherwise.
    pub url: Option<String>,
    /// For text elements: first 120 chars of `textContent`. None for non-text.
    pub text_preview: Option<String>,
    /// LCP entry's computed `size` — the area in CSS px² that the browser
    /// used to pick this element as the "largest contentful paint". Lets
    /// the AI rank LCP candidates by visual prominence.
    #[serde(default)]
    pub size: f64,
    /// For image LCP: ms from navigation start to when the image
    /// resource finished loading. `0` for text LCP (no resource).
    /// Useful split with `render_time` — if `load_time` is small but
    /// `render_time` is large, the bottleneck is render, not network.
    #[serde(default)]
    pub load_time: f64,
    /// Ms from navigation start to when the element was actually
    /// painted to the screen. May be `0` for cross-origin images
    /// without `Timing-Allow-Origin` — browser hides paint time as a
    /// side-channel mitigation. When `0` but `load_time > 0`, treat
    /// LCP as bounded by `load_time` + some unknown paint cost.
    #[serde(default)]
    pub render_time: f64,
    /// For `<img>`: `naturalWidth` (decoded pixel width). For `<video>`:
    /// `videoWidth`. `0` for text LCP or when the resource hasn't
    /// decoded yet. Combined with display size from
    /// `image_sizing` (when that feature is on), the AI can flag
    /// "image is N× the display size — serve a smaller variant".
    #[serde(default)]
    pub natural_width: u32,
    #[serde(default)]
    pub natural_height: u32,
}

/// One layout-shift entry — when it happened and which elements moved.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClsEntry {
    /// Time of the shift, ms from navigation start.
    pub time_ms: f64,
    /// Contribution to total CLS.
    pub value: f64,
    /// Elements involved in the shift.
    pub sources: Vec<ClsSourceElement>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClsSourceElement {
    pub tag: String,
    pub id: String,
    pub class: String,
    /// Bounding box of the element BEFORE the shift, in CSS pixels.
    /// `None` only when the browser didn't expose
    /// `LayoutShiftAttribution.previousRect` (older Chromium).
    #[serde(default)]
    pub previous_rect: Option<ShiftRect>,
    /// Bounding box AFTER the shift. Pairs with `previous_rect`.
    #[serde(default)]
    pub current_rect: Option<ShiftRect>,
    /// Euclidean movement distance between `previous_rect.{x,y}` and
    /// `current_rect.{x,y}`, in CSS pixels. Server-side derive — the
    /// raw rects are kept too for downstream visualization. `0.0`
    /// when geometry wasn't captured.
    #[serde(default)]
    pub distance_px: f64,
}

/// Bounding box of a CLS source element, in CSS pixels relative to
/// the viewport. Mirrors `DOMRect.{x, y, width, height}` semantics.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ShiftRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Geolocation {
    pub latitude: f64,
    pub longitude: f64,
    /// Reported position accuracy in meters. CDP requires this; default 100.
    pub accuracy: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pdf {
    /// Base64-encoded PDF bytes.
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix epoch seconds. -1 means session cookie (no explicit expiry).
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    /// `Strict` / `Lax` / `None` — Option because some CDP responses omit it.
    pub same_site: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WebPageResource {
    pub content_size: u64,
    pub request_id: String,
    pub status: u32,
    pub url: String,
    pub timing: Option<ResourceTiming>,
    pub mime_type: String,
    pub connection_reused: bool,
    /// HTTP version negotiated by the browser for this resource. Examples:
    /// `"h2"`, `"h3"`, `"h3-29"`, `"http/1.1"`. Empty for cached responses
    /// (no real wire transfer).
    pub protocol: String,
    /// `Content-Encoding` response header value if present
    /// (`gzip` / `br` / `deflate` / `zstd`). `None` when the response
    /// shipped uncompressed.
    pub content_encoding: Option<String>,
    /// `Cache-Control` response header value if present (any value —
    /// `no-store` is "present" because the origin made an explicit
    /// statement). `None` means the origin shipped no caching policy,
    /// which leaves the browser falling back to RFC 7234 heuristic
    /// freshness — usually a missed caching opportunity for static
    /// assets. Counted into `ResourceSummary.cache_control_*`.
    pub cache_control: Option<String>,
    /// True when the response carried a `SourceMap` (or legacy
    /// `X-SourceMap`) response header pointing to a `.map` file.
    /// Populated for every resource type but only meaningful for
    /// JS / CSS — those are the only kinds counted into
    /// `ResourceSummary.source_maps_*`. Two practical uses: (a) AI
    /// can decode `coverage.top_unused[].url` byte offsets back to
    /// original source when a sourcemap is published; (b) the
    /// inverse — production sites usually shouldn't expose
    /// sourcemaps publicly.
    pub has_source_map: bool,
    /// True if the response came from disk cache, service worker, or prefetch
    /// cache. Cache hits typically have `content_size = 0` and many `timing`
    /// fields = -1 (skipped phases).
    pub from_cache: bool,
    /// What triggered this request. Populated only when
    /// `SummaryRequest.initiators = true` (requires subscribing to
    /// `Network.requestWillBeSent`).
    pub initiator: Option<RequestInitiator>,
    /// Internal cache of `url::Url::parse(&url)` populated once when the
    /// response event first arrives. Lets downstream consumers (TLS
    /// extraction, `build_resource_summary`) read `host_str()` without
    /// re-parsing. Skipped during serialisation — the canonical wire
    /// form is the original `url` string above.
    #[serde(skip)]
    pub parsed_url: Option<url::Url>,
}

/// Simplified initiator: who/what triggered the request. Stack trace
/// omitted for compactness.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestInitiator {
    /// `"parser"` / `"script"` / `"preload"` / `"SignedExchange"` / `"other"`.
    pub r#type: String,
    /// Script URL or parser source URL.
    pub url: Option<String>,
    /// Line number in the source (1-based).
    pub line_number: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize)]
pub struct WebPageLifecycle {
    pub init_time: f64,
    pub fcp_time: f64,
    pub dcl_time: f64,
    pub load_time: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ResourceTiming {
    pub request_time: f64,
    pub dns_start: f64,
    pub dns_end: f64,
    pub connect_start: f64,
    pub connect_end: f64,
    pub ssl_start: f64,
    pub ssl_end: f64,
    pub send_start: f64,
    pub send_end: f64,
    pub receive_headers_end: f64,
}

/// Phase-by-phase timing for the **main document** response (final
/// landing page when redirects happened). Promoted from
/// `resources[].timing` to a top-level field because for SSR /
/// server-rendered pages the "is the server slow vs is the frontend
/// slow" split is the single most diagnostic first-triage signal —
/// and the detailed `resources[]` array is opt-in (`resources=true`).
///
/// Always populated when at least one Document response was observed
/// with timing data; `None` for fully-cached navigations or unusual
/// flows that produced no real Document response. All phase fields
/// are millisecond durations, **clamped to 0** when CDP reported the
/// phase as skipped (cache hit, connection reuse, etc.).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocumentTiming {
    /// Final URL after redirects (matches `http_errors.final_url`
    /// when that feature is on).
    pub url: String,
    pub status: u32,
    /// True if the main document came from disk / SW / prefetch
    /// cache — in that case all phase durations are typically `0`.
    pub from_cache: bool,
    /// HTTP version negotiated (`h2` / `h3` / `http/1.1` / empty for
    /// cached responses).
    pub protocol: String,
    /// DNS lookup time. `0` when the host was already resolved (DNS
    /// cache hit) or the connection was reused.
    pub dns_ms: u32,
    /// TCP handshake time. `0` when the connection was reused (HTTP/2
    /// multiplexing, keep-alive).
    pub tcp_ms: u32,
    /// TLS handshake time. `0` for plain HTTP, when the connection
    /// was reused, or when 0-RTT resumption was used.
    pub tls_ms: u32,
    /// Server processing time — from "request fully sent" to "first
    /// response byte received". The most direct measure of "how fast
    /// is your backend / SSR layer".
    pub ttfb_ms: u32,
}

/// Build a `DocumentTiming` from CDP response fields. Phases that CDP
/// reported as skipped (negative values) collapse to `0` so the
/// individual phase scalars sum correctly. Negative / NaN inputs
/// treated defensively.
fn build_document_timing(
    url: &str,
    status: u32,
    from_cache: bool,
    protocol: &str,
    t: &CdpResourceTiming,
) -> DocumentTiming {
    let phase = |a: f64, b: f64| -> u32 {
        let d = b - a;
        if d.is_finite() && d > 0.0 {
            d as u32
        } else {
            0
        }
    };
    DocumentTiming {
        url: url.to_string(),
        status,
        from_cache,
        protocol: protocol.to_string(),
        dns_ms: phase(t.dns_start, t.dns_end),
        tcp_ms: phase(t.connect_start, t.connect_end),
        tls_ms: phase(t.ssl_start, t.ssl_end),
        ttfb_ms: phase(t.send_start, t.receive_headers_end),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Screenshot {
    /// Base64-encoded PNG bytes (as returned by CDP `Page.captureScreenshot`).
    pub data: String,
    pub mime_type: String,
}

/// Per-feature capture toggles for `collect_summary`. Bundled together so
/// the function signature stays short as new optional captures are added
/// (each one would otherwise add a bool param). All default to `false` —
/// callers opt in explicitly.
#[derive(Debug, Clone, Copy, Default)]
pub struct CollectCaptures {
    /// Take a PNG screenshot at end of capture (`Page.captureScreenshot`).
    pub screenshot: bool,
    /// Subscribe to `Network.requestWillBeSent` and attach
    /// `initiator` info to each resource entry.
    pub initiators: bool,
    /// Subscribe to `Runtime.consoleAPICalled` and collect formatted
    /// log lines.
    pub console: bool,
    /// Subscribe to `Network.loadingFailed` and build the
    /// `HttpErrors` rollup (4xx/5xx lists, network failures, final
    /// URL, redirect count).
    pub http_errors: bool,
    /// Enable CDP `Profiler` precise coverage + `CSS` rule-usage
    /// tracking pre-navigation, then drain `takePreciseCoverage` +
    /// `stopRuleUsageTracking` after the page settles. Builds
    /// `CoverageReport`.
    pub coverage: bool,
}

/// Navigate to `url` and collect a full page summary: lifecycle timings,
/// per-resource network stats, JS exceptions, final HTML, and optionally a
/// screenshot. Drives Page / Network / Runtime CDP domains in parallel and
/// returns once the wait gate fires (or `timeout` elapses, returning a
/// best-effort partial snapshot).
///
/// Wait gate selection (`wait_until_load`):
/// - `true`  → exit shortly after the `load` (onload) lifecycle event.
///   Used when the caller didn't specify any of `wait_for_element` /
///   `wait_for_function` / `wait_for_request`; the page is considered
///   "ready enough" at onload and the caller drives any further waits
///   themselves (e.g. via `settle`).
/// - `false` → exit shortly after the `networkIdle` lifecycle event
///   (Chrome's ≥500ms zero-in-flight signal). Used when explicit waits
///   are in play and we want network to actually quiesce so all
///   responses are recorded.
pub async fn collect_summary(
    page: &Page,
    url: &str,
    timeout: Duration,
    wait_for_request: &[String],
    wait_until_load: bool,
    captures: CollectCaptures,
) -> Result<WebPageStat, Error> {
    page.execute(NetworkEnableParams::default()).await?;
    page.execute(PageEnableParams::default()).await?;
    page.execute(RuntimeEnableParams::default()).await?;
    page.execute(SetLifecycleEventsEnabledParams::new(true))
        .await?;

    let mut response_stream = page.event_listener::<EventResponseReceived>().await?;
    let mut loading_finished_stream = page.event_listener::<EventLoadingFinished>().await?;
    let mut lifecycle_stream = page.event_listener::<EventLifecycleEvent>().await?;
    let mut exception_stream = page.event_listener::<EventExceptionThrown>().await?;
    // Console + initiators: subscribe only when requested. When disabled,
    // swap in `stream::pending()` so the `select!` arm is still well-typed
    // but never wakes — zero runtime cost and the page never even decodes
    // console arg payloads we'd throw away.
    let mut console_stream: BoxStream<'static, std::sync::Arc<EventConsoleApiCalled>> =
        if captures.console {
            Box::pin(page.event_listener::<EventConsoleApiCalled>().await?)
        } else {
            Box::pin(stream::pending())
        };
    let mut request_stream: BoxStream<'static, std::sync::Arc<EventRequestWillBeSent>> =
        if captures.initiators {
            Box::pin(page.event_listener::<EventRequestWillBeSent>().await?)
        } else {
            Box::pin(stream::pending())
        };
    // `loadingFailed`: fired when a request never produces a response
    // (DNS / TLS / connection refused / blocked / canceled). Same gated
    // pending-stream pattern as console/initiators — zero cost when off.
    let mut loading_failed_stream: BoxStream<'static, std::sync::Arc<EventLoadingFailed>> =
        if captures.http_errors {
            Box::pin(page.event_listener::<EventLoadingFailed>().await?)
        } else {
            Box::pin(stream::pending())
        };
    // `styleSheetAdded`: fires once per stylesheet the CSS engine
    // registers. Needed only for coverage — the header carries
    // `length` (total stylesheet bytes) and the `style_sheet_id` that
    // `RuleUsage` entries reference. Gated zero-cost when off.
    let mut stylesheet_stream: BoxStream<'static, std::sync::Arc<EventStyleSheetAdded>> =
        if captures.coverage {
            Box::pin(page.event_listener::<EventStyleSheetAdded>().await?)
        } else {
            Box::pin(stream::pending())
        };

    page.goto(url).await?;

    let mut resources: HashMap<String, WebPageResource> = HashMap::new();
    let mut exceptions: Vec<String> = Vec::new();
    // Classified-exception roll-up state. Keyed by class name. The tuple
    // is `(count, first_seen_sample_message)` so we keep the FIRST sample
    // per class rather than overwriting — the AI usually wants to see
    // the earliest occurrence (often the trigger), not the latest.
    let mut exception_buckets: HashMap<String, (u32, Option<String>)> = HashMap::new();
    let mut console_messages: Vec<String> = Vec::new();
    let mut security_headers: Option<HashMap<String, String>> = None;
    let mut tls_info: Option<TlsInfo> = None;
    // HTTP error rollup state. All four are only meaningfully populated
    // when `captures.http_errors` is true — the response-handler arm
    // gates writes on the same flag so the per-resource hot path stays
    // cheap for callers who don't ask for this feature.
    //
    // `http_final_url` defaults to the navigation target; every Document-
    // type response with status <400 overwrites it, so the LAST surviving
    // Document URL wins (= post-redirect landing page).
    //
    // `http_redirect_count` increments for every Document-type response in
    // the 300-399 range — that's exactly one per redirect hop in the chain.
    let mut http_resource_types: HashMap<String, ResourceType> = HashMap::new();
    let mut http_final_url: String = url.to_string();
    let mut http_redirect_count: u32 = 0;
    let mut http_network_failures: Vec<NetworkFailure> = Vec::new();
    // Diagnostic-only: maps request_id → loadingFailed error text. Used
    // by the end-of-summary status=0 walker (see after the event loop)
    // to attribute ghost stubs to their failure reason. Always populated
    // regardless of `captures.http_errors` so debugging works even when
    // the http_errors output is opted-out. Cheap (tiny map per page).
    let mut failed_loading: HashMap<String, String> = HashMap::new();
    // Per-request_id timestamp of the `requestWillBeSent` event (CDP
    // MonotonicTime, seconds). Paired with the `loadingFailed`
    // timestamp to compute how quickly a request was aborted.
    let mut request_sent_at: HashMap<String, f64> = HashMap::new();
    // request_ids whose `loadingFailed` had `canceled=true` AND fired
    // within `QUICK_ABORT_THRESHOLD_S` of the matching
    // `requestWillBeSent`. These are framework-driven aborts (React
    // component unmount mid-fetch, route change AbortController, fetch
    // racing a navigation) — they aren't actionable errors, just noise
    // in `resources[]` and `http_network_failures[]`. Filtered out
    // before either is finalised.
    let mut quick_abort_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    const QUICK_ABORT_THRESHOLD_S: f64 = 0.1;
    // Document-timing state. Captured on the LAST Document response
    // with status <400 (so redirects don't win — we want the actual
    // landing page). Always tracked, no gate: it's a trivial
    // assignment per Document response, and the data is too
    // diagnostic to keep opt-in.
    let mut document_timing: Option<DocumentTiming> = None;
    // Coverage bookkeeping. `stylesheet_meta` is keyed by string-form
    // `style_sheet_id` (matches the field type on `RuleUsage`) and
    // carries `(source_url, length_bytes)`. Only populated when
    // `captures.coverage` is true.
    let mut stylesheet_meta: HashMap<String, (String, u64)> = HashMap::new();
    // Per-host TLS dedup. Same host on the page almost always serves the
    // same cert, so first sighting wins; subsequent sightings are skipped.
    let mut tls_by_host: HashMap<String, TlsInfo> = HashMap::new();
    let mut init_ts: Option<f64> = None;
    let mut fcp_ts: Option<f64> = None;
    let mut dcl_ts: Option<f64> = None;
    let mut load_ts: Option<f64> = None;

    // Exit policy:
    // - `load` alone is unsafe to break on: select may have skipped already-
    //   queued response events; breaking immediately drops them.
    // - When `wait_until_load` is true (no explicit caller-side waits), gate
    //   on the `load` (onload) lifecycle event and add a short grace window
    //   for the select loop to drain in-flight response events.
    // - Otherwise, gate on `networkIdle` lifecycle (Chrome emits this after
    //   ≥500ms with zero in-flight requests, i.e. all responses have already
    //   fired).
    // - After the chosen gate fires, give a 500ms grace window so the select
    //   loop can drain any response events still sitting in their channels.
    // - `wait_for_request` patterns (if any) always need to be matched
    //   regardless of which gate is active.
    // - Soft cap: `timeout` (the page never settles → return best-effort).
    const POST_IDLE_GRACE: Duration = Duration::from_millis(500);
    let deadline = Instant::now() + timeout;
    let mut idle_at: Option<Instant> = None;
    let mut load_at: Option<Instant> = None;
    let mut pending_patterns: Vec<&str> = wait_for_request.iter().map(String::as_str).collect();
    let total_patterns = pending_patterns.len();

    loop {
        // Pick the gate marker per strategy. Grace period only kicks in once
        // the gate has fired AND all wait_for_request patterns have matched.
        // If patterns are still pending after the gate, keep waiting (up to
        // timeout) for them.
        let gate_at = if wait_until_load { load_at } else { idle_at };
        let ready_to_finish = gate_at.is_some() && pending_patterns.is_empty();
        let stop_at = match (ready_to_finish, gate_at) {
            (true, Some(t)) => deadline.min(t + POST_IDLE_GRACE),
            _ => deadline,
        };
        let remaining = stop_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        let sleep = tokio::time::sleep(remaining);
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => {
                tracing::debug!(
                    timeout_ms = timeout.as_millis() as u64,
                    wait_until_load,
                    load = load_at.is_some(),
                    idle = idle_at.is_some(),
                    "collect_summary stopping",
                );
                break;
            }
            Some(ev) = lifecycle_stream.next() => {
                let ts = *ev.timestamp.inner();
                match ev.name.as_str() {
                    "init" => { init_ts.get_or_insert(ts); }
                    "firstContentfulPaint" => { fcp_ts.get_or_insert(ts); }
                    "DOMContentLoaded" => { dcl_ts.get_or_insert(ts); }
                    "load" => {
                        load_ts.get_or_insert(ts);
                        load_at.get_or_insert(Instant::now());
                    }
                    "networkIdle" => { idle_at.get_or_insert(Instant::now()); }
                    _ => {}
                }
            }
            Some(ev) = response_stream.next() => {
                // Skip pseudo-schemes that aren't real network requests
                // (inlined base64 images, blob URLs created in JS, etc.).
                if !is_real_resource(&ev.response.url) {
                    continue;
                }
                let url = ev.response.url.clone();
                let status = ev.response.status;

                // wait_for_request: match URL substring against pending patterns.
                // 4xx/5xx on a matched URL short-circuits with UpstreamFailure.
                if let Some(pos) = pending_patterns.iter().position(|p| url.contains(*p)) {
                    if !(200..400).contains(&status) {
                        return Err(Error::UpstreamFailure { status, url });
                    }
                    let matched = pending_patterns.swap_remove(pos);
                    tracing::debug!(
                        pattern = matched,
                        remaining = pending_patterns.len(),
                        total = total_patterns,
                        "wait_for_request matched",
                    );
                }

                let id = ev.request_id.inner().clone();
                let entry = resources.entry(id.clone()).or_insert_with(|| WebPageResource {
                    request_id: id.clone(),
                    ..Default::default()
                });
                // Parse URL once and cache on the entry. Both TLS extract
                // (below) and `build_resource_summary` (server-side derive)
                // need `host_str()`; the cache eliminates a second parse
                // per HTTPS resource. Hot-path cost: ~5-20μs per resource.
                entry.parsed_url = url::Url::parse(&url).ok();
                entry.url = url;
                entry.status = status as u32;
                entry.mime_type = ev.response.mime_type.clone();
                entry.connection_reused = ev.response.connection_reused;
                entry.timing = ev.response.timing.as_ref().map(map_timing);
                entry.from_cache = ev.response.from_disk_cache.unwrap_or(false)
                    || ev.response.from_service_worker.unwrap_or(false)
                    || ev.response.from_prefetch_cache.unwrap_or(false);
                entry.protocol = ev.response.protocol.clone().unwrap_or_default();
                entry.content_encoding = lookup_header(&ev.response.headers, "content-encoding");
                entry.cache_control = lookup_header(&ev.response.headers, "cache-control");
                // Two header spellings: modern `SourceMap` and the
                // legacy `X-SourceMap` that older tooling (webpack,
                // pre-2018 Babel) emitted. Either one counts.
                entry.has_source_map = lookup_header(&ev.response.headers, "sourcemap").is_some()
                    || lookup_header(&ev.response.headers, "x-sourcemap").is_some();

                // Diagnostic log (debug level). Pairs with the
                // `request_will_be_sent` log on the same request_id —
                // when investigating status=0 ghosts, the presence /
                // absence of a matching `response_received` line tells
                // you whether the request completed at the HTTP layer.
                tracing::debug!(
                    target: DIAG_RESOURCE,
                    request_id = %id,
                    url = %entry.url,
                    status = entry.status,
                    mime = %entry.mime_type,
                    from_cache = entry.from_cache,
                    "response_received",
                );

                // HTTP error bookkeeping. All gated — when off, the
                // `if captures.http_errors` branch is a single bool test
                // and nothing else runs.
                //
                //   - Cache resource_type by request_id so the finalize
                //     pass can label failed_4xx/5xx entries without
                //     widening WebPageResource just for this feature.
                //   - Track final URL: every Document response with
                //     status <400 updates the running landing-page URL
                //     (last successful Document wins). 3xx responses are
                //     intentionally skipped here — they're redirects, not
                //     the destination.
                //   - Count redirects: Document responses in 300-399 are
                //     exactly one per redirect hop.
                if captures.http_errors {
                    http_resource_types.insert(id.clone(), ev.r#type.clone());
                    if matches!(ev.r#type, ResourceType::Document) {
                        let s = status as u32;
                        if (300..400).contains(&s) {
                            http_redirect_count += 1;
                        } else if s < 400 {
                            http_final_url = entry.url.clone();
                        }
                    }
                }

                // Document timing: capture the FINAL (non-redirect)
                // main-document response. Multiple Document responses
                // can fire during a redirect chain — we only want the
                // landing page, so 3xx responses are skipped and each
                // subsequent <400 Document response overwrites.
                if matches!(ev.r#type, ResourceType::Document) {
                    let s = status as u32;
                    if s < 400 {
                        let from_cache = entry.from_cache;
                        let protocol = entry.protocol.clone();
                        let url_now = entry.url.clone();
                        if let Some(t) = ev.response.timing.as_ref() {
                            document_timing = Some(build_document_timing(
                                &url_now,
                                s,
                                from_cache,
                                &protocol,
                                t,
                            ));
                        } else if from_cache {
                            // Cached document: no CDP timing, but we
                            // still want to surface "this came from
                            // cache" rather than `None`.
                            document_timing = Some(DocumentTiming {
                                url: url_now,
                                status: s,
                                from_cache: true,
                                protocol,
                                dns_ms: 0,
                                tcp_ms: 0,
                                tls_ms: 0,
                                ttfb_ms: 0,
                            });
                        }
                    }
                }

                // Pluck security headers from the main document response.
                // Last Document-type response wins (handles redirects +
                // final landing page).
                if matches!(ev.r#type, ResourceType::Document)
                    && let Some(headers) = extract_security_headers(&ev.response.headers)
                {
                    security_headers = Some(headers);
                }

                // TLS / cert: capture per host across **all** HTTPS resources
                // (JS/CSS/fonts on CDNs often live on different domains with
                // their own certs). Dedupe by host. Main-document cert also
                // exposed via the singular `tls_info` field. Carries the
                // browser-resolved remote IP — useful for hijack/MITM audit.
                if let Some(sd) = ev.response.security_details.as_ref()
                    && let Some(host) = entry.parsed_url.as_ref().and_then(|u| u.host_str())
                {
                    let host = host.to_string();
                    let remote_ip = ev.response.remote_ip_address.clone();
                    // CDP gives port as i64; clamp into u16 (real ports fit).
                    let remote_port = ev
                        .response
                        .remote_port
                        .and_then(|p| u16::try_from(p).ok());
                    if matches!(ev.r#type, ResourceType::Document) {
                        tls_info = Some(extract_tls_info(
                            sd,
                            host.clone(),
                            remote_ip.clone(),
                            remote_port,
                        ));
                    }
                    tls_by_host
                        .entry(host.clone())
                        .or_insert_with(|| extract_tls_info(sd, host, remote_ip, remote_port));
                }
            }
            Some(ev) = loading_finished_stream.next() => {
                let id = ev.request_id.inner().clone();
                // Only update if the response was recorded — filtered URLs
                // never inserted an entry; don't create a stub here.
                if let Some(entry) = resources.get_mut(&id) {
                    entry.content_size = ev.encoded_data_length.max(0.0) as u64;
                }
            }
            Some(ev) = exception_stream.next() => {
                exceptions.push(format_exception(&ev));
                let name = classify_exception(&ev);
                let entry = exception_buckets.entry(name).or_insert((0, None));
                entry.0 += 1;
                if entry.1.is_none() {
                    entry.1 = exception_sample_message(&ev);
                }
            }
            Some(ev) = console_stream.next() => {
                console_messages.push(format_console(&ev));
            }
            Some(ev) = request_stream.next() => {
                // Skip pseudo-schemes (data: / blob: / about: / extensions)
                // for parity with the response arm — otherwise this arm
                // would create stubs for resources the response arm will
                // never bother with, leaving permanent "ghosts" in the
                // output.
                if !is_real_resource(&ev.request.url) {
                    continue;
                }
                let id = ev.request_id.inner().clone();
                let mapped = map_initiator(&ev.initiator);
                // Diagnostic log (debug level — enable via
                // `RUST_LOG=browser_headless=debug` when investigating
                // why a resource ended up with status=0).
                tracing::debug!(
                    target: DIAG_RESOURCE,
                    request_id = %id,
                    url = %ev.request.url,
                    method = %ev.request.method,
                    initiator_type = %mapped.r#type,
                    initiator_url = ?mapped.url,
                    "request_will_be_sent",
                );
                // Attach to the matching resource entry. Create a stub if
                // the response hasn't been seen yet — response_stream arm
                // will fill the rest of the fields later. Pre-fill `url`
                // from the request itself so that if the response NEVER
                // arrives (request canceled mid-flight, page navigated
                // away, blocked by extension / CSP without a loadingFailed
                // event), the stub still carries the intended URL for
                // diagnostics. The response arm overwrites unconditionally,
                // so a real response always wins over the request URL when
                // both arrive (matters for redirects: response.url is the
                // resolved final URL after each hop).
                let entry = resources.entry(id.clone()).or_insert_with(|| WebPageResource {
                    request_id: id.clone(),
                    ..Default::default()
                });
                if entry.url.is_empty() {
                    entry.url = ev.request.url.clone();
                    entry.parsed_url = url::Url::parse(&ev.request.url).ok();
                }
                entry.initiator = Some(mapped);
                // Record the send timestamp for quick-abort detection.
                // CDP redirects share a request_id across hops; last-
                // write-wins is correct (we care about the abort window
                // from the most recent attempt, not the original).
                request_sent_at.insert(id.clone(), *ev.timestamp.inner());
            }
            Some(ev) = loading_failed_stream.next() => {
                // Filter pseudo-schemes for parity with the response arm
                // (data:/blob:/about: never make it into `resources`, so
                // their failures wouldn't be actionable either).
                let request_id = ev.request_id.inner().clone();
                let failed_url = resources
                    .get(&request_id)
                    .map(|r| r.url.clone())
                    .unwrap_or_default();
                if !failed_url.is_empty() && !is_real_resource(&failed_url) {
                    continue;
                }
                // Quick-abort suppression. `canceled=true` is necessary
                // but not sufficient (user navigation away mid-fetch is
                // also `canceled=true` and might be meaningful) — we
                // additionally require the abort to fire within 100ms of
                // the request being sent, which is the classic signature
                // of a framework-driven cancellation (React unmount,
                // route change AbortController, fetch racing a route
                // hash change). Slower aborts are kept so user-initiated
                // cancels or timeout-driven aborts remain visible.
                let failed_ts = *ev.timestamp.inner();
                let canceled = ev.canceled.unwrap_or(false);
                let abort_duration_s = request_sent_at
                    .get(&request_id)
                    .map(|sent| failed_ts - sent);
                let is_quick_abort = canceled
                    && abort_duration_s
                        .map(|d| d < QUICK_ABORT_THRESHOLD_S)
                        .unwrap_or(false);
                if is_quick_abort {
                    quick_abort_ids.insert(request_id.clone());
                    tracing::debug!(
                        target: DIAG_RESOURCE,
                        request_id = %request_id,
                        url = %failed_url,
                        duration_ms = abort_duration_s.unwrap_or(0.0) * 1000.0,
                        "quick_abort_suppressed",
                    );
                    continue;
                }
                // Diagnostic: always record into the failed_loading map
                // regardless of `captures.http_errors`, so the end-of-
                // summary status=0 walker can attribute ghost stubs to
                // their failure even when the caller didn't opt into
                // http_errors capture.
                failed_loading.insert(request_id.clone(), ev.error_text.clone());
                tracing::debug!(
                    target: DIAG_RESOURCE,
                    request_id = %request_id,
                    url = %failed_url,
                    error_text = %ev.error_text,
                    resource_type = ?ev.r#type,
                    canceled,
                    "loading_failed",
                );
                http_network_failures.push(NetworkFailure {
                    url: failed_url,
                    error_text: ev.error_text.clone(),
                    resource_type: format!("{:?}", ev.r#type).to_lowercase(),
                    canceled,
                });
            }
            Some(ev) = stylesheet_stream.next() => {
                // Record total length so `RuleUsage` ranges can be
                // converted into a meaningful "% used" later. Inline
                // stylesheets get an empty source_url — we use a
                // synthetic `inline:<id>` label so they're still
                // rankable in `top_unused`.
                let id = ev.header.style_sheet_id.inner().to_string();
                let url = if ev.header.source_url.is_empty() {
                    format!("inline:{id}")
                } else {
                    ev.header.source_url.clone()
                };
                let length = ev.header.length.max(0.0) as u64;
                stylesheet_meta.insert(id, (url, length));
            }
            else => break,
        }
    }

    let to_ms = |t: Option<f64>| -> u32 {
        match (init_ts, t) {
            (Some(init), Some(t)) => ((t - init).max(0.0) * 1000.0) as u32,
            _ => 0,
        }
    };

    // Fetch cookies scoped to the target URL (parent domains included by CDP).
    let cookies_resp = page
        .execute(GetCookiesParams {
            urls: Some(vec![url.to_string()]),
        })
        .await?;
    let cookies: Vec<Cookie> = cookies_resp.result.cookies.iter().map(map_cookie).collect();

    // `data` is populated by `capture()` AFTER any user script runs, so the
    // returned HTML reflects post-script DOM. collect_summary just leaves it
    // empty.
    let screenshot = if captures.screenshot {
        let resp = page.execute(CaptureScreenshotParams::default()).await?;
        Some(Screenshot {
            data: resp.result.data.clone().into(),
            mime_type: "image/png".to_string(),
        })
    } else {
        None
    };

    // Defensive: drop stubs that never got a URL. With the request_stream
    // arm pre-filling `entry.url` from `ev.request.url`, this should now
    // be empty in practice — but keep the filter as a backstop so any
    // future code path that inserts a default entry without setting URL
    // can't leak a `status=0, url=""` ghost into the output (which would
    // render as the confusing `请求 ``` 状态码 0 (未知类型, 0 B).` line).
    resources.retain(|_, r| !r.url.is_empty() && !quick_abort_ids.contains(&r.request_id));
    // Diagnostic: scan for resources where the response never arrived
    // (`status == 0`) and emit a `warn!` per occurrence with the URL,
    // initiator, and — when available — the matching `loadingFailed`
    // error text. These are the entries that render as the confusing
    // `请求 \`URL\` 状态码 0 ...` markdown line; the log makes the
    // attribution obvious without grepping `http_errors.network_failures`.
    // Warn level so it shows up at the default log level (these ARE
    // anomalies). Silent on a clean page (no log emitted).
    for r in resources.values() {
        if r.status != 0 {
            continue;
        }
        let error_text = failed_loading.get(&r.request_id).cloned();
        let initiator_summary = r.initiator.as_ref().map(|i| {
            format!(
                "type={} url={:?} line={:?}",
                i.r#type, i.url, i.line_number,
            )
        });
        tracing::warn!(
            target: DIAG_RESOURCE,
            request_id = %r.request_id,
            url = %r.url,
            initiator = ?initiator_summary,
            loading_failed_error = ?error_text,
            "resource has status=0 — request initiated but no response observed",
        );
    }
    let total_size: u64 = resources.values().map(|r| r.content_size).sum();
    let resources_vec: Vec<WebPageResource> = resources.into_values().collect();
    let resource_count = resources_vec.len() as u64;

    // Bucketed exceptions: count desc, ties broken by name asc, top 10.
    // Always built — when zero exceptions fired, `total: 0, by_name: []`
    // (consistent with the existing `exceptions: Vec<String>` always
    // being present as `[]` rather than absent).
    let js_exceptions = {
        let mut entries: Vec<JsExceptionCount> = exception_buckets
            .into_iter()
            .map(|(name, (count, sample_message))| JsExceptionCount {
                name,
                count,
                sample_message,
            })
            .collect();
        entries.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        entries.truncate(10);
        JsExceptions {
            total: exceptions.len() as u32,
            by_name: entries,
        }
    };

    // Partition non-2xx/3xx responses into the failed_4xx / failed_5xx
    // buckets. Status 0 happens for the "request canceled before response"
    // edge case — we leave those to `network_failures` since CDP would
    // have fired `loadingFailed` for them. Only built when feature is on.
    let http_errors = if captures.http_errors {
        let mut failed_4xx: Vec<FailedRequest> = Vec::new();
        let mut failed_5xx: Vec<FailedRequest> = Vec::new();
        for r in &resources_vec {
            if r.status < 400 || r.status >= 600 {
                continue;
            }
            let entry = FailedRequest {
                url: r.url.clone(),
                status: r.status,
                resource_type: http_resource_types
                    .get(&r.request_id)
                    .map(|t| format!("{t:?}").to_lowercase())
                    .unwrap_or_default(),
            };
            if r.status < 500 {
                failed_4xx.push(entry);
            } else {
                failed_5xx.push(entry);
            }
        }
        let failed_count =
            (failed_4xx.len() + failed_5xx.len() + http_network_failures.len()) as u32;
        Some(HttpErrors {
            failed_count,
            failed_4xx,
            failed_5xx,
            network_failures: http_network_failures,
            final_url: http_final_url,
            redirect_count: http_redirect_count,
        })
    } else {
        None
    };

    let security_audit = build_security_audit(security_headers.as_ref(), &cookies);

    // Coverage finalize — only when feature was enabled in apply stage.
    // Calls Profiler.takePreciseCoverage + Profiler.stopPreciseCoverage
    // + CSS.stopRuleUsageTracking, then aggregates into a CoverageReport.
    let coverage = if captures.coverage {
        let js_coverage = page
            .execute(TakePreciseCoverageParams::default())
            .await?
            .result
            .result
            .clone();
        // Best-effort stop; failure to stop doesn't invalidate the
        // already-drained data, so ignore the error.
        let _ = page.execute(StopPreciseCoverageParams::default()).await;
        let css_coverage = page
            .execute(StopRuleUsageTrackingParams::default())
            .await?
            .result
            .rule_usage
            .clone();

        // Per-file entries: walk JS scripts first, then CSS stylesheets.
        let mut entries: Vec<CoverageEntry> = Vec::new();
        let mut js_total: u64 = 0;
        let mut js_used: u64 = 0;
        for script in &js_coverage {
            // Skip anonymous / internal scripts (no URL) and known
            // pseudo URLs — they're typically eval / Function-ctor
            // shims with no source the user controls.
            if script.url.is_empty() || !is_real_resource(&script.url) {
                continue;
            }
            let (used, total) = compute_js_coverage(script);
            if total == 0 {
                continue;
            }
            js_total += total;
            js_used += used;
            let unused = total.saturating_sub(used);
            entries.push(CoverageEntry {
                url: script.url.clone(),
                kind: "js".to_string(),
                total_bytes: total,
                used_bytes: used,
                unused_bytes: unused,
                unused_ratio: unused as f64 / total as f64,
            });
        }

        // Group CSS rule usage by stylesheet id, then look up the
        // total length from the styleSheetAdded map.
        let mut css_by_sheet: HashMap<String, Vec<&CssRuleUsage>> = HashMap::new();
        for r in &css_coverage {
            let id = r.style_sheet_id.inner().to_string();
            css_by_sheet.entry(id).or_default().push(r);
        }
        let mut css_total: u64 = 0;
        let mut css_used: u64 = 0;
        for (id, rules) in &css_by_sheet {
            let Some((url, total)) = stylesheet_meta.get(id) else {
                // No header was observed for this sheet (rare — user-agent
                // / extension sheets sometimes skip the event). Skip.
                continue;
            };
            if *total == 0 {
                continue;
            }
            let (used, total_bytes) = compute_css_coverage(rules, *total);
            css_total += total_bytes;
            css_used += used;
            let unused = total_bytes.saturating_sub(used);
            entries.push(CoverageEntry {
                url: url.clone(),
                kind: "css".to_string(),
                total_bytes,
                used_bytes: used,
                unused_bytes: unused,
                unused_ratio: unused as f64 / total_bytes as f64,
            });
        }

        // Top 10 by unused_bytes desc (largest waste first).
        entries.sort_by_key(|e| std::cmp::Reverse(e.unused_bytes));
        entries.truncate(10);

        let js_unused = js_total.saturating_sub(js_used);
        let css_unused = css_total.saturating_sub(css_used);
        Some(CoverageReport {
            js_total_bytes: js_total,
            js_used_bytes: js_used,
            js_unused_bytes: js_unused,
            js_unused_ratio: if js_total > 0 {
                js_unused as f64 / js_total as f64
            } else {
                0.0
            },
            css_total_bytes: css_total,
            css_used_bytes: css_used,
            css_unused_bytes: css_unused,
            css_unused_ratio: if css_total > 0 {
                css_unused as f64 / css_total as f64
            } else {
                0.0
            },
            top_unused: entries,
        })
    } else {
        None
    };

    Ok(WebPageStat {
        total_size,
        resource_count,
        fcp_time: to_ms(fcp_ts),
        dcl_time: to_ms(dcl_ts),
        load_time: to_ms(load_ts),
        data: String::new(),
        exceptions,
        js_exceptions,
        console_messages,
        resources: resources_vec,
        cookies,
        screenshot,
        pdf: None,
        har: None,
        dom_snapshot: None,
        web_vitals: None,
        metrics: None,
        resource_summary: ResourceSummary::default(),
        metadata: None,
        render_blocking_resources: None,
        security_headers,
        security_audit,
        service_worker: None,
        tls_info,
        tls_certificates: {
            let mut v: Vec<TlsInfo> = tls_by_host.into_values().collect();
            // Soonest-to-expire first, ties broken by host for stable output.
            v.sort_by(|a, b| {
                a.days_remaining
                    .cmp(&b.days_remaining)
                    .then_with(|| a.host.cmp(&b.host))
            });
            v
        },
        image_sizing: None,
        image_audit: None,
        font_audit: None,
        dom_mutations: None,
        http_errors,
        coverage,
        document_timing,
    })
}

/// Filter out URLs that aren't real network requests:
/// - `data:` — inlined data URIs (base64 images, JSON, etc.)
/// - `blob:` — JS-created object URLs (FileReader, MediaSource)
/// - `about:` — browser internal pages (about:blank)
/// - `chrome-extension:` / `moz-extension:` — extension resources
///   These have no meaningful timing/size and pollute the resource list.
fn is_real_resource(url: &str) -> bool {
    !(url.starts_with("data:")
        || url.starts_with("blob:")
        || url.starts_with("about:")
        || url.starts_with("chrome-extension:")
        || url.starts_with("moz-extension:"))
}

impl WebPageStat {
    /// Render this summary as Markdown suitable for feeding to an LLM as
    /// context. Each resource becomes a short prose sentence (cache hit /
    /// success / failure framed differently), exceptions are listed, and the
    /// page content goes in a fenced block at the end.
    ///
    /// `lang` controls the natural-language strings only — section
    /// headings, prose templates, warning labels. URLs, numbers, and
    /// enum-tag values (e.g. `missing_immutable`) are emitted verbatim
    /// regardless.
    pub fn to_markdown(&self, lang: Lang) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        // Tiny translation helper: takes the English and Chinese versions
        // of a literal, returns whichever matches the request's `lang`.
        // Closure form (not a top-level fn) so we can use it inside this
        // method without ceremony, and because every call site supplies
        // both arms as `&'static str` literals.
        let tr = |en: &'static str, zh: &'static str| -> &'static str {
            match lang {
                Lang::En => en,
                Lang::Zh => zh,
            }
        };

        let _ = writeln!(s, "{}", tr("# Page Summary", "# 页面摘要"));
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "{} **{}ms** (FCP {}ms, DCL {}ms). {} **{}** {} **{}** {}.",
            tr("Load completed in", "加载完成于"),
            self.load_time,
            self.fcp_time,
            self.dcl_time,
            tr("Transferred", "传输"),
            format_bytes(self.total_size),
            tr("across", "经由"),
            self.resource_count,
            tr("resources", "个资源"),
        );
        let _ = writeln!(s);

        if !self.exceptions.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## JavaScript Exceptions", "## JavaScript 异常"),
                self.exceptions.len(),
            );
            let _ = writeln!(s);
            if !self.js_exceptions.by_name.is_empty() {
                let _ = writeln!(s, "{}", tr("By class:", "按类型："));
                for entry in &self.js_exceptions.by_name {
                    match entry.sample_message.as_deref() {
                        Some(msg) => {
                            let _ = writeln!(s, "- **{}** ×{}: {}", entry.name, entry.count, msg);
                        }
                        None => {
                            let _ = writeln!(s, "- **{}** ×{}", entry.name, entry.count);
                        }
                    }
                }
                let _ = writeln!(s);
                let _ = writeln!(s, "{}", tr("Full list:", "完整列表："));
            }
            for ex in &self.exceptions {
                let _ = writeln!(s, "- {ex}");
            }
            let _ = writeln!(s);
        }

        if !self.console_messages.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Console Messages", "## 控制台输出"),
                self.console_messages.len(),
            );
            let _ = writeln!(s);
            for msg in &self.console_messages {
                let _ = writeln!(s, "- {msg}");
            }
            let _ = writeln!(s);
        }

        // ─── Overview block ─────────────────────────────────────────────
        // High-level summaries first (perf, security, SEO), then the raw
        // enumerations (resources, cookies), then binary attachments, then
        // the page content itself. Lets a reader (human or LLM) judge the
        // page from a few short sections before scrolling past long lists.

        if let Some(v) = &self.web_vitals {
            let _ = writeln!(s, "{}", tr("## Web Vitals", "## 网页关键性能指标"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- LCP **{:.0}ms** · CLS **{:.3}** · TBT **{:.0}ms** · TTFB **{:.0}ms** · {} **{}**",
                v.lcp,
                v.cls,
                v.tbt,
                v.ttfb,
                tr("long tasks", "长任务"),
                v.long_tasks,
            );
            // FPS — only render when frames were actually observed
            // (`fps_frame_count > 0`). Otherwise the values are all 0
            // and would mislead. `jank_ratio` rendered as percent and
            // tagged with ⚠️ above the visibly-stuttery 10% threshold.
            if v.fps_frame_count > 0 {
                let jank_pct = v.fps_jank_ratio * 100.0;
                let jank_warn = if v.fps_jank_ratio > 0.10 {
                    " ⚠️"
                } else {
                    ""
                };
                let _ = writeln!(
                    s,
                    "- FPS **{:.1}** ({} {}) · {} **{:.0}%**{} · {} **{:.0}ms**",
                    v.fps_avg,
                    v.fps_frame_count,
                    tr(
                        if v.fps_frame_count == 1 {
                            "frame"
                        } else {
                            "frames"
                        },
                        "帧",
                    ),
                    tr("jank", "卡顿率"),
                    jank_pct,
                    jank_warn,
                    tr("longest frame", "最长一帧"),
                    v.fps_longest_frame_ms,
                );
            }
            if let Some(el) = &v.lcp_element {
                let mut desc = format!("`<{}", el.tag);
                if !el.id.is_empty() {
                    desc.push_str(&format!(" id=\"{}\"", el.id));
                }
                if !el.class.is_empty() {
                    desc.push_str(&format!(" class=\"{}\"", el.class));
                }
                desc.push_str(">`");
                if let Some(u) = &el.url {
                    desc.push_str(&format!(" — `{u}`"));
                } else if let Some(t) = &el.text_preview {
                    desc.push_str(&format!(" — \"{t}\""));
                }
                let _ = writeln!(s, "- {}: {desc}", tr("LCP element", "LCP 元素"));
                // Size + load/render-time split, only when populated.
                // Cross-origin images often report `render_time=0` — we
                // suppress the field then so the markdown doesn't lie.
                if el.size > 0.0 {
                    let mut detail =
                        format!("  - {}: **{:.0}** CSS px²", tr("Size", "面积"), el.size,);
                    if el.natural_width > 0 && el.natural_height > 0 {
                        detail.push_str(&format!(
                            " · {} **{}×{}**",
                            tr("natural", "原生"),
                            el.natural_width,
                            el.natural_height,
                        ));
                    }
                    let _ = writeln!(s, "{detail}");
                }
                if el.load_time > 0.0 || el.render_time > 0.0 {
                    let mut detail = String::from("  - ");
                    if el.load_time > 0.0 {
                        detail.push_str(&format!(
                            "{} **{:.0}ms**",
                            tr("Load", "加载"),
                            el.load_time,
                        ));
                    }
                    if el.render_time > 0.0 {
                        if el.load_time > 0.0 {
                            detail.push_str(" · ");
                        }
                        detail.push_str(&format!(
                            "{} **{:.0}ms**",
                            tr("Render", "绘制"),
                            el.render_time,
                        ));
                    }
                    let _ = writeln!(s, "{detail}");
                }
            }
            if !v.cls_top_sources.is_empty() {
                let _ = writeln!(s, "- {}", tr("Top CLS offenders:", "CLS 主要肇事元素："));
                for (i, src) in v.cls_top_sources.iter().take(3).enumerate() {
                    // Optional "moved Npx" tail — only when source-level
                    // geometry was captured (`max_distance_px > 0`).
                    // Carries the concrete "reserve N px" actionable.
                    let moved = if src.max_distance_px > 0.0 {
                        format!(
                            " · {} **{:.0}px**",
                            tr("max move", "最大位移"),
                            src.max_distance_px,
                        )
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        s,
                        "  {}. **{}** — {:.3} ({:.0}%) {} {} {}{}",
                        i + 1,
                        src.selector,
                        src.total_shift,
                        src.fraction * 100.0,
                        tr("across", "共"),
                        src.shift_count,
                        tr(
                            if src.shift_count == 1 {
                                "shift"
                            } else {
                                "shifts"
                            },
                            "次抖动",
                        ),
                        moved,
                    );
                }
                if v.cls_top_sources.len() > 3 {
                    let _ = writeln!(
                        s,
                        "  {} {} {} (`cls_top_sources` / `cls_entries`).",
                        tr("…and", "……还有"),
                        v.cls_top_sources.len() - 3,
                        tr("more — see JSON for full list", "项更多，详见 JSON"),
                    );
                }
            }
            // INP — show only when there were actual interactions; otherwise
            // the value is just the default 0 and would mislead readers.
            if v.interaction_count > 0 {
                let _ = writeln!(
                    s,
                    "- INP **{:.0}ms** {} **{}** {}",
                    v.inp,
                    tr("across", "共"),
                    v.interaction_count,
                    tr(
                        if v.interaction_count == 1 {
                            "interaction"
                        } else {
                            "interactions"
                        },
                        "次交互",
                    ),
                );
            }
            // Long Task attribution — render only when offender data
            // was produced (raw entries captured AND at least one had
            // something to group on). The `long_tasks` headline number
            // is already in the top-line vitals row above.
            if !v.long_task_top_offenders.is_empty() {
                let _ = writeln!(s, "- {}", tr("Top long-task sources:", "长任务主要来源："),);
                for (i, o) in v.long_task_top_offenders.iter().take(3).enumerate() {
                    let src = if o.source.len() > 70 {
                        format!("…{}", &o.source[o.source.len() - 67..])
                    } else {
                        o.source.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  {}. `{}` — **{:.0}ms** {} {} {} ({} **{:.0}ms**)",
                        i + 1,
                        src,
                        o.total_duration_ms,
                        tr("across", "共"),
                        o.task_count,
                        tr(if o.task_count == 1 { "task" } else { "tasks" }, "次任务",),
                        tr("max", "最长"),
                        o.max_duration_ms,
                    );
                }
            }
            // LoAF — only render when something was observed (older
            // Chromium without the API returns count=0).
            if v.loaf_count > 0 {
                let _ = writeln!(
                    s,
                    "- {}: **{}** ({} **{:.0}ms** {})",
                    tr("Long Animation Frames", "长动画帧"),
                    v.loaf_count,
                    tr("blocking", "阻塞"),
                    v.loaf_total_blocking_duration,
                    tr("total", "总计"),
                );
                if !v.loaf_top_offenders.is_empty() {
                    let _ = writeln!(s, "- {}", tr("Top LoAF offenders:", "LoAF 主要肇事脚本："));
                    for (i, o) in v.loaf_top_offenders.iter().take(3).enumerate() {
                        let src = if o.source_url.is_empty() {
                            "(inline / unknown)".to_string()
                        } else if o.source_url.len() > 70 {
                            format!("…{}", &o.source_url[o.source_url.len() - 67..])
                        } else {
                            o.source_url.clone()
                        };
                        let fn_note = if !o.source_function_name.is_empty() {
                            format!(" `{}()`", o.source_function_name)
                        } else {
                            String::new()
                        };
                        let reflow_note = if o.total_forced_style_layout_ms > 5.0 {
                            format!(
                                " ⚠️ {} **{:.0}ms**",
                                tr("forced reflow", "强制重排"),
                                o.total_forced_style_layout_ms,
                            )
                        } else {
                            String::new()
                        };
                        let _ = writeln!(
                            s,
                            "  {}. `{}`{} — **{:.0}ms** {} {} {}{}",
                            i + 1,
                            src,
                            fn_note,
                            o.total_duration_ms,
                            tr("over", "共"),
                            o.invocation_count,
                            tr(
                                if o.invocation_count == 1 {
                                    "call"
                                } else {
                                    "calls"
                                },
                                "次调用",
                            ),
                            reflow_note,
                        );
                    }
                }
            }
            let _ = writeln!(s);
        }

        if let Some(dt) = &self.document_timing {
            let _ = writeln!(s, "{}", tr("## Document Timing", "## 主文档时序"));
            let _ = writeln!(s);
            let url_display = if dt.url.len() > 80 {
                format!("…{}", &dt.url[dt.url.len() - 77..])
            } else {
                dt.url.clone()
            };
            let _ = writeln!(
                s,
                "- `{}` — {} · {}{}",
                url_display,
                dt.status,
                if dt.protocol.is_empty() {
                    tr("(no protocol)", "(未知协议)")
                } else {
                    &dt.protocol
                },
                if dt.from_cache {
                    tr(" · cached", " · 来自缓存")
                } else {
                    ""
                },
            );
            let _ = writeln!(
                s,
                "- DNS **{}ms** · TCP **{}ms** · TLS **{}ms** · TTFB **{}ms**",
                dt.dns_ms, dt.tcp_ms, dt.tls_ms, dt.ttfb_ms,
            );
            let _ = writeln!(s);
        }

        // HTTP errors — placed right after Document Timing because the
        // two answer the same "did navigation actually succeed" question.
        // Section is silent on the trivial happy path (no 4xx/5xx, no
        // network failures, no redirects) so clean pages don't add
        // noise.
        if let Some(he) = &self.http_errors
            && (he.failed_count > 0 || he.redirect_count > 0)
        {
            let _ = writeln!(s, "{}", tr("## HTTP Errors", "## HTTP 错误"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}** ({} 4xx · {} 5xx · {} {})",
                tr("Failed requests", "失败请求"),
                he.failed_count,
                he.failed_4xx.len(),
                he.failed_5xx.len(),
                he.network_failures.len(),
                tr("network failures", "网络层失败"),
            );
            if he.redirect_count > 0 {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} → `{}`",
                    tr("Redirects", "重定向"),
                    he.redirect_count,
                    tr(
                        if he.redirect_count == 1 {
                            "hop"
                        } else {
                            "hops"
                        },
                        "跳",
                    ),
                    he.final_url,
                );
            }
            for r in he.failed_4xx.iter().take(5) {
                let display = if r.url.len() > 80 {
                    format!("…{}", &r.url[r.url.len() - 77..])
                } else {
                    r.url.clone()
                };
                let _ = writeln!(
                    s,
                    "  - **{}** [{}] `{}`",
                    r.status, r.resource_type, display,
                );
            }
            for r in he.failed_5xx.iter().take(5) {
                let display = if r.url.len() > 80 {
                    format!("…{}", &r.url[r.url.len() - 77..])
                } else {
                    r.url.clone()
                };
                let _ = writeln!(
                    s,
                    "  - **{}** [{}] `{}`",
                    r.status, r.resource_type, display,
                );
            }
            // Network failures — separate sub-list because they're a
            // different failure class (no response at all). Skip
            // `canceled=true` entries (typical: navigation supersession,
            // block_urls policy) so the reader sees real findings first.
            for f in he.network_failures.iter().filter(|f| !f.canceled).take(5) {
                let display = if f.url.len() > 80 {
                    format!("…{}", &f.url[f.url.len() - 77..])
                } else {
                    f.url.clone()
                };
                let _ = writeln!(
                    s,
                    "  - ⚠️ [{}] `{}` — `{}`",
                    f.resource_type, display, f.error_text,
                );
            }
            let _ = writeln!(s);
        }

        if !self.resource_summary.bytes_by_type.is_empty() {
            let rs = &self.resource_summary;
            let _ = writeln!(s, "{}", tr("## Resource Summary", "## 资源汇总"));
            let _ = writeln!(s);
            // Sort by bytes desc for stable readable output.
            let mut by_type: Vec<(&String, &u64)> = rs.bytes_by_type.iter().collect();
            by_type.sort_by(|a, b| b.1.cmp(a.1));
            let type_line = by_type
                .iter()
                .map(|(k, v)| {
                    let n = rs.count_by_type.get(*k).copied().unwrap_or(0);
                    format!("{} {} ({})", k, format_bytes(**v), n)
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let _ = writeln!(s, "- {}: {type_line}", tr("By type", "按类型"));
            let mut status: Vec<(&String, &u32)> = rs.status_distribution.iter().collect();
            status.sort_by_key(|x| x.0.clone());
            let status_line = status
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" · ");
            let _ = writeln!(s, "- {}: {status_line}", tr("Status", "状态码"));
            let _ = writeln!(
                s,
                "- {} **{:.0}%** ({} {})",
                tr("Cache hit ratio", "缓存命中率"),
                rs.cache_hit_ratio * 100.0,
                tr("saved", "节省"),
                format_bytes(rs.cached_bytes),
            );
            let _ = writeln!(
                s,
                "- {}: **{}**",
                tr("Third-party bytes", "第三方字节数"),
                format_bytes(rs.third_party_bytes),
            );
            // Top third-party domains by bytes — ranks the heaviest
            // external dependencies for an AI-scannable view.
            if !rs.top_third_party_domains.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr("Top third-party domains:", "第三方域名 TOP："),
                );
                for d in rs.top_third_party_domains.iter().take(5) {
                    let _ = writeln!(
                        s,
                        "  - `{}` — {} ({} {})",
                        d.host,
                        format_bytes(d.bytes),
                        d.count,
                        tr(
                            if d.count == 1 {
                                "resource"
                            } else {
                                "resources"
                            },
                            "个资源",
                        ),
                    );
                }
            }
            if let Some((url, sz)) = &rs.largest_resource {
                let _ = writeln!(
                    s,
                    "- {}: `{url}` ({})",
                    tr("Largest", "最大资源"),
                    format_bytes(*sz),
                );
            }
            // HTTP version distribution — sort by count desc so the
            // dominant protocol leads. Adjacent line shows the modern-
            // protocol scalar so AI can alert on a single ratio drop.
            if !rs.protocol_distribution.is_empty() {
                let mut proto: Vec<(&String, &u32)> = rs.protocol_distribution.iter().collect();
                proto.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let line = proto
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(
                    s,
                    "- {}: {line} ({} **{:.0}%**)",
                    tr("HTTP versions", "HTTP 版本"),
                    tr("HTTP/2+3 share", "HTTP/2+3 占比"),
                    rs.modern_protocol_share * 100.0,
                );
            }
            // Connection reuse + DNS approximation. Skip if no real
            // network resources (everything cached).
            let real_conns = rs.connections_reused + rs.connections_new;
            if real_conns > 0 {
                let reuse_pct = (rs.connections_reused as f64) * 100.0 / (real_conns as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} · **{}** {} (**{:.0}%** {}) · **{}** {}",
                    tr("Connections", "连接"),
                    rs.connections_reused,
                    tr("reused", "复用"),
                    rs.connections_new,
                    tr("new", "新建"),
                    reuse_pct,
                    tr("reuse", "复用率"),
                    rs.unique_hosts,
                    tr("unique hosts", "个独立主机"),
                );
            }
            // Compression audit. Only render when there's either
            // compression in use or a miss to flag.
            if rs.compressed_count > 0 || rs.uncompressed_text_count > 0 {
                let mut line = format!(
                    "- {}: **{}** {}",
                    tr("Compression", "压缩"),
                    rs.compressed_count,
                    tr("compressed", "已压缩"),
                );
                if rs.uncompressed_text_count > 0 {
                    line.push_str(&format!(
                        " · **{}** {} (**{}** {}) ⚠️",
                        rs.uncompressed_text_count,
                        tr("uncompressed text resources", "个未压缩的文本资源",),
                        format_bytes(rs.uncompressed_text_bytes),
                        tr("could be compressed", "本可压缩"),
                    ));
                }
                let _ = writeln!(s, "{line}");
            }
            // Compression algorithm breakdown — sort gzip/br/zstd by
            // count desc so the dominant codec leads.
            if !rs.compression_breakdown.is_empty() {
                let mut algos: Vec<(&String, &u32)> = rs.compression_breakdown.iter().collect();
                algos.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let line = algos
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(
                    s,
                    "- {}: {line}",
                    tr("Compression breakdown", "压缩算法分布"),
                );
            }
            // Cache-Control coverage — single ratio so monitors can
            // alert when a deploy drops headers from static assets.
            let cc_total = rs.cache_control_present + rs.cache_control_missing;
            if cc_total > 0 {
                let cov = (rs.cache_control_present as f64) * 100.0 / (cc_total as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{:.0}%** ({} {} · {} {})",
                    tr("Cache-Control coverage", "Cache-Control 覆盖率"),
                    cov,
                    rs.cache_control_present,
                    tr("present", "已设置"),
                    rs.cache_control_missing,
                    tr("missing", "未设置"),
                );
            }
            // Image-format modernisation — Lighthouse "Serve images
            // in next-gen formats" signal.
            let img_total = rs.legacy_image_bytes + rs.modern_image_bytes;
            if img_total > 0 {
                let modern_pct = (rs.modern_image_bytes as f64) * 100.0 / (img_total as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} (JPEG/PNG/GIF) · **{}** {} (WebP/AVIF) — **{:.0}%** {}",
                    tr("Image formats", "图片格式"),
                    format_bytes(rs.legacy_image_bytes),
                    tr("legacy", "传统格式"),
                    format_bytes(rs.modern_image_bytes),
                    tr("modern", "现代格式"),
                    modern_pct,
                    tr("modern", "现代格式占比"),
                );
            }
            // Source-map coverage across JS / CSS.
            let sm_total = rs.source_maps_present + rs.source_maps_missing;
            if sm_total > 0 {
                let cov = (rs.source_maps_present as f64) * 100.0 / (sm_total as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{:.0}%** {} ({} {} · {} {})",
                    tr("Source maps", "Source map"),
                    cov,
                    tr("of JS/CSS resources", "JS/CSS 资源覆盖"),
                    rs.source_maps_present,
                    tr("present", "已发布"),
                    rs.source_maps_missing,
                    tr("missing", "未发布"),
                );
            }
            // Duplicate-resource findings — only render when something
            // was detected; otherwise stay silent (empty lists carry no
            // information for a markdown reader).
            let dr = &rs.duplicate_resources;
            if dr.wasted_bytes > 0 || !dr.exact_url.is_empty() || !dr.likely_same_file.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} {} {}, {} {} ⚠️",
                    tr("Duplicate resources", "重复资源"),
                    format_bytes(dr.wasted_bytes),
                    tr("wasted across", "浪费，分布于"),
                    dr.exact_url.len(),
                    tr(
                        if dr.exact_url.len() == 1 {
                            "exact-URL group"
                        } else {
                            "exact-URL groups"
                        },
                        "组同 URL 重复",
                    ),
                    dr.likely_same_file.len(),
                    tr(
                        if dr.likely_same_file.len() == 1 {
                            "likely-same-file group"
                        } else {
                            "likely-same-file groups"
                        },
                        "组疑似同文件",
                    ),
                );
                for e in dr.exact_url.iter().take(3) {
                    let display = if e.key.len() > 80 {
                        format!("…{}", &e.key[e.key.len() - 77..])
                    } else {
                        e.key.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - {}: `{}` ×{} ({} {})",
                        tr("exact", "同 URL"),
                        display,
                        e.count,
                        format_bytes(e.wasted_bytes),
                        tr("wasted", "浪费"),
                    );
                }
                for e in dr.likely_same_file.iter().take(3) {
                    let _ = writeln!(
                        s,
                        "  - {}: `{}` {} {} URLs ({} {})",
                        tr("same-file", "同文件"),
                        e.key,
                        tr("across", "分布于"),
                        e.count,
                        format_bytes(e.wasted_bytes),
                        tr("wasted", "浪费"),
                    );
                }
            }
            // Mixed content — only render when detected. Clean HTTPS
            // pages and HTTP-served pages stay silent.
            let mc = &rs.mixed_content;
            if mc.detected {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} ⚠️",
                    tr("Mixed content", "混合内容"),
                    mc.total_count,
                    tr(
                        if mc.total_count == 1 {
                            "plain-HTTP resource on HTTPS page"
                        } else {
                            "plain-HTTP resources on HTTPS page"
                        },
                        "个明文 HTTP 资源出现在 HTTPS 页面",
                    ),
                );
                for r in mc.resources.iter().take(3) {
                    let display = if r.url.len() > 80 {
                        format!("…{}", &r.url[r.url.len() - 77..])
                    } else {
                        r.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - [{}] `{}` ({})",
                        r.kind,
                        display,
                        format_bytes(r.content_size),
                    );
                }
            }
            // Critical-chain depth — only render when initiators were
            // captured (`None` means `initiators=false`, value would be
            // meaningless). `0` is a real signal too: every resource
            // was parser-initiated, no JS-driven secondary fetches.
            if let Some(depth) = rs.max_initiator_chain_depth {
                let _ = writeln!(
                    s,
                    "- {}: **{depth}**",
                    tr("Max initiator chain depth", "最深请求依赖链",),
                );
            }
            // Per-type "largest resources" leaderboards. Stable bucket
            // order so the markdown diffs cleanly across captures.
            if !rs.top_largest_by_type.is_empty() {
                for bucket in ["javascript", "css", "image", "font"] {
                    let Some(list) = rs.top_largest_by_type.get(bucket) else {
                        continue;
                    };
                    if list.is_empty() {
                        continue;
                    }
                    let _ = writeln!(s, "- {} {bucket}:", tr("Largest", "最大"),);
                    for e in list.iter().take(5) {
                        let display = if e.url.len() > 80 {
                            format!("…{}", &e.url[e.url.len() - 77..])
                        } else {
                            e.url.clone()
                        };
                        let cache_tag = if e.from_cache {
                            tr(" (cached)", "（来自缓存）")
                        } else {
                            ""
                        };
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}{}",
                            display,
                            format_bytes(e.bytes),
                            cache_tag,
                        );
                    }
                }
            }
            // Uncompressed-text offenders — already summarised in the
            // compression line above; this section drills into specific
            // URLs so the AI can suggest concrete fixes.
            if !rs.uncompressed_text_resources.is_empty() {
                let _ = writeln!(
                    s,
                    "- {} ({} {}):",
                    tr("Uncompressed text resources", "未压缩的文本资源",),
                    tr("top", "前"),
                    rs.uncompressed_text_resources.len().min(5),
                );
                for e in rs.uncompressed_text_resources.iter().take(5) {
                    let display = if e.url.len() > 80 {
                        format!("…{}", &e.url[e.url.len() - 77..])
                    } else {
                        e.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - `{}` — {} ({})",
                        display,
                        format_bytes(e.bytes),
                        e.mime_type,
                    );
                }
            }
            // Cache-policy anti-patterns on static assets — surfaces
            // the actionable subset (short max-age + missing-immutable
            // on fingerprinted URLs) without paging through resources.
            if !rs.cache_policy_issues.is_empty() {
                let short_count = rs
                    .cache_policy_issues
                    .iter()
                    .filter(|i| i.reason == "short_max_age")
                    .count();
                let immut_count = rs
                    .cache_policy_issues
                    .iter()
                    .filter(|i| i.reason == "missing_immutable")
                    .count();
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} · **{}** {} ⚠️",
                    tr("Cache-policy issues", "缓存策略问题"),
                    short_count,
                    tr("short max-age", "max-age 过短"),
                    immut_count,
                    tr("missing immutable", "未加 immutable"),
                );
                for e in rs.cache_policy_issues.iter().take(5) {
                    let display = if e.url.len() > 80 {
                        format!("…{}", &e.url[e.url.len() - 77..])
                    } else {
                        e.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - [{}] `{}` — `{}`",
                        e.reason, display, e.cache_control,
                    );
                }
            }
            // Resource-hint audit — only rendered when the caller
            // opted in (`resource_hints=true` / `all_metrics=true`).
            // `gap` empty AND both declared lists empty → silent;
            // otherwise show the gap (highest priority for the AI)
            // and a one-line summary of declared coverage.
            if let Some(rh) = &rs.resource_hints {
                let declared_total = rh.declared_preconnect.len() + rh.declared_dns_prefetch.len();
                if !rh.gap.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {} ⚠️",
                        tr("Resource-hint gaps", "资源提示遗漏"),
                        rh.gap.len(),
                        tr(
                            if rh.gap.len() == 1 {
                                "third-party host hit without preconnect/dns-prefetch"
                            } else {
                                "third-party hosts hit without preconnect/dns-prefetch"
                            },
                            "个第三方主机命中但未声明 preconnect/dns-prefetch",
                        ),
                    );
                    for g in rh.gap.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {} ({} {})",
                            g.host,
                            format_bytes(g.bytes),
                            g.count,
                            tr(
                                if g.count == 1 {
                                    "resource"
                                } else {
                                    "resources"
                                },
                                "个资源",
                            ),
                        );
                    }
                }
                if declared_total > 0 {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** preconnect · **{}** dns-prefetch",
                        tr("Declared resource hints", "已声明的资源提示"),
                        rh.declared_preconnect.len(),
                        rh.declared_dns_prefetch.len(),
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(cov) = &self.coverage {
            let _ = writeln!(s, "{}", tr("## CSS / JS Coverage", "## CSS / JS 覆盖率"));
            let _ = writeln!(s);
            if cov.js_total_bytes > 0 {
                let _ = writeln!(
                    s,
                    "- JS: **{}** {} / {} {} (**{:.0}%** {})",
                    format_bytes(cov.js_unused_bytes),
                    tr("unused", "未使用"),
                    format_bytes(cov.js_total_bytes),
                    tr("total", "总计"),
                    cov.js_unused_ratio * 100.0,
                    tr("unused", "未使用"),
                );
            }
            if cov.css_total_bytes > 0 {
                let _ = writeln!(
                    s,
                    "- CSS: **{}** {} / {} {} (**{:.0}%** {})",
                    format_bytes(cov.css_unused_bytes),
                    tr("unused", "未使用"),
                    format_bytes(cov.css_total_bytes),
                    tr("total", "总计"),
                    cov.css_unused_ratio * 100.0,
                    tr("unused", "未使用"),
                );
            }
            if !cov.top_unused.is_empty() {
                let _ = writeln!(s, "- {}", tr("Top wasteful files:", "最浪费的文件："));
                for e in cov.top_unused.iter().take(5) {
                    let display_url = if e.url.len() > 80 {
                        format!("…{}", &e.url[e.url.len() - 77..])
                    } else {
                        e.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - [{}] `{}` — {} {} ({:.0}%)",
                        e.kind,
                        display_url,
                        format_bytes(e.unused_bytes),
                        tr("unused", "未使用"),
                        e.unused_ratio * 100.0,
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(tls) = &self.tls_info {
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "## TLS / Certificate (main document)",
                    "## TLS / 主文档证书",
                ),
            );
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: `{}`{}",
                tr("Host", "主机"),
                tls.host,
                format_remote_ip(tls),
            );
            let _ = writeln!(
                s,
                "- {}: **{}** · {}: `{}`{}",
                tr("Protocol", "协议"),
                tls.protocol,
                tr("cipher", "加密套件"),
                tls.cipher,
                match &tls.key_exchange {
                    Some(k) => format!(" · {}: `{k}`", tr("key exchange", "密钥交换"),),
                    None => String::new(),
                },
            );
            let _ = writeln!(s, "- {}: `{}`", tr("Subject", "签发对象"), tls.subject_name);
            let _ = writeln!(s, "- {}: `{}`", tr("Issuer", "颁发机构"), tls.issuer);
            let _ = writeln!(
                s,
                "- {}: {}",
                tr("Validity", "有效期"),
                format_tls_expiry(tls.days_remaining),
            );
            if !tls.san_list.is_empty() {
                let sans = tls
                    .san_list
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(
                    s,
                    "- SANs ({}): {}{}",
                    tls.san_list.len(),
                    sans,
                    if tls.san_list.len() > 8 { ", …" } else { "" },
                );
            }
            let _ = writeln!(s);
        }

        if !self.tls_certificates.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## TLS Certificates by Host", "## 按主机分组的 TLS 证书",),
                self.tls_certificates.len(),
            );
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "| Host | IP | Protocol | Issuer | Validity |",
                    "| 主机 | IP | 协议 | 颁发机构 | 有效期 |",
                ),
            );
            let _ = writeln!(s, "|---|---|---|---|---|");
            for tls in &self.tls_certificates {
                let ip_cell = match (&tls.remote_ip, tls.remote_port) {
                    (Some(ip), Some(443)) => format!("`{ip}`"),
                    (Some(ip), Some(p)) => format!("`{ip}:{p}`"),
                    (Some(ip), None) => format!("`{ip}`"),
                    (None, _) => String::from("—"),
                };
                let _ = writeln!(
                    s,
                    "| `{}` | {} | {} | {} | {} |",
                    tls.host,
                    ip_cell,
                    tls.protocol,
                    tls.issuer,
                    format_tls_expiry(tls.days_remaining),
                );
            }
            let _ = writeln!(s);
        }

        // Security audit scorecard — rendered as a compact 2-line view
        // so AI can see headers score + cookie coverage without scanning
        // the full headers map below. Always emitted (the struct is
        // always populated).
        {
            let a = &self.security_audit;
            let _ = writeln!(s, "{}", tr("## Security Audit", "## 安全审计"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}/{}** {}{}",
                tr("Headers", "响应头"),
                a.headers.present_count,
                CORE_SECURITY_HEADERS.len(),
                tr("core present", "核心头已配置"),
                if a.headers.missing.is_empty() {
                    String::new()
                } else {
                    format!(
                        " — {}: {}",
                        tr("missing", "缺失"),
                        a.headers.missing.join(", "),
                    )
                },
            );
            if a.cookies.total > 0 {
                let pct = |n: u32| (n as f64) * 100.0 / (a.cookies.total as f64);
                let mut line = format!(
                    "- {} ({}): Secure **{:.0}%** · HttpOnly **{:.0}%** · SameSite **{:.0}%**",
                    tr("Cookies", "Cookie"),
                    a.cookies.total,
                    pct(a.cookies.secure),
                    pct(a.cookies.http_only),
                    pct(a.cookies.same_site_set),
                );
                if a.cookies.same_site_none_without_secure > 0 {
                    line.push_str(&format!(
                        " ⚠️ {} {}",
                        a.cookies.same_site_none_without_secure,
                        tr(
                            "cookie(s) `SameSite=None` without `Secure`",
                            "个 Cookie 标了 `SameSite=None` 却没加 `Secure`",
                        ),
                    ));
                }
                let _ = writeln!(s, "{line}");
                // Cookie header byte size — flag when approaching the
                // 4 KB framework limit. Otherwise stay quiet (most
                // pages have tiny cookies).
                let hdr = a.cookies.header_bytes;
                if hdr >= 4096 {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** ⚠️ {}",
                        tr("Cookie header size", "Cookie 请求头大小"),
                        format_bytes(hdr),
                        tr(
                            "(≥ 4 KB — every request pays this tax)",
                            "(≥ 4 KB —— 每个请求都要带这么多)",
                        ),
                    );
                } else if hdr > 0 {
                    let _ = writeln!(
                        s,
                        "- {}: **{}**",
                        tr("Cookie header size", "Cookie 请求头大小"),
                        format_bytes(hdr),
                    );
                }
            } else {
                let _ = writeln!(
                    s,
                    "- {}: {}",
                    tr("Cookies", "Cookie"),
                    tr("(none)", "（无）")
                );
            }
            let _ = writeln!(s);
        }

        if let Some(sh) = &self.security_headers {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Security Headers", "## 安全响应头"),
                sh.len(),
            );
            let _ = writeln!(s);
            let mut items: Vec<(&String, &String)> = sh.iter().collect();
            items.sort_by_key(|(k, _)| k.as_str());
            for (k, v) in items {
                // Truncate very long CSP values for readability.
                let val = if v.len() > 200 {
                    format!("{}…", &v[..200])
                } else {
                    v.clone()
                };
                let _ = writeln!(s, "- `{k}`: {val}");
            }
            let _ = writeln!(s);
        }

        if let Some(sw) = &self.service_worker {
            let _ = writeln!(s, "{}", tr("## Service Worker", "## Service Worker"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}**",
                tr("Controlled", "已接管页面"),
                sw.controlled,
            );
            if let Some(scope) = &sw.scope {
                let _ = writeln!(s, "- {}: `{scope}`", tr("Scope", "作用域"));
            }
            if let Some(script) = &sw.active_script {
                let _ = writeln!(s, "- {}: `{script}`", tr("Active script", "激活的脚本"));
            }
            if sw.waiting {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr("Update **waiting** for activation", "有更新**等待**激活",),
                );
            }
            if sw.installing {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr("A SW is **installing**", "正在**安装** Service Worker"),
                );
            }
            let _ = writeln!(s);
        }

        if let Some(rb) = &self.render_blocking_resources {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Render-Blocking Resources", "## 阻塞渲染的资源",),
                rb.len(),
            );
            let _ = writeln!(s);
            if rb.is_empty() {
                let _ = writeln!(s, "- {}", tr("None detected.", "未发现。"));
            } else {
                for r in rb.iter().take(10) {
                    let _ = writeln!(s, "- `<{}>` `{}` — {}", r.tag, r.url, r.why);
                }
                if rb.len() > 10 {
                    let _ = writeln!(
                        s,
                        "- {} {} {}.",
                        tr("…and", "……还有"),
                        rb.len() - 10,
                        tr("more", "项更多"),
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(imgs) = &self.image_sizing {
            // Headline summary: counts + how many are wasteful enough to
            // matter (>50% waste AND >50KB transferred, or in-viewport
            // with any oversize). Empty list still rendered for "audited
            // but clean" signal.
            let total = imgs.len();
            let loaded = imgs.iter().filter(|i| i.loaded).count();
            let lazy_offscreen = imgs
                .iter()
                .filter(|i| i.loading == "lazy" && !i.in_viewport)
                .count();
            let alt_missing = imgs.iter().filter(|i| i.alt_missing).count();
            let _ = writeln!(s, "{} ({total})", tr("## Image Sizing", "## 图片尺寸审计"),);
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {loaded} {} · {lazy_offscreen} {} · {alt_missing} {}",
                tr("loaded", "已加载"),
                tr("lazy/off-screen", "懒加载/首屏外"),
                tr("without alt", "缺 alt"),
            );
            // Top offenders: significant waste OR meaningful bytes.
            let top: Vec<&ImageSizing> = imgs
                .iter()
                .filter(|i| {
                    i.loaded
                        && i.waste_ratio.map(|w| w >= 0.4).unwrap_or(false)
                        && i.transferred_bytes.map(|b| b >= 20_000).unwrap_or(true)
                })
                .take(10)
                .collect();
            if top.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr(
                        "No significantly oversized images detected.",
                        "未发现明显过大的图片。",
                    ),
                );
            } else {
                let _ = writeln!(s);
                let _ = writeln!(
                    s,
                    "{}",
                    tr(
                        "| URL | Natural | Display | Waste | Bytes | Viewport |",
                        "| URL | 原生尺寸 | 显示尺寸 | 浪费 | 字节 | 首屏 |",
                    ),
                );
                let _ = writeln!(s, "|---|---|---|---|---|---|");
                for i in &top {
                    let waste = i
                        .waste_ratio
                        .map(|w| format!("{:.0}%", w * 100.0))
                        .unwrap_or_else(|| "?".into());
                    let bytes = i
                        .transferred_bytes
                        .map(format_bytes)
                        .unwrap_or_else(|| "?".into());
                    let vp = if i.in_viewport {
                        tr("**yes**", "**是**")
                    } else {
                        tr("no", "否")
                    };
                    // Trim long URLs to keep the table readable.
                    let short_url = if i.url.len() > 60 {
                        format!("…{}", &i.url[i.url.len() - 57..])
                    } else {
                        i.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "| `{}` | {}×{} | {}×{} | **{}** | {} | {} |",
                        short_url,
                        i.natural_width,
                        i.natural_height,
                        i.display_width,
                        i.display_height,
                        waste,
                        bytes,
                        vp,
                    );
                }
            }
            // Lighthouse "image" four-pack — one short subsection per
            // category, each silent when its list is empty. Showing the
            // top URL + key numbers (display W×H, oversize ratio) keeps
            // the markdown skimmable while still pinning down which
            // file is the worst offender.
            if let Some(audit) = &self.image_audit {
                let trim_url = |u: &str| -> String {
                    if u.len() > 60 {
                        format!("…{}", &u[u.len() - 57..])
                    } else {
                        u.to_string()
                    }
                };
                if !audit.oversized.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {}",
                        tr("Oversized (>2× display)", "过大（> 显示尺寸 2 倍）",),
                        audit.oversized.len(),
                        tr(
                            if audit.oversized.len() == 1 {
                                "image"
                            } else {
                                "images"
                            },
                            "张图片",
                        ),
                    );
                    for i in audit.oversized.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — **{:.1}×** {} {}×{}",
                            trim_url(&i.url),
                            i.ratio,
                            tr("at", "显示为"),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
                if !audit.missing_dimensions.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {}",
                        tr(
                            "Missing `width`/`height` attrs (CLS risk)",
                            "缺 `width`/`height` 属性（CLS 风险）",
                        ),
                        audit.missing_dimensions.len(),
                        tr(
                            if audit.missing_dimensions.len() == 1 {
                                "image"
                            } else {
                                "images"
                            },
                            "张图片",
                        ),
                    );
                    for i in audit.missing_dimensions.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}×{}",
                            trim_url(&i.url),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
                if !audit.missing_lazy.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}**",
                        tr(
                            "Below-fold images NOT marked `loading=\"lazy\"`",
                            "首屏外图片未加 `loading=\"lazy\"`",
                        ),
                        audit.missing_lazy.len(),
                    );
                    for i in audit.missing_lazy.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}×{}",
                            trim_url(&i.url),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
                if !audit.missing_srcset.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {}",
                        tr(
                            "Missing `srcset` (no responsive variants)",
                            "缺 `srcset`（没有响应式变体）",
                        ),
                        audit.missing_srcset.len(),
                        tr(
                            if audit.missing_srcset.len() == 1 {
                                "image"
                            } else {
                                "images"
                            },
                            "张图片",
                        ),
                    );
                    for i in audit.missing_srcset.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}×{}",
                            trim_url(&i.url),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
            }
            let _ = writeln!(s);
        }

        if let Some(fa) = &self.font_audit {
            let _ = writeln!(s, "{}", tr("## Font Audit", "## 字体审计"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}** {} · **{}** {} · **{}** {}",
                tr("Fonts", "字体"),
                fa.font_count,
                tr("declared", "已声明"),
                fa.loaded_count,
                tr("loaded", "已加载"),
                fa.declared_preload_count,
                tr("preloaded", "已预加载"),
            );
            // font-display distribution — sort desc by count so the
            // dominant value leads, ties broken alphabetically for
            // stable diffs across captures.
            if !fa.display_distribution.is_empty() {
                let mut dist: Vec<(&String, &u32)> = fa.display_distribution.iter().collect();
                dist.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let line = dist
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(s, "- `font-display`: {line}");
            }
            if !fa.missing_swap.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} ⚠️",
                    tr(
                        "FOIT risk (no `font-display: swap`)",
                        "FOIT 风险（未声明 `font-display: swap`）",
                    ),
                    fa.missing_swap.len(),
                    tr(
                        if fa.missing_swap.len() == 1 {
                            "face"
                        } else {
                            "faces"
                        },
                        "个字体",
                    ),
                );
                for f in fa.missing_swap.iter().take(5) {
                    let url_part = match &f.source_url {
                        Some(u) => {
                            let trimmed = if u.len() > 70 {
                                format!("…{}", &u[u.len() - 67..])
                            } else {
                                u.clone()
                            };
                            format!(" — `{trimmed}`")
                        }
                        None => String::new(),
                    };
                    let display_part = match &f.display {
                        Some(d) if !d.is_empty() => format!(" (`{d}`)"),
                        _ => " (`auto`)".to_string(),
                    };
                    let family = if f.family.is_empty() {
                        tr("(unnamed)", "（未命名）").to_string()
                    } else {
                        f.family.clone()
                    };
                    let _ = writeln!(s, "  - **{family}**{display_part}{url_part}",);
                }
            }
            // CORS blind-spot honesty signal — only render when
            // non-zero so clean audits stay quiet.
            if fa.unreadable_stylesheets > 0 {
                let _ = writeln!(
                    s,
                    "- ⚠️ **{}** {} {}",
                    fa.unreadable_stylesheets,
                    tr(
                        if fa.unreadable_stylesheets == 1 {
                            "stylesheet"
                        } else {
                            "stylesheets"
                        },
                        "个样式表",
                    ),
                    tr(
                        "unreadable (cross-origin without `crossorigin`) — audit may be incomplete",
                        "无法读取（跨域且未加 `crossorigin`） — 审计可能不完整",
                    ),
                );
            }
            let _ = writeln!(s);
        }

        if let Some(md) = &self.metadata {
            let _ = writeln!(s, "{}", tr("## Page Metadata", "## 页面元数据"));
            let _ = writeln!(s);
            let _ = writeln!(s, "- {}: **{}**", tr("Title", "标题"), md.title);
            if let Some(d) = &md.description {
                let _ = writeln!(s, "- {}: {d}", tr("Description", "描述"));
            }
            if let Some(c) = &md.canonical {
                let _ = writeln!(s, "- {}: `{c}`", tr("Canonical", "Canonical URL"));
            }
            if let Some(r) = &md.robots {
                let _ = writeln!(s, "- {}: `{r}`", tr("Robots", "Robots 指令"));
            }
            if let Some(l) = &md.lang {
                let _ = writeln!(s, "- {}: `{l}`", tr("Lang", "语言"));
            }
            if let Some(v) = &md.viewport {
                let _ = writeln!(s, "- Viewport: `{v}`");
            }
            if let Some(ch) = &md.charset {
                let _ = writeln!(s, "- {}: `{ch}`", tr("Charset", "字符集"));
            }
            if let Some(tc) = &md.theme_color {
                let _ = writeln!(s, "- {}: `{tc}`", tr("Theme color", "主题色"));
            }
            if !md.og.is_empty() {
                let _ = writeln!(
                    s,
                    "- Open Graph ({} {}):",
                    md.og.len(),
                    tr("tags", "个标签"),
                );
                let mut og: Vec<(&String, &String)> = md.og.iter().collect();
                og.sort_by_key(|x| x.0.clone());
                for (k, v) in og.iter().take(8) {
                    let _ = writeln!(s, "  - `og:{k}` = {v}");
                }
                if md.og.len() > 8 {
                    let _ = writeln!(
                        s,
                        "  - {} {} {}.",
                        tr("…and", "……还有"),
                        md.og.len() - 8,
                        tr("more", "项"),
                    );
                }
            }
            if !md.twitter.is_empty() {
                let _ = writeln!(
                    s,
                    "- Twitter ({} {}):",
                    md.twitter.len(),
                    tr("tags", "个标签"),
                );
                let mut tw: Vec<(&String, &String)> = md.twitter.iter().collect();
                tw.sort_by_key(|x| x.0.clone());
                for (k, v) in tw.iter().take(8) {
                    let _ = writeln!(s, "  - `twitter:{k}` = {v}");
                }
                if md.twitter.len() > 8 {
                    let _ = writeln!(
                        s,
                        "  - {} {} {}.",
                        tr("…and", "……还有"),
                        md.twitter.len() - 8,
                        tr("more", "项"),
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(m) = &self.metrics {
            let _ = writeln!(s, "{}", tr("## Page Metrics", "## 页面性能指标"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- JS heap **{} / {}** · {} **{}** · {} **{}** · {} **{}** · {} **{}**",
                format_bytes(m.js_heap_used),
                format_bytes(m.js_heap_total),
                tr("nodes", "节点"),
                m.nodes,
                tr("frames", "frame"),
                m.frames,
                tr("documents", "document"),
                m.documents,
                tr("event listeners", "事件监听器"),
                m.js_event_listeners,
            );
            let _ = writeln!(
                s,
                "- CPU: {} **{:.1}ms** · {} **{:.1}ms** · {} **{:.1}ms** · {} **{:.1}ms**",
                tr("script", "脚本"),
                m.script_duration_ms,
                tr("layout", "布局"),
                m.layout_duration_ms,
                tr("style", "样式"),
                m.recalc_style_duration_ms,
                tr("total task", "总任务"),
                m.task_duration_ms,
            );
            let _ = writeln!(s);
        }

        if let Some(dm) = &self.dom_mutations {
            let total = dm.total_added_nodes + dm.total_removed_nodes + dm.total_attribute_changes;
            let rate = if dm.observation_window_ms > 0 {
                (total as f64) * 1000.0 / (dm.observation_window_ms as f64)
            } else {
                0.0
            };
            let _ = writeln!(s, "{}", tr("## DOM Mutations", "## DOM 变更"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {} **{total}** {} {}ms (~**{:.0}/sec**) — {} **{}** · {} **{}** · {} **{}**",
                tr("Total", "共"),
                tr("over", "记录于"),
                dm.observation_window_ms,
                rate,
                tr("added", "新增"),
                dm.total_added_nodes,
                tr("removed", "移除"),
                dm.total_removed_nodes,
                tr("attribute", "属性变更"),
                dm.total_attribute_changes,
            );
            if !dm.top_tags_by_mutation_count.is_empty() {
                let line = dm
                    .top_tags_by_mutation_count
                    .iter()
                    .take(5)
                    .map(|c| format!("`<{}>` {}", c.name, c.count))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(s, "- {}: {line}", tr("Top tags", "热点标签"));
            }
            if !dm.top_attributes_changed.is_empty() {
                let line = dm
                    .top_attributes_changed
                    .iter()
                    .take(5)
                    .map(|c| format!("`{}` {}", c.name, c.count))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(s, "- {}: {line}", tr("Top attributes", "热点属性"));
            }
            let _ = writeln!(s);
        }

        // ─── Details / raw enumerations ─────────────────────────────────

        if !self.resources.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Resources", "## 资源清单"),
                self.resources.len(),
            );
            let _ = writeln!(s);
            for r in &self.resources {
                let _ = writeln!(s, "- {}", describe_resource(r, lang));
            }
            let _ = writeln!(s);
        }

        if !self.cookies.is_empty() {
            // Name + domain only — values may contain session tokens. Use the
            // JSON response if you need the actual values.
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Cookies", "## Cookie"),
                self.cookies.len(),
            );
            let _ = writeln!(s);
            for c in &self.cookies {
                let _ = writeln!(s, "- `{}` on `{}`", c.name, c.domain);
            }
            let _ = writeln!(s);
        }

        // ─── Binary attachments ─────────────────────────────────────────

        if self.screenshot.is_some() {
            let _ = writeln!(s, "{}", tr("## Screenshot", "## 截图"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "Base64 PNG captured (omitted from markdown body).",
                    "已采集 Base64 PNG（不在 markdown 正文里输出）。",
                ),
            );
            let _ = writeln!(s);
        }

        if self.pdf.is_some() {
            let _ = writeln!(s, "{}", tr("## PDF", "## PDF"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "Base64 PDF captured (omitted from markdown body).",
                    "已采集 Base64 PDF（不在 markdown 正文里输出）。",
                ),
            );
            let _ = writeln!(s);
        }

        if let Some(har) = &self.har {
            let entries = har
                .get("log")
                .and_then(|l| l.get("entries"))
                .and_then(|e| e.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let _ = writeln!(s, "{}", tr("## HAR", "## HAR"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{} ({entries} {}).",
                tr("HAR 1.2 archive included", "已包含 HAR 1.2 归档",),
                tr(
                    "entries; omitted from markdown body",
                    "条记录；不在 markdown 正文里输出",
                ),
            );
            let _ = writeln!(s);
        }

        if let Some(snap) = &self.dom_snapshot {
            let docs = snap
                .get("documents")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let strings = snap
                .get("strings")
                .and_then(|s| s.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let _ = writeln!(s, "{}", tr("## DOM Snapshot", "## DOM 快照"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{} ({docs} {}, {strings} {}).",
                tr("DOMSnapshot included", "已包含 DOMSnapshot"),
                tr(
                    if docs == 1 { "document" } else { "documents" },
                    "个 document",
                ),
                tr(
                    "interned strings; omitted from markdown body",
                    "条 interned 字符串；不在 markdown 正文里输出",
                ),
            );
            let _ = writeln!(s);
        }

        let _ = writeln!(s, "{}", tr("## Page Content", "## 页面内容"));
        let _ = writeln!(s);
        let _ = writeln!(s, "```");
        s.push_str(&self.data);
        if !self.data.ends_with('\n') {
            s.push('\n');
        }
        let _ = writeln!(s, "```");

        s
    }
}

fn describe_resource(r: &WebPageResource, lang: Lang) -> String {
    let tr = |en: &'static str, zh: &'static str| -> &'static str {
        match lang {
            Lang::En => en,
            Lang::Zh => zh,
        }
    };
    let mime = if r.mime_type.is_empty() {
        tr("unknown type", "未知类型")
    } else {
        r.mime_type.as_str()
    };

    if r.from_cache {
        return format!(
            "{} `{}` {} ({}, {} {}).",
            tr("Served", "从浏览器缓存提供"),
            r.url,
            tr("from browser cache", ""),
            mime,
            tr("status", "状态码"),
            r.status,
        );
    }

    let ttfb = r
        .timing
        .as_ref()
        .map(|t| t.receive_headers_end)
        .filter(|&t| t >= 0.0)
        .map(|t| format!(", TTFB {}ms", t as u32))
        .unwrap_or_default();

    let size = format_bytes(r.content_size);
    let conn = if r.connection_reused {
        tr(", connection reused", "，连接复用")
    } else {
        ""
    };

    match r.status {
        200..=299 => format!(
            "{} `{}` {} {} ({}, {} {}{}{}).",
            tr("Loaded", "加载"),
            r.url,
            tr("as", "为"),
            mime,
            size,
            tr("status", "状态码"),
            r.status,
            ttfb,
            conn,
        ),
        300..=399 => format!(
            "{} ({} {}) {} `{}`{}.",
            tr("Redirected", "重定向"),
            tr("status", "状态码"),
            r.status,
            tr("from", "自"),
            r.url,
            ttfb,
        ),
        400..=499 => format!(
            "{} `{}` ({} {}, {}).",
            tr("Client error fetching", "请求客户端错误"),
            r.url,
            tr("status", "状态码"),
            r.status,
            mime,
        ),
        500..=599 => format!(
            "{} `{}` ({} {}, {}).",
            tr("Server error fetching", "请求服务端错误"),
            r.url,
            tr("status", "状态码"),
            r.status,
            mime,
        ),
        _ => format!(
            "{} `{}` {} {} {} ({}, {}).",
            tr("Fetched", "请求"),
            r.url,
            tr("with status", "状态码"),
            r.status,
            "",
            mime,
            size,
        ),
    }
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else if n < GB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else {
        format!("{:.2} GB", n as f64 / GB as f64)
    }
}

/// Format the `data` field is populated in. Owned by browser module so the
/// HTTP layer doesn't have to know about htmd or DOM walkers.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DataFormat {
    #[default]
    Html,
    Markdown,
    /// Computed-style-aware DOM walker, inserts newlines between block-like
    /// elements. Better than `innerText` for flex/grid pages.
    Text,
}

/// Language used for the **markdown rendering** of the response
/// (section headings, prose, warning labels). The JSON envelope is
/// unaffected — all field names, enum tag values (`"missing_immutable"`,
/// `"short_max_age"`, etc.), and machine-readable strings stay English
/// regardless of `lang`, so downstream code that branches on those
/// values keeps working across languages.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Zh,
}

/// All knobs `capture` accepts. Owned fields so callers don't juggle
/// lifetimes; the struct is consumed by `capture`.
pub struct SummaryRequest {
    pub url: String,
    pub timeout: Duration,
    pub screenshot: bool,
    pub wait_for_request: Vec<String>,
    /// When `true`, the collect stage exits shortly after the `load`
    /// (onload) lifecycle event instead of waiting for `networkIdle`.
    /// Caller-controlled — set explicitly per request. See the
    /// `SummaryQuery::wait_until_load` doc comment for trade-offs.
    pub wait_until_load: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub device_scale_factor: Option<f64>,
    pub user_agent: Option<String>,
    pub accept_language: Option<String>,
    pub cookies: Vec<(String, String)>,
    /// IANA timezone identifier (`Asia/Shanghai`, `America/New_York`, ...)
    /// — overrides `Intl.DateTimeFormat`, `Date.now()` zone, etc.
    pub timezone: Option<String>,
    /// BCP 47 locale (`zh-CN`, `fr-FR`, ...) — overrides `navigator.language`
    /// and `Intl.*` defaults.
    pub locale: Option<String>,
    /// Override the page's geolocation. Setting this grants permission and
    /// makes `navigator.geolocation.getCurrentPosition` return the fixed
    /// coordinates.
    pub geolocation: Option<Geolocation>,
    /// CPU slowdown multiplier (1.0 = no throttle, 2.0 = 2× slower, ...).
    /// Useful for low-end-device simulation. Values ≤ 1.0 are no-ops.
    pub cpu_throttle: Option<f64>,
    /// Enable touch event emulation (mobile-style). Combined with a small
    /// viewport this is what DevTools' "Mobile" mode does. Max touch
    /// points hardcoded to 5 (typical phone).
    pub touch: bool,
    /// Extra HTTP headers added to every request the page makes.
    pub headers: std::collections::HashMap<String, String>,
    /// URL substrings — any request whose URL contains any of these is
    /// blocked at the network layer (treated as a CDP wildcard `*pat*`).
    pub block_urls: Vec<String>,
    /// Resource types to block (e.g. `["image", "font", "stylesheet"]`).
    /// Unknown / unrecognized strings are silently dropped. Uses CDP
    /// `Fetch` domain with a per-page interception task that pauses every
    /// request, decides block vs continue based on `resource_type`, and
    /// is torn down when this request completes.
    pub block_resource_types: Vec<String>,
    pub disable_cache: bool,
    pub wait_for_element: Option<String>,
    /// JS expression polled with exponential backoff; resolves when it
    /// returns a truthy value. Evaluated after `wait_for_element`.
    pub wait_for_function: Option<String>,
    pub settle: Option<Duration>,
    /// Arbitrary JavaScript evaluated in the page's main world after
    /// `settle`, just before `data` extraction. Use it to dismiss modals,
    /// trigger lazy-load, scroll, or otherwise mutate the DOM so the final
    /// HTML reflects the post-script state. Promise return values are
    /// awaited by chromiumoxide. Runtime errors abort the request.
    pub script: Option<String>,
    pub capture_element: Option<String>,
    pub data_format: DataFormat,
    pub normalize_custom_elements: bool,
    /// Disable JS execution in the page before navigation (`Emulation.
    /// setScriptExecutionDisabled`). Renders static HTML only; much faster
    /// for content-only scraping but skips SPAs entirely.
    pub disable_javascript: bool,
    /// Capture a PDF render of the page via `Page.printToPDF`.
    pub pdf: bool,
    /// Emit a HAR 1.2 archive of the resources captured during navigation.
    pub har: bool,
    /// Capture a structured DOM + layout snapshot via
    /// `DOMSnapshot.captureSnapshot`. Includes per-node computed styles
    /// (small default set), layout rects, and text-box positions. Heavier
    /// than `outerHTML` but suitable for layout-aware downstream tasks
    /// (training data, visual diff, accessibility audits).
    pub save_dom_snapshot: bool,
    /// Collect Core Web Vitals (LCP / CLS / TBT / TTFB / long-task count).
    /// Installs `PerformanceObserver`s via `addScriptToEvaluateOnNewDocument`
    /// before navigation; reads accumulated values after `settle`.
    pub web_vitals: bool,
    /// Capture V8 heap + DOM counters + CPU time breakdown via
    /// `Performance.getMetrics` into `stat.metrics`. One CDP call,
    /// negligible overhead.
    pub metrics: bool,
    /// Extract page metadata (title / meta / OG / canonical / robots / ...)
    /// into `stat.metadata`. One extra `page.evaluate` call.
    pub metadata: bool,
    /// Identify render-blocking head resources (sync stylesheets, sync
    /// scripts). One extra `page.evaluate` call. Output goes to
    /// `stat.render_blocking_resources`.
    pub render_blocking: bool,
    /// Capture Service Worker registration state into
    /// `stat.service_worker` via a `page.evaluate` call.
    pub service_worker: bool,
    /// Subscribe to `Network.requestWillBeSent` and attach `initiator`
    /// info to each `resources[]` entry. Costs one extra event stream.
    pub initiators: bool,
    /// Subscribe to `Runtime.consoleAPICalled` and collect formatted
    /// `console.log/info/warn/error/debug` lines into
    /// `stat.console_messages`. Default off — console output is noisy,
    /// payloads can be large (objects get serialised), and most callers
    /// don't read it. When false, the stream is never subscribed (zero
    /// runtime cost).
    pub console_messages: bool,
    /// Audit per-`<img>` sizing: decoded vs display dimensions, lazy
    /// status, viewport overlap, alt presence, and (server-side joined)
    /// transferred bytes + waste ratio. Output: `stat.image_sizing`. One
    /// extra `page.evaluate` call — reads already-decoded browser state,
    /// no extra IO, <2ms for typical pages.
    pub image_sizing: bool,
    /// Install a pre-navigation `MutationObserver` and count DOM
    /// mutations (childList adds/removes, attribute changes) during the
    /// full page render. Output: `stat.dom_mutations`. Adds one
    /// `addScriptToEvaluateOnNewDocument` (pre-nav) + one `evaluate`
    /// (post-load) call. Typical overhead <5ms even on heavy SPAs.
    pub dom_mutations: bool,
    /// Include the full per-resource list (`stat.resources[]`) in the
    /// response. Default `false` — the resources are always **collected**
    /// internally (we need them for `resource_summary`, `total_size`,
    /// `image_sizing.transferred_bytes`, and HAR), but the per-entry
    /// array is dropped from the response unless explicitly requested.
    /// Default behaviour: functional-validation (load summary +
    /// `resource_summary` aggregates + scalar counts) without shipping
    /// dozens of detailed entries. Enable for forensic / per-request
    /// inspection.
    pub resources: bool,
    /// Emit a focused HTTP error rollup at `stat.http_errors`: failed_4xx
    /// / failed_5xx lists, network failures (DNS / TLS / connection
    /// refused — sourced from `Network.loadingFailed`), final URL after
    /// redirects, and redirect chain length. Costs one extra event
    /// subscription (`loadingFailed`); when off, that subscription is
    /// skipped entirely. Intended for periodic-health-check workflows
    /// where the caller needs a single "is this page broken / hijacked
    /// / redirected somewhere weird" signal without parsing
    /// `resources[]`.
    pub http_errors: bool,
    /// Capture CSS / JS coverage (Lighthouse "Reduce unused CSS / JS"
    /// feed) into `stat.coverage`. Enables CDP `Profiler` precise
    /// coverage + `CSS` rule-usage tracking pre-navigation. Costs:
    /// V8 disables some script optimisations while precise coverage is
    /// on, and the per-stylesheet rule-usage map keeps style-engine
    /// state for the full load. Small but real overhead — explicitly
    /// **not** enabled by `all_metrics=true`, so callers must opt in
    /// per request.
    pub coverage: bool,
    /// Audit the page's declared `<link rel="preconnect">` /
    /// `<link rel="dns-prefetch">` hints against the third-party hosts
    /// actually loaded. Populates `resource_summary.resource_hints`
    /// with the declared origins and a `gap` list of hot third-party
    /// hosts that were missed. One extra `page.evaluate` (~5ms) over
    /// `<head>`. OR-merged with `all_metrics`.
    pub resource_hints: bool,
    /// Audit `@font-face` declarations + `document.fonts` for FOIT
    /// risk (`font-display` distribution, missing-`swap` list,
    /// preload coverage). Populates `stat.font_audit`. One extra
    /// `page.evaluate` over CSSOM (~3–8ms). OR-merged with
    /// `all_metrics`. Cross-origin stylesheets without CORS are
    /// reported as `unreadable_stylesheets` rather than silently
    /// skipped — the audit is honest about its blind spots.
    pub font_audit: bool,
}

/// End-to-end browser-side orchestration for `/summary`:
/// 1. open `about:blank` with overrides applied
/// 2. set cookies + cache policy
/// 3. drive `collect_summary` (navigation + lifecycle + network + exceptions)
/// 4. optional post-load selector wait + settle delay
/// 5. format the `data` field (html / markdown / text, optionally scoped)
/// 6. close the page (chromium subprocess kept alive via the shared Browser)
pub async fn capture(
    browser: &Browser,
    default_user_agent: &str,
    req: SummaryRequest,
) -> Result<WebPageStat, Error> {
    // Fresh incognito context per request: cookies / cache / localStorage /
    // sessionStorage / serviceWorkers are scoped to this context and torn
    // down when we dispose it at the end of `capture`. Prevents cross-
    // request leakage of session state.
    //
    // Driven via raw CDP because chromiumoxide's `start_incognito_context`
    // requires `&mut Browser`, which the shared `Arc<Browser>` can't provide.
    let ctx_id = browser
        .execute(CreateBrowserContextParams::default())
        .await?
        .result
        .browser_context_id
        .clone();

    let target_params = CreateTargetParams::builder()
        .url("about:blank")
        .browser_context_id(ctx_id.clone())
        .build()
        .map_err(|e| Error::InvalidInput(format!("target params: {e}")))?;
    let page = browser.new_page(target_params).await?;

    // Stage 1: apply — all pre-navigation overrides + cookies + network rules.
    //
    // All 15 setters are **independent** (different CDP domains, no shared
    // state) and **idempotent within a fresh page**. We `try_join!` them so
    // chromiumoxide pipelines the CDP commands over the single underlying
    // WebSocket instead of paying one RTT per call serially. Total latency
    // for this stage drops from `sum(per-call RTT)` (~30-50ms with most
    // overrides set) to `max(per-call RTT)` (~5-10ms).
    //
    // Conditional setters (`cookies` / `disable_cache` / `web_vitals` /
    // `dom_mutations`) are wrapped in async blocks so they no-op when the
    // request didn't ask for them — same "skip CDP call when unused"
    // semantics as the original sequential version, just concurrent.
    //
    // `apply_block_resource_types` returns an `Option<oneshot::Sender<()>>`
    // RAII guard for the spawned Fetch drain task; it's captured by name
    // in the destructure pattern so its drop point stays at end of scope.
    // All `addScriptToEvaluateOnNewDocument` calls in this join still
    // complete **before** `page.goto()` in stage 2 starts, preserving the
    // "observer in place before initial render" guarantee.
    let t_apply = Instant::now();
    let (_, _, _, _, _, _, _, _, _, _, _resource_block_guard, _, _, _, _, _) = tokio::try_join!(
        apply_viewport(&page, req.width, req.height, req.device_scale_factor),
        apply_touch_emulation(&page, req.touch),
        apply_user_agent(
            &page,
            req.user_agent.as_deref(),
            req.accept_language.as_deref(),
            default_user_agent,
        ),
        apply_extra_headers(&page, &req.headers),
        apply_timezone(&page, req.timezone.as_deref()),
        apply_locale(&page, req.locale.as_deref()),
        apply_geolocation(&page, req.geolocation.as_ref()),
        async {
            if !req.cookies.is_empty() {
                set_cookies(&page, &req.cookies, &req.url).await
            } else {
                Ok(())
            }
        },
        async {
            if req.disable_cache {
                set_cache_disabled(&page, true).await
            } else {
                Ok(())
            }
        },
        apply_blocked_urls(&page, &req.block_urls),
        apply_block_resource_types(&page, &req.block_resource_types),
        apply_disable_javascript(&page, req.disable_javascript),
        apply_cpu_throttle(&page, req.cpu_throttle),
        // web_vitals + dom_mutations setup scripts merged into ONE
        // addScriptToEvaluateOnNewDocument call — saves a CDP RTT when
        // both are enabled. No-op if neither flag is set.
        apply_observers_setup(&page, req.web_vitals, req.dom_mutations),
        // Performance domain enable hoisted pre-navigation when metrics
        // requested — lets the later `collect_page_metrics` skip the
        // domain-enable RTT (saves ~3-5ms in the format stage).
        apply_performance_enable(&page, req.metrics),
        // Coverage: enables Profiler + DOM + CSS, starts precise
        // coverage + rule-usage tracking. Must be pre-navigation so
        // bytecode generated during script parsing is instrumented.
        apply_coverage_setup(&page, req.coverage),
    )?;
    tracing::debug!(
        stage = "apply",
        duration_ms = t_apply.elapsed().as_millis() as u64
    );

    // Stage 2: collect — navigate + drain lifecycle / network / exception events.
    //
    // `req.wait_until_load` is caller-supplied (no longer inferred from the
    // presence of other wait flags). `true` exits at onload + grace; `false`
    // waits for `networkIdle`. Pair with `settle` (stage 3) when late JS
    // needs to run before capture.
    let t_collect = Instant::now();
    let mut stat = collect_summary(
        &page,
        &req.url,
        req.timeout,
        &req.wait_for_request,
        req.wait_until_load,
        CollectCaptures {
            screenshot: req.screenshot,
            initiators: req.initiators,
            console: req.console_messages,
            http_errors: req.http_errors,
            coverage: req.coverage,
        },
    )
    .await?;
    tracing::debug!(
        stage = "collect",
        duration_ms = t_collect.elapsed().as_millis() as u64
    );

    // Stage 3: capture — post-load waits + user script (DOM mutation phase).
    let t_capture = Instant::now();
    if let Some(selector) = req.wait_for_element.as_deref() {
        wait_for_selector(&page, selector, req.timeout).await?;
    }
    if let Some(expression) = req.wait_for_function.as_deref() {
        wait_for_function(&page, expression, req.timeout).await?;
    }
    if let Some(settle) = req.settle {
        tokio::time::sleep(settle).await;
    }
    // Run user-provided script after all waits settle but before data
    // capture, so DOM mutations (modal removal, lazy-load triggers, etc.)
    // are reflected in the final `stat.data`.
    if let Some(script) = req.script.as_deref() {
        page.evaluate(script).await?;
    }
    tracing::debug!(
        stage = "capture",
        duration_ms = t_capture.elapsed().as_millis() as u64
    );

    // Stage 4: format — extract data + optional PDF / HAR / DOM snapshot.
    //
    // Two-phase design:
    //   Phase A — concurrent CDP / JS reads (all read-only of page state).
    //     Each arm is independent: data extraction, PDF print, DOM
    //     snapshot, and every observer-accumulator drain operate on
    //     different DOM subtrees or `window.*` globals. chromiumoxide
    //     pipelines the CDP commands over the single underlying socket
    //     so total latency drops from sum-of-RTTs (~50-80ms when most
    //     features on) to max-of-RTTs (~10-15ms). For AI-comparison
    //     mode (7+ features at once) this is the largest single-stage
    //     win in the request.
    //   Phase B — server-side derives + assignment.
    //     `enrich_image_sizing`, `build_har`, `build_resource_summary`
    //     all consume `stat.resources` (collected in stage 2). They are
    //     pure functions of already-collected state, no extra IO.
    //
    // The user `script` (stage 3) already ran and may have mutated the
    // DOM, so all reads here see the post-script DOM — including the
    // re-extracted `stat.data`.
    let t_format = Instant::now();
    let capture_sel = req.capture_element.as_deref();

    // Phase A — parallel reads. Each arm returns its own `Result<T, Error>`.
    // Conditional features no-op (return `None`) when not requested,
    // preserving the "skip the CDP call entirely" optimisation.
    let (
        data,
        pdf_data,
        dom_snapshot,
        web_vitals,
        dom_mutations,
        metrics,
        metadata,
        render_blocking,
        service_worker,
        image_sizing,
        resource_hints_raw,
        font_audit,
    ) = tokio::try_join!(
        // data — html / text / markdown extraction, scoped to capture_element.
        async {
            match req.data_format {
                DataFormat::Html => {
                    if let Some(sel) = capture_sel {
                        capture_property(&page, sel, "outerHTML", req.timeout).await
                    } else {
                        page.content().await.map_err(Error::from)
                    }
                }
                DataFormat::Text => extract_text(&page, capture_sel, req.timeout).await,
                DataFormat::Markdown => {
                    let source = if req.normalize_custom_elements {
                        normalize_dom(&page, capture_sel, req.timeout).await?
                    } else if let Some(sel) = capture_sel {
                        capture_property(&page, sel, "outerHTML", req.timeout).await?
                    } else {
                        page.content().await?
                    };
                    let converter = HtmlToMarkdown::builder()
                        .skip_tags(vec!["img", "script", "style", "svg", "iframe", "noscript"])
                        .build();
                    converter
                        .convert(&source)
                        .map_err(|e| Error::InvalidInput(format!("markdown convert: {e}")))
                }
            }
        },
        // PDF — `Page.printToPDF`. Independent of DOM reads.
        async {
            if req.pdf {
                let resp = page.execute(PrintToPdfParams::default()).await?;
                Ok(Some(Pdf {
                    data: resp.result.data.clone().into(),
                    mime_type: "application/pdf".to_string(),
                }))
            } else {
                Ok(None)
            }
        },
        // DOM snapshot — heavy CDP call but pure read.
        async {
            if req.save_dom_snapshot {
                // Small useful default set of computed styles; expand if
                // downstream training needs more (font-weight, line-height).
                let params = CaptureSnapshotParams::builder()
                    .computed_styles(vec![
                        "display".to_string(),
                        "position".to_string(),
                        "color".to_string(),
                        "background-color".to_string(),
                        "font-size".to_string(),
                        "visibility".to_string(),
                    ])
                    .include_dom_rects(true)
                    .build()
                    .map_err(|e| Error::InvalidInput(format!("snapshot params: {e}")))?;
                let resp = page.execute(params).await?;
                Ok(Some(serde_json::to_value(&resp.result).map_err(|e| {
                    Error::Cdp(format!("dom snapshot serialize: {e}"))
                })?))
            } else {
                Ok(None)
            }
        },
        // Web Vitals — drain the pre-navigation observer accumulator. By
        // now observers have had stages 2 + 3 to fill up. Inline decode
        // peels off the raw `loaf_entries` and `long_task_entries`
        // (private to this layer) before deserialising into the public
        // `WebVitals` shape.
        async {
            if req.web_vitals {
                let eval = page.evaluate(WEB_VITALS_READ_JS).await?;
                let mut value: serde_json::Value = eval
                    .into_value()
                    .map_err(|e| Error::Cdp(format!("web vitals decode: {e}")))?;
                let loaf_raw: Vec<LoafRawEntry> = value
                    .get_mut("loaf_entries")
                    .and_then(|v| serde_json::from_value(v.take()).ok())
                    .unwrap_or_default();
                let longtask_raw: Vec<LongTaskRawEntry> = value
                    .get_mut("long_task_entries")
                    .and_then(|v| serde_json::from_value(v.take()).ok())
                    .unwrap_or_default();
                let mut vitals: WebVitals = serde_json::from_value(value)
                    .map_err(|e| Error::Cdp(format!("web vitals decode: {e}")))?;
                aggregate_cls_sources(&mut vitals);
                aggregate_loaf(&mut vitals, &loaf_raw);
                aggregate_long_tasks(&mut vitals, &longtask_raw);
                Ok(Some(vitals))
            } else {
                Ok(None)
            }
        },
        // DOM mutations — drain the MutationObserver counters.
        async {
            if req.dom_mutations {
                collect_dom_mutations(&page).await
            } else {
                Ok(None)
            }
        },
        // Page metrics — CDP `Performance.getMetrics`.
        async {
            if req.metrics {
                collect_page_metrics(&page).await.map(Some)
            } else {
                Ok(None)
            }
        },
        // Page metadata — `<head>` walker JS.
        async {
            if req.metadata {
                collect_page_metadata(&page).await.map(Some)
            } else {
                Ok(None)
            }
        },
        // Render-blocking head resources — `<head>` scan JS.
        async {
            if req.render_blocking {
                collect_render_blocking(&page).await.map(Some)
            } else {
                Ok(None)
            }
        },
        // Service Worker registration state — `navigator.serviceWorker` read.
        async {
            if req.service_worker {
                collect_service_worker(&page).await.map(Some)
            } else {
                Ok(None)
            }
        },
        // Image sizing raw — browser-side collection only. The follow-up
        // `enrich_image_sizing` (joins with stat.resources) runs in
        // Phase B because it needs the server-collected resource list.
        async {
            if req.image_sizing {
                collect_image_sizing(&page).await.map(Some)
            } else {
                Ok(None)
            }
        },
        // Resource-hint scrape — small `<head>` query for declared
        // preconnect / dns-prefetch links. The gap derive against
        // `top_third_party_domains` runs in Phase B (needs the
        // server-derived third-party ranking from `build_resource_summary`).
        async {
            if req.resource_hints {
                collect_resource_hints(&page).await.map(Some)
            } else {
                Ok(None)
            }
        },
        // Font audit — walks CSSOM for `@font-face` rules + reads
        // `document.fonts`. Self-contained (no Phase B derive needed).
        async {
            if req.font_audit {
                collect_font_audit(&page).await.map(Some)
            } else {
                Ok(None)
            }
        },
    )?;

    // Phase B — assignment + pure server-side derives. All operate on
    // already-collected `stat` state; no extra browser IO. Ordered so
    // `enrich_image_sizing` / `build_har` / `build_resource_summary`
    // all see the fully populated `stat.resources` before any clearing.
    stat.data = data;
    stat.pdf = pdf_data;
    stat.dom_snapshot = dom_snapshot;
    stat.web_vitals = web_vitals;
    stat.dom_mutations = dom_mutations;
    stat.metrics = metrics;
    stat.metadata = metadata;
    stat.render_blocking_resources = render_blocking;
    stat.service_worker = service_worker;
    stat.font_audit = font_audit;
    if let Some(mut imgs) = image_sizing {
        // Server-side enrichment: join transferred_bytes from resources by
        // URL (currentSrc is the actual fetched URL), compute waste_ratio,
        // then sort worst-waste-first so the top of the list is actionable.
        // Same pass derives the Lighthouse "image four-pack" audit
        // (oversized / missing_dimensions / missing_lazy / missing_srcset).
        let audit = enrich_image_sizing(&mut imgs, &stat.resources);
        stat.image_sizing = Some(imgs);
        stat.image_audit = Some(audit);
    }

    if req.har {
        stat.har = Some(build_har(&stat, &req.url));
    }

    // Always compute — free derive from already-collected `resources`.
    stat.resource_summary = build_resource_summary(&stat.resources, &req.url);

    // Resource-hint audit Phase B: combine the raw `<head>` scrape
    // (from Phase A) with the now-built `top_third_party_domains`
    // ranking to compute the gap list. `None` when `resource_hints`
    // wasn't requested — Phase A skipped the evaluate entirely.
    if let Some(raw) = resource_hints_raw {
        stat.resource_summary.resource_hints = Some(build_resource_hints(
            raw,
            &stat.resource_summary.top_third_party_domains,
        ));
    }

    // Drop the detailed array unless explicitly requested. `resource_count`
    // and `total_size` (scalars) plus `resource_summary` (aggregates) are
    // preserved for functional-validation use. Must happen AFTER every
    // downstream consumer (HAR build, image_sizing enrichment,
    // resource_summary derive) has run.
    if !req.resources {
        stat.resources.clear();
    }

    tracing::debug!(
        stage = "format",
        duration_ms = t_format.elapsed().as_millis() as u64
    );

    let _ = page.close().await;
    // Best-effort dispose — if it fails, the browser's context GC eventually
    // collects the orphaned context. Errors here shouldn't fail the request.
    let _ = browser
        .execute(DisposeBrowserContextParams::new(ctx_id))
        .await;
    Ok(stat)
}

fn map_timing(t: &CdpResourceTiming) -> ResourceTiming {
    ResourceTiming {
        request_time: t.request_time,
        dns_start: t.dns_start,
        dns_end: t.dns_end,
        connect_start: t.connect_start,
        connect_end: t.connect_end,
        ssl_start: t.ssl_start,
        ssl_end: t.ssl_end,
        send_start: t.send_start,
        send_end: t.send_end,
        receive_headers_end: t.receive_headers_end,
    }
}

/// Best-effort HAR 1.2 archive from the data we collected. Several fields
/// are placeholders / -1 because we don't capture `Network.requestWillBeSent`
/// payloads (request method, headers, post body, full per-phase wire bytes).
/// Still imports cleanly into Chrome DevTools "Import HAR" / Wireshark /
/// `har-viewer` for resource-list visualization.
fn build_har(stat: &WebPageStat, url: &str) -> serde_json::Value {
    use serde_json::json;

    let entries: Vec<serde_json::Value> = stat
        .resources
        .iter()
        .map(|r| {
            let dns = r
                .timing
                .as_ref()
                .filter(|t| t.dns_start >= 0.0 && t.dns_end >= 0.0)
                .map(|t| t.dns_end - t.dns_start)
                .unwrap_or(-1.0);
            let connect = r
                .timing
                .as_ref()
                .filter(|t| t.connect_start >= 0.0 && t.connect_end >= 0.0)
                .map(|t| t.connect_end - t.connect_start)
                .unwrap_or(-1.0);
            let ssl = r
                .timing
                .as_ref()
                .filter(|t| t.ssl_start >= 0.0 && t.ssl_end >= 0.0)
                .map(|t| t.ssl_end - t.ssl_start)
                .unwrap_or(-1.0);
            let send = r
                .timing
                .as_ref()
                .map(|t| (t.send_end - t.send_start).max(0.0))
                .unwrap_or(-1.0);
            let wait = r
                .timing
                .as_ref()
                .map(|t| (t.receive_headers_end - t.send_end).max(0.0))
                .unwrap_or(-1.0);

            json!({
                "pageref": "page_1",
                "startedDateTime": "1970-01-01T00:00:00.000Z",
                "time": -1,
                "request": {
                    "method": "GET",
                    "url": r.url,
                    "httpVersion": "HTTP/1.1",
                    "cookies": [],
                    "headers": [],
                    "queryString": [],
                    "headersSize": -1,
                    "bodySize": -1,
                },
                "response": {
                    "status": r.status,
                    "statusText": "",
                    "httpVersion": "HTTP/1.1",
                    "cookies": [],
                    "headers": [],
                    "content": {
                        "size": r.content_size,
                        "mimeType": r.mime_type,
                    },
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": r.content_size,
                },
                "cache": if r.from_cache { json!({ "afterRequest": {} }) } else { json!({}) },
                "timings": {
                    "blocked": -1,
                    "dns": dns,
                    "connect": connect,
                    "ssl": ssl,
                    "send": send,
                    "wait": wait,
                    "receive": -1,
                },
            })
        })
        .collect();

    json!({
        "log": {
            "version": "1.2",
            "creator": {
                "name": "browser-headless",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "pages": [{
                "startedDateTime": "1970-01-01T00:00:00.000Z",
                "id": "page_1",
                "title": url,
                "pageTimings": {
                    "onContentLoad": stat.dcl_time,
                    "onLoad": stat.load_time,
                },
            }],
            "entries": entries,
        }
    })
}

fn format_console(ev: &EventConsoleApiCalled) -> String {
    let level = format!("{:?}", ev.r#type).to_lowercase();
    let parts: Vec<String> = ev
        .args
        .iter()
        .map(|a| {
            a.value
                .as_ref()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .or_else(|| a.description.clone())
                .unwrap_or_else(|| format!("[{:?}]", a.r#type))
        })
        .collect();
    format!("[{}] {}", level, parts.join(" "))
}

fn map_cookie(c: &CdpCookie) -> Cookie {
    Cookie {
        name: c.name.clone(),
        value: c.value.clone(),
        domain: c.domain.clone(),
        path: c.path.clone(),
        expires: c.expires,
        http_only: c.http_only,
        secure: c.secure,
        same_site: c
            .same_site
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok())
            .and_then(|v| v.as_str().map(String::from)),
    }
}

/// Compute `(used_bytes, total_bytes)` for a single script coverage
/// payload using the "innermost wins" sweep. A byte is "used" iff its
/// smallest enclosing range has `count > 0`. This matches what
/// puppeteer / playwright report as Lighthouse coverage.
///
/// Algorithm:
///
/// Steps: (1) flatten every function's ranges into a single list and
/// generate open/close events; (2) sort by offset asc — at the same
/// offset, closes happen before opens (adjacent ranges work
/// correctly); same offset + same type → longer opens first / shorter
/// closes first (parent contains child); (3) sweep, keeping a stack
/// of active range counts. The top of stack is the innermost active
/// range — its `> 0` / `== 0` count decides whether the current span
/// is used.
///
/// `total` is taken as `max(end_offset)` across all ranges (the
/// outermost script range almost always extends to script length).
fn compute_js_coverage(script: &ScriptCoverage) -> (u64, u64) {
    // (offset, ty, length, count). ty=0 open, ty=1 close.
    let mut points: Vec<(i64, u8, i64, i64)> = Vec::new();
    for f in &script.functions {
        for r in &f.ranges {
            let len = r.end_offset - r.start_offset;
            if len <= 0 {
                continue;
            }
            points.push((r.start_offset, 0, len, r.count));
            points.push((r.end_offset, 1, len, r.count));
        }
    }
    if points.is_empty() {
        return (0, 0);
    }
    let total = points
        .iter()
        .filter(|p| p.1 == 1)
        .map(|p| p.0)
        .max()
        .unwrap_or(0)
        .max(0) as u64;

    points.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.cmp(&a.1)) // close (1) before open (0) at same offset
            .then_with(|| {
                if a.1 == 0 {
                    b.2.cmp(&a.2) // opens: longer first (outer before inner)
                } else {
                    a.2.cmp(&b.2) // closes: shorter first (inner before outer)
                }
            })
    });

    let mut stack: Vec<i64> = Vec::new();
    let mut used: u64 = 0;
    let mut last_offset: i64 = points[0].0;
    for (offset, ty, _len, count) in points {
        if !stack.is_empty() && offset > last_offset && stack.last().copied().unwrap_or(0) > 0 {
            used += (offset - last_offset) as u64;
        }
        last_offset = offset;
        if ty == 0 {
            stack.push(count);
        } else {
            // Pop the matching count; ranges are well-formed so the
            // top of stack should equal `count` here. Pop unconditionally
            // — if backend produced mis-nested ranges we'd rather
            // under-count than panic.
            stack.pop();
        }
    }

    (used, total)
}

/// Compute per-stylesheet `(used_bytes, total_bytes)` from a slice of
/// `RuleUsage` entries that all share the same `style_sheet_id`. CSS
/// rule usage is non-overlapping (each rule is a top-level CSS rule
/// inside the stylesheet), so we just sum the `used: true` lengths;
/// total comes from the stylesheet header.
fn compute_css_coverage(rules: &[&CssRuleUsage], total_bytes: u64) -> (u64, u64) {
    let mut used: u64 = 0;
    for r in rules {
        if !r.used {
            continue;
        }
        let len = (r.end_offset - r.start_offset).max(0.0) as u64;
        used += len;
    }
    // Clamp — defensive. If rules overlapped or extended past total,
    // cap so the ratio stays sane.
    let used = used.min(total_bytes);
    (used, total_bytes)
}

fn format_exception(ev: &EventExceptionThrown) -> String {
    let d = &ev.exception_details;
    let extra = d
        .exception
        .as_ref()
        .and_then(|e| e.description.as_ref())
        .cloned()
        .unwrap_or_default();
    if extra.is_empty() {
        format!("{}:{} {}", d.line_number, d.column_number, d.text)
    } else {
        format!(
            "{}:{} {} | {}",
            d.line_number, d.column_number, d.text, extra
        )
    }
}

/// Classify an uncaught exception by its error class for the
/// `js_exceptions.by_name` rollup. Resolution order:
///
///   1. `RemoteObject.className` — set when JS code threw an `Error`
///      subclass (built-in or user-defined). This is the cleanest signal.
///   2. Parse `"Foo: bar baz"` prefix from `description` — covers cases
///      where CDP omitted `className` but the description still leads
///      with the class name (common for re-thrown errors and some hosts).
///   3. `"Other"` — `throw "string"` / `throw 42` / unparseable.
fn classify_exception(ev: &EventExceptionThrown) -> String {
    if let Some(exc) = ev.exception_details.exception.as_ref() {
        if let Some(class) = exc.class_name.as_ref().filter(|s| !s.is_empty()) {
            return class.clone();
        }
        if let Some(desc) = exc.description.as_ref()
            && let Some((head, _)) = desc.split_once(':')
        {
            let head = head.trim();
            // Filter pathological cases: descriptions like
            // "http://example.com: ..." would otherwise classify as
            // "http". Require ascii-letter start + no whitespace.
            if !head.is_empty()
                && head.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && !head.chars().any(char::is_whitespace)
            {
                return head.to_string();
            }
        }
    }
    "Other".to_string()
}

/// First-line message text for the `sample_message` field — prefers the
/// remote-object description, falls back to `ExceptionDetails.text`, and
/// trims to 200 chars so a stack trace doesn't blow up the payload.
fn exception_sample_message(ev: &EventExceptionThrown) -> Option<String> {
    const MAX: usize = 200;
    let raw = ev
        .exception_details
        .exception
        .as_ref()
        .and_then(|e| e.description.as_ref())
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&ev.exception_details.text);
    if raw.is_empty() {
        return None;
    }
    let first_line = raw.lines().next().unwrap_or(raw);
    let trimmed: String = first_line.chars().take(MAX).collect();
    Some(trimmed)
}
