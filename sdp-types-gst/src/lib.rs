// Copyright (C) 2024 Nirbheek Chauhan <nirbheek@centricular.com>
// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! SDP <-> GStreamer RTP caps conversion.
//!
//! Supports RTP sessions (`RTP/AVP` / `RTP/AVPF` and the secure profiles) with
//! multiple media streams per session, each media corresponding to one set of
//! `application/x-rtp` caps.
//!
//! Covers the SDP needs of the SAP, RTSP and WebRTC use cases: multiple medias
//! per session, multicast connection data, per-media directions, ICE, DTLS
//! and SRTP attributes, BUNDLE groups and SCTP data channel attributes. SRTP
//! keying material is converted to the caps fields used by the SRTP elements.
//!
//! The crate only models and converts SDPs for now. Offer/answer negotiation,
//! the ICE/DTLS/BUNDLE state machines and the SCTP data channel setup are
//! left to the consumers. SCTP media are not converted to caps, WebRTC
//! attributes without a typed representation (e.g. `a=simulcast`,
//! `a=ssrc-group`, `a=imageattr`, `a=tls-id`, `a=identity`) are kept as
//! unhandled attributes, and RFC 7273 clock source signalling
//! (`a=ts-refclk` / `a=mediaclk`) is not supported yet.

// Based on the SDP handling code of the `net/rtsp` plugin (`rtspsrc2`) and the
// `net/webrtcbin2` plugin, originally in `net/rtsp/src/rtspsrc/sdp.rs` and
// `net/webrtcbin2/src/webrtcsession/sdp.rs`:
//
// - rtspsrc2 provided the SDP media attribute to caps conversion, the
//   static/dynamic RTP payload tables and the SRTP/MIKEY keying material,
// - webrtcbin2 provided the SDP parsing/serialization via `sdp-types`, the
//   attribute handling helpers and the `RtcpFb` type.
//
// The implementation is checked against the C SDP library
// (`gst-plugins-base/gst-libs/gst/sdp/gstsdpmessage.c`) and the C `webrtcbin`
// (`gst-plugins-bad`) to stay compatible with them.

// Getter/setter pairs for the private fields of `Session` and `Media`,
// grouped by getter shape: `Option<T>` fields return `Option<&T>`, string
// fields `Option<&str>`, `Vec<T>` fields a slice, non-`Copy` fields a
// reference and `Copy` fields a value. Setters take the owned field type.
// Non-`Copy` fields also have a `*_mut` variant returning the field
// mutably.
macro_rules! opt_accessors {
    ($($name:ident, $name_mut:ident, $setter:ident : Option<$t:ty>),* $(,)?) => {
        $(
            pub fn $name(&self) -> Option<&$t> {
                self.$name.as_ref()
            }

            pub fn $name_mut(&mut self) -> Option<&mut $t> {
                self.$name.as_mut()
            }

            pub fn $setter(&mut self, value: Option<$t>) {
                self.$name = value;
            }
        )*
    };
}

macro_rules! opt_str_accessors {
    ($($name:ident, $name_mut:ident, $setter:ident),* $(,)?) => {
        $(
            pub fn $name(&self) -> Option<&str> {
                self.$name.as_deref()
            }

            pub fn $name_mut(&mut self) -> Option<&mut str> {
                self.$name.as_deref_mut()
            }

            pub fn $setter(&mut self, value: Option<String>) {
                self.$name = value;
            }
        )*
    };
}

macro_rules! vec_accessors {
    ($($name:ident, $name_mut:ident, $setter:ident : Vec<$t:ty>),* $(,)?) => {
        $(
            pub fn $name(&self) -> &[$t] {
                &self.$name
            }

            pub fn $name_mut(&mut self) -> &mut Vec<$t> {
                &mut self.$name
            }

            pub fn $setter(&mut self, value: Vec<$t>) {
                self.$name = value;
            }
        )*
    };
}

