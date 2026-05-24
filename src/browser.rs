//! Browser orchestration layer. Wraps chromiumoxide CDP calls in an
//! HTTP-agnostic API with a dedicated `Error` enum that callers can map onto
//! their own response types.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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
    ErrorReason, EventLoadingFinished, EventResponseReceived, GetCookiesParams, Headers,
    ResourceTiming as CdpResourceTiming, ResourceType, SetBlockedUrLsParams,
    SetCacheDisabledParams, SetCookiesParams, SetExtraHttpHeadersParams,
    SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotParams,
    EnableParams as PageEnableParams, EventLifecycleEvent, PrintToPdfParams,
    SetLifecycleEventsEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams, DisposeBrowserContextParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    EnableParams as RuntimeEnableParams, EventConsoleApiCalled, EventExceptionThrown,
};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
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

/// Launch Chromium, spawn the watcher task that keeps the CDP connection
/// alive, and return the browser handle, its default user-agent string,
/// and a `oneshot::Receiver` that fires once when the CDP stream ends
/// (chromium subprocess died, websocket dropped, etc.). The supervisor
/// uses that signal to respawn — see `main::supervise_browser`.
pub async fn launch() -> Result<(Browser, String, tokio::sync::oneshot::Receiver<()>), Error> {
    // --no-sandbox: required when running as non-root inside a container
    //   without user-namespace mapping (the default Docker config).
    // --disable-dev-shm-usage: containers ship a 64MB /dev/shm by default,
    //   which Chrome fills up under load and then crashes; switch to /tmp.
    // Safe defaults for an internal scraping service. Remove them if
    // exposing this to untrusted URLs in a multi-tenant context.
    let config = BrowserConfig::builder()
        .arg("--no-sandbox")
        .arg("--disable-dev-shm-usage")
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
                    tracing::debug!("browser handler error: {e}");
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
    Ok((browser, version.user_agent, notify_rx))
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
                            tracing::debug!("fetch handler error: {e}");
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
  window.__web_vitals = { lcp: 0, cls: 0, tbt: 0, ttfb: 0, long_tasks: 0 };
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (e.startTime > window.__web_vitals.lcp) window.__web_vitals.lcp = e.startTime;
      }
    }).observe({ type: 'largest-contentful-paint', buffered: true });
  } catch (e) {}
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (!e.hadRecentInput) window.__web_vitals.cls += e.value;
      }
    }).observe({ type: 'layout-shift', buffered: true });
  } catch (e) {}
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (e.duration > 50) window.__web_vitals.tbt += e.duration - 50;
        window.__web_vitals.long_tasks++;
      }
    }).observe({ type: 'longtask', buffered: true });
  } catch (e) {}
})();
"#;

/// Read accumulated `window.__web_vitals` and enrich with TTFB from
/// Navigation Timing API.
const WEB_VITALS_READ_JS: &str = r#"
(function() {
  const v = window.__web_vitals || { lcp: 0, cls: 0, tbt: 0, ttfb: 0, long_tasks: 0 };
  const nav = performance.getEntriesByType('navigation')[0];
  if (nav) v.ttfb = nav.responseStart;
  return v;
})()
"#;

