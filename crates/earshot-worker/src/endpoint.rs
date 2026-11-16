//! Whether the summary endpoint is the worker's own local model (T11.6, docs/sync.md section 10).
//!
//! An endpoint on the worker's own compose network counts as this device's local model, so the
//! worker may write a meeting kept on its owner's computers; any other endpoint is remote and a
//! kept-local request is declined `kept_local_no_model`. The rule is deliberately narrow, and an
//! endpoint it does not accept is simply remote:
//!
//! 1. The address is plain `http` (the stock llama.cpp server speaks nothing else on a compose
//!    network) and its host is exactly the configured model service's name
//!    (`EARSHOT_WORKER_LLM_SERVICE`, default `llama`, the compose file's alias): one DNS label,
//!    letters, digits, `-` and `_` only, no dots, not `localhost`. An IP address, a dotted name,
//!    another bare name or `https` is remote.
//! 2. The name resolves, and every address it resolves to lies inside the model network's fixed
//!    subnet (`EARSHOT_WORKER_MODEL_SUBNET`, the compose file's `172.31.250.0/24`), which must
//!    itself be private (inside 10/8, 172.16/12, 192.168/16 or fc00::/7). Without a subnet
//!    nothing is the worker's own model. This does not rely on DNS: if the model container is
//!    down and the host's resolvers answer the bare name with some LAN machine (a router's DNS,
//!    a search domain), that machine is outside the model network's subnet and so remote.
//! 3. The request goes to the address that was checked, not the name ([`Place::OwnNetwork`]'s
//!    `url` holds the first resolved address), so a later lookup cannot send the transcript
//!    somewhere the rule did not look. The name is looked up again for every request.
//!
//! An owner who changes the model network's subnet in the compose file must change
//! `EARSHOT_WORKER_MODEL_SUBNET` with it, or the model is treated as remote.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

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

/// A private network's addresses (`a.b.c.d/n` or an IPv6 `.../n`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    network: IpAddr,
    prefix: u8,
}

impl Subnet {
    /// Parses `text` as a CIDR subnet that lies wholly inside the private ranges (rule 2).
    /// `None` for anything else, a host part with bits set included.
    #[must_use]
    pub fn parse_private(text: &str) -> Option<Self> {
        let (addr, prefix) = text.trim().split_once('/')?;
        let network: IpAddr = addr.parse().ok()?;
        let prefix: u8 = prefix.parse().ok()?;
        let subnet = match network {
            IpAddr::V4(a) if prefix <= 32 => {
                let mask = mask_v4(prefix);
                (u32::from(a) & !mask == 0).then_some(Self { network, prefix })?
            }
            IpAddr::V6(a) if prefix <= 128 && a.to_ipv4_mapped().is_none() => {
                let mask = mask_v6(prefix);
                (u128::from(a) & !mask == 0).then_some(Self { network, prefix })?
            }
            _ => return None,
        };
        let (first, last) = subnet.bounds();
        (is_own_network_address(first) && is_own_network_address(last)).then_some(subnet)
    }

    /// The first and last address.
    fn bounds(self) -> (IpAddr, IpAddr) {
        match self.network {
            IpAddr::V4(a) => {
                let last = u32::from(a) | !mask_v4(self.prefix);
                (self.network, IpAddr::V4(Ipv4Addr::from(last)))
            }
            IpAddr::V6(a) => {
                let last = u128::from(a) | !mask_v6(self.prefix);
                (self.network, IpAddr::V6(Ipv6Addr::from(last)))
            }
        }
    }

    /// Whether `ip` lies inside (an IPv4-mapped IPv6 address counts as its IPv4 one).
    #[must_use]
    pub fn contains(self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(a) => a.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.network, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => u32::from(a) & mask_v4(self.prefix) == u32::from(n),
            (IpAddr::V6(n), IpAddr::V6(a)) => u128::from(a) & mask_v6(self.prefix) == u128::from(n),
            _ => false,
        }
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

fn mask_v4(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

fn mask_v6(prefix: u8) -> u128 {
    u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0)
}

/// What counts as the worker's own model: the model service's name and its network's subnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnModel {
    /// The model service's name on the compose network.
    pub service: String,
    /// The model network's fixed subnet.
    pub subnet: Subnet,
}

