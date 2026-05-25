# browser-headless

**English** · [简体中文](README_zh.md)

A headless-Chrome HTTP service. Send a URL, get back a structured snapshot
of the page: rendered HTML/text/markdown, performance timings, every
network resource, JS exceptions, console logs, cookies, optional screenshot
/ PDF / HAR / DOM snapshot — all in one request.

Built on [chromiumoxide](https://github.com/mattsse/chromiumoxide) (CDP
client) + [axum](https://github.com/tokio-rs/axum) (HTTP server).

---

## Features

- **Multiple output formats** — `html`, `markdown`, `text`. Markdown uses
  [htmd](https://github.com/letmutex/htmd) plus a DOM walker that rewrites
  custom elements (Taro / web components) so the result reads naturally for
  LLM consumption.
- **Element scoping** — `capture_element` returns just one element's
  outerHTML / innerText / markdown.
- **Pre-navigation overrides** — viewport (width / height / DPR), user
  agent, accept-language, cookies, extra HTTP headers, timezone, locale,
  geolocation, HTTP cache disable, URL blocking, **resource-type
  blocking** (image/font/css/script/etc. via CDP `Fetch` interception),
  JS execution disable, **touch emulation** (mobile-style touch events),
  **CPU throttling** (low-end-device simulation).
- **Waits** — element selector (`wait_for_element`), JS predicate
  (`wait_for_function`), network responses (`wait_for_request`, multiple),
  fixed `settle_ms`, custom JS via `script`.
- **Full snapshot** — every resource with size / status / timing / mime /
  cache flag; JS exceptions; cookies in the jar; optional console
  messages, screenshot (PNG base64), PDF (base64), HAR 1.2 archive, CDP
  `DOMSnapshot` (structured layout + computed styles), **Core Web Vitals**
  enriched with **LCP element identity**, per-shift **CLS sources** (with
  pre-aggregated top offenders), **INP** (2024 Core Web Vital replacing
  FID), and **Long Animation Frames** (Chrome 123+ jank attribution with
  per-script source URL + forced-reflow flag), **page metrics** (V8 heap + DOM
  counts + CPU time breakdown: script/layout/style/task durations),
  **render-blocking head resource detection**, per-resource **request
  initiator** (parser / script + source URL + line number), and a
  server-derived **resource summary** (bytes & count by MIME bucket,
  status distribution, cache hit ratio, third-party bytes, largest
  single resource, **HTTP version distribution** h1/h2/h3,
  **compression audit** with missed-opportunity bytes for text
  resources without `Content-Encoding`, **connection reuse**
  vs new-handshake counts, and **unique-host** count as DNS-lookup
  proxy).
- **Security audit** — **HTTP security headers** (CSP / HSTS / X-Frame-
  Options / ...) from the main document; **TLS / certificate info** of
  the landing page (protocol, cipher, issuer, subject, SAN list,
  expiry-day countdown) plus the **resolved remote IP / port** the
  browser actually connected to (DNS-resolution + cert pinning diff);
  **per-host TLS certificate inventory** across all HTTPS resources
  (CDNs / fonts / analytics) sorted by soonest expiry — catches
  third-party cert expiry before it breaks the page; **Service Worker**
  registration state.
- **Render diagnostics (opt-in)** — **DOM mutation hotspots** via a
  pre-navigation `MutationObserver` that counts childList /
  attribute mutations during the full render window with top-N tag /
  attribute breakdowns — diagnoses render-thrash regressions in SPAs.
  **Image sizing audit** compares each `<img>`'s decoded natural size
  vs laid-out display size (DPR-corrected, so retina-optimised images
  aren't false-flagged) and joins with the network response to surface
  bandwidth-wasting oversized images.
- **Response envelope** — `format=json` for full structured access, or
  `format=markdown` for an LLM-friendly rendered document.
- **SSRF guard** — rejects non-http(s) schemes and private / loopback /
  link-local / ULA / multicast IPs (incl. cloud metadata `169.254.169.254`)
  before even acquiring a page slot. Disabled via env var for internal
  scraping.
- **Total deadline** — hard upper bound `timeout_ms + 10s` around the
  whole capture; over the line returns 504.
- **Browser isolation** — every request runs in a fresh CDP browser
  context (incognito), so cookies / cache / localStorage never leak
  across requests.
- **Concurrency limit** — bounded semaphore around `Browser.new_page`
  prevents Chrome OOM under load (default 8 pages, env-configurable).
- **API key auth (opt-in)** — set `BROWSER_HEADLESS_API_KEY` env var to
  require `X-Api-Key` header on `/summary`. Default: disabled (open).
  `/healthz` and `/readyz` are always open so probes work.
- **Self-healing** — on CDP disconnect a supervisor task respawns the
  browser with exponential backoff; in-flight requests see 503 until
  the new browser is ready.
- **Health probes** — `/healthz` for liveness, `/readyz` for readiness
  (sends `Browser.getVersion` over CDP).
- **Graceful shutdown** — SIGTERM / SIGINT trigger axum's
  `with_graceful_shutdown`; in-flight requests finish before exit.
- **Request tracing** — every request gets a `request_id` (caller param
  → `X-Request-ID` header → auto UUID) attached to all log lines, with
  per-stage duration (`apply` / `collect` / `capture` / `format`).
- **GET + POST** — same parameter set on both. POST takes JSON, ideal
  for long cookies / many headers / multi-line scripts.

---

## Quick start

### Docker

```bash
docker build -t browser-headless .
docker run --rm -p 3000:3000 --shm-size=512m browser-headless
```

### Build from source

```bash
cargo build --release
# Requires a Chrome/Chromium binary in PATH or pointed at via $CHROME.
./target/release/browser-headless
```

The server listens on `0.0.0.0:3000`.

---

## API

### `GET /healthz`

Liveness — returns `ok` if the HTTP server is responding. Does not check
the browser.

### `GET /readyz`

Readiness — sends `Browser.getVersion` over CDP. Returns `ok` only if the
browser is reachable AND the supervisor is not respawning. Returns 503
otherwise.

### `GET /summary` · `POST /summary`

The main endpoint. Same parameter set on both — GET uses query string,
POST takes a JSON body (preferred for long payloads).

#### Minimal example

```bash
curl 'http://localhost:3000/summary?url=https://example.com'
```

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com"}'
```

#### Parameters

| Name | Type | Default | Description |
|---|---|---|---|
| `url` | string | — | **Required.** Target URL (http/https only). |
| `timeout_ms` | u64 | 30000 | Internal soft cap for waits. Hard total cap = `timeout_ms + 10s`. |
| `screenshot` | bool | false | Capture PNG screenshot into `stat.screenshot`. |
| `pdf` | bool | false | Capture PDF via `Page.printToPDF` into `stat.pdf`. |
| `har` | bool | false | Emit HAR 1.2 archive into `stat.har` (importable into Chrome DevTools). |
| `save_dom_snapshot` | bool | false | Capture `DOMSnapshot.captureSnapshot` into `stat.dom_snapshot`. |
| `web_vitals` | bool | false | Collect Core Web Vitals (LCP / CLS / TBT / TTFB / long-task count) into `stat.web_vitals` via `PerformanceObserver` installed pre-navigation. Also records `lcp_element` (tag / id / class / url / text), `cls_entries[]` + server-aggregated `cls_top_sources[]`, **INP** (max interaction duration — 2024 Core Web Vital; `0` in non-interactive scrapes, meaningful when `script` simulates clicks), and **Long Animation Frames** (Chrome 123+: `loaf_count` + `loaf_total_blocking_duration` + server-aggregated `loaf_top_offenders[]` ranked by attributable script source — pinpoints which JS file is causing jank, with forced-reflow flag). |
| `metrics` | bool | false | V8 heap + DOM counts + CPU time breakdown (`script_duration_ms` / `layout_duration_ms` / `recalc_style_duration_ms` / `task_duration_ms`) into `stat.metrics` via `Performance.getMetrics`. Gold for "LCP unchanged but script time +30%" regressions. |
| `metadata` | bool | false | Page `<head>` metadata (title / description / canonical / robots / lang / viewport / charset / theme-color / OG / Twitter) into `stat.metadata`. Catches SEO regressions instantly. |
| `render_blocking` | bool | false | Scan `<head>` for render-blocking sync stylesheets and scripts without `async`/`defer`/`module`; result in `stat.render_blocking_resources[]`. |
| `service_worker` | bool | false | Snapshot `navigator.serviceWorker` registration into `stat.service_worker` (controlled / scope / active_script / waiting / installing). |
| `initiators` | bool | false | Subscribe to `Network.requestWillBeSent` and attach per-resource `initiator` (type / url / line_number) — answers "what code triggered this request". |
| `console_messages` | bool | false | Collect `console.log/info/warn/error/debug` lines into `stat.console_messages`. Default off — console output is noisy (framework warnings, analytics, large object dumps); enable only when actually auditing console. When off the CDP `Runtime.consoleAPICalled` stream is never subscribed (zero cost). |
| `image_sizing` | bool | false | Per-`<img>` audit: decoded natural dimensions vs laid-out display dimensions (DPR-corrected so retina-tuned images aren't false-flagged), `loading` mode, viewport overlap, missing alt, server-joined `transferred_bytes` and computed `waste_ratio`. Output sorted worst-first into `stat.image_sizing`. One `evaluate` call, ~2ms even for 100+ images. |
| `dom_mutations` | bool | false | Install a pre-navigation `MutationObserver` and count DOM mutations (childList adds/removes + attribute changes) during the full render window. Output: `stat.dom_mutations` with totals + observation duration + top tags + top attributes. ≤5ms overhead even on heavy SPAs (counter-only, never stores raw records, `characterData` skipped). |
| `resources` | bool | false | Include the full per-resource list (`stat.resources[]`) in the response. Default off — for "did the page load OK" validation, scalar `total_size` + `resource_count` + aggregated `resource_summary` (bytes & count by MIME bucket, status distribution, cache hit ratio, third-party bytes + top third-party domains, modern-protocol share, compression breakdown by algorithm, Cache-Control coverage, largest resource) cover the signal at a fraction of the payload. Enable only when you need per-entry forensics (timing / mime / cache flag / cache_control header value / initiator). Internal collection is always on, so dependent features (HAR, `image_sizing.transferred_bytes`, `resource_summary`) keep working regardless. |
| `http_errors` | bool | false | Emit `stat.http_errors`: `failed_4xx[]` / `failed_5xx[]` lists, `network_failures[]` (DNS / TLS / connection-refused / blocked — sourced from CDP `Network.loadingFailed`), `final_url` after redirects, and `redirect_count`. Built for periodic health checks where the caller wants one focused "is this page broken / hijacked / redirected somewhere weird" signal without parsing `resources[]`. Subscribes to one extra CDP event stream when on; zero overhead when off. |
| `coverage` | bool | false | Capture CSS / JS coverage into `stat.coverage` — Lighthouse "Reduce unused CSS / JS" feed (per-file used / unused bytes + top-10 wasteful files). Enables CDP `Profiler.startPreciseCoverage` + `CSS.startRuleUsageTracking` pre-navigation; takes / stops both after load. **Explicitly NOT enabled by `all_metrics=true`** — coverage disables some V8 script optimisations and keeps style-engine state for the full load, so it stays per-request opt-in even when the caller asks for "every analytical signal". Set `coverage=true` explicitly. |
| `all_metrics` | bool | false | Convenience master switch that turns ON every **analytical** flag in one shot: `web_vitals` / `metrics` / `metadata` / `render_blocking` / `service_worker` / `initiators` / `console_messages` / `image_sizing` / `dom_mutations` / `resources` / `http_errors`. Designed for AI-comparison / regression-audit workflows where you want everything analysable. **Does NOT enable binary captures** (`screenshot` / `pdf` / `har` / `save_dom_snapshot`) or `coverage` — both have real per-request cost so they stay on explicit opt-in. OR-merged with individual flags, so anything already `true` stays `true`. |
| `data_format` | `html`\|`markdown`\|`text` | `html` | Format of `stat.data` field. |
| `format` | `json`\|`markdown` | `json` | Response envelope. `markdown` renders the whole `WebPageStat` for LLM use. |
| `normalize_custom_elements` | bool | true | (Markdown only) Rewrite custom elements (`taro-view-core`, etc.) to `<div>`/`<span>` based on computed `display`. |
| `width` / `height` | u32 | 1920 / 1080 | Viewport size (any of width/height/DPR triggers override). |
| `device_scale_factor` | f64 | 1.0 | Device pixel ratio. |
| `touch` | bool | false | Enable mobile-style touch event emulation (`navigator.maxTouchPoints=5`, `ontouchstart` dispatchable). Pair with small viewport for full mobile sim. |
| `cpu_throttle` | f64 | — | CPU slowdown multiplier (1.0 = native, 4.0 = 4× slower). Values ≤ 1.0 ignored. |
| `user_agent` | string | — | UA override. |
| `accept_language` | string | — | `Accept-Language` header override. Falls back to cached browser UA when set without `user_agent`. |
| `cookie` | string | — | Standard HTTP `Cookie` header format (`name=v; name2=v2`). Set before navigation. |
| `headers` | `{ string: string }` | `{}` | Extra HTTP request headers. Best supplied via POST JSON. |
| `timezone` | string | — | IANA tz id (`Asia/Shanghai`). |
| `locale` | string | — | BCP 47 locale (`zh-CN`). |
| `latitude` / `longitude` | f64 | — | Geolocation override (both required together). |
| `accuracy` | f64 | 100 | Geolocation accuracy in meters. |
| `disable_cache` | bool | false | Bypass disk + memory cache for every resource. |
| `disable_javascript` | bool | false | Render static HTML only — SPAs come back blank. Very fast for static sites. |
| `block_urls` | `[string]` | `[]` | URL substrings (CDP wildcard `*pat*`) blocked at the network layer. |
| `block_resource_types` | `[string]` | `[]` | Block by resource type. Recognized: `document` / `stylesheet` (`css`) / `image` (`img`) / `media` (`video`, `audio`) / `font` / `script` (`js`) / `xhr` / `fetch` / `websocket` (`ws`) / `manifest` / `ping` / `other`. Unknown names ignored. Uses CDP `Fetch` interception. |
| `wait_for_element` | string | — | CSS selector — block until element appears. |
| `wait_for_function` | string | — | JS expression polled until truthy. |
| `wait_for_request` | `[string]` | `[]` | URL substrings — block until **all** matching responses arrive (4xx/5xx → 502). |
| `settle_ms` | u64 | — | Fixed delay after all waits, before data capture. |
| `script` | string | — | JS evaluated after settle, before data capture. Use to dismiss modals, trigger lazy-load, etc. |
| `capture_element` | string | — | CSS selector — return only this element's content in `data`. |
| `request_id` | string | auto | Override the request ID for trace correlation. |

#### Execution order

`new_page("about:blank")` →
**apply** (viewport / UA / headers / timezone / locale / geolocation /
cookies / cache / block_urls / disable_js) →
**collect** (`goto` + lifecycle + network + exceptions + console) →
**capture** (`wait_for_element` → `wait_for_function` → `settle_ms` →
`script`) →
**format** (`data_format` × `capture_element` → optional PDF / HAR / DOM
snapshot) →
close page + dispose context.

Each stage logs `duration_ms` at debug level so slow requests are
trivially attributable.

#### Response shape (JSON envelope)

```json
{
  "total_size": 245678,
  "resource_count": 26,
  "fcp_time": 234,
  "dcl_time": 567,
  "load_time": 1234,
  "data": "<html>...</html>",
  "exceptions": ["42:15 ReferenceError: foo is not defined"],
  "js_exceptions": {
    "total": 1,
    "by_name": [
      { "name": "ReferenceError", "count": 1, "sample_message": "ReferenceError: foo is not defined" }
    ]
  },
  "console_messages": ["[log] hello world", "[error] api failed"],
  "resources": [
    {
      "content_size": 12345,
      "request_id": "9024.3",
      "status": 200,
      "url": "https://example.com/app.js",
      "mime_type": "application/javascript",
      "connection_reused": true,
      "protocol": "h2",
      "content_encoding": "br",
      "from_cache": false,
      "timing": { "request_time": 5.123, "dns_start": 0.1 }
    }
  ],
  "cookies": [
    {
      "name": "sid",
      "value": "abc",
      "domain": ".example.com",
      "path": "/",
      "expires": -1,
      "http_only": true,
      "secure": true,
      "same_site": "Lax"
    }
  ],
  "screenshot": { "data": "<base64 PNG>", "mime_type": "image/png" },
  "pdf": { "data": "<base64 PDF>", "mime_type": "application/pdf" },
  "har": { "log": { "version": "1.2" } },
  "dom_snapshot": { "documents": [], "strings": [] },
  "web_vitals": {
    "lcp": 1234.5, "cls": 0.045, "tbt": 182.3, "ttfb": 156.0, "long_tasks": 3,
    "lcp_element": {
      "tag": "img", "id": "hero", "class": "hero-image",
      "url": "https://cdn.example.com/hero.webp", "text_preview": null
    },
    "cls_entries": [
      { "time_ms": 850, "value": 0.032, "sources": [{"tag":"div","id":"","class":"ad-banner"}] }
    ],
    "cls_top_sources": [
      { "selector": "div.ad-banner", "total_shift": 0.032, "fraction": 0.71, "shift_count": 1 }
    ],
    "inp": 0, "interaction_count": 0,
    "loaf_count": 5, "loaf_total_blocking_duration": 312.5,
    "loaf_top_offenders": [
      {
        "source_url": "https://cdn.example.com/app.js",
        "source_function_name": "render",
        "invoker_type": "script",
        "total_duration_ms": 187.2,
        "total_forced_style_layout_ms": 42.1,
        "invocation_count": 3
      }
    ]
  },
  "metrics": {
    "js_heap_used": 12582912, "js_heap_total": 18874368,
    "documents": 1, "frames": 1, "nodes": 156, "js_event_listeners": 8,
    "script_duration_ms": 234.5, "layout_duration_ms": 45.2,
    "recalc_style_duration_ms": 12.8, "task_duration_ms": 312.1
  },
  "resource_summary": {
    "bytes_by_type": { "javascript": 720000, "image": 245000, "css": 35000 },
    "count_by_type": { "javascript": 8, "image": 5, "css": 3 },
    "status_distribution": { "2xx": 24, "4xx": 1 },
    "cache_hit_ratio": 0.32, "cached_bytes": 35000,
    "third_party_bytes": 18200,
    "top_third_party_domains": [
      { "host": "cdn.vendor.com", "bytes": 12400, "count": 3 },
      { "host": "analytics.example", "bytes": 5800, "count": 2 }
    ],
    "largest_resource": ["https://cdn.example.com/vendors.js", 712600],
    "protocol_distribution": { "h2": 18, "h3": 4, "http/1.1": 2 },
    "modern_protocol_share": 0.92,
    "compressed_count": 14,
    "compression_breakdown": { "br": 10, "gzip": 4, "none": 3 },
    "uncompressed_text_count": 3,
    "uncompressed_text_bytes": 84200,
    "cache_control_present": 22,
    "cache_control_missing": 3,
    "connections_reused": 19,
    "connections_new": 5,
    "unique_hosts": 6
  },
  "metadata": {
    "title": "Example", "description": "An example page",
    "canonical": "https://example.com/", "robots": "index, follow",
    "lang": "en", "viewport": "width=device-width, initial-scale=1",
    "charset": "UTF-8", "theme_color": "#1976d2",
    "og": { "title": "Example", "image": "..." },
    "twitter": { "card": "summary_large_image" }
  },
  "render_blocking_resources": [
    { "tag": "link", "url": "https://cdn.example.com/critical.css", "why": "sync stylesheet" },
    { "tag": "script", "url": "https://cdn.example.com/jquery.min.js", "why": "no async/defer" }
  ],
  "security_headers": {
    "Content-Security-Policy": "default-src 'self'",
    "Strict-Transport-Security": "max-age=31536000; includeSubDomains",
    "X-Frame-Options": "SAMEORIGIN",
    "X-Content-Type-Options": "nosniff"
  },
  "security_audit": {
    "headers": {
      "hsts": true, "csp": true, "csp_report_only": false,
      "x_frame_options": true, "x_content_type_options": true,
      "referrer_policy": false, "permissions_policy": false,
      "coop": false, "coep": false,
      "present_count": 4,
      "missing": ["Referrer-Policy", "Permissions-Policy", "Cross-Origin-Opener-Policy"]
    },
    "cookies": {
      "total": 3, "secure": 3, "http_only": 2,
      "same_site_set": 3, "same_site_none_without_secure": 0
    }
  },
  "coverage": {
    "js_total_bytes": 845000, "js_used_bytes": 312000,
    "js_unused_bytes": 533000, "js_unused_ratio": 0.63,
    "css_total_bytes": 42000, "css_used_bytes": 18000,
    "css_unused_bytes": 24000, "css_unused_ratio": 0.57,
    "top_unused": [
      { "url": "https://cdn.example.com/vendors.js", "kind": "js",
        "total_bytes": 712600, "used_bytes": 198000,
        "unused_bytes": 514600, "unused_ratio": 0.722 }
    ]
  },
  "service_worker": {
    "controlled": true, "scope": "https://example.com/",
    "active_script": "https://example.com/sw.js",
    "waiting": false, "installing": false
  },
  "tls_info": {
    "host": "example.com",
    "remote_ip": "203.0.113.42", "remote_port": 443,
    "protocol": "TLS 1.3", "cipher": "TLS_AES_128_GCM_SHA256",
    "key_exchange": null,
    "subject_name": "*.example.com", "issuer": "Let's Encrypt R3",
    "valid_from": 1705276800.0, "valid_to": 1713052800.0,
    "days_remaining": 45,
    "san_list": ["*.example.com", "example.com"]
  },
  "tls_certificates": [
    {
      "host": "fonts.gstatic.com",
      "remote_ip": "142.250.190.10", "remote_port": 443,
      "protocol": "TLS 1.3", "cipher": "TLS_AES_128_GCM_SHA256",
      "issuer": "WR2", "subject_name": "*.gstatic.com",
      "days_remaining": 67,
      "valid_from": 1705276800.0, "valid_to": 1719052800.0,
      "key_exchange": null, "san_list": ["*.gstatic.com"]
    }
  ],
  "image_sizing": [
    {
      "url": "https://example.com/hero.jpg",
      "natural_width": 3840, "natural_height": 2160,
      "display_width": 800, "display_height": 450,
      "device_pixel_ratio": 2.0,
      "loaded": true, "loading": "eager", "decoding": "auto",
      "in_viewport": true, "alt_missing": false,
      "transferred_bytes": 1843200, "waste_ratio": 0.83
    }
  ],
  "dom_mutations": {
    "total_added_nodes": 4521, "total_removed_nodes": 1203,
    "total_attribute_changes": 8932,
    "observation_window_ms": 3450,
    "top_tags_by_mutation_count": [
      { "name": "div", "count": 3201 },
      { "name": "span", "count": 1822 }
    ],
    "top_attributes_changed": [
      { "name": "class", "count": 4521 },
      { "name": "style", "count": 3201 }
    ]
  }
}
```

(`resources[].initiator` is also populated when `initiators=true`:
`{ "type": "parser" | "script" | "preload" | ..., "url": "...", "line_number": 12 }`.)

`tls_info` is the main document's certificate (always captured for HTTPS,
`null` for HTTP / file://). `tls_certificates` is the deduplicated cert
list across **all** HTTPS hosts the page contacted (including third-party
CDNs), sorted by `days_remaining` ascending — always present (empty list
for a fully-HTTP page).

Opt-in fields default to `null` until explicitly requested:
`screenshot`, `pdf`, `har`, `dom_snapshot`, `web_vitals`, `metrics`,
`metadata`, `render_blocking_resources`, `service_worker`, `image_sizing`,
`dom_mutations`, `http_errors`, `coverage`. `console_messages` and
`resources` default to empty array `[]` until `console_messages=true`
/ `resources=true` is set —— `resource_count` and `total_size`
(scalars) plus `resource_summary` (aggregates) are always emitted so
functional-validation callers don't need the detailed list.

`exceptions` and `js_exceptions` are always emitted (no opt-in): the
`Runtime.exceptionThrown` stream is always subscribed, so the cost of
the bucketed count is essentially zero. When no exceptions fire,
`exceptions: []` and `js_exceptions: { total: 0, by_name: [] }`. Use
`js_exceptions.total` as a single AI-/monitor-scannable scalar to spot
regressions like "today this page has 12 ReferenceErrors vs 0
yesterday"; `by_name` is the top-10 ranked breakdown with a sample
message per class.

`security_audit` is also always emitted (pure derive from
`security_headers` + `cookies`). It's a config-check scorecard:
`security_audit.headers.present_count` (0..=7) shows how many core
enforced headers (HSTS / CSP / X-Frame-Options / X-Content-Type-Options
/ Referrer-Policy / Permissions-Policy / Cross-Origin-Opener-Policy) are
present, `missing` names the gaps; `security_audit.cookies` reports
total cookies plus per-flag coverage (`secure` / `http_only` /
`same_site_set`) and the `same_site_none_without_secure` anti-pattern
counter (any non-zero value is a finding — modern browsers reject those
cookies). When the page sets no cookies and serves no security headers
the whole struct is zeroes/falses, which is itself a real signal.

#### Markdown envelope (`format=markdown`)

Returns `text/markdown; charset=utf-8` with the same fields rendered as
prose: load summary, exception list, console list, **cookie names + domains
only** (values redacted for safety; use JSON to get full values), per-
resource prose lines, and the `data` field in a fenced block.

#### Error codes

| Status | Cause |
|---|---|
| 400 | Invalid URL, non-http(s) scheme, DNS resolve failed |
| 401 | `BROWSER_HEADLESS_API_KEY` is set but request `X-Api-Key` header is missing or wrong |
| 403 | SSRF guard — URL resolves to a blocked IP |
| 404 | `capture_element` selector matched nothing |
| 408 | Internal wait (`wait_for_element` / `wait_for_function`) timed out |
| 502 | A `wait_for_request` URL came back 4xx/5xx |
| 503 | Browser respawning or `Browser.getVersion` failed (`/readyz`) |
| 504 | Total deadline (`timeout_ms` + 10s buffer) exceeded |

---

## Configuration

| Env var | Default | Effect |
|---|---|---|
| `BROWSER_HEADLESS_API_KEY` | unset (open) | Enables API key auth. When set, every `/summary` call must carry header `X-Api-Key: <value>`; mismatch / missing returns 401. `/healthz` and `/readyz` are always open so health probes work. Use a high-entropy key (≥32 random bytes); the check is byte-comparison, not constant-time. |
| `BROWSER_HEADLESS_MAX_PAGES` | 8 | Concurrency limit. Requests beyond this queue; permit released when handler returns. |
| `BROWSER_HEADLESS_ALLOW_PRIVATE_IPS` | unset | Set to `1` / `true` / `yes` / `on` to disable SSRF guard (allow private / loopback / link-local IPs). For internal deployments only. |
| `CHROME` | (auto-detect) | Path to the Chrome / Chromium binary. The provided Dockerfile sets `/usr/bin/chromium`. |
| `RUST_LOG` | `info,chromiumoxide::conn=off,chromiumoxide::handler=off` | Standard `tracing_subscriber` filter. Set `browser_headless=debug` to see per-stage timings. |

---

## Deployment notes

- **`--no-sandbox`**: launch flag is hardcoded for container compatibility.
  Acceptable for an internal scraping service; do not expose to untrusted
  URLs in a multi-tenant context without an outer sandbox (gVisor / kata).
- **`--shm-size`**: Docker's default 64MB `/dev/shm` is too small for
  Chrome under load. We pass `--disable-dev-shm-usage` to fall back to
  `/tmp`, but bumping shm with `docker run --shm-size=512m` is still safer.
- **Health probes**: see `/healthz` (liveness) and `/readyz` (readiness).
  Example k8s config:
  ```yaml
  livenessProbe:
    httpGet: { path: /healthz, port: 3000 }
    periodSeconds: 10
  readinessProbe:
    httpGet: { path: /readyz, port: 3000 }
    periodSeconds: 5
    initialDelaySeconds: 5
  ```
- **Graceful shutdown**: `docker stop` (SIGTERM) waits for in-flight
  requests up to the docker stop timeout. Use `--stop-timeout=60` if
  long-running captures may exceed the default 10s.
- **DNS rebinding caveat**: SSRF guard resolves the host once at request
  entry. Chromium re-resolves at navigation time; a malicious DNS server
  could return a private IP between the two. For high-stakes deployments
  combine with egress firewall / dedicated egress proxy.
- **HAR limitations**: we don't capture `Network.requestWillBeSent`
  payloads, so HAR entries have placeholder request method (`GET`),
  empty headers/cookies, and `-1` for timing phases that weren't observed.
  Sufficient for resource listing / status code visualization; not a full
  Chrome DevTools recording.

---

## Examples

### LLM-ready scraping

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://example.com/article",
    "wait_for_element": "article",
    "capture_element": "article",
    "data_format": "markdown",
    "format": "markdown",
    "block_urls": ["google-analytics", "doubleclick"]
  }'
```

### Session continuation

```bash
# 1. Log in
LOGIN=$(curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://app.example.com/login",
    "script": "document.querySelector(\"#u\").value=\"alice\"; document.querySelector(\"#p\").value=\"secret\"; document.querySelector(\"form\").submit();",
    "wait_for_request": ["api/login"],
    "settle_ms": 300
  }')

# 2. Reuse the cookies
COOKIE=$(echo "$LOGIN" | jq -r '.cookies | map("\(.name)=\(.value)") | join("; ")')
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg c "$COOKIE" '{
    url: "https://app.example.com/profile",
    cookie: $c,
    data_format: "text"
  }')"
```

### Geo-locale emulation

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://maps.example.com",
    "timezone": "Asia/Shanghai",
    "locale": "zh-CN",
    "latitude": 31.2304,
    "longitude": 121.4737,
    "accept_language": "zh-CN,zh;q=0.9"
  }'
```

### Long-page PDF archive

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://example.com/long-article",
    "width": 1280,
    "height": 4000,
    "pdf": true
  }' | jq -r '.pdf.data' | base64 -d > article.pdf
```

### Wait for SPA data then capture

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://taro-spa.example.com",
    "wait_for_request": ["gfmiddle/ecss/infotrade/rzt/list"],
    "wait_for_function": "document.querySelectorAll(\".item\").length >= 10",
    "capture_element": "#rzt-list",
    "data_format": "markdown",
    "format": "markdown"
  }'
```

### Mobile device simulation with Web Vitals

Full mobile emulation (viewport + DPR + touch + UA + locale) plus
low-end-device CPU throttling and Core Web Vitals — what a slow phone
user actually experiences.

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://m.example.com",
    "width": 390,
    "height": 844,
    "device_scale_factor": 3.0,
    "touch": true,
    "user_agent": "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
    "accept_language": "zh-CN,zh;q=0.9",
    "cpu_throttle": 4.0,
    "block_resource_types": ["font"],
    "web_vitals": true,
    "settle_ms": 500
  }' | jq '{ vitals: .web_vitals, total_size: .total_size, load_time: .load_time }'
```

Output:
```json
{
  "vitals": { "lcp": 2845, "cls": 0.082, "tbt": 510, "ttfb": 320, "long_tasks": 7 },
  "total_size": 487234,
  "load_time": 3120
}
```

Compared to the same call without `cpu_throttle` you'll see TBT and LCP
jump 3–5×, matching real low-end Android numbers — useful for catching
Web Vitals regressions before they hit production users.

### Regression / correctness baseline for AI comparison

Enable everything an LLM (or a diff job) needs to compare deploys:

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://example.com/critical-page",
    "format": "markdown",
    "all_metrics": true,
    "settle_ms": 500
  }'
