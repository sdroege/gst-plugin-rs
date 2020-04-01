// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
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
use gstrsaudiofx;

use glib;

extern crate gstreamer as gst;
extern crate gstreamer_app as gst_app;
extern crate gstreamer_audio as gst_audio;
extern crate gstreamer_check as gst_check;

use glib::prelude::*;
use gst::prelude::*;

use byte_slice_cast::*;

use std::sync::{Arc, Mutex};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsaudiofx::plugin_register_static().expect("Failed to register rsaudiofx plugin");
    });
}

fn run_test(wave: &str, num_buffers: u32, samples_per_buffer: u32, channels: u32) {
    init();

    let pipeline = gst::parse_launch(&format!(
        "audiotestsrc wave={} num-buffers={} samplesperbuffer={} ! rsaudioloudnorm ! appsink name=sink",
        wave, num_buffers, samples_per_buffer
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let sink = pipeline
        .get_by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    sink.set_property("sync", &false).unwrap();
    let caps = gst_audio::AudioInfo::new(gst_audio::AUDIO_FORMAT_F64, 192_000, channels)
        .build()
        .unwrap()
        .to_caps()
        .unwrap();
    sink.set_caps(Some(&caps));

    let samples = Arc::new(Mutex::new(Vec::new()));

    let samples_clone = samples.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::new()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().unwrap();

                let mut samples = samples_clone.lock().unwrap();
                samples.push(sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    let bus = pipeline.get_bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::CLOCK_TIME_NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(..) => unreachable!(),
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    assert!(eos);
    let samples = samples.lock().unwrap();

    let mut r128 = ebur128::EbuR128::new(
        channels,
        192_000,
        ebur128::Mode::I | ebur128::Mode::SAMPLE_PEAK,
    )
    .unwrap();

    let mut num_samples = 0;
    let mut expected_ts = gst::ClockTime::from(0);
    for sample in samples.iter() {
        let buf = sample.get_buffer().unwrap();

        let ts = buf.get_pts();
        if ts > expected_ts {
            assert!(
                ts - expected_ts <= gst::ClockTime::from(1),
                "TS is {} instead of {}",
                ts,
                expected_ts
            );
        } else if ts < expected_ts {
            assert!(
                expected_ts - ts <= gst::ClockTime::from(1),
                "TS is {} instead of {}",
                ts,
                expected_ts
            );
        }

        let map = buf.map_readable().unwrap();
        let data = map.as_slice_of::<f64>().unwrap();

        num_samples += data.len() / channels as usize;
        r128.add_frames_f64(data).unwrap();

        expected_ts +=
            gst::ClockTime::from((data.len() as u64 / channels as u64) * gst::SECOND_VAL / 192_000);
    }

    assert_eq!(
        num_samples,
        num_buffers as usize * samples_per_buffer as usize
    );

    let loudness = r128.loudness_global().unwrap();
    assert!(
        f64::abs(loudness + 24.0) < 1.0,
        "Loudness is {} instead of -24.0",
        loudness
    );

    for c in 0..channels {
        let peak = 20.0 * f64::log10(r128.sample_peak(c).unwrap());
        assert!(peak < -2.0, "Peak {} for channel {} is above -2.0", c, peak,);
    }
}

#[test]
fn basic() {
    run_test("sine", 1000, 1920, 1);
}

#[test]
fn basic_white_noise() {
    run_test("white-noise", 1000, 1920, 1);
}

#[test]
fn remaining_at_eos() {
    run_test("sine", 1000, 1024, 1);
}

#[test]
fn short_input() {
    run_test("sine", 100, 1024, 1);
}

#[test]
fn basic_two_channels() {
    run_test("sine", 1000, 1920, 2);
}
