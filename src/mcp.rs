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
//! Tool design: deliberately three intent-shaped tools with lean defaults
//! instead of the REST API's full knob surface — tool output lands in a
//! model's context window, so the heavyweight payloads (`resources[]`, HAR,
//! DOM snapshots) are not exposed here. SSRF checks, pool admission control,
//! timeout clamps, and per-capture metrics all apply unchanged because every
//! tool goes through `capture::capture_one`.

use std::sync::Arc;

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

use crate::browser::Lang;
use crate::capture::{self, CaptureCtx, Captured, SummaryQuery};
use crate::error::CaptureError;
use crate::rate_limit::RateLimiter;

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

#[tool_router]
impl McpServer {
    #[tool(
        description = "Fetch a web page with a real headless-Chrome browser (JavaScript executed, SPA content rendered) and return the page body as markdown (default), plain text, or raw HTML. Use this to read articles, documentation, or any page content."
    )]
    async fn fetch_page(
        &self,
        Parameters(args): Parameters<FetchPageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(limited) = self.check_rate() {
            return Ok(limited);
        }
        let requested_url = args.url.clone();
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
            // content_only=true always yields the compact variant; treat the
            // other arm as an internal invariant break rather than panicking.
            Ok(Captured::Full(_)) => {
                return Err(ErrorData::internal_error(
                    "unexpected full capture result for content-only query",
                    None,
                ));
            }
            Err(e) => return Ok(capture_error(e)),
        };

        // Prefix the body with capture facts only when they are noteworthy —
        // a clean 200 with no redirect returns the bare page content.
        let mut notes = String::new();
        if content.status != 200 {
            notes.push_str(&format!("[HTTP status: {}]\n", content.status));
        }
        if content.final_url != requested_url {
            notes.push_str(&format!(
                "[final URL after redirects: {}]\n",
                content.final_url
            ));
        }
        if content.char_count == 0 {
            notes.push_str("[page returned no content — possibly blocked or render failure]\n");
        }
        let text = if notes.is_empty() {
            content.data
        } else {
            format!("{notes}\n{}", content.data)
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Render a web page in headless Chrome and return a screenshot of the viewport as a PNG image. Set width/height to control the viewport."
    )]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(limited) = self.check_rate() {
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
                Some(shot) => Ok(CallToolResult::success(vec![ContentBlock::image(
                    shot.data,
                    shot.mime_type,
                )])),
                None => Ok(CallToolResult::error(vec![ContentBlock::text(
                    "page loaded but no screenshot was produced",
                )])),
            },
            Ok(Captured::Content(_)) => Err(ErrorData::internal_error(
                "unexpected content-only result for screenshot query",
                None,
            )),
            Err(e) => Ok(capture_error(e)),
        }
    }

    #[tool(
        description = "Run a full page audit in headless Chrome — load performance timings, Core Web Vitals, resource/network summary, JS exceptions, security scan (CSP/HSTS/SRI/mixed content), SEO metadata — and return a structured markdown report. Use for diagnosing slow, broken, or insecure pages; use fetch_page instead when you only need the page content."
    )]
    async fn page_audit(
        &self,
        Parameters(args): Parameters<PageAuditArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(limited) = self.check_rate() {
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
                stat.data = String::from("(omitted — use the fetch_page tool for page content)");
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    stat.to_markdown(lang),
                )]))
            }
            Ok(Captured::Content(_)) => Err(ErrorData::internal_error(
                "unexpected content-only result for audit query",
                None,
            )),
            Err(e) => Ok(capture_error(e)),
        }
    }
}

/// Server-level guidance shown to the model alongside the tool list.
const INSTRUCTIONS: &str = "Headless-Chrome page capture service. fetch_page returns a page's rendered content (JavaScript executed, so SPAs work) as markdown/text/html; screenshot returns a PNG of the rendered viewport; page_audit returns a markdown report covering performance, Core Web Vitals, network resources, JS errors, and security. Only public http(s) URLs are allowed — private and internal addresses are blocked.";

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