/// The compose service name `url` names (rule 1's shape), or `None` when it names none.
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

/// Whether `ip` is in a private range a compose network can use.
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

/// The service name `url` names when it is exactly `own`'s.
fn own_service<'a>(url: &'a Url, own: Option<&OwnModel>) -> Option<&'a str> {
    let own = own?;
    service_name(url).filter(|name| name.eq_ignore_ascii_case(&own.service))
}

/// Where `url` is, given what counts as the worker's own model (`own`; `None`: nothing does) and
/// the addresses the host resolved to (`resolved`). A pure function: [`locate`] does the lookup.
#[must_use]
pub fn place_of(url: &Url, own: Option<&OwnModel>, resolved: &[IpAddr]) -> Place {
    let remote = || Place::Remote { url: url.clone() };
    let (Some(_), Some(own)) = (own_service(url, own), own) else {
        return remote();
    };
    if resolved.is_empty()
        || !resolved
            .iter()
            .all(|ip| is_own_network_address(*ip) && own.subnet.contains(*ip))
    {
        return remote();
    }
    let mut checked = url.clone();
    if checked.set_ip_host(resolved[0]).is_err() {
        return remote();
    }
    Place::OwnNetwork { url: checked }
}

/// [`place_of`] with the host looked up by `resolve` (name, port) when it is `own`'s service;
/// any other host is not looked up.
#[must_use]
pub fn locate_with(
    url: &Url,
    own: Option<&OwnModel>,
    resolve: &dyn Fn(&str, u16) -> Vec<IpAddr>,
) -> Place {
    let resolved = match (own_service(url, own), url.port_or_known_default()) {
        (Some(name), Some(port)) => resolve(name, port),
        _ => Vec::new(),
    };
    place_of(url, own, &resolved)
}

/// The system's name lookup (no addresses when it fails).
#[must_use]
pub fn system_resolve(name: &str, port: u16) -> Vec<IpAddr> {
    (name, port)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|a| a.ip()).collect())
        .unwrap_or_default()
}

