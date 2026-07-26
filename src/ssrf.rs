//! SSRF host-blocklist checks, shared by two layers:
//!
//! * [`crate::capture`]'s pre-flight `check_ssrf` — rejects a bad **initial**
//!   URL before a pool slot is even checked out, and
//! * [`crate::browser`]'s in-page Fetch interception — re-checks each
//!   navigation / redirect hop so a public URL can't 3xx-bounce the headless
//!   browser into an internal host (the redirect bypass the pre-flight check
//!   alone can't catch).
//!
//! Pure IP classification (`is_blocked_*`) plus a DNS-resolving URL check. No
//! axum / transport types live here, so `browser` (which never touches axum)
//! can depend on it just like `capture` does.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Why an SSRF check failed. Kept transport-neutral; the HTTP layer maps the
/// variants to status codes (`NoHost` / `DnsFailed` → 400, `Blocked` → 403),
/// while the interception path only cares that it's an `Err`.
pub(crate) enum SsrfError {
    NoHost,
    DnsFailed(String),
    Blocked(String),
}

impl std::fmt::Display for SsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SsrfError::NoHost => write!(f, "URL has no host"),
            SsrfError::DnsFailed(m) | SsrfError::Blocked(m) => write!(f, "{m}"),
        }
    }
}

/// Check a parsed URL's host against the blocklist. Literal-IP hosts are checked
/// directly; domains are resolved via DNS and rejected if **any** resolved
/// address is blocked. `Ok(())` means allowed.
pub(crate) async fn check_url(url: &Url) -> Result<(), SsrfError> {
    let host = url.host().ok_or(SsrfError::NoHost)?;
    let port = url.port_or_known_default().unwrap_or(80);

    match host {
        Host::Ipv4(ip) => {
            if is_blocked_ipv4(&ip) {
                return Err(SsrfError::Blocked(format!("blocked IPv4 host: {ip}")));
            }
        }
        Host::Ipv6(ip) => {
            if is_blocked_ipv6(&ip) {
                return Err(SsrfError::Blocked(format!("blocked IPv6 host: {ip}")));
            }
        }
        Host::Domain(name) => {
            let addrs = tokio::net::lookup_host(format!("{name}:{port}"))
                .await
                .map_err(|e| {
                    SsrfError::DnsFailed(format!("dns resolution failed for `{name}`: {e}"))
                })?;
            for addr in addrs {
                if is_blocked_ip(&addr.ip()) {
                    return Err(SsrfError::Blocked(format!(
                        "`{name}` resolves to blocked IP {}",
                        addr.ip()
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Interception-path helper: parse a raw request URL and report whether it
/// should be blocked, returning `Some(reason)` when blocked. Only `http(s)` is
/// checked — `data:` / `blob:` / `about:` and other non-network schemes are
/// never egress SSRF vectors, so they're allowed through (blocking them would
/// break legitimate in-browser navigations like `about:blank`). An unparseable
/// URL is also allowed (it's not an `http` target we can resolve).
pub(crate) async fn raw_url_blocked(raw: &str) -> Option<String> {
    let Ok(url) = Url::parse(raw) else {
        return None;
    };
    match url.scheme() {
        "http" | "https" => check_url(&url).await.err().map(|e| e.to_string()),
        _ => None,
    }
}

fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

pub(crate) fn is_blocked_ipv4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback()           // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254/16 (incl. cloud metadata 169.254.169.254)
        || ip.is_broadcast()    // 255.255.255.255
        || ip.is_unspecified()  // 0.0.0.0
        || ip.octets()[0] == 0 // 0.0.0.0/8 reserved
        // 100.64.0.0/10 shared address space (CGNAT, RFC 6598) — used inside
        // carrier / cloud networks (e.g. Tailscale, some VPC fabrics), so it's
        // internal-facing the same way RFC 1918 is. (`Ipv4Addr::is_shared` is
        // still unstable, hence the manual mask.)
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
        // 192.0.2/24, 198.51.100/24, 203.0.113/24 (TEST-NET-1/2/3) — never
        // routable on the public internet; a URL resolving here is at best
        // misconfigured and at worst probing. NOTE: the 198.18.0.0/15
        // benchmarking range is deliberately NOT blocked — it's routable on
        // some lab/interface setups and our own e2e redirect tests use it as
        // a public-looking local address.
        || ip.is_documentation()
}

pub(crate) fn is_blocked_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    let segments = ip.segments();
    // fe80::/10 link-local
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // fc00::/7 ULA (unique local addresses)
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // 2001:db8::/32 documentation range — the v6 analogue of TEST-NET; never
    // publicly routable. (`Ipv6Addr::is_documentation` is still unstable.)
    if segments[0] == 0x2001 && segments[1] == 0xdb8 {
        return true;
    }
    // IPv4-mapped (::ffff:x.x.x.x) — check embedded v4
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(&v4);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_blocklist() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            // CGNAT 100.64.0.0/10 boundaries
            "100.64.0.0",
            "100.100.100.100",
            "100.127.255.255",
            // TEST-NET-1/2/3 documentation ranges
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
        ] {
            assert!(
                is_blocked_ipv4(&ip.parse().unwrap()),
                "{ip} should be blocked"
            );
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "198.18.0.1",
            // one address either side of the CGNAT /10
            "100.63.255.255",
            "100.128.0.0",
        ] {
            assert!(
                !is_blocked_ipv4(&ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
    }

    #[test]
    fn ipv6_blocklist() {
        for ip in [
            "::1",
            "fe80::1",
            "fc00::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            // documentation 2001:db8::/32 + a v4-mapped CGNAT address
            "2001:db8::1",
            "::ffff:100.64.0.1",
        ] {
            assert!(
                is_blocked_ipv6(&ip.parse().unwrap()),
                "{ip} should be blocked"
            );
        }
        for ip in ["2606:4700:4700::1111", "2001:4860:4860::8888"] {
            assert!(
                !is_blocked_ipv6(&ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
    }

    // Literal-IP and non-http URLs take no DNS path, so these are deterministic.
    #[tokio::test]
    async fn raw_url_blocked_decisions() {
        // Internal literal IPs → blocked (this is the redirect-bypass case).
        for url in [
            "http://127.0.0.1:9/",
            "http://169.254.169.254/latest/meta-data/",
            "https://10.0.0.5/admin",
            "http://[::1]:8080/",
        ] {
            assert!(
                raw_url_blocked(url).await.is_some(),
                "{url} should be blocked"
            );
        }
        // Public literal IPs → allowed.
        for url in ["https://8.8.8.8/", "http://198.18.0.1:18080/"] {
            assert!(
                raw_url_blocked(url).await.is_none(),
                "{url} should be allowed"
            );
        }
        // Non-http(s) schemes and unparseable URLs → never an egress vector.
        for url in ["data:text/html,hi", "about:blank", "blob:xyz", "not a url"] {
            assert!(
                raw_url_blocked(url).await.is_none(),
                "{url} should pass through"
            );
        }
    }
}
