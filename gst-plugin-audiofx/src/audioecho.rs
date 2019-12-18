// Copyright (C) 2017,2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_audio;
use gst_base;
use gst_base::subclass::prelude::*;

use std::sync::Mutex;
use std::{cmp, i32, iter, u64};

use byte_slice_cast::*;

use num_traits::cast::{FromPrimitive, ToPrimitive};
use num_traits::float::Float;

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "rsaudioecho",
        gst::DebugColorFlags::empty(),
        Some("Rust Audio Echo Filter"),
    );
}

const DEFAULT_MAX_DELAY: u64 = gst::SECOND_VAL;
const DEFAULT_DELAY: u64 = 500 * gst::MSECOND_VAL;
const DEFAULT_INTENSITY: f64 = 0.5;
const DEFAULT_FEEDBACK: f64 = 0.0;

#[derive(Debug, Clone, Copy)]
struct Settings {
    pub max_delay: u64,
    pub delay: u64,
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

struct AudioEcho {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

static PROPERTIES: [subclass::Property; 4] = [
    subclass::Property("max-delay", |name| {
        glib::ParamSpec::uint64(name,
        "Maximum Delay",
        "Maximum delay of the echo in nanoseconds (can't be changed in PLAYING or PAUSED state)",
        0, u64::MAX,
        DEFAULT_MAX_DELAY,
        glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("delay", |name| {
        glib::ParamSpec::uint64(
            name,
            "Delay",
            "Delay of the echo in nanoseconds",
            0,
            u64::MAX,
            DEFAULT_DELAY,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("intensity", |name| {
        glib::ParamSpec::double(
            name,
            "Intensity",
            "Intensity of the echo",
            0.0,
            1.0,
            DEFAULT_INTENSITY,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("feedback", |name| {
        glib::ParamSpec::double(
            name,
            "Feedback",
            "Amount of feedback",
            0.0,
            1.0,
            DEFAULT_FEEDBACK,
            glib::ParamFlags::READWRITE,
        )
    }),
];

impl AudioEcho {
    fn process<F: Float + ToPrimitive + FromPrimitive>(
        data: &mut [F],
        state: &mut State,
        settings: &Settings,
    ) {
        let delay_frames = (settings.delay as usize)
            * (state.info.channels() as usize)
            * (state.info.rate() as usize)
            / (gst::SECOND_VAL as usize);

        for (i, (o, e)) in data.iter_mut().zip(state.buffer.iter(delay_frames)) {
            let inp = (*i).to_f64().unwrap();
            let out = inp + settings.intensity * e;
            *o = inp + settings.feedback * e;
            *i = FromPrimitive::from_f64(out).unwrap();
        }
    }
}

impl ObjectSubclass for AudioEcho {
    const NAME: &'static str = "RsAudioEcho";
    type ParentType = gst_base::BaseTransform;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(None),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Audio echo",
            "Filter/Effect/Audio",
            "Adds an echo or reverb effect to an audio stream",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

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
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&PROPERTIES);

        klass.configure(
            gst_base::subclass::BaseTransformMode::AlwaysInPlace,
            false,
            false,
        );
    }
}

impl ObjectImpl for AudioEcho {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("max-delay", ..) => {
                let mut settings = self.settings.lock().unwrap();
                if self.state.lock().unwrap().is_none() {
                    settings.max_delay = value.get_some().expect("type checked upstream");
                }
            }
            subclass::Property("delay", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.delay = value.get_some().expect("type checked upstream");
            }
            subclass::Property("intensity", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.intensity = value.get_some().expect("type checked upstream");
            }
            subclass::Property("feedback", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.feedback = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("max-delay", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.max_delay.to_value())
            }
            subclass::Property("delay", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.delay.to_value())
            }
            subclass::Property("intensity", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.intensity.to_value())
            }
            subclass::Property("feedback", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.feedback.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for AudioEcho {}

impl BaseTransformImpl for AudioEcho {
    fn transform_ip(
        &self,
        _element: &gst_base::BaseTransform,
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
        _element: &gst_base::BaseTransform,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        if incaps != outcaps {
            return Err(gst_loggable_error!(
                CAT,
                "Input and output caps are not the same"
            ));
        }

        let info = gst_audio::AudioInfo::from_caps(incaps)
            .or(Err(gst_loggable_error!(CAT, "Failed to parse input caps")))?;
        let max_delay = self.settings.lock().unwrap().max_delay;
        let size = max_delay * (info.rate() as u64) / gst::SECOND_VAL;
        let buffer_size = size * (info.channels() as u64);

        *self.state.lock().unwrap() = Some(State {
            info,
            buffer: RingBuffer::new(buffer_size as usize),
        });

        Ok(())
    }

    fn stop(&self, _element: &gst_base::BaseTransform) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.lock().unwrap().take();

        Ok(())
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsaudioecho",
        gst::Rank::None,
        AudioEcho::get_type(),
    )
}

struct RingBuffer {
    buffer: Box<[f64]>,
    pos: usize,
}

impl RingBuffer {
    fn new(size: usize) -> Self {
        let mut buffer = Vec::with_capacity(size as usize);
        buffer.extend(iter::repeat(0.0).take(size as usize));

        Self {
            buffer: buffer.into_boxed_slice(),
            pos: 0,
        }
    }

    fn iter(&mut self, delay: usize) -> RingBufferIter {
        RingBufferIter::new(self, delay)
    }
}

struct RingBufferIter<'a> {
    buffer: &'a mut [f64],
    buffer_pos: &'a mut usize,
    read_pos: usize,
    write_pos: usize,
}

impl<'a> RingBufferIter<'a> {
    fn new(buffer: &'a mut RingBuffer, delay: usize) -> RingBufferIter<'a> {
        let size = buffer.buffer.len();

        assert!(size >= delay);
        assert_ne!(size, 0);

        let read_pos = (size - delay + buffer.pos) % size;
        let write_pos = buffer.pos % size;

        let buffer_pos = &mut buffer.pos;
        let buffer = &mut buffer.buffer;

        RingBufferIter {
            buffer,
            buffer_pos,
            read_pos,
            write_pos,
        }
    }
}

impl<'a> Iterator for RingBufferIter<'a> {
    type Item = (&'a mut f64, f64);

    fn next(&mut self) -> Option<Self::Item> {
        let res = unsafe {
            let r = *self.buffer.get_unchecked(self.read_pos);
            let w = self.buffer.get_unchecked_mut(self.write_pos);
            // Cast needed to get from &mut f64 to &'a mut f64
            (&mut *(w as *mut f64), r)
        };

        let size = self.buffer.len();
        self.write_pos = (self.write_pos + 1) % size;
        self.read_pos = (self.read_pos + 1) % size;

        Some(res)
    }
}

impl<'a> Drop for RingBufferIter<'a> {
    fn drop(&mut self) {
        *self.buffer_pos = self.write_pos;
    }
}
