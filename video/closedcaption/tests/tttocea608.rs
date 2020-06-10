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
use gst::EventView;

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

    let mut h = gst_check::Harness::new_parse("tttocea608 mode=pop-on");
    h.set_src_caps_str("text/x-raw");

    let inbuf = gst::Buffer::from_slice(&"Hello");

    assert_eq!(h.push(inbuf), Err(gst::FlowError::Error));
}

/* Check translation of a simple string */
#[test]
fn test_one_timed_buffer_and_eos() {
    init();

    let mut h = gst_check::Harness::new_parse("tttocea608 mode=pop-on");
    h.set_src_caps_str("text/x-raw");

    while h.events_in_queue() != 0 {
        let _event = h.pull_event().unwrap();
    }

    let inbuf = new_timed_buffer(&"Hello", gst::SECOND, gst::SECOND);

    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let expected: [(gst::ClockTime, gst::ClockTime, [u8; 2usize]); 11] = [
        (700_000_000.into(), 33_333_333.into(), [0x94, 0x20]), /* resume_caption_loading */
        (733_333_333.into(), 33_333_334.into(), [0x94, 0x20]), /* control doubled */
        (766_666_667.into(), 33_333_333.into(), [0x94, 0xae]), /* erase_non_displayed_memory */
        (800_000_000.into(), 33_333_333.into(), [0x94, 0xae]), /* control doubled */
        (833_333_333.into(), 33_333_334.into(), [0x94, 0x40]), /* preamble */
        (866_666_667.into(), 33_333_333.into(), [0x94, 0x40]), /* control doubled */
        (900_000_000.into(), 33_333_333.into(), [0xc8, 0xe5]), /* H e */
        (933_333_333.into(), 33_333_334.into(), [0xec, 0xec]), /* l l */
        (966_666_667.into(), 33_333_333.into(), [0xef, 0x80]), /* o, nil */
        (gst::SECOND, 33_333_333.into(), [0x94, 0x2f]),        /* end_of_caption */
        (1_033_333_333.into(), 33_333_334.into(), [0x94, 0x2f]), /* control doubled */
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

    assert_eq!(h.buffers_in_queue(), 0);

    h.push_event(gst::Event::new_eos().build());

    /* Check that we do receive an erase_display */
    assert_eq!(h.buffers_in_queue(), 2);
    while h.buffers_in_queue() > 0 {
        let outbuf = h.try_pull().unwrap();
        let data = outbuf.map_readable().unwrap();
        assert_eq!(&*data, &[0x94, 0x2c]);
    }

    assert_eq!(h.events_in_queue() >= 1, true);

    /* Gap event, we ignore those here and test them separately */
    while h.events_in_queue() > 1 {
        let _event = h.pull_event().unwrap();
    }

    let event = h.pull_event().unwrap();
    assert_eq!(event.get_type(), gst::EventType::Eos);
}

/* Here we test that the erase_display_memory control code
 * gets inserted at the correct moment, when there's enough
 * of an interval between two buffers
 */
#[test]
fn test_erase_display_memory_non_spliced() {
    init();

    let mut h = gst_check::Harness::new_parse("tttocea608 mode=pop-on");
    h.set_src_caps_str("text/x-raw");

    while h.events_in_queue() != 0 {
        let _event = h.pull_event().unwrap();
    }

    let inbuf = new_timed_buffer(&"Hello", 1_000_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let inbuf = new_timed_buffer(&"World", 3_000_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let mut erase_display_buffers = 0;

    while h.buffers_in_queue() > 0 {
        let outbuf = h.pull().unwrap();

        if outbuf.get_pts() == 2_000_000_000.into() || outbuf.get_pts() == 2_033_333_333.into() {
            let data = outbuf.map_readable().unwrap();
            assert_eq!(&*data, &[0x94, 0x2c]);
            erase_display_buffers += 1;
        }
    }

    assert_eq!(erase_display_buffers, 2);
}

/* Here we test that the erase_display_memory control code
 * gets spliced in with the byte pairs of the following buffer
 * when there's not enough of an interval between them.
 */
#[test]
fn test_erase_display_memory_spliced() {
    init();

    let mut h = gst_check::Harness::new_parse("tttocea608 mode=pop-on");
    h.set_src_caps_str("text/x-raw");

    while h.events_in_queue() != 0 {
        let _event = h.pull_event().unwrap();
    }

    let inbuf = new_timed_buffer(&"Hello", 1_000_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let inbuf = new_timed_buffer(&"World", 2_200_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let mut erase_display_buffers = 0;
    let mut prev_pts: gst::ClockTime = 0.into();

    while h.buffers_in_queue() > 0 {
        let outbuf = h.pull().unwrap();

        /* Check that our timestamps are strictly ascending */
        assert!(outbuf.get_pts() > prev_pts);

        if outbuf.get_pts() == 2_000_000_000.into() || outbuf.get_pts() == 2_033_333_333.into() {
            let data = outbuf.map_readable().unwrap();
            assert_eq!(&*data, &[0x94, 0x2c]);
            erase_display_buffers += 1;
        }

        prev_pts = outbuf.get_pts();
    }

    assert_eq!(erase_display_buffers, 2);
}

/* Here we test that the erase_display_memory control code
 * gets output "in time" when we receive gaps
 */
#[test]
fn test_erase_display_memory_gaps() {
    init();

    let mut h = gst_check::Harness::new_parse("tttocea608 mode=pop-on");
    h.set_src_caps_str("text/x-raw");

    while h.events_in_queue() != 0 {
        let _event = h.pull_event().unwrap();
    }

    let inbuf = new_timed_buffer(&"Hello", 1_000_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    /* Let's first push a gap that doesn't leave room for our two control codes */
    let gap_event = gst::Event::new_gap(2 * gst::SECOND, 2_533_333_333.into()).build();
    assert_eq!(h.push_event(gap_event), true);
    let mut erase_display_buffers = 0;

    while h.buffers_in_queue() > 0 {
        let outbuf = h.pull().unwrap();

        let data = outbuf.map_readable().unwrap();
        if *data == [0x94, 0x2c] {
            erase_display_buffers += 1;
        }
    }

    assert_eq!(erase_display_buffers, 0);

    let gap_event = gst::Event::new_gap(4_533_333_333.into(), 1.into()).build();
    assert_eq!(h.push_event(gap_event), true);

    while h.buffers_in_queue() > 0 {
        let outbuf = h.pull().unwrap();

        let data = outbuf.map_readable().unwrap();
        if *data == [0x94, 0x2c] {
            erase_display_buffers += 1;
        }
    }

    assert_eq!(erase_display_buffers, 2);
}

/* Here we verify that the element outputs a continuous stream
 * with gap events
 */
#[test]
fn test_output_gaps() {
    init();

    let mut h = gst_check::Harness::new_parse("tttocea608 mode=pop-on");
    h.set_src_caps_str("text/x-raw");

    while h.events_in_queue() != 0 {
        let _event = h.pull_event().unwrap();
    }

    let inbuf = new_timed_buffer(&"Hello", 1_000_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let inbuf = new_timed_buffer(&"World", 3_000_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    assert_eq!(h.events_in_queue(), 3);

    /* One gap from the start of the segment to the first
     * buffer, another from the end_of_caption control code for
     * the first buffer to its erase_display control code,
     * then one gap from erase_display to the beginning
     * of the second buffer
     */
    let expected: [(gst::ClockTime, gst::ClockTime); 3] = [
        (0.into(), 700_000_000.into()),
        (1_066_666_667.into(), 933_333_333.into()),
        (2_066_666_667.into(), 633_333_333.into()),
    ];

    for e in &expected {
        let event = h.pull_event().unwrap();

        assert_eq!(event.get_type(), gst::EventType::Gap);

        if let EventView::Gap(ev) = event.view() {
            let (timestamp, duration) = ev.get();
            assert_eq!(e.0, timestamp);
            assert_eq!(e.1, duration);
        }
    }
}

#[test]
fn test_one_timed_buffer_and_eos_roll_up2() {
    init();

    let mut h = gst_check::Harness::new_parse("tttocea608 mode=roll-up2");
    h.set_src_caps_str("text/x-raw");

    while h.events_in_queue() != 0 {
        let _event = h.pull_event().unwrap();
    }

    let inbuf = new_timed_buffer(&"Hello", gst::SECOND, gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let inbuf = new_timed_buffer(&"World", gst::SECOND, 1.into());
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let expected: [(gst::ClockTime, gst::ClockTime, [u8; 2usize]); 12] = [
        (1_000_000_000.into(), 33_333_333.into(), [0x94, 0x2c]), /* erase_display_memory */
        (1_033_333_333.into(), 33_333_334.into(), [0x94, 0x2c]), /* control doubled */
        (1_066_666_667.into(), 33_333_333.into(), [0x94, 0x25]), /* roll_up_2 */
        (1_100_000_000.into(), 33_333_333.into(), [0x94, 0x25]), /* control doubled */
        (1_133_333_333.into(), 33_333_334.into(), [0x94, 0xe0]), /* preamble */
        (1_166_666_667.into(), 33_333_333.into(), [0x94, 0xe0]), /* control doubled */
        (1_200_000_000.into(), 33_333_333.into(), [0xc8, 0xe5]), /* H e */
        (1_233_333_333.into(), 33_333_334.into(), [0xec, 0xec]), /* l l */
        (1_266_666_667.into(), 33_333_333.into(), [0xef, 0x80]), /* o, nil */
        (2_000_000_000.into(), 0.into(), [0x20, 0x57]),          /* SPACE, W */
        (2_000_000_000.into(), 0.into(), [0xef, 0xf2]),          /* o, r */
        (2_000_000_000.into(), 0.into(), [0xec, 0x64]),          /* l, d */
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

    assert_eq!(h.buffers_in_queue(), 0);

    h.push_event(gst::Event::new_eos().build());

    let expected_gaps: [(gst::ClockTime, gst::ClockTime); 2] = [
        (0.into(), 1_000_000_000.into()),
        (1_300_000_000.into(), 700_000_000.into()),
    ];

    for e in &expected_gaps {
        let event = h.pull_event().unwrap();

        assert_eq!(event.get_type(), gst::EventType::Gap);

        if let EventView::Gap(ev) = event.view() {
            let (timestamp, duration) = ev.get();
            assert_eq!(e.0, timestamp);
            assert_eq!(e.1, duration);
        }
    }

    assert_eq!(h.events_in_queue(), 1);

    let event = h.pull_event().unwrap();
    assert_eq!(event.get_type(), gst::EventType::Eos);
}
