// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
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

#[cfg(not(feature = "v1_18"))]
use self::gst_base::prelude::*;
#[cfg(not(feature = "v1_18"))]
use self::gst_base::subclass::prelude::*;
#[cfg(not(feature = "v1_18"))]
use super::gst_base_compat as gst_base;

#[cfg(feature = "v1_18")]
use gst_base::prelude::*;
#[cfg(feature = "v1_18")]
use gst_base::subclass::prelude::*;

use glib::subclass;
use glib::subclass::prelude::*;
use gst::subclass::prelude::*;

use once_cell::sync::Lazy;

use std::sync::{Mutex, RwLock};

struct FallbackSwitch {
    sinkpad: gst_base::AggregatorPad,
    fallback_sinkpad: RwLock<Option<gst_base::AggregatorPad>>,
    active_sinkpad: Mutex<gst::Pad>,
    output_state: Mutex<OutputState>,
    pad_states: RwLock<PadStates>,
    settings: Mutex<Settings>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbackswitch",
        gst::DebugColorFlags::empty(),
        Some("Fallback switch Element"),
    )
});

#[derive(Debug)]
struct OutputState {
    last_sinkpad_time: gst::ClockTime,
}

#[derive(Debug, Default)]
struct PadStates {
    sinkpad: PadState,
    fallback_sinkpad: Option<PadState>,
}

#[derive(Debug, Default)]
struct PadState {
    caps: Option<gst::Caps>,
    audio_info: Option<gst_audio::AudioInfo>,
    video_info: Option<gst_video::VideoInfo>,
}

const DEFAULT_TIMEOUT: u64 = 5 * gst::SECOND_VAL;

#[derive(Debug, Clone)]
struct Settings {
    timeout: gst::ClockTime,
}

impl Default for OutputState {
    fn default() -> Self {
        OutputState {
            last_sinkpad_time: gst::CLOCK_TIME_NONE,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            timeout: DEFAULT_TIMEOUT.into(),
        }
    }
}

