// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
use gst::glib;

pub(crate) static DEFAULT_SERVER_NAME: &str = "localhost";
pub(crate) static DEFAULT_ADDR: &str = "127.0.0.1";
pub(crate) static DEFAULT_PORT: u16 = 5000;
pub(crate) static DEFAULT_BIND_ADDR: &str = "0.0.0.0";
pub(crate) static DEFAULT_BIND_PORT: u16 = 0;

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
