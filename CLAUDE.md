# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A headless-Chrome HTTP service: POST a URL, get back a structured snapshot of
the rendered page (HTML/text/markdown, performance timings, every network
resource, JS exceptions, security audit, Core Web Vitals, optional
screenshot/PDF/HAR/DOM-snapshot). Built on **chromiumoxide** (CDP client) +
**axum** (HTTP), Rust 2024 edition.

## Commands

```bash
cargo build                 # debug build
cargo build --release       # production build
cargo run                   # run the server (Makefile: `make dev`) — needs Chrome, see below
cargo test                  # all tests (12 pure unit tests in browser.rs; no Chrome needed)
cargo test <name_substr>    # a single test by name
cargo fmt --all             # format (Makefile: `make fmt`)
cargo clippy --all-targets --all-features -- -D warnings   # the lint gate CI enforces
```

**CI (`.github/workflows/ci.yml`) gates every push/PR on three things:**
`cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test`. Run all three before pushing — `make lint` only runs bare
`cargo clippy` (no `-D warnings`), which is weaker than the gate.

**Running the service needs a Chrome/Chromium binary.** chromiumoxide reads
`$CHROME` first, else scans PATH. On macOS dev:
`CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" cargo run`.
The binary listens on `0.0.0.0:3000`.

The unit tests are pure functions (CSP/resource-summary/cookie parsing in
`browser.rs`) and do **not** launch Chrome — so `cargo test` passes without a
browser, but they cover almost nothing in `http.rs`/`capture.rs`/`pool.rs`. For
those layers, verify with a live smoke test: start the server and `curl` the
endpoints (`/readyz`, `/summary?url=...`, `POST /summary/batch`, `/metrics`).

## Architecture

Module layering is the thing to internalize — it was deliberately split so the
capture engine is reusable outside HTTP:

- **`main.rs`** — entrypoint only: builds the tokio runtime, then dispatches on
  `BROWSER_HEADLESS_MODE` — `serve` (default) runs the HTTP API, `worker` runs
  the Redis queue consumer (plus a health-only `/healthz`+`/readyz`+`/metrics`
  listener on `BROWSER_HEADLESS_HEALTH_PORT`, default 3000), `all` runs both in
  one process (worker as a background task) sharing one `CaptureCtx` / pool. A
  `healthcheck` argv subcommand (`browser-headless healthcheck`) does an
  internal `GET /healthz` and exits 0/1 — it's the container HEALTHCHECK, so
  the image needs no curl/wget.
- **`config.rs`** — env-backed knobs, each cached once (`default_timeout_ms`,
  `deadline_buffer_ms`, `max_batch_urls`, `checkout_wait_ms`, `health_port`).
- **`capture.rs`** — the **HTTP-agnostic capture core**. `CaptureCtx { pool,
  allow_private_ips }` is the only context it needs; `capture_one(&CaptureCtx,
  SummaryQuery) -> Captured` is the unit of work; `run_batch` fans out under
  the pool. Also owns `SummaryQuery` (the params DTO), the initial-URL SSRF /
  scheme pre-check (delegated to `ssrf`, fast-reject before pool checkout), and
  per-capture metrics. `capture_one` bounds the pool-slot wait by
  `checkout_wait_ms()` (admission control — 503 on saturation rather than an
  unbounded queue). **Depends only on `browser` + `pool` + `config` — never
  axum.** This is the reuse boundary: both `http.rs` and `worker.rs` call
  `capture::capture_one` without depending on each other.
- **`http.rs`** — the axum layer: `router()`, all handlers (`/summary`
  GET+POST, `/summary/batch`, `/healthz`, `/readyz`, `/metrics`), API-key auth,
  request-shape logging, and Prometheus recorder install. `AppState` embeds a
  `CaptureCtx`; `HealthState` (a `FromRef` sub-state, no API key) backs the
  probe/metrics routes, and `health_router()` exposes just those three for
  worker mode.
- **`pool.rs`** — `BrowserPool`: a fixed pool of N chromium instances.
  `checkout()` routes each request to the least-loaded active instance and is
  the concurrency gate (per-instance semaphore of `pages_per_instance`; total
  concurrency = `pool_size × pages_per_instance`). Each instance has its own
  manager task that respawns it on crash and recycles it (drain → replace
  subprocess) on request-count/age thresholds, serialized so at most one
  instance is unavailable at a time (zero-downtime recycle at `pool_size ≥ 2`).
- **`ssrf.rs`** — shared SSRF host-blocklist: IP classification (`is_blocked_*`)
  + a DNS-resolving URL check. Used by `capture` (initial-URL pre-check) **and**
  `browser` (re-checks every navigation/redirect hop). Leaf module — no axum, so
  `browser` can depend on it.
