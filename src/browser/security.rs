//! Security header parsing, TLS helpers, and CORS audit derives.
use super::*;

/// Project CDP `SecurityDetails` into our compact `TlsInfo`. `days_remaining`
/// is computed at capture time from wall clock; negative if expired.
/// `host` identifies which origin the certificate was observed on.
/// `remote_ip` / `remote_port` come from the same Network.responseReceived —
/// the IP the browser actually connected to (already resolved, no extra DNS
/// lookup needed on our side; safe from SSRF surface since it's observation).
pub(crate) fn extract_tls_info(
    sd: &CdpSecurityDetails,
    host: String,
    remote_ip: Option<String>,
    remote_port: Option<u16>,
) -> TlsInfo {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as f64)
        .unwrap_or(0.0);
    // CDP's valid_to is `TimeSinceEpoch` (seconds since unix epoch as f64).
    let valid_to = *sd.valid_to.inner();
    let valid_from = *sd.valid_from.inner();
    let days_remaining = ((valid_to - now_secs) / 86400.0).floor() as i64;
    TlsInfo {
        host,
        remote_ip,
        remote_port,
        protocol: sd.protocol.clone(),
        cipher: sd.cipher.clone(),
        key_exchange: if sd.key_exchange.is_empty() {
            None
        } else {
            Some(sd.key_exchange.clone())
        },
        subject_name: sd.subject_name.clone(),
        issuer: sd.issuer.clone(),
        valid_from,
        valid_to,
        days_remaining,
        san_list: sd.san_list.clone(),
    }
}

/// Format the resolved remote IP suffix for a TLS section header.
/// Returns `" → 198.51.100.42"` (port hidden if standard 443), or empty
/// string when CDP didn't report an IP (cached responses, local schemes).
pub(crate) fn format_remote_ip(tls: &TlsInfo) -> String {
    match (&tls.remote_ip, tls.remote_port) {
        (Some(ip), Some(443)) | (Some(ip), None) => format!(" → `{ip}`"),
        (Some(ip), Some(p)) => format!(" → `{ip}:{p}`"),
        (None, _) => String::new(),
    }
}

/// Format certificate expiry as human-readable string with severity markers.
/// Negative days = already expired. <30 days = warning. Used by markdown
/// rendering for both the main-document section and the per-host table.
pub(crate) fn format_tls_expiry(days_remaining: i64) -> String {
    if days_remaining < 0 {
        format!("**EXPIRED {} days ago**", -days_remaining)
    } else if days_remaining < 30 {
        format!("**expires in {days_remaining} days ⚠️**")
    } else {
        format!("expires in {days_remaining} days")
    }
}

/// Extract the security-relevant headers from a response's header map.
/// Returns None when no security headers are present.
/// Build the `SecurityAudit` scorecard from already-captured data.
/// `headers` is the curated main-document header map (`None` when no
/// Document response was ever observed — same shape as
/// `WebPageStat.security_headers`). `cookies` is the page's full jar.
///
/// Pure derive — runs in O(headers + cookies), no IO.
/// Parse an enforcing `Content-Security-Policy` value into a weakness
/// report. `value` is the raw header string (e.g.
/// `"default-src 'self'; script-src 'self' 'unsafe-inline' *"`).
/// Returns `None` when the value parses to zero directives (blank /
/// malformed) — the caller treats that the same as "no CSP".
pub(crate) fn parse_csp(value: &str) -> Option<CspAnalysis> {
    // directive name (lowercased) -> source tokens (case preserved;
    // keywords like 'unsafe-inline' are case-insensitive per spec).
    let mut directives: HashMap<String, Vec<String>> = HashMap::new();
    for raw in value.split(';') {
        let mut parts = raw.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let sources: Vec<String> = parts.map(|s| s.to_string()).collect();
        // A directive can legally appear once; last-wins matches browser
        // behaviour but for a weakness scan a merge is safer (any unsafe
        // source anywhere is a finding).
        directives
            .entry(name.to_ascii_lowercase())
            .or_default()
            .extend(sources);
    }
    if directives.is_empty() {
        return None;
    }

    let has_kw = |kw: &str| -> bool {
        directives
            .values()
            .flatten()
            .any(|src| src.eq_ignore_ascii_case(kw))
    };
    let mut a = CspAnalysis {
        directive_count: directives.len() as u32,
        unsafe_inline: has_kw("'unsafe-inline'"),
        unsafe_eval: has_kw("'unsafe-eval'"),
        ..Default::default()
    };
    // Wildcard sources: a bare `*` in a directive's source list. Sorted
    // for stable output.
    let mut wildcard: Vec<String> = directives
        .iter()
        .filter(|(_, srcs)| srcs.iter().any(|s| s == "*"))
        .map(|(name, _)| name.clone())
        .collect();
    wildcard.sort();
    a.wildcard_directives = wildcard;
    // object-src falls back to default-src; base-uri / frame-ancestors
    // do NOT fall back (frame-specific / standalone directives).
    a.missing_object_src =
        !directives.contains_key("object-src") && !directives.contains_key("default-src");
    a.missing_base_uri = !directives.contains_key("base-uri");
    a.missing_frame_ancestors = !directives.contains_key("frame-ancestors");

    let mut w = Vec::new();
    if a.unsafe_inline {
        w.push("unsafe-inline".to_string());
    }
    if a.unsafe_eval {
        w.push("unsafe-eval".to_string());
    }
    if !a.wildcard_directives.is_empty() {
        w.push("wildcard-source".to_string());
    }
    if a.missing_object_src {
        w.push("missing-object-src".to_string());
    }
    if a.missing_base_uri {
        w.push("missing-base-uri".to_string());
    }
    if a.missing_frame_ancestors {
        w.push("missing-frame-ancestors".to_string());
    }
    a.weaknesses = w;
    Some(a)
}

