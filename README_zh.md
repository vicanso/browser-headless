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
  自定义 JS，外加 `wait_until_load` gate 切换（在 `load` 事件 vs Chrome
  `networkIdle` 之间选择返回时机）。
- **完整快照** —— 每条资源的体积 / 状态 / 时间线 / mime / 缓存命中标记；
  JS 异常；cookie jar 里的全部 cookie；可选 console 消息、PNG 截图(base64)、
  PDF (base64)、HAR 1.2 归档、CDP `DOMSnapshot`（带 layout / computed style
  的结构化快照）、**Core Web Vitals** 增强版（含 **LCP 元素身份** + 每次
  shift 的 **CLS 源** + 服务端预聚合的 top offenders + **INP** 2024
  Core Web Vital（取代 FID）+ **Long Animation Frames** Chrome 123+
  的卡顿归因，按脚本源 URL 聚合 + forced reflow 标记）、**Page Metrics**
  （V8 heap + DOM 计数 + CPU 时间分解：script / layout / style / task
  累计时长）、**render-blocking head 资源识别**、per-resource
  **请求 initiator**（parser / script + 源 URL + 行号）、服务端派生的
  **资源汇总**（按 MIME bucket 的字节/数量、状态码分布、缓存命中率、
  第三方字节、最大单资源、**HTTP 版本分布** h1/h2/h3、**压缩审计**
  含可压缩文本资源未压缩的字节数、**连接复用** vs 新建握手次数、
  **唯一 host 数**（≈ DNS 查询次数））。
- **安全审计** —— 主文档的 **HTTP 安全头摘要**（CSP / HSTS /
  X-Frame-Options / ...）；落地页的 **TLS / 证书信息**（协议、cipher、
  CA issuer、subject、SAN 列表、到期天数倒计时）以及浏览器实际连接的
  **resolved remote IP / port**（DNS 解析 + 证书 pinning diff）；
  **per-host TLS 证书清单** —— 覆盖所有 HTTPS 资源（CDN / 字体 /
  统计），按最快到期排序，在第三方证书过期把页面打挂之前抓到；
  **Service Worker 注册状态**。
- **渲染诊断（按需启用）** —— 通过导航前注入的 `MutationObserver`
  收集 **DOM 突变热点**，覆盖完整渲染期的 childList / 属性 mutation
  并给出 top-N tag / attribute 分布 —— 用于诊断 SPA 渲染抖动回归。
  **图片尺寸审计**：每个 `<img>` 的解码原始尺寸 vs 实际布局尺寸（已
  考虑 DPR，retina 优化图不会误报），关联网络响应揭示首屏大图浪费。
- **响应封装** —— `format=json` 返回完整结构化数据；`format=markdown`
  返回适合 LLM 的 markdown 文档。
- **只取内容模式** —— `content_only=true` 返回紧凑的
  `{ status, final_url, char_count, data }`，正文按所选 `data_format`
  （`html` / `text` / `markdown`）给出，跳过所有分析信号与二进制采集。
  专为廉价的"页面是否真的渲染出来"检查设计：把 `status` 和非平凡的
  `char_count` 配对判断，或把 `data`（markdown）喂给 LLM 判断页面是否
  有效 —— 代价只是完整快照的一小部分。
- **SSRF 防护** —— 拒绝非 http(s) 协议以及私有 / 回环 / 链路本地 / ULA /
  组播 IP（含云元数据 `169.254.169.254`），在占用 page 名额之前就快速失败。
  内网部署可通过环境变量关闭。
- **总体超时兜底** —— 整个 capture 流程硬上限 `timeout_ms + buffer`
  （buffer 默认 10s，可经 `BROWSER_HEADLESS_DEADLINE_BUFFER_MS` 调整），
  超出返 504。
- **浏览器隔离** —— 每个请求一个全新的 CDP browser context（无痕模式），
  cookie / cache / localStorage 不会跨请求泄漏。
- **浏览器实例池 + 滚动回收** —— 固定大小的 chromium 进程池（
  `BROWSER_HEADLESS_POOL_SIZE`，默认 1）。每个请求路由到最闲的活跃实例，
  总并发 = `pool_size × pages_per_instance`。实例在服务一定请求数或达到一定
  年龄后回收（先排空、再换子进程），抑制 chromium 内存膨胀；`pool_size ≥ 2`
  时回收零停机（任意时刻最多 1 个实例不可用）。每个实例用独立 profile 目录，
  回收 / 退出时清理。默认值与原单实例行为完全一致。
- **并发限流** —— 每个实例外层用 semaphore 限流，防止 Chrome 在高并发下
  OOM（默认**每实例** 8 个 page，可通过环境变量配置）。超出 `pool_size ×
  pages_per_instance` 的请求排队。
- **API key 鉴权（可选）** —— 设置 `BROWSER_HEADLESS_API_KEY` 环境变量
  即可要求 `/summary` 携带 `X-Api-Key` header。默认关闭（开放访问）。
  `/healthz` 和 `/readyz` 永不校验，保证探针可用。
- **自愈** —— 每个实例有独立的 manager 任务，CDP 断开时按指数退避重启该实例；
  只影响该实例的 in-flight 请求，其它实例照常服务。
- **健康探针** —— `/healthz` liveness、`/readyz` readiness（向 CDP 发
  `Browser.getVersion` 验证）。
- **Prometheus 指标** —— `/metrics` 暴露请求数、延迟直方图、in-flight
  gauge、浏览器 respawn 计数，供抓取。
- **优雅退出** —— SIGTERM / SIGINT 触发 axum 的 `with_graceful_shutdown`，
  in-flight 请求完成后才退出进程。
- **请求级 trace** —— 每个请求生成 `request_id`（参数 → `X-Request-ID`
  header → 自动生成 UUID），所有日志带上；每个阶段（`apply` / `collect` /
  `capture` / `format`）记录耗时。
- **GET + POST** —— 两种方法参数集完全相同。POST 走 JSON 体，适合带
  长 cookie / 多 header / 多行脚本的场景。
- **批量端点** —— `POST /summary/batch` 一次请求抓 N 个 URL（共享参数模板），
  在池上统一排并发，返回逐 URL 的结果数组（单个坏 URL 不会让整批失败）。
  正适合「AI 批量判断一堆页面是否正常」。

---

## 快速开始

### Docker

