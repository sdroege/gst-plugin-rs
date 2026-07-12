// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-pcapparse2:
 * @short-description: Extracts payloads from PCAP capture file format.
 *
 * Supported data formats are classical
 * [libpcap file format](https://wiki.wireshark.org/Development/LibpcapFileFormat)
 * and [Next Generation](https://datatracker.ietf.org/doc/html/draft-ietf-opsawg-pcapng).
 *
 * ## Example pipelines
 * |[
 * gst-launch-1.0 filesrc location=rtph264.pcap !
 * pcapparse2 caps=application/x-rtp,media=video,clock-rate=90000,encoding-name=H264 !
 * rtph264depay ! h264parse ! avdec_h264 ! videoconvert ! autovideosink
 * ]| Read from a pcap dump file using filesrc, extract the raw UDP packets,
 * depayload and decode them.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct PcapParse(ObjectSubclass<imp::PcapParse>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "pcapparse2",
        gst::Rank::MARGINAL,
        PcapParse::static_type(),
    )
}
