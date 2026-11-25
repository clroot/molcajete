//! Who is on the other end: the client address behind trusted reverse
//! proxies (`[server] trusted_proxies`).
//!
//! `X-Real-IP` and `X-Forwarded-For` are read only when the TCP peer is a
//! trusted proxy; anyone else could put anything there. Behind a proxy that
//! is not listed, every request comes from the proxy's address.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use ipnet::IpNet;

use crate::AppState;

/// Where a request comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The browser's address; `None` without a TCP peer (as in tests).
    pub ip: Option<IpAddr>,
    /// For logs and the audit log: the address, followed by the proxy that
    /// reported it (`203.0.113.9 (via 10.0.0.2)`), or `unknown`.
    pub display: String,
}

fn trusted(ip: IpAddr, proxies: &[IpNet]) -> bool {
    proxies.iter().any(|net| net.contains(&ip))
}

fn parse_ip(text: &str) -> Option<IpAddr> {
    let text = text.trim();
    text.parse::<IpAddr>()
        .ok()
        // `[2001:db8::1]:443` or `192.0.2.1:443`, which some proxies send.
        .or_else(|| text.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .map(|ip| ip.to_canonical())
}

/// The client behind `peer`. A trusted peer's `X-Real-IP` wins (the edge
/// proxy sets it, and proxies in between, such as the Tailscale ingress,
/// pass it through while replacing `X-Forwarded-For`); otherwise the
/// rightmost `X-Forwarded-For` address that is not itself a trusted proxy.
pub fn resolve(headers: &HeaderMap, peer: Option<SocketAddr>, proxies: &[IpNet]) -> Client {
    let Some(peer) = peer.map(|p| p.ip().to_canonical()) else {
        return Client {
            ip: None,
            display: "unknown".to_owned(),
        };
    };
    let forwarded = trusted(peer, proxies)
        .then(|| real_ip(headers).or_else(|| forwarded_for(headers, proxies)))
        .flatten()
        .filter(|ip| *ip != peer);
    match forwarded {
        Some(ip) => Client {
            ip: Some(ip),
            display: format!("{ip} (via {peer})"),
        },
        None => Client {
            ip: Some(peer),
            display: peer.to_string(),
        },
    }
}

fn real_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_ip)
}

fn forwarded_for(headers: &HeaderMap, proxies: &[IpNet]) -> Option<IpAddr> {
    let hops: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .collect();
    let mut client = None;
    for hop in hops.iter().rev() {
        // Whatever is left of a garbled entry was written by the client.
        let Some(ip) = parse_ip(hop) else { break };
        client = Some(ip);
        if !trusted(ip, proxies) {
            break;
        }
    }
    client
}

impl FromRequestParts<Arc<AppState>> for Client {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0);
        Ok(resolve(
            &parts.headers,
            peer,
            &state.config.server.trusted_proxies,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter()
            .map(|n| crate::config::parse_ip_network(n).unwrap())
            .collect()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, value.parse().unwrap());
        }
        headers
    }

    fn display(headers: &HeaderMap, peer: &str, proxies: &[IpNet]) -> String {
        resolve(headers, Some(peer.parse().unwrap()), proxies).display
    }

    #[test]
    fn headers_from_untrusted_peers_are_ignored() {
        let spoofed = headers(&[
            ("x-real-ip", "203.0.113.7"),
            ("x-forwarded-for", "198.51.100.1"),
        ]);
        assert_eq!(display(&spoofed, "192.0.2.5:5000", &[]), "192.0.2.5");
        let proxies = nets(&["10.0.0.0/8"]);
        assert_eq!(display(&spoofed, "192.0.2.5:5000", &proxies), "192.0.2.5");
        let client = resolve(&spoofed, Some("192.0.2.5:5000".parse().unwrap()), &proxies);
        assert_eq!(client.ip, Some("192.0.2.5".parse().unwrap()));
        assert_eq!(resolve(&spoofed, None, &proxies).display, "unknown");
    }

    #[test]
    fn trusted_proxies_name_the_client() {
        let proxies = nets(&["10.0.0.0/8", "fd00::/8"]);
        let peer = "10.0.0.2:5000";
        assert_eq!(display(&HeaderMap::new(), peer, &proxies), "10.0.0.2");
        let xff = headers(&[("x-forwarded-for", "100.96.0.1, 10.0.0.1")]);
        assert_eq!(display(&xff, peer, &proxies), "100.96.0.1 (via 10.0.0.2)");
        // A client cannot hide behind an entry of its own making.
        let spoofed = headers(&[("x-forwarded-for", "1.2.3.4, 198.51.100.9, 10.0.0.1")]);
        assert_eq!(
            display(&spoofed, peer, &proxies),
            "198.51.100.9 (via 10.0.0.2)"
        );
        let garbled = headers(&[("x-forwarded-for", "nonsense, 10.0.0.9")]);
        assert_eq!(display(&garbled, peer, &proxies), "10.0.0.9 (via 10.0.0.2)");
        let split = headers(&[
            ("x-forwarded-for", "198.51.100.3"),
            ("x-forwarded-for", "10.0.0.1"),
        ]);
        assert_eq!(
            display(&split, peer, &proxies),
            "198.51.100.3 (via 10.0.0.2)"
        );
        let real = headers(&[
            ("x-real-ip", "203.0.113.7"),
            ("x-forwarded-for", "100.96.0.1"),
        ]);
        assert_eq!(display(&real, peer, &proxies), "203.0.113.7 (via 10.0.0.2)");
        let v6 = headers(&[("x-real-ip", "[2001:db8::1]:443")]);
        assert_eq!(
            display(&v6, "[fd00::5]:80", &proxies),
            "2001:db8::1 (via fd00::5)"
        );
        let mapped = headers(&[("x-real-ip", "::ffff:203.0.113.8")]);
        assert_eq!(
            display(&mapped, "[::ffff:10.0.0.2]:80", &proxies),
            "203.0.113.8 (via 10.0.0.2)"
        );
    }
}
