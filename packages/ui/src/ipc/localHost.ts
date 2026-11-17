// Whether an endpoint address is this computer or the local network: the UI's copy of
// earshot-net's `url_policy::is_local_url`, for the first-run wizard's warning while an address is
// typed and not yet stored (T12.79 review) and for the mock. Pinned to the Rust rule by
// `fixtures/net/local_host_cases.txt`, which both suites read; a rule change edits both copies and
// the table in one commit.

/** A typed endpoint address's host as the app names it (IPv6 without brackets) and whether it is
 * local; null when the text is not an address with a host. */
export interface EndpointHost {
  host: string;
  local: boolean;
}

/** The host of `url` and whether it is local, or null when `url` is not an address with a host. */
export function endpointHost(url: string): EndpointHost | null {
  let parsed: URL;
  try {
    parsed = new URL(url.trim());
  } catch {
    return null;
  }
  const host = parsed.hostname.replace(/^\[|\]$/g, "");
  if (host === "") return null;
  return { host, local: isLocalHost(host) };
}

/** `localhost`, IPv4 loopback and RFC 1918 ranges, IPv6 loopback and unique local addresses
 * (fc00::/7), and IPv4-mapped forms of the IPv4 ones. Any other host name is not local. `host` is
 * a WHATWG URL hostname (IPv4 dotted, IPv6 compressed and lower case), brackets optional. */
export function isLocalHost(host: string): boolean {
  const h = host.replace(/^\[|\]$/g, "").toLowerCase();
  if (h === "localhost") return true;
  if (h.includes(":")) return isLocalIpv6(h);
  return isLocalIpv4(h);
}

function isLocalIpv4(h: string): boolean {
  const v4 = h.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
  if (!v4) return false;
  const [a, b] = [Number(v4[1]), Number(v4[2])];
  return a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
}

function isLocalIpv6(h: string): boolean {
  if (h === "::1") return true;
  // IPv4-mapped, as the URL parser writes it: ::ffff:XXXX:XXXX.
  const mapped = h.match(/^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/);
  if (mapped) {
    const hi = parseInt(mapped[1], 16);
    const lo = parseInt(mapped[2], 16);
    return isLocalIpv4(`${hi >> 8}.${hi & 0xff}.${lo >> 8}.${lo & 0xff}`);
  }
  // fc00::/7: the first segment is fcXX or fdXX (written without leading zeros, so 4 digits).
  return /^f[cd][0-9a-f]{2}:/.test(h);
}