```bash
docker run --rm -p 3000:3000 --shm-size=512m vicanso/browser-headless
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

### `GET /metrics`

Prometheus 文本格式指标。与健康探针一样开放、无需 `X-Api-Key`，方便
集群内 Prometheus 抓取；指标敏感时在网络层做限制。暴露：

| 指标 | 类型 | 标签 | 含义 |
|---|---|---|---|
| `browser_headless_requests_total` | counter | `status` | 按最终 HTTP 状态码统计的 `/summary` 请求数 |
| `browser_headless_request_duration_seconds` | histogram | `outcome`（`ok`/`error`） | `/summary` 端到端处理耗时 |
| `browser_headless_requests_in_flight` | gauge | — | 当前正在处理的请求数（≤ `pool_size × pages_per_instance` + 排队） |
| `browser_headless_pool_size` | gauge | — | 配置的 chromium 实例数 |
| `browser_headless_pool_active_instances` | gauge | — | 当前 `Active`（未在排空 / 重启）的实例数 |
| `browser_headless_browser_respawns_total` | counter | — | 崩溃实例被重启的次数 |
| `browser_headless_recycles_total` | counter | `reason`（`age`/`count`） | 主动回收实例的次数 |

`in_flight` 长期顶在 `pool_size × pages_per_instance` 上限 = 请求在并发
semaphore 上排队；`browser_respawns_total` 上升 = chromium 在崩溃并被自动
恢复；`pool_active_instances < pool_size` = 有实例正在回收中。

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
| `timeout_ms` | u64 | 30000 | 内部等待的软上限。整体硬上限 = `timeout_ms + buffer`（buffer 默认 10s，见 `BROWSER_HEADLESS_DEADLINE_BUFFER_MS`）。30000 这个默认值本身可经 `BROWSER_HEADLESS_DEFAULT_TIMEOUT_MS` 覆盖。 |
| `screenshot` | bool | false | 截图（PNG）写入 `stat.screenshot`。 |
| `pdf` | bool | false | `Page.printToPDF` 写入 `stat.pdf`。 |
| `har` | bool | false | HAR 1.2 归档写入 `stat.har`（可在 Chrome DevTools 导入）。 |
| `save_dom_snapshot` | bool | false | `DOMSnapshot.captureSnapshot` 写入 `stat.dom_snapshot`。 |
| `web_vitals` | bool | false | 收集 Core Web Vitals（LCP / CLS / TBT / TTFB / 长任务计数）写入 `stat.web_vitals`，通过导航前安装的 `PerformanceObserver` 实现。同时记录 `lcp_element`（tag / id / class / url / text **再加 `size` / `load_time` / `render_time` / `natural_width` / `natural_height`** —— AI 能直接说出"LCP 是 3840×2160 图被渲染成 1920×1080，980ms 加载、1023ms 上屏，应该换更小尺寸"），`cls_entries[]` 携带每个 source 的移动几何（`previous_rect` / `current_rect` / `distance_px`），加上服务端聚合的 `cls_top_sources[]`（新增 `max_distance_px` —— 单次最大跳动距离，就是预留 `min-height` 的下限），**`long_task_top_offenders[]`**（服务端按 `PerformanceLongTaskTiming.attribution[].container_src` 归类聚合 —— 把"long_tasks: 3"变成"3 个 longtask 共 800ms，全来自 gtm.js"这种可定位的归因），**INP**（最大交互响应时长，2024 Core Web Vital；纯抓取场景 `interaction_count == 0` 时为 `null`，只有 `script` 模拟点击时才是真实数值），**Long Animation Frames**（Chrome 123+：`loaf_count` + `loaf_total_blocking_duration` + 服务端聚合的 `loaf_top_offenders[]`，按脚本源 URL 归因，标记 forced reflow —— 直接定位是哪个 JS 文件在卡顿），以及 **FPS**（`fps_avg` / `fps_jank_ratio` / `fps_longest_frame_ms` / `fps_frame_count` —— 基于 rAF 循环、对照 60fps 基准；`jank_ratio` 和 `longest_frame_ms` 是动画 / 滚动密集型页面真正可行动的信号，能补 LoAF 漏掉的"亚 jank 阈值"平滑度损失，比如稳定 45fps 的 banner 动画。headless + VM 用软件光栅化，绝对数字不能跟真机比 —— 同 harness 做回归对比是 ok 的）。 |
| `metrics` | bool | false | V8 heap + DOM 计数 + CPU 时间分解（`script_duration_ms` / `layout_duration_ms` / `recalc_style_duration_ms` / `task_duration_ms`）写入 `stat.metrics`，通过 `Performance.getMetrics`。回归检测的金矿 ——「LCP 没变但 script 时长 +30%」一眼可见。 |
| `metadata` | bool | false | 抓 `<head>` 元数据（title / description / canonical / robots / lang / viewport / charset / theme-color / OG / Twitter）写入 `stat.metadata`。SEO 回归一眼可见。 |
| `render_blocking` | bool | false | 扫 `<head>` 找 render-blocking 同步 stylesheet 和 没 `async`/`defer`/`module` 的 script，结果在 `stat.render_blocking_resources[]`。 |
| `service_worker` | bool | false | 抓 `navigator.serviceWorker` 注册信息写入 `stat.service_worker`（controlled / scope / active_script / waiting / installing）。 |
| `initiators` | bool | false | 订阅 `Network.requestWillBeSent` 给每条 resource 加 `initiator` 信息（type / url / line_number），回答「这个请求是谁触发的」。 |
| `console_messages` | bool | false | 收集 `console.log/info/warn/error/debug` 写入 `stat.console_messages`。默认关闭 —— console 通常很吵（框架开发警告、统计脚本、大对象 dump），只在确实要做 console 审计时打开。关闭时根本不订阅 CDP `Runtime.consoleAPICalled`（零成本）。 |
| `image_sizing` | bool | false | 逐张 `<img>` 审计：解码后的原始尺寸 vs 实际布局尺寸（已按 DPR 修正，retina 优化图不会误报浪费）、`loading` 模式、是否在首屏、是否缺 `alt`、**`has_width_attr` / `has_height_attr` / `has_srcset`** 属性是否存在；服务端关联 `transferred_bytes` 并计算 `waste_ratio`。结果按浪费率降序写入 `stat.image_sizing`。同一遍派生出 `stat.image_audit` —— Lighthouse "图片四大件"（`oversized` / `missing_dimensions` / `missing_lazy` / `missing_srcset`），每项是预排序好的 top-20 列表，带具体 URL + 显示尺寸，AI 能按类别一行一条出建议。一次 `evaluate` 调用，~2ms（100+ 张图）。 |
| `dom_mutations` | bool | false | 导航前注入 `MutationObserver`，统计整个渲染期间的 DOM 变更（childList 增删 + 属性修改）。输出 `stat.dom_mutations`：总数 + 观测窗口 + top tags + top attributes。重度 SPA 也 ≤5ms 开销（只增不读、不存原始 records、跳过 `characterData`）。 |
| `resources` | bool | false | 是否在响应中包含完整的 `stat.resources[]` 列表。默认关闭 —— 功能校验（"页面是否正常加载"）只需要标量 `total_size` + `resource_count` + 聚合 `resource_summary`（按 MIME bucket 的字节/数量、状态码分布、缓存命中率、第三方字节 + top 3rd-party 域名、modern-protocol 占比、按算法的压缩分布、Cache-Control 覆盖率、最大资源），覆盖度足够。只在需要逐条 forensics（timing / mime / 缓存命中 / cache_control 头值 / initiator）时打开。内部始终采集，所以依赖 resources 的下游特性（HAR、`image_sizing.transferred_bytes`、`resource_summary`）不受影响。 |
| `http_errors` | bool | false | 输出 `stat.http_errors`：`failed_4xx[]` / `failed_5xx[]` 列表、`network_failures[]`（DNS / TLS / 连接拒绝 / 被拦截 —— 来自 CDP `Network.loadingFailed`）、跳转后的 `final_url`、`redirect_count`。专为定时健康巡检设计，给一个聚焦的"页面是否挂了 / 被劫持 / 跳到奇怪地方"信号，不用解析整个 `resources[]`。开启时多订阅一个 CDP 事件流；关闭时零开销。 |
| `coverage` | bool | false | 采集 CSS / JS coverage 输出到 `stat.coverage` —— Lighthouse "Reduce unused CSS / JS" 数据源（按文件统计 used / unused 字节 + top-10 浪费列表）。开启时导航前启用 CDP `Profiler.startPreciseCoverage` + `CSS.startRuleUsageTracking`，加载完后 take / stop。**`all_metrics=true` 也不会自动启用** —— coverage 会让 V8 关掉部分脚本优化、CSS 引擎全程保留 rule-usage 状态，所以即使开了 "所有分析信号" 也保持显式 opt-in。需要时单独设 `coverage=true`。 |
| `resource_hints` | bool | false | 审计 `<link rel="preconnect">` / `<link rel="dns-prefetch">` 声明与实际命中的第三方主机的差距。结果写入 `resource_summary.resource_hints`，包含 `declared_preconnect[]` / `declared_dns_prefetch[]` 以及 `gap[]` —— 实际加载量大但未声明 hint 的第三方主机列表（每个 = 一次可避免的 100–300ms DNS+TLS 开销）。多一次 `<head>` evaluate（约 5ms）。与 `all_metrics` OR 合并。 |
| `font_audit` | bool | false | 审计 `@font-face` 声明 + `document.fonts` 的 FOIT（Flash of Invisible Text，"文字加载期间不可见"）风险。结果写入 `stat.font_audit`，包含 `font-display` 取值分布、`missing_swap[]`（FOIT 罪魁列表 —— 每条对应一个 `font-display: swap;` 的具体修复）、`declared_preload_count`（标量 —— "你到底有没有 preload 字体"）、`unreadable_stylesheets`（CORS 盲区计数 —— 跨域 stylesheet 没加 `crossorigin` 就读不到 cssRules，把这部分数据如实暴露而不是默默丢掉）。多一次 CSSOM `page.evaluate`（约 3–8ms）。与 `all_metrics` OR 合并。 |
| `security_scan` | bool | false | 深度客户端安全扫描，写入 `stat.security_scan`：**SRI 覆盖**（跨域 `<script>`/`<link>` 缺 `integrity` 的供应链风险）、**`target=_blank`** 显式带 `rel=opener`（高危反向 tabnabbing；现代浏览器已默认隐含 noopener，故单纯缺 noopener 不再上报）、**表单安全**（明文 `action` / 非 HTTPS 页上的密码框）、**JS 库版本指纹**（jQuery / React / Vue / Angular / …，可离线对照 CVE 区间）、以及被动检测的 **CORS** `Access-Control-Allow-Origin: *` 配合 credentials 的配置错误。多一次 `page.evaluate` DOM 遍历（约 2–5ms）+ 一次纯服务端 CORS 派生。与 `all_metrics` OR 合并。与始终输出的 `security_audit`（响应头/cookie 配置 scorecard）互补。 |
| `all_metrics` | bool | false | 总开关，一次性启用所有 **分析类** flag：`web_vitals` / `metrics` / `metadata` / `render_blocking` / `service_worker` / `initiators` / `console_messages` / `image_sizing` / `dom_mutations` / `resources` / `http_errors` / `resource_hints` / `font_audit` / `security_scan`。专为 AI 比对 / 回归审计场景设计，避免长查询串。**不会**自动启用大体积二进制（`screenshot` / `pdf` / `har` / `save_dom_snapshot`）或 `coverage` —— 两者都有真实的每次请求开销，保持显式 opt-in。与单 flag 是 OR 合并，已经为 `true` 的不变。 |
| `content_only` | bool | false | 精简的**只取内容**模式 —— "我只想拿页面内容"。正文按调用方选择的 `data_format` 返回（默认 `html` / `text` / `markdown`）；此 flag **不会**强制 markdown —— 需要哪种格式由 `data_format` 决定。抑制所有分析类 flag + `all_metrics` + 二进制采集（`screenshot`/`pdf`/`har`/`save_dom_snapshot`）+ `coverage`，并跳过 `resource_summary` 派生。返回一个紧凑 JSON 对象 `{ status, final_url, char_count, data }`（忽略 `format`/`lang` 参数）—— `status` + 非平凡的 `char_count`（外加 `final_url` 没有跳到意外位置）顺带就是一个廉价的"是否正确渲染"检查，无需返回完整 `WebPageStat`。JS 仍会执行，所以 SPA 内容可被捕获；空白/骨架页会表现为接近空的 `data`。 |
| `data_format` | `html`\|`markdown`\|`text` | `html` | `stat.data` 字段的格式。 |
| `format` | `json`\|`markdown` | `json` | 响应封装格式。`markdown` 把整个 `WebPageStat` 渲染成 LLM 可读的文档。 |
| `lang` | `en`\|`zh` | `en` | **markdown 渲染**的语言（section 标题、说明文字、警告标签）。JSON 封装**绝不**翻译 —— 字段名、enum 标签值（`missing_immutable` / `short_max_age` 等）以及其它机器可读字符串始终保持英文，让下游基于这些值做分支的代码跨语言依然能工作。`format=json` 时该参数被忽略。 |
| `normalize_custom_elements` | bool | true | （仅 markdown 模式生效）把自定义元素（如 `taro-view-core`）按 computed `display` 重写成 `<div>` / `<span>`。 |
| `width` / `height` | u32 | 1920 / 1080 | viewport 尺寸（width / height / DPR 任一字段都会触发 override）。 |
| `device_scale_factor` | f64 | 1.0 | 设备像素比。 |
| `touch` | bool | false | 启用移动端 touch 事件模拟（`navigator.maxTouchPoints=5`，`ontouchstart` 可派发）。配合小 viewport 是完整移动端模拟。 |
| `cpu_throttle` | f64 | — | CPU 节流倍率（1.0 = 原生，4.0 = 4× 慢）。≤ 1.0 忽略。 |
| `user_agent` | string | — | UA 覆盖。**未指定时的默认 UA** 是一个 pin 死的主线 Chrome 字符串：`Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36`，专门避开 `HeadlessChrome` 这个关键字（绝大多数 WAF —— Cloudflare / Akamai / 企业网关 —— 会直接拦带这个 token 的请求）。真实的 Chromium 二进制版本仍然记录在 `chromium launched` 日志的 `binary_user_agent` 字段里，方便排查。 |
| `accept_language` | string | — | `Accept-Language` header 覆盖。跟 `user_agent` 独立；可以单独设而不指定 UA（此时 UA 用上面那个默认 Chrome 字符串）。 |
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
| `wait_until_load` | bool | false | collect 阶段的等待策略。`true` 在 `load`（onload）lifecycle 事件后短暂等待即返回 —— 在带大量长尾分析 / WebSocket 流量、永远到不了 `networkIdle` 的页面上更快、更确定。`false`（默认）在 Chrome 的 `networkIdle`（≥500ms 零在飞请求）之后返回 —— 当你需要把每个迟到的响应都记录进 `resources[]` 时用这个。与 `wait_for_element` / `wait_for_function` / `wait_for_request` 独立（不论用哪种 gate，这三个都会照常执行 / 匹配）。如果还有迟到的 JS 要跑完才能抓数据，配合 `settle_ms` 用。 |
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

<details>
<summary>点击展开完整 JSON 示例（约 300 行，所有分析字段齐全）</summary>

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
      "cache_control": "public, max-age=31536000, immutable",
      "has_source_map": true,
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
      "url": "https://cdn.example.com/hero.webp", "text_preview": null,
      "size": 384000.0, "load_time": 980.4, "render_time": 1023.7,
      "natural_width": 1920, "natural_height": 1080
    },
    "cls_entries": [
      { "time_ms": 850, "value": 0.032, "sources": [
        { "tag": "div", "id": "", "class": "ad-banner",
          "previous_rect": { "x": 0, "y": 200, "width": 1280, "height": 90 },
          "current_rect":  { "x": 0, "y": 440, "width": 1280, "height": 90 },
          "distance_px": 240.0 }
      ] }
    ],
    "cls_top_sources": [
      { "selector": "div.ad-banner", "total_shift": 0.032, "fraction": 0.71,
        "shift_count": 1, "max_distance_px": 240.0 }
    ],
    "inp": null, "interaction_count": 0,
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
    ],
    "long_task_top_offenders": [
      { "source": "https://www.googletagmanager.com/gtm.js?id=GTM-XYZ",
        "total_duration_ms": 412.0, "max_duration_ms": 187.5, "task_count": 3 },
      { "source": "self", "total_duration_ms": 96.4,
        "max_duration_ms": 96.4, "task_count": 1 }
    ],
    "fps_avg": 58.7, "fps_jank_ratio": 0.04,
    "fps_longest_frame_ms": 42.0, "fps_frame_count": 178
  },
  "metrics": {
    "js_heap_used": 12582912, "js_heap_total": 18874368,
    "documents": 1, "frames": 1, "nodes": 156, "js_event_listeners": 8,
    "script_duration_ms": 234.5, "layout_duration_ms": 45.2,
    "recalc_style_duration_ms": 12.8, "task_duration_ms": 312.1
  },
  "document_timing": {
    "url": "https://example.com/",
    "status": 200, "from_cache": false, "protocol": "h2",
    "dns_ms": 12, "tcp_ms": 28, "tls_ms": 41, "ttfb_ms": 187
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
    "third_party_script_bytes": 11200,
    "third_party_script_origins": [
      { "host": "cdn.vendor.com", "bytes": 8400, "count": 2 },
      { "host": "analytics.example", "bytes": 2800, "count": 1 }
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
    "legacy_image_bytes": 180000,
    "modern_image_bytes": 65000,
    "source_maps_present": 2,
    "source_maps_missing": 9,
    "top_largest_by_type": {
      "javascript": [
        { "url": "https://cdn.example.com/vendors.js",
          "bytes": 712600, "mime_type": "application/javascript",
          "from_cache": false }
      ],
      "image": [
        { "url": "https://example.com/hero.png",
          "bytes": 184000, "mime_type": "image/png",
          "from_cache": false }
      ]
    },
    "uncompressed_text_resources": [
      { "url": "https://example.com/static/legacy.js",
        "mime_type": "application/javascript", "bytes": 62000 },
      { "url": "https://example.com/static/styles.css",
        "mime_type": "text/css", "bytes": 22200 }
    ],
    "cache_policy_issues": [
      { "url": "https://cdn.example.com/app.4f7c2a91.js",
        "mime_type": "application/javascript",
        "cache_control": "public, max-age=31536000",
        "reason": "missing_immutable" },
      { "url": "https://example.com/static/logo.svg",
        "mime_type": "image/svg+xml",
        "cache_control": "public, max-age=30",
        "reason": "short_max_age" }
    ],
    "resource_hints": {
      "declared_preconnect": ["https://cdn.example.com"],
      "declared_dns_prefetch": [],
      "gap": [
        { "host": "analytics.example", "bytes": 5800, "count": 2 }
      ]
    },
    "duplicate_resources": {
      "exact_url": [
        { "key": "https://example.com/static/app.js",
          "urls": ["https://example.com/static/app.js"],
          "count": 2, "bytes_each": 84200, "wasted_bytes": 84200 }
      ],
      "likely_same_file": [
        { "key": "jquery.min.js|89476",
          "urls": [
            "https://cdn.jsdelivr.net/npm/jquery@3.7.1/dist/jquery.min.js",
            "https://cdnjs.cloudflare.com/ajax/libs/jquery/3.7.1/jquery.min.js"
          ],
          "count": 2, "bytes_each": 89476, "wasted_bytes": 89476 }
      ],
      "wasted_bytes": 173676
    },
    "mixed_content": {
      "detected": false, "total_count": 0, "resources": []
    },
    "max_initiator_chain_depth": 3,
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
      "missing": ["Referrer-Policy", "Permissions-Policy", "Cross-Origin-Opener-Policy"],
      "csp_analysis": {
        "directive_count": 5, "unsafe_inline": true, "unsafe_eval": false,
        "wildcard_directives": ["img-src"],
        "missing_object_src": false, "missing_base_uri": true,
        "missing_frame_ancestors": true,
        "weaknesses": ["unsafe-inline", "wildcard-source", "missing-base-uri", "missing-frame-ancestors"]
      },
      "hsts_analysis": {
        "max_age": 31536000, "include_subdomains": true,
        "preload": false, "effective": true
      }
    },
    "cookies": {
      "total": 3, "secure": 3, "http_only": 2,
      "same_site_set": 3, "same_site_none_without_secure": 0,
      "header_bytes": 248
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
  "http_errors": {
    "failed_count": 2,
    "failed_4xx": [
      { "url": "https://api.example.com/missing.json",
        "status": 404, "resource_type": "Fetch" }
    ],
    "failed_5xx": [
      { "url": "https://api.example.com/buggy",
        "status": 503, "resource_type": "Xhr" }
    ],
    "network_failures": [
      { "url": "https://blocked.example.com/script.js",
        "error_text": "net::ERR_BLOCKED_BY_CLIENT",
        "resource_type": "Script", "canceled": false }
    ],
    "final_url": "https://example.com/",
    "redirect_count": 0
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
      "has_width_attr": false, "has_height_attr": false, "has_srcset": false,
      "transferred_bytes": 1843200, "waste_ratio": 0.83
    }
  ],
  "image_audit": {
    "oversized": [
      { "url": "https://example.com/hero.jpg",
        "display_width": 800, "display_height": 450,
        "in_viewport": true, "ratio": 5.76 }
    ],
    "missing_dimensions": [
      { "url": "https://example.com/hero.jpg",
        "display_width": 800, "display_height": 450,
        "in_viewport": true, "ratio": 0.0 }
    ],
    "missing_lazy": [
      { "url": "https://example.com/footer-ad.png",
        "display_width": 728, "display_height": 90,
        "in_viewport": false, "ratio": 0.0 }
    ],
    "missing_srcset": [
      { "url": "https://example.com/hero.jpg",
        "display_width": 800, "display_height": 450,
        "in_viewport": true, "ratio": 0.0 }
    ]
  },
  "font_audit": {
    "font_count": 4, "loaded_count": 3,
    "display_distribution": { "swap": 2, "auto": 2 },
    "missing_swap": [
      { "family": "Inter",
        "source_url": "https://example.com/fonts/inter.woff2",
        "display": "auto" },
      { "family": "Roboto Mono",
        "source_url": "https://fonts.gstatic.com/s/robotomono/v23/...woff2",
        "display": "block" }
    ],
    "declared_preload_count": 1,
    "unreadable_stylesheets": 0
  },
  "security_scan": {
    "sri": {
      "total_cross_origin": 4, "protected": 1,
      "missing": [
        { "tag": "script", "url": "https://cdn.vendor.com/widget.js", "crossorigin": "anonymous" },
        { "tag": "link", "url": "https://cdn.vendor.com/theme.css", "crossorigin": null }
      ]
    },
    "unsafe_target_blank": [
      { "href": "https://partner.example/promo", "rel": "opener" }
    ],
    "forms": {
      "total": 2,
      "insecure_action": [
        { "action": "http://legacy.example.com/login", "has_password": true }
      ],
      "password_on_insecure_page": 0
    },
    "libraries": [
      { "name": "jQuery", "version": "3.6.0", "global": "jQuery" },
      { "name": "React", "version": "18.2.0", "global": "React" }
    ],
    "cors_issues": [
      { "url": "https://api.vendor.com/data", "allow_origin": "*",
        "allow_credentials": true, "reason": "wildcard-with-credentials" }
    ]
  },
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

</details>

`tls_info` 是主文档的证书（HTTPS 站点自动采集，HTTP / file:// 时为
`null`）。`tls_certificates` 是页面访问过的**所有** HTTPS host
（含第三方 CDN）去重后的证书清单，按 `days_remaining` 升序 ——
始终存在（纯 HTTP 页面为空数组）。

按需启用、未启用时为 `null` 的字段：`screenshot` / `pdf` / `har` /
`dom_snapshot` / `web_vitals` / `metrics` / `metadata` /
`render_blocking_resources` / `service_worker` / `image_sizing` /
`dom_mutations` / `http_errors` / `coverage`。`console_messages` 和
`resources` 在对应 flag 未设之前是空数组 `[]` —— `resource_count`
和 `total_size`（标量）以及 `resource_summary`（聚合）始终存在，
所以"功能校验"用法不需要拉取完整列表也能获得关键信号。

`exceptions` 和 `js_exceptions` 始终输出（无需 opt-in）：
`Runtime.exceptionThrown` 一直订阅，分桶统计成本基本为零。无异常时
`exceptions: []`、`js_exceptions: { total: 0, by_name: [] }`。AI 或
监控只看一个标量 `js_exceptions.total` 就能发现回归（例如"今天突然
多了 12 个 ReferenceError"）；`by_name` 是按出现次数排序的 top 10
明细，每个 bucket 附带一条 sample 消息。

`document_timing` 在观测到 Document 响应时始终输出（基本每次请求都
有）。`dns_ms` / `tcp_ms` / `tls_ms` / `ttfb_ms` 这几个 phase 标量在
CDP 报告 phase 被跳过（缓存命中、连接复用、纯 HTTP 等）时会 clamp
到 `0`，可以直接相加。`ttfb_ms` 用来回答"后端是不是慢了"；配合
`metrics.script_duration_ms` 区分服务端慢 vs 前端慢。仅在异常流程
（全缓存导航，没有真实 Document 响应）下为 `None`。

`security_audit` 也是始终输出（从 `security_headers` + `cookies`
派生）：一次性配置审计的 scorecard。`security_audit.headers.present_count`
（0..=7）表示七个核心强制头（HSTS / CSP / X-Frame-Options /
X-Content-Type-Options / Referrer-Policy / Permissions-Policy /
Cross-Origin-Opener-Policy）有几个存在，`missing` 列出缺失的那些；
`security_audit.cookies` 输出 cookie 总数和各标志覆盖率（`secure` /
`http_only` / `same_site_set`），以及反模式计数
`same_site_none_without_secure`（任意非零都是 finding —— 现代浏览器
会直接拒收这些 cookie）。页面没有 cookie 也没有安全头时整个 struct
全是 0 / false，本身就是有意义的信号。

有两个头真正的信号在**值**里而非"有没有"，所以会被深度解析：
`security_audit.headers.csp_analysis`（仅在存在强制 CSP 时出现）把策略
拆成 `unsafe_inline` / `unsafe_eval` / `wildcard_directives` /
`missing_object_src` / `missing_base_uri` / `missing_frame_ancestors`，
并汇总成一个 `weaknesses[]` 列表 —— "有 CSP 但很弱"正是 `csp: true`
单独那个 bool 掩盖掉的 finding。`security_audit.headers.hsts_analysis`
解析 `max-age` / `includeSubDomains` / `preload` 并给出 `effective`
（`max-age=0` 时为 `false`，即 HSTS 配了但被禁用 —— 典型的回滚事故）。
底层头不存在时两者都为 `None`/省略。

`resource_summary.third_party_script_origins` 是页面的第三方**可执行
JS** 攻击面：这些外部来源加载的代码运行在你的 origin 下，拥有完整的
DOM/cookie 访问权限（Magecart 式供应链攻击向量）。按 JS 字节排序、上限
10 条；`third_party_script_bytes` 是标量合计。它与 `third_party_bytes` /
`top_third_party_domains`（统计所有资源类型）不同 —— 某次发布后这里
冒出一个新来源，意味着引入了新的外部代码依赖。

`security_scan`（opt-in，`security_scan=true`）是始终输出的 `security_audit`
配置 scorecard 的 DOM 层伴侣，打包五类从渲染后 DOM / 观测响应里读出的
发现：`sri`（跨域 `<script>`/`<link>` 的 Subresource-Integrity 覆盖 ——
`total_cross_origin` / `protected` 以及缺 `integrity` 的 `missing[]` 供应链
缺口列表）；`unsafe_target_blank[]`（显式带 `rel=opener` 的链接 —— 高危
反向 tabnabbing；单纯缺 `noopener` 的链接**不**上报，因为现代浏览器对
`target=_blank` 已默认隐含 `noopener`）；`forms`
（明文 `action` 端点 + 非 HTTPS 页上的密码框）；`libraries[]`（从知名
全局变量识别的 JS 框架 + 版本，便于离线对照 CVE —— 注意检测不到**不**
等于不存在，打包器常会剥掉全局变量）；以及 `cors_issues[]`（被动检测的
`Access-Control-Allow-Origin: *` 配合 credentials 的服务端 bug —— **不会**
主动探测反射 origin 绕过）。多一次 DOM 遍历；CORS 部分是对已抓响应的
纯服务端派生。

(`resources[].initiator` 在 `initiators=true` 时也会填充：
`{ "type": "parser" | "script" | "preload" | ..., "url": "...", "line_number": 12 }`。)

#### 只取内容响应（`content_only=true`）

"只要渲染出来、告诉我成没成"的精简快捷方式。完全绕过完整的
`WebPageStat` 封装，返回一个紧凑对象：

```json
{ "status": 200, "final_url": "https://example.com/", "char_count": 4213, "data": "..." }
```

- `status` —— 最终文档（跳转后）的 HTTP 状态码。
- `final_url` —— 跳转后的落地 URL（能抓到意外的跳转 / 登录墙）。
- `char_count` —— `data.chars().count()`；本该有内容的页面若接近 0，
  就是空白 / 骨架屏 / 反爬拦截的信号。
- `data` —— 正文，格式由 `data_format` 决定（默认 `html` / `text` /
  `markdown`）。

所有分析类 flag、`all_metrics`、二进制采集
（`screenshot` / `pdf` / `har` / `save_dom_snapshot`）和 `coverage`
都被强制**关闭**，`format` / `lang` 被忽略。JS 仍会执行，所以 SPA
内容可被捕获。`status` + 健康的 `char_count` + 没有跑偏的 `final_url`
三者组合，就是最便宜的渲染正确性探针 —— 而 `data`（markdown）正好是
你交给 LLM 做"这个页面有效吗"语义判断的输入。

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
| 504 | 总体硬上限（`timeout_ms` + buffer，默认 10s）超时 |

### `POST /summary/batch`

**一次请求抓多个 URL**。body 为 `{ "urls": [...] }` 加任意 `/summary` 参数，
后者作为**共享模板**应用到每个 URL（顶层 flatten 的 `url` 若存在则忽略）。
URL 在池的并发上限（`pool_size × pages_per_instance`）下并发抓取；连接保持
到所有项完成。

```bash
curl -X POST http://localhost:3000/summary/batch \
  -H 'Content-Type: application/json' \
  -d '{
    "urls": ["https://a.example", "https://b.example", "https://c.example"],
    "content_only": true,
    "data_format": "markdown",
    "wait_for_element": "article"
  }'
