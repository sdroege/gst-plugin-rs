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

use pretty_assertions::assert_eq;

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

    loop {
        let outbuf = h.pull().unwrap();
        if outbuf.get_pts() + outbuf.get_duration() >= gst::SECOND {
            break;
        }

        let data = outbuf.map_readable().unwrap();
        assert_eq!(&*data, &[0x80, 0x80]);
    }

    let expected: [(gst::ClockTime, gst::ClockTime, [u8; 2usize]); 7] = [
        (1_000_000_000.into(), 33_333_333.into(), [0x94, 0x20]), /* resume_caption_loading */
        (1_033_333_333.into(), 33_333_334.into(), [0x94, 0xae]), /* erase_non_displayed_memory */
        (1_066_666_667.into(), 33_333_333.into(), [0x94, 0x70]), /* preamble */
        (1_100_000_000.into(), 33_333_333.into(), [0xc8, 0xe5]), /* H e */
        (1_133_333_333.into(), 33_333_334.into(), [0xec, 0xec]), /* l l */
        (1_166_666_667.into(), 33_333_333.into(), [0xef, 0x80]), /* o, nil */
        (1_200_000_000.into(), 33_333_333.into(), [0x94, 0x2f]), /* end_of_caption */
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

    assert_eq!(h.buffers_in_queue(), 23);

    h.push_event(gst::event::Eos::new());

    /* Check that we do receive an erase_display */
    loop {
        let outbuf = h.try_pull().unwrap();
        let data = outbuf.map_readable().unwrap();
        if outbuf.get_pts() == 2_200_000_000.into() {
            assert_eq!(&*data, &[0x94, 0x2c]);
            break;
        } else {
            assert_eq!(&*data, &[0x80, 0x80]);
        }
    }

    assert_eq!(h.events_in_queue() == 1, true);

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

        if outbuf.get_pts() == 2_200_000_000.into() {
            let data = outbuf.map_readable().unwrap();
            assert_eq!(&*data, &[0x94, 0x2c]);
            erase_display_buffers += 1;
        }
    }

    assert_eq!(erase_display_buffers, 1);
}

/* Here we test that the erase_display_memory control code
 * gets inserted before the following pop-on captions
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

    let inbuf = new_timed_buffer(&"World", 2_000_000_000.into(), gst::SECOND);
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    let mut erase_display_buffers = 0;
    let mut prev_pts: gst::ClockTime = 0.into();

    while h.buffers_in_queue() > 0 {
        let outbuf = h.pull().unwrap();

        /* Check that our timestamps are strictly ascending */
        assert!(outbuf.get_pts() >= prev_pts);

        if outbuf.get_pts() == 2_000_000_000.into() {
            let data = outbuf.map_readable().unwrap();
            assert_eq!(&*data, &[0x94, 0x2c]);
            erase_display_buffers += 1;
        }

        prev_pts = outbuf.get_pts();
    }

    assert_eq!(erase_display_buffers, 1);
}

/* Here we verify that the element outputs a continuous stream
 * with padding buffers
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

    h.push_event(gst::event::Eos::new());

    /* Padding */
    loop {
        let outbuf = h.pull().unwrap();
        if outbuf.get_pts() + outbuf.get_duration() >= gst::SECOND {
            break;
        }

        let data = outbuf.map_readable().unwrap();
        assert_eq!(&*data, &[0x80, 0x80]);
    }

    /* Hello */
    loop {
        let outbuf = h.pull().unwrap();
        if outbuf.get_pts() + outbuf.get_duration() >= 1_233_333_333.into() {
            break;
        }

        let data = outbuf.map_readable().unwrap();
        assert_ne!(&*data, &[0x80, 0x80]);
    }

    /* Padding */
    loop {
        let outbuf = h.pull().unwrap();
        if outbuf.get_pts() + outbuf.get_duration() >= 3_000_000_000.into() {
            break;
        }

        let data = outbuf.map_readable().unwrap();
        if outbuf.get_pts() == 2_200_000_000.into() {
            /* Erase display one second after Hello */
            assert_eq!(&*data, &[0x94, 0x2C]);
        } else {
            assert_eq!(&*data, &[0x80, 0x80]);
        }
    }

    /* World */
    loop {
        let outbuf = h.pull().unwrap();
        if outbuf.get_pts() + outbuf.get_duration() >= 3_233_333_333.into() {
            break;
        }

        let data = outbuf.map_readable().unwrap();
        assert_ne!(&*data, &[0x80, 0x80]);
    }

    assert_eq!(h.events_in_queue(), 1);

    let event = h.pull_event().unwrap();
    assert_eq!(event.get_type(), gst::EventType::Eos);
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

    let inbuf = new_timed_buffer(&"World", 2 * gst::SECOND, 1.into());
    assert_eq!(h.push(inbuf), Ok(gst::FlowSuccess::Ok));

    /* Padding */
    loop {
        let outbuf = h.pull().unwrap();
        if outbuf.get_pts() + outbuf.get_duration() >= gst::SECOND {
            break;
        }

        let data = outbuf.map_readable().unwrap();
        assert_eq!(&*data, &[0x80, 0x80]);
    }

    let expected: [(gst::ClockTime, gst::ClockTime, [u8; 2usize]); 5] = [
        (1_000_000_000.into(), 33_333_333.into(), [0x94, 0x25]), /* roll_up_2 */
        (1_033_333_333.into(), 33_333_334.into(), [0x94, 0x70]), /* preamble */
        (1_066_666_667.into(), 33_333_333.into(), [0xc8, 0xe5]), /* H e */
        (1_100_000_000.into(), 33_333_333.into(), [0xec, 0xec]), /* l l */
        (1_133_333_333.into(), 33_333_334.into(), [0xef, 0x80]), /* o nil */
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

    /* Padding */
    loop {
        let outbuf = h.pull().unwrap();
        if outbuf.get_pts() + outbuf.get_duration() >= 2 * gst::SECOND {
            break;
        }

        let data = outbuf.map_readable().unwrap();
        assert_eq!(&*data, &[0x80, 0x80]);
    }

    let expected: [(gst::ClockTime, gst::ClockTime, [u8; 2usize]); 3] = [
        (2_000_000_000.into(), 0.into(), [0x20, 0x57]), /* SPACE W */
        (2_000_000_000.into(), 0.into(), [0xef, 0xf2]), /* o r */
        (2_000_000_000.into(), 0.into(), [0xec, 0x64]), /* l d */
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

    h.push_event(gst::event::Eos::new());

    assert_eq!(h.events_in_queue(), 1);

    let event = h.pull_event().unwrap();
    assert_eq!(event.get_type(), gst::EventType::Eos);
}
