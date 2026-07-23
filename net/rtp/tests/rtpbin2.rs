//
// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, AtomicUsize, Ordering},
    mpsc,
};

use gst::{Caps, prelude::*};
use gst_check::Harness;
use rtcp_types::{RtcpPacketWriter, RtcpPacketWriterExt};
use rtp_types::*;

static ELEMENT_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_element_counter() -> usize {
    ELEMENT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsrtp::plugin_register_static().expect("rtpbin2 test");
    });
}

const TEST_DEFAULT_SSRC: u32 = 0x12345678;
const TEST_PT: u8 = 96;
const TEST_CLOCK_RATE: u32 = 48000;

fn generate_rtp_buffer(ssrc: u32, seqno: u16, rtpts: u32, payload_len: usize) -> gst::Buffer {
    let payload = vec![4; payload_len];
    let packet = RtpPacketBuilder::new()
        .ssrc(ssrc)
        .payload_type(TEST_PT)
        .sequence_number(seqno)
        .timestamp(rtpts)
        .payload(payload.as_slice());
    let size = packet.calculate_size().unwrap();
    let mut data = vec![0; size];
    packet.write_into(&mut data).unwrap();
    gst::Buffer::from_mut_slice(data)
}

#[derive(Debug, Copy, Clone)]
struct PacketInfo {
    seq_no: u16,
    rtp_ts: u32,
    payload_len: usize,
    ssrc: u32,
}

impl PacketInfo {
    fn generate_buffer(&self, dts: Option<gst::ClockTime>) -> gst::Buffer {
        let mut buf = generate_rtp_buffer(self.ssrc, self.seq_no, self.rtp_ts, self.payload_len);
        let buf_mut = buf.make_mut();
        buf_mut.set_dts(dts);
        buf
    }
}

fn send_init() -> gst_check::Harness {
    init();

    let id = next_element_counter();

    let elem = gst::ElementFactory::make("rtpsend")
        .property("rtp-id", id.to_string())
        .build()
        .unwrap();
    let mut h = Harness::with_element(&elem, Some("rtp_sink_0"), Some("rtp_src_0"));
    h.play();

    let caps = Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("payload", TEST_PT as i32)
        .field("clock-rate", TEST_CLOCK_RATE as i32)
        .field("encoding-name", "custom-test")
        .build();
    h.set_src_caps(caps);

    h
}

fn send_push<I>(h: &mut gst_check::Harness, packets: I, buffer_list: bool)
where
    I: IntoIterator<Item = PacketInfo>,
{
    if buffer_list {
        let mut list = gst::BufferList::new();
        let list_mut = list.make_mut();
        for packet in packets {
            list_mut.add(packet.generate_buffer(None));
        }
        let push_pad = h
            .element()
            .unwrap()
            .static_pad("rtp_sink_0")
            .unwrap()
            .peer()
            .unwrap();
        push_pad.push_list(list).unwrap();
    } else {
        for packet in packets {
            h.push(packet.generate_buffer(None)).unwrap();
        }
    }
}

fn send_pull<I>(h: &mut gst_check::Harness, packets: I)
where
    I: IntoIterator<Item = PacketInfo>,
{
    for packet in packets {
        let buffer = h.pull().unwrap();
        let mapped = buffer.map_readable().unwrap();
        let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
        assert_eq!(rtp.sequence_number(), packet.seq_no);
    }
}

