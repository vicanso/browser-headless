//! MCP (Model Context Protocol) front-end — exposes the capture engine as
//! tools an AI agent can call. A third, parallel entrypoint next to `http`
//! (REST) and `worker` (Redis queue): depends only on `capture` + `browser` +
//! `config` + `rate_limit`, never axum. Both transports drive the same
//! [`McpServer`]:
//!
//! - **Streamable HTTP** — [`streamable_service`] is mounted at `/mcp` inside
//!   the serve-mode router (see `http::router`), sharing the process's browser
//!   pool and the capture routes' X-Api-Key auth.
//! - **stdio** — `BROWSER_HEADLESS_MODE=mcp` runs [`run_stdio`]: the MCP
//!   client (e.g. Claude Code) spawns this binary and speaks JSON-RPC over
//!   stdin/stdout. Logs MUST go to stderr in this mode — `main` picks the
//!   stderr writer before tracing init.
//!
//! Tool design: intent-shaped tools with **context-window-friendly defaults**
//! (`max_chars` truncation, scalar `page_signals` JSON, audit body omitted).
//! Heavyweight REST payloads (`resources[]`, HAR, DOM snapshots) are not
//! exposed. SSRF, pool admission, timeout clamps, and per-capture metrics all
//! apply because every tool goes through `capture::capture_one`.

use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::browser::{Lang, WebPageStat};
use crate::capture::{self, CaptureCtx, Captured, ContentResponse, SummaryQuery};
use crate::error::CaptureError;
use crate::rate_limit::RateLimiter;

/// Default max Unicode scalars returned by `fetch_page` body. Agents that need
/// more can raise `max_chars` up to [`MAX_CHARS_HARD_CAP`].
const DEFAULT_MAX_CHARS: usize = 30_000;

/// Absolute ceiling for `max_chars` so a tool call cannot dump multi-MB HTML
/// into a model context.
const MAX_CHARS_HARD_CAP: usize = 200_000;

const M_MCP_TOOLS: &str = "browser_headless_mcp_tool_calls_total";
const M_MCP_DURATION: &str = "browser_headless_mcp_tool_duration_seconds";

/// Content format for `fetch_page`'s returned page body.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum PageFormat {
    Markdown,
    Text,
    Html,
}

impl PageFormat {
    fn as_str(self) -> &'static str {
        match self {
            PageFormat::Markdown => "markdown",
            PageFormat::Text => "text",
            PageFormat::Html => "html",
        }
    }
}

