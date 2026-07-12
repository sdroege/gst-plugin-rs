// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstpcap::plugin_register_static().unwrap();
    });
}

// Legacy PCAP, one Ethernet/IPv4/UDP packet:
//   src_ip=10.0.0.1  dst_ip=10.0.0.2  src_port=1234  dst_port=5000
//   timestamp: ts_sec=100, ts_usec=0
//   payload:   16 bytes (UDP_PAYLOAD)
static TEST_PCAP: &[u8] = include_bytes!("test.pcap");

// Legacy PCAP, one packet with zero-size UDP payload
static TEST_EMPTY_PCAP: &[u8] = include_bytes!("test_empty.pcap");

// Legacy PCAP, two packets at ts_sec={0, 1} (same packet data as TEST_PCAP)
static TEST_LEGACY_TS_PCAP: &[u8] = include_bytes!("test_legacy_ts.pcap");

// PCAPNG, one EPB, same packet data as TEST_PCAP
static TEST_PCAPNG: &[u8] = include_bytes!("test.pcapng");

// PCAPNG, one EPB with zero-size UDP payload
static TEST_EMPTY_PCAPNG: &[u8] = include_bytes!("test_empty.pcapng");

// PCAPNG, two EPBs at timestamps 0 s and 1 s (default µs resolution),
// same packet data as TEST_PCAP.
static TEST_MULTI_PCAPNG: &[u8] = include_bytes!("test_multi.pcapng");

// PCAPNG, two EPBs at raw timestamps 2 s and 3 s (µs resolution), with
// `if_tsoffset = -1` second in the IDB (adjusted: 1 s and 2 s).
static TEST_TS_NEG_OFFSET_PCAPNG: &[u8] = include_bytes!("test_ts_neg_offset.pcapng");

/// PCAPNG, two EPBs at raw timestamps 1 s and 2 s, with
/// `if_tsoffset = +1` second in the IDB (adjusted: 2 s and 3 s).
static TEST_TS_POS_OFFSET_PCAPNG: &[u8] = include_bytes!("test_ts_pos_offset.pcapng");

/// PCAPNG, two EPBs at raw timestamps 100 s and 101 s (µs resolution).
static TEST_EPOCH_TS_PCAPNG: &[u8] = include_bytes!("test_epoch_ts.pcapng");

/// PCAPNG, IDB with `if_tsresol = 3` (millisecond resolution), two EPBs at
/// raw 1500 ms and 2500 ms, i.e. 1.5 s and 2.5 s.
static TEST_MS_RES_PCAPNG: &[u8] = include_bytes!("test_ms_res.pcapng");

/// PCAPNG, one EPB at 1 s followed by a Simple Packet Block (which has no
/// timestamp in the pcapng format).
static TEST_SPB_PCAPNG: &[u8] = include_bytes!("test_spb.pcapng");

// Legacy PCAP with nanosecond-resolution magic (0xa1b23c4d): two packets at
// 0.000000500 s and 1.000002500 s (the ts_usec field holds nanoseconds).
static TEST_NS_PCAP: &[u8] = include_bytes!("test_ns.pcap");

// Big-endian legacy PCAP (magic bytes a1 b2 c3 d4): two packets at
// 0 s and 1 s (µs resolution).
static TEST_BE_PCAP: &[u8] = include_bytes!("test_be.pcap");

// Legacy PCAP with non-monotonic timestamps: second packet (1 s) is
// earlier than the first (2 s).
static TEST_OUT_OF_ORDER_PCAP: &[u8] = include_bytes!("test_out_of_order.pcap");

// Legacy PCAP, one Ethernet/IPv4/TCP packet:
//   src_ip=10.0.0.1  dst_ip=10.0.0.2  src_port=1234  dst_port=5000
//   payload: TCP_PAYLOAD
static TEST_TCP_PCAP: &[u8] = include_bytes!("test_tcp.pcap");

// PCAPNG in a big-endian section with IDB `if_tsoffset = +1` second:
// raw timestamps 1 s and 2 s (µs resolution), adjusted 2 s and 3 s.
static TEST_BE_TS_OFFSET_PCAPNG: &[u8] = include_bytes!("test_be_ts_offset.pcapng");

// PCAPNG, two EPBs with raw µs timestamps of 2^52 and 2^52 + 10^6 ticks
// (i.e. far beyond 2^32 seconds; ~4503.6e9 s and +1 s).
static TEST_TS_HIGH_PCAPNG: &[u8] = include_bytes!("test_ts_high.pcapng");

