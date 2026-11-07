//! Host allowlist (PLAN.md 3.9 invariant 3): model/fixture manifest hosts, the configured
//! LLM endpoint and, later, the sync server. Nothing else may be contacted.

use std::collections::BTreeSet;
use std::fmt;

use url::{Host, Url};

use crate::url_policy::{UrlError, check_url_shape, host_key, is_local_host};

/// Why a request URL was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// Not an `http`/`https` URL with a host, or it embeds credentials.
    BadUrl,
    /// Plain `http` to a host that is not loopback or a private LAN address.
    InsecureScheme,
    /// The exact (host, port) pair is not on the allowlist.
    HostNotAllowed,
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DenyReason::BadUrl => "not an http(s) URL with a host and no credentials",
            DenyReason::InsecureScheme => "plain http is only allowed to loopback or private LAN",
            DenyReason::HostNotAllowed => "host and port are not on the allowlist",
        })
    }
}

/// Set of exact `(host, port)` pairs requests may go to, plus redirect-only entries.
///
/// Exact entries match exactly: no wildcards, no subdomains, and the port is part of the
/// key (`https://h` means `h:443`). Host names compare case-insensitively; `localhost` and
/// `127.0.0.1` are different entries. Exact entries are valid on every hop.
///
/// Redirect entries ([`Allowlist::allow_redirect_host`], the manifests' `redirect_hosts`,
/// for example a model host's CDN) are valid only on a redirect hop (hop 1 and later,
/// [`Allowlist::check_hop`]), never for the first request. They are an exact host or
/// `*.<domain>`, which matches hosts strictly below `<domain>` (never `<domain>` itself),
/// always with an exact port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: BTreeSet<(String, u16)>,
    redirect_entries: BTreeSet<(String, u16)>,
}

/// Whether `host` (a canonical domain) matches a redirect pattern: the exact host, or
/// `*.<domain>` for any host strictly below `<domain>`. Same rule as xtask's
/// `download::host_matches`.
fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(domain) => host
            .strip_suffix(domain)
            .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
        None => pattern == host,
    }
}

impl Allowlist {
    /// An empty allowlist (every request is refused).
    pub fn new() -> Self {
        Self::default()
    }

    /// Allows the host and port (explicit, or the scheme default) of `url`, for example the
    /// configured LLM endpoint or sync server. The URL must pass
    /// [`crate::validate_url`]'s rules.
    pub fn allow_url(&mut self, url: &Url) -> Result<(), UrlError> {
        check_url_shape(url)?;
        let host = url.host().ok_or(UrlError::NoHost)?;
        let port = url.port_or_known_default().ok_or(UrlError::NoHost)?;
        self.entries.insert((host_key(&host), port));
        Ok(())
    }

