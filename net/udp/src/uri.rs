// Copyright (C) 2024-2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Shared URI parsing utilities for UDP elements.

use gst::glib;
use std::net::{IpAddr, Ipv6Addr, ToSocketAddrs};

/// A resolved destination address with an optional original hostname.
///
/// The `addr` field always contains the resolved IP address. The `name` field
/// preserves the original hostname.
#[derive(Debug, Clone)]
pub struct Host {
    /// The resolved IP address.
    pub addr: IpAddr,
    /// The original hostname string, if it was a hostname rather than a literal IP.
    pub name: Option<String>,
    /// The string representation of `addr`.
    pub addr_string: String,
}

impl PartialEq for Host {
    fn eq(&self, other: &Self) -> bool {
        // If both were created from a hostname then compare the hostnames, otherwise
        // if both were created from an IP compare the IPs, otherwise not created from
        // the same source and not equal.
        if self.name.is_none() && other.name.is_none() {
            self.addr == other.addr
        } else if let Some((s, o)) = Option::zip(self.name.as_ref(), other.name.as_ref()) {
            s == o
        } else {
            false
        }
    }
}

impl Eq for Host {}

impl PartialEq<str> for Host {
    fn eq(&self, other: &str) -> bool {
        if let Some(ref name) = self.name {
            name == other
        } else if let Ok(other_addr) = other.parse::<IpAddr>() {
            self.addr == other_addr
        } else {
            false
        }
    }
}

impl std::hash::Hash for Host {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Consistent with PartialEq. Keep in sync with ClientRef::hash
        match &self.name {
            Some(name) => {
                1u8.hash(state);
                name.hash(state);
            }
            None => {
                0u8.hash(state);
                self.addr.hash(state);
            }
        }
    }
}

impl Host {
    /// Create a host from a literal IP address (no original hostname).
    pub fn new(addr: IpAddr) -> Self {
        Host {
            addr,
            name: None,
            addr_string: addr.to_string(),
        }
    }

    /// Create a host from a resolved IP with the original hostname preserved.
    pub fn with_name(addr: IpAddr, name: String) -> Self {
        Host {
            addr,
            name: Some(name),
            addr_string: addr.to_string(),
        }
    }

    /// Resolve a hostname or IP address string into a `Host`.
    /// Returns an error if the hostname cannot be resolved.
    pub fn resolve(host: &str) -> Result<Self, glib::Error> {
        if let Ok(addr) = host.parse::<IpAddr>() {
            return Ok(Host::new(addr));
        }

        let saddr = (host, 0u16)
            .to_socket_addrs()
            .map_err(|err| {
                glib::Error::new(
                    gst::URIError::BadUri,
                    format!("Couldn't resolve host '{host}': {err}").as_str(),
                )
            })?
            .next()
            .ok_or_else(|| {
                glib::Error::new(
                    gst::URIError::BadUri,
                    format!("Couldn't resolve host '{host}'").as_str(),
                )
            })?;

        Ok(Host::with_name(saddr.ip(), host.to_string()))
    }

    /// Returns the original hostname if present, otherwise the IP address as a string.
    pub fn as_str(&self) -> &str {
        match self.name {
            Some(ref name) => name,
            None => &self.addr_string,
        }
    }

    /// Returns the host string suitable for use in a URI.
    /// Bare IPv6 addresses are wrapped in `[]` brackets as required by URI syntax.
    pub fn uri_string(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => {
                if self.addr.is_ipv6() {
                    format!("[{}]", self.addr_string)
                } else {
                    self.addr_string.clone()
                }
            }
        }
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref name) = self.name {
            f.write_str(name)
        } else {
            write!(f, "{}", self.addr)
        }
    }
}

