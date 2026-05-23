mod browser;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::extract::Query;
use chromiumoxide::Browser;
use serde::Deserialize;
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    browser: Arc<Browser>,
    default_user_agent: Arc<String>,
}

/// How the entire `/summary` response is delivered. Independent of
/// `OutputFormat` which controls only the `data` field's representation.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ResponseFormat {
    #[default]
    Json,
    Markdown,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum OutputFormat {
    #[default]
    Html,
    Markdown,
    /// Computed-style-aware DOM walker, inserts newlines between block-like
    /// elements. Better than `innerText` for flex/grid pages.
    Text,
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
    let (browser_inst, default_ua) = browser::launch().await.expect("failed to launch browser");
    tracing::info!("browser UA: {}", default_ua);

    let state = AppState {
        browser: Arc::new(browser_inst),
        default_user_agent: Arc::new(default_ua),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/summary", get(summary_handler))
        .with_state(state);

    let addr = "0.0.0.0:3000";
    let listener = TcpListener::bind(addr).await.expect("failed to bind");
    tracing::info!(
        "listening on {addr} with {} worker threads",
        num_cpus::get()
    );
    axum::serve(listener, app).await.expect("server error");
}

async fn root() -> &'static str {
    "Hello, world!"
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
    data_format: OutputFormat,
    /// Only meaningful for `data_format=markdown`. Default on.
    normalize_custom_elements: Option<bool>,
    width: Option<u32>,
    height: Option<u32>,
    device_scale_factor: Option<f64>,
    user_agent: Option<String>,
    accept_language: Option<String>,
    cookie: Option<String>,

    /// Force-bypass the browser's HTTP cache (disk + memory) for this request.
    /// All resources will be re-fetched from origin; `from_cache` stays false.
    #[serde(default)]
    disable_cache: bool,

    /// Optional CSS selector — wait for this element to appear before
    /// snapshotting. Polled with exponential backoff up to `timeout_ms`.
    wait_for_element: Option<String>,

    /// Optional stabilization period (ms) applied after every deterministic
    /// gate (`wait_for_request`, `networkIdle`, `wait_for_element`) and just
    /// before `data` extraction. Lets late JS render / CSS animation finish
    /// for cases the explicit waits can't express.
    settle_ms: Option<u64>,

    /// Optional CSS selector — return only the matched element's content
    /// in `data` (outerHTML for `format=html`, innerText-style for
    /// `format=text`, normalized HTML→markdown for `format=markdown`).
    capture_element: Option<String>,

    /// Zero or more URL substrings — block `collect_summary` until a
    /// response whose URL contains each substring has arrived (ALL semantics,
    /// 4xx/5xx → 502). Repeat the key: `?wait_for_request=a&wait_for_request=b`.
    #[serde(default)]
    wait_for_request: Vec<String>,

    /// Response envelope format. `json` (default) returns `application/json`
    /// with all `WebPageStat` fields. `markdown` returns `text/markdown`
    /// rendered for LLM consumption (resources as prose). Independent of
    /// `data_format` which controls only the `data` field's representation.
    #[serde(default)]
    format: ResponseFormat,
}

async fn summary_handler(
    State(state): State<AppState>,
    Query(q): Query<SummaryQuery>,
) -> Result<Response, (StatusCode, String)> {
    let page = state
        .browser
        .new_page("about:blank")
        .await
        .map_err(|e| browser_error(e.into()))?;

    browser::apply_viewport(&page, q.width, q.height, q.device_scale_factor)
        .await
        .map_err(browser_error)?;
    browser::apply_user_agent(
        &page,
        q.user_agent.as_deref(),
        q.accept_language.as_deref(),
        &state.default_user_agent,
    )
    .await
    .map_err(browser_error)?;

    if let Some(cookie_header) = q.cookie.as_deref() {
        let cookies = parse_cookie_header(cookie_header);
        browser::set_cookies(&page, &cookies, &q.url)
            .await
            .map_err(browser_error)?;
    }

    if q.disable_cache {
        browser::set_cache_disabled(&page, true)
            .await
            .map_err(browser_error)?;
    }

    let timeout = Duration::from_millis(q.timeout_ms);
    let mut stat =
        browser::collect_summary(&page, &q.url, timeout, q.screenshot, &q.wait_for_request)
            .await
            .map_err(browser_error)?;

    // After the page has settled (load + networkIdle + wait_for_request),
    // optionally wait for a late-rendered selector before extracting data.
    if let Some(selector) = q.wait_for_element.as_deref() {
        browser::wait_for_selector(&page, selector, timeout)
            .await
            .map_err(browser_error)?;
    }

    if let Some(settle_ms) = q.settle_ms {
        tokio::time::sleep(Duration::from_millis(settle_ms)).await;
    }

    // Convert `stat.data` to the requested format, scoped to capture_element
    // when provided. collect_summary already populated `data` with raw HTML
    // of the full document — we overwrite for text/markdown OR when scoping.
    let capture = q.capture_element.as_deref();
    match q.data_format {
        OutputFormat::Html => {
            if let Some(sel) = capture {
                stat.data = browser::capture_property(&page, sel, "outerHTML", timeout)
                    .await
                    .map_err(browser_error)?;
            }
        }
        OutputFormat::Text => {
            stat.data = browser::extract_text(&page, capture, timeout)
                .await
                .map_err(browser_error)?;
        }
        OutputFormat::Markdown => {
            let source = if q.normalize_custom_elements.unwrap_or(true) {
                browser::normalize_dom(&page, capture, timeout)
                    .await
                    .map_err(browser_error)?
            } else if let Some(sel) = capture {
                browser::capture_property(&page, sel, "outerHTML", timeout)
                    .await
                    .map_err(browser_error)?
            } else {
                stat.data.clone()
            };
            let converter = htmd::HtmlToMarkdown::builder()
                .skip_tags(vec!["img", "script", "style", "svg", "iframe", "noscript"])
                .build();
            stat.data = converter
                .convert(&source)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }

    let _ = page.close().await;

    let response = match q.format {
        ResponseFormat::Json => Json(stat).into_response(),
        ResponseFormat::Markdown => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            stat.to_markdown(),
        )
            .into_response(),
    };
    Ok(response)
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
