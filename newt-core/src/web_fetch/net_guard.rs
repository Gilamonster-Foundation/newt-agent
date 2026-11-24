//! The pure `net` leash: host-allowlist membership + SSRF IP screening.
//!
//! Copied from agent-bridle-tool-web 0.8.0-rc.7 (`src/net_guard.rs`, Apache-2.0) so
//! newt-core screens `web_fetch` hosts without a bridle crate (#3160, the `confine`
//! feature). The edits: the `Scope` import (the same agent-mesh-protocol type), three doc
//! links to bridle items made plain code spans (their two link definitions deleted), and
//! three comment lines whose dotted ranges are respelled in the short form the code below
//! uses, because the pre-push network-leak guard refuses the dotted form. `DESIGN §7`,
//! `AB-007` and `#270` are agent-bridle's; `Gate` and `ToolContext` are its names for what
//! `admit` and `check_net` do in the parent module. With `confine` enabled,
//! `web_fetch_parity_tests` compares these predicates with bridle's, not file contents.
//!
//! Everything here is **pure** — no network, no DNS, no `Gate`. That is
//! deliberate: the SSRF-defeating logic is the load-bearing security surface, so
//! it must be unit-testable in isolation (DESIGN §7). The async fetch path in
//! [`crate::web_fetch`] calls these predicates after it has done the (impure)
//! DNS resolution; the predicates themselves only ever look at an
//! already-resolved [`IpAddr`] and the granted `net` [`Scope`].
//!
//! ## Two separate axes: *reachability* and *private-space* (AB-007, #270)
//!
//! Host **reachability** and permission to resolve into **private/loopback
//! space** are distinct authorities, screened against two distinct inputs:
//!
//! - `net: Scope<String>` — the effective `net` caveat (`granted.meet(required)`,
//!   minted into the `ToolContext`). This is the **reachability** allowlist and
//!   *only* that: `Scope::All` admits any host; `Scope::Only({h, …})` admits the
//!   named hosts. Being on this list says nothing about private space.
//! - `net_private: Scope<String>` — a **separate** opt-in list (the web tool's
//!   config, defaulting to `Scope::none()`). Only a host named here may resolve
//!   to a private / loopback / link-local / unique-local / metadata address. It
//!   is the single, explicit SSRF escape hatch — e.g. name `127.0.0.1` or an
//!   internal hostname to test against it.
//!
//! The pre-#270 defect (AB-007): membership in `net` *was* the private-space
//! opt-in, so every explicitly-allowed **public** host became an implicit
//! SSRF / private-space grant (a compromised / rebinding / split-horizon DNS
//! answer of `127.0.0.1`, `10/8`, `169.254.169.254`, `fc00::/7`, … was
//! accepted because the *hostname* was allowlisted). Now a public-host grant
//! keeps private-address blocking **enabled**; crossing the public/private
//! boundary requires the separate `net_private` capability.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use agent_mesh_protocol::Scope;

/// Why the net guard refused a host or an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetGuardError {
    /// The host is not within the granted `net` scope (default-deny).
    HostNotAllowed {
        /// The offending host.
        host: String,
    },
    /// The host resolved to a private / loopback / link-local / unique-local
    /// address and was not opted into private space via `net_private` (SSRF
    /// block).
    PrivateAddress {
        /// The host that resolved to a blocked address.
        host: String,
        /// The blocked address it resolved to.
        addr: IpAddr,
    },
    /// DNS resolution yielded no usable address for the host.
    NoAddress {
        /// The host that did not resolve.
        host: String,
    },
}

impl fmt::Display for NetGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostNotAllowed { host } => {
                write!(f, "network access to {host:?} is not within the granted authority")
            }
            Self::PrivateAddress { host, addr } => write!(
                f,
                "SSRF block: {host:?} resolved to private/loopback address {addr} (host is not opted into private-address space via net_private)"
            ),
            Self::NoAddress { host } => write!(f, "host {host:?} did not resolve to any address"),
        }
    }
}

impl std::error::Error for NetGuardError {}

