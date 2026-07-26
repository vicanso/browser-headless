//! Markdown rendering of [`super::WebPageStat`] for LLM-friendly output.
use super::*;

impl WebPageStat {
    /// Render this summary as Markdown suitable for feeding to an LLM as
    /// context. Each resource becomes a short prose sentence (cache hit /
    /// success / failure framed differently), exceptions are listed, and the
    /// page content goes in a fenced block at the end.
    ///
    /// `lang` controls the natural-language strings only — section
    /// headings, prose templates, warning labels. URLs, numbers, and
    /// enum-tag values (e.g. `missing_immutable`) are emitted verbatim
    /// regardless.
    pub fn to_markdown(&self, lang: Lang) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        // Tiny translation helper: takes the English and Chinese versions
        // of a literal, returns whichever matches the request's `lang`.
        // Closure form (not a top-level fn) so we can use it inside this
        // method without ceremony, and because every call site supplies
        // both arms as `&'static str` literals.
        let tr = |en: &'static str, zh: &'static str| -> &'static str {
            match lang {
                Lang::En => en,
                Lang::Zh => zh,
            }
        };

        let _ = writeln!(s, "{}", tr("# Page Summary", "# 页面摘要"));
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "{} **{}ms** (FCP {}ms, DCL {}ms). {} **{}** {} **{}** {}.",
            tr("Load completed in", "加载完成于"),
            self.load_time,
            self.fcp_time,
            self.dcl_time,
            tr("Transferred", "传输"),
            format_bytes(self.total_size),
            tr("across", "经由"),
            self.resource_count,
            tr("resources", "个资源"),
        );
        let _ = writeln!(s);

        if !self.exceptions.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## JavaScript Exceptions", "## JavaScript 异常"),
                self.exceptions.len(),
            );
            let _ = writeln!(s);
            if !self.js_exceptions.by_name.is_empty() {
                let _ = writeln!(s, "{}", tr("By class:", "按类型："));
                for entry in &self.js_exceptions.by_name {
                    match entry.sample_message.as_deref() {
                        Some(msg) => {
                            let _ = writeln!(s, "- **{}** ×{}: {}", entry.name, entry.count, msg);
                        }
                        None => {
                            let _ = writeln!(s, "- **{}** ×{}", entry.name, entry.count);
                        }
                    }
                }
                let _ = writeln!(s);
                let _ = writeln!(s, "{}", tr("Full list:", "完整列表："));
            }
            for ex in &self.exceptions {
                let _ = writeln!(s, "- {ex}");
            }
            let _ = writeln!(s);
        }

        if !self.console_messages.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Console Messages", "## 控制台输出"),
                self.console_messages.len(),
            );
            let _ = writeln!(s);
            for msg in &self.console_messages {
                let _ = writeln!(s, "- {msg}");
            }
            let _ = writeln!(s);
        }

        // ─── Overview block ─────────────────────────────────────────────
        // High-level summaries first (perf, security, SEO), then the raw
        // enumerations (resources, cookies), then binary attachments, then
        // the page content itself. Lets a reader (human or LLM) judge the
        // page from a few short sections before scrolling past long lists.

        if let Some(v) = &self.web_vitals {
            let _ = writeln!(s, "{}", tr("## Web Vitals", "## 网页关键性能指标"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- LCP **{:.0}ms** · CLS **{:.3}** · TBT **{:.0}ms** · TTFB **{:.0}ms** · {} **{}**",
                v.lcp,
                v.cls,
                v.tbt,
                v.ttfb,
                tr("long tasks", "长任务"),
                v.long_tasks,
            );
            // FPS — only render when frames were actually observed
            // (`fps_frame_count > 0`). Otherwise the values are all 0
            // and would mislead. `jank_ratio` rendered as percent and
            // tagged with ⚠️ above the visibly-stuttery 10% threshold.
            if v.fps_frame_count > 0 {
                let jank_pct = v.fps_jank_ratio * 100.0;
                let jank_warn = if v.fps_jank_ratio > 0.10 {
                    " ⚠️"
                } else {
                    ""
                };
                let _ = writeln!(
                    s,
                    "- FPS **{:.1}** ({} {}) · {} **{:.0}%**{} · {} **{:.0}ms**",
                    v.fps_avg,
                    v.fps_frame_count,
                    tr(
                        if v.fps_frame_count == 1 {
                            "frame"
                        } else {
                            "frames"
                        },
                        "帧",
                    ),
                    tr("jank", "卡顿率"),
                    jank_pct,
                    jank_warn,
                    tr("longest frame", "最长一帧"),
                    v.fps_longest_frame_ms,
                );
            }
            if let Some(el) = &v.lcp_element {
                let mut desc = format!("`<{}", el.tag);
                if !el.id.is_empty() {
                    desc.push_str(&format!(" id=\"{}\"", el.id));
                }
                if !el.class.is_empty() {
                    desc.push_str(&format!(" class=\"{}\"", el.class));
                }
                desc.push_str(">`");
                if let Some(u) = &el.url {
                    desc.push_str(&format!(" — `{u}`"));
                } else if let Some(t) = &el.text_preview {
                    desc.push_str(&format!(" — \"{t}\""));
                }
                let _ = writeln!(s, "- {}: {desc}", tr("LCP element", "LCP 元素"));
                // Size + load/render-time split, only when populated.
                // Cross-origin images often report `render_time=0` — we
                // suppress the field then so the markdown doesn't lie.
                if el.size > 0.0 {
                    let mut detail =
                        format!("  - {}: **{:.0}** CSS px²", tr("Size", "面积"), el.size,);
                    if el.natural_width > 0 && el.natural_height > 0 {
                        detail.push_str(&format!(
                            " · {} **{}×{}**",
                            tr("natural", "原生"),
                            el.natural_width,
                            el.natural_height,
                        ));
                    }
                    let _ = writeln!(s, "{detail}");
                }
                if el.load_time > 0.0 || el.render_time > 0.0 {
                    let mut detail = String::from("  - ");
                    if el.load_time > 0.0 {
                        detail.push_str(&format!(
                            "{} **{:.0}ms**",
                            tr("Load", "加载"),
                            el.load_time,
                        ));
                    }
                    if el.render_time > 0.0 {
                        if el.load_time > 0.0 {
                            detail.push_str(" · ");
                        }
                        detail.push_str(&format!(
                            "{} **{:.0}ms**",
                            tr("Render", "绘制"),
                            el.render_time,
                        ));
                    }
                    let _ = writeln!(s, "{detail}");
                }
            }
            if !v.cls_top_sources.is_empty() {
                let _ = writeln!(s, "- {}", tr("Top CLS offenders:", "CLS 主要肇事元素："));
                for (i, src) in v.cls_top_sources.iter().take(3).enumerate() {
                    // Optional "moved Npx" tail — only when source-level
                    // geometry was captured (`max_distance_px > 0`).
                    // Carries the concrete "reserve N px" actionable.
                    let moved = if src.max_distance_px > 0.0 {
                        format!(
                            " · {} **{:.0}px**",
                            tr("max move", "最大位移"),
                            src.max_distance_px,
                        )
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        s,
                        "  {}. **{}** — {:.3} ({:.0}%) {} {} {}{}",
                        i + 1,
                        src.selector,
                        src.total_shift,
                        src.fraction * 100.0,
                        tr("across", "共"),
                        src.shift_count,
                        tr(
                            if src.shift_count == 1 {
                                "shift"
                            } else {
                                "shifts"
                            },
                            "次抖动",
                        ),
                        moved,
                    );
                }
                if v.cls_top_sources.len() > 3 {
                    let _ = writeln!(
                        s,
                        "  {} {} {} (`cls_top_sources` / `cls_entries`).",
                        tr("…and", "……还有"),
                        v.cls_top_sources.len() - 3,
                        tr("more — see JSON for full list", "项更多，详见 JSON"),
                    );
                }
            }
            // INP — show only when there were actual interactions; otherwise
            // the value is just the default 0 and would mislead readers.
            if v.interaction_count > 0 {
                let _ = writeln!(
                    s,
                    "- INP **{:.0}ms** {} **{}** {}",
                    v.inp.unwrap_or(0.0),
                    tr("across", "共"),
                    v.interaction_count,
                    tr(
                        if v.interaction_count == 1 {
                            "interaction"
                        } else {
                            "interactions"
                        },
                        "次交互",
                    ),
                );
            }
            // Long Task attribution — render only when offender data
            // was produced (raw entries captured AND at least one had
            // something to group on). The `long_tasks` headline number
            // is already in the top-line vitals row above.
            if !v.long_task_top_offenders.is_empty() {
                let _ = writeln!(s, "- {}", tr("Top long-task sources:", "长任务主要来源："),);
                for (i, o) in v.long_task_top_offenders.iter().take(3).enumerate() {
                    let src = if o.source.len() > 70 {
                        format!("…{}", &o.source[o.source.len() - 67..])
                    } else {
                        o.source.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  {}. `{}` — **{:.0}ms** {} {} {} ({} **{:.0}ms**)",
                        i + 1,
                        src,
                        o.total_duration_ms,
                        tr("across", "共"),
                        o.task_count,
                        tr(if o.task_count == 1 { "task" } else { "tasks" }, "次任务",),
                        tr("max", "最长"),
                        o.max_duration_ms,
                    );
                }
            }
            // LoAF — only render when something was observed (older
            // Chromium without the API returns count=0).
            if v.loaf_count > 0 {
                let _ = writeln!(
                    s,
                    "- {}: **{}** ({} **{:.0}ms** {})",
                    tr("Long Animation Frames", "长动画帧"),
                    v.loaf_count,
                    tr("blocking", "阻塞"),
                    v.loaf_total_blocking_duration,
                    tr("total", "总计"),
                );
                if !v.loaf_top_offenders.is_empty() {
                    let _ = writeln!(s, "- {}", tr("Top LoAF offenders:", "LoAF 主要肇事脚本："));
                    for (i, o) in v.loaf_top_offenders.iter().take(3).enumerate() {
                        let src = if o.source_url.is_empty() {
                            "(inline / unknown)".to_string()
                        } else if o.source_url.len() > 70 {
                            format!("…{}", &o.source_url[o.source_url.len() - 67..])
                        } else {
                            o.source_url.clone()
                        };
                        let fn_note = if !o.source_function_name.is_empty() {
                            format!(" `{}()`", o.source_function_name)
                        } else {
                            String::new()
                        };
                        let reflow_note = if o.total_forced_style_layout_ms > 5.0 {
                            format!(
                                " ⚠️ {} **{:.0}ms**",
                                tr("forced reflow", "强制重排"),
                                o.total_forced_style_layout_ms,
                            )
                        } else {
                            String::new()
                        };
                        let _ = writeln!(
                            s,
                            "  {}. `{}`{} — **{:.0}ms** {} {} {}{}",
                            i + 1,
                            src,
                            fn_note,
                            o.total_duration_ms,
                            tr("over", "共"),
                            o.invocation_count,
                            tr(
                                if o.invocation_count == 1 {
                                    "call"
                                } else {
                                    "calls"
                                },
                                "次调用",
                            ),
                            reflow_note,
                        );
                    }
                }
            }
            let _ = writeln!(s);
        }

        if let Some(dt) = &self.document_timing {
            let _ = writeln!(s, "{}", tr("## Document Timing", "## 主文档时序"));
            let _ = writeln!(s);
            let url_display = if dt.url.len() > 80 {
                format!("…{}", &dt.url[dt.url.len() - 77..])
            } else {
                dt.url.clone()
            };
            let _ = writeln!(
                s,
                "- `{}` — {} · {}{}",
                url_display,
                dt.status,
                if dt.protocol.is_empty() {
                    tr("(no protocol)", "(未知协议)")
                } else {
                    &dt.protocol
                },
                if dt.from_cache {
                    tr(" · cached", " · 来自缓存")
                } else {
                    ""
                },
            );
            let _ = writeln!(
                s,
                "- DNS **{}ms** · TCP **{}ms** · TLS **{}ms** · TTFB **{}ms**",
                dt.dns_ms, dt.tcp_ms, dt.tls_ms, dt.ttfb_ms,
            );
            let _ = writeln!(s);
        }

        // HTTP errors — placed right after Document Timing because the
        // two answer the same "did navigation actually succeed" question.
        // Section is silent on the trivial happy path (no 4xx/5xx, no
        // network failures, no redirects) so clean pages don't add
        // noise.
        if let Some(he) = &self.http_errors
            && (he.failed_count > 0 || he.redirect_count > 0)
        {
            let _ = writeln!(s, "{}", tr("## HTTP Errors", "## HTTP 错误"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}** ({} 4xx · {} 5xx · {} {})",
                tr("Failed requests", "失败请求"),
                he.failed_count,
                he.failed_4xx.len(),
                he.failed_5xx.len(),
                he.network_failures.len(),
                tr("network failures", "网络层失败"),
            );
            if he.redirect_count > 0 {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} → `{}`",
                    tr("Redirects", "重定向"),
                    he.redirect_count,
                    tr(
                        if he.redirect_count == 1 {
                            "hop"
                        } else {
                            "hops"
                        },
                        "跳",
                    ),
                    he.final_url,
                );
            }
            for r in he.failed_4xx.iter().take(5) {
                let display = if r.url.len() > 80 {
                    format!("…{}", &r.url[r.url.len() - 77..])
                } else {
                    r.url.clone()
                };
                let _ = writeln!(
                    s,
                    "  - **{}** [{}] `{}`",
                    r.status, r.resource_type, display,
                );
            }
            for r in he.failed_5xx.iter().take(5) {
                let display = if r.url.len() > 80 {
                    format!("…{}", &r.url[r.url.len() - 77..])
                } else {
                    r.url.clone()
                };
                let _ = writeln!(
                    s,
                    "  - **{}** [{}] `{}`",
                    r.status, r.resource_type, display,
                );
            }
            // Network failures — separate sub-list because they're a
            // different failure class (no response at all). Skip
            // `canceled=true` entries (typical: navigation supersession,
            // block_urls policy) so the reader sees real findings first.
            for f in he.network_failures.iter().filter(|f| !f.canceled).take(5) {
                let display = if f.url.len() > 80 {
                    format!("…{}", &f.url[f.url.len() - 77..])
                } else {
                    f.url.clone()
                };
                let _ = writeln!(
                    s,
                    "  - ⚠️ [{}] `{}` — `{}`",
                    f.resource_type, display, f.error_text,
                );
            }
            let _ = writeln!(s);
        }

        if !self.resource_summary.bytes_by_type.is_empty() {
            let rs = &self.resource_summary;
            let _ = writeln!(s, "{}", tr("## Resource Summary", "## 资源汇总"));
            let _ = writeln!(s);
            // Sort by bytes desc for stable readable output.
            let mut by_type: Vec<(&String, &u64)> = rs.bytes_by_type.iter().collect();
            by_type.sort_by(|a, b| b.1.cmp(a.1));
            let type_line = by_type
                .iter()
                .map(|(k, v)| {
                    let n = rs.count_by_type.get(*k).copied().unwrap_or(0);
                    format!("{} {} ({})", k, format_bytes(**v), n)
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let _ = writeln!(s, "- {}: {type_line}", tr("By type", "按类型"));
            let mut status: Vec<(&String, &u32)> = rs.status_distribution.iter().collect();
            status.sort_by_key(|x| x.0.clone());
            let status_line = status
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" · ");
            let _ = writeln!(s, "- {}: {status_line}", tr("Status", "状态码"));
            let _ = writeln!(
                s,
                "- {} **{:.0}%** ({} {})",
                tr("Cache hit ratio", "缓存命中率"),
                rs.cache_hit_ratio * 100.0,
                tr("saved", "节省"),
                format_bytes(rs.cached_bytes),
            );
            let _ = writeln!(
                s,
                "- {}: **{}**",
                tr("Third-party bytes", "第三方字节数"),
                format_bytes(rs.third_party_bytes),
            );
            // Top third-party domains by bytes — ranks the heaviest
            // external dependencies for an AI-scannable view.
            if !rs.top_third_party_domains.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr("Top third-party domains:", "第三方域名 TOP："),
                );
                for d in rs.top_third_party_domains.iter().take(5) {
                    let _ = writeln!(
                        s,
                        "  - `{}` — {} ({} {})",
                        d.host,
                        format_bytes(d.bytes),
                        d.count,
                        tr(
                            if d.count == 1 {
                                "resource"
                            } else {
                                "resources"
                            },
                            "个资源",
                        ),
                    );
                }
            }
            // Third-party executable-JS origins — the supply-chain attack
            // surface. Only emitted when the page loaded external JS.
            if !rs.third_party_script_origins.is_empty() {
                let _ = writeln!(
                    s,
                    "- {} (**{}** {}, {} {}):",
                    tr("Third-party JS origins", "第三方 JS 来源"),
                    rs.third_party_script_origins.len(),
                    tr("origins", "个来源"),
                    format_bytes(rs.third_party_script_bytes),
                    tr("total", "合计"),
                );
                for d in rs.third_party_script_origins.iter().take(5) {
                    let _ = writeln!(
                        s,
                        "  - `{}` — {} ({} {})",
                        d.host,
                        format_bytes(d.bytes),
                        d.count,
                        tr(if d.count == 1 { "script" } else { "scripts" }, "个脚本",),
                    );
                }
            }
            if let Some((url, sz)) = &rs.largest_resource {
                let _ = writeln!(
                    s,
                    "- {}: `{url}` ({})",
                    tr("Largest", "最大资源"),
                    format_bytes(*sz),
                );
            }
            // HTTP version distribution — sort by count desc so the
            // dominant protocol leads. Adjacent line shows the modern-
            // protocol scalar so AI can alert on a single ratio drop.
            if !rs.protocol_distribution.is_empty() {
                let mut proto: Vec<(&String, &u32)> = rs.protocol_distribution.iter().collect();
                proto.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let line = proto
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(
                    s,
                    "- {}: {line} ({} **{:.0}%**)",
                    tr("HTTP versions", "HTTP 版本"),
                    tr("HTTP/2+3 share", "HTTP/2+3 占比"),
                    rs.modern_protocol_share * 100.0,
                );
            }
            // Connection reuse + DNS approximation. Skip if no real
            // network resources (everything cached).
            let real_conns = rs.connections_reused + rs.connections_new;
            if real_conns > 0 {
                let reuse_pct = (rs.connections_reused as f64) * 100.0 / (real_conns as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} · **{}** {} (**{:.0}%** {}) · **{}** {}",
                    tr("Connections", "连接"),
                    rs.connections_reused,
                    tr("reused", "复用"),
                    rs.connections_new,
                    tr("new", "新建"),
                    reuse_pct,
                    tr("reuse", "复用率"),
                    rs.unique_hosts,
                    tr("unique hosts", "个独立主机"),
                );
            }
            // Compression audit. Only render when there's either
            // compression in use or a miss to flag.
            if rs.compressed_count > 0 || rs.uncompressed_text_count > 0 {
                let mut line = format!(
                    "- {}: **{}** {}",
                    tr("Compression", "压缩"),
                    rs.compressed_count,
                    tr("compressed", "已压缩"),
                );
                if rs.uncompressed_text_count > 0 {
                    line.push_str(&format!(
                        " · **{}** {} (**{}** {}) ⚠️",
                        rs.uncompressed_text_count,
                        tr("uncompressed text resources", "个未压缩的文本资源",),
                        format_bytes(rs.uncompressed_text_bytes),
                        tr("could be compressed", "本可压缩"),
                    ));
                }
                let _ = writeln!(s, "{line}");
            }
            // Compression algorithm breakdown — sort gzip/br/zstd by
            // count desc so the dominant codec leads.
            if !rs.compression_breakdown.is_empty() {
                let mut algos: Vec<(&String, &u32)> = rs.compression_breakdown.iter().collect();
                algos.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let line = algos
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(
                    s,
                    "- {}: {line}",
                    tr("Compression breakdown", "压缩算法分布"),
                );
            }
            // Cache-Control coverage — single ratio so monitors can
            // alert when a deploy drops headers from static assets.
            let cc_total = rs.cache_control_present + rs.cache_control_missing;
            if cc_total > 0 {
                let cov = (rs.cache_control_present as f64) * 100.0 / (cc_total as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{:.0}%** ({} {} · {} {})",
                    tr("Cache-Control coverage", "Cache-Control 覆盖率"),
                    cov,
                    rs.cache_control_present,
                    tr("present", "已设置"),
                    rs.cache_control_missing,
                    tr("missing", "未设置"),
                );
            }
            // Image-format modernisation — Lighthouse "Serve images
            // in next-gen formats" signal.
            let img_total = rs.legacy_image_bytes + rs.modern_image_bytes;
            if img_total > 0 {
                let modern_pct = (rs.modern_image_bytes as f64) * 100.0 / (img_total as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} (JPEG/PNG/GIF) · **{}** {} (WebP/AVIF) — **{:.0}%** {}",
                    tr("Image formats", "图片格式"),
                    format_bytes(rs.legacy_image_bytes),
                    tr("legacy", "传统格式"),
                    format_bytes(rs.modern_image_bytes),
                    tr("modern", "现代格式"),
                    modern_pct,
                    tr("modern", "现代格式占比"),
                );
            }
            // Source-map coverage across JS / CSS.
            let sm_total = rs.source_maps_present + rs.source_maps_missing;
            if sm_total > 0 {
                let cov = (rs.source_maps_present as f64) * 100.0 / (sm_total as f64);
                let _ = writeln!(
                    s,
                    "- {}: **{:.0}%** {} ({} {} · {} {})",
                    tr("Source maps", "Source map"),
                    cov,
                    tr("of JS/CSS resources", "JS/CSS 资源覆盖"),
                    rs.source_maps_present,
                    tr("present", "已发布"),
                    rs.source_maps_missing,
                    tr("missing", "未发布"),
                );
            }
            // Duplicate-resource findings — only render when something
            // was detected; otherwise stay silent (empty lists carry no
            // information for a markdown reader).
            let dr = &rs.duplicate_resources;
            if dr.wasted_bytes > 0 || !dr.exact_url.is_empty() || !dr.likely_same_file.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} {} {}, {} {} ⚠️",
                    tr("Duplicate resources", "重复资源"),
                    format_bytes(dr.wasted_bytes),
                    tr("wasted across", "浪费，分布于"),
                    dr.exact_url.len(),
                    tr(
                        if dr.exact_url.len() == 1 {
                            "exact-URL group"
                        } else {
                            "exact-URL groups"
                        },
                        "组同 URL 重复",
                    ),
                    dr.likely_same_file.len(),
                    tr(
                        if dr.likely_same_file.len() == 1 {
                            "likely-same-file group"
                        } else {
                            "likely-same-file groups"
                        },
                        "组疑似同文件",
                    ),
                );
                for e in dr.exact_url.iter().take(3) {
                    let display = if e.key.len() > 80 {
                        format!("…{}", &e.key[e.key.len() - 77..])
                    } else {
                        e.key.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - {}: `{}` ×{} ({} {})",
                        tr("exact", "同 URL"),
                        display,
                        e.count,
                        format_bytes(e.wasted_bytes),
                        tr("wasted", "浪费"),
                    );
                }
                for e in dr.likely_same_file.iter().take(3) {
                    let _ = writeln!(
                        s,
                        "  - {}: `{}` {} {} URLs ({} {})",
                        tr("same-file", "同文件"),
                        e.key,
                        tr("across", "分布于"),
                        e.count,
                        format_bytes(e.wasted_bytes),
                        tr("wasted", "浪费"),
                    );
                }
            }
            // Mixed content — only render when detected. Clean HTTPS
            // pages and HTTP-served pages stay silent.
            let mc = &rs.mixed_content;
            if mc.detected {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} ⚠️",
                    tr("Mixed content", "混合内容"),
                    mc.total_count,
                    tr(
                        if mc.total_count == 1 {
                            "plain-HTTP resource on HTTPS page"
                        } else {
                            "plain-HTTP resources on HTTPS page"
                        },
                        "个明文 HTTP 资源出现在 HTTPS 页面",
                    ),
                );
                for r in mc.resources.iter().take(3) {
                    let display = if r.url.len() > 80 {
                        format!("…{}", &r.url[r.url.len() - 77..])
                    } else {
                        r.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - [{}] `{}` ({})",
                        r.kind,
                        display,
                        format_bytes(r.content_size),
                    );
                }
            }
            // Critical-chain depth — only render when initiators were
            // captured (`None` means `initiators=false`, value would be
            // meaningless). `0` is a real signal too: every resource
            // was parser-initiated, no JS-driven secondary fetches.
            if let Some(depth) = rs.max_initiator_chain_depth {
                let _ = writeln!(
                    s,
                    "- {}: **{depth}**",
                    tr("Max initiator chain depth", "最深请求依赖链",),
                );
            }
            // Per-type "largest resources" leaderboards. Stable bucket
            // order so the markdown diffs cleanly across captures.
            if !rs.top_largest_by_type.is_empty() {
                for bucket in ["javascript", "css", "image", "font"] {
                    let Some(list) = rs.top_largest_by_type.get(bucket) else {
                        continue;
                    };
                    if list.is_empty() {
                        continue;
                    }
                    let _ = writeln!(s, "- {} {bucket}:", tr("Largest", "最大"),);
                    for e in list.iter().take(5) {
                        let display = if e.url.len() > 80 {
                            format!("…{}", &e.url[e.url.len() - 77..])
                        } else {
                            e.url.clone()
                        };
                        let cache_tag = if e.from_cache {
                            tr(" (cached)", "（来自缓存）")
                        } else {
                            ""
                        };
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}{}",
                            display,
                            format_bytes(e.bytes),
                            cache_tag,
                        );
                    }
                }
            }
            // Uncompressed-text offenders — already summarised in the
            // compression line above; this section drills into specific
            // URLs so the AI can suggest concrete fixes.
            if !rs.uncompressed_text_resources.is_empty() {
                let _ = writeln!(
                    s,
                    "- {} ({} {}):",
                    tr("Uncompressed text resources", "未压缩的文本资源",),
                    tr("top", "前"),
                    rs.uncompressed_text_resources.len().min(5),
                );
                for e in rs.uncompressed_text_resources.iter().take(5) {
                    let display = if e.url.len() > 80 {
                        format!("…{}", &e.url[e.url.len() - 77..])
                    } else {
                        e.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - `{}` — {} ({})",
                        display,
                        format_bytes(e.bytes),
                        e.mime_type,
                    );
                }
            }
            // Cache-policy anti-patterns on static assets — surfaces
            // the actionable subset (short max-age + missing-immutable
            // on fingerprinted URLs) without paging through resources.
            if !rs.cache_policy_issues.is_empty() {
                let short_count = rs
                    .cache_policy_issues
                    .iter()
                    .filter(|i| i.reason == "short_max_age")
                    .count();
                let immut_count = rs
                    .cache_policy_issues
                    .iter()
                    .filter(|i| i.reason == "missing_immutable")
                    .count();
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} · **{}** {} ⚠️",
                    tr("Cache-policy issues", "缓存策略问题"),
                    short_count,
                    tr("short max-age", "max-age 过短"),
                    immut_count,
                    tr("missing immutable", "未加 immutable"),
                );
                for e in rs.cache_policy_issues.iter().take(5) {
                    let display = if e.url.len() > 80 {
                        format!("…{}", &e.url[e.url.len() - 77..])
                    } else {
                        e.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - [{}] `{}` — `{}`",
                        e.reason, display, e.cache_control,
                    );
                }
            }
            // Resource-hint audit — only rendered when the caller
            // opted in (`resource_hints=true` / `all_metrics=true`).
            // `gap` empty AND both declared lists empty → silent;
            // otherwise show the gap (highest priority for the AI)
            // and a one-line summary of declared coverage.
            if let Some(rh) = &rs.resource_hints {
                let declared_total = rh.declared_preconnect.len() + rh.declared_dns_prefetch.len();
                if !rh.gap.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {} ⚠️",
                        tr("Resource-hint gaps", "资源提示遗漏"),
                        rh.gap.len(),
                        tr(
                            if rh.gap.len() == 1 {
                                "third-party host hit without preconnect/dns-prefetch"
                            } else {
                                "third-party hosts hit without preconnect/dns-prefetch"
                            },
                            "个第三方主机命中但未声明 preconnect/dns-prefetch",
                        ),
                    );
                    for g in rh.gap.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {} ({} {})",
                            g.host,
                            format_bytes(g.bytes),
                            g.count,
                            tr(
                                if g.count == 1 {
                                    "resource"
                                } else {
                                    "resources"
                                },
                                "个资源",
                            ),
                        );
                    }
                }
                if declared_total > 0 {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** preconnect · **{}** dns-prefetch",
                        tr("Declared resource hints", "已声明的资源提示"),
                        rh.declared_preconnect.len(),
                        rh.declared_dns_prefetch.len(),
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(cov) = &self.coverage {
            let _ = writeln!(s, "{}", tr("## CSS / JS Coverage", "## CSS / JS 覆盖率"));
            let _ = writeln!(s);
            if cov.js_total_bytes > 0 {
                let _ = writeln!(
                    s,
                    "- JS: **{}** {} / {} {} (**{:.0}%** {})",
                    format_bytes(cov.js_unused_bytes),
                    tr("unused", "未使用"),
                    format_bytes(cov.js_total_bytes),
                    tr("total", "总计"),
                    cov.js_unused_ratio * 100.0,
                    tr("unused", "未使用"),
                );
            }
            if cov.css_total_bytes > 0 {
                let _ = writeln!(
                    s,
                    "- CSS: **{}** {} / {} {} (**{:.0}%** {})",
                    format_bytes(cov.css_unused_bytes),
                    tr("unused", "未使用"),
                    format_bytes(cov.css_total_bytes),
                    tr("total", "总计"),
                    cov.css_unused_ratio * 100.0,
                    tr("unused", "未使用"),
                );
            }
            if !cov.top_unused.is_empty() {
                let _ = writeln!(s, "- {}", tr("Top wasteful files:", "最浪费的文件："));
                for e in cov.top_unused.iter().take(5) {
                    let display_url = if e.url.len() > 80 {
                        format!("…{}", &e.url[e.url.len() - 77..])
                    } else {
                        e.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "  - [{}] `{}` — {} {} ({:.0}%)",
                        e.kind,
                        display_url,
                        format_bytes(e.unused_bytes),
                        tr("unused", "未使用"),
                        e.unused_ratio * 100.0,
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(tls) = &self.tls_info {
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "## TLS / Certificate (main document)",
                    "## TLS / 主文档证书",
                ),
            );
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: `{}`{}",
                tr("Host", "主机"),
                tls.host,
                format_remote_ip(tls),
            );
            let _ = writeln!(
                s,
                "- {}: **{}** · {}: `{}`{}",
                tr("Protocol", "协议"),
                tls.protocol,
                tr("cipher", "加密套件"),
                tls.cipher,
                match &tls.key_exchange {
                    Some(k) => format!(" · {}: `{k}`", tr("key exchange", "密钥交换"),),
                    None => String::new(),
                },
            );
            let _ = writeln!(s, "- {}: `{}`", tr("Subject", "签发对象"), tls.subject_name);
            let _ = writeln!(s, "- {}: `{}`", tr("Issuer", "颁发机构"), tls.issuer);
            let _ = writeln!(
                s,
                "- {}: {}",
                tr("Validity", "有效期"),
                format_tls_expiry(tls.days_remaining),
            );
            if !tls.san_list.is_empty() {
                let sans = tls
                    .san_list
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(
                    s,
                    "- SANs ({}): {}{}",
                    tls.san_list.len(),
                    sans,
                    if tls.san_list.len() > 8 { ", …" } else { "" },
                );
            }
            let _ = writeln!(s);
        }

        if !self.tls_certificates.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## TLS Certificates by Host", "## 按主机分组的 TLS 证书",),
                self.tls_certificates.len(),
            );
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "| Host | IP | Protocol | Issuer | Validity |",
                    "| 主机 | IP | 协议 | 颁发机构 | 有效期 |",
                ),
            );
            let _ = writeln!(s, "|---|---|---|---|---|");
            for tls in &self.tls_certificates {
                let ip_cell = match (&tls.remote_ip, tls.remote_port) {
                    (Some(ip), Some(443)) => format!("`{ip}`"),
                    (Some(ip), Some(p)) => format!("`{ip}:{p}`"),
                    (Some(ip), None) => format!("`{ip}`"),
                    (None, _) => String::from("—"),
                };
                let _ = writeln!(
                    s,
                    "| `{}` | {} | {} | {} | {} |",
                    tls.host,
                    ip_cell,
                    tls.protocol,
                    tls.issuer,
                    format_tls_expiry(tls.days_remaining),
                );
            }
            let _ = writeln!(s);
        }

        // Security audit scorecard — rendered as a compact 2-line view
        // so AI can see headers score + cookie coverage without scanning
        // the full headers map below. Always emitted (the struct is
        // always populated).
        {
            let a = &self.security_audit;
            let _ = writeln!(s, "{}", tr("## Security Audit", "## 安全审计"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}/{}** {}{}",
                tr("Headers", "响应头"),
                a.headers.present_count,
                CORE_SECURITY_HEADERS.len(),
                tr("core present", "核心头已配置"),
                if a.headers.missing.is_empty() {
                    String::new()
                } else {
                    format!(
                        " — {}: {}",
                        tr("missing", "缺失"),
                        a.headers.missing.join(", "),
                    )
                },
            );
            // CSP strength — only when an enforcing policy exists. A
            // present-but-weak CSP is the finding the bool hides.
            if let Some(csp) = &a.headers.csp_analysis {
                if csp.weaknesses.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {} ✅ {}",
                        tr("CSP", "CSP"),
                        csp.directive_count,
                        tr("directives", "条指令"),
                        tr("no obvious weaknesses", "无明显弱点"),
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {} ⚠️ {}: {}",
                        tr("CSP", "CSP"),
                        csp.directive_count,
                        tr("directives", "条指令"),
                        tr("weaknesses", "弱点"),
                        csp.weaknesses.join(", "),
                    );
                }
            }
            // HSTS strength — distinguish a real policy from max-age=0.
            if let Some(hsts) = &a.headers.hsts_analysis {
                let age = match hsts.max_age {
                    Some(m) => format!("max-age={m}"),
                    None => tr("max-age missing", "缺 max-age").to_string(),
                };
                let mut flags = String::new();
                if hsts.include_subdomains {
                    flags.push_str(" +includeSubDomains");
                }
                if hsts.preload {
                    flags.push_str(" +preload");
                }
                let warn = if hsts.effective { "" } else { " ⚠️" };
                let _ = writeln!(s, "- {}: {age}{flags}{warn}", tr("HSTS", "HSTS"));
            }
            if a.cookies.total > 0 {
                let pct = |n: u32| (n as f64) * 100.0 / (a.cookies.total as f64);
                let mut line = format!(
                    "- {} ({}): Secure **{:.0}%** · HttpOnly **{:.0}%** · SameSite **{:.0}%**",
                    tr("Cookies", "Cookie"),
                    a.cookies.total,
                    pct(a.cookies.secure),
                    pct(a.cookies.http_only),
                    pct(a.cookies.same_site_set),
                );
                if a.cookies.same_site_none_without_secure > 0 {
                    line.push_str(&format!(
                        " ⚠️ {} {}",
                        a.cookies.same_site_none_without_secure,
                        tr(
                            "cookie(s) `SameSite=None` without `Secure`",
                            "个 Cookie 标了 `SameSite=None` 却没加 `Secure`",
                        ),
                    ));
                }
                let _ = writeln!(s, "{line}");
                // Cookie header byte size — flag when approaching the
                // 4 KB framework limit. Otherwise stay quiet (most
                // pages have tiny cookies).
                let hdr = a.cookies.header_bytes;
                if hdr >= 4096 {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** ⚠️ {}",
                        tr("Cookie header size", "Cookie 请求头大小"),
                        format_bytes(hdr),
                        tr(
                            "(≥ 4 KB — every request pays this tax)",
                            "(≥ 4 KB —— 每个请求都要带这么多)",
                        ),
                    );
                } else if hdr > 0 {
                    let _ = writeln!(
                        s,
                        "- {}: **{}**",
                        tr("Cookie header size", "Cookie 请求头大小"),
                        format_bytes(hdr),
                    );
                }
            } else {
                let _ = writeln!(
                    s,
                    "- {}: {}",
                    tr("Cookies", "Cookie"),
                    tr("(none)", "（无）")
                );
            }
            let _ = writeln!(s);
        }

        if let Some(sh) = &self.security_headers {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Security Headers", "## 安全响应头"),
                sh.len(),
            );
            let _ = writeln!(s);
            let mut items: Vec<(&String, &String)> = sh.iter().collect();
            items.sort_by_key(|(k, _)| k.as_str());
            for (k, v) in items {
                // Truncate very long CSP values for readability.
                let val = if v.len() > 200 {
                    format!("{}…", &v[..200])
                } else {
                    v.clone()
                };
                let _ = writeln!(s, "- `{k}`: {val}");
            }
            let _ = writeln!(s);
        }

        if let Some(scan) = &self.security_scan {
            let _ = writeln!(s, "{}", tr("## Security Scan", "## 安全扫描"));
            let _ = writeln!(s);
            // SRI coverage.
            let sri = &scan.sri;
            if sri.total_cross_origin > 0 {
                let _ = writeln!(
                    s,
                    "- {}: **{}/{}** {}",
                    tr("SRI coverage", "SRI 覆盖"),
                    sri.protected,
                    sri.total_cross_origin,
                    tr(
                        "cross-origin subresources protected",
                        "个跨域子资源已加 integrity",
                    ),
                );
                for g in sri.missing.iter().take(5) {
                    let _ = writeln!(s, "  - ⚠️ `<{}>` `{}`", g.tag, g.url);
                }
                if sri.missing.len() > 5 {
                    let _ = writeln!(
                        s,
                        "  - {} {} {}",
                        tr("…and", "……还有"),
                        sri.missing.len() - 5,
                        tr("more without integrity", "个缺 integrity"),
                    );
                }
            } else {
                let _ = writeln!(
                    s,
                    "- {}: {}",
                    tr("SRI coverage", "SRI 覆盖"),
                    tr("no cross-origin subresources", "无跨域子资源"),
                );
            }
            // Unsafe target=_blank.
            if !scan.unsafe_target_blank.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {}",
                    tr("Unsafe `target=_blank`", "不安全的 `target=_blank`"),
                    scan.unsafe_target_blank.len(),
                    tr(
                        "link(s) with explicit `rel=opener`",
                        "个链接显式带 `rel=opener`",
                    ),
                );
            }
            // Form security.
            let f = &scan.forms;
            if f.total > 0 {
                let mut bits: Vec<String> = Vec::new();
                if !f.insecure_action.is_empty() {
                    bits.push(format!(
                        "{} {}",
                        f.insecure_action.len(),
                        tr("cleartext action(s)", "个明文 action"),
                    ));
                }
                if f.password_on_insecure_page > 0 {
                    bits.push(format!(
                        "{} {}",
                        f.password_on_insecure_page,
                        tr("password field(s) on HTTP", "个 HTTP 页密码框"),
                    ));
                }
                if bits.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {} ({}): {}",
                        tr("Forms", "表单"),
                        f.total,
                        tr("no issues", "无问题"),
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "- {} ({}): ⚠️ {}",
                        tr("Forms", "表单"),
                        f.total,
                        bits.join(" · "),
                    );
                }
            }
            // CORS issues.
            if !scan.cors_issues.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {}",
                    tr("CORS misconfig", "CORS 配置错误"),
                    scan.cors_issues.len(),
                    tr(
                        "response(s) with `ACAO:*` + credentials",
                        "个响应同时带 `ACAO:*` 和 credentials",
                    ),
                );
                for c in scan.cors_issues.iter().take(5) {
                    let _ = writeln!(s, "  - ⚠️ `{}` (`{}`)", c.url, c.allow_origin);
                }
            }
            // Library fingerprint.
            if !scan.libraries.is_empty() {
                let list = scan
                    .libraries
                    .iter()
                    .map(|l| match &l.version {
                        Some(v) => format!("{} {}", l.name, v),
                        None => l.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(s, "- {}: {}", tr("Libraries", "检测到的库"), list);
            }
            let _ = writeln!(s);
        }

        if let Some(sw) = &self.service_worker {
            let _ = writeln!(s, "{}", tr("## Service Worker", "## Service Worker"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}**",
                tr("Controlled", "已接管页面"),
                sw.controlled,
            );
            if let Some(scope) = &sw.scope {
                let _ = writeln!(s, "- {}: `{scope}`", tr("Scope", "作用域"));
            }
            if let Some(script) = &sw.active_script {
                let _ = writeln!(s, "- {}: `{script}`", tr("Active script", "激活的脚本"));
            }
            if sw.waiting {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr("Update **waiting** for activation", "有更新**等待**激活",),
                );
            }
            if sw.installing {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr("A SW is **installing**", "正在**安装** Service Worker"),
                );
            }
            let _ = writeln!(s);
        }

        if let Some(rb) = &self.render_blocking_resources {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Render-Blocking Resources", "## 阻塞渲染的资源",),
                rb.len(),
            );
            let _ = writeln!(s);
            if rb.is_empty() {
                let _ = writeln!(s, "- {}", tr("None detected.", "未发现。"));
            } else {
                for r in rb.iter().take(10) {
                    let _ = writeln!(s, "- `<{}>` `{}` — {}", r.tag, r.url, r.why);
                }
                if rb.len() > 10 {
                    let _ = writeln!(
                        s,
                        "- {} {} {}.",
                        tr("…and", "……还有"),
                        rb.len() - 10,
                        tr("more", "项更多"),
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(imgs) = &self.image_sizing {
            // Headline summary: counts + how many are wasteful enough to
            // matter (>50% waste AND >50KB transferred, or in-viewport
            // with any oversize). Empty list still rendered for "audited
            // but clean" signal.
            let total = imgs.len();
            let loaded = imgs.iter().filter(|i| i.loaded).count();
            let lazy_offscreen = imgs
                .iter()
                .filter(|i| i.loading == "lazy" && !i.in_viewport)
                .count();
            let alt_missing = imgs.iter().filter(|i| i.alt_missing).count();
            let _ = writeln!(s, "{} ({total})", tr("## Image Sizing", "## 图片尺寸审计"),);
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {loaded} {} · {lazy_offscreen} {} · {alt_missing} {}",
                tr("loaded", "已加载"),
                tr("lazy/off-screen", "懒加载/首屏外"),
                tr("without alt", "缺 alt"),
            );
            // Top offenders: significant waste OR meaningful bytes.
            let top: Vec<&ImageSizing> = imgs
                .iter()
                .filter(|i| {
                    i.loaded
                        && i.waste_ratio.map(|w| w >= 0.4).unwrap_or(false)
                        && i.transferred_bytes.map(|b| b >= 20_000).unwrap_or(true)
                })
                .take(10)
                .collect();
            if top.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}",
                    tr(
                        "No significantly oversized images detected.",
                        "未发现明显过大的图片。",
                    ),
                );
            } else {
                let _ = writeln!(s);
                let _ = writeln!(
                    s,
                    "{}",
                    tr(
                        "| URL | Natural | Display | Waste | Bytes | Viewport |",
                        "| URL | 原生尺寸 | 显示尺寸 | 浪费 | 字节 | 首屏 |",
                    ),
                );
                let _ = writeln!(s, "|---|---|---|---|---|---|");
                for i in &top {
                    let waste = i
                        .waste_ratio
                        .map(|w| format!("{:.0}%", w * 100.0))
                        .unwrap_or_else(|| "?".into());
                    let bytes = i
                        .transferred_bytes
                        .map(format_bytes)
                        .unwrap_or_else(|| "?".into());
                    let vp = if i.in_viewport {
                        tr("**yes**", "**是**")
                    } else {
                        tr("no", "否")
                    };
                    // Trim long URLs to keep the table readable.
                    let short_url = if i.url.len() > 60 {
                        format!("…{}", &i.url[i.url.len() - 57..])
                    } else {
                        i.url.clone()
                    };
                    let _ = writeln!(
                        s,
                        "| `{}` | {}×{} | {}×{} | **{}** | {} | {} |",
                        short_url,
                        i.natural_width,
                        i.natural_height,
                        i.display_width,
                        i.display_height,
                        waste,
                        bytes,
                        vp,
                    );
                }
            }
            // Lighthouse "image" four-pack — one short subsection per
            // category, each silent when its list is empty. Showing the
            // top URL + key numbers (display W×H, oversize ratio) keeps
            // the markdown skimmable while still pinning down which
            // file is the worst offender.
            if let Some(audit) = &self.image_audit {
                let trim_url = |u: &str| -> String {
                    if u.len() > 60 {
                        format!("…{}", &u[u.len() - 57..])
                    } else {
                        u.to_string()
                    }
                };
                if !audit.oversized.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {}",
                        tr("Oversized (>2× display)", "过大（> 显示尺寸 2 倍）",),
                        audit.oversized.len(),
                        tr(
                            if audit.oversized.len() == 1 {
                                "image"
                            } else {
                                "images"
                            },
                            "张图片",
                        ),
                    );
                    for i in audit.oversized.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — **{:.1}×** {} {}×{}",
                            trim_url(&i.url),
                            i.ratio,
                            tr("at", "显示为"),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
                if !audit.missing_dimensions.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {}",
                        tr(
                            "Missing `width`/`height` attrs (CLS risk)",
                            "缺 `width`/`height` 属性（CLS 风险）",
                        ),
                        audit.missing_dimensions.len(),
                        tr(
                            if audit.missing_dimensions.len() == 1 {
                                "image"
                            } else {
                                "images"
                            },
                            "张图片",
                        ),
                    );
                    for i in audit.missing_dimensions.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}×{}",
                            trim_url(&i.url),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
                if !audit.missing_lazy.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}**",
                        tr(
                            "Below-fold images NOT marked `loading=\"lazy\"`",
                            "首屏外图片未加 `loading=\"lazy\"`",
                        ),
                        audit.missing_lazy.len(),
                    );
                    for i in audit.missing_lazy.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}×{}",
                            trim_url(&i.url),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
                if !audit.missing_srcset.is_empty() {
                    let _ = writeln!(
                        s,
                        "- {}: **{}** {}",
                        tr(
                            "Missing `srcset` (no responsive variants)",
                            "缺 `srcset`（没有响应式变体）",
                        ),
                        audit.missing_srcset.len(),
                        tr(
                            if audit.missing_srcset.len() == 1 {
                                "image"
                            } else {
                                "images"
                            },
                            "张图片",
                        ),
                    );
                    for i in audit.missing_srcset.iter().take(5) {
                        let _ = writeln!(
                            s,
                            "  - `{}` — {}×{}",
                            trim_url(&i.url),
                            i.display_width,
                            i.display_height,
                        );
                    }
                }
            }
            let _ = writeln!(s);
        }

        if let Some(fa) = &self.font_audit {
            let _ = writeln!(s, "{}", tr("## Font Audit", "## 字体审计"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {}: **{}** {} · **{}** {} · **{}** {}",
                tr("Fonts", "字体"),
                fa.font_count,
                tr("declared", "已声明"),
                fa.loaded_count,
                tr("loaded", "已加载"),
                fa.declared_preload_count,
                tr("preloaded", "已预加载"),
            );
            // font-display distribution — sort desc by count so the
            // dominant value leads, ties broken alphabetically for
            // stable diffs across captures.
            if !fa.display_distribution.is_empty() {
                let mut dist: Vec<(&String, &u32)> = fa.display_distribution.iter().collect();
                dist.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let line = dist
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(s, "- `font-display`: {line}");
            }
            if !fa.missing_swap.is_empty() {
                let _ = writeln!(
                    s,
                    "- {}: **{}** {} ⚠️",
                    tr(
                        "FOIT risk (no `font-display: swap`)",
                        "FOIT 风险（未声明 `font-display: swap`）",
                    ),
                    fa.missing_swap.len(),
                    tr(
                        if fa.missing_swap.len() == 1 {
                            "face"
                        } else {
                            "faces"
                        },
                        "个字体",
                    ),
                );
                for f in fa.missing_swap.iter().take(5) {
                    let url_part = match &f.source_url {
                        Some(u) => {
                            let trimmed = if u.len() > 70 {
                                format!("…{}", &u[u.len() - 67..])
                            } else {
                                u.clone()
                            };
                            format!(" — `{trimmed}`")
                        }
                        None => String::new(),
                    };
                    let display_part = match &f.display {
                        Some(d) if !d.is_empty() => format!(" (`{d}`)"),
                        _ => " (`auto`)".to_string(),
                    };
                    let family = if f.family.is_empty() {
                        tr("(unnamed)", "（未命名）").to_string()
                    } else {
                        f.family.clone()
                    };
                    let _ = writeln!(s, "  - **{family}**{display_part}{url_part}",);
                }
            }
            // CORS blind-spot honesty signal — only render when
            // non-zero so clean audits stay quiet.
            if fa.unreadable_stylesheets > 0 {
                let _ = writeln!(
                    s,
                    "- ⚠️ **{}** {} {}",
                    fa.unreadable_stylesheets,
                    tr(
                        if fa.unreadable_stylesheets == 1 {
                            "stylesheet"
                        } else {
                            "stylesheets"
                        },
                        "个样式表",
                    ),
                    tr(
                        "unreadable (cross-origin without `crossorigin`) — audit may be incomplete",
                        "无法读取（跨域且未加 `crossorigin`） — 审计可能不完整",
                    ),
                );
            }
            let _ = writeln!(s);
        }

        if let Some(md) = &self.metadata {
            let _ = writeln!(s, "{}", tr("## Page Metadata", "## 页面元数据"));
            let _ = writeln!(s);
            let _ = writeln!(s, "- {}: **{}**", tr("Title", "标题"), md.title);
            if let Some(d) = &md.description {
                let _ = writeln!(s, "- {}: {d}", tr("Description", "描述"));
            }
            if let Some(c) = &md.canonical {
                let _ = writeln!(s, "- {}: `{c}`", tr("Canonical", "Canonical URL"));
            }
            if let Some(r) = &md.robots {
                let _ = writeln!(s, "- {}: `{r}`", tr("Robots", "Robots 指令"));
            }
            if let Some(l) = &md.lang {
                let _ = writeln!(s, "- {}: `{l}`", tr("Lang", "语言"));
            }
            if let Some(v) = &md.viewport {
                let _ = writeln!(s, "- Viewport: `{v}`");
            }
            if let Some(ch) = &md.charset {
                let _ = writeln!(s, "- {}: `{ch}`", tr("Charset", "字符集"));
            }
            if let Some(tc) = &md.theme_color {
                let _ = writeln!(s, "- {}: `{tc}`", tr("Theme color", "主题色"));
            }
            if !md.og.is_empty() {
                let _ = writeln!(
                    s,
                    "- Open Graph ({} {}):",
                    md.og.len(),
                    tr("tags", "个标签"),
                );
                let mut og: Vec<(&String, &String)> = md.og.iter().collect();
                og.sort_by_key(|x| x.0.clone());
                for (k, v) in og.iter().take(8) {
                    let _ = writeln!(s, "  - `og:{k}` = {v}");
                }
                if md.og.len() > 8 {
                    let _ = writeln!(
                        s,
                        "  - {} {} {}.",
                        tr("…and", "……还有"),
                        md.og.len() - 8,
                        tr("more", "项"),
                    );
                }
            }
            if !md.twitter.is_empty() {
                let _ = writeln!(
                    s,
                    "- Twitter ({} {}):",
                    md.twitter.len(),
                    tr("tags", "个标签"),
                );
                let mut tw: Vec<(&String, &String)> = md.twitter.iter().collect();
                tw.sort_by_key(|x| x.0.clone());
                for (k, v) in tw.iter().take(8) {
                    let _ = writeln!(s, "  - `twitter:{k}` = {v}");
                }
                if md.twitter.len() > 8 {
                    let _ = writeln!(
                        s,
                        "  - {} {} {}.",
                        tr("…and", "……还有"),
                        md.twitter.len() - 8,
                        tr("more", "项"),
                    );
                }
            }
            let _ = writeln!(s);
        }

        if let Some(m) = &self.metrics {
            let _ = writeln!(s, "{}", tr("## Page Metrics", "## 页面性能指标"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- JS heap **{} / {}** · {} **{}** · {} **{}** · {} **{}** · {} **{}**",
                format_bytes(m.js_heap_used),
                format_bytes(m.js_heap_total),
                tr("nodes", "节点"),
                m.nodes,
                tr("frames", "frame"),
                m.frames,
                tr("documents", "document"),
                m.documents,
                tr("event listeners", "事件监听器"),
                m.js_event_listeners,
            );
            let _ = writeln!(
                s,
                "- CPU: {} **{:.1}ms** · {} **{:.1}ms** · {} **{:.1}ms** · {} **{:.1}ms**",
                tr("script", "脚本"),
                m.script_duration_ms,
                tr("layout", "布局"),
                m.layout_duration_ms,
                tr("style", "样式"),
                m.recalc_style_duration_ms,
                tr("total task", "总任务"),
                m.task_duration_ms,
            );
            let _ = writeln!(s);
        }

        if let Some(dm) = &self.dom_mutations {
            let total = dm.total_added_nodes + dm.total_removed_nodes + dm.total_attribute_changes;
            let rate = if dm.observation_window_ms > 0 {
                (total as f64) * 1000.0 / (dm.observation_window_ms as f64)
            } else {
                0.0
            };
            let _ = writeln!(s, "{}", tr("## DOM Mutations", "## DOM 变更"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "- {} **{total}** {} {}ms (~**{:.0}/sec**) — {} **{}** · {} **{}** · {} **{}**",
                tr("Total", "共"),
                tr("over", "记录于"),
                dm.observation_window_ms,
                rate,
                tr("added", "新增"),
                dm.total_added_nodes,
                tr("removed", "移除"),
                dm.total_removed_nodes,
                tr("attribute", "属性变更"),
                dm.total_attribute_changes,
            );
            if !dm.top_tags_by_mutation_count.is_empty() {
                let line = dm
                    .top_tags_by_mutation_count
                    .iter()
                    .take(5)
                    .map(|c| format!("`<{}>` {}", c.name, c.count))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(s, "- {}: {line}", tr("Top tags", "热点标签"));
            }
            if !dm.top_attributes_changed.is_empty() {
                let line = dm
                    .top_attributes_changed
                    .iter()
                    .take(5)
                    .map(|c| format!("`{}` {}", c.name, c.count))
                    .collect::<Vec<_>>()
                    .join(" · ");
                let _ = writeln!(s, "- {}: {line}", tr("Top attributes", "热点属性"));
            }
            let _ = writeln!(s);
        }

        // ─── Details / raw enumerations ─────────────────────────────────

        if !self.resources.is_empty() {
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Resources", "## 资源清单"),
                self.resources.len(),
            );
            let _ = writeln!(s);
            for r in &self.resources {
                let _ = writeln!(s, "- {}", describe_resource(r, lang));
            }
            let _ = writeln!(s);
        }

        if !self.cookies.is_empty() {
            // Name + domain only — values may contain session tokens. Use the
            // JSON response if you need the actual values.
            let _ = writeln!(
                s,
                "{} ({})",
                tr("## Cookies", "## Cookie"),
                self.cookies.len(),
            );
            let _ = writeln!(s);
            for c in &self.cookies {
                let _ = writeln!(s, "- `{}` on `{}`", c.name, c.domain);
            }
            let _ = writeln!(s);
        }

        // ─── Binary attachments ─────────────────────────────────────────

        if self.screenshot.is_some() {
            let _ = writeln!(s, "{}", tr("## Screenshot", "## 截图"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "Base64 PNG captured (omitted from markdown body).",
                    "已采集 Base64 PNG（不在 markdown 正文里输出）。",
                ),
            );
            let _ = writeln!(s);
        }

        if self.pdf.is_some() {
            let _ = writeln!(s, "{}", tr("## PDF", "## PDF"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{}",
                tr(
                    "Base64 PDF captured (omitted from markdown body).",
                    "已采集 Base64 PDF（不在 markdown 正文里输出）。",
                ),
            );
            let _ = writeln!(s);
        }

        if let Some(har) = &self.har {
            let entries = har
                .get("log")
                .and_then(|l| l.get("entries"))
                .and_then(|e| e.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let _ = writeln!(s, "{}", tr("## HAR", "## HAR"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{} ({entries} {}).",
                tr("HAR 1.2 archive included", "已包含 HAR 1.2 归档",),
                tr(
                    "entries; omitted from markdown body",
                    "条记录；不在 markdown 正文里输出",
                ),
            );
            let _ = writeln!(s);
        }

        if let Some(snap) = &self.dom_snapshot {
            let docs = snap
                .get("documents")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let strings = snap
                .get("strings")
                .and_then(|s| s.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let _ = writeln!(s, "{}", tr("## DOM Snapshot", "## DOM 快照"));
            let _ = writeln!(s);
            let _ = writeln!(
                s,
                "{} ({docs} {}, {strings} {}).",
                tr("DOMSnapshot included", "已包含 DOMSnapshot"),
                tr(
                    if docs == 1 { "document" } else { "documents" },
                    "个 document",
                ),
                tr(
                    "interned strings; omitted from markdown body",
                    "条 interned 字符串；不在 markdown 正文里输出",
                ),
            );
            let _ = writeln!(s);
        }

        let _ = writeln!(s, "{}", tr("## Page Content", "## 页面内容"));
        let _ = writeln!(s);
        let _ = writeln!(s, "```");
        s.push_str(&self.data);
        if !self.data.ends_with('\n') {
            s.push('\n');
        }
        let _ = writeln!(s, "```");

        s
    }
}

fn describe_resource(r: &WebPageResource, lang: Lang) -> String {
    let tr = |en: &'static str, zh: &'static str| -> &'static str {
        match lang {
            Lang::En => en,
            Lang::Zh => zh,
        }
    };
    let mime = if r.mime_type.is_empty() {
        tr("unknown type", "未知类型")
    } else {
        r.mime_type.as_str()
    };

    if r.from_cache {
        return format!(
            "{} `{}` {} ({}, {} {}).",
            tr("Served", "从浏览器缓存提供"),
            r.url,
            tr("from browser cache", ""),
            mime,
            tr("status", "状态码"),
            r.status,
        );
    }

    let ttfb = r
        .timing
        .as_ref()
        .map(|t| t.receive_headers_end)
        .filter(|&t| t >= 0.0)
        .map(|t| format!(", TTFB {}ms", t as u32))
        .unwrap_or_default();

    let size = format_bytes(r.content_size);
    let conn = if r.connection_reused {
        tr(", connection reused", "，连接复用")
    } else {
        ""
    };

    match r.status {
        200..=299 => format!(
            "{} `{}` {} {} ({}, {} {}{}{}).",
            tr("Loaded", "加载"),
            r.url,
            tr("as", "为"),
            mime,
            size,
            tr("status", "状态码"),
            r.status,
            ttfb,
            conn,
        ),
        300..=399 => format!(
            "{} ({} {}) {} `{}`{}.",
            tr("Redirected", "重定向"),
            tr("status", "状态码"),
            r.status,
            tr("from", "自"),
            r.url,
            ttfb,
        ),
        400..=499 => format!(
            "{} `{}` ({} {}, {}).",
            tr("Client error fetching", "请求客户端错误"),
            r.url,
            tr("status", "状态码"),
            r.status,
            mime,
        ),
        500..=599 => format!(
            "{} `{}` ({} {}, {}).",
            tr("Server error fetching", "请求服务端错误"),
            r.url,
            tr("status", "状态码"),
            r.status,
            mime,
        ),
        _ => format!(
            "{} `{}` {} {} {} ({}, {}).",
            tr("Fetched", "请求"),
            r.url,
            tr("with status", "状态码"),
            r.status,
            "",
            mime,
            size,
        ),
    }
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else if n < GB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else {
        format!("{:.2} GB", n as f64 / GB as f64)
    }
}