macro_rules! ref_accessors {
    ($($name:ident, $name_mut:ident, $setter:ident : $t:ty),* $(,)?) => {
        $(
            pub fn $name(&self) -> &$t {
                &self.$name
            }

            pub fn $name_mut(&mut self) -> &mut $t {
                &mut self.$name
            }

            pub fn $setter(&mut self, value: $t) {
                self.$name = value;
            }
        )*
    };
}

macro_rules! val_accessors {
    ($($name:ident, $setter:ident : $t:ty),* $(,)?) => {
        $(
            pub fn $name(&self) -> $t {
                self.$name
            }

            pub fn $setter(&mut self, value: $t) {
                self.$name = value;
            }
        )*
    };
}

// Chainable builder methods over the builder's `inner` field.
macro_rules! builder_fns {
    ($($name:ident : $t:ty),* $(,)?) => {
        $(
            pub fn $name(mut self, value: $t) -> Self {
                self.inner.$name = value;
                self
            }
        )*
    };
}

mod attrs;
mod caps;
mod error;
mod media;
mod payload_table;
mod session;

pub use self::{
    attrs::{KeyMgmtExt, RtcpFb},
    caps::{caps_to_media, media_to_caps},
    error::{AttributeParseError, CapsError, MediaError, ParseError},
    media::{Media, MediaBuilder},
    session::{Session, SessionBuilder},
};

use std::sync::LazyLock;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "sdp-types-gst",
        gst::DebugColorFlags::empty(),
        Some("SDP and GStreamer caps conversion"),
    )
});

// Test helpers shared between the submodules.
#[cfg(test)]
mod tests {
    pub(super) fn init() {
        let _ = gst::init();
    }

    pub(super) const SESSION_HEADER: &str = concat!(
        "v=0\r\n",
        "o=alice 1234567890 0 IN IP4 0.0.0.0\r\n",
        "s=-\r\n",
        "i=session information\r\n",
        "u=rtsp://example.com/session\r\n",
        "e=alice@example.com\r\n",
        "p=+49 1234 56789\r\n",
        "c=IN IP4 224.12.4.1/127\r\n",
        "b=AS:1024\r\n",
        "t=0 0\r\n",
        "r=60 30 15\r\n",
        "z=2882880000 -3600\r\n",
        "a=group:BUNDLE 0 1\r\n",
        "a=tool:session\r\n",
    );

    pub(super) const MEDIA_AUDIO: &str = concat!(
        "m=audio 5004/2 RTP/AVP 96\r\n",
        "c=IN IP4 239.255.42.42/16\r\n",
        "a=sendonly\r\n",
        "a=rtcp-mux\r\n",
        "a=rtpmap:96 opus/48000/2\r\n",
        "a=fmtp:96 sprop-stereo=1;usedtx=1\r\n",
        "a=ptime:20\r\n",
        "a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
        "a=extmap:2/sendonly urn:ietf:params:rtp-hdrext:ssrc-audio-level vad=on\r\n",
        "a=rtcp-fb:96 nack\r\n",
        "a=rtcp-fb:* transport-cc\r\n",
        "a=ssrc:1234 cname:foo\r\n",
        "a=rid:1 send\r\n",
        "a=rid:2 recv apt=96\r\n",
        "a=tool:test\r\n",
        "a=x-custom:val\r\n",
    );

    pub(super) const MEDIA_VIDEO: &str = concat!(
        "m=video 7004 RTP/AVP 96\r\n",
        "c=IN IP4 239.255.42.43\r\n",
        "a=rtpmap:96 H264/90000\r\n",
        "a=fmtp:96 profile-level-id=42e01f;packetization-mode=1;level-asymmetry-allowed=1\r\n",
        "a=framesize:96 1280x720\r\n",
        "a=rtcp-fb:96 nack pli\r\n",
        "a=rtcp-fb:96 ccm fir\r\n",
    );
}
