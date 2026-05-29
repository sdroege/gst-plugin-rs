// Copyright (C) 2026 Vivia Nikolaidou <vivia@ahiru.eu>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
/**
 * SECTION:element-agingradio
 *
 * An element that adds age to an audio stream using various kinds of distortion. Imagine listening
 * to music on a very old and broken radio, or waiting on hold at a customer service hotline where
 * the hold music sounds horrible.
 *
 * Currently supports: White noise of configurable amplitude, clicks of configurable probability,
 * lowpass filter of configurable frequency, quantisation noise of a configurable amount of bits,
 * and cubic curve distortion of a configurable amount and number of passes.
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 filesrc location=music_file ! decodebin ! audioconvert ! agingradio !
 * audioconvert ! autoaudiosink
 * ]| This will play back the music file using the default distortion parameters.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_audio::subclass::prelude::*;

use std::sync::Mutex;

use byte_slice_cast::*;

use num_traits::cast::{FromPrimitive, ToPrimitive};
use num_traits::{Pow, float::Float};

use lowpass_filter::LowpassFilter;
use rand::prelude::*;

use std::sync::LazyLock;
static _CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "agingradio",
        gst::DebugColorFlags::empty(),
        Some("Rust Aging Radio Filter"),
    )
});

const DEFAULT_WHITE_NOISE_AMPL: f32 = 0.011;
const DEFAULT_CLICKS_PROB: f32 = 1.0 / 100000.0;
const DEFAULT_LOWPASS_FREQ: u32 = 2000;
const DEFAULT_BITS_TO_QUANTIZE: f32 = 4.0;
const DEFAULT_CUBIC_CURVE_DISTORTION: f32 = 1.0;
const DEFAULT_CUBIC_CURVE_PASSES: u32 = 3;

#[derive(Debug, Clone, Copy)]
struct Settings {
    pub white_noise_ampl: f32,
    pub clicks_prob: f32,
    pub lowpass_freq: u32,
    pub bits_to_quantize: f32,
    pub cubic_curve_distortion: f32,
    pub cubic_curve_passes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            white_noise_ampl: DEFAULT_WHITE_NOISE_AMPL,
            clicks_prob: DEFAULT_CLICKS_PROB,
            lowpass_freq: DEFAULT_LOWPASS_FREQ,
            bits_to_quantize: DEFAULT_BITS_TO_QUANTIZE,
            cubic_curve_distortion: DEFAULT_CUBIC_CURVE_DISTORTION,
            cubic_curve_passes: DEFAULT_CUBIC_CURVE_PASSES,
        }
    }
}

struct State {
    info: gst_audio::AudioInfo,
    filters: Option<Vec<LowpassFilter<f64>>>,
}

#[derive(Default)]
pub struct AgingRadio {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

impl AgingRadio {
    fn process<F: Float + ToPrimitive + FromPrimitive>(
        data: &mut [F],
        state: &mut State,
        settings: &Settings,
    ) {
        let mut rng = rand::rng();
        let channels = state.info.channels();
        for slice in data.chunks_exact_mut(channels as usize * 2) {
            let have_click =
                settings.clicks_prob > 0.0 && rng.random_bool(settings.clicks_prob.into());
            for (c, sample) in slice.iter_mut().enumerate() {
                let mut out_sample = (*sample).to_f64().unwrap();
                if have_click {
                    out_sample = 1.0;
                } else {
                    let ampl = settings.white_noise_ampl as f64;
                    if ampl > 0.0 {
                        out_sample += rng.random_range(-ampl..ampl);
                    }
                    if let Some(ref mut filters) = state.filters {
                        let lowpass_filter = &mut filters[c % channels as usize];
                        out_sample = lowpass_filter.run(out_sample.clamp(-1.0, 1.0));
                    }
                    if settings.bits_to_quantize > 0.0 {
                        let factor = 2.0_f64.pow(settings.bits_to_quantize);
                        out_sample *= factor;
                        out_sample = out_sample.round();
                        out_sample /= factor;
                    }
                    if settings.cubic_curve_distortion > 0.0 && settings.cubic_curve_passes > 0 {
                        for _ in 0..settings.cubic_curve_passes {
                            out_sample = out_sample
                                - (settings.cubic_curve_distortion as f64) * out_sample.pow(3);
                        }
                    }
                }
                *sample = FromPrimitive::from_f64(out_sample).unwrap();
            }
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AgingRadio {
    const NAME: &'static str = "GstRsAgingRadio";
    type Type = super::AgingRadio;
    type ParentType = gst_audio::AudioFilter;
}

impl ObjectImpl for AgingRadio {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("white-noise-ampl")
                    .nick("White noise amplitude")
                    .blurb("White noise amplitude (0 to disable)")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(DEFAULT_WHITE_NOISE_AMPL)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFloat::builder("clicks-prob")
                    .nick("Clicks probability")
                    .blurb("Clicks probability (0 to disable)")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(DEFAULT_CLICKS_PROB)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("lowpass-freq")
                    .nick("Lowpass filter frequency")
                    .blurb("Lowpass filter frequency (0 to disable)")
                    .maximum(22000)
                    .default_value(DEFAULT_LOWPASS_FREQ)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFloat::builder("bits-to-quantize")
                    .nick("Bits to quantize")
                    .blurb("Bits to quantize (0 to disable)")
                    .minimum(0.0)
                    .maximum(64.0)
                    .default_value(DEFAULT_BITS_TO_QUANTIZE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFloat::builder("cubic-curve-distortion")
                    .nick("Cubic curve distortion")
                    .blurb("Cubic curve distortion (0 to disable)")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(DEFAULT_CUBIC_CURVE_DISTORTION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("cubic-curve-passes")
                    .nick("Cubic curve passes")
                    .blurb("Cubic curve passes (0 to disable)")
                    .default_value(DEFAULT_CUBIC_CURVE_PASSES)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "white-noise-ampl" => {
                let mut settings = self.settings.lock().unwrap();
                if self.state.lock().unwrap().is_none() {
                    settings.white_noise_ampl = value.get::<f32>().unwrap();
                }
            }
            "clicks-prob" => {
                let mut settings = self.settings.lock().unwrap();
                settings.clicks_prob = value.get::<f32>().unwrap();
            }
            "lowpass-freq" => {
                let mut settings = self.settings.lock().unwrap();
                settings.lowpass_freq = value.get::<u32>().unwrap();
            }
            "bits-to-quantize" => {
                let mut settings = self.settings.lock().unwrap();
                settings.bits_to_quantize = value.get::<f32>().unwrap();
            }
            "cubic-curve-distortion" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cubic_curve_distortion = value.get::<f32>().unwrap();
            }
            "cubic-curve-passes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cubic_curve_passes = value.get::<u32>().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "white-noise-ampl" => {
                let settings = self.settings.lock().unwrap();
                settings.white_noise_ampl.to_value()
            }
            "clicks-prob" => {
                let settings = self.settings.lock().unwrap();
                settings.clicks_prob.to_value()
            }
            "lowpass-freq" => {
                let settings = self.settings.lock().unwrap();
                settings.lowpass_freq.to_value()
            }
            "bits-to-quantize" => {
                let settings = self.settings.lock().unwrap();
                settings.bits_to_quantize.to_value()
            }
            "cubic-curve-distortion" => {
                let settings = self.settings.lock().unwrap();
                settings.cubic_curve_distortion.to_value()
            }
            "cubic-curve-passes" => {
                let settings = self.settings.lock().unwrap();
                settings.cubic_curve_passes.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for AgingRadio {}

impl ElementImpl for AgingRadio {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Aging Radio",
                "Filter/Effect/Audio",
                "Adds age to audio input using various kinds of distortion",
                "Vivia Nikolaidou <vivia@ahiru.eu>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BaseTransformImpl for AgingRadio {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_ip(&self, buf: &mut gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = *self.settings.lock().unwrap();

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        let mut map = buf.map_writable().map_err(|_| gst::FlowError::Error)?;

        match state.info.format() {
            gst_audio::AUDIO_FORMAT_F64 => {
                let data = map.as_mut_slice_of::<f64>().unwrap();
                Self::process(data, state, &settings);
            }
            gst_audio::AUDIO_FORMAT_F32 => {
                let data = map.as_mut_slice_of::<f32>().unwrap();
                Self::process(data, state, &settings);
            }
            _ => return Err(gst::FlowError::NotNegotiated),
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.lock().unwrap().take();

        Ok(())
    }
}

impl AudioFilterImpl for AgingRadio {
    fn allowed_caps() -> &'static gst::Caps {
        static CAPS: LazyLock<gst::Caps> = LazyLock::new(|| {
            gst_audio::AudioCapsBuilder::new_interleaved()
                .format_list([gst_audio::AUDIO_FORMAT_F32, gst_audio::AUDIO_FORMAT_F64])
                .build()
        });

        &CAPS
    }

    fn setup(&self, info: &gst_audio::AudioInfo) -> Result<(), gst::LoggableError> {
        let settings = self.settings.lock().unwrap();
        let filters = if settings.lowpass_freq > 0 {
            let mut filters = Vec::new();
            for _ in 0..info.channels() {
                filters.push(LowpassFilter::<f64>::new(
                    info.rate().into(),
                    settings.lowpass_freq.into(),
                ));
            }
            Some(filters)
        } else {
            None
        };
        *self.state.lock().unwrap() = Some(State {
            info: info.clone(),
            filters,
        });

        Ok(())
    }
}
