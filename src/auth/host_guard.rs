//! Host-header allowlisting to defeat DNS-rebinding attacks (CWE-350).
//!
//! When the server runs without `MCP_AUTH_TOKEN` ("local only" mode), a
//! malicious web page the operator visits can use DNS rebinding to point its
//! hostname at 127.0.0.1 and reach the MCP endpoint from the browser. Binding
//! to loopback does not help: the OS resolves the attacker hostname to
//! loopback while the page keeps its origin. The browser still sends the
//! attacker's hostname in the `Host` header, so validating Host against a
//! localhost allowlist rejects the rebound request. A bearer token (auth mode)
//! already defeats this, since a browser won't attach it; this guard protects
//! the no-token path.

use std::collections::BTreeSet;

use url::Url;

const LOCAL_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

fn bare_host(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(host.trim_start_matches('[').trim_end_matches(']').to_lowercase())
}

/// The normalized, lowercased hostname in a raw `Host` header (port and IPv6
/// brackets stripped), or `None` when the header is missing or malformed.
pub fn parse_hostname(host_header: Option<&str>) -> Option<String> {
    let header = host_header.filter(|h| !h.is_empty())?;
    // A real Host header is a bare host[:port]. Reject anything carrying
    // userinfo or a path (e.g. "attacker.example@127.0.0.1", "127.0.0.1/x") so a
    // crafted value can't normalize into the allowlist.
    if header.contains(['@', '/', '?', '#', '\\']) || header.chars().any(char::is_whitespace) {
        return None;
    }
    let url = Url::parse(&format!("http://{header}")).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.path() != "/" {
        return None;
    }
    bare_host(&url)
}

/// The allowed hostnames: always localhost, plus any from a comma-separated list.
pub fn build_allowed_hosts(extra: Option<&str>) -> BTreeSet<String> {
    let mut hosts: BTreeSet<String> = LOCAL_HOSTS.iter().map(|h| h.to_string()).collect();
    for entry in extra.unwrap_or("").split(',').map(str::trim).filter(|e| !e.is_empty()) {
        hosts.insert(parse_hostname(Some(entry)).unwrap_or_else(|| entry.to_lowercase()));
    }
    hosts
}

/// True if the request's Host header names an allowed host. Missing or malformed Host is rejected.
pub fn is_host_allowed(host_header: Option<&str>, allowed: &BTreeSet<String>) -> bool {
    parse_hostname(host_header).is_some_and(|host| allowed.contains(&host))
}

