mod browser;
mod capture;
mod config;
mod error;
mod http;
mod jobs;
mod mcp;
mod pool;
mod queue;
mod rate_limit;
mod redis_conn;
mod ssrf;
mod worker;

use std::sync::Arc;

use metrics_exporter_prometheus::PrometheusHandle;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::capture::CaptureCtx;
use crate::config::LogFormat;
use crate::http::AppState;
use crate::rate_limit::RateLimiter;

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,chromiumoxide::conn=off,chromiumoxide::handler=off".into());
    // stdio MCP mode speaks JSON-RPC on stdout — a single log line there
    // corrupts the protocol stream, so logs must go to stderr.
    let log_to_stderr = std::env::var("BROWSER_HEADLESS_MODE").is_ok_and(|m| m == "mcp");
    match (config::log_format(), log_to_stderr) {
        (LogFormat::Json, false) => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        }
        (LogFormat::Json, true) => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
        }
        (LogFormat::Text, false) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
        (LogFormat::Text, true) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }

    // Redis TLS (`rediss://` / custom certs in worker mode) goes through rustls
    // 0.23, which needs a process-wide crypto provider chosen explicitly before
    // the first TLS handshake. Install `ring` here so a worker pointed at a TLS
    // Redis doesn't panic. Harmless when TLS is unused.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

    let worker_threads = num_cpus::get();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(worker_threads)
        .build()
        .unwrap_or_else(|e| panic!("failed to build tokio runtime: {}", e))
        .block_on(run());
}

/// Dispatch on `BROWSER_HEADLESS_MODE`: `serve` (default) runs the HTTP API,
/// `worker` runs the Redis queue consumer, `all` runs both in one process
/// sharing a single browser pool, `mcp` runs an MCP server over stdio (for
/// clients that spawn the binary locally — the HTTP API already exposes the
/// same tools at `/mcp` in serve/all mode). The serve and worker code paths
/// never call into each other; `all` just starts both against the same
/// `CaptureCtx`.
async fn run() {
    // `browser-headless healthcheck` — probe the local health endpoint and exit
    // 0 (HTTP 200) / 1 (anything else). Used as the container HEALTHCHECK so the
    // image needs no curl/wget. Handled before mode dispatch; never returns.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        run_healthcheck().await;
    }

    let mode = std::env::var("BROWSER_HEADLESS_MODE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "serve".to_string());
    match mode.as_str() {
        "serve" => run_serve().await,
        "worker" => run_worker().await,
        "all" => run_all().await,
        "mcp" => run_mcp().await,
        other => {
            panic!(
                "unknown BROWSER_HEADLESS_MODE `{other}` (expected `serve`, `worker`, `all`, or `mcp`)"
            )
        }
    }
}

/// stdio MCP mode: launch the pool, then serve MCP over stdin/stdout until
/// the client disconnects or the process is signalled. No HTTP listener and
/// no Prometheus recorder — the metric macros inside `capture_one` no-op
/// without one, which is fine for a client-spawned local process.
async fn run_mcp() {
    let ctx = build_capture_ctx().await;
    mcp::run_stdio(ctx, install_shutdown()).await;
}

/// Launch the browser pool and build the shared, HTTP-agnostic capture context.
/// Used by both modes; logs the resolved timeout defaults and SSRF policy.
async fn build_capture_ctx() -> CaptureCtx {
    // Each instance gets its own manager task that respawns it on crash and
    // recycles it on the configured age / request thresholds. Backwards-
    // compatible default: pool_size = 1, recycle off.
    let pool = Arc::new(pool::BrowserPool::launch(pool::PoolConfig::from_env()).await);

    // Surface the resolved per-request default so operators can confirm an
    // override took effect (set via BROWSER_HEADLESS_DEFAULT_TIMEOUT_MS).
    tracing::info!(
        default_timeout_ms = config::default_timeout_ms(),
        deadline_buffer_ms = config::deadline_buffer_ms(),
        "per-request timeout default"
    );

    let allow_private_ips = std::env::var("BROWSER_HEADLESS_ALLOW_PRIVATE_IPS")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if allow_private_ips {
        tracing::warn!("SSRF guard disabled — private/loopback IPs allowed");
    }

    CaptureCtx {
        pool,
        allow_private_ips,
    }
}