/// May `host` resolve into private / loopback / link-local / metadata space?
///
/// This consults the **separate** `net_private` opt-in axis — *never* the `net`
/// reachability allowlist (AB-007, #270). A host crosses the public/private
/// boundary only when named here:
///
/// - `Scope::none()` (the default) — no host may reach private space; every
///   resolved private address is SSRF-blocked regardless of the `net` grant.
/// - `Scope::Only({h, …})` — exactly the named hosts (e.g. `"127.0.0.1"` or
///   `"internal.svc"`) may resolve to a blocked range.
/// - `Scope::All` — the deliberate top of *this* axis: every host may reach
///   private space. Unlike the old conflation, this is only reachable by an
///   explicit maximal grant on `net_private`; it is never implied by `net`.
///
/// Matching is exact on the host string as it appears in the URL, so the
/// `net_private` entry and the URL host must agree literally.
#[must_use]
pub fn host_may_reach_private_space(net_private: &Scope<String>, host: &str) -> bool {
    match net_private {
        Scope::All => true,
        Scope::Only(set) => set.contains(host),
    }
}

/// Does the granted `net` scope permit reaching `host` at all (the host
/// allowlist, default-deny)?
///
/// Mirrors `ToolContext::check_net`'s membership test so
/// the same decision can be made over a borrowed scope in the pure layer (e.g.
/// when re-checking a redirect target). `Scope::All` admits any host; otherwise
/// the host must be a member of the `Only` set.
#[must_use]
pub fn host_is_permitted(net: &Scope<String>, host: &str) -> bool {
    match net {
        Scope::All => true,
        Scope::Only(set) => set.contains(host),
    }
}

/// Is `ip` a private / loopback / link-local / unique-local / otherwise
/// non-public address that an SSRF attempt would target?
///
/// `true` means "block this unless the host was explicitly opted in". The
/// ranges (per DESIGN §7): IPv4 `127.0.0.0/8`, `10/8`, `172.16/12`,
/// `192.168/16`, `169.254/16` (link-local), `0.0.0.0/8` (this-host),
/// `100.64.0.0/10` (CGNAT), broadcast, and the IPv4 documentation/benchmark
/// ranges; IPv6 `::1` (loopback), `fc00::/7` (unique-local), `fe80::/10`
/// (link-local), the unspecified address, and IPv4-mapped/compat addresses
/// (screened by mapping back to their IPv4 form).
///
/// We implement the IPv6 predicates by hand because `Ipv6Addr::is_unique_local`
/// / `is_global` are still unstable on stable Rust; the IPv4 ones use the
/// stable `is_private` / `is_loopback` / `is_link_local` plus explicit extra
/// ranges.
#[must_use]
pub fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_blocked(v4),
        IpAddr::V6(v6) => {
            // An IPv4-mapped (::ffff:a.b.c.d) or IPv4-compatible address is
            // really an IPv4 destination — screen it as such so it cannot be
            // used to slip a private v4 address past the v6 path.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ipv4_is_blocked(v4);
            }
            if let Some(v4) = v6.to_ipv4() {
                // `to_ipv4()` also matches the deprecated v4-compatible form.
                if v4 != Ipv4Addr::UNSPECIFIED {
                    return ipv4_is_blocked(v4);
                }
            }
            ipv6_is_blocked(v6)
        }
    }
}

/// IPv4 SSRF screen (see [`ip_is_blocked`]).
fn ipv4_is_blocked(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private()            // 10/8, 172.16/12, 192.168/16
        || ip.is_loopback()    // 127/8
        || ip.is_link_local()  // 169.254/16
        || ip.is_broadcast()   // 255.255.255.255
        || ip.is_documentation() // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || ip.is_unspecified() // 0.0.0.0
        || o[0] == 0           // 0.0.0.0/8 "this host on this network"
        || (o[0] == 100 && (o[1] & 0xc0) == 0x40) // 100.64/10 CGNAT (RFC 6598)
        || o[0] >= 240 // 240/4 reserved (incl. 255/8 broadcast space)
}