/// True if the request has no browser `Origin`, or its Origin host is allowlisted.
///
/// Host validation alone is not enough: the server answers with a wildcard
/// `Access-Control-Allow-Origin: *`, so a page at any origin can skip DNS
/// rebinding and directly fetch `http://127.0.0.1:<port>/mcp`; that request
/// carries a genuine loopback Host but an attacker Origin. Non-browser MCP
/// clients (CLI, desktop apps) send no Origin, so requiring "no Origin, or an
/// allowlisted Origin" blocks cross-origin browsers without affecting
/// legitimate local callers.
pub fn is_origin_allowed(origin_header: Option<&str>, allowed: &BTreeSet<String>) -> bool {
    let Some(origin) = origin_header.filter(|o| !o.is_empty()) else {
        return true; // Non-browser client: no Origin header.
    };
    // "null" (opaque) or malformed origins are rejected.
    Url::parse(origin).ok().as_ref().and_then(bare_host).is_some_and(|host| allowed.contains(&host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_headers() {
        assert_eq!(parse_hostname(Some("localhost:8787")).as_deref(), Some("localhost"));
        assert_eq!(parse_hostname(Some("LOCALHOST")).as_deref(), Some("localhost"));
        assert_eq!(parse_hostname(Some("[::1]:8787")).as_deref(), Some("::1"));
        assert_eq!(parse_hostname(Some("127.0.0.1")).as_deref(), Some("127.0.0.1"));
        assert_eq!(parse_hostname(Some("attacker.example@127.0.0.1")), None);
        assert_eq!(parse_hostname(Some("127.0.0.1/x")), None);
        assert_eq!(parse_hostname(Some("")), None);
        assert_eq!(parse_hostname(None), None);
        assert_eq!(parse_hostname(Some("host:notaport")), None);
    }

    #[test]
    fn allows_local_hosts_and_extras() {
        let allowed = build_allowed_hosts(Some(" 192.168.1.5 , MyBox.local:8787,"));
        for host in ["localhost:8787", "127.0.0.1", "[::1]:1", "192.168.1.5:8787", "mybox.local"] {
            assert!(is_host_allowed(Some(host), &allowed), "{host}");
        }
        for host in ["evil.example", "localhost.evil.example", "127.0.0.2"] {
            assert!(!is_host_allowed(Some(host), &allowed), "{host}");
        }
        assert!(!is_host_allowed(None, &allowed));
    }

    #[test]
    fn checks_origins() {
        let allowed = build_allowed_hosts(None);
        assert!(is_origin_allowed(None, &allowed));
        assert!(is_origin_allowed(Some("http://localhost:3000"), &allowed));
        assert!(is_origin_allowed(Some("http://[::1]:3000"), &allowed));
        assert!(!is_origin_allowed(Some("https://evil.example"), &allowed));
        assert!(!is_origin_allowed(Some("null"), &allowed));
        assert!(!is_origin_allowed(Some("not a url"), &allowed));
        assert!(!is_origin_allowed(Some("file:///etc/passwd"), &allowed));
    }

    #[test]
    fn parses_bare_and_mixed_case_hostnames() {
        assert_eq!(parse_hostname(Some("127.0.0.1:8787")).as_deref(), Some("127.0.0.1"));
        assert_eq!(parse_hostname(Some("attacker.example")).as_deref(), Some("attacker.example"));
        assert_eq!(parse_hostname(Some("Attacker.Example")).as_deref(), Some("attacker.example"));
    }

    #[test]
    fn rejects_smuggled_userinfo_and_paths() {
        for header in ["attacker.example@127.0.0.1", "127.0.0.1/../x", "attacker.example#@127.0.0.1", "127.0.0.1 x"] {
            assert_eq!(parse_hostname(Some(header)), None, "{header:?}");
        }
    }

    #[test]
    fn default_allowlist_rejects_rebound_and_lan_hosts() {
        let allowed = build_allowed_hosts(None);
        for host in ["localhost:8787", "127.0.0.1:8787", "[::1]:8787"] {
            assert!(is_host_allowed(Some(host), &allowed), "{host} should be allowed");
        }
        for host in ["attacker.example", "attacker.example:8787", "192.168.1.5:8787"] {
            assert!(!is_host_allowed(Some(host), &allowed), "{host} should be rejected");
        }
    }

    #[test]
    fn extra_hosts_do_not_open_up_everything() {
        let allowed = build_allowed_hosts(Some("192.168.1.5, mybox.local:8787"));
        assert!(is_host_allowed(Some("192.168.1.5:8787"), &allowed));
        assert!(is_host_allowed(Some("mybox.local"), &allowed));
        assert!(!is_host_allowed(Some("attacker.example"), &allowed));
    }

    #[test]
    fn origin_checks_ignore_loopback_host_with_foreign_origin() {
        let allowed = build_allowed_hosts(None);
        assert!(is_origin_allowed(Some(""), &allowed));
        assert!(is_origin_allowed(Some("http://localhost:6274"), &allowed));
        assert!(is_origin_allowed(Some("http://127.0.0.1:8787"), &allowed));
        assert!(!is_origin_allowed(Some("http://attacker.example"), &allowed));
        assert!(!is_origin_allowed(Some("https://attacker.example"), &allowed));
    }
}