fn send_check_stats<I>(h: &mut gst_check::Harness, packets: I)
where
    I: IntoIterator<Item = PacketInfo>,
{
    let mut n_packets = 0;
    let mut n_bytes = 0;
    for packet in packets {
        n_packets += 1;
        n_bytes += packet.payload_len;
    }
    let stats = h.element().unwrap().property::<gst::Structure>("stats");

    let session_stats = stats.get::<gst::Structure>("0").unwrap();
    let source_stats = session_stats
        .get::<gst::Structure>(TEST_DEFAULT_SSRC.to_string())
        .unwrap();
    assert_eq!(source_stats.get::<u32>("ssrc").unwrap(), TEST_DEFAULT_SSRC);
    assert_eq!(
        source_stats.get::<u32>("clock-rate").unwrap(),
        TEST_CLOCK_RATE
    );
    assert!(source_stats.get::<bool>("sender").unwrap());
    assert!(source_stats.get::<bool>("local").unwrap());
    assert_eq!(source_stats.get::<u64>("packets-sent").unwrap(), n_packets);
    assert_eq!(
        source_stats.get::<u64>("octets-sent").unwrap(),
        n_bytes as u64
    );
}

#[test]
fn test_send() {
    init();

    let mut h = send_init();

    send_push(&mut h, PACKETS_TEST_1, false);
    send_pull(&mut h, PACKETS_TEST_1);
    send_check_stats(&mut h, PACKETS_TEST_1);
}

#[test]
fn test_send_list() {
    let mut h = send_init();

    send_push(&mut h, PACKETS_TEST_1, true);
    send_pull(&mut h, PACKETS_TEST_1);
    send_check_stats(&mut h, PACKETS_TEST_1);
}

#[test]
fn test_send_benchmark() {
    init();
    let clock = gst::SystemClock::obtain();
    const N_PACKETS: usize = 2 * 1024 * 1024;
    let mut packets = Vec::with_capacity(N_PACKETS);
    for i in 0..N_PACKETS {
        packets.push(
            PacketInfo {
                seq_no: (i & u16::MAX as usize) as u16,
                rtp_ts: i as u32,
                payload_len: 8,
                ssrc: TEST_DEFAULT_SSRC,
            }
            .generate_buffer(None),
        )
    }

    let mut h = send_init();

    let start = clock.time();

    for packet in packets {
        h.push(packet).unwrap();
    }

    let pushed = clock.time();

    for i in 0..N_PACKETS {
        let buffer = h.pull().unwrap();
        let mapped = buffer.map_readable().unwrap();
        let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
        assert_eq!(rtp.sequence_number(), (i & u16::MAX as usize) as u16);
    }

    let end = clock.time();

    let test_time = end.opt_sub(start);
    let pull_time = end.opt_sub(pushed);
    let push_time = pushed.opt_sub(start);
    println!(
        "test took {} (push {}, pull {})",
        test_time.display(),
        push_time.display(),
        pull_time.display()
    );
}

#[test]
fn test_send_list_benchmark() {
    init();
    let clock = gst::SystemClock::obtain();
    const N_PACKETS: usize = 2 * 1024 * 1024;
    let mut list = gst::BufferList::new();
    let list_mut = list.make_mut();
    for i in 0..N_PACKETS {
        list_mut.add(
            PacketInfo {
                seq_no: (i & u16::MAX as usize) as u16,
                rtp_ts: i as u32,
                payload_len: 8,
                ssrc: TEST_DEFAULT_SSRC,
            }
            .generate_buffer(None),
        );
    }
    let mut h = send_init();

    let push_pad = h
        .element()
        .unwrap()
        .static_pad("rtp_sink_0")
        .unwrap()
        .peer()
        .unwrap();

    let start = clock.time();

    push_pad.push_list(list).unwrap();

    let pushed = clock.time();

    for i in 0..N_PACKETS {
        let buffer = h.pull().unwrap();
        let mapped = buffer.map_readable().unwrap();
        let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
        assert_eq!(rtp.sequence_number(), (i & u16::MAX as usize) as u16);
    }

    let end = clock.time();

    let test_time = end.opt_sub(start);
    let pull_time = end.opt_sub(pushed);
    let push_time = pushed.opt_sub(start);
    println!(
        "test took {} (push {}, pull {})",
        test_time.display(),
        push_time.display(),
        pull_time.display()
    );
}

fn receive_init() -> Arc<Mutex<gst_check::Harness>> {
    receive_init_with_new_srcpad(|h, srcpad| h.add_element_src_pad(srcpad))
}

