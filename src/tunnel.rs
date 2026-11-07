//! Making sure plain `http://` to Immich really goes through Tailscale.
//!
//! A Tailscale-looking address (`100.x.x.x`) is not enough on its own: if
//! Tailscale is off on this Mac, traffic to it goes out over WiFi in the
//! clear, API key included. So at start-up the tool asks macOS which network
//! interface it would use, and refuses unless that interface is Tailscale's
//! tunnel. It then binds every connection to this Mac's own Tailscale
//! address, so if Tailscale goes down mid-session, requests fail instead of
//! falling back to WiFi.

use std::ffi::CStr;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::process::Command;

use crate::config::is_tailscale_ip;

/// How to reach Immich through the tunnel.
#[derive(Debug, Clone, PartialEq)]
pub struct Tunnel {
    /// The name in the URL, e.g. `100.100.10.20` or `immich.tail1234.ts.net`.
    pub host: String,
    /// The Tailscale address it resolved to, and the port.
    pub remote: SocketAddr,
    /// The network interface macOS uses for it, e.g. `utun5`.
    pub interface: String,
    /// This Mac's own Tailscale address on that interface.
    pub local: IpAddr,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TunnelError {
    #[error("IMMICH_URL has no host")]
    NoHost,
    #[error("could not look up {0}: {1}")]
    Lookup(String, String),
    #[error("{0} resolves to {1}, which is not a Tailscale address, so the API key would travel unencrypted")]
    NotTailscale(String, String),
    #[error("could not ask macOS how it reaches {0}: {1}")]
    Route(IpAddr, String),
    #[error(
        "traffic to {0} would go out through {1}, not through Tailscale, so the API key would travel \
         unencrypted. Is Tailscale connected on this Mac?"
    )]
    NotThroughTunnel(IpAddr, String),
}

/// Checks that requests to `url` would go through Tailscale's tunnel right
/// now, and returns how to pin them to it.
///
/// # Errors
/// If the host isn't a Tailscale address, or macOS would send the traffic
/// anywhere other than Tailscale's tunnel.
pub fn through_tailscale(url: &reqwest::Url) -> Result<Tunnel, TunnelError> {
    let host = url.host_str().ok_or(TunnelError::NoHost)?.to_string();
    let port = url.port_or_known_default().unwrap_or(80);

    // A name (e.g. MagicDNS) must resolve to a Tailscale address too.
    // A lookup over WiFi could be spoofed to point anywhere.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (bare, port)
        .to_socket_addrs()
        .map_err(|e| TunnelError::Lookup(host.clone(), e.to_string()))?
        .collect();
    let remote = addrs
        .iter()
        .copied()
        .find(|a| is_tailscale_ip(a.ip()))
        .ok_or_else(|| {
            let found = addrs.iter().map(|a| a.ip().to_string()).collect::<Vec<_>>().join(", ");
            TunnelError::NotTailscale(host.clone(), found)
        })?;

    let interface = route_interface(remote.ip())?;
    // Other VPNs use utun interfaces too; Tailscale's is the one carrying a
    // Tailscale address of this Mac's own.
    let local = interface_addresses(&interface)
        .into_iter()
        .find(|ip| is_tailscale_ip(*ip) && ip.is_ipv4() == remote.is_ipv4())
        .ok_or_else(|| TunnelError::NotThroughTunnel(remote.ip(), interface.clone()))?;

    Ok(Tunnel { host, remote, interface, local })
}

/// The interface macOS would use to reach `ip` (`route -n get`).
fn route_interface(ip: IpAddr) -> Result<String, TunnelError> {
    let family = if ip.is_ipv6() { "-inet6" } else { "-inet" };
    let out = Command::new("/sbin/route")
        .args(["-n", "get", family, &ip.to_string()])
        .output()
        .map_err(|e| TunnelError::Route(ip, e.to_string()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match parse_route_interface(&text) {
        Some(name) if out.status.success() => Ok(name),
        // No route at all (e.g. no network): certainly not the tunnel.
        _ => Err(TunnelError::NotThroughTunnel(ip, "no route".into())),
    }
}

/// The `interface:` line of `route -n get` output.
fn parse_route_interface(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// The IP addresses on one network interface of this Mac.
#[must_use]
pub fn interface_addresses(name: &str) -> Vec<IpAddr> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that we only read,
    // and that freeifaddrs releases exactly once below.
    if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
        return found;
    }
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list from getifaddrs, still alive.
        let entry = unsafe { &*cur };
        cur = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: ifa_name is a NUL-terminated string owned by the list.
        if unsafe { CStr::from_ptr(entry.ifa_name) }.to_bytes() != name.as_bytes() {
            continue;
        }
        // SAFETY: ifa_addr points to a sockaddr whose sa_family says which
        // larger struct it really is; each is read only as that struct.
        unsafe {
            match i32::from((*entry.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*entry.ifa_addr.cast::<libc::sockaddr_in>();
                    found.push(IpAddr::from(u32::from_be(sin.sin_addr.s_addr).to_be_bytes()));
                }
                libc::AF_INET6 => {
                    let sin6 = &*entry.ifa_addr.cast::<libc::sockaddr_in6>();
                    found.push(IpAddr::from(sin6.sin6_addr.s6_addr));
                }
                _ => {}
            }
        }
    }
    // SAFETY: `list` came from a successful getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(list) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_interface_from_route_output() {
        let out = "   route to: 100.100.10.20\n\
                   destination: 100.64.0.0\n\
                          mask: 255.192.0.0\n\
                     interface: utun5\n\
                         flags: <UP,DONE,STATIC>\n";
        assert_eq!(parse_route_interface(out).as_deref(), Some("utun5"));
        assert_eq!(parse_route_interface("route: writing to routing socket: not in table"), None);
    }

    #[test]
    fn finds_the_loopback_address() {
        // Every Mac has lo0 with 127.0.0.1, so this checks the address reading.
        assert!(interface_addresses("lo0").contains(&IpAddr::from([127, 0, 0, 1])));
        assert!(interface_addresses("no-such-interface").is_empty());
    }

    #[test]
    fn a_name_that_is_not_on_tailscale_is_refused() {
        let url = reqwest::Url::parse("http://127.0.0.1:2283").unwrap();
        assert!(matches!(through_tailscale(&url), Err(TunnelError::NotTailscale(..))));
    }
}