/// HTTP API mode: serve `/summary*` + probes + `/metrics` on `0.0.0.0:3000`.
async fn run_serve() {
    // Fail-fast on config errors BEFORE the (expensive) pool launch.
    let api_key = resolve_api_key();
    // Install the Prometheus recorder BEFORE launching the pool so the pool's
    // startup gauges (pool_size / active_instances) are recorded.
    let metrics_handle = http::init_metrics();
    let ctx = build_capture_ctx().await;
    serve_http(ctx, metrics_handle, api_key, install_shutdown()).await;
}

/// Queue worker mode: consume capture jobs from Redis. Binds no API port, but
/// DOES expose a minimal `/healthz` + `/readyz` + `/metrics` listener on
/// `BROWSER_HEADLESS_HEALTH_PORT` so the worker is probeable / scrapeable. The
/// Prometheus recorder is installed here (worker mode otherwise has none, so
/// the capture + worker metric macros would silently no-op). Runs until
/// signalled, then drains in-flight jobs and returns.
async fn run_worker() {
    let metrics_handle = http::init_metrics();
    let ctx = build_capture_ctx().await;
    let api_key = std::env::var("BROWSER_HEADLESS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Arc::new);
    tokio::spawn(serve_health(
        ctx.clone(),
        metrics_handle,
        config::health_port(),
        api_key,
    ));
    worker::run(ctx, install_shutdown()).await;
}

/// Combined mode: run the HTTP API and the Redis queue consumer in one process,
/// both driving the SAME browser pool (so they compete for the
/// `POOL_SIZE × MAX_PAGES` capacity — split into separate processes if you need
/// them to scale independently). Both share one shutdown signal: on SIGTERM the
/// HTTP server drains its connections while the worker drains its in-flight
/// jobs; we await the worker AFTER the server returns so the process doesn't
/// exit (killing the worker) mid-drain.
async fn run_all() {
    // Fail-fast on config errors BEFORE the (expensive) pool launch.
    let api_key = resolve_api_key();
    let metrics_handle = http::init_metrics();
    let ctx = build_capture_ctx().await;
    let shutdown = install_shutdown();
    // Background queue consumer, sharing the pool. A fatal worker config error
    // panics this task (logged by tokio) without taking down the HTTP server.
    let worker = tokio::spawn(worker::run(ctx.clone(), shutdown.clone()));
    serve_http(ctx, metrics_handle, api_key, shutdown).await;
    let _ = worker.await;
}

/// Build `AppState` from the shared capture context and serve the HTTP API
/// until `shutdown` fires, then drain connections. Shared by `serve` and `all`.
/// Resolve the API key, enforcing `BROWSER_HEADLESS_REQUIRE_API_KEY`.
/// Called BEFORE the browser pool launches (fail-fast: a config error must
/// not burn a full Chromium fleet startup before being reported).
fn resolve_api_key() -> Option<Arc<String>> {
    let api_key = std::env::var("BROWSER_HEADLESS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Arc::new);
    if api_key.is_none() && config::require_api_key() {
        panic!(
            "BROWSER_HEADLESS_REQUIRE_API_KEY is set but BROWSER_HEADLESS_API_KEY is empty — refusing to start open"
        );
    }
    api_key
}

async fn serve_http(
    ctx: CaptureCtx,
    metrics_handle: PrometheusHandle,
    api_key: Option<Arc<String>>,
    shutdown: watch::Receiver<bool>,
) {
    if api_key.is_some() {
        tracing::info!("API key auth enabled (X-Api-Key header required)");
    } else {
        tracing::warn!("API key auth disabled — /summary is open to anyone");
    }
    if config::disable_script() {
        tracing::info!("script parameter disabled (BROWSER_HEADLESS_DISABLE_SCRIPT)");
    }
    if config::protect_metrics() {
        tracing::info!("/metrics requires X-Api-Key (BROWSER_HEADLESS_PROTECT_METRICS)");
    }

    let jobs = queue::init_jobs_backend().await;
    if jobs.is_some() {
        tracing::info!("async job API enabled at POST /jobs + GET /jobs/:id");
    }
    if !config::disable_mcp() {
        tracing::info!("MCP endpoint enabled at /mcp (streamable HTTP)");
    }
    // Kept for the post-serve drain — `state` is consumed by the router.
    let jobs_drain = jobs.clone();

    let state = AppState {
        ctx,
        api_key,
        metrics_handle,
        rate_limiter: Arc::new(RateLimiter::from_env()),
        jobs,
    };

    let addr = "0.0.0.0:3000";
    let listener = TcpListener::bind(addr).await.expect("failed to bind");
    tracing::info!(
        max_timeout_ms = config::max_timeout_ms(),
        max_body_bytes = config::max_body_bytes(),
        "listening on {addr} with {} worker threads",
        num_cpus::get()
    );
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_future(shutdown))
        .await
        .expect("server error");

    // Local async jobs run as detached tasks the HTTP graceful-shutdown
    // knows nothing about — drain them before returning (mirrors the
    // worker's drain), else runtime teardown cancels them mid-capture.
    if let Some(jobs::JobsBackend::Local(store)) = &jobs_drain {
        tracing::info!("draining in-flight local async jobs");
        store
            .drain(std::time::Duration::from_millis(config::jobs_drain_ms()))
            .await;
    }
    tracing::info!("server shut down cleanly");
}

/// Spawn the process-wide signal listener and return a `watch` receiver that
/// flips to `true` on SIGTERM (`docker stop`, k8s eviction, systemd
/// `Term=signal`) or SIGINT (Ctrl-C). One sender drives every mode's graceful
/// shutdown: the HTTP server stops accepting connections and drains in-flight
/// requests, and the worker stops pulling jobs and drains its in-flight
/// captures. After both return, the pool drops, `Arc<Browser>`s hit refcount 0,
/// and chromiumoxide tears down the chrome subprocesses.
fn install_shutdown() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_signal().await;
        let _ = tx.send(true);
    });
    rx
}

