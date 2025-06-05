// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use pretty_assertions::assert_eq;

use cea708_types::tables::*;
use cea708_types::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

fn gen_cc_data(seq: u8, service: u8, codes: &[Code]) -> gst::Buffer {
    assert!(seq < 4);
    assert!(service < 64);

    let fps = Framerate::new(30, 1);
    let mut writer = CCDataWriter::default();
    let mut packet = DTVCCPacket::new(seq);
    let mut service = Service::new(service);
    for c in codes {
        service.push_code(c).unwrap();
    }
    packet.push_service(service).unwrap();
    writer.push_packet(packet);
    let mut data = vec![];
    writer.write(fps, &mut data).unwrap();
    println!("generated {data:x?}");
    let data = data.split_off(2);
    let mut buf = gst::Buffer::from_mut_slice(data);
    {
        let buf = buf.get_mut().unwrap();
        buf.set_pts(0.nseconds());
        buf.set_duration(gst::ClockTime::from_mseconds(400));
    }
    buf
}

fn cc_data_to_cea708_types(cc_data: &[u8]) -> Vec<u8> {
    let mut ret = vec![0; 2];
    ret[0] = 0x80 | 0x40 | ((cc_data.len() / 3) & 0x1f) as u8;
    ret[1] = 0xFF;
    ret.extend(cc_data);
    ret
}

#[test]
fn test_cea708mux_single_buffer_cc_data() {
    init();

    let mut h = gst_check::Harness::with_padnames("cea708mux", Some("sink_0"), Some("src"));
    h.set_src_caps_str("closedcaption/x-cea-708,format=cc_data,framerate=60/1");

    let buf = gen_cc_data(0, 1, &[Code::LatinCapitalA]);
    h.push(buf).unwrap();

    let mut parser = CCDataParser::new();
    let out = h.pull().unwrap();
    let readable = out.map_readable().unwrap();
    let cc_data = cc_data_to_cea708_types(&readable);
    parser.push(&cc_data).unwrap();
    let parsed_packet = parser.pop_packet().unwrap();
    assert_eq!(parsed_packet.sequence_no(), 0);
    let services = parsed_packet.services();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].number(), 1);
    let codes = services[0].codes();
    assert_eq!(codes.len(), 1);
    assert_eq!(codes[0], Code::LatinCapitalA);
}

#[test]
fn test_cea708mux_2pads_cc_data() {
    init();

    let mut h = gst_check::Harness::with_padnames("cea708mux", None, Some("src"));
    let mut sink_0 = gst_check::Harness::with_element(&h.element().unwrap(), Some("sink_0"), None);
    sink_0.set_src_caps_str("closedcaption/x-cea-708,format=cc_data,framerate=60/1");
    let mut sink_1 = gst_check::Harness::with_element(&h.element().unwrap(), Some("sink_1"), None);
    sink_1.set_src_caps_str("closedcaption/x-cea-708,format=cc_data,framerate=60/1");

    let buf = gen_cc_data(0, 1, &[Code::LatinLowerA]);
    sink_0.push(buf).unwrap();

    let buf = gen_cc_data(0, 2, &[Code::LatinCapitalA]);
    sink_1.push(buf).unwrap();

    let mut parser = CCDataParser::new();
    let out = h.pull().unwrap();
    let readable = out.map_readable().unwrap();
    let mut cc_data = vec![0; 2];
    cc_data[0] = 0x80 | 0x40 | ((readable.len() / 3) & 0x1f) as u8;
    cc_data[1] = 0xFF;
    cc_data.extend(readable.iter());
    parser.push(&cc_data).unwrap();
    let parsed_packet = parser.pop_packet().unwrap();
    assert_eq!(parsed_packet.sequence_no(), 0);
    let services = parsed_packet.services();
    assert_eq!(services.len(), 2);
    // TODO: deterministic service ordering?
    if services[0].number() == 1 {
        let codes = services[0].codes();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0], Code::LatinLowerA);
        assert_eq!(services[1].number(), 2);
        let codes = services[1].codes();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0], Code::LatinCapitalA);
    } else if services[0].number() == 2 {
        let codes = services[0].codes();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0], Code::LatinCapitalA);
        assert_eq!(services[1].number(), 1);
        let codes = services[1].codes();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0], Code::LatinLowerA);
    } else {
        unreachable!();
    }
}