/// Parse the scheme from a URI. Returns the remainder after `://`.
///
/// Validates that the scheme is `udp`.
fn parse_scheme(uri: &str) -> Result<&str, glib::Error> {
    let Some((scheme, remainder)) = uri.split_once("://") else {
        return Err(glib::Error::new(
            gst::URIError::BadUri,
            "Invalid URI format",
        ));
    };

    if scheme.to_lowercase() != "udp" {
        return Err(glib::Error::new(
            gst::URIError::UnsupportedProtocol,
            format!("Unsupported URI scheme {scheme}").as_str(),
        ));
    }

    Ok(remainder)
}

/// Parse a single `host:port` or `[host]:port` address.
///
/// Hostnames are resolved to IP addresses, with the original name preserved.
pub fn parse_client_str(spec: &str) -> Result<(Host, u16), glib::Error> {
    let (host, port_str) = if let Some(remainder) = spec.strip_prefix('[') {
        let (ip, remainder) = remainder.split_once(']').ok_or_else(|| {
            glib::Error::new(
                gst::URIError::BadUri,
                "Invalid address: missing ']' after IPv6 address",
            )
        })?;

        let port_str = remainder.strip_prefix(':').ok_or_else(|| {
            glib::Error::new(gst::URIError::BadUri, "Invalid address: missing port")
        })?;

        let ip = ip.parse::<Ipv6Addr>().map_err(|err| {
            glib::Error::new(
                gst::URIError::BadUri,
                format!("Invalid IPv6 address: {err}").as_str(),
            )
        })?;

        (Host::new(IpAddr::V6(ip)), port_str)
    } else {
        let (host_str, port_str) = spec.split_once(':').ok_or_else(|| {
            glib::Error::new(gst::URIError::BadUri, "Invalid address: missing port")
        })?;

        (Host::resolve(host_str)?, port_str)
    };

    let port = port_str.parse::<u16>().map_err(|err| {
        glib::Error::new(
            gst::URIError::BadUri,
            format!("Invalid port: {err}").as_str(),
        )
    })?;

    Ok((host, port))
}

/// Parse the host and port from a URI remainder (after the scheme).
///
/// Returns the query string after `?` if present.
fn parse_address_port(remainder: &str) -> Result<(Host, u16, Option<&str>), glib::Error> {
    let (addr, query_str) = match remainder.split_once('?') {
        Some((addr, query)) => (addr, Some(query)),
        None => (remainder, None),
    };

    let (host, port) = parse_client_str(addr)?;
    Ok((host, port, query_str))
}

/// Parse the query string portion of a URI (after `?`).
///
/// Returns an iterator over (key, value) pairs.
fn parse_query(query: &str) -> impl Iterator<Item = (&str, &str)> {
    query.split('&').filter_map(|s| s.split_once('='))
}

/// Parse a comma-separated list of IP addresses or hostnames.
pub fn parse_source_filter(source_filter: &str) -> Result<Vec<IpAddr>, glib::Error> {
    let mut addrs = Vec::new();

    if source_filter.is_empty() {
        return Ok(addrs);
    }

    for addr_str in source_filter.split(',').map(str::trim) {
        if addr_str.is_empty() {
            continue;
        }

        let addr = match addr_str.parse::<IpAddr>() {
            Ok(addr) => addr,
            Err(_err) => {
                let saddr = (addr_str, 0u16)
                    .to_socket_addrs()
                    .map_err(|err| {
                        glib::Error::new(
                            gst::URIError::BadUri,
                            format!("Couldn't resolve source filter address: {err}").as_str(),
                        )
                    })?
                    .next()
                    .ok_or_else(|| {
                        glib::Error::new(
                            gst::URIError::BadUri,
                            "Couldn't resolve source filter address",
                        )
                    })?;

                saddr.ip()
            }
        };

        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }

    Ok(addrs)
}

