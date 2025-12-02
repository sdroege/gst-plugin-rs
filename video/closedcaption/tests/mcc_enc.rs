// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().expect("mccenc test");
    });
}

/// Encode a single ST-2038 packet and compare the output
#[test]
fn test_encode() {
    init();

    // First packet from the output of
    // filesrc location=captions-test_708.mcc ! mccparse ! fakesink dump=1 silent=false -v
    let input = [
        0x00, 0x3f, 0xff, 0xfe, 0x61, 0x80, 0x65, 0x26, 0x59, 0x69, 0x94, 0xa4, 0xf9, 0x9d, 0x00,
        0x40, 0x17, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xf8, 0x09, 0x22,
        0xbf, 0xa8, 0xc7, 0xfd, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00,
        0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00,
        0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00,
        0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00,
        0x7e, 0x90, 0x04, 0x02, 0x73, 0xa4, 0x58, 0x15, 0x96, 0x6e, 0x99, 0xd8, 0x19, 0xfd, 0xff,
        0x5d, 0x10, 0x04, 0x02, 0x1c, 0xad, 0x3f,
    ];

    let expected_output = format!("File Format=MacCaption_MCC V1.0\r\n\
\r\n\
///////////////////////////////////////////////////////////////////////////////////\r\n\
// Computer Prompting and Captioning Company\r\n\
// Ancillary Data Packet Transfer File\r\n\
//\r\n\
// Permission to generate this format is granted provided that\r\n\
//   1. This ANC Transfer file format is used on an as-is basis and no warranty is given, and\r\n\
//   2. This entire descriptive information text is included in a generated .mcc file.\r\n\
//\r\n\
// General file format:\r\n\
//   HH:MM:SS:FF(tab)[Hexadecimal ANC data in groups of 2 characters]\r\n\
//     Hexadecimal data starts with the Ancillary Data Packet DID (Data ID defined in S291M)\r\n\
//       and concludes with the Check Sum following the User Data Words.\r\n\
//     Each time code line must contain at most one complete ancillary data packet.\r\n\
//     To transfer additional ANC Data successive lines may contain identical time code.\r\n\
//     Time Code Rate=[24, 25, 30, 30DF, 50, 60]\r\n\
//\r\n\
//   ANC data bytes may be represented by one ASCII character according to the following schema:\r\n\
//     G  FAh 00h 00h\r\n\
//     H  2 x (FAh 00h 00h)\r\n\
//     I  3 x (FAh 00h 00h)\r\n\
//     J  4 x (FAh 00h 00h)\r\n\
//     K  5 x (FAh 00h 00h)\r\n\
//     L  6 x (FAh 00h 00h)\r\n\
//     M  7 x (FAh 00h 00h)\r\n\
//     N  8 x (FAh 00h 00h)\r\n\
//     O  9 x (FAh 00h 00h)\r\n\
//     P  FBh 80h 80h\r\n\
//     Q  FCh 80h 80h\r\n\
//     R  FDh 80h 80h\r\n\
//     S  96h 69h\r\n\
//     T  61h 01h\r\n\
//     U  E1h 00h 00h 00h\r\n\
//     Z  00h\r\n\
//\r\n\
///////////////////////////////////////////////////////////////////////////////////\r\n\
\r\n\
UUID=14720C04-857D-40E2-86FC-F080DE44CE74\r\n\
Creation Program=GStreamer MCC Encoder {}\r\n\
Creation Date=Thursday, December 27, 2018\r\n\
Creation Time=17:34:47\r\n\
Time Code Rate=30DF\r\n\
\r\n\
11:12:13;14	T52S524F67ZZ72F4QRFF0222FE8CFFOM739181656E67817FFF74ZZ1CB4\r\n", env!("CARGO_PKG_VERSION"));

    let mut h = gst_check::Harness::new("mccenc");
    {
        let enc = h.element().expect("could not create encoder");
        enc.set_property("uuid", "14720C04-857D-40E2-86FC-F080DE44CE74");
        enc.set_property(
            "creation-date",
            glib::DateTime::from_utc(2018, 12, 27, 17, 34, 47.0).unwrap(),
        );
    }

    h.set_src_caps_str("meta/x-st-2038, alignment=(string)packet, framerate=(fraction)30000/1001");

    let tc = gst_video::ValidVideoTimeCode::new(
        gst::Fraction::new(30000, 1001),
        None,
        gst_video::VideoTimeCodeFlags::DROP_FRAME,
        11,
        12,
        13,
        14,
        0,
    )
    .unwrap();

    let buf = {
        let mut buf = gst::Buffer::from_mut_slice(Vec::from(input));
        let buf_ref = buf.get_mut().unwrap();
        gst_video::VideoTimeCodeMeta::add(buf_ref, &tc);
        buf_ref.set_pts(gst::ClockTime::ZERO);
        buf
    };

    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    h.push_event(gst::event::Eos::new());

    let buf = h.pull().expect("Couldn't pull buffer");
    let timecode = buf
        .meta::<gst_video::VideoTimeCodeMeta>()
        .expect("No timecode for buffer")
        .tc();
    assert_eq!(timecode, tc);

    let pts = buf.pts().unwrap();
    assert_eq!(pts, gst::ClockTime::ZERO);

    let map = buf.map_readable().expect("Couldn't map buffer readable");
    assert_eq!(
        std::str::from_utf8(map.as_ref()),
        Ok(expected_output.as_str()),
    );
}