#[test]
fn test_cea708mux_inputs_overflow_output() {
    init();

    static CODES: [Code; 40] = [
        Code::LatinLowerA,
        Code::LatinLowerB,
        Code::LatinLowerC,
        Code::LatinLowerD,
        Code::LatinLowerE,
        Code::LatinLowerF,
        Code::LatinLowerG,
        Code::LatinLowerH,
        Code::LatinLowerI,
        Code::LatinLowerJ,
        Code::LatinLowerK,
        Code::LatinLowerL,
        Code::LatinLowerM,
        Code::LatinLowerN,
        Code::LatinLowerO,
        Code::LatinLowerP,
        Code::LatinLowerQ,
        Code::LatinLowerR,
        Code::LatinLowerS,
        Code::LatinLowerT,
        Code::LatinLowerU,
        Code::LatinLowerV,
        Code::LatinLowerW,
        Code::LatinLowerX,
        Code::LatinLowerY,
        Code::LatinLowerZ,
        Code::LatinCapitalA,
        Code::LatinCapitalB,
        Code::LatinCapitalC,
        Code::LatinCapitalD,
        Code::LatinCapitalE,
        Code::LatinCapitalF,
        Code::LatinCapitalG,
        Code::LatinCapitalH,
        Code::LatinCapitalI,
        Code::LatinCapitalJ,
        Code::LatinCapitalK,
        Code::LatinCapitalL,
        Code::LatinCapitalM,
        Code::LatinCapitalN,
    ];

    let mut h = gst_check::Harness::with_padnames("cea708mux", None, Some("src"));
    let mut sinks = (0..10)
        .map(|idx| {
            let mut sink = gst_check::Harness::with_element(
                &h.element().unwrap(),
                Some(&format!("sink_{idx}")),
                None,
            );
            sink.set_src_caps_str("closedcaption/x-cea-708,format=cc_data,framerate=60/1");
            sink
        })
        .collect::<Vec<_>>();

    let eos = gst::event::Eos::new();

    for (i, sink) in sinks.iter_mut().enumerate() {
        let buf = gen_cc_data(0, i as u8 + 1, &CODES[i..i + 31]);
        sink.push(buf).unwrap();
        sink.push_event(eos.clone());
    }

    let mut parser = CCDataParser::new();
    let mut parsed_packet = None;
    while parsed_packet.is_none() {
        let out = h.pull().unwrap();
        let readable = out.map_readable().unwrap();
        let mut cc_data = vec![0; 2];
        cc_data[0] = 0x80 | 0x40 | ((readable.len() / 3) & 0x1f) as u8;
        cc_data[1] = 0xFF;
        cc_data.extend(readable.iter());
        println!("pushed {cc_data:x?}");
        parser.push(&cc_data).unwrap();
        println!("parser: {parser:x?}");
        parsed_packet = parser.pop_packet();
    }
    let parsed_packet = parsed_packet.unwrap();
    println!("parsed: {parsed_packet:?}");
    assert_eq!(parsed_packet.sequence_no(), 0);
    let services = parsed_packet.services();
    assert_eq!(services.len(), 4);
    // TODO: deterministic service ordering?
    for service in services {
        let codes = service.codes();
        println!("service {}: {:?}", service.number(), codes);
        assert!((1..=10).contains(&service.number()));
        let no = service.number() - 1;
        // one of the services will have a length that is 1 byte shorter than others due to size
        // limits of the packet.
        assert!((30..=31).contains(&codes.len()));
        assert_eq!(&codes[..30], &CODES[no as usize..no as usize + 30]);
    }
}
