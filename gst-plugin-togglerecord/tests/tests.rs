// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
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

extern crate glib;
use glib::prelude::*;

extern crate gstreamer as gst;
use gst::prelude::*;

extern crate either;
use either::*;

use std::sync::{mpsc, Mutex};
use std::thread;

extern crate gsttogglerecord;

fn init() {
    use std::sync::{Once, ONCE_INIT};
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        gst::init().unwrap();
        gsttogglerecord::plugin_register_static();
    });
}

enum SendData {
    Buffers(usize),
    BuffersDelta(usize),
    Gaps(usize),
    Eos,
}

fn setup_sender_receiver(
    pipeline: &gst::Pipeline,
    togglerecord: &gst::Element,
    pad: &str,
    offset: gst::ClockTime,
) -> (
    mpsc::Sender<SendData>,
    mpsc::Receiver<()>,
    mpsc::Receiver<Either<gst::Buffer, gst::Event>>,
    thread::JoinHandle<()>,
) {
    let fakesink = gst::ElementFactory::make("fakesink", None).unwrap();
    fakesink.set_property("async", &false).unwrap();
    pipeline.add(&fakesink).unwrap();

    let (srcpad, sinkpad) = if pad == "src" {
        (
            togglerecord.get_static_pad("src").unwrap(),
            togglerecord.get_static_pad("sink").unwrap(),
        )
    } else {
        let sinkpad = togglerecord.get_request_pad("sink_%u").unwrap();
        let srcpad = sinkpad.iterate_internal_links().next().unwrap().unwrap();
        (srcpad, sinkpad)
    };

    let fakesink_sinkpad = fakesink.get_static_pad("sink").unwrap();
    srcpad.link(&fakesink_sinkpad).unwrap();

    let (sender_output, receiver_output) = mpsc::channel::<Either<gst::Buffer, gst::Event>>();
    let sender_output = Mutex::new(sender_output);
    srcpad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        move |_, ref probe_info| {
            match probe_info.data {
                Some(gst::PadProbeData::Buffer(ref buffer)) => {
                    sender_output
                        .lock()
                        .unwrap()
                        .send(Left(buffer.clone()))
                        .unwrap();
                }
                Some(gst::PadProbeData::Event(ref event)) => {
                    sender_output
                        .lock()
                        .unwrap()
                        .send(Right(event.clone()))
                        .unwrap();
                }
                _ => {
                    unreachable!();
                }
            }

            gst::PadProbeReturn::Ok
        },
    );

    let (sender_input, receiver_input) = mpsc::channel::<SendData>();
    let (sender_input_done, receiver_input_done) = mpsc::channel::<()>();
    let thread = thread::spawn(move || {
        let mut i = 0;
        let mut first = true;
        while let Ok(send_data) = receiver_input.recv() {
            if first {
                assert!(sinkpad.send_event(gst::Event::new_stream_start("test").build()));
                let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                assert!(sinkpad.send_event(gst::Event::new_segment(&segment).build()));

                let mut tags = gst::TagList::new();
                tags.get_mut()
                    .unwrap()
                    .add::<gst::tags::Title>(&"some title", gst::TagMergeMode::Append);
                assert!(sinkpad.send_event(gst::Event::new_tag(tags).build()));

                first = false;
            }

            match send_data {
                SendData::Eos => {
                    break;
                }
                SendData::Buffers(n) => {
                    for _ in 0..n {
                        let mut buffer = gst::Buffer::new();
                        buffer
                            .get_mut()
                            .unwrap()
                            .set_pts(offset + i * 20 * gst::MSECOND);
                        buffer.get_mut().unwrap().set_duration(20 * gst::MSECOND);
                        let _ = sinkpad.chain(buffer);
                        i += 1;
                    }
                }
                SendData::BuffersDelta(n) => {
                    for _ in 0..n {
                        let mut buffer = gst::Buffer::new();
                        buffer
                            .get_mut()
                            .unwrap()
                            .set_pts(offset + i * 20 * gst::MSECOND);
                        buffer.get_mut().unwrap().set_duration(20 * gst::MSECOND);
                        buffer
                            .get_mut()
                            .unwrap()
                            .set_flags(gst::BufferFlags::DELTA_UNIT);
                        let _ = sinkpad.chain(buffer);
                        i += 1;
                    }
                }
                SendData::Gaps(n) => {
                    for _ in 0..n {
                        let event =
                            gst::Event::new_gap(offset + i * 20 * gst::MSECOND, 20 * gst::MSECOND)
                                .build();
                        let _ = sinkpad.send_event(event);
                        i += 1;
                    }
                }
            }

            let _ = sender_input_done.send(());
        }

        let _ = sinkpad.send_event(gst::Event::new_eos().build());
        let _ = sender_input_done.send(());
    });

    (sender_input, receiver_input_done, receiver_output, thread)
}