/// Parse a `Strict-Transport-Security` value (e.g.
/// `"max-age=31536000; includeSubDomains; preload"`).
pub(crate) fn parse_hsts(value: &str) -> HstsAnalysis {
    let mut a = HstsAnalysis::default();
    for raw in value.split(';') {
        let tok = raw.trim();
        if let Some(rest) = tok
            .strip_prefix("max-age")
            .or_else(|| tok.strip_prefix("Max-Age"))
            .and_then(|r| r.trim_start().strip_prefix('='))
        {
            // Value may be quoted (`max-age="31536000"`) per the grammar.
            let digits = rest.trim().trim_matches('"');
            a.max_age = digits.parse::<u64>().ok();
        } else if tok.eq_ignore_ascii_case("includeSubDomains") {
            a.include_subdomains = true;
        } else if tok.eq_ignore_ascii_case("preload") {
            a.preload = true;
        }
    }
    a.effective = a.max_age.is_some_and(|m| m > 0);
    a
}

pub(crate) fn build_security_audit(
    headers: Option<&HashMap<String, String>>,
    cookies: &[Cookie],
) -> SecurityAudit {
    let mut h = SecurityHeadersCheck::default();
    let has = |name: &str| -> bool { headers.is_some_and(|m| m.contains_key(name)) };
    let get =
        |name: &str| -> Option<&str> { headers.and_then(|m| m.get(name)).map(|s| s.as_str()) };
    h.hsts = has("Strict-Transport-Security");
    h.csp = has("Content-Security-Policy");
    h.csp_report_only = has("Content-Security-Policy-Report-Only");
    h.x_frame_options = has("X-Frame-Options");
    h.x_content_type_options = has("X-Content-Type-Options");
    h.referrer_policy = has("Referrer-Policy");
    h.permissions_policy = has("Permissions-Policy");
    h.coop = has("Cross-Origin-Opener-Policy");
    h.coep = has("Cross-Origin-Embedder-Policy");

    // Deep-parse the two headers whose *value* carries the real signal.
    // Presence bools above stay as the headline; these add the "is it
    // actually any good" layer.
    h.csp_analysis = get("Content-Security-Policy").and_then(parse_csp);
    h.hsts_analysis = get("Strict-Transport-Security").map(parse_hsts);

    let mut missing = Vec::new();
    for &name in CORE_SECURITY_HEADERS {
        if !has(name) {
            missing.push(name.to_string());
        }
    }
    h.present_count = (CORE_SECURITY_HEADERS.len() - missing.len()) as u32;
    h.missing = missing;

    let mut c = CookieSecurityCheck {
        total: cookies.len() as u32,
        ..Default::default()
    };
    let mut header_bytes: u64 = 0;
    for cookie in cookies {
        if cookie.secure {
            c.secure += 1;
        }
        if cookie.http_only {
            c.http_only += 1;
        }
        if let Some(ss) = cookie.same_site.as_deref() {
            c.same_site_set += 1;
            // SameSite=None without Secure is rejected by modern browsers
            // outright. Case-insensitive match — CDP returns "None" but
            // origin headers may differ.
            if ss.eq_ignore_ascii_case("None") && !cookie.secure {
                c.same_site_none_without_secure += 1;
            }
        }
        // Estimate the on-the-wire `Cookie:` header contribution:
        // `name=value` for the cookie, plus `"; "` separator between
        // cookies. Subtract the trailing separator at the end.
        header_bytes += cookie.name.len() as u64;
        header_bytes += 1; // '='
        header_bytes += cookie.value.len() as u64;
        header_bytes += 2; // "; "
    }
    if header_bytes >= 2 {
        header_bytes -= 2; // drop the trailing "; " after the last cookie
    }
    c.header_bytes = header_bytes;

    SecurityAudit {
        headers: h,
        cookies: c,
    }
}

pub(crate) fn extract_security_headers(headers: &Headers) -> Option<HashMap<String, String>> {
    let obj = headers.inner().as_object()?;
    let mut out = HashMap::new();
    for &name in SECURITY_HEADER_NAMES {
        for (k, v) in obj {
            if k.eq_ignore_ascii_case(name) {
                if let Some(s) = v.as_str() {
                    out.insert(name.to_string(), s.to_string());
                }
                break;
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Derive passive CORS misconfiguration findings from observed responses.
/// Flags the one unambiguous server bug: `Access-Control-Allow-Origin`
/// of `*` or `null` together with `Access-Control-Allow-Credentials:
/// true` (browsers reject the combo, but the server is misconfigured and
/// it's reportable). Deduplicated by URL, capped at 20.
pub(crate) fn build_cors_issues(resources: &[WebPageResource]) -> Vec<CorsIssue> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in resources {
        if !r.cors_allow_credentials {
            continue;
        }
        let Some(acao) = r.cors_allow_origin.as_deref() else {
            continue;
        };
        let acao_trim = acao.trim();
        if acao_trim != "*" && !acao_trim.eq_ignore_ascii_case("null") {
            continue;
        }
        if !seen.insert(r.url.as_str()) {
            continue;
        }
        out.push(CorsIssue {
            url: r.url.clone(),
            allow_origin: acao_trim.to_string(),
            allow_credentials: true,
            reason: "wildcard-with-credentials".to_string(),
        });
        if out.len() >= 20 {
            break;
        }
    }
    out
}