/// Report language for `page_audit`.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum AuditLang {
    En,
    Zh,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FetchPageArgs {
    /// The http(s) URL to fetch. Private/internal addresses are rejected.
    url: String,
    /// Format of the returned page body: `markdown` (default, best for
    /// reading articles/docs), `text` (plain text), or `html` (raw HTML).
    format: Option<PageFormat>,
    /// Total capture budget in milliseconds (server default/limits apply).
    timeout_ms: Option<u64>,
    /// CSS selector to wait for before capturing — use when the content you
    /// need renders late (e.g. `#article-body`).
    wait_for_element: Option<String>,
    /// When true, wait for network-idle instead of the faster `load` event.
    /// Slower but catches content loaded by late XHR/fetch calls.
    wait_for_network_idle: Option<bool>,
    /// Max Unicode characters of page body to return (default 30000, hard cap
    /// 200000). Longer pages are truncated with an explicit note so the model
    /// context is not flooded. Meta header (status / final_url / char_count)
    /// is always included and is not counted against this limit.
    max_chars: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScreenshotArgs {
    /// The http(s) URL to render. Private/internal addresses are rejected.
    url: String,
    /// Viewport width in pixels (server default when omitted).
    width: Option<u32>,
    /// Viewport height in pixels (server default when omitted).
    height: Option<u32>,
    /// Total capture budget in milliseconds (server default/limits apply).
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageAuditArgs {
    /// The http(s) URL to audit. Private/internal addresses are rejected.
    url: String,
    /// Report language: `en` (default) or `zh`.
    lang: Option<AuditLang>,
    /// Total capture budget in milliseconds (server default/limits apply).
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageSignalsArgs {
    /// The http(s) URL to probe. Private/internal addresses are rejected.
    url: String,
    /// Total capture budget in milliseconds (server default/limits apply).
    timeout_ms: Option<u64>,
    /// When true, wait for network-idle (slower). Default uses the faster
    /// `load` event path.
    wait_for_network_idle: Option<bool>,
}

/// The MCP tool server. Cheap to clone/construct (Arc handles only) — the
/// Streamable HTTP transport builds one per session via a factory closure.
#[derive(Clone)]
pub(crate) struct McpServer {
    ctx: CaptureCtx,
    rate_limiter: Arc<RateLimiter>,
}

impl McpServer {
    pub(crate) fn new(ctx: CaptureCtx, rate_limiter: Arc<RateLimiter>) -> Self {
        McpServer { ctx, rate_limiter }
    }

    /// Same optional token bucket as the REST capture routes. Returns the
    /// ready-made tool-level error so tools can `return Ok(err)` — rate
    /// exhaustion is the caller's problem to back off from, not a protocol
    /// failure.
    fn check_rate(&self) -> Result<(), CallToolResult> {
        if self.rate_limiter.try_acquire() {
            Ok(())
        } else {
            Err(CallToolResult::error(vec![ContentBlock::text(
                "rate limit exceeded (HTTP 429): retry later",
            )]))
        }
    }
}

/// Record one MCP tool invocation for Prometheus (`entry=mcp` is implicit
/// via the metric name; `tool` labels the specific tool).
fn record_mcp_tool(tool: &'static str, outcome: &'static str, started: Instant) {
    metrics::counter!(M_MCP_TOOLS, "tool" => tool, "outcome" => outcome).increment(1);
    metrics::histogram!(M_MCP_DURATION, "tool" => tool).record(started.elapsed().as_secs_f64());
}

/// Deserialize a `json!` field map into `SummaryQuery`, letting serde fill
/// every unspecified knob with the same defaults the HTTP layer uses.
fn build_query(fields: serde_json::Value) -> Result<SummaryQuery, ErrorData> {
    serde_json::from_value(fields)
        .map_err(|e| ErrorData::internal_error(format!("failed to build capture query: {e}"), None))
}

/// Render a capture failure as a tool-level error the model can read and
/// react to (retry, fix the URL, back off) — not a protocol error.
fn capture_error(e: CaptureError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "capture failed (HTTP {}): {}",
        e.status_u16(),
        e.message
    ))])
}

/// Resolve `max_chars` with default + hard cap.
fn resolve_max_chars(requested: Option<u64>) -> usize {
    let n = requested.unwrap_or(DEFAULT_MAX_CHARS as u64) as usize;
    n.clamp(1, MAX_CHARS_HARD_CAP)
}

/// Always-on meta header for `fetch_page` so models can branch without
/// scraping free-form prose. Body may be truncated independently.
fn format_fetch_page_result(
    requested_url: &str,
    content: ContentResponse,
    max_chars: usize,
) -> String {
    let original_chars = content.char_count;
    let (body, truncated) = truncate_chars(&content.data, max_chars);
    let mut out = String::with_capacity(body.len() + 256);
    out.push_str(&format!("status: {}\n", content.status));
    out.push_str(&format!("final_url: {}\n", content.final_url));
    out.push_str(&format!("char_count: {original_chars}\n"));
    out.push_str(&format!("truncated: {truncated}\n"));
    if truncated {
        out.push_str(&format!(
            "truncated_note: body truncated to {max_chars} Unicode scalars (of {original_chars}); raise max_chars (cap {MAX_CHARS_HARD_CAP}) or re-fetch a narrower page section with wait_for_element if needed\n"
        ));
    }
    if content.status != 200 {
        out.push_str(&format!(
            "warning: non-200 HTTP status {}\n",
            content.status
        ));
    }
    if content.final_url != requested_url {
        out.push_str("warning: URL redirected (final_url differs from request)\n");
    }
    if original_chars == 0 {
        out.push_str(
            "warning: empty body — page may be blocked, bot-walled, or failed to render\n",
        );
    }
    out.push('\n');
    out.push_str(&body);
    out
}