static PROPERTIES: [subclass::Property; 2] = [
    subclass::Property("timeout", |name| {
        glib::ParamSpec::uint64(
            name,
            "Timeout",
            "Timeout in nanoseconds",
            0,
            std::u64::MAX,
            DEFAULT_TIMEOUT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("active-pad", |name| {
        glib::ParamSpec::object(
            name,
            "Active Pad",
            "Currently active pad",
            gst::Pad::static_type(),
            glib::ParamFlags::READABLE,
        )
    }),
];

impl FallbackSwitch {
    fn handle_main_buffer(
        &self,
        agg: &gst_base::Aggregator,
        state: &mut OutputState,
        mut buffer: gst::Buffer,
        fallback_sinkpad: Option<&gst_base::AggregatorPad>,
    ) -> Result<Option<(gst::Buffer, gst::Caps, bool)>, gst::FlowError> {
        // If we got a buffer on the sinkpad just handle it
        gst_debug!(CAT, obj: agg, "Got buffer on sinkpad {:?}", buffer);

        if buffer.get_pts().is_none() {
            gst_error!(CAT, obj: agg, "Only buffers with PTS supported");
            return Err(gst::FlowError::Error);
        }

        let segment = self
            .sinkpad
            .get_segment()
            .downcast::<gst::ClockTime>()
            .map_err(|_| {
                gst_error!(CAT, obj: agg, "Only TIME segments supported");
                gst::FlowError::Error
            })?;

        {
            // FIXME: This will not work correctly for negative DTS
            let buffer = buffer.make_mut();
            buffer.set_pts(segment.to_running_time(buffer.get_pts()));
            buffer.set_dts(segment.to_running_time(buffer.get_dts()));
        }

        let mut active_sinkpad = self.active_sinkpad.lock().unwrap();
        let pad_change = &*active_sinkpad != self.sinkpad.upcast_ref::<gst::Pad>();
        if pad_change {
            if buffer.get_flags().contains(gst::BufferFlags::DELTA_UNIT) {
                gst_info!(
                    CAT,
                    obj: agg,
                    "Can't change back to sinkpad, waiting for keyframe"
                );
                self.sinkpad.push_event(
                    gst_video::new_upstream_force_key_unit_event()
                        .all_headers(true)
                        .build(),
                );
                return Ok(None);
            }

            gst_info!(CAT, obj: agg, "Active pad changed to sinkpad");
            *active_sinkpad = self.sinkpad.clone().upcast();
        }
        drop(active_sinkpad);

        state.last_sinkpad_time = segment.to_running_time(buffer.get_dts_or_pts());

        // Drop all older buffers from the fallback sinkpad
        if let Some(fallback_sinkpad) = fallback_sinkpad {
            let fallback_segment = self
                .sinkpad
                .get_segment()
                .downcast::<gst::ClockTime>()
                .map_err(|_| {
                    gst_error!(CAT, obj: agg, "Only TIME segments supported");
                    gst::FlowError::Error
                })?;

            while let Some(fallback_buffer) = fallback_sinkpad.peek_buffer() {
                let fallback_pts = fallback_buffer.get_dts_or_pts();
                if fallback_pts.is_none()
                    || fallback_segment.to_running_time(fallback_pts) <= state.last_sinkpad_time
                {
                    gst_debug!(
                        CAT,
                        obj: agg,
                        "Dropping fallback buffer {:?}",
                        fallback_buffer
                    );
                    fallback_sinkpad.drop_buffer();
                } else {
                    break;
                }
            }
        }

        let pad_states = self.pad_states.read().unwrap();
        let active_caps = pad_states.sinkpad.caps.as_ref().unwrap().clone();
        drop(pad_states);

        Ok(Some((buffer, active_caps, pad_change)))
    }

    fn get_fallback_buffer(
        &self,
        agg: &gst_base::Aggregator,
        state: &mut OutputState,
        settings: &Settings,
        fallback_sinkpad: &gst_base::AggregatorPad,
    ) -> Result<(gst::Buffer, gst::Caps, bool), gst::FlowError> {
        // If we have a fallback sinkpad and timeout, try to get a fallback buffer from here
        // and drop all too old buffers in the process
        loop {
            let mut buffer = fallback_sinkpad
                .pop_buffer()
                .ok_or(gst_base::AGGREGATOR_FLOW_NEED_DATA)?;

            gst_debug!(CAT, obj: agg, "Got buffer on fallback sinkpad {:?}", buffer);

            if buffer.get_pts().is_none() {
                gst_error!(CAT, obj: agg, "Only buffers with PTS supported");
                return Err(gst::FlowError::Error);
            }

            let fallback_segment = fallback_sinkpad
                .get_segment()
                .downcast::<gst::ClockTime>()
                .map_err(|_| {
                    gst_error!(CAT, obj: agg, "Only TIME segments supported");
                    gst::FlowError::Error
                })?;
            let running_time = fallback_segment.to_running_time(buffer.get_dts_or_pts());

            {
                // FIXME: This will not work correctly for negative DTS
                let buffer = buffer.make_mut();
                buffer.set_pts(fallback_segment.to_running_time(buffer.get_pts()));
                buffer.set_dts(fallback_segment.to_running_time(buffer.get_dts()));
            }

            // If we never had a real buffer, initialize with the running time of the fallback
            // sinkpad so that we still output fallback buffers after the timeout
            if state.last_sinkpad_time.is_none() {
                state.last_sinkpad_time = running_time;
            }

            // Get the next one if this one is before the timeout
            if state.last_sinkpad_time + settings.timeout > running_time {
                gst_debug!(
                    CAT,
                    obj: agg,
                    "Timeout not reached yet: {} + {} > {}",
                    state.last_sinkpad_time,
                    settings.timeout,
                    running_time
                );

                continue;
            }

            gst_debug!(
                CAT,
                obj: agg,
                "Timeout reached: {} + {} <= {}",
                state.last_sinkpad_time,
                settings.timeout,
                running_time
            );

            let mut active_sinkpad = self.active_sinkpad.lock().unwrap();
            let pad_change = &*active_sinkpad != fallback_sinkpad.upcast_ref::<gst::Pad>();
            if pad_change {
                if buffer.get_flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    gst_info!(
                        CAT,
                        obj: agg,
                        "Can't change to fallback sinkpad yet, waiting for keyframe"
                    );
                    fallback_sinkpad.push_event(
                        gst_video::new_upstream_force_key_unit_event()
                            .all_headers(true)
                            .build(),
                    );
                    continue;
                }

                gst_info!(CAT, obj: agg, "Active pad changed to fallback sinkpad");
                *active_sinkpad = fallback_sinkpad.clone().upcast();
            }
            drop(active_sinkpad);

            let pad_states = self.pad_states.read().unwrap();
            let active_caps = match pad_states.fallback_sinkpad {
                None => {
                    // This can happen if the pad is removed in the meantime,
                    // not a problem really
                    return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
                }
                Some(ref fallback_sinkpad) => fallback_sinkpad.caps.as_ref().unwrap().clone(),
            };
            drop(pad_states);

            break Ok((buffer, active_caps, pad_change));
        }
    }

    fn get_next_buffer(
        &self,
        agg: &gst_base::Aggregator,
        timeout: bool,
    ) -> Result<(gst::Buffer, gst::Caps, bool), gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.output_state.lock().unwrap();
        let fallback_sinkpad = self.fallback_sinkpad.read().unwrap();

        gst_debug!(CAT, obj: agg, "Aggregate called: timeout {}", timeout);

        if let Some(buffer) = self.sinkpad.pop_buffer() {
            if let Some(res) =
                self.handle_main_buffer(agg, &mut *state, buffer, fallback_sinkpad.as_ref())?
            {
                return Ok(res);
            }
        } else if self.sinkpad.is_eos() {
            gst_log!(CAT, obj: agg, "Sinkpad is EOS");
            return Err(gst::FlowError::Eos);
        }

        if let (false, Some(_)) = (timeout, &*fallback_sinkpad) {
            gst_debug!(CAT, obj: agg, "Have fallback sinkpad but no timeout yet");

            Err(gst_base::AGGREGATOR_FLOW_NEED_DATA)
        } else if let (true, Some(fallback_sinkpad)) = (timeout, &*fallback_sinkpad) {
            self.get_fallback_buffer(agg, &mut *state, &settings, fallback_sinkpad)
        } else {
            // Otherwise there's not much we can do at this point
            gst_debug!(
                CAT,
                obj: agg,
                "Got no buffer on sinkpad and have no fallback sinkpad"
            );
            Err(gst_base::AGGREGATOR_FLOW_NEED_DATA)
        }
    }
}

impl ObjectSubclass for FallbackSwitch {
    const NAME: &'static str = "FallbackSwitch";
    type ParentType = gst_base::Aggregator;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad: gst_base::AggregatorPad = glib::Object::new(
            gst_base::AggregatorPad::static_type(),
            &[
                ("name", &"sink"),
                ("direction", &gst::PadDirection::Sink),
                ("template", &templ),
            ],
        )
        .unwrap()
        .downcast()
        .unwrap();

        Self {
            sinkpad: sinkpad.clone(),
            fallback_sinkpad: RwLock::new(None),
            active_sinkpad: Mutex::new(sinkpad.upcast()),
            output_state: Mutex::new(OutputState::default()),
            pad_states: RwLock::new(PadStates::default()),
            settings: Mutex::new(Settings::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Fallback Switch",
            "Generic",
            "Allows switching to a fallback input after a given timeout",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new_with_gtype(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
            gst_base::AggregatorPad::static_type(),
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new_with_gtype(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
            gst_base::AggregatorPad::static_type(),
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let fallbacksink_pad_template = gst::PadTemplate::new_with_gtype(
            "fallback_sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Request,
            &caps,
            gst_base::AggregatorPad::static_type(),
        )
        .unwrap();
        klass.add_pad_template(fallbacksink_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for FallbackSwitch {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let agg = obj.downcast_ref::<gst_base::Aggregator>().unwrap();
        agg.add_pad(&self.sinkpad).unwrap();
    }

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        let agg = obj.downcast_ref::<gst_base::Aggregator>().unwrap();

        match *prop {
            subclass::Property("timeout", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: agg,
                    "Changing timeout from {} to {}",
                    settings.timeout,
                    timeout
                );
                settings.timeout = timeout;
                drop(settings);
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("timeout", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.timeout.to_value())
            }
            subclass::Property("active-pad", ..) => {
                let active_pad = self.active_sinkpad.lock().unwrap().clone();
                Ok(active_pad.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for FallbackSwitch {
    fn request_new_pad(
        &self,
        element: &gst::Element,
        templ: &gst::PadTemplate,
        name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let agg = element.downcast_ref::<gst_base::Aggregator>().unwrap();
        let fallback_sink_templ = agg.get_pad_template("fallback_sink").unwrap();
        if templ != &fallback_sink_templ
            || (name.is_some() && name.as_deref() != Some("fallback_sink"))
        {
            gst_error!(CAT, obj: agg, "Wrong pad template or name");
            return None;
        }

        let mut fallback_sinkpad = self.fallback_sinkpad.write().unwrap();
        if fallback_sinkpad.is_some() {
            gst_error!(CAT, obj: agg, "Already have a fallback sinkpad");
            return None;
        }

        let sinkpad: gst_base::AggregatorPad = glib::Object::new(
            gst_base::AggregatorPad::static_type(),
            &[
                ("name", &"fallback_sink"),
                ("direction", &gst::PadDirection::Sink),
                ("template", templ),
            ],
        )
        .unwrap()
        .downcast()
        .unwrap();
        *fallback_sinkpad = Some(sinkpad.clone());
        drop(fallback_sinkpad);

        agg.add_pad(&sinkpad).unwrap();

        Some(sinkpad.upcast())
    }

    fn release_pad(&self, element: &gst::Element, pad: &gst::Pad) {
        let agg = element.downcast_ref::<gst_base::Aggregator>().unwrap();
        let mut fallback_sinkpad = self.fallback_sinkpad.write().unwrap();
        let mut pad_states = self.pad_states.write().unwrap();

        if fallback_sinkpad.as_ref().map(|p| p.upcast_ref()) == Some(pad) {
            *fallback_sinkpad = None;
            pad_states.fallback_sinkpad = None;
            drop(pad_states);
            drop(fallback_sinkpad);
            agg.remove_pad(pad).unwrap();
            gst_debug!(CAT, obj: agg, "Removed fallback sinkpad {:?}", pad);
        }
    }
}

impl AggregatorImpl for FallbackSwitch {
    fn start(&self, _agg: &gst_base::Aggregator) -> Result<(), gst::ErrorMessage> {
        *self.active_sinkpad.lock().unwrap() = self.sinkpad.clone().upcast();
        *self.output_state.lock().unwrap() = OutputState::default();
        *self.pad_states.write().unwrap() = PadStates::default();

        Ok(())
    }

    fn sink_event_pre_queue(
        &self,
        agg: &gst_base::Aggregator,
        agg_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        use gst::EventView;

        match event.view() {
            EventView::Gap(_) => {
                gst_debug!(CAT, obj: agg_pad, "Dropping gap event");
                Ok(gst::FlowSuccess::Ok)
            }
            _ => self.parent_sink_event_pre_queue(agg, agg_pad, event),
        }
    }

    fn sink_event(
        &self,
        agg: &gst_base::Aggregator,
        agg_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Caps(caps) => {
                let caps = caps.get_caps_owned();
                gst_debug!(CAT, obj: agg_pad, "Received caps {}", caps);

                let audio_info;
                let video_info;
                if caps.get_structure(0).unwrap().get_name() == "audio/x-raw" {
                    audio_info = gst_audio::AudioInfo::from_caps(&caps).ok();
                    video_info = None;
                } else if caps.get_structure(0).unwrap().get_name() == "video/x-raw" {
                    audio_info = None;
                    video_info = gst_video::VideoInfo::from_caps(&caps).ok();
                } else {
                    audio_info = None;
                    video_info = None;
                }

                let new_pad_state = PadState {
                    caps: Some(caps),
                    audio_info,
                    video_info,
                };

                let mut pad_states = self.pad_states.write().unwrap();
                if agg_pad == &self.sinkpad {
                    pad_states.sinkpad = new_pad_state;
                } else if Some(agg_pad) == self.fallback_sinkpad.read().unwrap().as_ref() {
                    pad_states.fallback_sinkpad = Some(new_pad_state);
                }
                drop(pad_states);

                self.parent_sink_event(agg, agg_pad, event)
            }
            _ => self.parent_sink_event(agg, agg_pad, event),
        }
    }

    fn get_next_time(&self, agg: &gst_base::Aggregator) -> gst::ClockTime {
        // If we have a buffer on the sinkpad then the timeout is always going to be immediately,
        // i.e. 0. We want to output that buffer immediately, no matter what.
        //
        // Otherwise if we have a fallback sinkpad and it has a buffer, then the timeout is going
        // to be its running time. We will then either output the buffer or drop it, depending on
        // its distance from the last sinkpad time
        if self.sinkpad.peek_buffer().is_some() {
            gst_debug!(CAT, obj: agg, "Have buffer on sinkpad, immediate timeout");
            0.into()
        } else if self.sinkpad.is_eos() {
            gst_debug!(CAT, obj: agg, "Sinkpad is EOS, immediate timeout");
            0.into()
        } else if let Some((buffer, fallback_sinkpad)) = self
            .fallback_sinkpad
            .read()
            .unwrap()
            .as_ref()
            .and_then(|p| p.peek_buffer().map(|buffer| (buffer, p)))
        {
            if buffer.get_pts().is_none() {
                gst_error!(CAT, obj: agg, "Only buffers with PTS supported");
                // Trigger aggregate immediately to error out immediately
                return 0.into();
            }

            let segment = match fallback_sinkpad.get_segment().downcast::<gst::ClockTime>() {
                Ok(segment) => segment,
                Err(_) => {
                    gst_error!(CAT, obj: agg, "Only TIME segments supported");
                    // Trigger aggregate immediately to error out immediately
                    return 0.into();
                }
            };

            let running_time = segment.to_running_time(buffer.get_dts_or_pts());
            gst_debug!(
                CAT,
                obj: agg,
                "Have buffer on fallback sinkpad, timeout at {}",
                running_time
            );
            running_time
        } else {
            gst_debug!(CAT, obj: agg, "Have no buffer at all yet");
            gst::CLOCK_TIME_NONE
        }
    }

    // Clip the raw audio/video buffers we have to the segment boundaries to ensure that
    // calculating the running times later works correctly
    fn clip(
        &self,
        agg: &gst_base::Aggregator,
        agg_pad: &gst_base::AggregatorPad,
        mut buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        let segment = match agg_pad.get_segment().downcast::<gst::ClockTime>() {
            Ok(segment) => segment,
            Err(_) => {
                gst_error!(CAT, obj: agg, "Only TIME segments supported");
                return Some(buffer);
            }
        };

        let pts = buffer.get_pts();
        if pts.is_none() {
            gst_error!(CAT, obj: agg, "Only buffers with PTS supported");
            return Some(buffer);
        }

        let pad_states = self.pad_states.read().unwrap();
        let pad_state = if agg_pad == &self.sinkpad {
            &pad_states.sinkpad
        } else if Some(agg_pad) == self.fallback_sinkpad.read().unwrap().as_ref() {
            if let Some(ref pad_state) = pad_states.fallback_sinkpad {
                pad_state
            } else {
                return Some(buffer);
            }
        } else {
            unreachable!()
        };

        if pad_state.audio_info.is_none() && pad_state.video_info.is_none() {
            // No clipping possible for non-raw formats
            return Some(buffer);
        }

        let duration = if buffer.get_duration().is_some() {
            buffer.get_duration()
        } else if let Some(ref audio_info) = pad_state.audio_info {
            gst::SECOND
                .mul_div_floor(
                    buffer.get_size() as u64,
                    audio_info.rate() as u64 * audio_info.bpf() as u64,
                )
                .unwrap()
        } else if let Some(ref video_info) = pad_state.video_info {
            if *video_info.fps().numer() > 0 {
                gst::SECOND
                    .mul_div_floor(
                        *video_info.fps().denom() as u64,
                        *video_info.fps().numer() as u64,
                    )
                    .unwrap()
            } else {
                gst::CLOCK_TIME_NONE
            }
        } else {
            unreachable!()
        };

        gst_debug!(
            CAT,
            obj: agg_pad,
            "Clipping buffer {:?} with PTS {} and duration {}",
            buffer,
            pts,
            duration
        );
        if let Some(ref audio_info) = pad_state.audio_info {
            gst_audio::audio_buffer_clip(
                buffer,
                segment.upcast_ref(),
                audio_info.rate(),
                audio_info.bpf(),
            )
        } else if pad_state.video_info.is_some() {
            segment.clip(pts, pts + duration).map(|(start, stop)| {
                {
                    let buffer = buffer.make_mut();
                    buffer.set_pts(start);
                    buffer.set_dts(start);
                    if duration.is_some() {
                        buffer.set_duration(stop - start);
                    }
                }

                buffer
            })
        } else {
            unreachable!();
        }
    }

    fn aggregate(
        &self,
        agg: &gst_base::Aggregator,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(CAT, obj: agg, "Aggregate called: timeout {}", timeout);

        let (mut buffer, active_caps, pad_change) = self.get_next_buffer(agg, timeout)?;

        let current_src_caps = agg.get_static_pad("src").unwrap().get_current_caps();
        if Some(&active_caps) != current_src_caps.as_ref() {
            gst_info!(
                CAT,
                obj: agg,
                "Caps change from {:?} to {:?}",
                current_src_caps,
                active_caps
            );
            agg.set_src_caps(&active_caps);
        }

        if pad_change {
            agg.notify("active-pad");
            buffer.make_mut().set_flags(gst::BufferFlags::DISCONT);
        }

        gst_debug!(CAT, obj: agg, "Finishing buffer {:?}", buffer);
        agg.finish_buffer(buffer)
    }

    fn negotiate(&self, _agg: &gst_base::Aggregator) -> bool {
        true
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "fallbackswitch",
        gst::Rank::None,
        FallbackSwitch::get_type(),
    )
}