fn receive_init_with_new_srcpad<F>(new_srcpad: F) -> Arc<Mutex<gst_check::Harness>>
where
    F: Fn(&mut gst_check::Harness, &gst::Pad) + Send + Sync + 'static,
{
    let id = next_element_counter();

    let elem = gst::ElementFactory::make("rtprecv")
        .property("rtp-id", id.to_string())
        .build()
        .unwrap();
    let h = Arc::new(Mutex::new(Harness::with_element(
        &elem,
        Some("rtp_sink_0"),
        None,
    )));
    let weak_h = Arc::downgrade(&h);
    let mut inner = h.lock().unwrap();
    inner
        .element()
        .unwrap()
        .connect_pad_added(move |_elem, pad| {
            let h = weak_h.upgrade().unwrap();
            let mut h = h.lock().unwrap();
            new_srcpad(&mut h, pad);
        });
    inner.play();
    let caps = Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("payload", TEST_PT as i32)
        .field("clock-rate", TEST_CLOCK_RATE as i32)
        .field("encoding-name", "custom-test")
        .build();
    inner.set_src_caps(caps);

    drop(inner);

    h
}

fn receive_push<I>(h: Arc<Mutex<gst_check::Harness>>, packets: I, buffer_list: bool)
where
    I: IntoIterator<Item = PacketInfo>,
{
    let inner = h.lock().unwrap();
    // Cannot push with harness lock as the 'pad-added' handler needs to add the newly created pad to
    // the harness and needs to also take the harness lock.  Workaround by pushing from the
    // internal harness pad directly.
    let push_pad = inner
        .element()
        .unwrap()
        .static_pad("rtp_sink_0")
        .unwrap()
        .peer()
        .unwrap();
    drop(inner);

    if buffer_list {
        let mut list = gst::BufferList::new();
        let list_mut = list.make_mut();
        for packet in packets {
            list_mut.add(packet.generate_buffer(None));
        }
        push_pad.push_list(list).unwrap();
    } else {
        for packet in packets {
            push_pad.push(packet.generate_buffer(None)).unwrap();
        }
    }
}

fn receive_pull<I>(h: Arc<Mutex<gst_check::Harness>>, packets: I)
where
    I: IntoIterator<Item = PacketInfo>,
{
    let mut inner = h.lock().unwrap();
    for packet in packets {
        let buffer = inner.pull().unwrap();
        let mapped = buffer.map_readable().unwrap();
        let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
        assert_eq!(rtp.sequence_number(), packet.seq_no);
    }
}

fn receive_check_stats<I>(h: Arc<Mutex<gst_check::Harness>>, packets: I)
where
    I: IntoIterator<Item = PacketInfo>,
{
    let mut n_packets = 0;
    let mut n_bytes = 0;
    for packet in packets {
        n_packets += 1;
        n_bytes += packet.payload_len;
    }

    let inner = h.lock().unwrap();
    let stats = inner.element().unwrap().property::<gst::Structure>("stats");
    drop(inner);

    let session_stats = stats.get::<gst::Structure>("0").unwrap();
    let source_stats = session_stats
        .get::<gst::Structure>(TEST_DEFAULT_SSRC.to_string())
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
    assert_eq!(source_stats.get::<u32>("ssrc").unwrap(), TEST_DEFAULT_SSRC);
    assert_eq!(
        source_stats.get::<u32>("clock-rate").unwrap(),
        TEST_CLOCK_RATE
    );
    assert!(source_stats.get::<bool>("sender").unwrap());
    assert!(!source_stats.get::<bool>("local").unwrap());
    assert_eq!(
        source_stats.get::<u64>("packets-received").unwrap(),
        n_packets
    );
    assert_eq!(
        source_stats.get::<u64>("octets-received").unwrap(),
        n_bytes as u64
    );
    assert_eq!(jitterbuffer_stats.get::<u64>("num-late").unwrap(), 0);
    assert_eq!(jitterbuffer_stats.get::<u64>("num-lost").unwrap(), 0);
    assert_eq!(jitterbuffer_stats.get::<u64>("num-duplicates").unwrap(), 0);
    assert_eq!(
        jitterbuffer_stats.get::<u64>("num-pushed").unwrap(),
        n_packets
    );
    assert_eq!(jitterbuffer_stats.get::<i32>("pt").unwrap(), TEST_PT as i32);
    assert_eq!(
        jitterbuffer_stats.get::<i32>("ssrc").unwrap(),
        TEST_DEFAULT_SSRC as i32
    );
}