// Constants matching values in the test files above
const UDP_PAYLOAD: &[u8] = &[
    0x80, 0xe3, 0x7c, 0xca, 0x79, 0xba, 0x09, 0xc0, 0x70, 0x6e, 0x8b, 0x33, 0x05, 0x0a, 0x00, 0xa0,
];
const TCP_PAYLOAD: &[u8] = &[
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
];
const PCAP_CAPS_STR: &str = "raw/x-pcap";

#[test]
fn test_parse_pcap_eth_payload() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(
        out_buf.size(),
        UDP_PAYLOAD.len(),
        "Output buffer should contain only the UDP payload"
    );

    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcap_zerosize() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_EMPTY_PCAP);
    h.push(buf).unwrap();

    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(
        out_buf.is_some(),
        "Should receive a zero-size buffer for zero-size UDP payload"
    );
    let out_buf = out_buf.unwrap();
    assert_eq!(
        out_buf.size(),
        0,
        "Output buffer should be 0 bytes for zero-size UDP payload"
    );
}

#[test]
fn test_parse_pcap_dst_ip_filter() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("dst-ip", "10.0.0.2");
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(out_buf.size(), UDP_PAYLOAD.len());
    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcap_dst_ip_filter_no_match() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("dst-ip", "192.168.1.2");
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();
    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(
        out_buf.is_none(),
        "No output should be produced when filter doesn't match"
    );
}

#[test]
fn test_parse_pcap_src_ip_filter() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("src-ip", "10.0.0.1");
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(out_buf.size(), UDP_PAYLOAD.len());
    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcap_src_ip_filter_no_match() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("src-ip", "192.168.1.1");
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();
    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(
        out_buf.is_none(),
        "No output should be produced when filter doesn't match"
    );
}

#[test]
fn test_parse_pcap_dst_port_filter() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("dst-port", 5000u32);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(out_buf.size(), UDP_PAYLOAD.len());
    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcap_src_port_filter() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("src-port", 1234u32);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(out_buf.size(), UDP_PAYLOAD.len());
    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcap_src_port_filter_no_match() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("src-port", 9999u32);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();
    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(
        out_buf.is_none(),
        "No output should be produced when the source port filter doesn't match"
    );
}

#[test]
fn test_parse_pcap_dst_port_filter_no_match() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("dst-port", 9999u32);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAP);
    h.push(buf).unwrap();
    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(
        out_buf.is_none(),
        "No output should be produced when the destination port filter doesn't match"
    );
}

#[test]
fn test_parse_pcapng_epb() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_PCAPNG);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(
        out_buf.size(),
        UDP_PAYLOAD.len(),
        "Output buffer should contain only the UDP payload from PCAPNG"
    );

    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcapng_empty_payload() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_EMPTY_PCAPNG);
    h.push(buf).unwrap();
    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(out_buf.is_some(), "Should receive a zero-size buffer");
    assert_eq!(out_buf.unwrap().size(), 0);
}

#[test]
fn test_parse_pcapng_ts_neg_offset_idb() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element()
        .unwrap()
        .set_property("ts-offset", gst::ClockTime::NONE);
    h.play();

    // IDB if_tsoffset = -1 second
    // Raw timestamps 2 s and 3 s → adjusted 1 s and 2 s
    let buf = gst::Buffer::from_slice(TEST_TS_NEG_OFFSET_PCAPNG);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(1)),
        "First packet DTS should be the adjusted timestamp"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(2)),
        "Second packet DTS should be the adjusted timestamp"
    );
}

#[test]
fn test_parse_pcapng_ts_pos_offset_idb() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element()
        .unwrap()
        .set_property("ts-offset", gst::ClockTime::NONE);
    h.play();

    // IDB if_tsoffset = +1 second
    // Raw timestamps 1 s and 2 s → adjusted 2 s and 3 s
    let buf = gst::Buffer::from_slice(TEST_TS_POS_OFFSET_PCAPNG);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(2)),
        "First packet DTS should be the adjusted timestamp"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(3)),
        "Second packet DTS should be the adjusted timestamp"
    );
}

#[test]
fn test_parse_pcap_legacy_timestamps() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    // Two packets at ts_sec=0 and ts_sec=1
    let buf = gst::Buffer::from_slice(TEST_LEGACY_TS_PCAP);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(0)),
        "First legacy packet DTS should be 0"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(1)),
        "Second legacy packet DTS should be 1 second"
    );
}

