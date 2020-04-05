// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

#[macro_use]
extern crate pretty_assertions;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

fn new_timed_buffer<T: AsRef<[u8]> + Send + 'static>(
    slice: T,
    timestamp: gst::ClockTime,
    duration: gst::ClockTime,
) -> gst::buffer::Buffer {
    let mut buf = gst::Buffer::from_slice(slice);
    let buf_ref = buf.get_mut().unwrap();
    buf_ref.set_pts(timestamp);
    buf_ref.set_duration(duration);
    buf
}

#[test]
fn test_non_timed_buffer() {
    init();

    let mut h = gst_check::Harness::new("tttocea608");
    h.set_src_caps_str("text/x-raw");

    let inbuf = gst::Buffer::from_slice(&"Hello");

    assert_eq!(h.push(inbuf), Err(gst::FlowError::Error));
}

/* Check translation of a simple string */
#[test]
fn test_one_timed_buffer_and_eos() {
    init();

    let mut h = gst_check::Harness::new("tttocea608");
    h.set_src_caps_str("text/x-raw");

    while h.events_in_queue() != 0 {
        let _event = h.pull_event().unwrap();
    }

    let inbuf = new_timed_buffer(&"Hello", gst::SECOND, gst::SECOND);

    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let expected: [(gst::ClockTime, gst::ClockTime, [u8; 2usize]); 11] = [
        (700_000_003.into(), 33_333_333.into(), [0x94, 0x20]), /* resume_caption_loading */
        (733_333_336.into(), 33_333_333.into(), [0x94, 0x20]), /* control doubled */
        (766_666_669.into(), 33_333_333.into(), [0x94, 0xae]), /* erase_non_displayed_memory */
        (800_000_002.into(), 33_333_333.into(), [0x94, 0xae]), /* control doubled */
        (833_333_335.into(), 33_333_333.into(), [0x94, 0x40]), /* preamble */
        (866_666_668.into(), 33_333_333.into(), [0x94, 0x40]), /* control doubled */
        (900_000_001.into(), 33_333_333.into(), [0xc8, 0xe5]), /* H e */
        (933_333_334.into(), 33_333_333.into(), [0xec, 0xec]), /* l l */
        (966_666_667.into(), 33_333_333.into(), [0xef, 0x80]), /* o, nil */
        (gst::SECOND, 33_333_333.into(), [0x94, 0x2f]),        /* end_of_caption */
        (1_033_333_333.into(), 33_333_333.into(), [0x94, 0x2f]), /* control doubled */
    ];

    for (i, e) in expected.iter().enumerate() {
        let outbuf = h.try_pull().unwrap();

        assert_eq!(
            e.0,
            outbuf.get_pts(),
            "Unexpected PTS for {}th buffer",
            i + 1
        );
        assert_eq!(
            e.1,
            outbuf.get_duration(),
            "Unexpected duration for {}th buffer",
            i + 1
        );

        let data = outbuf.map_readable().unwrap();
        assert_eq!(e.2, &*data);
    }

    h.push_event(gst::Event::new_eos().build());

    assert_eq!(h.events_in_queue(), 1);
    let event = h.pull_event().unwrap();
    assert_eq!(event.get_type(), gst::EventType::Eos);
}