/// Truncate to at most `max` Unicode scalars (not bytes). Returns the body
/// and whether truncation occurred.
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    let count = s.chars().count();
    if count <= max {
        return (s.to_string(), false);
    }
    let truncated: String = s.chars().take(max).collect();
    (truncated, true)
}

/// Compact JSON for `page_signals` — scalars only, no page body / resources[].
fn build_page_signals_json(requested_url: &str, stat: &WebPageStat) -> serde_json::Value {
    let http_status = stat
        .document_timing
        .as_ref()
        .map(|dt| dt.status)
        .unwrap_or(0);

    let final_url = stat
        .document_timing
        .as_ref()
        .map(|dt| dt.url.clone())
        .or_else(|| stat.http_errors.as_ref().map(|h| h.final_url.clone()))
        .unwrap_or_else(|| requested_url.to_string());

    let redirect_count = stat
        .http_errors
        .as_ref()
        .map(|h| h.redirect_count)
        .unwrap_or(0);

    let failed_4xx = stat
        .http_errors
        .as_ref()
        .map(|h| h.failed_4xx.len() as u32)
        .unwrap_or(0);
    let failed_5xx = stat
        .http_errors
        .as_ref()
        .map(|h| h.failed_5xx.len() as u32)
        .unwrap_or(0);
    let network_failures = stat
        .http_errors
        .as_ref()
        .map(|h| h.network_failures.len() as u32)
        .unwrap_or(0);

    let vitals = stat.web_vitals.as_ref().map(|v| {
        json!({
            "lcp_ms": v.lcp,
            "cls": v.cls,
            "tbt_ms": v.tbt,
            "ttfb_ms": v.ttfb,
            "inp_ms": v.inp,
            "long_tasks": v.long_tasks,
        })
    });

    let doc_timing = stat.document_timing.as_ref().map(|dt| {
        json!({
            "dns_ms": dt.dns_ms,
            "tcp_ms": dt.tcp_ms,
            "tls_ms": dt.tls_ms,
            "ttfb_ms": dt.ttfb_ms,
            "from_cache": dt.from_cache,
            "protocol": dt.protocol,
        })
    });

    let metadata = stat.metadata.as_ref().map(|m| {
        json!({
            "title": m.title,
            "description": m.description,
            "canonical": m.canonical,
            "lang": m.lang,
            "robots": m.robots,
        })
    });

    let hdr = &stat.security_audit.headers;
    let sec = json!({
        "hsts": hdr.hsts,
        "csp": hdr.csp,
        "x_frame_options": hdr.x_frame_options,
        "x_content_type_options": hdr.x_content_type_options,
        "referrer_policy": hdr.referrer_policy,
        "present_count": hdr.present_count,
        "missing": hdr.missing,
    });

    let cache_hit = if stat.resource_summary.cache_hit_ratio.is_finite() {
        stat.resource_summary.cache_hit_ratio
    } else {
        0.0
    };

    let ok = (200..400).contains(&http_status) && failed_5xx == 0;

    json!({
        "url": requested_url,
        "final_url": final_url,
        "status": http_status,
        "redirected": final_url != requested_url,
        "redirect_count": redirect_count,
        "load_ms": stat.load_time,
        "fcp_ms": stat.fcp_time,
        "dcl_ms": stat.dcl_time,
        "resource_count": stat.resource_count,
        "total_transfer_bytes": stat.total_size,
        "cache_hit_ratio": cache_hit,
        "js_exception_count": stat.js_exceptions.total,
        "js_exception_top": stat.js_exceptions.by_name.iter().take(5).map(|e| json!({
            "name": e.name,
            "count": e.count,
            "sample": e.sample_message,
        })).collect::<Vec<_>>(),
        "failed_4xx_count": failed_4xx,
        "failed_5xx_count": failed_5xx,
        "network_failure_count": network_failures,
        "document_timing": doc_timing,
        "web_vitals": vitals,
        "metadata": metadata,
        "security_headers": sec,
        "ok": ok,
    })
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "DEFAULT for reading page content. Headless Chrome (JS runs, SPAs work). Returns a fixed meta header (status, final_url, char_count, truncated) then the body as markdown (default) / text / html. Body is truncated at max_chars (default 30000) so it fits model context. Do NOT use for performance/security diagnosis — use page_signals (cheap JSON) or page_audit (full report). Private/internal URLs are blocked."
    )]
    async fn fetch_page(
        &self,
        Parameters(args): Parameters<FetchPageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        if let Err(limited) = self.check_rate() {
            record_mcp_tool("fetch_page", "rate_limited", started);
            return Ok(limited);
        }
        let requested_url = args.url.clone();
        let max_chars = resolve_max_chars(args.max_chars);
        let mut fields = json!({
            "url": args.url,
            "content_only": true,
            "data_format": args.format.unwrap_or(PageFormat::Markdown).as_str(),
            // Fast content path by default; network-idle on request.
            "wait_until_load": !args.wait_for_network_idle.unwrap_or(false),
        });
        if let Some(t) = args.timeout_ms {
            fields["timeout_ms"] = json!(t);
        }
        if let Some(sel) = args.wait_for_element {
            fields["wait_for_element"] = json!(sel);
        }
        let q = build_query(fields)?;

        let content = match capture::capture_one(&self.ctx, q).await {
            Ok(Captured::Content(c)) => c,
            Ok(Captured::Full(_)) => {
                record_mcp_tool("fetch_page", "error", started);
                return Err(ErrorData::internal_error(
                    "unexpected full capture result for content-only query",
                    None,
                ));
            }
            Err(e) => {
                record_mcp_tool("fetch_page", "error", started);
                return Ok(capture_error(e));
            }
        };

        let text = format_fetch_page_result(&requested_url, content, max_chars);
        record_mcp_tool("fetch_page", "ok", started);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Cheap health/perf/security SIGNAL check — returns compact JSON only (status, redirects, load timings, Core Web Vitals, JS exception counts, 4xx/5xx counts, key security headers, title). Prefer this over page_audit when you only need to know if a page is slow, broken, empty, redirected, or missing HSTS/CSP. Does NOT return page body (use fetch_page). Does NOT return a long narrative report (use page_audit for that)."
    )]
    async fn page_signals(
        &self,
        Parameters(args): Parameters<PageSignalsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        if let Err(limited) = self.check_rate() {
            record_mcp_tool("page_signals", "rate_limited", started);
            return Ok(limited);
        }
        let requested_url = args.url.clone();
        // Lite analytical set — not profile=audit (too heavy). No resources[]
        // in the response; free-early summary still runs server-side.
        let mut fields = json!({
            "url": args.url,
            "wait_until_load": !args.wait_for_network_idle.unwrap_or(false),
            "http_errors": true,
            "web_vitals": true,
            "metadata": true,
            "metrics": true,
            "data_format": "text",
        });
        if let Some(t) = args.timeout_ms {
            fields["timeout_ms"] = json!(t);
        }
        let q = build_query(fields)?;

        match capture::capture_one(&self.ctx, q).await {
            Ok(Captured::Full(stat)) => {
                let payload = build_page_signals_json(&requested_url, &stat);
                record_mcp_tool("page_signals", "ok", started);
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
                )]))
            }
            Ok(Captured::Content(_)) => {
                record_mcp_tool("page_signals", "error", started);
                Err(ErrorData::internal_error(
                    "unexpected content-only result for page_signals query",
                    None,
                ))
            }
            Err(e) => {
                record_mcp_tool("page_signals", "error", started);
                Ok(capture_error(e))
            }
        }
    }

    #[tool(
        description = "Render a web page in headless Chrome and return a PNG screenshot of the viewport (image block). Use only when the agent must SEE layout/visual state; prefer fetch_page for reading text and page_signals for health checks. Set width/height for viewport size."
    )]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        if let Err(limited) = self.check_rate() {
            record_mcp_tool("screenshot", "rate_limited", started);
            return Ok(limited);
        }
        let mut fields = json!({
            "url": args.url,
            "screenshot": true,
        });
        if let Some(w) = args.width {
            fields["width"] = json!(w);
        }
        if let Some(h) = args.height {
            fields["height"] = json!(h);
        }
        if let Some(t) = args.timeout_ms {
            fields["timeout_ms"] = json!(t);
        }
        let q = build_query(fields)?;

        match capture::capture_one(&self.ctx, q).await {
            Ok(Captured::Full(stat)) => match stat.screenshot {
                Some(shot) => {
                    record_mcp_tool("screenshot", "ok", started);
                    Ok(CallToolResult::success(vec![ContentBlock::image(
                        shot.data,
                        shot.mime_type,
                    )]))
                }
                None => {
                    record_mcp_tool("screenshot", "error", started);
                    Ok(CallToolResult::error(vec![ContentBlock::text(
                        "page loaded but no screenshot was produced",
                    )]))
                }
            },
            Ok(Captured::Content(_)) => {
                record_mcp_tool("screenshot", "error", started);
                Err(ErrorData::internal_error(
                    "unexpected content-only result for screenshot query",
                    None,
                ))
            }
            Err(e) => {
                record_mcp_tool("screenshot", "error", started);
                Ok(capture_error(e))
            }
        }
    }

    #[tool(
        description = "Full diagnostic markdown report (performance, Core Web Vitals, network summary, JS exceptions, security, SEO metadata). HEAVY and long — prefer page_signals first for a cheap JSON triage. Page body is omitted (use fetch_page for content). Only use when the user asks for a deep audit or page_signals already indicated a problem worth explaining."
    )]
    async fn page_audit(
        &self,
        Parameters(args): Parameters<PageAuditArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        if let Err(limited) = self.check_rate() {
            record_mcp_tool("page_audit", "rate_limited", started);
            return Ok(limited);
        }
        let mut fields = json!({
            "url": args.url,
            // Full analytical suite (same OR-mask as all_metrics), no binaries.
            "profile": "audit",
        });
        if let Some(t) = args.timeout_ms {
            fields["timeout_ms"] = json!(t);
        }
        let q = build_query(fields)?;
        let lang = match args.lang.unwrap_or(AuditLang::En) {
            AuditLang::En => Lang::En,
            AuditLang::Zh => Lang::Zh,
        };

        match capture::capture_one(&self.ctx, q).await {
            Ok(Captured::Full(mut stat)) => {
                // The markdown report ends with the raw page body — megabytes
                // of HTML that would drown the audit signal in a model
                // context. Replace it with a pointer to the right tool.
                stat.data = String::from(
                    "(omitted — use fetch_page for content; use page_signals for compact JSON health)",
                );
                record_mcp_tool("page_audit", "ok", started);
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    stat.to_markdown(lang),
                )]))
            }
            Ok(Captured::Content(_)) => {
                record_mcp_tool("page_audit", "error", started);
                Err(ErrorData::internal_error(
                    "unexpected content-only result for audit query",
                    None,
                ))
            }
            Err(e) => {
                record_mcp_tool("page_audit", "error", started);
                Ok(capture_error(e))
            }
        }
    }
}