/// Install the Web Vitals collection script (runs on every new document).
/// Must be called before navigation so observers are in place for the
/// initial paint / shift / longtask entries.
pub async fn apply_web_vitals_setup(page: &Page) -> Result<(), Error> {
    page.execute(AddScriptToEvaluateOnNewDocumentParams::new(
        WEB_VITALS_SETUP_JS,
    ))
    .await?;
    Ok(())
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
    if user_agent.is_none() && accept_language.is_none() {
        return Ok(());
    }
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
    pub fcp_time: u32,
    pub dcl_time: u32,
    pub load_time: u32,
    /// Page content. `collect_summary` populates this with raw HTML from
    /// `page.content()`; callers may overwrite with text/markdown afterwards.
    pub data: String,
    pub exceptions: Vec<String>,
    /// `console.log/info/warn/error/debug` calls observed during the page
    /// lifecycle, formatted as `[<level>] <args>`.
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
    /// True if the response came from disk cache, service worker, or prefetch
    /// cache. Cache hits typically have `content_size = 0` and many `timing`
    /// fields = -1 (skipped phases).
    pub from_cache: bool,
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

#[derive(Debug, Clone, Serialize)]
pub struct Screenshot {
    /// Base64-encoded PNG bytes (as returned by CDP `Page.captureScreenshot`).
    pub data: String,
    pub mime_type: String,
}

/// Navigate to `url` and collect a full page summary: lifecycle timings,
/// per-resource network stats, JS exceptions, final HTML, and optionally a
/// screenshot. Drives Page / Network / Runtime CDP domains in parallel and
/// returns once the `load` lifecycle event fires (or `timeout` elapses,
/// returning a best-effort partial snapshot).
pub async fn collect_summary(
    page: &Page,
    url: &str,
    timeout: Duration,
    capture_screenshot: bool,
    wait_for_request: &[String],
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
    let mut console_stream = page.event_listener::<EventConsoleApiCalled>().await?;

    page.goto(url).await?;

    let mut resources: HashMap<String, WebPageResource> = HashMap::new();
    let mut exceptions: Vec<String> = Vec::new();
    let mut console_messages: Vec<String> = Vec::new();
    let mut init_ts: Option<f64> = None;
    let mut fcp_ts: Option<f64> = None;
    let mut dcl_ts: Option<f64> = None;
    let mut load_ts: Option<f64> = None;

    // Exit policy:
    // - `load` alone is unsafe to break on: select may have skipped already-
    //   queued response events; breaking immediately drops them.
    // - Wait for `networkIdle` lifecycle (Chrome emits this after ≥500ms with
    //   zero in-flight requests, i.e. all responses have already fired).
    // - After networkIdle fires, give a 500ms grace window so the select loop
    //   can drain any response events still sitting in their channels.
    // - Soft cap: `timeout` (the page never settles → return best-effort).
    const POST_IDLE_GRACE: Duration = Duration::from_millis(500);
    let deadline = Instant::now() + timeout;
    let mut idle_at: Option<Instant> = None;
    let mut pending_patterns: Vec<&str> = wait_for_request.iter().map(String::as_str).collect();
    let total_patterns = pending_patterns.len();

    loop {
        // Grace period only kicks in once BOTH networkIdle has fired AND all
        // wait_for_request patterns have been matched. If patterns are still
        // pending after idle, keep waiting (up to timeout) for them.
        let ready_to_finish = idle_at.is_some() && pending_patterns.is_empty();
        let stop_at = match (ready_to_finish, idle_at) {
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
                    "collect_summary stopping (timeout={}ms idle={:?})",
                    timeout.as_millis(),
                    idle_at.is_some(),
                );
                break;
            }
            Some(ev) = lifecycle_stream.next() => {
                let ts = *ev.timestamp.inner();
                match ev.name.as_str() {
                    "init" => { init_ts.get_or_insert(ts); }
                    "firstContentfulPaint" => { fcp_ts.get_or_insert(ts); }
                    "DOMContentLoaded" => { dcl_ts.get_or_insert(ts); }
                    "load" => { load_ts.get_or_insert(ts); }
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
                        "wait_for_request matched {} ({}/{} remaining)",
                        matched,
                        pending_patterns.len(),
                        total_patterns,
                    );
                }

                let id = ev.request_id.inner().clone();
                let entry = resources.entry(id.clone()).or_insert_with(|| WebPageResource {
                    request_id: id.clone(),
                    ..Default::default()
                });
                entry.url = url;
                entry.status = status as u32;
                entry.mime_type = ev.response.mime_type.clone();
                entry.connection_reused = ev.response.connection_reused;
                entry.timing = ev.response.timing.as_ref().map(map_timing);
                entry.from_cache = ev.response.from_disk_cache.unwrap_or(false)
                    || ev.response.from_service_worker.unwrap_or(false)
                    || ev.response.from_prefetch_cache.unwrap_or(false);
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
            }
            Some(ev) = console_stream.next() => {
                console_messages.push(format_console(&ev));
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
    let screenshot = if capture_screenshot {
        let resp = page.execute(CaptureScreenshotParams::default()).await?;
        Some(Screenshot {
            data: resp.result.data.clone().into(),
            mime_type: "image/png".to_string(),
        })
    } else {
        None
    };

    let total_size: u64 = resources.values().map(|r| r.content_size).sum();
    let resources_vec: Vec<WebPageResource> = resources.into_values().collect();

    Ok(WebPageStat {
        total_size,
        fcp_time: to_ms(fcp_ts),
        dcl_time: to_ms(dcl_ts),
        load_time: to_ms(load_ts),
        data: String::new(),
        exceptions,
        console_messages,
        resources: resources_vec,
        cookies,
        screenshot,
        pdf: None,
        har: None,
        dom_snapshot: None,
        web_vitals: None,
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
    pub fn to_markdown(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();

        let _ = writeln!(s, "# Page Summary");
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "Load completed in **{}ms** (FCP {}ms, DCL {}ms). Transferred **{}** across **{}** resources.",
            self.load_time,
            self.fcp_time,
            self.dcl_time,
            format_bytes(self.total_size),
            self.resources.len(),
        );
        let _ = writeln!(s);

        if !self.exceptions.is_empty() {
            let _ = writeln!(s, "## JavaScript Exceptions ({})", self.exceptions.len());
            let _ = writeln!(s);
            for ex in &self.exceptions {
                let _ = writeln!(s, "- {ex}");
            }
            let _ = writeln!(s);
        }

        if !self.console_messages.is_empty() {
            let _ = writeln!(s, "## Console Messages ({})", self.console_messages.len());
            let _ = writeln!(s);
            for msg in &self.console_messages {
                let _ = writeln!(s, "- {msg}");
            }
            let _ = writeln!(s);
        }

        if !self.cookies.is_empty() {
            // Name + domain only — values may contain session tokens. Use the
            // JSON response if you need the actual values.
            let _ = writeln!(s, "## Cookies ({})", self.cookies.len());
            let _ = writeln!(s);
            for c in &self.cookies {
                let _ = writeln!(s, "- `{}` on `{}`", c.name, c.domain);
            }
            let _ = writeln!(s);
        }

        if !self.resources.is_empty() {
            let _ = writeln!(s, "## Resources ({})", self.resources.len());
            let _ = writeln!(s);
            for r in &self.resources {
                let _ = writeln!(s, "- {}", describe_resource(r));
            }
            let _ = writeln!(s);
        }

        if self.screenshot.is_some() {
            let _ = writeln!(s, "## Screenshot");
            let _ = writeln!(s);
            let _ = writeln!(s, "Base64 PNG captured (omitted from markdown body).");
            let _ = writeln!(s);
        }

        if self.pdf.is_some() {
            let _ = writeln!(s, "## PDF");
            let _ = writeln!(s);
            let _ = writeln!(s, "Base64 PDF captured (omitted from markdown body).");
            let _ = writeln!(s);
        }

        if let Some(har) = &self.har {
            let entries = har
                .get("log")
                .and_then(|l| l.get("entries"))
                .and_then(|e| e.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let _ = writeln!(s, "## HAR");
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "HAR 1.2 archive included ({entries} entries; omitted from markdown body)."
            );
            let _ = writeln!(s);
        }

        if let Some(v) = &self.web_vitals {
            let _ = writeln!(s, "## Web Vitals");
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- LCP **{:.0}ms** · CLS **{:.3}** · TBT **{:.0}ms** · TTFB **{:.0}ms** · long tasks **{}**",
                v.lcp, v.cls, v.tbt, v.ttfb, v.long_tasks
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
            let _ = writeln!(s, "## DOM Snapshot");
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "DOMSnapshot included ({docs} document(s), {strings} interned strings; omitted from markdown body)."
            );
            let _ = writeln!(s);
        }

        let _ = writeln!(s, "## Page Content");
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

fn describe_resource(r: &WebPageResource) -> String {
    let mime = if r.mime_type.is_empty() {
        "unknown type"
    } else {
        r.mime_type.as_str()
    };

    if r.from_cache {
        return format!(
            "Served `{}` from browser cache ({}, status {}).",
            r.url, mime, r.status,
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
        ", connection reused"
    } else {
        ""
    };

    match r.status {
        200..=299 => format!(
            "Loaded `{}` as {} ({}, status {}{}{}).",
            r.url, mime, size, r.status, ttfb, conn,
        ),
        300..=399 => format!("Redirected (status {}) from `{}`{}.", r.status, r.url, ttfb,),
        400..=499 => format!(
            "Client error fetching `{}` (status {}, {}).",
            r.url, r.status, mime,
        ),
        500..=599 => format!(
            "Server error fetching `{}` (status {}, {}).",
            r.url, r.status, mime,
        ),
        _ => format!(
            "Fetched `{}` with status {} ({}, {}).",
            r.url, r.status, mime, size,
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
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DataFormat {
    #[default]
    Html,
    Markdown,
    /// Computed-style-aware DOM walker, inserts newlines between block-like
    /// elements. Better than `innerText` for flex/grid pages.
    Text,
}

/// All knobs `capture` accepts. Owned fields so callers don't juggle
/// lifetimes; the struct is consumed by `capture`.
pub struct SummaryRequest {
    pub url: String,
    pub timeout: Duration,
    pub screenshot: bool,
    pub wait_for_request: Vec<String>,
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
    let t_apply = Instant::now();
    apply_viewport(&page, req.width, req.height, req.device_scale_factor).await?;
    apply_touch_emulation(&page, req.touch).await?;
    apply_user_agent(
        &page,
        req.user_agent.as_deref(),
        req.accept_language.as_deref(),
        default_user_agent,
    )
    .await?;
    apply_extra_headers(&page, &req.headers).await?;
    apply_timezone(&page, req.timezone.as_deref()).await?;
    apply_locale(&page, req.locale.as_deref()).await?;
    apply_geolocation(&page, req.geolocation.as_ref()).await?;

    if !req.cookies.is_empty() {
        set_cookies(&page, &req.cookies, &req.url).await?;
    }
    if req.disable_cache {
        set_cache_disabled(&page, true).await?;
    }
    apply_blocked_urls(&page, &req.block_urls).await?;
    // Held until `capture` returns. Dropping the Sender wakes the spawned
    // Fetch drain task so it exits cleanly when the page is gone.
    let _resource_block_guard =
        apply_block_resource_types(&page, &req.block_resource_types).await?;
    apply_disable_javascript(&page, req.disable_javascript).await?;
    apply_cpu_throttle(&page, req.cpu_throttle).await?;
    if req.web_vitals {
        apply_web_vitals_setup(&page).await?;
    }
    tracing::debug!(
        stage = "apply",
        duration_ms = t_apply.elapsed().as_millis() as u64
    );

    // Stage 2: collect — navigate + drain lifecycle / network / exception events.
    let t_collect = Instant::now();
    let mut stat = collect_summary(
        &page,
        &req.url,
        req.timeout,
        req.screenshot,
        &req.wait_for_request,
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
    let t_format = Instant::now();

    // Format dispatch for `stat.data`, scoped to capture_element if provided.
    // Always populates from a live read here (the optional script above may
    // have changed the DOM since collect_summary).
    let capture_sel = req.capture_element.as_deref();
    match req.data_format {
        DataFormat::Html => {
            stat.data = if let Some(sel) = capture_sel {
                capture_property(&page, sel, "outerHTML", req.timeout).await?
            } else {
                page.content().await?
            };
        }
        DataFormat::Text => {
            stat.data = extract_text(&page, capture_sel, req.timeout).await?;
        }
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
            stat.data = converter
                .convert(&source)
                .map_err(|e| Error::InvalidInput(format!("markdown convert: {e}")))?;
        }
    }

    if req.pdf {
        let resp = page.execute(PrintToPdfParams::default()).await?;
        stat.pdf = Some(Pdf {
            data: resp.result.data.clone().into(),
            mime_type: "application/pdf".to_string(),
        });
    }

    if req.har {
        stat.har = Some(build_har(&stat, &req.url));
    }

    if req.save_dom_snapshot {
        // Small useful default set of computed styles; expand if downstream
        // training needs more (font-weight, line-height, etc.).
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
        stat.dom_snapshot = Some(
            serde_json::to_value(&resp.result)
                .map_err(|e| Error::Cdp(format!("dom snapshot serialize: {e}")))?,
        );
    }

    if req.web_vitals {
        // Read the accumulator the pre-navigation observer populated. Safe
        // to read here — observers have had `collect` + `capture` stages
        // worth of time to accumulate entries.
        let eval = page.evaluate(WEB_VITALS_READ_JS).await?;
        let vitals: WebVitals = eval
            .into_value()
            .map_err(|e| Error::Cdp(format!("web vitals decode: {e}")))?;
        stat.web_vitals = Some(vitals);
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
