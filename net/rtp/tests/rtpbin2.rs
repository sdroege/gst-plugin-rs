//
// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::{Arc, Mutex};

use gst::{prelude::*, Caps};
use gst_check::Harness;
use rtp_types::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsrtp::plugin_register_static().expect("rtpbin2 test");
    });
}

const TEST_SSRC: u32 = 0x12345678;
const TEST_PT: u8 = 96;
const TEST_CLOCK_RATE: u32 = 48000;

fn generate_rtp_buffer(seqno: u16, rtpts: u32, payload_len: usize) -> gst::Buffer {
    let payload = vec![4; payload_len];
    let packet = RtpPacketBuilder::new()
        .ssrc(TEST_SSRC)
        .payload_type(TEST_PT)
        .sequence_number(seqno)
        .timestamp(rtpts)
        .payload(&payload);
    let size = packet.calculate_size().unwrap();
    let mut data = vec![0; size];
    packet.write_into(&mut data).unwrap();
    gst::Buffer::from_mut_slice(data)
}

#[test]
fn test_send() {
    init();

    let mut h = Harness::with_padnames("rtpbin2", Some("rtp_send_sink_0"), Some("rtp_send_src_0"));
    h.play();

    let caps = Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("payload", TEST_PT as i32)
        .field("clock-rate", TEST_CLOCK_RATE as i32)
        .field("encoding-name", "custom-test")
        .build();
    h.set_src_caps(caps);

    h.push(generate_rtp_buffer(500, 20, 9)).unwrap();
    h.push(generate_rtp_buffer(501, 30, 11)).unwrap();

    let buffer = h.pull().unwrap();
    let mapped = buffer.map_readable().unwrap();
    let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
    assert_eq!(rtp.sequence_number(), 500);

    let buffer = h.pull().unwrap();
    let mapped = buffer.map_readable().unwrap();
    let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
    assert_eq!(rtp.sequence_number(), 501);

    let stats = h.element().unwrap().property::<gst::Structure>("stats");

    let session_stats = stats.get::<gst::Structure>("0").unwrap();
    let source_stats = session_stats
        .get::<gst::Structure>(TEST_SSRC.to_string())
        .unwrap();
    assert_eq!(source_stats.get::<u32>("ssrc").unwrap(), TEST_SSRC);
    assert_eq!(
        source_stats.get::<u32>("clock-rate").unwrap(),
        TEST_CLOCK_RATE
    );
    assert!(source_stats.get::<bool>("sender").unwrap());
    assert!(source_stats.get::<bool>("local").unwrap());
    assert_eq!(source_stats.get::<u64>("packets-sent").unwrap(), 2);
    assert_eq!(source_stats.get::<u64>("octets-sent").unwrap(), 20);
}

#[test]
fn test_receive() {
    init();

    let h = Arc::new(Mutex::new(Harness::with_padnames(
        "rtpbin2",
        Some("rtp_recv_sink_0"),
        None,
    )));
    let weak_h = Arc::downgrade(&h);
    let mut inner = h.lock().unwrap();
    inner
        .element()
        .unwrap()
        .connect_pad_added(move |_elem, pad| {
            weak_h
                .upgrade()
                .unwrap()
                .lock()
                .unwrap()
                .add_element_src_pad(pad)
        });
    inner.play();

    let caps = Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("payload", TEST_PT as i32)
        .field("clock-rate", TEST_CLOCK_RATE as i32)
        .field("encoding-name", "custom-test")
        .build();
    inner.set_src_caps(caps);

    // Cannot push with harness lock as the 'pad-added' handler needs to add the newly created pad to
    // the harness and needs to also take the harness lock.  Workaround by pushing from the
    // internal harness pad directly.
    let push_pad = inner
        .element()
        .unwrap()
        .static_pad("rtp_recv_sink_0")
        .unwrap()
        .peer()
        .unwrap();
    drop(inner);
    push_pad.push(generate_rtp_buffer(500, 20, 9)).unwrap();
    push_pad.push(generate_rtp_buffer(501, 30, 11)).unwrap();
    let mut inner = h.lock().unwrap();

    let buffer = inner.pull().unwrap();
    let mapped = buffer.map_readable().unwrap();
    let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
    assert_eq!(rtp.sequence_number(), 500);

    let buffer = inner.pull().unwrap();
    let mapped = buffer.map_readable().unwrap();
    let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
    assert_eq!(rtp.sequence_number(), 501);

    let stats = inner.element().unwrap().property::<gst::Structure>("stats");

    let session_stats = stats.get::<gst::Structure>("0").unwrap();
    let source_stats = session_stats
        .get::<gst::Structure>(TEST_SSRC.to_string())
        .unwrap();
    let jitterbuffers_stats = session_stats
        .get::<gst::List>("jitterbuffer-stats")
        .unwrap();
    assert_eq!(jitterbuffers_stats.len(), 1);
    let jitterbuffer_stats = jitterbuffers_stats
        .first()
        .unwrap()
        .get::<gst::Structure>()
        .unwrap();
    assert_eq!(source_stats.get::<u32>("ssrc").unwrap(), TEST_SSRC);
    assert_eq!(
        source_stats.get::<u32>("clock-rate").unwrap(),
        TEST_CLOCK_RATE
    );
    assert!(source_stats.get::<bool>("sender").unwrap());
    assert!(!source_stats.get::<bool>("local").unwrap());
    assert_eq!(source_stats.get::<u64>("packets-received").unwrap(), 2);
    assert_eq!(source_stats.get::<u64>("octets-received").unwrap(), 20);
    assert_eq!(jitterbuffer_stats.get::<u64>("num-late").unwrap(), 0);
    assert_eq!(jitterbuffer_stats.get::<u64>("num-lost").unwrap(), 0);
    assert_eq!(jitterbuffer_stats.get::<u64>("num-duplicates").unwrap(), 0);
    assert_eq!(jitterbuffer_stats.get::<u64>("num-pushed").unwrap(), 2);
    assert_eq!(jitterbuffer_stats.get::<i32>("pt").unwrap(), TEST_PT as i32);
    assert_eq!(
        jitterbuffer_stats.get::<i32>("ssrc").unwrap(),
        TEST_SSRC as i32
    );
}