/// Parse a multicast source filter string in the legacy +/- format of the old udpsrc.
///
/// Positive sources (+addr or just addr) are added to the filter list.
/// Negative sources (-addr) are ignored (not supported by the old udpsrc).
fn parse_multicast_source(mut multicast_source: &str) -> Result<Vec<IpAddr>, glib::Error> {
    let mut addrs = Vec::new();

    while !multicast_source.is_empty() {
        let (positive, remainder) = if let Some(remainder) = multicast_source.strip_prefix('+') {
            (true, remainder)
        } else if let Some(remainder) = multicast_source.strip_prefix('-') {
            (false, remainder)
        } else {
            // Assume it's a positive source
            (true, multicast_source)
        };

        let next_idx = remainder.match_indices(['+', '-']).next();
        let (addr, remainder) = next_idx
            .map(|(next_idx, _)| remainder.split_at(next_idx))
            .unwrap_or((remainder, ""));

        let addr = if addr.is_empty() {
            return Err(glib::Error::new(
                gst::URIError::BadUri,
                "Invalid empty URI host",
            ));
        } else {
            let saddr = (addr, 0u16)
                .to_socket_addrs()
                .map_err(|err| {
                    glib::Error::new(
                        gst::URIError::BadUri,
                        format!("Couldn't resolve URI host: {err}").as_str(),
                    )
                })?
                .next()
                .ok_or_else(|| {
                    glib::Error::new(gst::URIError::BadUri, "Couldn't resolve URI host")
                })?;

            saddr.ip()
        };

        if positive {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        } else {
            // Negative filters are ignored here as old udpsrc did not support them anyway.
        }

        multicast_source = remainder;
    }

    Ok(addrs)
}

/// Parse a full UDP URI for the source element.
pub fn parse_uri_for_src(uri: &str) -> Result<(Host, u16, Vec<IpAddr>, bool), glib::Error> {
    let remainder = parse_scheme(uri)?;
    let (host, port, query_str) = parse_address_port(remainder)?;

    let (source_filter, source_filter_exclusive) = if let Some(query) = query_str {
        let mut source_filter = Vec::new();
        let mut source_filter_exclusive = false;

        for (key, value) in parse_query(query) {
            match key {
                "source-filter" => {
                    source_filter = parse_source_filter(value)?;
                }
                "source-filter-exclusive" => {
                    source_filter_exclusive = match value {
                        "true" | "1" => true,
                        "false" | "0" => false,
                        _ => {
                            return Err(glib::Error::new(
                                gst::URIError::BadUri,
                                format!("Invalid source-filter-exclusive value {value}").as_str(),
                            ));
                        }
                    };
                }
                "multicast-source" => {
                    // Backwards compatibility with old udpsrc. Theoretically it supported mixed
                    // inclusive and exclusive filters, which made no sense and only inclusive
                    // filters we supported anyway so that's what we do here as a best effort.
                    source_filter = parse_multicast_source(value)?;
                    source_filter_exclusive = false;
                }
                _ => {}
            }
        }

        (source_filter, source_filter_exclusive)
    } else {
        (Vec::new(), false)
    };

    Ok((host, port, source_filter, source_filter_exclusive))
}