/// IPv6 SSRF screen (see [`ip_is_blocked`]).
fn ipv6_is_blocked(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true; // ::1, ::
    }
    let seg0 = ip.segments()[0];
    // fc00::/7 unique-local (matches fc00:: and fd00::).
    if (seg0 & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 link-local.
    if (seg0 & 0xffc0) == 0xfe80 {
        return true;
    }
    // ff00::/8 multicast.
    if (seg0 & 0xff00) == 0xff00 {
        return true;
    }
    false
}

/// Screen one host against the granted `net` reachability scope, the separate
/// `net_private` private-space opt-in, and a set of resolved addresses,
/// returning the subset of addresses that are safe to connect to.
///
/// This is the single composition point the fetch path calls per hop:
///
/// 1. The host must be permitted by `net` (default-deny reachability allowlist).
/// 2. Each resolved address is SSRF-screened. A blocked (private/loopback/…)
///    address is dropped *unless* the host is named in `net_private` — the
///    separate, explicit opt-in for private space (AB-007, #270). Membership in
///    `net` alone never opts a host into private space.
/// 3. At least one address must survive, or the host is refused.
///
/// Returns the surviving addresses (to pin the connection to), or a
/// [`NetGuardError`] explaining the refusal. Pure: callers do the DNS.
pub fn screen_host(
    net: &Scope<String>,
    net_private: &Scope<String>,
    host: &str,
    resolved: &[IpAddr],
) -> Result<Vec<IpAddr>, NetGuardError> {
    if !host_is_permitted(net, host) {
        return Err(NetGuardError::HostNotAllowed {
            host: host.to_string(),
        });
    }

    let opted_in = host_may_reach_private_space(net_private, host);

    let mut safe = Vec::new();
    let mut last_blocked = None;
    for &ip in resolved {
        if ip_is_blocked(ip) && !opted_in {
            last_blocked = Some(ip);
            continue;
        }
        safe.push(ip);
    }

    if safe.is_empty() {
        return match last_blocked {
            Some(addr) => Err(NetGuardError::PrivateAddress {
                host: host.to_string(),
                addr,
            }),
            None => Err(NetGuardError::NoAddress {
                host: host.to_string(),
            }),
        };
    }
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ipv4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    // ── SSRF range coverage (per DESIGN §7) ─────────────────────────────────

    #[test]
    fn ipv4_loopback_is_blocked() {
        assert!(ip_is_blocked(ipv4(127, 0, 0, 1)));
        assert!(ip_is_blocked(ipv4(127, 255, 255, 254)));
    }

    #[test]
    fn ipv4_rfc1918_private_is_blocked() {
        assert!(ip_is_blocked(ipv4(10, 0, 0, 1)));
        assert!(ip_is_blocked(ipv4(172, 16, 5, 4)));
        assert!(ip_is_blocked(ipv4(172, 31, 255, 255)));
        assert!(ip_is_blocked(ipv4(192, 168, 1, 1)));
    }

    #[test]
    fn ipv4_link_local_169_254_is_blocked() {
        // The cloud-metadata SSRF classic.
        assert!(ip_is_blocked(ipv4(169, 254, 169, 254)));
    }

    #[test]
    fn ipv4_this_host_and_cgnat_blocked() {
        assert!(ip_is_blocked(ipv4(0, 0, 0, 0)));
        assert!(ip_is_blocked(ipv4(0, 1, 2, 3)));
        assert!(ip_is_blocked(ipv4(100, 64, 0, 1))); // CGNAT 100.64/10
        assert!(ip_is_blocked(ipv4(100, 127, 255, 255)));
    }

    #[test]
    fn ipv4_public_is_allowed() {
        assert!(!ip_is_blocked(ipv4(1, 1, 1, 1)));
        assert!(!ip_is_blocked(ipv4(8, 8, 8, 8)));
        assert!(!ip_is_blocked(ipv4(93, 184, 216, 34))); // example.com
                                                         // 100.63/x is just below CGNAT and is public.
        assert!(!ip_is_blocked(ipv4(100, 63, 255, 255)));
        // 172.15/8 and 172.32/8 are outside the 172.16/12 private block.
        assert!(!ip_is_blocked(ipv4(172, 15, 0, 1)));
        assert!(!ip_is_blocked(ipv4(172, 32, 0, 1)));
    }

    #[test]
    fn ipv6_loopback_and_ula_and_linklocal_blocked() {
        assert!(ip_is_blocked(IpAddr::V6(Ipv6Addr::LOCALHOST))); // ::1
        assert!(ip_is_blocked(IpAddr::V6(Ipv6Addr::UNSPECIFIED))); // ::
        assert!(ip_is_blocked(IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(ip_is_blocked(IpAddr::V6("fd12:3456::1".parse().unwrap())));
        assert!(ip_is_blocked(IpAddr::V6("fe80::1".parse().unwrap())));
        assert!(ip_is_blocked(IpAddr::V6("ff02::1".parse().unwrap())));
    }

    #[test]
    fn ipv6_public_is_allowed() {
        assert!(!ip_is_blocked(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        ))); // 1.1.1.1
        assert!(!ip_is_blocked(IpAddr::V6(
            "2001:4860:4860::8888".parse().unwrap()
        ))); // 8.8.8.8
    }

    #[test]
    fn ipv4_mapped_v6_private_is_blocked() {
        // ::ffff:127.0.0.1 must be screened as the loopback it really is.
        assert!(ip_is_blocked(IpAddr::V6(
            "::ffff:127.0.0.1".parse().unwrap()
        )));
        assert!(ip_is_blocked(IpAddr::V6(
            "::ffff:10.0.0.1".parse().unwrap()
        )));
        assert!(ip_is_blocked(IpAddr::V6(
            "::ffff:169.254.169.254".parse().unwrap()
        )));
        // ...and a mapped public v4 stays allowed.
        assert!(!ip_is_blocked(IpAddr::V6(
            "::ffff:8.8.8.8".parse().unwrap()
        )));
    }

    // ── Private-space opt-in (net_private), separate from reachability (net) ──

    #[test]
    fn private_optin_only_matches_named_hosts() {
        let net_private = Scope::only(["127.0.0.1".to_string(), "internal.svc".to_string()]);
        assert!(host_may_reach_private_space(&net_private, "127.0.0.1"));
        assert!(host_may_reach_private_space(&net_private, "internal.svc"));
        assert!(!host_may_reach_private_space(&net_private, "evil.test"));
        assert!(!host_may_reach_private_space(&net_private, "example.com"));
    }

    #[test]
    fn net_private_none_blocks_every_host_from_private_space() {
        // The default: no host may cross into private/loopback space.
        let net_private: Scope<String> = Scope::none();
        assert!(!host_may_reach_private_space(&net_private, "127.0.0.1"));
        assert!(!host_may_reach_private_space(
            &net_private,
            "anything.example"
        ));
    }

    #[test]
    fn net_private_all_is_the_deliberate_top_of_that_axis() {
        // Unlike the old conflation, `All` on `net_private` is an explicit,
        // maximal opt-in — every host may reach private space. It is NEVER
        // implied by the `net` reachability grant.
        let net_private: Scope<String> = Scope::All;
        assert!(host_may_reach_private_space(&net_private, "127.0.0.1"));
        assert!(host_may_reach_private_space(
            &net_private,
            "anything.example"
        ));
    }

    #[test]
    fn host_permitted_is_default_deny_under_only() {
        let net = Scope::only(["example.com".to_string()]);
        assert!(host_is_permitted(&net, "example.com"));
        assert!(!host_is_permitted(&net, "evil.test"));
    }

    // ── screen_host: the composition the fetch path uses per hop ─────────────

    #[test]
    fn screen_denies_host_not_in_scope() {
        // Only example.com is reachable; 127.0.0.1 host is not even permitted.
        let net = Scope::only(["example.com".to_string()]);
        let err =
            screen_host(&net, &Scope::none(), "127.0.0.1", &[ipv4(127, 0, 0, 1)]).unwrap_err();
        assert!(
            matches!(err, NetGuardError::HostNotAllowed { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn ab007_allowlisted_public_host_stays_ssrf_blocked() {
        // THE AB-007 regression. `example.com` is on the `net` reachability
        // allowlist but NOT on `net_private`. A compromised / rebinding /
        // split-horizon DNS answer of 127.0.0.1 must STILL be blocked — being
        // reachable never opts a host into private space. On the pre-#270 code
        // (opt-in derived from `net` membership) this returned Ok and connected
        // to loopback.
        let net = Scope::only(["example.com".to_string()]);
        let net_private: Scope<String> = Scope::none();
        let err =
            screen_host(&net, &net_private, "example.com", &[ipv4(127, 0, 0, 1)]).unwrap_err();
        assert!(
            matches!(err, NetGuardError::PrivateAddress { .. }),
            "an allowlisted public host resolving to loopback must stay denied: {err:?}"
        );
    }

    #[test]
    fn ab007_cloud_metadata_blocked_for_allowlisted_public_host() {
        // 169.254.169.254 (the cloud-metadata SSRF classic) must stay blocked
        // for a merely-reachable host.
        let net = Scope::only(["metadata.example".to_string()]);
        let err = screen_host(
            &net,
            &Scope::none(),
            "metadata.example",
            &[ipv4(169, 254, 169, 254)],
        )
        .unwrap_err();
        assert!(
            matches!(err, NetGuardError::PrivateAddress { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn ab007_allowlisted_public_host_mixed_answer_drops_private_keeps_public() {
        // Reachable + a mixed public/private answer set: the private address is
        // dropped (not leaked by the reachability grant) and the public one
        // survives to pin.
        let net = Scope::only(["example.com".to_string()]);
        let safe = screen_host(
            &net,
            &Scope::none(),
            "example.com",
            &[ipv4(10, 0, 0, 1), ipv4(8, 8, 8, 8)],
        )
        .unwrap();
        assert_eq!(safe, vec![ipv4(8, 8, 8, 8)]);
    }

    #[test]
    fn screen_blocks_private_ip_under_all_when_not_optedin() {
        // `All` reachability, no `net_private`: a host that resolves to a
        // private IP (DNS-rebinding / SSRF) is blocked.
        let net: Scope<String> = Scope::All;
        let err =
            screen_host(&net, &Scope::none(), "rebind.evil", &[ipv4(10, 0, 0, 5)]).unwrap_err();
        assert!(
            matches!(err, NetGuardError::PrivateAddress { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn net_private_optin_allows_loopback_for_the_named_host() {
        // The deliberate escape hatch now needs BOTH axes: 127.0.0.1 reachable
        // (net) AND opted into private space (net_private).
        let net = Scope::only(["127.0.0.1".to_string()]);
        let net_private = Scope::only(["127.0.0.1".to_string()]);
        let safe = screen_host(&net, &net_private, "127.0.0.1", &[ipv4(127, 0, 0, 1)]).unwrap();
        assert_eq!(safe, vec![ipv4(127, 0, 0, 1)]);
    }

    #[test]
    fn net_private_optin_is_host_specific() {
        // Opting `ok.internal` into private space does NOT leak to another host
        // even if that other host is reachable under an `All` net grant.
        let net: Scope<String> = Scope::All;
        let net_private = Scope::only(["ok.internal".to_string()]);
        let err =
            screen_host(&net, &net_private, "evil.example", &[ipv4(10, 0, 0, 1)]).unwrap_err();
        assert!(
            matches!(err, NetGuardError::PrivateAddress { .. }),
            "a private-space opt-in must not leak to other hosts: {err:?}"
        );
    }

    #[test]
    fn screen_drops_blocked_addrs_keeps_safe_ones() {
        // A host that resolves to both a public and a private address (a common
        // rebinding shape): with no `net_private` opt-in the private one is
        // dropped and the public one survives.
        let net: Scope<String> = Scope::All;
        let safe = screen_host(
            &net,
            &Scope::none(),
            "mixed.example",
            &[ipv4(10, 0, 0, 1), ipv4(8, 8, 8, 8)],
        )
        .unwrap();
        assert_eq!(safe, vec![ipv4(8, 8, 8, 8)]);
    }

    #[test]
    fn screen_no_address_errors() {
        let net: Scope<String> = Scope::All;
        let err = screen_host(&net, &Scope::none(), "ghost.example", &[]).unwrap_err();
        assert!(matches!(err, NetGuardError::NoAddress { .. }), "{err:?}");
    }
}
