//! URL validation and the local-address rule (PLAN.md 3.7).

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use url::{Host, Url};

/// Why a URL string is not a usable endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// Not a URL at all.
    Parse(String),
    /// Scheme other than `http` or `https`.
    Scheme(String),
    /// No host (or an empty one).
    NoHost,
    /// `user:password@` in the URL: credentials go in headers, never in URLs (they would
    /// end up in settings files and logs).
    Credentials,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrlError::Parse(e) => write!(f, "not a valid URL: {e}"),
            UrlError::Scheme(s) => write!(f, "scheme `{s}` is not allowed (use http or https)"),
            UrlError::NoHost => write!(f, "the URL has no host"),
            UrlError::Credentials => write!(f, "the URL must not contain a user name or password"),
        }
    }
}

impl std::error::Error for UrlError {}

/// Validates an endpoint URL as typed by the user (for example the LLM endpoint when it is
/// saved in settings): `http` or `https` only, a non-empty host, no embedded credentials.
pub fn validate_url(s: &str) -> Result<Url, UrlError> {
    let url = Url::parse(s.trim()).map_err(|e| UrlError::Parse(e.to_string()))?;
    check_url_shape(&url)?;
    Ok(url)
}

/// The shape rules of [`validate_url`] on an already parsed URL.
pub fn check_url_shape(url: &Url) -> Result<(), UrlError> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(UrlError::Scheme(other.to_string())),
    }
    match url.host() {
        None => return Err(UrlError::NoHost),
        Some(Host::Domain("")) => return Err(UrlError::NoHost),
        Some(_) => {}
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlError::Credentials);
    }
    Ok(())
}

/// Canonical host string used for allowlist matching and the audit log: lower-case domain,
/// dotted IPv4, or IPv6 without brackets.
pub fn host_key(host: &Host<&str>) -> String {
    match host {
        Host::Domain(d) => d.to_ascii_lowercase(),
        Host::Ipv4(a) => a.to_string(),
        Host::Ipv6(a) => a.to_string(),
    }
}

/// Whether `host` is this machine or a private LAN address: `localhost`, IPv4 loopback
/// (127/8) and RFC 1918 ranges (10/8, 172.16/12, 192.168/16), IPv6 loopback and unique
/// local addresses (fc00::/7), including IPv4-mapped forms. Only these may use plain
/// `http` (a local LLM server); everything else needs `https`. Host names other than
/// `localhost` are never treated as local, because what they resolve to is not known here.
pub fn is_local_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(a) => is_local_ip(IpAddr::V4(*a)),
        Host::Ipv6(a) => is_local_ip(IpAddr::V6(*a)),
    }
}

/// [`is_local_host`] for a whole URL (false when it has no host). The UI uses this to warn
/// when the LLM endpoint is not local (PLAN.md 3.7).
pub fn is_local_url(url: &Url) -> bool {
    url.host().is_some_and(|h| is_local_host(&h))
}

fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback() || a.is_private(),
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return is_local_ip(IpAddr::V4(v4));
            }
            a.is_loopback() || is_unique_local(a)
        }
    }
}

fn is_unique_local(a: Ipv6Addr) -> bool {
    (a.segments()[0] & 0xfe00) == 0xfc00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_http_and_https_with_a_host() {
        for s in [
            "https://api.example.com/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:8080/v1",
            "  https://Example.COM  ",
        ] {
            assert!(validate_url(s).is_ok(), "{s}");
        }
    }

    #[test]
    fn validate_rejects_other_schemes_missing_hosts_and_credentials() {
        assert!(matches!(validate_url("not a url"), Err(UrlError::Parse(_))));
        assert!(matches!(validate_url(""), Err(UrlError::Parse(_))));
        assert_eq!(
            validate_url("ftp://example.com"),
            Err(UrlError::Scheme("ftp".into()))
        );
        assert_eq!(
            validate_url("file:///C:/x"),
            Err(UrlError::Scheme("file".into()))
        );
        assert_eq!(
            validate_url("mailto:a@b.c"),
            Err(UrlError::Scheme("mailto".into()))
        );
        assert!(validate_url("http://").is_err());
        assert_eq!(
            validate_url("https://user:pw@example.com"),
            Err(UrlError::Credentials)
        );
        assert_eq!(
            validate_url("https://user@example.com"),
            Err(UrlError::Credentials)
        );
    }

    #[test]
    fn local_hosts_are_loopback_and_private_lan_only() {
        for s in [
            "http://localhost:1",
            "http://LOCALHOST",
            "http://127.0.0.1",
            "http://127.9.9.9",
            "http://10.1.2.3",
            "http://172.16.0.1",
            "http://172.31.255.255",
            "http://192.168.1.20",
            "http://[::1]",
            "http://[fd12:3456::1]",
            "http://[::ffff:192.168.1.1]",
        ] {
            assert!(is_local_url(&Url::parse(s).unwrap()), "{s}");
        }
        for s in [
            "http://8.8.8.8",
            "http://172.32.0.1",
            "http://172.15.0.1",
            "http://169.254.1.1",
            "http://example.com",
            "http://localhost.example.com",
            "http://mybox.local",
            "http://[2001:db8::1]",
            "http://[::ffff:8.8.8.8]",
        ] {
            assert!(!is_local_url(&Url::parse(s).unwrap()), "{s}");
        }
    }

    #[test]
    fn host_keys_are_canonical() {
        let u = Url::parse("https://API.Example.com").unwrap();
        assert_eq!(host_key(&u.host().unwrap()), "api.example.com");
        let u = Url::parse("http://[0:0:0:0:0:0:0:1]").unwrap();
        assert_eq!(host_key(&u.host().unwrap()), "::1");
    }
}
