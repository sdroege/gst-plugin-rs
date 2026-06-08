// GStreamer RTSP Source v2
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtspsrc2
 *
 * `rtspsrc2` is a from-scratch rewrite of the `rtspsrc` element to fix some fundamental
 * architectural issues, with the aim of making the two functionally equivalent.
 *
 * Implemented features:
 * * RTSP 1.0 support
 * * Lower transports: TCP, UDP, UDP-Multicast
 * * RTCP SR and RTCP RR
 * * RTCP-based A/V sync
 * * Lower transport selection and priority (NEW!)
 *   - Also supports different lower transports for each SETUP
 * * Credentials support
 * * TLS/TCP support
 * * HTTP tunnelling
 * * Keep-alive
 * * Support for stream selection at the start
 * * Allow unlinked pads
 * * Support for SRTP with `rtpbin`/`!USE_RTP2`
 * * `GET_PARAMETER` / `SET_PARAMETER`
 *
 * Some missing features:
 * * VOD support: PAUSE, seeking, etc
 * * ONVIF backchannel and trick mode support
 * * and more
 *
 * Please see the [README](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/blob/main/net/rtsp/README.md)
 * for a complete and up-to-date list.
 */
use gst::glib;
use gst::prelude::*;

mod body;
mod digest;
mod http_tunnel;
mod imp;
mod sdp;
mod tcp_message;
mod transport;

#[glib::flags(name = "GstRtspSrc2TlsValidationFlags")]
pub(crate) enum RtspSrc2TlsValidationFlags {
    #[flags_value(
        name = "Signing certificate authority is not known.",
        nick = "unknown-ca"
    )]
    UnknownCA = 1 << 0,

    #[flags_value(
        name = "Certificate does not match the expected identity of the site that it was retrieved from.",
        nick = "bad-identity"
    )]
    BadIdentity = 1 << 1,

    #[flags_value(
        name = "Certificate's activation time is still in the future.",
        nick = "not-activated"
    )]
    NotActivated = 1 << 2,

    #[flags_value(name = "Certificate has expired.", nick = "expired")]
    Expired = 1 << 3,

    #[flags_value(name = "Certificate has been revoked.", nick = "revoked")]
    Revoked = 1 << 4,

    #[flags_value(
        name = "Certificate's algorithm is considered insecure.",
        nick = "insecure"
    )]
    Insecure = 1 << 5,

    #[flags_value(
        name = "Some other error occurred validating the certificate.",
        nick = "generic-error"
    )]
    GenericError = 1 << 6,

    #[flags_value(name = "Combination of all of the above flags.", nick = "validate-all")]
    ValidateAll = 0x00_7f,
}

glib::wrapper! {
    pub struct RtspSrc(ObjectSubclass<imp::RtspSrc>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::URIHandler;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        RtspSrc2TlsValidationFlags::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    gst::Element::register(
        Some(plugin),
        "rtspsrc2",
        gst::Rank::NONE,
        RtspSrc::static_type(),
    )
}