- **`worker.rs`** — queue-consumer mode (`BROWSER_HEADLESS_MODE=worker`).
  Works against single-node **or** Redis Cluster (`BROWSER_HEADLESS_REDIS_CLUSTER`)
  via a `Conn` enum that delegates `ConnectionLike` — all commands are single-key,
  so cluster routing is transparent.
  Reads jobs from a Redis Streams consumer group, runs `capture::capture_one` on
  the shared pool, writes results to `result:{id}` keys (TTL'd), and acks
  (at-least-once; `XAUTOCLAIM` reclaims jobs from crashed workers). A
  permit-bounded **continuous pipeline** keeps `pool capacity` captures in
  flight (a slow job frees its own slot — no batch-at-a-time stalls); a
  background sampler exports `worker_*` metrics (jobs / duration / in-flight /
  reclaimed / retries / XLEN / XPENDING). Transient capture failures
  (408/502/503/504) are retried up to `JOB_MAX_RETRIES`; on success it
  `PUBLISH`es the result to a `result:{id}`-named channel (`RESULT_NOTIFY`) so
  clients can block-wait instead of poll. On SIGTERM it stops pulling and drains
  in-flight jobs (`WORKER_DRAIN_MS`) before returning — `main.rs` hands a shared
  `watch` shutdown signal to both serve + worker, and `all` mode awaits the
  worker after the HTTP server drains. Never imports `http` — the health/metrics
  listener is wired up in `main.rs`; horizontal scaling = more worker processes.
- **`browser.rs`** — the CDP engine (~9k lines; the bulk of the codebase).
  `launch()` starts a Chromium and returns `(Browser, default_ua,
  disconnect_rx, UserDataDir)`; `capture()` runs the full pipeline
  (apply overrides → collect lifecycle/network/exceptions → wait gates →
  extract data → optional PDF/HAR/snapshot) and returns a `WebPageStat`. Every
  analytical feature (web vitals, security scan, resource summary, font/image
  audits, TLS inventory) lives here.

**Request flow:** `http` handler → `capture::capture_one(&ctx, q)` →
`pool.checkout()` → `browser::capture(&browser, ua, req)` → `WebPageStat` →
rendered as JSON / markdown / compact content object.

## Non-obvious gotchas (read these before touching the relevant code)

- **One Fetch interception handler, two jobs.** `apply_request_interception`
  (browser.rs) enables `Fetch` once and a single drain task does BOTH
  resource-type blocking and the SSRF redirect guard. Don't add a second
  `Fetch.enable` + `EventRequestPaused` listener — two handlers race to
  continue/fail the same `request_id` and trip CDP "invalid interception id".
  SSRF only checks `Document` requests (navigations + redirect hops) to bound
  DNS lookups; CDP re-pauses each redirect hop at the Request stage, so a
  blocked redirect fails before the request leaves the browser.
- **chromiumoxide flags omit leading dashes.** Pass `.arg("disable-dev-shm-usage")`,
  not `.arg("--disable-dev-shm-usage")` — chromiumoxide prepends `--` itself, so
  a literal `--flag` becomes `----flag` and is silently ignored. `--no-sandbox`
  must go through `.no_sandbox()`, not `.arg(...)`.
- **Each browser instance needs a unique `user_data_dir`.** chromiumoxide 0.9.1
  defaults *every* instance to one fixed `temp/chromiumoxide-runner`, so a
  second concurrent Chromium aborts on that profile's `SingletonLock`
  ("Failed to create a ProcessSingleton"). `launch()` assigns a unique temp dir
  per instance and returns a `UserDataDir` guard that removes it on
  recycle/shutdown. Without this, a pool of >1 cannot start.
- **chromiumoxide hardcodes a 30s timeout for discrete `page.execute` calls**
  (`CommandFuture`) regardless of `request_timeout`. Page-load waiting uses our
  own event-drain loop bounded by the request's `timeout_ms`, so slow loads are
  governed by `timeout_ms`, not the 30s cap.
- **Never name a module `metrics`** — it would shadow the `metrics` crate and
  break `metrics::counter!`. Metrics install lives in `http.rs`; `InFlightGuard`
  lives in `capture.rs`.
- **Per-capture metering is inside `capture_one`** (requests_total / duration /
  in-flight), so single `/summary` and each `/summary/batch` item are metered
  identically. Don't re-record in the HTTP wrapper.
- **The capture error type is `(StatusCode, String)`** end to end — transport-
  neutral, reused as the per-item status in batch results. `SummaryQuery.url` is
  `#[serde(default)]` so `BatchQuery` can `#[serde(flatten)]` the shared params
  without a top-level url.
- **The JSON envelope is never translated.** `lang` only affects
  `WebPageStat::to_markdown(lang)` prose; all field names / enum tag values stay
  English so downstream code that branches on them keeps working.

## Conventions

- **Import modules/items with `use`; never inline `crate::` paths in code.**
  Bring things in at the top (`use crate::config;` or
  `use crate::config::default_timeout_ms;`) and reference them by the short path
  (`config::default_timeout_ms()` / `default_timeout_ms()`). Do not write
  fully-qualified `crate::…` paths in expression position — that includes serde
  attribute strings, so `use` the function and reference its short name
  (`#[serde(default = "default_timeout_ms")]`). The `use crate::…` statements
  themselves are of course expected.
- **Backwards compatibility is a hard requirement** for config: new knobs
  default to the previous behavior (e.g. `POOL_SIZE=1` + recycling off reproduces
  the original single-instance service exactly).
- **`README.md` and `README_zh.md` are kept in sync** — any user-facing doc
  change (endpoints, params, env vars, metrics) goes in both. They hold the full
  parameter / env-var / metric tables; consult them rather than re-deriving.
- All tuning is via `BROWSER_HEADLESS_*` env vars (pool size, per-instance page
  cap, recycle thresholds, timeouts, batch cap, API key, SSRF toggle) — see the
  Configuration table in the README. `MAX_PAGES` is **per instance**, not global.
- Container caveat: Docker's default 64MB `/dev/shm` is too small under load;
  the image passes `--disable-dev-shm-usage`, but `docker run --shm-size=512m`
  is still recommended (more so with `POOL_SIZE > 1`).