/// [`locate_with`] the system's name lookup. A name that does not resolve is remote.
#[must_use]
pub fn locate(url: &Url, own: Option<&OwnModel>) -> Place {
    locate_with(url, own, &system_resolve)
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

    fn own_model(service: &str, subnet: &str) -> OwnModel {
        OwnModel {
            service: service.into(),
            subnet: Subnet::parse_private(subnet).unwrap(),
        }
    }

    fn llama() -> OwnModel {
        own_model("llama", "172.31.250.0/24")
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
    fn endpoint_subnets_must_be_private_cidr() {
        let s = Subnet::parse_private("172.31.250.0/24").unwrap();
        assert_eq!(s.to_string(), "172.31.250.0/24");
        assert!(s.contains(ip("172.31.250.1")) && s.contains(ip("172.31.250.255")));
        assert!(s.contains(ip("::ffff:172.31.250.9")));
        assert!(!s.contains(ip("172.31.251.1")) && !s.contains(ip("192.168.1.5")));
        assert!(!s.contains(ip("fd00::1")));
        let v6 = Subnet::parse_private("fd00:e5:0:1::/64").unwrap();
        assert!(v6.contains(ip("fd00:e5:0:1::9")) && !v6.contains(ip("fd00:e5:0:2::9")));
        assert!(Subnet::parse_private(" 10.0.0.0/8 ").is_some());
        for refused in [
            "",
            "172.31.250.0",
            "172.31.250.0/33",
            "172.31.250.1/24",
            "8.8.8.0/24",
            "172.0.0.0/8",
            "0.0.0.0/0",
            "127.0.0.0/8",
            "100.64.0.0/10",
            "2001:db8::/32",
            "::/0",
            "::ffff:172.31.250.0/120",
            "fd00::/129",
            "lots",
        ] {
            assert_eq!(Subnet::parse_private(refused), None, "{refused}");
        }
    }

    #[test]
    fn endpoint_the_service_on_its_subnet_is_the_own_model_at_the_checked_address() {
        let url = u("http://llama:8080/v1");
        let own = llama();
        let place = place_of(&url, Some(&own), &[ip("172.31.250.3")]);
        assert_eq!(
            place,
            Place::OwnNetwork {
                url: u("http://172.31.250.3:8080/v1")
            }
        );
        assert!(place.is_own_network());
        let v6own = own_model("llama", "fd00:e5::/64");
        let v6 = place_of(&url, Some(&v6own), &[ip("fd00:e5::3")]);
        assert_eq!(v6.url().as_str(), "http://[fd00:e5::3]:8080/v1");
        // Without a port, the scheme's own.
        assert_eq!(
            place_of(&u("http://llama/v1"), Some(&own), &[ip("172.31.250.7")])
                .url()
                .as_str(),
            "http://172.31.250.7/v1"
        );
        // The name compares without case (URLs lower-case their hosts).
        let upper = own_model("LLaMA", "172.31.250.0/24");
        assert!(place_of(&url, Some(&upper), &[ip("172.31.250.3")]).is_own_network());
    }

    #[test]
    fn endpoint_an_address_outside_the_model_subnet_is_remote_even_when_private() {
        let url = u("http://llama:8080/v1");
        let own = llama();
        // A LAN machine a router's DNS answers for the bare name: private, but not the model
        // network's.
        for resolved in [
            vec![ip("192.168.1.5")],
            vec![ip("172.31.251.3")],
            vec![ip("10.0.0.9")],
            vec![ip("172.31.250.3"), ip("192.168.1.5")],
        ] {
            assert_eq!(
                place_of(&url, Some(&own), &resolved),
                Place::Remote { url: url.clone() },
                "{resolved:?}"
            );
        }
        // Without a subnet nothing is the worker's own model.
        assert_eq!(
            place_of(&url, None, &[ip("172.31.250.3")]),
            Place::Remote { url: url.clone() }
        );
    }

    #[test]
    fn endpoint_anything_else_is_remote_as_configured() {
        let url = u("http://llama:8080/v1");
        let own = llama();
        // No address, or any address that is not private, is remote.
        for resolved in [
            vec![],
            vec![ip("8.8.8.8")],
            vec![ip("127.0.0.1")],
            vec![ip("100.100.1.1")],
        ] {
            assert_eq!(
                place_of(&url, Some(&own), &resolved),
                Place::Remote { url: url.clone() },
                "{resolved:?}"
            );
        }
        // Only the configured service's name: another bare name is remote however it resolves.
        let model = own_model("model", "172.31.250.0/24");
        assert_eq!(
            place_of(&url, Some(&model), &[ip("172.31.250.3")]),
            Place::Remote { url: url.clone() }
        );
        // A name that is not a service name is remote whatever it resolves to.
        for other in [
            "https://llama:8080/v1",
            "http://llama.lan:8080/v1",
            "http://172.31.250.3:8080/v1",
            "https://api.example.com/v1",
            "http://localhost:8080/v1",
            "http://nas:8080/v1",
        ] {
            let url = u(other);
            assert_eq!(
                place_of(&url, Some(&own), &[ip("172.31.250.3")]),
                Place::Remote { url: url.clone() },
                "{other}"
            );
        }
    }

    #[test]
    fn endpoint_locate_looks_up_only_the_model_service() {
        let own = llama();
        let asked = std::cell::RefCell::new(Vec::new());
        let resolve = |name: &str, port: u16| {
            asked.borrow_mut().push((name.to_owned(), port));
            vec![ip("172.31.250.3")]
        };
        assert!(locate_with(&u("http://llama:8080/v1"), Some(&own), &resolve).is_own_network());
        assert!(!locate_with(&u("http://nas:8080/v1"), Some(&own), &resolve).is_own_network());
        assert!(!locate_with(&u("http://llama:8080/v1"), None, &resolve).is_own_network());
        assert_eq!(asked.into_inner(), vec![("llama".to_owned(), 8080)]);
        // The system's lookup of a name that does not exist: remote.
        let url = u("http://earshot-no-such-service-here:8080/v1");
        let none = own_model("earshot-no-such-service-here", "172.31.250.0/24");
        assert_eq!(
            locate(&url, Some(&none)),
            Place::Remote { url: url.clone() }
        );
    }
}
