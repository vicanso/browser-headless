//! Process-wide configuration knobs, each resolved once from the environment
//! and cached (env is fixed for the process lifetime). Shared by the HTTP
//! layer and the capture core.

use std::sync::OnceLock;

/// Per-request default for `timeout_ms` (the soft page-wait budget) when the
/// caller doesn't pass one. Configurable via `BROWSER_HEADLESS_DEFAULT_TIMEOUT_MS`
/// — falls back to 30_000 (30s) when unset, empty, non-numeric, or `0`.
/// Read once and cached: serde calls this on every deserialize, so we avoid
/// re-parsing per request.
pub(crate) fn default_timeout_ms() -> u64 {
    static DEFAULT: OnceLock<u64> = OnceLock::new();
    *DEFAULT.get_or_init(|| {
        std::env::var("BROWSER_HEADLESS_DEFAULT_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(30_000)
    })
}

/// Headroom added on top of `timeout_ms` to form the hard request deadline
/// (`tokio::time::timeout` around the whole capture). It covers chromium
/// overhead outside the page-wait budget — context create / page open / data
/// extraction / dispose — so the hard cap fires a bit later than the soft
/// `timeout_ms`. Configurable via `BROWSER_HEADLESS_DEADLINE_BUFFER_MS`
/// (default 10_000 = 10s); `0` is allowed (no headroom). Read once + cached.
pub(crate) fn deadline_buffer_ms() -> u64 {
    static BUFFER: OnceLock<u64> = OnceLock::new();
    *BUFFER.get_or_init(|| {
        std::env::var("BROWSER_HEADLESS_DEADLINE_BUFFER_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10_000)
    })
}

/// Per-request cap on `/summary/batch` URL count
/// (`BROWSER_HEADLESS_MAX_BATCH_URLS`, default 100). Read once + cached.
pub(crate) fn max_batch_urls() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("BROWSER_HEADLESS_MAX_BATCH_URLS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(100)
    })
}

/// Admission control: how long a capture waits for a free browser-pool slot
/// before being shed with `503` (`BROWSER_HEADLESS_CHECKOUT_WAIT_MS`, default
/// 30_000). When concurrent demand exceeds pool capacity
/// (`POOL_SIZE × MAX_PAGES`) requests queue for a permit; this bounds that
/// queue wait so a saturated service fails fast instead of parking callers (and
/// their futures) indefinitely. `0` disables the bound — wait forever, the
/// original behaviour. Read once + cached.
pub(crate) fn checkout_wait_ms() -> u64 {
    static WAIT: OnceLock<u64> = OnceLock::new();
    *WAIT.get_or_init(|| {
        std::env::var("BROWSER_HEADLESS_CHECKOUT_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30_000)
    })
}

/// TCP port for the worker mode's health/metrics HTTP listener
/// (`BROWSER_HEADLESS_HEALTH_PORT`, default 3000). Only worker mode binds it —
/// serve / all already serve `/healthz`, `/readyz`, `/metrics` on the main API
/// port (3000). The `healthcheck` subcommand probes this port in worker mode.
/// Read once + cached.
pub(crate) fn health_port() -> u16 {
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        std::env::var("BROWSER_HEADLESS_HEALTH_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(3000)
    })
}