/// Resolve on the first SIGTERM / SIGINT.
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM, initiating graceful shutdown"),
        _ = sigint.recv() => tracing::info!("received SIGINT, initiating graceful shutdown"),
    }
}

/// Resolve once `shutdown` flips to `true`; used as the axum graceful-shutdown
/// future.
async fn shutdown_future(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|v| *v).await;
}

/// Bind the worker's health/metrics listener (`/healthz`, `/readyz`,
/// `/metrics`) on `0.0.0.0:port`. A bind failure is logged but NON-fatal — the
/// worker keeps consuming jobs; only the probes/metrics go dark. Spawned as a
/// background task by `run_worker`.
async fn serve_health(
    ctx: CaptureCtx,
    metrics_handle: PrometheusHandle,
    port: u16,
    api_key: Option<Arc<String>>,
) {
    let addr = format!("0.0.0.0:{port}");
    match TcpListener::bind(&addr).await {
        Ok(listener) => {
            tracing::info!("worker health/metrics listening on {addr}");
            if let Err(e) =
                axum::serve(listener, http::health_router(ctx, metrics_handle, api_key)).await
            {
                tracing::error!(error = %e, "worker health server error");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, %addr, "failed to bind worker health port; probes disabled");
        }
    }
}

/// `healthcheck` subcommand: GET `/healthz` on the local health port and exit
/// 0 on HTTP 200, else 1. The probed port matches the running mode — `worker`
/// uses `BROWSER_HEADLESS_HEALTH_PORT`; serve / all use the fixed API port 3000.
async fn run_healthcheck() -> ! {
    let mode = std::env::var("BROWSER_HEADLESS_MODE").unwrap_or_default();
    let port = if mode == "worker" {
        config::health_port()
    } else {
        3000
    };
    match probe_health(port).await {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("healthcheck failed (port {port}): {e}");
            std::process::exit(1);
        }
    }
}

/// Minimal, dependency-free HTTP/1.0 `GET /healthz` against `127.0.0.1:port`.
/// Success = a `200` status line received within the 5s budget. Kept tiny on
/// purpose: it avoids pulling an HTTP client into the runtime image just for
/// the container HEALTHCHECK.
async fn probe_health(port: u16) -> Result<(), String> {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let budget = Duration::from_secs(5);
    let mut stream = tokio::time::timeout(budget, TcpStream::connect(("127.0.0.1", port)))
        .await
        .map_err(|_| "connect timed out".to_string())?
        .map_err(|e| format!("connect: {e}"))?;

    let req =
        format!("GET /healthz HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;

    let mut buf = Vec::with_capacity(256);
    tokio::time::timeout(budget, stream.read_to_end(&mut buf))
        .await
        .map_err(|_| "read timed out".to_string())?
        .map_err(|e| format!("read: {e}"))?;

    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or_default();
    if status_line.contains(" 200 ") {
        Ok(())
    } else {
        Err(format!("unexpected status line: {status_line:?}"))
    }
}