static PACKETS_TEST_1: [PacketInfo; 2] = [
    PacketInfo {
        seq_no: 500,
        rtp_ts: 20,
        payload_len: 13,
        ssrc: TEST_DEFAULT_SSRC,
    },
    PacketInfo {
        seq_no: 501,
        rtp_ts: 30,
        payload_len: 7,
        ssrc: TEST_DEFAULT_SSRC,
    },
];

#[test]
fn test_receive() {
    init();

    let h = receive_init();
    receive_push(h.clone(), PACKETS_TEST_1, false);
    receive_pull(h.clone(), PACKETS_TEST_1);
    receive_check_stats(h, PACKETS_TEST_1);
}

#[test]
fn test_receive_list() {
    init();

    let h = receive_init();
    receive_push(h.clone(), PACKETS_TEST_1, true);
    receive_pull(h.clone(), PACKETS_TEST_1);
    receive_check_stats(h, PACKETS_TEST_1);
}

#[test]
fn test_receive_flush() {
    init();
    let h = receive_init();

    receive_push(h.clone(), PACKETS_TEST_1, false);

    let mut inner = h.lock().unwrap();
    let seqnum = gst::Seqnum::next();
    inner.push_event(gst::event::FlushStart::builder().seqnum(seqnum).build());
    inner.push_event(gst::event::FlushStop::builder(false).seqnum(seqnum).build());

    let event = inner.pull_event().unwrap();
    let gst::EventView::FlushStart(fs) = event.view() else {
        unreachable!();
    };
    assert_eq!(fs.seqnum(), seqnum);
    let event = inner.pull_event().unwrap();
    let gst::EventView::FlushStop(fs) = event.view() else {
        unreachable!();
    };
    assert_eq!(fs.seqnum(), seqnum);

    // No buffers should get pulled
    assert!(inner.try_pull_event().is_none());
}

#[test]
fn test_receive_benchmark() {
    init();
    let clock = gst::SystemClock::obtain();
    //const N_PACKETS: usize = 1024 * 1024;
    const N_PACKETS: usize = 1024;
    let mut packets = Vec::with_capacity(N_PACKETS);
    for i in 0..N_PACKETS {
        packets.push(
            PacketInfo {
                seq_no: (i & u16::MAX as usize) as u16,
                rtp_ts: i as u32,
                payload_len: 8,
                ssrc: TEST_DEFAULT_SSRC,
            }
            .generate_buffer(None),
        )
    }

    let h = receive_init();
    let inner = h.lock().unwrap();
    let push_pad = inner
        .element()
        .unwrap()
        .static_pad("rtp_sink_0")
        .unwrap()
        .peer()
        .unwrap();
    drop(inner);

    let start = clock.time();

    for packet in packets {
        push_pad.push(packet).unwrap();
    }

    let pushed = clock.time();

    let mut inner = h.lock().unwrap();
    for i in 0..N_PACKETS {
        let buffer = inner.pull().unwrap();
        let mapped = buffer.map_readable().unwrap();
        let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
        assert_eq!(rtp.sequence_number(), (i & u16::MAX as usize) as u16);
    }

    let end = clock.time();
    drop(inner);

    let test_time = end.opt_sub(start);
    let pull_time = end.opt_sub(pushed);
    let push_time = pushed.opt_sub(start);
    println!(
        "test took {} (push {}, pull {})",
        test_time.display(),
        push_time.display(),
        pull_time.display()
    );
}

