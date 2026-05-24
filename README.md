# browser-headless

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
  geolocation, HTTP cache disable, URL blocking, JS execution disable.
- **Waits** — element selector (`wait_for_element`), JS predicate
  (`wait_for_function`), network responses (`wait_for_request`, multiple),
  fixed `settle_ms`, custom JS via `script`.
- **Full snapshot** — every resource with size / status / timing / mime /
  cache flag; JS exceptions + console messages; cookies in the jar;
  optional screenshot (PNG base64), PDF (base64), HAR 1.2 archive, and CDP
  `DOMSnapshot` (structured layout + computed styles).
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
| `data_format` | `html`\|`markdown`\|`text` | `html` | Format of `stat.data` field. |
| `format` | `json`\|`markdown` | `json` | Response envelope. `markdown` renders the whole `WebPageStat` for LLM use. |
| `normalize_custom_elements` | bool | true | (Markdown only) Rewrite custom elements (`taro-view-core`, etc.) to `<div>`/`<span>` based on computed `display`. |
| `width` / `height` | u32 | 1920 / 1080 | Viewport size (any of width/height/DPR triggers override). |
| `device_scale_factor` | f64 | 1.0 | Device pixel ratio. |
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
  "dom_snapshot": { "documents": [], "strings": [] }
}
```

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

---

## License

[Apache License 2.0](LICENSE)
