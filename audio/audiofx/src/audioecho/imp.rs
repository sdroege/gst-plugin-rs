// Copyright (C) 2017,2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use std::sync::Mutex;
use std::{cmp, i32, u64};

use byte_slice_cast::*;

use num_traits::cast::{FromPrimitive, ToPrimitive};
use num_traits::float::Float;

use once_cell::sync::Lazy;
static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rsaudioecho",
        gst::DebugColorFlags::empty(),
        Some("Rust Audio Echo Filter"),
    )
});

use super::ring_buffer::RingBuffer;

const DEFAULT_MAX_DELAY: gst::ClockTime = gst::ClockTime::SECOND;
const DEFAULT_DELAY: gst::ClockTime = gst::ClockTime::from_seconds(500);
const DEFAULT_INTENSITY: f64 = 0.5;
const DEFAULT_FEEDBACK: f64 = 0.0;

#[derive(Debug, Clone, Copy)]
struct Settings {
    pub max_delay: gst::ClockTime,
    pub delay: gst::ClockTime,
    pub intensity: f64,
    pub feedback: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_delay: DEFAULT_MAX_DELAY,
            delay: DEFAULT_DELAY,
            intensity: DEFAULT_INTENSITY,
            feedback: DEFAULT_FEEDBACK,
        }
    }
}

struct State {
    info: gst_audio::AudioInfo,
    buffer: RingBuffer,
}

#[derive(Default)]
pub struct AudioEcho {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

impl AudioEcho {
    fn process<F: Float + ToPrimitive + FromPrimitive>(
        data: &mut [F],
        state: &mut State,
        settings: &Settings,
    ) {
        let delay_frames = (settings.delay
            * (state.info.channels() as u64)
            * (state.info.rate() as u64))
            .seconds() as usize;

        for (i, (o, e)) in data.iter_mut().zip(state.buffer.iter(delay_frames)) {
            let inp = (*i).to_f64().unwrap();
            let out = inp + settings.intensity * e;
            *o = inp + settings.feedback * e;
            *i = FromPrimitive::from_f64(out).unwrap();
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AudioEcho {
    const NAME: &'static str = "RsAudioEcho";
    type Type = super::AudioEcho;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for AudioEcho {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_uint64("max-delay",
                    "Maximum Delay",
                    "Maximum delay of the echo in nanoseconds (can't be changed in PLAYING or PAUSED state)",
                    0, u64::MAX - 1,
                    DEFAULT_MAX_DELAY.nseconds(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_uint64(
                    "delay",
                    "Delay",
                    "Delay of the echo in nanoseconds",
                    0,
                    u64::MAX - 1,
                    DEFAULT_DELAY.nseconds(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_double(
                    "intensity",
                    "Intensity",
                    "Intensity of the echo",
                    0.0,
                    1.0,
                    DEFAULT_INTENSITY,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_double(
                    "feedback",
                    "Feedback",
                    "Amount of feedback",
                    0.0,
                    1.0,
                    DEFAULT_FEEDBACK,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "max-delay" => {
                let mut settings = self.settings.lock().unwrap();
                if self.state.lock().unwrap().is_none() {
                    settings.max_delay =
                        gst::ClockTime::from_nseconds(value.get().expect("type checked upstream"));
                }
            }
            "delay" => {
                let mut settings = self.settings.lock().unwrap();
                settings.delay =
                    gst::ClockTime::from_nseconds(value.get().expect("type checked upstream"));
            }
            "intensity" => {
                let mut settings = self.settings.lock().unwrap();
                settings.intensity = value.get().expect("type checked upstream");
            }
            "feedback" => {
                let mut settings = self.settings.lock().unwrap();
                settings.feedback = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "max-delay" => {
                let settings = self.settings.lock().unwrap();
                settings.max_delay.to_value()
            }
            "delay" => {
                let settings = self.settings.lock().unwrap();
                settings.delay.to_value()
            }
            "intensity" => {
                let settings = self.settings.lock().unwrap();
                settings.intensity.to_value()
            }
            "feedback" => {
                let settings = self.settings.lock().unwrap();
                settings.feedback.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for AudioEcho {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Audio echo",
                "Filter/Effect/Audio",
                "Adds an echo or reverb effect to an audio stream",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_simple(
                "audio/x-raw",
                &[
                    (
                        "format",
                        &gst::List::new(&[
                            &gst_audio::AUDIO_FORMAT_F32.to_str(),
                            &gst_audio::AUDIO_FORMAT_F64.to_str(),
                        ]),
                    ),
                    ("rate", &gst::IntRange::<i32>::new(0, i32::MAX)),
                    ("channels", &gst::IntRange::<i32>::new(0, i32::MAX)),
                    ("layout", &"interleaved"),
                ],
            );
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for AudioEcho {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_ip(
        &self,
        _element: &Self::Type,
        buf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut settings = *self.settings.lock().unwrap();
        settings.delay = cmp::min(settings.max_delay, settings.delay);

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

    fn set_caps(
        &self,
        _element: &Self::Type,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        if incaps != outcaps {
            return Err(gst::loggable_error!(
                CAT,
                "Input and output caps are not the same"
            ));
        }

        let info = gst_audio::AudioInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse input caps"))?;
        let max_delay = self.settings.lock().unwrap().max_delay;
        let size = (max_delay * (info.rate() as u64)).seconds() as usize;
        let buffer_size = size * (info.channels() as usize);

        *self.state.lock().unwrap() = Some(State {
            info,
            buffer: RingBuffer::new(buffer_size),
        });

        Ok(())
    }

    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.lock().unwrap().take();

        Ok(())
    }
}