    /// Allows `host` on port 443 (or the port given as `host:port`), as listed in the
    /// `hosts` of `models/manifest.toml` and `fixtures/manifest.toml`.
    pub fn allow_https_host(&mut self, host: &str) -> Result<(), UrlError> {
        let url =
            Url::parse(&format!("https://{host}")).map_err(|e| UrlError::Parse(e.to_string()))?;
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(UrlError::Parse(format!("`{host}` is not a bare host")));
        }
        self.allow_url(&url)
    }

    /// Allows redirect hops (never a first request) to `pattern` on port 443, or the port
    /// given as `pattern:port`: an exact host, or `*.<domain>` for hosts strictly below
    /// `<domain>`, as listed in the `redirect_hosts` of `models/manifest.toml` and
    /// `fixtures/manifest.toml`. A wildcard needs a domain name, not an IP address.
    pub fn allow_redirect_host(&mut self, pattern: &str) -> Result<(), UrlError> {
        let (wildcard, rest) = match pattern.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        let url =
            Url::parse(&format!("https://{rest}")).map_err(|e| UrlError::Parse(e.to_string()))?;
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(UrlError::Parse(format!("`{pattern}` is not a bare host")));
        }
        check_url_shape(&url)?;
        let host = url.host().ok_or(UrlError::NoHost)?;
        let port = url.port_or_known_default().ok_or(UrlError::NoHost)?;
        if matches!(host, Host::Domain(d) if d.contains('*')) {
            // The URL parser accepts `*` in a domain; only one leading `*.` is a pattern.
            return Err(UrlError::Parse(format!(
                "`{pattern}`: only a leading `*.` is a wildcard"
            )));
        }
        let key = match (wildcard, &host) {
            (false, h) => host_key(h),
            (true, Host::Domain(d)) => format!("*.{}", d.to_ascii_lowercase()),
            (true, _) => {
                return Err(UrlError::Parse(format!(
                    "`{pattern}`: a wildcard needs a domain name"
                )));
            }
        };
        self.redirect_entries.insert((key, port));
        Ok(())
    }

    /// Whether the exact `(host, port)` pair is listed (exact entries only).
    pub fn contains(&self, host: &str, port: u16) -> bool {
        self.entries.contains(&(host.to_ascii_lowercase(), port))
    }

    /// The listed exact `(host, port)` pairs, sorted.
    pub fn entries(&self) -> impl Iterator<Item = (&str, u16)> {
        self.entries.iter().map(|(h, p)| (h.as_str(), *p))
    }

    /// The redirect-only `(pattern, port)` entries, sorted (`*.<domain>` or a host).
    pub fn redirect_entries(&self) -> impl Iterator<Item = (&str, u16)> {
        self.redirect_entries.iter().map(|(h, p)| (h.as_str(), *p))
    }

    /// Decides whether a first request to `url` may be sent: [`Allowlist::check_hop`]
    /// with hop 0 (exact entries only).
    pub fn check(&self, url: &Url) -> Result<(), DenyReason> {
        self.check_hop(url, 0)
    }

    /// Decides whether hop `hop` of a request (0 = the first request, then one per
    /// redirect followed) may go to `url`: it must be a valid http(s) URL, plain `http`
    /// only to loopback or private LAN hosts (PLAN.md 3.7), and its host and port must
    /// match an exact entry, or on hop 1 and later an exact or redirect entry.
    pub fn check_hop(&self, url: &Url, hop: usize) -> Result<(), DenyReason> {
        check_url_shape(url).map_err(|_| DenyReason::BadUrl)?;
        let host = url.host().ok_or(DenyReason::BadUrl)?;
        if url.scheme() == "http" && !is_local_host(&host) {
            return Err(DenyReason::InsecureScheme);
        }
        let port = url.port_or_known_default().ok_or(DenyReason::BadUrl)?;
        let key = host_key(&host);
        if self.entries.contains(&(key.clone(), port)) {
            return Ok(());
        }
        let redirect_ok = hop > 0
            && self.redirect_entries.iter().any(|(pattern, p)| {
                *p == port
                    && match &host {
                        Host::Domain(_) => host_matches(pattern, &key),
                        // A wildcard never matches an IP address; an exact IP entry does.
                        _ => !pattern.starts_with("*.") && *pattern == key,
                    }
            });
        if redirect_ok {
            Ok(())
        } else {
            Err(DenyReason::HostNotAllowed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Reads `fixtures/net/host_match_cases.toml` (std only; the file's header gives the
    /// subset it may use) as `(pattern, host, matches)`. Fails on anything else.
    fn host_match_cases() -> Vec<(String, String, bool)> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .map(|d| d.join("fixtures/net/host_match_cases.toml"))
            .find(|p| p.is_file())
            .expect("fixtures/net/host_match_cases.toml above the crate");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut cases = Vec::new();
        let mut open: Option<(Option<String>, Option<String>, Option<bool>)> = None;
        let mut close = |open: &mut Option<(Option<String>, Option<String>, Option<bool>)>| {
            if let Some(case) = open.take() {
                match case {
                    (Some(p), Some(h), Some(m)) => cases.push((p, h, m)),
                    other => panic!("incomplete case in {}: {other:?}", path.display()),
                }
            }
        };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[cases]]" {
                close(&mut open);
                open = Some((None, None, None));
                continue;
            }
            let case = open
                .as_mut()
                .unwrap_or_else(|| panic!("line {}: key outside [[cases]]", n + 1));
            let (key, value) = line
                .split_once(" = ")
                .unwrap_or_else(|| panic!("line {}: not `key = value`: {line}", n + 1));
            let string = || {
                value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .filter(|v| !v.contains(['"', '\\']))
                    .unwrap_or_else(|| panic!("line {}: not a plain string: {value}", n + 1))
                    .to_string()
            };
            let slot_free = match key {
                "pattern" => case.0.replace(string()).is_none(),
                "host" => case.1.replace(string()).is_none(),
                "matches" => {
                    let b = match value {
                        "true" => true,
                        "false" => false,
                        _ => panic!("line {}: not a bool: {value}", n + 1),
                    };
                    case.2.replace(b).is_none()
                }
                _ => panic!("line {}: unknown key {key}", n + 1),
            };
            assert!(slot_free, "line {}: duplicate key {key}", n + 1);
        }
        close(&mut open);
        cases
    }

    #[test]
    fn host_matches_follows_the_shared_case_table() {
        // The same table is run by xtask's
        // `download_host_matches_follows_the_shared_case_table`.
        let cases = host_match_cases();
        assert!(cases.len() >= 10, "{} cases read", cases.len());
        assert!(cases.iter().any(|c| c.2) && cases.iter().any(|c| !c.2));
        for (pattern, host, matches) in &cases {
            assert_eq!(
                host_matches(pattern, host),
                *matches,
                "host_matches({pattern:?}, {host:?})"
            );
        }
    }

    #[test]
    fn empty_allowlist_refuses_everything() {
        let a = Allowlist::new();
        assert_eq!(
            a.check(&u("https://example.com")),
            Err(DenyReason::HostNotAllowed)
        );
        assert_eq!(
            a.check(&u("http://127.0.0.1:8080")),
            Err(DenyReason::HostNotAllowed)
        );
    }

    #[test]
    fn manifest_hosts_allow_https_on_443_only() {
        let mut a = Allowlist::new();
        a.allow_https_host("huggingface.co").unwrap();
        a.allow_https_host("GitHub.com").unwrap();
        assert!(a.check(&u("https://huggingface.co/x/y.bin")).is_ok());
        assert!(a.check(&u("https://HUGGINGFACE.CO:443/")).is_ok());
        assert!(a.check(&u("https://github.com/a")).is_ok());
        assert!(a.contains("github.com", 443));
        assert_eq!(
            a.check(&u("https://huggingface.co:8443/")),
            Err(DenyReason::HostNotAllowed)
        );
        assert_eq!(
            a.check(&u("https://cdn-lfs.huggingface.co/")),
            Err(DenyReason::HostNotAllowed),
            "no implicit subdomains"
        );
        assert_eq!(
            a.check(&u("https://evilhuggingface.co/")),
            Err(DenyReason::HostNotAllowed)
        );
        assert_eq!(
            a.check(&u("http://huggingface.co/")),
            Err(DenyReason::InsecureScheme)
        );
        assert!(a.allow_https_host("example.com/path").is_err());
        assert!(a.allow_https_host("user@example.com").is_err());
    }

    #[test]
    fn llm_url_allows_exactly_its_host_and_port() {
        let mut a = Allowlist::new();
        a.allow_url(&u("http://192.168.1.20:8080/v1")).unwrap();
        assert!(
            a.check(&u("http://192.168.1.20:8080/v1/chat/completions"))
                .is_ok()
        );
        assert_eq!(
            a.check(&u("http://192.168.1.20:8081/")),
            Err(DenyReason::HostNotAllowed)
        );
        assert_eq!(
            a.check(&u("http://192.168.1.21:8080/")),
            Err(DenyReason::HostNotAllowed)
        );
        let entries: Vec<_> = a.entries().collect();
        assert_eq!(entries, [("192.168.1.20", 8080)]);
    }

    #[test]
    fn plain_http_only_reaches_local_hosts() {
        let mut a = Allowlist::new();
        a.allow_url(&u("https://api.example.com")).unwrap();
        a.allow_url(&u("http://localhost:8080")).unwrap();
        a.allow_url(&u("http://[::1]:8080")).unwrap();
        assert!(a.check(&u("http://localhost:8080/v1")).is_ok());
        assert!(a.check(&u("http://[::1]:8080/v1")).is_ok());
        assert_eq!(
            a.check(&u("http://127.0.0.1:8080/")),
            Err(DenyReason::HostNotAllowed)
        );
        assert_eq!(
            a.check(&u("http://api.example.com:443/")),
            Err(DenyReason::InsecureScheme)
        );
        // An allowlisted public http URL still cannot be used over http.
        a.allow_url(&u("http://public.example.com")).unwrap();
        assert_eq!(
            a.check(&u("http://public.example.com/")),
            Err(DenyReason::InsecureScheme)
        );
    }

    #[test]
    fn redirect_hosts_are_valid_only_after_the_first_hop() {
        let mut a = Allowlist::new();
        a.allow_https_host("huggingface.co").unwrap();
        a.allow_redirect_host("*.hf.co").unwrap();
        a.allow_redirect_host("Release-Assets.githubusercontent.com")
            .unwrap();
        for (url, hop) in [
            ("https://huggingface.co/x", 0),
            ("https://huggingface.co/x", 2),
            ("https://us.aws.cdn.hf.co/x?sig=1", 1),
            ("https://US.AWS.Cdn.HF.co/x", 3),
            ("https://release-assets.githubusercontent.com/a", 1),
        ] {
            assert_eq!(a.check_hop(&u(url), hop), Ok(()), "{url} at hop {hop}");
        }
        for (url, hop, why) in [
            // redirect entries are never a first request
            ("https://us.aws.cdn.hf.co/x", 0, DenyReason::HostNotAllowed),
            (
                "https://release-assets.githubusercontent.com/a",
                0,
                DenyReason::HostNotAllowed,
            ),
            // wildcards match strict subdomains only, on the listed port
            ("https://hf.co/x", 1, DenyReason::HostNotAllowed),
            ("https://evilhf.co/x", 1, DenyReason::HostNotAllowed),
            (
                "https://hf.co.evil.example/x",
                1,
                DenyReason::HostNotAllowed,
            ),
            ("https://cdn.hf.co:8443/x", 1, DenyReason::HostNotAllowed),
            (
                "https://x.release-assets.githubusercontent.com/a",
                1,
                DenyReason::HostNotAllowed,
            ),
            (
                "https://objects.githubusercontent.com/x",
                1,
                DenyReason::HostNotAllowed,
            ),
            // the shape rules still hold on redirect hops
            ("http://cdn.hf.co/x", 1, DenyReason::InsecureScheme),
            ("https://u@cdn.hf.co/x", 1, DenyReason::BadUrl),
        ] {
            assert_eq!(a.check_hop(&u(url), hop), Err(why), "{url} at hop {hop}");
        }
        assert_eq!(
            a.check(&u("https://cdn.hf.co/x")),
            Err(DenyReason::HostNotAllowed)
        );
        let exact: Vec<_> = a.entries().collect();
        assert_eq!(exact, [("huggingface.co", 443)]);
        let redirects: Vec<_> = a.redirect_entries().collect();
        assert_eq!(
            redirects,
            [
                ("*.hf.co", 443),
                ("release-assets.githubusercontent.com", 443)
            ]
        );
    }

    #[test]
    fn redirect_hosts_take_ports_and_refuse_bad_patterns() {
        let mut a = Allowlist::new();
        a.allow_redirect_host("127.0.0.1:8080").unwrap();
        a.allow_redirect_host("*.example.test:8443").unwrap();
        assert!(a.check_hop(&u("http://127.0.0.1:8080/x"), 1).is_ok());
        assert!(a.check_hop(&u("http://127.0.0.1:8081/x"), 1).is_err());
        assert!(a.check_hop(&u("https://a.example.test:8443/x"), 1).is_ok());
        assert!(a.check_hop(&u("https://a.example.test/x"), 1).is_err());
        for bad in [
            "",
            "*.",
            "*.127.0.0.1",
            "*.[::1]",
            "example.com/path",
            "user@example.com",
            "*.*.example.com",
            "*example.com",
        ] {
            assert!(a.allow_redirect_host(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn non_http_urls_are_bad() {
        let mut a = Allowlist::new();
        a.allow_https_host("example.com").unwrap();
        assert_eq!(a.check(&u("ftp://example.com/")), Err(DenyReason::BadUrl));
        assert_eq!(
            a.check(&u("https://me:pw@example.com/")),
            Err(DenyReason::BadUrl)
        );
        assert!(a.allow_url(&u("file:///C:/x")).is_err());
    }
}
