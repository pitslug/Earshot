//! Whether the summary endpoint is the worker's own local model (T11.6, docs/sync.md section 10).
//!
//! An endpoint on the worker's own compose network counts as this device's local model, so the
//! worker may write a meeting kept on its owner's computers; any other endpoint is remote and a
//! kept-local request is declined `kept_local_no_model`. The rule is deliberately narrow, and a
//! endpoint it does not accept is simply remote:
//!
//! 1. The address is plain `http` (the stock llama.cpp server speaks nothing else on a compose
//!    network) and its host is a compose service name: one DNS label, letters, digits, `-` and
//!    `_` only, with no dots, and not `localhost`. An IP address, a dotted name or `https` is
//!    remote.
//! 2. The name resolves, and every address it resolves to is private: IPv4 10/8, 172.16/12 or
//!    192.168/16, or IPv6 unique local (fc00::/7). Docker's networks use these. Loopback is not
//!    accepted (the worker's container holds no model, and a loopback port can be forwarded
//!    anywhere), nor link-local, shared (100.64/10, Tailscale's) or public addresses.
//! 3. The request goes to the address that was checked, not the name ([`Place::OwnNetwork`]'s
//!    `url` holds the first resolved address), so a later lookup cannot send the transcript
//!    somewhere the rule did not look. The name is looked up again for every request.
//!
//! What it cannot tell apart: a machine on the owner's LAN that the host's DNS search domain
//! answers for a bare name with a private address would also pass. The compose file names the
//! model service `llama`; an owner who points the worker at another bare name takes on that
//! name's meaning.

use std::net::{IpAddr, ToSocketAddrs};

use earshot_net::Url;

/// Where the summary endpoint is, for the worker's purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The worker's own local model on its compose network. `url` names the private address
    /// the rule checked, in place of the service name.
    OwnNetwork {
        /// The endpoint with its host replaced by the checked address.
        url: Url,
    },
    /// Any other endpoint: an ordinary remote writer.
    Remote {
        /// The endpoint as configured.
        url: Url,
    },
}

impl Place {
    /// Whether this is the worker's own local model.
    #[must_use]
    pub const fn is_own_network(&self) -> bool {
        matches!(self, Self::OwnNetwork { .. })
    }

    /// The address requests go to.
    #[must_use]
    pub const fn url(&self) -> &Url {
        match self {
            Self::OwnNetwork { url } | Self::Remote { url } => url,
        }
    }
}

/// The compose service name `url` names (rule 1), or `None` when it names none.
#[must_use]
pub fn service_name(url: &Url) -> Option<&str> {
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    // An IP address holds a dot, a colon or brackets, which the label rule refuses.
    let name = url.host_str()?;
    let ok = !name.is_empty()
        && name.len() <= 63
        && !name.eq_ignore_ascii_case("localhost")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(name)
}

/// Whether `ip` is an address of a compose network (rule 2).
#[must_use]
pub fn is_own_network_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_private(),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => v4.is_private(),
            None => (a.segments()[0] & 0xfe00) == 0xfc00,
        },
    }
}

/// Where `url` is, given the addresses its host resolved to (`resolved`; ignored when the host
/// is not a service name). A pure function: [`locate`] does the lookup.
#[must_use]
pub fn place_of(url: &Url, resolved: &[IpAddr]) -> Place {
    let remote = || Place::Remote { url: url.clone() };
    if service_name(url).is_none()
        || resolved.is_empty()
        || !resolved.iter().all(|ip| is_own_network_address(*ip))
    {
        return remote();
    }
    let mut checked = url.clone();
    if checked.set_ip_host(resolved[0]).is_err() {
        return remote();
    }
    Place::OwnNetwork { url: checked }
}