#[test]
fn test_parse_pcapng_segment_start_relative() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    // Two EPBs at 0 s and 1 s; DTS is relative to the first packet, i.e.
    // 0 and 1 s. The segment must start at 0 so the first DTS is inside it.
    let buf = gst::Buffer::from_slice(TEST_MULTI_PCAPNG);
    h.push(buf).unwrap();

    let segment_start = loop {
        let ev = h.pull_event().unwrap();
        if let gst::event::EventView::Segment(seg) = ev.view() {
            let seg = seg.segment().downcast_ref::<gst::ClockTime>().unwrap();
            break seg.start();
        }
    };
    assert_eq!(
        segment_start,
        Some(gst::ClockTime::ZERO),
        "Segment start should be 0 for relative timestamps"
    );

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(0)),
        "First packet DTS should be 0"
    );
    let map1 = out_buf1.map_readable().unwrap();
    assert_eq!(&map1[..], UDP_PAYLOAD, "First EPB payload mismatch");

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(1)),
        "Second packet DTS should be 1 second"
    );
    let map2 = out_buf2.map_readable().unwrap();
    assert_eq!(&map2[..], UDP_PAYLOAD, "Second EPB payload mismatch");
}

#[test]
fn test_parse_pcapng_ts_offset_absolute() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element()
        .unwrap()
        .set_property("ts-offset", gst::ClockTime::NONE);
    h.play();

    // Two EPBs at raw 100 s and 101 s (µs resolution)
    let buf = gst::Buffer::from_slice(TEST_EPOCH_TS_PCAPNG);
    h.push(buf).unwrap();

    // With absolute timestamps the DTS starts at the first packet's
    // timestamp, and so does the segment.
    let segment_start = loop {
        let ev = h.pull_event().unwrap();
        if let gst::event::EventView::Segment(seg) = ev.view() {
            let seg = seg.segment().downcast_ref::<gst::ClockTime>().unwrap();
            break seg.start();
        }
    };
    assert_eq!(
        segment_start,
        Some(gst::ClockTime::from_seconds(100)),
        "Segment start should be the first packet's timestamp"
    );

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(100)),
        "First packet DTS should be its absolute timestamp"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(101)),
        "Second packet DTS should be its absolute timestamp"
    );
}

#[test]
fn test_parse_pcapng_ms_resolution() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    // IDB if_tsresol = 3 → millisecond resolution (1000 units per second).
    // Raw 1500 ms and 2500 ms → 1.5 s and 2.5 s.
    // DTS relative to first packet: 0 and 1 s. If the resolution were
    // wrongly interpreted as µs, the second DTS would be 1 ms instead.
    let buf = gst::Buffer::from_slice(TEST_MS_RES_PCAPNG);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(0)),
        "First packet DTS should be 0"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(1)),
        "Second packet DTS should be 1 second"
    );
}

#[test]
fn test_parse_pcapng_simple_packet_no_dts() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    // EPB at 1 s followed by a Simple Packet Block. SPBs carry no timestamp
    // in the PCAPNG format, so the second buffer must have no DTS (and must
    // not inherit the EPB's timestamp).
    let buf = gst::Buffer::from_slice(TEST_SPB_PCAPNG);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(0)),
        "EPB packet DTS should be 0 (relative to the 1 s base)"
    );
    let map = out_buf1.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        None,
        "Simple Packet Block has no timestamp, DTS must be unset"
    );
    let map = out_buf2.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcapng_ts_offset_property() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element()
        .unwrap()
        .set_property("ts-offset", 500_000_000u64);
    h.play();

    // Two EPBs at 0 s and 1 s
    // With ts-offset=500 ms: DTS = {500 ms, 1.5 s}
    let buf = gst::Buffer::from_slice(TEST_MULTI_PCAPNG);
    h.push(buf).unwrap();

    // Relative timestamps (ts-offset >= 0): segment starts at 0.
    let segment_start = loop {
        let ev = h.pull_event().unwrap();
        if let gst::event::EventView::Segment(seg) = ev.view() {
            let seg = seg.segment().downcast_ref::<gst::ClockTime>().unwrap();
            break seg.start();
        }
    };
    assert_eq!(
        segment_start,
        Some(gst::ClockTime::ZERO),
        "Segment start should be 0 for relative timestamps"
    );

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_mseconds(500)),
        "First packet with ts-offset=0.5s should have DTS=0.5s"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_mseconds(1500)),
        "Second packet with ts-offset=0.5s should have DTS=1.5s"
    );
}