fn recv_buffers(
    receiver_output: &mpsc::Receiver<Either<gst::Buffer, gst::Event>>,
    segment: &mut gst::FormattedSegment<gst::ClockTime>,
    wait_buffers: usize,
) -> Vec<(gst::ClockTime, gst::ClockTime)> {
    let mut res = Vec::new();
    let mut n_buffers = 0;
    while let Ok(val) = receiver_output.recv() {
        match val {
            Left(buffer) => {
                res.push((segment.to_running_time(buffer.get_pts()), buffer.get_pts()));
                n_buffers += 1;
                if wait_buffers > 0 && n_buffers == wait_buffers {
                    return res;
                }
            }
            Right(event) => {
                use gst::EventView;

                match event.view() {
                    EventView::Gap(ref e) => {
                        let (ts, _) = e.get();

                        res.push((segment.to_running_time(ts), ts));
                        n_buffers += 1;
                        if wait_buffers > 0 && n_buffers == wait_buffers {
                            return res;
                        }
                    }
                    EventView::Eos(..) => {
                        return res;
                    }
                    EventView::Segment(ref e) => {
                        *segment = e.get_segment().clone().downcast().unwrap();
                    }
                    _ => (),
                }
            }
        }
    }

    res
}

#[test]
fn test_create() {
    init();
    assert!(gst::ElementFactory::make("togglerecord", None).is_some());
}

#[test]
fn test_create_pads() {
    init();
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();

    let sinkpad = togglerecord.get_request_pad("sink_%u").unwrap();
    let srcpad = sinkpad.iterate_internal_links().next().unwrap().unwrap();

    assert_eq!(sinkpad.get_name(), "sink_0");
    assert_eq!(srcpad.get_name(), "src_0");

    togglerecord.release_request_pad(&sinkpad);
    assert!(sinkpad.get_parent().is_none());
    assert!(srcpad.get_parent().is_none());
}