/// Server-level guidance shown to the model alongside the tool list.
const INSTRUCTIONS: &str = "\
Headless-Chrome capture for agents. Decision tree:
1) Read/summarize page text → fetch_page (markdown default; body capped by max_chars; meta header always present).
2) Quick health/perf/security triage → page_signals (small JSON; prefer over page_audit).
3) Deep diagnosis / narrative report → page_audit (heavy; only after signals or user asks for full audit).
4) Visual layout only → screenshot.
Only public http(s) URLs; private/internal hosts are blocked (SSRF guard). \
Do not call page_audit when you only need content or a yes/no health check.";

#[tool_handler]
impl ServerHandler for McpServer {
    // Explicit so serverInfo advertises THIS service, not the SDK — the
    // macro's default `Implementation::from_build_env()` expands inside the
    // rmcp crate and would report `rmcp/<sdk-version>`.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

/// Build the Streamable-HTTP tower service mounted at `/mcp` by
/// `http::router`. Host-header validation is disabled: the service binds
/// `0.0.0.0` for remote clients by design, and `/mcp` sits behind the same
/// X-Api-Key check as the capture routes (enforced by the router-side
/// middleware, not here).
pub(crate) fn streamable_service(
    ctx: CaptureCtx,
    rate_limiter: Arc<RateLimiter>,
) -> StreamableHttpService<McpServer, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(McpServer::new(ctx.clone(), Arc::clone(&rate_limiter))),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    )
}