#[test]
fn test_first_buffer_discont_and_pts_unset() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_LEGACY_TS_PCAP);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert!(
        out_buf1.flags().contains(gst::BufferFlags::DISCONT),
        "First buffer after startup must carry the DISCONT flag"
    );
    // The element only ever sets DTS (matching the legacy `pcapparse`
    // element); PTS must stay unset.
    assert!(out_buf1.pts().is_none(), "PTS must not be set (DTS only)");

    let out_buf2 = h.pull().unwrap();
    assert!(
        !out_buf2.flags().contains(gst::BufferFlags::DISCONT),
        "Only the very first buffer may carry DISCONT"
    );
}

#[test]
fn test_parse_pcap_out_of_order_timestamps() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    // Non-monotonic capture: second packet (1 s) is earlier than the first
    // (2 s). The relative DTS delta must clamp to zero instead of underflow.
    let buf = gst::Buffer::from_slice(TEST_OUT_OF_ORDER_PCAP);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(0)),
        "First packet DTS should be 0"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(0)),
        "Out-of-order packet DTS must not underflow, delta clamps to 0"
    );
}

#[test]
fn test_parse_pcap_nanosecond_resolution() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    // Nanosecond-magic legacy PCAP: packet timestamps 0.000000500 s and
    // 1.000002500 s. Misreading the ts_usec field as µs would shift the
    // second DTS by ~2.5 ms instead of 2 µs.
    let buf = gst::Buffer::from_slice(TEST_NS_PCAP);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(out_buf1.dts(), Some(gst::ClockTime::ZERO));

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_nseconds(1_000_002_000)),
        "Second packet DTS should be 1 s + 2000 ns"
    );
}

#[test]
fn test_parse_pcap_big_endian() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    // Big-endian legacy PCAP (swapped magic): packets at 0 s and 1 s.
    let buf = gst::Buffer::from_slice(TEST_BE_PCAP);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(out_buf1.dts(), Some(gst::ClockTime::ZERO));

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_seconds(1)),
        "Big-endian record timestamps must be byte-swapped correctly"
    );
}

#[test]
fn test_parse_pcapng_big_endian_tsoffset() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element()
        .unwrap()
        .set_property("ts-offset", gst::ClockTime::NONE);
    h.play();

    // Big-endian PCAPNG section with IDB `if_tsoffset = +1 s`:
    // raw 1 s / 2 s (µs resolution) → adjusted 2 s / 3 s.
    let buf = gst::Buffer::from_slice(TEST_BE_TS_OFFSET_PCAPNG);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_seconds(2)),
        "Big-endian section if_tsoffset must be decoded big-endian"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(out_buf2.dts(), Some(gst::ClockTime::from_seconds(3)));
}

#[test]
fn test_parse_pcapng_timestamps_beyond_32_bit_seconds() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element()
        .unwrap()
        .set_property("ts-offset", gst::ClockTime::NONE);
    h.play();

    // Raw µs timestamps of 2^52 and 2^52 + 10^6 ticks (~4503.6e9 s). The
    // seconds part is far beyond 32 bits, so it must not be truncated.
    // 1 µs = 1000 ns, so DTS(ns) = raw * 1000.
    let dts1 = (1u64 << 52) * 1000;
    let buf = gst::Buffer::from_slice(TEST_TS_HIGH_PCAPNG);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    assert_eq!(
        out_buf1.dts(),
        Some(gst::ClockTime::from_nseconds(dts1)),
        "Large PCAPNG timestamps must not be truncated to 32-bit seconds"
    );

    let out_buf2 = h.pull().unwrap();
    assert_eq!(
        out_buf2.dts(),
        Some(gst::ClockTime::from_nseconds(dts1 + 1_000_000_000)),
        "Second large timestamp must be exactly 1 s later"
    );
}

#[test]
fn test_parse_pcap_tcp_payload() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let buf = gst::Buffer::from_slice(TEST_TCP_PCAP);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(
        out_buf.size(),
        TCP_PAYLOAD.len(),
        "Output buffer should contain only the TCP payload"
    );

    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], TCP_PAYLOAD);
}