/// [`place_of`] with the service name looked up now. A name that does not resolve is remote.
#[must_use]
pub fn locate(url: &Url) -> Place {
    let resolved: Vec<IpAddr> = match (service_name(url), url.port_or_known_default()) {
        (Some(name), Some(port)) => (name, port)
            .to_socket_addrs()
            .map(|addrs| addrs.map(|a| a.ip()).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    place_of(url, &resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn endpoint_service_names_are_single_labels_over_plain_http() {
        assert_eq!(service_name(&u("http://llama:8080/v1")), Some("llama"));
        assert_eq!(
            service_name(&u("http://llama-cpu_1:8080/v1")),
            Some("llama-cpu_1")
        );
        for refused in [
            "https://llama:8080/v1",
            "http://llama.example.com:8080/v1",
            "http://llama.:8080/v1",
            "http://localhost:8080/v1",
            "http://LOCALHOST:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://172.18.0.3:8080/v1",
            "http://[fd00::3]:8080/v1",
            "http://user:pw@llama:8080/v1",
        ] {
            assert_eq!(service_name(&u(refused)), None, "{refused}");
        }
    }

    #[test]
    fn endpoint_own_network_addresses_are_private_only() {
        for own in [
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.9",
            "fd12::3",
            "fc00::1",
        ] {
            assert!(is_own_network_address(ip(own)), "{own}");
        }
        assert!(is_own_network_address(ip("::ffff:172.18.0.2")));
        for not in [
            "127.0.0.1",
            "::1",
            "169.254.1.1",
            "fe80::1",
            "100.64.0.1",
            "172.32.0.1",
            "8.8.8.8",
            "2001:db8::1",
            "0.0.0.0",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_own_network_address(ip(not)), "{not}");
        }
    }

    #[test]
    fn endpoint_a_service_name_on_private_addresses_is_the_own_model_at_the_checked_address() {
        let url = u("http://llama:8080/v1");
        let place = place_of(&url, &[ip("172.18.0.3")]);
        assert_eq!(
            place,
            Place::OwnNetwork {
                url: u("http://172.18.0.3:8080/v1")
            }
        );
        assert!(place.is_own_network());
        let v6 = place_of(&url, &[ip("fd00::3"), ip("172.18.0.3")]);
        assert_eq!(v6.url().as_str(), "http://[fd00::3]:8080/v1");
        // Without a port, the scheme's own.
        assert_eq!(
            place_of(&u("http://llama/v1"), &[ip("10.1.2.3")])
                .url()
                .as_str(),
            "http://10.1.2.3/v1"
        );
    }

    #[test]
    fn endpoint_anything_else_is_remote_as_configured() {
        let url = u("http://llama:8080/v1");
        // No address, or any address that is not private, is remote.
        for resolved in [
            vec![],
            vec![ip("8.8.8.8")],
            vec![ip("172.18.0.3"), ip("8.8.8.8")],
            vec![ip("127.0.0.1")],
            vec![ip("100.100.1.1")],
        ] {
            assert_eq!(
                place_of(&url, &resolved),
                Place::Remote { url: url.clone() },
                "{resolved:?}"
            );
        }
        // A name that is not a service name is remote whatever it resolves to.
        for other in [
            "https://llama:8080/v1",
            "http://llama.lan:8080/v1",
            "http://192.168.1.20:8080/v1",
            "https://api.example.com/v1",
            "http://localhost:8080/v1",
        ] {
            let url = u(other);
            assert_eq!(
                place_of(&url, &[ip("172.18.0.3")]),
                Place::Remote { url: url.clone() },
                "{other}"
            );
        }
    }

    #[test]
    fn endpoint_locate_of_a_name_that_does_not_resolve_is_remote() {
        let url = u("http://earshot-no-such-service-here:8080/v1");
        assert_eq!(locate(&url), Place::Remote { url: url.clone() });
        let ip_url = u("http://192.168.1.20:8080/v1");
        assert_eq!(
            locate(&ip_url),
            Place::Remote {
                url: ip_url.clone()
            }
        );
    }
}