```

响应是**按输入顺序**的 JSON 数组；单个坏 URL 不会让整批失败，而是变成该项
的错误：

```json
{
  "count": 3,
  "results": [
    { "url": "https://a.example", "status": 200, "data": { "status": 200, "final_url": "https://a.example/", "char_count": 4213, "data": "# ..." } },
    { "url": "https://b.example", "status": 400, "error": "scheme `ftp` not allowed; only http/https" },
    { "url": "https://c.example", "status": 200, "data": { "status": 200, "final_url": "https://c.example/", "char_count": 980, "data": "# ..." } }
  ]
}
```

每项：`status` 成功为 `200`、失败为对应错误码；成功时 `data` 就是单接口返回的
同款 JSON（`content_only` 为紧凑内容对象，否则为完整 `WebPageStat`），失败时
`error` 带错误信息。封装恒为 JSON —— `format=markdown` 被忽略（批量是数组）。
每项与单次 `/summary` 一样计入指标。每请求最大 URL 数由
`BROWSER_HEADLESS_MAX_BATCH_URLS` 控制（默认 100）；空 `urls` 或超限返回 400。
这正是「AI 批量判断一堆页面是否正常」的高效形态：一次请求、服务端统一排并发。

---

## 配置

| 环境变量 | 默认值 | 作用 |
|---|---|---|
| `BROWSER_HEADLESS_MODE` | `serve` | `serve` 跑 HTTP 接口；`worker` 跑 Redis 队列消费者（无 HTTP）。见 [Worker 模式](#worker-模式redis-队列)。 |
| `BROWSER_HEADLESS_API_KEY` | 未设置（开放） | 开启 API key 鉴权。设置后，`/summary` 必须带 `X-Api-Key: <value>` header，不匹配 / 缺失返 401。`/healthz`、`/readyz`、`/metrics` 永远开放（保证探针 / 抓取可用）。key 采用常量时间比对（`subtle::ConstantTimeEq`）；但因长度不同会短路，仍建议使用高熵 key（≥32 随机字节）。 |
| `BROWSER_HEADLESS_POOL_SIZE` | 1 | 池中 chromium 进程数。总并发 = `POOL_SIZE × MAX_PAGES`。`1` 即原单实例行为。每个实例用独立 profile 目录、独立监督。 |
| `BROWSER_HEADLESS_MAX_PAGES` | 8 | **每实例**的 page 并发上限。超出 `POOL_SIZE × MAX_PAGES` 的请求排队，handler 返回时释放 permit。 |
| `BROWSER_HEADLESS_RECYCLE_AFTER_REQUESTS` | 0（关闭） | 实例服务满这么多请求后回收（排空 → 换子进程），抑制 chromium 内存增长。`0` 关闭按次数回收。 |
| `BROWSER_HEADLESS_RECYCLE_AFTER_SECS` | 0（关闭） | 实例达到该年龄（秒）后回收。`0` 关闭按年龄回收。与按次数触发相互独立。 |
| `BROWSER_HEADLESS_DRAIN_TIMEOUT_MS` | 30000 | 主动回收时，等待该实例 in-flight 请求跑完的最长时间，超时则强制换子进程（drop 旧 browser 会取消卡住的 capture）。 |
| `BROWSER_HEADLESS_MAX_BATCH_URLS` | 100 | `POST /summary/batch` 单请求接受的最大 URL 数。空 `urls` 或超限返回 400。每个 URL 仍受池限流，此项只限单请求的 fan-out。 |
| `BROWSER_HEADLESS_ALLOW_PRIVATE_IPS` | 未设置 | 设为 `1` / `true` / `yes` / `on` 关闭 SSRF 守卫（允许私有 / 回环 / 链路本地 IP）。仅用于内网部署。 |
| `BROWSER_HEADLESS_DEFAULT_TIMEOUT_MS` | 30000 | 调用方不传 `timeout_ms` 时的每请求默认软等待预算。空 / 非数字 / `0` 时忽略。硬上限仍为 `timeout_ms + buffer`（buffer 默认 10s）。单次请求的 `?timeout_ms=` 仍然优先覆盖。 |
| `BROWSER_HEADLESS_REQUEST_TIMEOUT_MS` | `max(默认超时 + 30s, 120000)` | chromiumoxide 的每次导航 CDP command-chain 超时（从其 30s 默认上调，避免较大的 `timeout_ms` 在导航层被截断）。**注意：** chromiumoxide 0.9 对*离散* `page.execute` 调用的超时硬编码为 30s，不受此项影响 —— 但页面加载等待走的是我们自己的事件循环、受 `timeout_ms` 约束，所以慢加载仍按 `timeout_ms` 独立生效。 |
| `BROWSER_HEADLESS_DEADLINE_BUFFER_MS` | 10000 | 在 `timeout_ms` 之上追加的硬截止 buffer（`总上限 = timeout_ms + buffer`），覆盖页面等待预算之外的 chromium 开销（建上下文 / 开 page / 释放）。允许设为 `0`（不留余量）。硬截止触发时返回 `504`。 |
| `CHROME` | （自动探测） | Chrome / Chromium 可执行文件路径。Dockerfile 里设的是 `/usr/bin/chromium`。 |
| `RUST_LOG` | `info,chromiumoxide::conn=off,chromiumoxide::handler=off` | 标准 `tracing_subscriber` 过滤器。`browser_headless=debug` 可看到每阶段耗时。 |

---

## Worker 模式（Redis 队列）

为了跨机横向扩展，可以把实例跑成**队列 worker**，与 HTTP 接口并存或替代它。
设 `BROWSER_HEADLESS_MODE=worker`：进程不绑任何 HTTP 口，加入 Redis Streams
consumer group，对每个 job 用同一套抓取引擎在自己的浏览器池上跑，把结果写回
`result:{id}` 键。加机器 = 加 worker；HTTP 接口完全解耦（两者只共享库代码，
互不调用）。

**入队**：往 stream `XADD` 一个 `payload` 字段（JSON）——
`{ "id": "...", "url": "...", ...任意 /summary 参数... }`（`id` 可选，缺省回退到
stream entry id）：

```bash
redis-cli XADD browser_headless:jobs '*' payload \
  '{"id":"job1","url":"https://example.com","content_only":true,"data_format":"markdown"}'