```

(`all_metrics: true` is shorthand for enabling all ten analytical flags at
once — `web_vitals` / `metrics` / `metadata` / `render_blocking` /
`service_worker` / `initiators` / `console_messages` / `image_sizing` /
`dom_mutations` / `resources`. Binary captures like `screenshot` / `pdf`
stay separate.)

You get back a single markdown document with sections for: load summary,
exceptions, web vitals + LCP element + top CLS offenders, resource
summary, render-blocking head resources, TLS certificate + per-host
inventory, security headers, service worker, image sizing audit, page
metadata, page metrics with CPU time breakdown, DOM mutation hotspots,
resources list, cookies. Store snapshots over time and diff to catch:

- `lcp_element` changed → image broken → fallback rendered
- `cls_top_sources[0].selector` changed → new layout offender
- `metrics.script_duration_ms` +30% with LCP unchanged → JS regression
- `security_headers["Content-Security-Policy"]` missing → security regression
- `security_audit.headers.present_count` dropped vs baseline → a deploy
  stripped one of the core enforced headers; `security_audit.headers.missing`
  names exactly which
- `security_audit.cookies.same_site_none_without_secure > 0` → cookies
  that modern browsers reject outright; always actionable
- `security_audit.cookies.secure / total` ratio dropped → a new cookie was
  set without the `Secure` flag (likely a third-party script)
- `metadata.robots` contains `noindex` unexpectedly → SEO catastrophe
- New entries in `render_blocking_resources` → perf regression
- `service_worker.controlled` flipped to false → PWA broken
- New third-party `resources[].url` whose `initiator.url` points at a
  legitimate first-party script → know which library brought the tracker in
- `tls_info.days_remaining` < 30 → cert expiring soon; < 0 → already
  expired; any `tls_certificates[*]` close to expiry → third-party CDN
  cert risk before it breaks the page
- `tls_info.remote_ip` / `.issuer` changed unexpectedly → DNS hijack /
  CA migration / MITM signal
- `image_sizing[0].waste_ratio > 0.5 && in_viewport=true` → bandwidth
  waste in the above-the-fold critical path
- `dom_mutations.total_added_nodes` +N× vs baseline → render-thrash
  regression (framework downgrade, lost `key` optimization, etc.)
- `dom_mutations.top_attributes_changed` dominated by `style` with very
  high count → uncontrolled animation / transition triggering reflow
- `web_vitals.loaf_top_offenders[0].source_url` changed or moved up the
  list → JS jank source shifted; combined with `total_forced_style_layout_ms`
  flags layout-thrashing scripts directly
- `web_vitals.loaf_count` or `loaf_total_blocking_duration` jumped → new
  long animation frames during render (more precise than TBT)
- `web_vitals.inp` > 200ms with `interaction_count > 0` → degraded
  responsiveness on simulated interaction (`script` clicked something)
- `resource_summary.protocol_distribution["h2"]` ratio dropped → HTTP/2
  rollout regression; falling back to `http/1.1` triggers connection thrash
- `resource_summary.modern_protocol_share < 0.9` → HTTP/2+3 coverage gap
  in a single scalar — easier to alert on than the per-version histogram
- `resource_summary.uncompressed_text_bytes > 50_000` → text resources
  shipped without `Content-Encoding` — missed compression opportunity
- `resource_summary.compression_breakdown["none"]` rising vs baseline →
  some text responses lost their encoding header (CDN cache misconfig)
- `resource_summary.cache_control_missing` jumped after deploy → static
  assets shipped without caching directives; usually a new origin tier
  bypassing the CDN config
- `resource_summary.top_third_party_domains[0].bytes` jumped → heaviest
  external vendor grew; isolate before it dominates total payload
- `coverage.js_unused_ratio > 0.6` or `coverage.css_unused_ratio > 0.6`
  → significant dead code shipped to the client; `coverage.top_unused[]`
  names which files to trim first (only emitted when `coverage=true`,
  not implied by `all_metrics`)
- `resource_summary.connections_new` jumped vs baseline with
  `connections_reused` flat → connection-pool / HTTP-multiplexing broke
- `resource_summary.unique_hosts` ballooned → DNS-lookup overhead grew,
  often new third-party scripts were introduced

---

## License

[Apache License 2.0](LICENSE)