#[derive(Debug)]
enum BufferOrList {
    Buffer(gst::Buffer),
    List(gst::BufferList),
}

#[test]
fn test_receive_list_benchmark() {
    init();
    let clock = gst::SystemClock::obtain();
    const N_PACKETS: usize = 32 * 1024;
    const N_PUSHES: usize = 1024 / 32;

    let (send, recv) = std::sync::mpsc::channel();

    let sinkpad = gst::Pad::builder(gst::PadDirection::Sink)
        .name("sink")
        .chain_list_function({
            let send = send.clone();
            move |_pad, _parent, list| {
                send.send(BufferOrList::List(list)).unwrap();
                Ok(gst::FlowSuccess::Ok)
            }
        })
        .chain_function(move |_pad, _parent, buffer| {
            send.send(BufferOrList::Buffer(buffer)).unwrap();

            Ok(gst::FlowSuccess::Ok)
        })
        .event_function(move |_pad, _parent, _event| true)
        .build();
    sinkpad.set_active(true).unwrap();

    let h = receive_init_with_new_srcpad(move |_h, srcpad| {
        srcpad.link(&sinkpad).unwrap();
    });
    let inner = h.lock().unwrap();

    let push_pad = inner
        .element()
        .unwrap()
        .static_pad("rtp_sink_0")
        .unwrap()
        .peer()
        .unwrap();
    drop(inner);

    let mut lists = Vec::with_capacity(N_PUSHES);
    for p in 0..N_PUSHES {
        let mut list = gst::BufferList::new();
        let list_mut = list.make_mut();
        for i in 0..N_PACKETS {
            list_mut.add(
                PacketInfo {
                    seq_no: ((p * N_PACKETS + i) & u16::MAX as usize) as u16,
                    rtp_ts: (p * N_PACKETS + i) as u32,
                    payload_len: 8,
                    ssrc: TEST_DEFAULT_SSRC,
                }
                .generate_buffer(None),
            );
        }
        lists.push(list);
    }

    let start = clock.time();

    for list in lists {
        push_pad.push_list(list).unwrap();
    }

    let pushed = clock.time();

    let mut idx = 0;
    loop {
        let list = recv.recv().unwrap();
        println!("{idx} received {list:?}");
        match list {
            BufferOrList::List(list) => {
                list.foreach(|buffer, _buf_idx| {
                    let mapped = buffer.map_readable().unwrap();
                    let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
                    assert_eq!(rtp.sequence_number(), (idx & u16::MAX as usize) as u16);
                    idx += 1;
                    std::ops::ControlFlow::Continue(())
                });
            }
            BufferOrList::Buffer(buffer) => {
                let mapped = buffer.map_readable().unwrap();
                let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
                assert_eq!(rtp.sequence_number(), (idx & u16::MAX as usize) as u16);
                idx += 1;
            }
        }
        if idx >= N_PUSHES * N_PACKETS {
            break;
        }
    }

    let end = clock.time();

    let test_time = end.opt_sub(start);
    let pull_time = end.opt_sub(pushed);
    let push_time = pushed.opt_sub(start);
    println!(
        "test took {} (push {}, pull {})",
        test_time.display(),
        push_time.display(),
        pull_time.display()
    );
}

