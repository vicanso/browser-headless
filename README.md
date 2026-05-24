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
  cache flag; JS exceptions + console messages; cookies in the jar;
  optional screenshot (PNG base64), PDF (base64), HAR 1.2 archive, CDP
  `DOMSnapshot` (structured layout + computed styles), **Core Web Vitals**
  enriched with **LCP element identity** + per-shift **CLS sources**
  (with pre-aggregated top offenders), **page metrics** (V8 heap + DOM
  counts + CPU time breakdown: script/layout/style/task durations),
  **render-blocking head resource detection**, **security response
  headers** (CSP / HSTS / X-Frame-Options / ...), **Service Worker**
  registration state, per-resource **request initiator** (parser / script
  + source URL + line number), and a server-derived **resource summary**
  (bytes & count by MIME bucket, status distribution, cache hit ratio,
  third-party bytes, largest single resource).
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
| `web_vitals` | bool | false | Collect Core Web Vitals (LCP / CLS / TBT / TTFB / long-task count) into `stat.web_vitals` via `PerformanceObserver` installed pre-navigation. Also records `lcp_element` (tag / id / class / url / text) and `cls_entries[]` + server-aggregated `cls_top_sources[]` for attribution. |
| `metrics` | bool | false | V8 heap + DOM counts + CPU time breakdown (`script_duration_ms` / `layout_duration_ms` / `recalc_style_duration_ms` / `task_duration_ms`) into `stat.metrics` via `Performance.getMetrics`. Gold for "LCP unchanged but script time +30%" regressions. |
| `metadata` | bool | false | Page `<head>` metadata (title / description / canonical / robots / lang / viewport / charset / theme-color / OG / Twitter) into `stat.metadata`. Catches SEO regressions instantly. |
| `render_blocking` | bool | false | Scan `<head>` for render-blocking sync stylesheets and scripts without `async`/`defer`/`module`; result in `stat.render_blocking_resources[]`. |
| `service_worker` | bool | false | Snapshot `navigator.serviceWorker` registration into `stat.service_worker` (controlled / scope / active_script / waiting / installing). |
| `initiators` | bool | false | Subscribe to `Network.requestWillBeSent` and attach per-resource `initiator` (type / url / line_number) — answers "what code triggered this request". |
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
  "fcp_time": 234,
  "dcl_time": 567,
  "load_time": 1234,
  "data": "<html>...</html>",
  "exceptions": ["42:15 ReferenceError: foo is not defined"],
  "console_messages": ["[log] hello world", "[error] api failed"],
  "resources": [
    {
      "content_size": 12345,
      "request_id": "9024.3",
      "status": 200,
      "url": "https://example.com/app.js",
      "mime_type": "application/javascript",
      "connection_reused": true,
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
    "largest_resource": ["https://cdn.example.com/vendors.js", 712600]
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
  "service_worker": {
    "controlled": true, "scope": "https://example.com/",
    "active_script": "https://example.com/sw.js",
    "waiting": false, "installing": false
  }
}
```

(`resources[].initiator` is also populated when `initiators=true`:
`{ "type": "parser" | "script" | "preload" | ..., "url": "...", "line_number": 12 }`.)

Optional fields (`screenshot`, `pdf`, `har`, `dom_snapshot`) are `null`
unless explicitly requested.

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
    "web_vitals": true,
    "metrics": true,
    "metadata": true,
    "render_blocking": true,
    "service_worker": true,
    "initiators": true,
    "settle_ms": 500
  }'
```

You get back a single markdown document with sections for: load summary,
exceptions, console messages, cookies, resources, web vitals + LCP element
+ top CLS offenders, security headers, service worker, page metrics with
CPU time breakdown, render-blocking head resources, page metadata, and a
resource summary (bytes by type + status distribution + cache hit ratio +
third-party bytes + largest resource). Store snapshots over time and diff
to catch:

- `lcp_element` changed → image broken → fallback rendered
- `cls_top_sources[0].selector` changed → new layout offender
- `metrics.script_duration_ms` +30% with LCP unchanged → JS regression
- `security_headers["Content-Security-Policy"]` missing → security regression
- `metadata.robots` contains `noindex` unexpectedly → SEO catastrophe
- New entries in `render_blocking_resources` → perf regression
- `service_worker.controlled` flipped to false → PWA broken
- New third-party `resources[].url` whose `initiator.url` points at a
  legitimate first-party script → know which library brought the tracker in

---

## License

[Apache License 2.0](LICENSE)
