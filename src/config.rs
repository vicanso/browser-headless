//! Process-wide configuration knobs, each resolved once from the environment
//! and cached (env is fixed for the process lifetime). Shared by the HTTP
//! layer, capture core, pool, and worker.

use std::sync::OnceLock;
use std::time::Duration;

// ─── shared env helpers ─────────────────────────────────────────────────────

pub(crate) fn env_string(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub(crate) fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

pub(crate) fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}

pub(crate) fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ─── cached knobs ───────────────────────────────────────────────────────────

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

/// Hard upper bound on per-request `timeout_ms` (and, transitively, on the
/// total deadline). Callers that pass a larger value are clamped with a
/// warning log. Default 120_000 (2 min); set via
/// `BROWSER_HEADLESS_MAX_TIMEOUT_MS`. `0` disables the clamp (not
/// recommended for multi-tenant).
pub(crate) fn max_timeout_ms() -> u64 {
    static MAX: OnceLock<u64> = OnceLock::new();
    *MAX.get_or_init(|| env_u64("BROWSER_HEADLESS_MAX_TIMEOUT_MS", 120_000))
}

/// Hard upper bound on `settle_ms`. Default 30_000; `0` disables.
pub(crate) fn max_settle_ms() -> u64 {
    static MAX: OnceLock<u64> = OnceLock::new();
    *MAX.get_or_init(|| env_u64("BROWSER_HEADLESS_MAX_SETTLE_MS", 30_000))
}

/// Clamp `timeout_ms` to `[1, max_timeout_ms()]` when a max is configured.
/// Returns the effective value (may equal the input).
pub(crate) fn clamp_timeout_ms(requested: u64) -> u64 {
    let max = max_timeout_ms();
    if max == 0 {
        return requested.max(1);
    }
    requested.clamp(1, max)
}

/// Clamp `settle_ms` when a max is configured.
pub(crate) fn clamp_settle_ms(requested: u64) -> u64 {
    let max = max_settle_ms();
    if max == 0 {
        return requested;
    }
    requested.min(max)
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

/// Max HTTP request body size in bytes for POST `/summary` and
/// `/summary/batch` (`BROWSER_HEADLESS_MAX_BODY_BYTES`, default 2 MiB).
/// Protects against oversized cookie/script/header payloads.
pub(crate) fn max_body_bytes() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| env_usize("BROWSER_HEADLESS_MAX_BODY_BYTES", 2 * 1024 * 1024).max(1024))
}

/// Process-wide request rate limit in requests/second
/// (`BROWSER_HEADLESS_RATE_LIMIT_RPS`, default 0 = off). Applies to
/// `/summary*` and `/jobs*` only; health/metrics are unrestricted.
pub(crate) fn rate_limit_rps() -> u32 {
    static RPS: OnceLock<u32> = OnceLock::new();
    *RPS.get_or_init(|| env_u64("BROWSER_HEADLESS_RATE_LIMIT_RPS", 0) as u32)
}

/// Burst capacity for the rate limiter (`BROWSER_HEADLESS_RATE_LIMIT_BURST`,
/// default = max(rps, 1) when rps > 0).
pub(crate) fn rate_limit_burst() -> u32 {
    static BURST: OnceLock<u32> = OnceLock::new();
    *BURST.get_or_init(|| {
        let rps = rate_limit_rps();
        let default = rps.max(1);
        env_u64("BROWSER_HEADLESS_RATE_LIMIT_BURST", default as u64) as u32
    })
}

/// When true, reject requests that include a `script` param
/// (`BROWSER_HEADLESS_DISABLE_SCRIPT`, default false). Multi-tenant
/// deployments should enable this unless callers are fully trusted.
pub(crate) fn disable_script() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("BROWSER_HEADLESS_DISABLE_SCRIPT", false))
}

/// When true, refuse to start in serve/all mode without
/// `BROWSER_HEADLESS_API_KEY` (`BROWSER_HEADLESS_REQUIRE_API_KEY`, default
/// false).
pub(crate) fn require_api_key() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("BROWSER_HEADLESS_REQUIRE_API_KEY", false))
}

/// When true, `/metrics` requires the same `X-Api-Key` as the API
/// (`BROWSER_HEADLESS_PROTECT_METRICS`, default false). Health probes stay
/// open so k8s liveness keeps working.
pub(crate) fn protect_metrics() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("BROWSER_HEADLESS_PROTECT_METRICS", false))
}

/// Log format: `text` (default) or `json` (`BROWSER_HEADLESS_LOG_FORMAT`).
pub(crate) fn log_format() -> LogFormat {
    static V: OnceLock<LogFormat> = OnceLock::new();
    *V.get_or_init(|| {
        match std::env::var("BROWSER_HEADLESS_LOG_FORMAT")
            .ok()
            .as_deref()
            .map(str::trim)
            .unwrap_or("text")
        {
            "json" | "JSON" => LogFormat::Json,
            _ => LogFormat::Text,
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogFormat {
    Text,
    Json,
}

/// TTL for in-process async job results (`BROWSER_HEADLESS_JOB_TTL_SECS`,
/// default 3600).
pub(crate) fn job_ttl() -> Duration {
    static V: OnceLock<Duration> = OnceLock::new();
    *V.get_or_init(|| Duration::from_secs(env_u64("BROWSER_HEADLESS_JOB_TTL_SECS", 3600).max(1)))
}

/// Max concurrent in-process async jobs (`BROWSER_HEADLESS_MAX_ASYNC_JOBS`,
/// default = pool will bound captures; this caps the job map size, default
/// 256).
pub(crate) fn max_async_jobs() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| env_usize("BROWSER_HEADLESS_MAX_ASYNC_JOBS", 256).max(1))
}

/// Whether the in-process async job API is enabled
/// (`BROWSER_HEADLESS_ASYNC_JOBS`, default true). Set `false` to hide
/// `/jobs` routes entirely.
pub(crate) fn async_jobs_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| env_bool("BROWSER_HEADLESS_ASYNC_JOBS", true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_timeout_respects_zero_max_as_unlimited() {
        // max is process-global OnceLock — we can only assert the helper math
        // via the public clamp with whatever is configured. When max is the
        // default 120_000, values above clamp down.
        let clamped = clamp_timeout_ms(u64::MAX);
        let max = max_timeout_ms();
        if max == 0 {
            assert_eq!(clamped, u64::MAX);
        } else {
            assert_eq!(clamped, max);
        }
    }

    #[test]
    fn clamp_timeout_floors_at_one() {
        assert_eq!(clamp_timeout_ms(0), 1);
    }

    #[test]
    fn clamp_settle_caps() {
        let max = max_settle_ms();
        if max > 0 {
            assert_eq!(clamp_settle_ms(max + 1), max);
        }
    }
}
