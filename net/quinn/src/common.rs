// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
use gst::glib;
use quinn::VarInt;

pub(crate) static DEFAULT_SERVER_NAME: &str = "localhost";
pub(crate) static DEFAULT_ADDR: &str = "127.0.0.1";
pub(crate) static DEFAULT_PORT: u16 = 5000;
pub(crate) static DEFAULT_BIND_ADDR: &str = "0.0.0.0";
pub(crate) static DEFAULT_BIND_PORT: u16 = 0;
pub(crate) static DEFAULT_INITIAL_MTU: u16 = 1200;
pub(crate) static DEFAULT_MINIMUM_MTU: u16 = 1200;
pub(crate) static DEFAULT_UPPER_BOUND_MTU: u16 = 1452;
pub(crate) static DEFAULT_MAX_UPPER_BOUND_MTU: u16 = 65527;
pub(crate) static DEFAULT_UDP_PAYLOAD_SIZE: u16 = 1452;
pub(crate) static DEFAULT_MIN_UDP_PAYLOAD_SIZE: u16 = 1200;
pub(crate) static DEFAULT_MAX_UDP_PAYLOAD_SIZE: u16 = 65527;
pub(crate) static DEFAULT_DROP_BUFFER_FOR_DATAGRAM: bool = false;
pub(crate) static DEFAULT_MAX_CONCURRENT_UNI_STREAMS: VarInt = VarInt::from_u32(32);

/*
 * For QUIC transport parameters
 * <https://datatracker.ietf.org/doc/html/rfc9000#section-7.4>
 *
 * A HTTP client might specify "http/1.1" and/or "h2" or "h3".
 * Other well-known values are listed in the at IANA registry at
 * <https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids>.
 */
pub(crate) const DEFAULT_ALPN: &str = "gst-quinn";
pub(crate) const DEFAULT_TIMEOUT: u32 = 15;
pub(crate) const DEFAULT_SECURE_CONNECTION: bool = true;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstQuinnQuicRole")]
pub enum QuinnQuicRole {
    #[enum_value(name = "Server: Act as QUIC server.", nick = "server")]
    Server,

    #[enum_value(name = "Client: Act as QUIC client.", nick = "client")]
    Client,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct QuinnQuicTransportConfig {
    pub datagram_receive_buffer_size: usize,
    pub datagram_send_buffer_size: usize,
    pub initial_mtu: u16,
    pub max_udp_payload_size: u16,
    pub min_mtu: u16,
    pub upper_bound_mtu: u16,
    pub max_concurrent_uni_streams: VarInt,
    pub send_window: u64,
}

impl Default for QuinnQuicTransportConfig {
    fn default() -> Self {
        // Copied from Quinn::TransportConfig defaults
        const EXPECTED_RTT: u32 = 100; // ms
        const MAX_STREAM_BANDWIDTH: u32 = 12500 * 1000; // bytes/s
                                                        // Window size needed to avoid pipeline
                                                        // stalls
        const STREAM_RWND: u32 = MAX_STREAM_BANDWIDTH / 1000 * EXPECTED_RTT;

        Self {
            datagram_receive_buffer_size: STREAM_RWND as usize,
            datagram_send_buffer_size: 1024 * 1024,
            initial_mtu: DEFAULT_INITIAL_MTU,
            max_udp_payload_size: DEFAULT_MAX_UDP_PAYLOAD_SIZE,
            min_mtu: DEFAULT_MINIMUM_MTU,
            upper_bound_mtu: DEFAULT_UPPER_BOUND_MTU,
            max_concurrent_uni_streams: DEFAULT_MAX_CONCURRENT_UNI_STREAMS,
            send_window: (8 * STREAM_RWND).into(),
        }
    }
}