```

**取结果**（JSON，带 TTL）：

```bash
redis-cli GET browser_headless:result:job1
# {"id":"job1","status":200,"data":{"status":200,"final_url":"https://example.com/","char_count":167,"data":"# ..."}}
```

`data` 与单接口返回同款（`content_only` 为紧凑内容对象，否则完整 `WebPageStat`）；
失败时结果带 `status` + `error`。投递为 **at-least-once**：结果写完才 ack，崩溃
worker 遗留的条目在可见性超时后被 `XAUTOCLAIM` 重认领——所以同一 URL 可能抓两次
（只读抓取无害；带副作用的 `script` 请加幂等键）。单 worker 并发 = 其池容量
（`POOL_SIZE × MAX_PAGES`）。

| 环境变量 | 默认值 | 作用 |
|---|---|---|
| `BROWSER_HEADLESS_REDIS_URL` | `redis://127.0.0.1:6379` | Redis 连接 URL。 |
| `BROWSER_HEADLESS_JOBS_STREAM` | `browser_headless:jobs` | 消费 job 的 stream。 |
| `BROWSER_HEADLESS_CONSUMER_GROUP` | `workers` | consumer group——在多 worker 间均衡 job。 |
| `BROWSER_HEADLESS_CONSUMER_NAME` | `worker-<pid>` | 本 worker 的 consumer 名（每进程唯一）。 |
| `BROWSER_HEADLESS_RESULT_PREFIX` | `browser_headless:result:` | 结果键前缀；键为 `<前缀><id>`。 |
| `BROWSER_HEADLESS_RESULT_TTL_SECS` | 3600 | 结果键 TTL（秒）。 |
| `BROWSER_HEADLESS_JOB_BLOCK_MS` | 5000 | `XREADGROUP` 阻塞时长，到点后做一次 reclaim。 |
| `BROWSER_HEADLESS_JOB_VISIBILITY_MS` | 120000 | 条目空闲多久后可被其它 worker 重认领。 |