#[test]
fn recv_release_sink_pad() {
    init();

    let id = next_element_counter();

    let clock = gst::SystemClock::obtain();
    let elem = gst::ElementFactory::make("rtprecv")
        .property("rtp-id", id.to_string())
        .build()
        .unwrap();
    elem.set_clock(Some(&clock)).unwrap();
    elem.set_state(gst::State::Playing).unwrap();
    let sinkpad = elem.request_pad_simple("rtp_sink_0").unwrap();
    let stream_start = gst::event::StreamStart::new("random");
    sinkpad.send_event(stream_start);
    let caps = Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("payload", TEST_PT as i32)
        .field("clock-rate", TEST_CLOCK_RATE as i32)
        .field("encoding-name", "custom-test")
        .build();
    sinkpad.send_event(gst::event::Caps::new(&caps));
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    sinkpad.send_event(gst::event::Segment::new(&segment));

    let (sender, recv) = std::sync::mpsc::sync_channel(1);
    elem.connect_pad_added({
        let sender = sender.clone();
        move |_elem, pad| {
            let other_pad = gst::Pad::builder(gst::PadDirection::Sink)
                .chain_function(|_pad, _parent, _buffer| Ok(gst::FlowSuccess::Ok))
                .event_function(move |_pad, _parent, _event| true)
                .build();
            other_pad.set_active(true).unwrap();
            pad.link(&other_pad).unwrap();
            sender.send(other_pad).unwrap();
        }
    });
    // push two buffers to get past the rtpsource validation
    sinkpad
        .chain(
            PacketInfo {
                seq_no: 30,
                rtp_ts: 10,
                payload_len: 4,
                ssrc: TEST_DEFAULT_SSRC,
            }
            .generate_buffer(Some(gst::ClockTime::from_mseconds(50))),
        )
        .unwrap();
    sinkpad
        .chain(
            PacketInfo {
                seq_no: 31,
                rtp_ts: 10,
                payload_len: 4,
                ssrc: TEST_DEFAULT_SSRC,
            }
            .generate_buffer(Some(gst::ClockTime::from_mseconds(100))),
        )
        .unwrap();
    let _other_pad = recv.recv().unwrap();

    elem.release_request_pad(&sinkpad);
    elem.set_state(gst::State::Null).unwrap();
}

