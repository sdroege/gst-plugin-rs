// GStreamer minus1mixer element - unit tests
//
// Copyright (C) 2026 Taruntej Kanakamalla <tarun centricular com>
// Copyright (C) 2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use std::collections::HashMap;
use std::sync::Once;

use byte_slice_cast::*;

fn init() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("plugin registration");
    });
}

struct Pipeline(gst::Pipeline);

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

impl std::ops::Deref for Pipeline {
    type Target = gst::Pipeline;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[test]
fn minus1mixer_basic() {
    init();

    let rate = 48_000;
    let bands = 512;
    let band_width = rate / 2 / bands;
    let threshold = -80.0;
    let input_freq = [440, 880, 1760];
    let spectra = HashMap::from([("s0", [880, 1760]), ("s1", [440, 1760]), ("s2", [440, 880])]);

    let pipeline = Pipeline(gst::parse::launch(&format!(
        "minus1mixer name=mixer sample-rate={rate}
            audiotestsrc num-buffers=10 samplesperbuffer=960 freq={f0} ! mixer.sink_0
            audiotestsrc num-buffers=10 samplesperbuffer=960 freq={f1} ! mixer.sink_1
            audiotestsrc num-buffers=10 samplesperbuffer=960 freq={f2} ! mixer.sink_2
            mixer.minus_0 ! queue ! spectrum name=s0 interval=200000000 post-messages=true bands={bands} ! fakesink
            mixer.minus_1 ! queue ! spectrum name=s1 interval=200000000 post-messages=true bands={bands} ! fakesink
            mixer.minus_2 ! queue ! spectrum name=s2 interval=200000000 post-messages=true bands={bands} ! fakesink
            ",
        f0 = input_freq[0],
        f1 = input_freq[1],
        f2 = input_freq[2],
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap());

    pipeline.debug_to_dot_file(gst::DebugGraphDetails::empty(), "pipeline.dot");

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(e) => panic!("Received error on bus {e}"),
            MessageView::Element(_) => {
                let s = msg.structure().unwrap();
                println!(" {}", s.name());

                let Some(ele) = msg.src() else {
                    break;
                };

                println!("{}", ele.name());
                let Some(freq) = spectra.get(ele.name().as_str()) else {
                    break;
                };

                let mags = s
                    .get::<gst::List>("magnitude")
                    .unwrap()
                    .iter()
                    .map(|v| v.get::<f32>().unwrap())
                    .collect::<Vec<f32>>();

                assert_eq!(mags.len(), bands);

                let mut peak_mag = threshold;
                let mut peak_count = 0_usize;
                for (bin, mag) in mags.iter().enumerate() {
                    // let f = bin * 44100 / (bands * 2);
                    // println!("{mag}: {f}");

                    peak_mag = f32::max(*mag, peak_mag);
                    if (peak_mag > *mag) && (peak_mag > -25.0) {
                        //previous magnitude was the peak, get its frequency
                        let peak_freq = bin * band_width - band_width / 2;

                        println!("peak_mag: {peak_mag} {peak_freq} ");
                        println!("expected {freq:?} {peak_count}");

                        // ~20Hz of tolerance
                        assert!(
                            (peak_freq.saturating_sub(band_width)
                                ..peak_freq.saturating_add(band_width))
                                .contains(&freq[peak_count])
                        );

                        // TODO handle if frequency is in the same range for two or more streams?
                        peak_count += 1;

                        // reset the peak to threshold
                        peak_mag = threshold;
                    }
                }
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    assert!(eos);
}

fn minus1mixer_direct_link<
    T: byte_slice_cast::FromByteSlice
        + std::fmt::Display
        + std::ops::Add
        + std::ops::Add<Output = T>
        + std::ops::Div
        + std::ops::Div<Output = T>
        + std::ops::Sub
        + std::ops::Sub<Output = T>
        + Copy,
>(
    format: gst_audio::AudioFormat,
    values: &[T; 3],
    check_func: fn(T, T) -> bool,
) where
    <T as std::ops::Add>::Output: std::ops::Div<T>,
{
    init();

    let pipeline = Pipeline(gst::Pipeline::default());

    let mixer = gst::ElementFactory::make("minus1mixer")
        .property("output-buffer-duration", 10.mseconds())
        .build()
        .unwrap();

    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", format.to_str())
        .build();

    let appsink_3 = gst_app::AppSink::builder()
        .name("appsink_3")
        .property("caps", &caps)
        .async_(false)
        .sync(false)
        .build();
    let appsink_17 = gst_app::AppSink::builder()
        .name("appsink_17")
        .property("caps", &caps)
        .async_(false)
        .sync(false)
        .build();
    let appsink_32 = gst_app::AppSink::builder()
        .name("appsink_32")
        .property("caps", caps)
        .async_(false)
        .sync(false)
        .build();

    pipeline.add(&mixer).unwrap();
    pipeline.add(&appsink_3).unwrap();
    pipeline.add(&appsink_17).unwrap();
    pipeline.add(&appsink_32).unwrap();

    let sink_3 = mixer.request_pad_simple("sink_3").expect("request sink_3");
    let sink_17 = mixer
        .request_pad_simple("sink_17")
        .expect("request sink_17");
    let sink_32 = mixer
        .request_pad_simple("sink_32")
        .expect("request sink_32");

    mixer.link_pads(Some("minus_3"), &appsink_3, None).unwrap();
    mixer
        .link_pads(Some("minus_17"), &appsink_17, None)
        .unwrap();
    mixer
        .link_pads(Some("minus_32"), &appsink_32, None)
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    let caps = gst_audio::AudioInfo::builder(format, 48_000, 1)
        .build()
        .unwrap()
        .to_caps()
        .unwrap();

    for (i, sink) in [&sink_3, &sink_17, &sink_32].into_iter().enumerate() {
        assert!(sink.send_event(gst::event::StreamStart::builder(&format!("stream-{i}")).build()));
        assert!(sink.send_event(gst::event::Caps::builder(&caps).build()));
        assert!(sink.send_event(gst::event::Segment::builder(&segment).build()));
    }

    for (i, sink) in [&sink_3, &sink_17, &sink_32].into_iter().enumerate() {
        let mut buffer = gst::Buffer::with_size(480 * std::mem::size_of::<T>()).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::ZERO);
            let mut map = buffer.map_writable().unwrap();
            let slice = map.as_mut_slice_of::<T>().unwrap();

            slice.fill(values[i]);
        }
        sink.chain(buffer).unwrap();
    }

    for (i, sink) in [&appsink_3, &appsink_17, &appsink_32]
        .into_iter()
        .enumerate()
    {
        println!("pulling {i}");
        let sample = sink.pull_sample().unwrap();
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        let slice = map.as_slice_of::<T>().unwrap();
        assert_eq!(slice.len(), 480);

        let expected: T = match i {
            0 => values[1] + values[2],
            1 => values[0] + values[2],
            2 => values[0] + values[1],
            _ => unreachable!(),
        };
        for (j, &sample) in slice.iter().enumerate() {
            assert!(
                check_func(sample, expected),
                "{} sample[{j}] = {sample}, expected {expected}",
                sink.name(),
            )
        }
    }

    println!("shutdown");

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn minus1mixer_direct_link_f32() {
    minus1mixer_direct_link::<f32>(
        gst_audio::AUDIO_FORMAT_F32,
        &[1.0, 10.0, 100.0],
        |sample, expected| (sample - expected).abs() < 1e-6,
    );
}

#[test]
fn minus1mixer_direct_link_s16() {
    minus1mixer_direct_link::<i16>(
        gst_audio::AUDIO_FORMAT_S16,
        &[1, 10, 100],
        |sample, expected| sample == expected,
    );
}