上面[配置](#配置)里的 池 / 回收 / SSRF / 超时 等环境变量对 worker 同样生效。
worker 不暴露 `/metrics`（无 HTTP）；用日志和 Redis consumer-group lag
（`XPENDING`）监控。

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

### 廉价的渲染正确性检查（AI 友好）

只抓渲染后的正文（markdown），不带指标、不带二进制采集，再交给模型
判断页面是否有效。

```bash
curl -X POST http://localhost:3000/summary \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "https://example.com/article",
    "content_only": true,
    "data_format": "markdown",
    "wait_for_element": "article"
  }'
```

```json
{ "status": 200, "final_url": "https://example.com/article", "char_count": 4213, "data": "# Title\n\n..." }
```

`200` 状态 + 健康的 `char_count` + 主题相关的 `data` = 页面渲染成功；
接近空的 `data`（骨架屏 / 空白 / 反爬墙）、非 2xx 的 `status`、或跳到
意外位置的 `final_url`，就是需要标记的信号。在机械检查之上，把 `data`
喂给 LLM、用类似"这读起来像不像一个完整的文章页？"的提示做一次语义
判断。

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

### AI 比对 / 回归基线

一次性启用所有对比维度，输出 markdown 文档给 LLM 或 diff 任务：

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

(`all_metrics: true` 是简写，一次性开启全部 14 个分析类 flag ——
`web_vitals` / `metrics` / `metadata` / `render_blocking` /
`service_worker` / `initiators` / `console_messages` / `image_sizing` /
`dom_mutations` / `resources` / `http_errors` / `resource_hints` /
`font_audit` / `security_scan`。大体积二进制 `screenshot` / `pdf` 等
以及 `coverage` 仍需显式开启。)

返回的单一 markdown 文档包含：加载摘要、异常、Web Vitals + LCP 元素 +
top CLS offenders、资源汇总、render-blocking 资源、TLS 证书 + per-host
清单、安全头、Service Worker、图片尺寸审计、页面元数据、Page Metrics
（含 CPU 时间分解）、DOM 突变热点、resources 列表、cookies。
存档多次快照按时间 diff，可捕捉：

- `lcp_element` 变了 → 图片挂了走降级
- `lcp_element.natural_width / display_width > 2`（display 取自
  `image_sizing` 或 LCP `size`）→ 首屏图过大；换更小尺寸的变体并
  对其 origin 加 `<link rel="preload">`
- `lcp_element.load_time` 涨了但 `render_time` 没变 → LCP 资源的
  网络阶段变慢（CDN 退化 / 缺 preconnect 到其 origin）；反过来
  `render_time` 涨而 `load_time` 平 → 瓶颈移到主线程绘制阶段
- `cls_top_sources[0].selector` 变了 → 新的布局抖动源
- `cls_top_sources[0].max_distance_px > 100` → 至少有一次单次跳
  动超过 100px；这个数就是"如果当初给它写死 `min-height`"的下限
- `web_vitals.fps_jank_ratio > 0.10` → 观察窗口内超过 10% 的帧没达到
  60fps 目标；marketing / 滚动密集型页面会有明显卡顿感。结合
  `loaf_top_offenders` 看是哪个脚本源造成的。当作"同 harness 上的回归
  告警"用，不要当成"真机上的用户体感"—— headless 走的是软件光栅化
- `web_vitals.fps_longest_frame_ms > 100` → 至少有一帧耗时超过
  100ms；用户能明显感觉到一次停顿。罪魁脚本通常就在
  `loaf_top_offenders[0].source_url`
- `web_vitals.long_task_top_offenders[0].source` 指明了贡献最多
  主线程阻塞时间的脚本 URL / iframe / 函数名。当 `total_duration_ms
  > 200` 且来源是第三方标签（gtm / segment / 各种 analytics），可以
  考虑改 `async` 加载或者拆出关键路径
- `image_audit.oversized[]` 非空 → 每条的 `ratio` 是 natural /
  effective-display 的倍数；ratio > 4 就该换更小尺寸或加 `srcset`
- `image_audit.missing_dimensions[]` 非空 → CLS 直接成因（解码前
  浏览器无法预留布局空间）；给 `<img>` 加上等于显示尺寸的
  `width=` / `height=` 属性
- `image_audit.missing_lazy[]` 非空 → 首屏外的图被立即加载；逐个
  加 `loading="lazy"`
- `image_audit.missing_srcset[]` 非空 → 没做响应式变体；加
  `srcset`（需要 art direction 再加 `sizes`）
- `font_audit.missing_swap[]` 非空 → `@font-face` 的 `font-display`
  不在 `{swap, optional}` 集合里，会出现 FOIT（加载期间不可见）。
  每条带 `family` + `source_url`，直接对应一条修复："在这个
  `@font-face` 块加 `font-display: swap;`"
- `font_audit.declared_preload_count == 0` 且 `font_count > 0` →
  页面用了 web 字体但一个都没 preload。如果有渲染关键字体（首屏
  正文 / 标题用的），加 `<link rel="preload" as="font"
  type="font/woff2" crossorigin>`
- `font_audit.unreadable_stylesheets > 0` → 因为跨域 stylesheet
  没加 `crossorigin`，审计本身是不完整的。AI 建议：给那些
  `<link rel="stylesheet">` 加 `crossorigin`，既能让审计看到，
  又能让浏览器一致地缓存它们
- `security_scan.sri.missing[]` 非空 → 跨域 `<script>`/`<link>` 缺
  `integrity`，CDN 一旦被攻破就会下发任意代码。给每条加 SRI 哈希
  （`integrity="sha384-…"` + `crossorigin`）
- `security_scan.cors_issues[]` 非空 → 某 API 同时下发
  `Access-Control-Allow-Origin: *` 和 credentials（spec-invalid 的服务端
  bug）；改成具体的 allow-list origin 而不是 `*`
- `security_scan.forms.insecure_action[]` 非空 → 表单提交走明文 HTTP
  （凭证泄露 / 混合内容）；把 `action` 换成 HTTPS
- `security_scan.libraries[]` 显示某库版本过旧 → 拿 `name` + `version`
  对照已知 CVE 区间，升级
- `metrics.script_duration_ms` +30% 而 LCP 没动 → JS 回归
- `security_headers["Content-Security-Policy"]` 没了 → 安全回归
- `security_audit.headers.present_count` 比基线下降 → 某次发布
  去掉了一个核心强制头；`security_audit.headers.missing` 直接列出
  少了哪几个
- `security_audit.cookies.same_site_none_without_secure > 0` → 这种
  cookie 现代浏览器会直接拒收，永远是 actionable 信号
- `security_audit.cookies.secure / total` 比例下降 → 新增了一个不带
  `Secure` 的 cookie（通常是某个第三方脚本设置的）
- `security_audit.headers.csp_analysis.weaknesses[]` 变长（或
  `unsafe_inline` 翻成 true）→ 某次发布削弱了 CSP；这正是 `csp` bool
  仍为 `true` 时背后最常见的真实回归
- `security_audit.headers.hsts_analysis.effective` 翻成 false → HSTS
  虽在但 `max-age=0`（被禁用），通常是回滚事故
- `resource_summary.third_party_script_origins[]` 出现新来源 → 有新的
  外部代码依赖开始在你的 origin 下运行，需要审查该供应方
- `metadata.robots` 意外包含 `noindex` → SEO 灾难
- `render_blocking_resources` 新增条目 → 性能回归
- `service_worker.controlled` 变成 false → PWA 坏了
- 新的第三方 `resources[].url`，其 `initiator.url` 指向一个第一方脚本
  → 立刻知道是哪个库带进来的 tracker
- `tls_info.days_remaining < 30` → 证书快过期；< 0 → 已过期；
  `tls_certificates[*]` 中任一接近过期 → 第三方 CDN 证书在把页面打挂
  之前抓到
- `tls_info.remote_ip` / `.issuer` 异常变化 → DNS 劫持 / CA 切换 /
  MITM 信号
- `image_sizing[0].waste_ratio > 0.5 && in_viewport=true` → 首屏关键
  路径有大图浪费带宽
- `dom_mutations.total_added_nodes` 比基线翻 N 倍 → 渲染抖动回归
  （框架降级、`key` 优化失效等）
- `dom_mutations.top_attributes_changed` 由 `style` 主导且数量极高 →
  失控的动画 / transition 频繁触发 reflow
- `web_vitals.loaf_top_offenders[0].source_url` 变更或在排名上升 →
  JS 卡顿源切换；结合 `total_forced_style_layout_ms` 直接定位
  layout thrashing 的脚本
- `web_vitals.loaf_count` 或 `loaf_total_blocking_duration` 大幅上升 →
  渲染期出现新的长动画帧（比 TBT 更精确）
- `web_vitals.inp > 200ms` 且 `interaction_count > 0` → 模拟交互
  （通过 `script` 触发点击）后响应性退化
- `resource_summary.protocol_distribution["h2"]` 占比下降 → HTTP/2
  覆盖率退化；回落到 `http/1.1` 会触发连接抖动
- `resource_summary.modern_protocol_share < 0.9` → HTTP/2+3 覆盖率
  的单标量信号，比每版本直方图更适合报警阈值
- `resource_summary.uncompressed_text_bytes > 50_000` → 文本资源未启用
  `Content-Encoding`，错失压缩
- `resource_summary.compression_breakdown["none"]` 相对基线上升 →
  部分文本响应丢了 encoding 头（通常是 CDN 缓存配置走偏）
- `resource_summary.cache_control_missing` 发布后突增 → 静态资源
  没带 caching 指令，通常是新增 origin 旁路了 CDN 配置
- `resource_summary.top_third_party_domains[0].bytes` 突增 → 最重的
  外部依赖膨胀，越早隔离越好
- `coverage.js_unused_ratio > 0.6` 或 `coverage.css_unused_ratio > 0.6`
  → 发送给客户端的死代码占比过大；`coverage.top_unused[]` 直接列出
  最浪费的文件（仅当显式 `coverage=true` 时输出，`all_metrics` 不会
  自动启用）
- `document_timing.ttfb_ms` 上升而 `metrics.script_duration_ms` 没变
  → 后端 / SSR 层慢了，跟前端无关
- `document_timing.tls_ms` 在热连接上仍 > 100ms → 证书链过长，或者
  0-RTT 恢复没配上
- 在图片多的页面 `resource_summary.modern_image_bytes / (modern + legacy)`
  偏低 → Lighthouse "Serve images in next-gen formats" 候选；把热图
  转 WebP / AVIF
- 生产环境 `resource_summary.source_maps_present > 0` → sourcemap
  对外暴露（安全 / IP 泄漏），修复后应归零
- `resource_summary.duplicate_resources.wasted_bytes > 0` → 同一份
  静态文件被多次加载；`exact_url[]` 抓重复导入 / 水合循环这类 bug，
  `likely_same_file[]` 抓"同一库从多个 CDN 引入"的浪费
- `resource_summary.mixed_content.detected = true` → HTTPS 页面在加
  plain-HTTP 资源；现代浏览器会直接拦或者自动升级。`resources[]`
  按字节大小排序列出问题资源
- `resource_summary.top_largest_by_type["javascript"][0].bytes > 500_000`
  （或其它桶的对应阈值）→ 单个 bundle 主导了这一类型，是代码分割或
  重新评估压缩方案的候选。每个桶（`javascript` / `css` / `image` /
  `font`）都有独立的 top-5 列表
- `resource_summary.uncompressed_text_resources[]` 非空 → 文本资源未
  带 `Content-Encoding`；每条直接指向具体文件（aggregate 数据
  在 `uncompressed_text_bytes`）
- `resource_summary.cache_policy_issues[]` 非空 → 静态资源的缓存配置
  有问题。`reason="short_max_age"` 标记 JS / CSS / image / font 上
  `max-age < 60s`；`reason="missing_immutable"` 标记指纹化 URL
  （≥ 8 位 hex token）带了 `Cache-Control` 但缺少 `immutable` ——
  每次重新校验都是纯浪费
- `resource_summary.resource_hints.gap[]` 非空 → 实际命中的第三方
  主机没有声明 `<link rel="preconnect">` / `dns-prefetch`。每个 gap
  ≈ 一次可避免的 DNS+TLS 往返（~100–300ms）。仅当 `resource_hints=true`
  （或 `all_metrics=true`）时输出
- `resource_summary.max_initiator_chain_depth > 4` → 关键请求链太深
  （Lighthouse "Avoid chaining critical requests"）；给中间关键资源
  加 preload 或者把依赖图压扁。仅在 `initiators=true` 时输出
- `security_audit.cookies.header_bytes ≥ 4096` → cookie header 接近
  或超过常见框架 4 KB 上限；每个请求都背这个带宽税
- `resource_summary.connections_new` 大幅上升而 `connections_reused`
  持平 → 连接池 / HTTP 多路复用退化
- `resource_summary.unique_hosts` 增长 → DNS 查询开销上升，通常
  伴随新引入的第三方脚本

---

## License

[Apache License 2.0](LICENSE)