/// Parse a full UDP URI for the sink element.
pub fn parse_uri_for_sink(uri: &str) -> Result<(Host, u16), glib::Error> {
    let remainder = parse_scheme(uri)?;
    let (host, port, _query) = parse_address_port(remainder)?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_parse_scheme() {
        assert_eq!(parse_scheme("udp://host:5000").unwrap(), "host:5000");

        let Err(err) = parse_scheme("http://host:5000") else {
            unreachable!();
        };
        assert_eq!(
            err.kind::<gst::URIError>(),
            Some(gst::URIError::UnsupportedProtocol)
        );

        let Err(err) = parse_scheme("host:5000") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));
    }

    #[test]
    fn test_parse_address_port() {
        let (host, port, query) = parse_address_port("0.0.0.0:5000").unwrap();
        assert_eq!(host.addr, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(host.name, None);
        assert_eq!(host.as_str(), "0.0.0.0");
        assert_eq!(host.uri_string(), "0.0.0.0");
        assert_eq!(port, 5000);
        assert_eq!(query, None);

        let (host, port, query) = parse_address_port("[::]:5000").unwrap();
        assert_eq!(host.addr, IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)));
        assert_eq!(host.name, None);
        assert_eq!(host.as_str(), "::");
        assert_eq!(host.uri_string(), "[::]");
        assert_eq!(port, 5000);
        assert_eq!(query, None);

        let (host, port, query) = parse_address_port("127.0.0.1:8080?foo=bar").unwrap();
        assert_eq!(host.addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(host.name, None);
        assert_eq!(host.as_str(), "127.0.0.1");
        assert_eq!(host.uri_string(), "127.0.0.1");
        assert_eq!(port, 8080);
        assert_eq!(query, Some("foo=bar"));

        let (host, port, query) = parse_address_port("localhost:5000").unwrap();
        assert!(host.addr.is_ipv4() || host.addr.is_ipv6());
        assert_eq!(host.name, Some("localhost".to_string()));
        assert_eq!(host.as_str(), "localhost");
        assert_eq!(host.uri_string(), "localhost");
        assert_eq!(port, 5000);
        assert_eq!(query, None);

        let Err(err) = parse_address_port("::1:5000") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_address_port("127.0.0.1") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));
    }

    #[test]
    fn test_parse_client_str() {
        let (host, port) = parse_client_str("127.0.0.1:5000").unwrap();
        assert_eq!(host.addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(host.name, None);
        assert_eq!(port, 5000);

        let (host, port) = parse_client_str("[::1]:5000").unwrap();
        assert_eq!(host.addr, IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)));
        assert_eq!(host.name, None);
        assert_eq!(port, 5000);

        // Hostname should be resolved but original name preserved
        let (host, port) = parse_client_str("localhost:5000").unwrap();
        assert!(host.addr.is_ipv4() || host.addr.is_ipv6());
        assert_eq!(host.name, Some("localhost".to_string()));
        assert_eq!(port, 5000);

        // Port 0 is valid here, e.g. for udpsrc to bind a dynamic port
        let (_, port) = parse_client_str("127.0.0.1:0").unwrap();
        assert_eq!(port, 0);

        // Unbracketed IPv6 is ambiguous and rejected
        let Err(err) = parse_client_str("::1:5000") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        for spec in [
            "",
            "127.0.0.1",
            "[::1",
            "[::1]5000",
            "127.0.0.1:99999",
            "127.0.0.1:x",
        ] {
            let Err(err) = parse_client_str(spec) else {
                unreachable!();
            };
            assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));
        }
    }

    #[test]
    fn test_parse_uri_for_sink() {
        let (host, port) = parse_uri_for_sink("udp://127.0.0.1:5000").unwrap();
        assert_eq!(host.addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(host.name, None);
        assert_eq!(port, 5000);

        let (host, port) = parse_uri_for_sink("udp://[::1]:5000").unwrap();
        assert_eq!(host.addr, IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)));
        assert_eq!(host.name, None);
        assert_eq!(port, 5000);

        // Query params should be ignored for sink
        let (host, port) =
            parse_uri_for_sink("udp://127.0.0.1:5000?source-filter=10.0.0.1").unwrap();
        assert_eq!(host.addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(host.name, None);
        assert_eq!(port, 5000);

        // Hostname should be resolved but original name preserved
        let (host, port) = parse_uri_for_sink("udp://localhost:5000").unwrap();
        assert!(host.addr.is_ipv4() || host.addr.is_ipv6());
        assert_eq!(host.name, Some("localhost".to_string()));
        assert_eq!(port, 5000);
    }

    #[test]
    fn test_parse_uri_for_src() {
        let (host, port, sf, excl) = parse_uri_for_src("udp://0.0.0.0:5000").unwrap();
        assert_eq!(host.addr, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(host.name, None);
        assert_eq!(port, 5000);
        assert!(sf.is_empty());
        assert!(!excl);

        let (host, port, sf, excl) = parse_uri_for_src(
            "udp://0.0.0.0:5000?source-filter=127.0.0.1&source-filter-exclusive=true",
        )
        .unwrap();
        assert_eq!(host.addr, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(host.name, None);
        assert_eq!(port, 5000);
        assert_eq!(sf, vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]);
        assert!(excl);
    }
}