/// stdio transport for `BROWSER_HEADLESS_MODE=mcp`: serve MCP over
/// stdin/stdout until the client closes the pipe or the process is signalled.
/// The pool is dropped by the caller after return, tearing down Chrome.
pub(crate) async fn run_stdio(ctx: CaptureCtx, mut shutdown: watch::Receiver<bool>) {
    let server = McpServer::new(ctx, Arc::new(RateLimiter::from_env()));
    let running = match server.serve(stdio()).await {
        Ok(running) => running,
        Err(e) => {
            tracing::error!(error = %e, "mcp stdio server failed to initialize");
            return;
        }
    };
    tracing::info!("mcp stdio server ready");
    tokio::select! {
        _ = running.waiting() => tracing::info!("mcp client disconnected"),
        _ = shutdown.wait_for(|v| *v) => tracing::info!("shutdown signal received, closing mcp server"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_short_unchanged() {
        let (s, t) = truncate_chars("hello", 10);
        assert_eq!(s, "hello");
        assert!(!t);
    }

    #[test]
    fn truncate_chars_long() {
        let (s, t) = truncate_chars("abcdef", 3);
        assert_eq!(s, "abc");
        assert!(t);
        assert_eq!(s.chars().count(), 3);
    }

    #[test]
    fn resolve_max_chars_default_and_cap() {
        assert_eq!(resolve_max_chars(None), DEFAULT_MAX_CHARS);
        assert_eq!(resolve_max_chars(Some(100)), 100);
        assert_eq!(resolve_max_chars(Some(0)), 1);
        assert_eq!(resolve_max_chars(Some(u64::MAX)), MAX_CHARS_HARD_CAP);
    }

    #[test]
    fn format_fetch_page_always_has_meta() {
        let text = format_fetch_page_result(
            "https://example.com/",
            ContentResponse {
                status: 200,
                final_url: "https://example.com/".into(),
                char_count: 5,
                data: "hello".into(),
            },
            30_000,
        );
        assert!(text.starts_with("status: 200\n"));
        assert!(text.contains("final_url: https://example.com/\n"));
        assert!(text.contains("char_count: 5\n"));
        assert!(text.contains("truncated: false\n"));
        assert!(text.contains("\nhello"));
    }

    #[test]
    fn format_fetch_page_truncates_body() {
        let text = format_fetch_page_result(
            "https://x/",
            ContentResponse {
                status: 200,
                final_url: "https://x/".into(),
                char_count: 10,
                data: "0123456789".into(),
            },
            4,
        );
        assert!(text.contains("truncated: true\n"));
        assert!(text.contains("truncated_note:"));
        assert!(text.ends_with("0123") || text.contains("\n0123"));
        assert!(!text.contains("0123456789"));
    }
}