#[test]
fn test_one_stream_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input, _, receiver_output, thread) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();
    sender_input.send(SendData::Buffers(10)).unwrap();
    drop(sender_input);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers = recv_buffers(&receiver_output, &mut segment, 0);
    assert_eq!(buffers.len(), 10);
    for (index, &(running_time, pts)) in buffers.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }

    thread.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_one_stream_gaps_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input, _, receiver_output, thread) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();
    sender_input.send(SendData::Buffers(5)).unwrap();
    sender_input.send(SendData::Gaps(5)).unwrap();
    drop(sender_input);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers = recv_buffers(&receiver_output, &mut segment, 0);
    assert_eq!(buffers.len(), 10);
    for (index, &(running_time, pts)) in buffers.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }

    thread.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_one_stream_close_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input, receiver_input_done, receiver_output, thread) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    sender_input.send(SendData::Buffers(10)).unwrap();
    receiver_input_done.recv().unwrap();
    togglerecord.set_property("record", &true).unwrap();
    sender_input.send(SendData::Buffers(10)).unwrap();
    drop(sender_input);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers = recv_buffers(&receiver_output, &mut segment, 0);
    assert_eq!(buffers.len(), 10);
    for (index, &(running_time, pts)) in buffers.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, (10 + index) * 20 * gst::MSECOND);
    }

    thread.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_one_stream_open_close() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input, receiver_input_done, receiver_output, thread) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();
    sender_input.send(SendData::Buffers(10)).unwrap();
    receiver_input_done.recv().unwrap();
    togglerecord.set_property("record", &false).unwrap();
    sender_input.send(SendData::Buffers(10)).unwrap();
    drop(sender_input);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers = recv_buffers(&receiver_output, &mut segment, 0);
    assert_eq!(buffers.len(), 10);
    for (index, &(running_time, pts)) in buffers.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }

    thread.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_one_stream_open_close_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input, receiver_input_done, receiver_output, thread) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();
    sender_input.send(SendData::Buffers(10)).unwrap();
    receiver_input_done.recv().unwrap();
    togglerecord.set_property("record", &false).unwrap();
    sender_input.send(SendData::Buffers(10)).unwrap();
    receiver_input_done.recv().unwrap();
    togglerecord.set_property("record", &true).unwrap();
    sender_input.send(SendData::Buffers(10)).unwrap();
    drop(sender_input);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers = recv_buffers(&receiver_output, &mut segment, 0);
    assert_eq!(buffers.len(), 20);
    for (index, &(running_time, pts)) in buffers.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }

    thread.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();
    receiver_input_done_1.recv().unwrap();
    sender_input_1.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 10);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 10);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_open_shift() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 5 * gst::MSECOND);

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_1.recv().unwrap();
    sender_input_1.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 10);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, 5 * gst::MSECOND + index * 20 * gst::MSECOND);
        assert_eq!(pts, 5 * gst::MSECOND + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 9);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_open_shift_main() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 5 * gst::MSECOND);
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();
    receiver_input_done_1.recv().unwrap();
    sender_input_1.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    // PTS 5 maps to running time 0 now
    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, 5 * gst::MSECOND + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 10);

    // First and last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, 15 * gst::MSECOND + index * 20 * gst::MSECOND);
        assert_eq!(pts, 20 * gst::MSECOND + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 9);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_open_close() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();

    // Sender 2 is waiting for sender 1 to continue, sender 1 is finished
    receiver_input_done_1.recv().unwrap();

    // Stop recording and push new buffers to sender 1, which will advance
    // it and release the 11th buffer of sender 2 above
    togglerecord.set_property("record", &false).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another 9 buffers to sender 2, both are the same position now
    sender_input_2.send(SendData::Buffers(9)).unwrap();

    // Wait until all 20 buffers of both senders are done
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send EOS and wait for it to be handled
    sender_input_1.send(SendData::Eos).unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 10);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 10);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_close_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &false).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();

    // Sender 2 is waiting for sender 1 to continue, sender 1 is finished
    receiver_input_done_1.recv().unwrap();

    // Start recording and push new buffers to sender 1, which will advance
    // it and release the 11th buffer of sender 2 above
    togglerecord.set_property("record", &true).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another 9 buffers to sender 2, both are the same position now
    sender_input_2.send(SendData::Buffers(9)).unwrap();

    // Wait until all 20 buffers of both senders are done
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send EOS and wait for it to be handled
    sender_input_1.send(SendData::Eos).unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, (10 + index) * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 10);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, (10 + index) * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 10);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_open_close_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();

    // Sender 2 is waiting for sender 1 to continue, sender 1 is finished
    receiver_input_done_1.recv().unwrap();

    // Stop recording and push new buffers to sender 1, which will advance
    // it and release the 11th buffer of sender 2 above
    togglerecord.set_property("record", &false).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another 9 buffers to sender 2, both are the same position now
    sender_input_2.send(SendData::Buffers(9)).unwrap();

    // Wait until all 20 buffers of both senders are done
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another buffer to sender 2, this will block until sender 1 advances
    // but must not be dropped, although we're not recording (yet)
    sender_input_2.send(SendData::Buffers(1)).unwrap();

    // Start recording again and send another set of buffers to both senders
    togglerecord.set_property("record", &true).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_1.recv().unwrap();
    // The single buffer above for sender 1 should be handled now
    receiver_input_done_2.recv().unwrap();

    // Send EOS and wait for it to be handled
    sender_input_1.send(SendData::Eos).unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 20);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 20);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_open_close_open_gaps() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();

    sender_input_1.send(SendData::Buffers(3)).unwrap();
    sender_input_1.send(SendData::Gaps(3)).unwrap();
    sender_input_1.send(SendData::Buffers(4)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();

    // Sender 2 is waiting for sender 1 to continue, sender 1 is finished
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_1.recv().unwrap();

    // Stop recording and push new buffers to sender 1, which will advance
    // it and release the 11th buffer of sender 2 above
    togglerecord.set_property("record", &false).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another 4 gaps and 5 buffers to sender 2, both are the same position now
    sender_input_2.send(SendData::Gaps(4)).unwrap();
    sender_input_2.send(SendData::Buffers(5)).unwrap();

    // Wait until all 20 buffers of both senders are done
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another gap to sender 2, this will block until sender 1 advances
    // but must not be dropped, although we're not recording (yet)
    sender_input_2.send(SendData::Gaps(1)).unwrap();

    // Start recording again and send another set of buffers to both senders
    togglerecord.set_property("record", &true).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_1.recv().unwrap();
    // The single buffer above for sender 1 should be handled now
    receiver_input_done_2.recv().unwrap();

    // Send EOS and wait for it to be handled
    sender_input_1.send(SendData::Eos).unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 20);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 20);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_two_stream_close_open_close_delta() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &false).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();

    // Sender 2 is waiting for sender 1 to continue, sender 1 is finished
    receiver_input_done_1.recv().unwrap();

    // Start recording and push new buffers to sender 1. The first one is a delta frame,
    // so will be dropped, and as such the next frame of sender 2 will also be dropped
    // Sender 2 is empty now
    togglerecord.set_property("record", &true).unwrap();
    sender_input_1.send(SendData::BuffersDelta(1)).unwrap();
    sender_input_1.send(SendData::Buffers(9)).unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another 9 buffers to sender 2, both are the same position now
    sender_input_2.send(SendData::Buffers(9)).unwrap();

    // Wait until all 20 buffers of both senders are done
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another buffer to sender 2, this will block until sender 1 advances
    // but must not be dropped, and we're still recording
    sender_input_2.send(SendData::Buffers(1)).unwrap();

    // Stop recording again and send another set of buffers to both senders
    // The first one is a delta frame, so we only actually stop recording
    // after recording another frame
    togglerecord.set_property("record", &false).unwrap();
    sender_input_1.send(SendData::BuffersDelta(1)).unwrap();
    sender_input_1.send(SendData::Buffers(9)).unwrap();
    sender_input_2.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_1.recv().unwrap();
    // The single buffer above for sender 1 should be handled now
    receiver_input_done_2.recv().unwrap();

    // Send EOS and wait for it to be handled
    sender_input_1.send(SendData::Eos).unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_2.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, (11 + index) * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 10);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, (11 + index) * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 10);

    thread_1.join().unwrap();
    thread_2.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_three_stream_open_close_open() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();
    pipeline.add(&togglerecord).unwrap();

    let (sender_input_1, receiver_input_done_1, receiver_output_1, thread_1) =
        setup_sender_receiver(&pipeline, &togglerecord, "src", 0.into());
    let (sender_input_2, receiver_input_done_2, receiver_output_2, thread_2) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());
    let (sender_input_3, receiver_input_done_3, receiver_output_3, thread_3) =
        setup_sender_receiver(&pipeline, &togglerecord, "src_%u", 0.into());

    pipeline.set_state(gst::State::Playing).unwrap();

    togglerecord.set_property("record", &true).unwrap();

    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(11)).unwrap();
    sender_input_3.send(SendData::Buffers(10)).unwrap();

    // Sender 2 is waiting for sender 1 to continue, sender 1/3 are finished
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_3.recv().unwrap();

    // Stop recording and push new buffers to sender 1, which will advance
    // it and release the 11th buffer of sender 2 above
    togglerecord.set_property("record", &false).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    receiver_input_done_2.recv().unwrap();

    // Send another 9 buffers to sender 2, 1/2 are at the same position now
    sender_input_2.send(SendData::Buffers(9)).unwrap();

    // Send the remaining 10 buffers to sender 3, all are at the same position now
    sender_input_3.send(SendData::Buffers(10)).unwrap();

    // Wait until all 20 buffers of all senders are done
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_3.recv().unwrap();

    // Send another buffer to sender 2, this will block until sender 1 advances
    // but must not be dropped, although we're not recording (yet)
    sender_input_2.send(SendData::Buffers(1)).unwrap();

    // Start recording again and send another set of buffers to both senders
    togglerecord.set_property("record", &true).unwrap();
    sender_input_1.send(SendData::Buffers(10)).unwrap();
    sender_input_2.send(SendData::Buffers(10)).unwrap();
    sender_input_3.send(SendData::Buffers(5)).unwrap();
    receiver_input_done_1.recv().unwrap();
    // The single buffer above for sender 1 should be handled now
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_3.recv().unwrap();

    sender_input_3.send(SendData::Buffers(5)).unwrap();
    receiver_input_done_3.recv().unwrap();

    // Send EOS and wait for it to be handled
    sender_input_1.send(SendData::Eos).unwrap();
    sender_input_2.send(SendData::Eos).unwrap();
    sender_input_3.send(SendData::Eos).unwrap();
    receiver_input_done_1.recv().unwrap();
    receiver_input_done_2.recv().unwrap();
    receiver_input_done_3.recv().unwrap();

    let mut segment_1 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_1 = recv_buffers(&receiver_output_1, &mut segment_1, 0);
    for (index, &(running_time, pts)) in buffers_1.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_1.len(), 20);

    // Last buffer should be dropped from second stream
    let mut segment_2 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_2 = recv_buffers(&receiver_output_2, &mut segment_2, 0);
    for (index, &(running_time, pts)) in buffers_2.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_2.len(), 20);

    let mut segment_3 = gst::FormattedSegment::<gst::ClockTime>::new();
    let buffers_3 = recv_buffers(&receiver_output_3, &mut segment_3, 0);
    for (index, &(running_time, pts)) in buffers_3.iter().enumerate() {
        let pts_off = if index >= 10 {
            10 * 20 * gst::MSECOND
        } else {
            0.into()
        };

        let index = index as u64;
        assert_eq!(running_time, index * 20 * gst::MSECOND);
        assert_eq!(pts, pts_off + index * 20 * gst::MSECOND);
    }
    assert_eq!(buffers_3.len(), 20);

    thread_1.join().unwrap();
    thread_2.join().unwrap();
    thread_3.join().unwrap();

    pipeline.set_state(gst::State::Null).unwrap();
}
