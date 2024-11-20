// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use serial_test::serial;

use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsinter::plugin_register_static().unwrap();
    });
}

fn start_consumer(producer_name: &str) -> gst_check::Harness {
    let mut hc = gst_check::Harness::new("intersrc");

    hc.element()
        .unwrap()
        .set_property("producer-name", producer_name);
    hc.play();

    hc
}

fn start_producer(producer_name: &str) -> (gst::Pad, gst::Element) {
    let element = gst::ElementFactory::make("intersink")
        .property("producer-name", producer_name)
        .build()
        .unwrap();

    element.set_state(gst::State::Playing).unwrap();

    let sinkpad = element.static_pad("sink").unwrap();
    let srcpad = gst::Pad::new(gst::PadDirection::Src);
    srcpad.set_active(true).unwrap();
    srcpad.link(&sinkpad).unwrap();

    srcpad.push_event(gst::event::StreamStart::builder("foo").build());
    srcpad
        .push_event(gst::event::Caps::builder(&gst::Caps::builder("video/x-raw").build()).build());
    srcpad.push_event(
        gst::event::Segment::builder(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
    );

    (srcpad, element)
}

fn push_one(srcpad: &gst::Pad, pts: gst::ClockTime) {
    let mut inbuf = gst::Buffer::with_size(1).unwrap();

    {
        let buf = inbuf.get_mut().unwrap();
        buf.set_pts(pts);
    }

    srcpad.push(inbuf).unwrap();
}

#[test]
#[serial]
fn test_forward_one_buffer() {
    init();

    let mut hc = start_consumer("p1");
    let (srcpad, element) = start_producer("p1");

    push_one(&srcpad, gst::ClockTime::from_nseconds(1));

    let outbuf = hc.pull().unwrap();

    assert_eq!(outbuf.pts(), Some(gst::ClockTime::from_nseconds(1)));

    element.set_state(gst::State::Null).unwrap();
}

#[test]
#[serial]
fn test_change_name_of_producer() {
    init();

    let mut hc1 = start_consumer("p1");
    let mut hc2 = start_consumer("p2");
    let (srcpad, element) = start_producer("p1");

    /* Once this returns, the buffer should have been dispatched only to hc1 */
    push_one(&srcpad, gst::ClockTime::from_nseconds(1));
    let outbuf = hc1.pull().unwrap();
    assert_eq!(outbuf.pts(), Some(gst::ClockTime::from_nseconds(1)));

    element.set_property("producer-name", "p2");

    /* This should only get dispatched to hc2, and it should be its first buffer */
    push_one(&srcpad, gst::ClockTime::from_nseconds(2));
    let outbuf = hc2.pull().unwrap();
    assert_eq!(outbuf.pts(), Some(gst::ClockTime::from_nseconds(2)));

    element.set_property("producer-name", "p1");

    /* Back to hc1, which should not see the buffer we pushed in the previous step */
    push_one(&srcpad, gst::ClockTime::from_nseconds(3));
    let outbuf = hc1.pull().unwrap();
    assert_eq!(outbuf.pts(), Some(gst::ClockTime::from_nseconds(3)));

    element.set_state(gst::State::Null).unwrap();
}

#[test]
#[serial]
fn test_change_producer_name() {
    init();

    let mut hc = start_consumer("p1");
    let (srcpad1, element1) = start_producer("p1");
    let (srcpad2, element2) = start_producer("p2");

    /* This buffer should be dispatched to no consumer */
    push_one(&srcpad2, gst::ClockTime::from_nseconds(1));

    /* This one should be dispatched to hc, and it should be its first buffer */
    push_one(&srcpad1, gst::ClockTime::from_nseconds(2));
    let outbuf = hc.pull().unwrap();
    assert_eq!(outbuf.pts(), Some(gst::ClockTime::from_nseconds(2)));

    hc.element().unwrap().set_property("producer-name", "p2");

    /* This buffer should be dispatched to no consumer */
    push_one(&srcpad1, gst::ClockTime::from_nseconds(3));

    /* This one should be dispatched to hc, and it should be its next buffer */
    push_one(&srcpad2, gst::ClockTime::from_nseconds(4));
    let outbuf = hc.pull().unwrap();
    assert_eq!(outbuf.pts(), Some(gst::ClockTime::from_nseconds(4)));

    element1.set_state(gst::State::Null).unwrap();
    element2.set_state(gst::State::Null).unwrap();
}

#[test]
#[serial]
fn test_event_forwarding() {
    init();

    let mut hc = start_consumer("p");
    let (srcpad, intersink) = start_producer("p");

    intersink.set_state(gst::State::Null).unwrap();
    intersink.set_property(
        "event-types",
        gst::Array::new(vec![gst::EventType::Eos, gst::EventType::CustomDownstream]),
    );
    intersink.set_state(gst::State::Playing).unwrap();

    // FYI necessary push b/c:
    // https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/merge_requests/1875#note_2630453
    push_one(&srcpad, gst::ClockTime::from_nseconds(1));

    let s = gst::Structure::builder("MyEvent")
        .field("unsigned", 100u64)
        .build();
    assert!(srcpad.push_event(gst::event::CustomDownstream::new(s)));
    assert!(srcpad.push_event(gst::event::Eos::new()));

    let mut found = false;
    loop {
        use gst::EventView;

        if let Ok(event) = hc.pull_event() {
            match event.view() {
                EventView::CustomDownstream(e) => {
                    let v = e.structure().unwrap().get::<u64>("unsigned");
                    assert!(v.is_ok_and(|v| v == 100u64));
                    found = true;
                    break;
                }
                EventView::Eos(..) => {
                    break;
                }
                _ => (),
            };
        }
    }
    intersink.set_state(gst::State::Null).unwrap();
    assert!(found);
}
