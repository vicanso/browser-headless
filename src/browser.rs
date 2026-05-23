//! Browser orchestration layer. Wraps chromiumoxide CDP calls in an
//! HTTP-agnostic API with a dedicated `Error` enum that callers can map onto
//! their own response types.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, EnableParams as NetworkEnableParams, EventLoadingFinished, EventResponseReceived,
    ResourceTiming as CdpResourceTiming, SetCacheDisabledParams, SetCookiesParams,
    SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotParams, EnableParams as PageEnableParams, EventLifecycleEvent,
    SetLifecycleEventsEnabledParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    EnableParams as RuntimeEnableParams, EventExceptionThrown,
};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use serde::Serialize;

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
/// alive, and return the shared browser handle plus the browser's default
/// user-agent string. The watcher must outlive the browser; this fn
/// `tokio::spawn`s it as an unsupervised task.
pub async fn launch() -> Result<(Browser, String), Error> {
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

    // Watcher: must never exit. Dropping it tears down the CDP connection.
    tokio::spawn(async move {
        loop {
            match handler.next().await {
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::debug!("browser handler error: {e}");
                }
                None => {
                    tracing::error!("browser handler stream ended; reconnect required");
                    futures::future::pending::<()>().await;
                }
            }
        }
    });

    let version = browser.version().await?;
    Ok((browser, version.user_agent))
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
    pub resources: Vec<WebPageResource>,
    pub screenshot: Option<Screenshot>,
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

    page.goto(url).await?;

    let mut resources: HashMap<String, WebPageResource> = HashMap::new();
    let mut exceptions: Vec<String> = Vec::new();
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
            else => break,
        }
    }

    let to_ms = |t: Option<f64>| -> u32 {
        match (init_ts, t) {
            (Some(init), Some(t)) => ((t - init).max(0.0) * 1000.0) as u32,
            _ => 0,
        }
    };

    let data = page.content().await?;

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
        data,
        exceptions,
        resources: resources_vec,
        screenshot,
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
