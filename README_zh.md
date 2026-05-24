# browser-headless

[English](README.md) · **简体中文**

一个 headless Chrome 的 HTTP 服务。传入 URL，一次请求拿回页面的结构化快照：
渲染后的 HTML / 纯文本 / Markdown、性能时间线、每条网络资源、JS 异常、
console 日志、Cookie，以及可选的截图 / PDF / HAR / DOM snapshot。

基于 [chromiumoxide](https://github.com/mattsse/chromiumoxide)（CDP 客户端）
+ [axum](https://github.com/tokio-rs/axum)（HTTP server）构建。

---

## 特性

- **多种输出格式** —— `html` / `markdown` / `text`。Markdown 用
  [htmd](https://github.com/letmutex/htmd) 配合一个 DOM walker 重写自定义
  元素（Taro / Web Component），保证输出适合喂给 LLM。
- **元素级裁切** —— `capture_element` 只返回某个元素的 outerHTML /
  innerText / markdown。
- **导航前覆盖** —— viewport（宽 / 高 / DPR）、user agent、accept-language、
  Cookie、自定义 HTTP header、时区、locale、地理位置、HTTP 缓存禁用、
  按 URL 屏蔽、**按资源类型屏蔽**（image/font/css/script 等，走 CDP `Fetch`
  拦截）、禁用 JS 执行、**触屏模拟**（移动端 touch 事件）、**CPU 节流**
  （低端机模拟）。
- **等待** —— 元素选择器(`wait_for_element`)、JS 谓词(`wait_for_function`)、
  网络响应(`wait_for_request`，多个)、固定 `settle_ms` 延迟、`script`
  自定义 JS。
- **完整快照** —— 每条资源的体积 / 状态 / 时间线 / mime / 缓存命中标记；
  JS 异常和 console 消息；cookie jar 里的全部 cookie；可选 PNG 截图(base64)、
  PDF (base64)、HAR 1.2 归档、CDP `DOMSnapshot`（带 layout / computed style
  的结构化快照）、**Core Web Vitals** 增强版（含 **LCP 元素身份** + 每次
  shift 的 **CLS 源** + 服务端预聚合的 top offenders）、**Page Metrics**
  （V8 heap + DOM 计数 + CPU 时间分解：script / layout / style / task
  累计时长）、**render-blocking head 资源识别**、**HTTP 安全头摘要**
  （CSP / HSTS / X-Frame-Options / ...）、**Service Worker 注册状态**、
  per-resource **请求 initiator**（parser / script + 源 URL + 行号）、
  服务端派生的 **资源汇总**（按 MIME bucket 的字节/数量、状态码分布、
  缓存命中率、第三方字节、最大单资源）。
- **响应封装** —— `format=json` 返回完整结构化数据；`format=markdown`
  返回适合 LLM 的 markdown 文档。
- **SSRF 防护** —— 拒绝非 http(s) 协议以及私有 / 回环 / 链路本地 / ULA /
  组播 IP（含云元数据 `169.254.169.254`），在占用 page 名额之前就快速失败。
  内网部署可通过环境变量关闭。
- **总体超时兜底** —— 整个 capture 流程硬上限 `timeout_ms + 10s`，超出
  返 504。
- **浏览器隔离** —— 每个请求一个全新的 CDP browser context（无痕模式），
  cookie / cache / localStorage 不会跨请求泄漏。
- **并发限流** —— `Browser.new_page` 外层用 semaphore 限流，防止 Chrome
  在高并发下 OOM（默认 8 个 page，可通过环境变量配置）。
- **API key 鉴权（可选）** —— 设置 `BROWSER_HEADLESS_API_KEY` 环境变量
  即可要求 `/summary` 携带 `X-Api-Key` header。默认关闭（开放访问）。
  `/healthz` 和 `/readyz` 永不校验，保证探针可用。
- **自愈** —— CDP 断开时 supervisor 任务按指数退避重启浏览器；期间
  in-flight 请求返 503，新浏览器就位后立即恢复。
- **健康探针** —— `/healthz` liveness、`/readyz` readiness（向 CDP 发
  `Browser.getVersion` 验证）。
- **优雅退出** —— SIGTERM / SIGINT 触发 axum 的 `with_graceful_shutdown`，
  in-flight 请求完成后才退出进程。
- **请求级 trace** —— 每个请求生成 `request_id`（参数 → `X-Request-ID`
  header → 自动生成 UUID），所有日志带上；每个阶段（`apply` / `collect` /
  `capture` / `format`）记录耗时。
- **GET + POST** —— 两种方法参数集完全相同。POST 走 JSON 体，适合带
  长 cookie / 多 header / 多行脚本的场景。

---

## 快速开始

### Docker

```bash
docker build -t browser-headless .
docker run --rm -p 3000:3000 --shm-size=512m browser-headless
```

### 从源码构建

```bash
cargo build --release
# 需要 PATH 里有 Chrome/Chromium，或通过 $CHROME 显式指定。
./target/release/browser-headless
```

服务监听 `0.0.0.0:3000`。

---

## API

### `GET /healthz`

Liveness —— HTTP server 在响应就返 `ok`。**不检查浏览器**。

### `GET /readyz`

Readiness —— 向 CDP 发 `Browser.getVersion`。浏览器可达且 supervisor
未在重启浏览器才返 `ok`，否则 503。

### `GET /summary` · `POST /summary`

主接口。两种方法参数集完全相同 —— GET 走 query string，POST 走 JSON
body（推荐用于参数较长的场景）。

#### 最小示例

```bash
curl 'http://localhost:3000/summary?url=https://example.com'
```

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com"}'
```

#### 参数

| 名称 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `url` | string | — | **必填**。目标 URL（仅 http/https）。 |
| `timeout_ms` | u64 | 30000 | 内部等待的软上限。整体硬上限 = `timeout_ms + 10s`。 |
| `screenshot` | bool | false | 截图（PNG）写入 `stat.screenshot`。 |
| `pdf` | bool | false | `Page.printToPDF` 写入 `stat.pdf`。 |
| `har` | bool | false | HAR 1.2 归档写入 `stat.har`（可在 Chrome DevTools 导入）。 |
| `save_dom_snapshot` | bool | false | `DOMSnapshot.captureSnapshot` 写入 `stat.dom_snapshot`。 |
| `web_vitals` | bool | false | 收集 Core Web Vitals（LCP / CLS / TBT / TTFB / 长任务计数）写入 `stat.web_vitals`，通过导航前安装的 `PerformanceObserver` 实现。 |
| `data_format` | `html`\|`markdown`\|`text` | `html` | `stat.data` 字段的格式。 |
| `format` | `json`\|`markdown` | `json` | 响应封装格式。`markdown` 把整个 `WebPageStat` 渲染成 LLM 可读的文档。 |
| `normalize_custom_elements` | bool | true | （仅 markdown 模式生效）把自定义元素（如 `taro-view-core`）按 computed `display` 重写成 `<div>` / `<span>`。 |
| `width` / `height` | u32 | 1920 / 1080 | viewport 尺寸（width / height / DPR 任一字段都会触发 override）。 |
| `device_scale_factor` | f64 | 1.0 | 设备像素比。 |
| `touch` | bool | false | 启用移动端 touch 事件模拟（`navigator.maxTouchPoints=5`，`ontouchstart` 可派发）。配合小 viewport 是完整移动端模拟。 |
| `cpu_throttle` | f64 | — | CPU 节流倍率（1.0 = 原生，4.0 = 4× 慢）。≤ 1.0 忽略。 |
| `user_agent` | string | — | UA 覆盖。 |
| `accept_language` | string | — | `Accept-Language` header 覆盖。仅设此字段不设 `user_agent` 时，UA 用启动时缓存的浏览器原生 UA。 |
| `cookie` | string | — | 标准 HTTP `Cookie` header 格式（`name=v; name2=v2`）。导航前写入。 |
| `headers` | `{ string: string }` | `{}` | 额外的 HTTP 请求 header。推荐用 POST JSON 传。 |
| `timezone` | string | — | IANA 时区 ID（如 `Asia/Shanghai`）。 |
| `locale` | string | — | BCP 47 locale（如 `zh-CN`）。 |
| `latitude` / `longitude` | f64 | — | 地理位置覆盖（必须同时提供两者）。 |
| `accuracy` | f64 | 100 | 地理位置精度（米）。 |
| `disable_cache` | bool | false | 每条请求都绕过磁盘 + 内存缓存。 |
| `disable_javascript` | bool | false | 仅渲染静态 HTML —— SPA 会是空白。静态站超快。 |
| `block_urls` | `[string]` | `[]` | 按 URL 子串（CDP 通配 `*pat*`）在网络层屏蔽。 |
| `block_resource_types` | `[string]` | `[]` | 按资源类型屏蔽。支持：`document` / `stylesheet`（`css`）/ `image`（`img`）/ `media`（`video`，`audio`）/ `font` / `script`（`js`）/ `xhr` / `fetch` / `websocket`（`ws`）/ `manifest` / `ping` / `other`。未识别静默忽略。走 CDP `Fetch` 拦截。 |
| `wait_for_element` | string | — | CSS 选择器 —— 阻塞直到该元素出现。 |
| `wait_for_function` | string | — | JS 表达式轮询，truthy 即通过。 |
| `wait_for_request` | `[string]` | `[]` | URL 子串列表 —— 阻塞直到**所有**匹配的响应都到（4xx/5xx → 502）。 |
| `settle_ms` | u64 | — | 所有等待之后、数据抓取之前的固定延迟。 |
| `script` | string | — | settle 之后、抓取之前执行的 JS。用于关弹窗、触发懒加载等。 |
| `capture_element` | string | — | CSS 选择器 —— `data` 只放该元素的内容。 |
| `request_id` | string | auto | 自定义 request ID 用于 trace 关联。 |

#### 执行顺序

`new_page("about:blank")` →
**apply**（viewport / UA / headers / timezone / locale / geolocation /
cookies / cache / block_urls / disable_js）→
**collect**（`goto` + lifecycle + network + exceptions + console）→
**capture**（`wait_for_element` → `wait_for_function` → `settle_ms` →
`script`）→
**format**（`data_format` × `capture_element` → 可选 PDF / HAR / DOM
snapshot）→ 关闭 page + 销毁 context。

每个阶段在 debug 级别记 `duration_ms`，方便定位慢请求。

#### 响应结构（JSON 封装）

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
    "lcp": 1234.5,
    "cls": 0.045,
    "tbt": 182.3,
    "ttfb": 156.0,
    "long_tasks": 3
  }
}
```

可选字段（`screenshot` / `pdf` / `har` / `dom_snapshot` / `web_vitals`）
不显式请求时为 `null`。

#### Markdown 封装（`format=markdown`）

返回 `text/markdown; charset=utf-8`，把所有字段渲染成自然语言：加载摘要、
异常列表、console 列表、**只列 cookie 名 + domain**（value 出于安全考虑
不展示，要完整 cookie 走 JSON）、每条资源一行简短描述、`data` 字段在
代码块里。

#### 错误码

| 状态码 | 触发条件 |
|---|---|
| 400 | URL 不合法 / 非 http(s) 协议 / DNS 解析失败 |
| 401 | 已设 `BROWSER_HEADLESS_API_KEY`，但请求缺少 / 错误 `X-Api-Key` |
| 403 | SSRF 守卫 —— URL 解析到被屏蔽的 IP |
| 404 | `capture_element` 选择器没匹配到元素 |
| 408 | 内部等待（`wait_for_element` / `wait_for_function`）超时 |
| 502 | `wait_for_request` 命中的某个 URL 返了 4xx/5xx |
| 503 | 浏览器在重启中，或 `Browser.getVersion` 失败（`/readyz`） |
| 504 | 总体硬上限（`timeout_ms` + 10s 缓冲）超时 |

---

## 配置

| 环境变量 | 默认值 | 作用 |
|---|---|---|
| `BROWSER_HEADLESS_API_KEY` | 未设置（开放） | 开启 API key 鉴权。设置后，`/summary` 必须带 `X-Api-Key: <value>` header，不匹配 / 缺失返 401。`/healthz` 和 `/readyz` 永远开放（保证探针可用）。建议使用高熵 key（≥32 随机字节）—— 这里是字节比对，不是 constant-time。 |
| `BROWSER_HEADLESS_MAX_PAGES` | 8 | 并发限流。超出的请求排队，handler 返回时释放 permit。 |
| `BROWSER_HEADLESS_ALLOW_PRIVATE_IPS` | 未设置 | 设为 `1` / `true` / `yes` / `on` 关闭 SSRF 守卫（允许私有 / 回环 / 链路本地 IP）。仅用于内网部署。 |
| `CHROME` | （自动探测） | Chrome / Chromium 可执行文件路径。Dockerfile 里设的是 `/usr/bin/chromium`。 |
| `RUST_LOG` | `info,chromiumoxide::conn=off,chromiumoxide::handler=off` | 标准 `tracing_subscriber` 过滤器。`browser_headless=debug` 可看到每阶段耗时。 |

---

## 部署提示

- **`--no-sandbox`**：启动 flag 硬编码以兼容容器环境。内网爬虫服务可
  接受；如要在多租户场景下接触不可信 URL，需要再叠一层外部沙箱
  （gVisor / kata）。
- **`--shm-size`**：Docker 默认 64MB `/dev/shm` 不够 Chrome 高负载使用。
  我们传了 `--disable-dev-shm-usage` 让 Chrome 回退到 `/tmp`，但
  `docker run --shm-size=512m` 还是更稳。
- **健康探针**：`/healthz`（liveness）+ `/readyz`（readiness）。k8s
  示例：
  ```yaml
  livenessProbe:
    httpGet: { path: /healthz, port: 3000 }
    periodSeconds: 10
  readinessProbe:
    httpGet: { path: /readyz, port: 3000 }
    periodSeconds: 5
    initialDelaySeconds: 5
  ```
- **优雅退出**：`docker stop` 发 SIGTERM，会等 in-flight 请求完成
  （docker stop timeout 内）。如长抓取可能超过默认 10s，加
  `--stop-timeout=60`。
- **DNS rebinding 残留风险**：SSRF 守卫在请求入口做一次 DNS 解析。
  chromium 在导航时会自己再解析一次 —— 攻击者可在两次解析之间换成
  内网 IP 绕过。高安全场景需配合出口防火墙 / 专用代理。
- **HAR 限制**：未捕获 `Network.requestWillBeSent` 的 payload，所以
  HAR 条目的请求 method 默认为 `GET`，headers/cookies 为空数组，
  未观测到的 timing 字段为 `-1`。够用于资源列表 / 状态码可视化，
  但不是完整的 Chrome DevTools recording。

---

## 示例

### LLM 友好抓取

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

### 会话续期

```bash
# 1. 登录
LOGIN=$(curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://app.example.com/login",
    "script": "document.querySelector(\"#u\").value=\"alice\"; document.querySelector(\"#p\").value=\"secret\"; document.querySelector(\"form\").submit();",
    "wait_for_request": ["api/login"],
    "settle_ms": 300
  }')

# 2. 复用 cookies
COOKIE=$(echo "$LOGIN" | jq -r '.cookies | map("\(.name)=\(.value)") | join("; ")')
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg c "$COOKIE" '{
    url: "https://app.example.com/profile",
    cookie: $c,
    data_format: "text"
  }')"
```

### 时区 / locale 模拟

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

### 长页面 PDF 归档

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

### 等待 SPA 数据后抓取

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

### 移动端模拟 + Web Vitals

完整移动端模拟（viewport + DPR + touch + UA + locale）叠加低端机 CPU
节流和 Core Web Vitals —— 真实低端手机用户的体验。

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

输出：
```json
{
  "vitals": { "lcp": 2845, "cls": 0.082, "tbt": 510, "ttfb": 320, "long_tasks": 7 },
  "total_size": 487234,
  "load_time": 3120
}
```

不开 `cpu_throttle` 跑同样请求做对比，TBT 和 LCP 会有 3–5× 差距，符合
真实低端 Android 用户的体验 —— 在影响真实用户之前抓 Web Vitals 回归。

---

## License

[Apache License 2.0](LICENSE)