#[test]
fn recv_multiple_ssrc_buffer_list() {
    const SSRC_1: u32 = 1;
    const BASE_SEQ_1: u16 = 10;
    const BASE_RTP_TS_1: u32 = 100;
    const SSRC_2: u32 = 2;
    const BASE_SEQ_2: u16 = 20;
    const BASE_RTP_TS_2: u32 = 200;

    const NB_BUFFERS_PER_STREAM: usize = 2;

    fn new_sink_pad(ssrc: u32, base_seq: u16, sender: mpsc::Sender<u16>) -> gst::Pad {
        fn check_packet(ssrc: u32, seq: &AtomicU16, buffer: &gst::BufferRef) -> u16 {
            let mapped = buffer.map_readable().unwrap();
            let rtp = rtp_types::RtpPacket::parse(&mapped).unwrap();
            assert_eq!(
                ssrc,
                rtp.ssrc(),
                "pad for ssrc {ssrc} received packet with ssrc {}",
                rtp.ssrc()
            );
            assert_eq!(seq.fetch_add(1, Ordering::SeqCst), rtp.sequence_number());
            println!(
                "ssrc {ssrc} checked packet with seq num {}",
                rtp.sequence_number()
            );

            rtp.sequence_number()
        }

        let seq_num = Arc::new(AtomicU16::new(base_seq));
        let sinkpad = gst::Pad::builder(gst::PadDirection::Sink)
            .name(format!("sink_{ssrc}"))
            .chain_list_function({
                let seq_num = seq_num.clone();
                let sender = sender.clone();
                move |_pad, _parent, list| {
                    for buffer in list.iter() {
                        sender.send(check_packet(ssrc, &seq_num, buffer)).unwrap();
                    }
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .chain_function({
                let seq_num = seq_num.clone();
                move |_pad, _parent, buffer| {
                    sender.send(check_packet(ssrc, &seq_num, &buffer)).unwrap();
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .event_function(|_pad, _parent, _event| true)
            .build();
        sinkpad.set_active(true).unwrap();

        sinkpad
    }

    init();

    let mut seq_1 = BASE_SEQ_1;
    let mut rtp_ts_1 = BASE_RTP_TS_1;
    let mut seq_2 = BASE_SEQ_2;
    let mut rtp_ts_2 = BASE_RTP_TS_2;

    let (tx, rx) = mpsc::channel();
    let h = receive_init_with_new_srcpad({
        let sinkpad_1 = new_sink_pad(SSRC_1, seq_1, tx.clone());
        let sinkpad_2 = new_sink_pad(SSRC_2, seq_2, tx);

        move |_h, srcpad| {
            let srcpad_name = srcpad.name();
            println!("h sink srcpad {srcpad_name}");
            let _ = match srcpad_name.as_str() {
                "rtp_src_0_96_1" => srcpad.link(&sinkpad_1).unwrap(),
                "rtp_src_0_96_2" => srcpad.link(&sinkpad_2).unwrap(),
                other => unreachable!("{other}"),
            };
        }
    });

    receive_push(
        h.clone(),
        [
            PacketInfo {
                seq_no: seq_1,
                rtp_ts: rtp_ts_1,
                payload_len: 8,
                ssrc: SSRC_1,
            },
            PacketInfo {
                seq_no: seq_2,
                rtp_ts: rtp_ts_2,
                payload_len: 8,
                ssrc: SSRC_2,
            },
        ],
        true,
    );

    seq_1 += 1;
    rtp_ts_1 += 1;
    seq_2 += 1;
    rtp_ts_2 += 1;
    receive_push(
        h.clone(),
        [
            PacketInfo {
                seq_no: seq_1,
                rtp_ts: rtp_ts_1,
                payload_len: 8,
                ssrc: SSRC_1,
            },
            PacketInfo {
                seq_no: seq_2,
                rtp_ts: rtp_ts_2,
                payload_len: 8,
                ssrc: SSRC_2,
            },
        ],
        true,
    );

    let mut buffer_count = 0;
    while let Ok(seq) = rx.recv() {
        println!("main loop got seq {seq}");
        // note: out packet / stream consistency checked by `check_packet()` above
        buffer_count += 1;
        if buffer_count == 2 * NB_BUFFERS_PER_STREAM {
            // done
            return;
        }
    }

    panic!("recv failed");
}

#[test]
// make sure we don't deadlock pushing a buffer list with muxed RTP & RTCP packets
fn push_buffer_list_muxed_rtp_and_rtcp_packets() {
    init();

    let id = next_element_counter();

    let clock = gst::SystemClock::obtain();
    let elem = gst::ElementFactory::make("rtprecv")
        .property("rtp-id", id.to_string())
        .build()
        .unwrap();
    elem.set_clock(Some(&clock)).unwrap();
    elem.set_state(gst::State::Playing).unwrap();
    let sinkpad = elem.request_pad_simple("rtp_sink_0").unwrap();
    let stream_start = gst::event::StreamStart::new("random");
    sinkpad.send_event(stream_start);
    let caps = Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("payload", TEST_PT as i32)
        .field("clock-rate", TEST_CLOCK_RATE as i32)
        .field("encoding-name", "custom-test")
        .build();
    sinkpad.send_event(gst::event::Caps::new(&caps));
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    sinkpad.send_event(gst::event::Segment::new(&segment));

    let rtcp_app = rtcp_types::App::builder(0x1234, "test");
    let mut rtcp_app_buf = gst::Buffer::with_size(rtcp_app.calculate_size().unwrap()).unwrap();
    {
        let rtc_app_buf = rtcp_app_buf.get_mut().unwrap();
        rtcp_app
            .write_into(rtc_app_buf.map_writable().unwrap().as_mut_slice())
            .unwrap();
    }

    let buf_list = gst::BufferList::from([
        PacketInfo {
            seq_no: 30,
            rtp_ts: 10,
            payload_len: 4,
            ssrc: TEST_DEFAULT_SSRC,
        }
        .generate_buffer(Some(gst::ClockTime::from_mseconds(50))),
        rtcp_app_buf,
    ]);

    sinkpad.chain_list(buf_list).unwrap();

    elem.set_state(gst::State::Null).unwrap();
}
