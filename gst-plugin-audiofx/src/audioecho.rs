// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use gst;
use gst::prelude::*;
use gst_base;
use gst_base::prelude::*;
use gst_audio;
use gst_audio::prelude::*;

use gst_plugin::properties::*;
use gst_plugin::object::*;
use gst_plugin::element::*;
use gst_plugin::base_transform::*;

use std::{cmp, iter, i32, u64};
use std::sync::Mutex;

use byte_slice_cast::*;

use num_traits::float::Float;
use num_traits::cast::{FromPrimitive, ToPrimitive};

const DEFAULT_MAX_DELAY: u64 = 1 * gst::SECOND_VAL;
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
    cat: gst::DebugCategory,
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

static PROPERTIES: [Property; 4] = [
    Property::UInt64(
        "max-delay",
        "Maximum Delay",
        "Maximum delay of the echo in nanoseconds (can't be changed in PLAYING or PAUSED state)",
        (0, u64::MAX),
        DEFAULT_MAX_DELAY,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt64(
        "delay",
        "Delay",
        "Delay of the echo in nanoseconds",
        (0, u64::MAX),
        DEFAULT_DELAY,
        PropertyMutability::ReadWrite,
    ),
    Property::Double(
        "intensity",
        "Intensity",
        "Intensity of the echo",
        (0.0, 1.0),
        DEFAULT_INTENSITY,
        PropertyMutability::ReadWrite,
    ),
    Property::Double(
        "feedback",
        "Feedback",
        "Amount of feedback",
        (0.0, 1.0),
        DEFAULT_FEEDBACK,
        PropertyMutability::ReadWrite,
    ),
];

impl AudioEcho {
    fn new(_transform: &RsBaseTransform) -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "rsaudiofx",
                gst::DebugColorFlags::empty(),
                "Rust audiofx effect",
            ),
            settings: Mutex::new(Default::default()),
            state: Mutex::new(None),
        }
    }

    fn class_init(klass: &mut RsBaseTransformClass) {
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
                        &gst_audio::AUDIO_FORMAT_F32.to_string(),
                        &gst_audio::AUDIO_FORMAT_F64.to_string(),
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
        );
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&PROPERTIES);

        klass.configure(BaseTransformMode::AlwaysInPlace, false, false);
    }

    fn init(element: &RsBaseTransform) -> Box<BaseTransformImpl<RsBaseTransform>> {
        let imp = Self::new(element);
        Box::new(imp)
    }

    fn process<F: Float + ToPrimitive + FromPrimitive>(
        data: &mut [F],
        state: &mut State,
        settings: &Settings,
    ) {
        let delay_frames = (settings.delay as usize) * (state.info.channels() as usize)
            * (state.info.rate() as usize) / (gst::SECOND_VAL as usize);

        for (i, (o, e)) in data.iter_mut().zip(state.buffer.iter(delay_frames)) {
            let inp = (*i).to_f64().unwrap();
            let out = inp + settings.intensity * e;
            *o = inp + settings.feedback * e;
            *i = FromPrimitive::from_f64(out).unwrap();
        }
    }
}

impl ObjectImpl<RsBaseTransform> for AudioEcho {
    fn set_property(&self, _obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::UInt64("max-delay", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_delay = value.get().unwrap();
            }
            Property::UInt64("delay", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.delay = value.get().unwrap();
            }
            Property::Double("intensity", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.intensity = value.get().unwrap();
            }
            Property::Double("feedback", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.feedback = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::UInt64("max-delay", ..) => {
                let settings = self.settings.lock().unwrap();
                if self.state.lock().unwrap().is_none() {
                    Ok(settings.max_delay.to_value())
                } else {
                    Err(())
                }
            }
            Property::UInt64("delay", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.delay.to_value())
            }
            Property::Double("intensity", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.intensity.to_value())
            }
            Property::Double("feedback", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.feedback.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl<RsBaseTransform> for AudioEcho {}

impl BaseTransformImpl<RsBaseTransform> for AudioEcho {
    fn transform_ip(
        &self,
        _element: &RsBaseTransform,
        buf: &mut gst::BufferRef,
    ) -> gst::FlowReturn {
        let mut settings = *self.settings.lock().unwrap();
        settings.delay = cmp::min(settings.max_delay, settings.delay);

        let mut state_guard = self.state.lock().unwrap();
        let state = match *state_guard {
            None => return gst::FlowReturn::NotNegotiated,
            Some(ref mut state) => state,
        };

        let mut map = match buf.map_writable() {
            None => return gst::FlowReturn::Error,
            Some(map) => map,
        };

        match state.info.format() {
            gst_audio::AUDIO_FORMAT_F64 => {
                let data = map.as_mut_slice().as_mut_slice_of::<f64>().unwrap();
                Self::process(data, state, &settings);
            }
            gst_audio::AUDIO_FORMAT_F32 => {
                let data = map.as_mut_slice().as_mut_slice_of::<f32>().unwrap();
                Self::process(data, state, &settings);
            }
            _ => return gst::FlowReturn::NotNegotiated,
        }

        gst::FlowReturn::Ok
    }

    fn set_caps(
        &self,
        _element: &RsBaseTransform,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> bool {
        if incaps != outcaps {
            return false;
        }

        let info = match gst_audio::AudioInfo::from_caps(incaps) {
            None => return false,
            Some(info) => info,
        };

        let max_delay = self.settings.lock().unwrap().max_delay;
        let size = max_delay * (info.rate() as u64) / gst::SECOND_VAL;
        let buffer_size = size * (info.channels() as u64);

        *self.state.lock().unwrap() = Some(State {
            info: info,
            buffer: RingBuffer::new(buffer_size as usize),
        });

        true
    }

    fn stop(&self, _element: &RsBaseTransform) -> bool {
        // Drop state
        let _ = self.state.lock().unwrap().take();

        true
    }
}

struct AudioEchoStatic;

impl ImplTypeStatic<RsBaseTransform> for AudioEchoStatic {
    fn get_name(&self) -> &str {
        "AudioEcho"
    }

    fn new(&self, element: &RsBaseTransform) -> Box<BaseTransformImpl<RsBaseTransform>> {
        AudioEcho::init(element)
    }

    fn class_init(&self, klass: &mut RsBaseTransformClass) {
        AudioEcho::class_init(klass);
    }
}

pub fn register(plugin: &gst::Plugin) {
    let audioecho_static = AudioEchoStatic;
    let type_ = register_type(audioecho_static);
    gst::Element::register(plugin, "rsaudioecho", 0, type_);
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
            buffer: buffer,
            buffer_pos: buffer_pos,
            read_pos: read_pos,
            write_pos: write_pos,
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
