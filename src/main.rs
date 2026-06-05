mod browser;
mod capture;
mod config;
mod http;
mod pool;
mod worker;

use std::sync::Arc;

use tokio::net::TcpListener;

use crate::capture::CaptureCtx;
use crate::http::AppState;

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

/// Dispatch on `BROWSER_HEADLESS_MODE`: `serve` (default) runs the HTTP API;
/// `worker` runs the Redis queue consumer. Both share the same browser pool /
/// capture engine; the two paths never call into each other.
async fn run() {
    let mode = std::env::var("BROWSER_HEADLESS_MODE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "serve".to_string());
    match mode.as_str() {
        "serve" => run_serve().await,
        "worker" => run_worker().await,
        other => panic!("unknown BROWSER_HEADLESS_MODE `{other}` (expected `serve` or `worker`)"),
    }
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
    // Install the Prometheus recorder BEFORE launching the pool so the pool's
    // startup gauges (pool_size / active_instances) are recorded.
    let metrics_handle = http::init_metrics();
    let ctx = build_capture_ctx().await;

    let api_key = std::env::var("BROWSER_HEADLESS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Arc::new);
    if api_key.is_some() {
        tracing::info!("API key auth enabled (X-Api-Key header required)");
    } else {
        tracing::warn!("API key auth disabled — /summary is open to anyone");
    }

    let state = AppState {
        ctx,
        api_key,
        metrics_handle,
    };

    let addr = "0.0.0.0:3000";
    let listener = TcpListener::bind(addr).await.expect("failed to bind");
    tracing::info!(
        "listening on {addr} with {} worker threads",
        num_cpus::get()
    );
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    tracing::info!("server shut down cleanly");
}

/// Queue worker mode: consume capture jobs from Redis (no HTTP). Runs until the
/// process is signalled.
async fn run_worker() {
    let ctx = build_capture_ctx().await;
    worker::run(ctx).await;
}

/// Resolves when the process receives SIGTERM (`docker stop`, k8s pod
/// eviction, systemd `Term=signal`) or SIGINT (Ctrl-C in a foreground shell).
/// Used with `axum::serve(...).with_graceful_shutdown(...)` so the server
/// stops accepting new connections and waits for in-flight requests to
/// complete before returning. After return, AppState drops, the pool's
/// `Arc<Browser>`s reach refcount 0, and chromiumoxide's Drop tears down the
/// chrome subprocesses.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM, initiating graceful shutdown"),
        _ = sigint.recv() => tracing::info!("received SIGINT, initiating graceful shutdown"),
    }
}
