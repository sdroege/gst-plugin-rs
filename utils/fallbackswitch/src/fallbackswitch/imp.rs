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

use gst::glib;

#[cfg(not(feature = "v1_18"))]
use self::gst_base::prelude::*;
#[cfg(not(feature = "v1_18"))]
use self::gst_base::subclass::prelude::*;
#[cfg(not(feature = "v1_18"))]
use crate::gst_base_compat as gst_base;

#[cfg(feature = "v1_18")]
use gst_base::prelude::*;
#[cfg(feature = "v1_18")]
use gst_base::subclass::prelude::*;

#[cfg(not(feature = "v1_18"))]
use glib::subclass::prelude::*;
#[cfg(not(feature = "v1_18"))]
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_log, gst_warning};

use once_cell::sync::Lazy;

use std::sync::{Mutex, RwLock};

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::GEnum)]
#[repr(u32)]
#[genum(type_name = "GstFallbackSwitchStreamHealth")]
pub(crate) enum StreamHealth {
    #[genum(name = "Data flow is inactive or late", nick = "inactive")]
    Inactive = 0,
    #[genum(name = "Data is currently flowing in the stream", nick = "present")]
    Present = 1,
}

pub struct FallbackSwitch {
    primary_sinkpad: gst_base::AggregatorPad,
    primary_state: RwLock<PadInputState>,

    fallback_sinkpad: RwLock<Option<gst_base::AggregatorPad>>,
    fallback_state: RwLock<PadInputState>,

    active_sinkpad: Mutex<Option<gst::Pad>>,
    output_state: Mutex<OutputState>,
    settings: Mutex<Settings>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbackswitch",
        gst::DebugColorFlags::empty(),
        Some("Fallback switch Element"),
    )
});

#[derive(Debug, Default)]
struct PadOutputState {
    last_sinkpad_time: Option<gst::ClockTime>,
    stream_health: StreamHealth,
}

#[derive(Debug)]
struct OutputState {
    last_output_time: Option<gst::ClockTime>,
    primary: PadOutputState,
    fallback: PadOutputState,
}

#[derive(Debug, Default)]
struct PadInputState {
    caps: Option<gst::Caps>,
    audio_info: Option<gst_audio::AudioInfo>,
    video_info: Option<gst_video::VideoInfo>,
}

const DEFAULT_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(5);
const DEFAULT_AUTO_SWITCH: bool = true;
const DEFAULT_STREAM_HEALTH: StreamHealth = StreamHealth::Inactive;

#[derive(Debug, Clone)]
struct Settings {
    timeout: gst::ClockTime,
    auto_switch: bool,
}

impl Default for StreamHealth {
    fn default() -> Self {
        DEFAULT_STREAM_HEALTH
    }
}

impl Default for OutputState {
    fn default() -> Self {
        OutputState {
            last_output_time: gst::ClockTime::NONE,
            primary: PadOutputState::default(),
            fallback: PadOutputState::default(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            timeout: DEFAULT_TIMEOUT,
            auto_switch: DEFAULT_AUTO_SWITCH,
        }
    }
}

impl OutputState {
    fn health(
        &self,
        settings: &Settings,
        check_primary_pad: bool,
        cur_running_time: impl Into<Option<gst::ClockTime>>,
    ) -> StreamHealth {
        let last_sinkpad_time = if check_primary_pad {
            self.primary.last_sinkpad_time
        } else {
            self.fallback.last_sinkpad_time
        };

        if last_sinkpad_time.is_none() {
            StreamHealth::Inactive
        } else if cur_running_time.into().map_or(false, |cur_running_time| {
            cur_running_time < last_sinkpad_time.expect("checked above") + settings.timeout
        }) {
            StreamHealth::Present
        } else {
            StreamHealth::Inactive
        }
    }

    fn check_health_changes(
        &mut self,
        settings: &Settings,
        backup_pad: &Option<&gst_base::AggregatorPad>,
        preferred_is_primary: bool,
        cur_running_time: impl Into<Option<gst::ClockTime>> + Copy,
    ) -> (bool, bool) {
        let preferred_health = self.health(settings, preferred_is_primary, cur_running_time);
        let backup_health = if backup_pad.is_some() {
            self.health(settings, !preferred_is_primary, cur_running_time)
        } else {
            StreamHealth::Inactive
        };

        if preferred_is_primary {
            let primary_changed = preferred_health != self.primary.stream_health;
            let fallback_changed = backup_health != self.fallback.stream_health;

            self.primary.stream_health = preferred_health;
            self.fallback.stream_health = backup_health;

            (primary_changed, fallback_changed)
        } else {
            let primary_changed = backup_health != self.primary.stream_health;
            let fallback_changed = preferred_health != self.fallback.stream_health;

            self.primary.stream_health = backup_health;
            self.fallback.stream_health = preferred_health;

            (primary_changed, fallback_changed)
        }
    }
}

impl FallbackSwitch {
    fn drain_pad_to_time(
        &self,
        state: &mut OutputState,
        pad: &gst_base::AggregatorPad,
        target_running_time: impl Into<Option<gst::ClockTime>> + Copy,
    ) -> Result<(), gst::FlowError> {
        let segment = pad.segment();

        /* No segment yet - no data */
        if segment.format() == gst::Format::Undefined {
            return Ok(());
        }

        let segment = segment.downcast::<gst::ClockTime>().map_err(|_| {
            gst_error!(CAT, obj: pad, "Only TIME segments supported");
            gst::FlowError::Error
        })?;

        let mut running_time = gst::ClockTime::NONE;

        while let Some(buffer) = pad.peek_buffer() {
            let pts = buffer.dts_or_pts();
            let new_running_time = segment.to_running_time(pts);

            if pts.is_none()
                || new_running_time
                    .zip(target_running_time.into())
                    .map_or(false, |(new, target)| new <= target)
            {
                gst_debug!(CAT, obj: pad, "Dropping trailing buffer {:?}", buffer);
                pad.drop_buffer();
                running_time = new_running_time;
            } else {
                break;
            }
        }
        if running_time.is_some() {
            if pad == &self.primary_sinkpad {
                state.primary.last_sinkpad_time = running_time;
            } else {
                state.fallback.last_sinkpad_time = running_time;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_main_buffer(
        &self,
        agg: &super::FallbackSwitch,
        state: &mut OutputState,
        settings: &Settings,
        mut buffer: gst::Buffer,
        preferred_pad: &gst_base::AggregatorPad,
        backup_pad: &Option<&gst_base::AggregatorPad>,
        cur_running_time: impl Into<Option<gst::ClockTime>>,
    ) -> Result<Option<(gst::Buffer, gst::Caps, bool)>, gst::FlowError> {
        // If we got a buffer on the sinkpad just handle it
        gst_debug!(
            CAT,
            obj: preferred_pad,
            "Got buffer on pad {} - {:?}",
            preferred_pad.name(),
            buffer
        );

        if buffer.pts().is_none() {
            gst_error!(CAT, obj: preferred_pad, "Only buffers with PTS supported");
            return Err(gst::FlowError::Error);
        }

        let segment = preferred_pad
            .segment()
            .downcast::<gst::ClockTime>()
            .map_err(|_| {
                gst_error!(CAT, obj: preferred_pad, "Only TIME segments supported");
                gst::FlowError::Error
            })?;

        let running_time = segment.to_running_time(buffer.dts_or_pts());

        {
            // FIXME: This will not work correctly for negative DTS
            let buffer = buffer.make_mut();
            buffer.set_pts(segment.to_running_time(buffer.pts()));
            buffer.set_dts(segment.to_running_time(buffer.dts()));
        }

        if preferred_pad == &self.primary_sinkpad {
            state.primary.last_sinkpad_time = running_time;
        } else {
            state.fallback.last_sinkpad_time = running_time;
        }

        let cur_running_time = cur_running_time.into();
        let (is_late, deadline) = cur_running_time
            .zip(agg.latency())
            .zip(running_time)
            .map_or(
                (false, None),
                |((cur_running_time, latency), running_time)| {
                    let dealine = running_time + latency + 40 * gst::ClockTime::MSECOND;
                    (cur_running_time > dealine, Some(dealine))
                },
            );

        if is_late {
            gst_debug!(
                CAT,
                obj: preferred_pad,
                "Buffer is too late: {} > {}",
                cur_running_time.display(),
                deadline.display(),
            );

            if state.last_output_time.zip(running_time).map_or(
                false,
                |(last_output_time, running_time)| {
                    last_output_time + settings.timeout <= running_time
                },
            ) {
                /* This buffer arrived too late - we either already switched
                 * to the other pad or there's no point outputting this anyway */
                gst_debug!(
                    CAT,
                    obj: preferred_pad,
                    "Buffer is too late and timeout reached: {} + {} <= {}",
                    state.last_output_time.display(),
                    settings.timeout,
                    running_time.display(),
                );

                return Ok(None);
            }
        }

        let mut active_sinkpad = self.active_sinkpad.lock().unwrap();
        let pad_change = settings.auto_switch
            && active_sinkpad.as_ref() != Some(preferred_pad.upcast_ref::<gst::Pad>());

        if pad_change {
            if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                gst_info!(
                    CAT,
                    obj: preferred_pad,
                    "Can't change back to sinkpad {}, waiting for keyframe",
                    preferred_pad.name()
                );
                preferred_pad.push_event(
                    gst_video::UpstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build(),
                );
                return Ok(None);
            }

            gst_info!(CAT, obj: preferred_pad, "Active pad changed to sinkpad");
            *active_sinkpad = Some(preferred_pad.clone().upcast());
        }
        drop(active_sinkpad);

        if !is_late || state.last_output_time.is_none() {
            state.last_output_time = running_time;
        }

        let active_caps = if preferred_pad == &self.primary_sinkpad {
            let pad_state = self.primary_state.read().unwrap();
            pad_state.caps.as_ref().unwrap().clone()
        } else {
            let pad_state = self.fallback_state.read().unwrap();
            pad_state.caps.as_ref().unwrap().clone()
        };

        // Drop all older buffers from the fallback sinkpad
        if let Some(backup_pad) = backup_pad {
            self.drain_pad_to_time(state, &backup_pad, state.last_output_time)?;
        }

        Ok(Some((buffer, active_caps, pad_change)))
    }

    fn backup_buffer(
        &self,
        state: &mut OutputState,
        settings: &Settings,
        backup_pad: &gst_base::AggregatorPad,
    ) -> Result<(gst::Buffer, gst::Caps, bool), gst::FlowError> {
        // If we have a fallback sinkpad and timeout, try to get a fallback buffer from here
        // and drop all too old buffers in the process
        loop {
            let mut buffer = backup_pad
                .pop_buffer()
                .ok_or(gst_base::AGGREGATOR_FLOW_NEED_DATA)?;

            gst_debug!(
                CAT,
                obj: backup_pad,
                "Got buffer on fallback sinkpad {:?}",
                buffer
            );

            if buffer.pts().is_none() {
                gst_error!(CAT, obj: backup_pad, "Only buffers with PTS supported");
                return Err(gst::FlowError::Error);
            }

            let backup_segment =
                backup_pad
                    .segment()
                    .downcast::<gst::ClockTime>()
                    .map_err(|_| {
                        gst_error!(CAT, obj: backup_pad, "Only TIME segments supported");
                        gst::FlowError::Error
                    })?;
            let running_time = backup_segment.to_running_time(buffer.dts_or_pts());

            {
                // FIXME: This will not work correctly for negative DTS
                let buffer = buffer.make_mut();
                buffer.set_pts(backup_segment.to_running_time(buffer.pts()));
                buffer.set_dts(backup_segment.to_running_time(buffer.dts()));
            }

            // If we never had a real buffer, initialize with the running time of the fallback
            // sinkpad so that we still output fallback buffers after the timeout
            if state.last_output_time.is_none() {
                state.last_output_time = running_time;
            }
            if backup_pad == &self.primary_sinkpad {
                state.primary.last_sinkpad_time = running_time;
            } else {
                state.fallback.last_sinkpad_time = running_time;
            }

            // Get the next one if this one is before the timeout
            if state.last_output_time.zip(running_time).map_or(
                false,
                |(last_output_time, running_time)| {
                    last_output_time + settings.timeout > running_time
                },
            ) {
                gst_debug!(
                    CAT,
                    obj: backup_pad,
                    "Timeout not reached yet: {} + {} > {}",
                    state.last_output_time.display(),
                    settings.timeout,
                    running_time.display(),
                );

                continue;
            }

            gst_debug!(
                CAT,
                obj: backup_pad,
                "Timeout reached: {} + {} <= {}",
                state.last_output_time.display(),
                settings.timeout,
                running_time.display(),
            );

            let mut active_sinkpad = self.active_sinkpad.lock().unwrap();
            let pad_change = settings.auto_switch
                && active_sinkpad.as_ref() != Some(backup_pad.upcast_ref::<gst::Pad>());
            if pad_change {
                if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    gst_info!(
                        CAT,
                        obj: backup_pad,
                        "Can't change to sinkpad {} yet, waiting for keyframe",
                        backup_pad.name()
                    );
                    backup_pad.push_event(
                        gst_video::UpstreamForceKeyUnitEvent::builder()
                            .all_headers(true)
                            .build(),
                    );
                    continue;
                }

                gst_info!(
                    CAT,
                    obj: backup_pad,
                    "Active pad changed to fallback sinkpad"
                );
                *active_sinkpad = Some(backup_pad.clone().upcast());
            }
            drop(active_sinkpad);

            let active_caps = if backup_pad == &self.primary_sinkpad {
                let pad_state = self.primary_state.read().unwrap();
                pad_state.caps.as_ref().unwrap().clone()
            } else {
                let pad_state = self.fallback_state.read().unwrap();
                pad_state.caps.as_ref().unwrap().clone()
            };

            break Ok((buffer, active_caps, pad_change));
        }
    }

    #[allow(clippy::type_complexity)]
    fn next_buffer(
        &self,
        agg: &super::FallbackSwitch,
        timeout: bool,
    ) -> (
        Result<
            (
                gst::Buffer, // Next buffer from the chosen pad
                gst::Caps,   // Caps for the buffer
                bool,        // If the input pad changed to/from primary<->fallback
            ),
            gst::FlowError,
        >,
        (
            bool, // If the health of the primary pad changed
            bool, // If the health of the fallback pad changed
        ),
    ) {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.output_state.lock().unwrap();

        gst_debug!(CAT, obj: agg, "Aggregate called: timeout {}", timeout);

        if self.primary_sinkpad.is_eos() {
            gst_log!(CAT, obj: agg, "Sinkpad is EOS");
            return (Err(gst::FlowError::Eos), (false, false));
        }

        /* Choose which pad we check first */
        let active_sinkpad = self.active_sinkpad.lock().unwrap();
        let prefer_primary = settings.auto_switch
            || active_sinkpad.is_none()
            || active_sinkpad.as_ref() == Some(self.primary_sinkpad.upcast_ref::<gst::Pad>());
        drop(active_sinkpad);

        let fallback_sinkpad = self.fallback_sinkpad.read().unwrap();

        let (preferred_pad, backup_pad) = if prefer_primary {
            (&self.primary_sinkpad, fallback_sinkpad.as_ref())
        } else {
            (
                fallback_sinkpad.as_ref().unwrap(),
                Some(&self.primary_sinkpad),
            )
        };

        let clock = agg.clock();
        let base_time = agg.base_time();

        let cur_running_time = if let Some(clock) = clock {
            clock
                .time()
                .zip(base_time)
                .and_then(|(time, base_time)| time.checked_sub(base_time))
        } else {
            gst::ClockTime::NONE
        };

        /* See if there's a buffer on the preferred pad and output that */
        if let Some(buffer) = preferred_pad.pop_buffer() {
            match self.handle_main_buffer(
                agg,
                &mut *state,
                &settings,
                buffer,
                preferred_pad,
                &backup_pad,
                cur_running_time,
            ) {
                Ok(Some(res)) => {
                    return (
                        Ok(res),
                        state.check_health_changes(
                            &settings,
                            &backup_pad,
                            prefer_primary,
                            cur_running_time,
                        ),
                    )
                }
                Err(e) => {
                    return (
                        Err(e),
                        state.check_health_changes(
                            &settings,
                            &backup_pad,
                            prefer_primary,
                            cur_running_time,
                        ),
                    )
                }
                _ => (),
            }
        }

        /* If we can't auto-switch, then can't fetch anything from the backup pad */
        if !settings.auto_switch {
            /* Not switching, but backup pad needs draining of late buffers still */
            gst_log!(
                CAT,
                obj: agg,
                "No primary buffer, but can't autoswitch - draining backup pad"
            );
            if let Some(backup_pad) = &backup_pad {
                if let Err(e) = self.drain_pad_to_time(&mut *state, &backup_pad, cur_running_time) {
                    return (
                        Err(e),
                        state.check_health_changes(
                            &settings,
                            &Some(backup_pad),
                            prefer_primary,
                            cur_running_time,
                        ),
                    );
                }
            }

            return (
                Err(gst_base::AGGREGATOR_FLOW_NEED_DATA),
                state.check_health_changes(
                    &settings,
                    &backup_pad,
                    prefer_primary,
                    cur_running_time,
                ),
            );
        }

        if let (false, Some(backup_pad)) = (timeout, &backup_pad) {
            gst_debug!(CAT, obj: agg, "Have fallback sinkpad but no timeout yet");
            (
                Err(gst_base::AGGREGATOR_FLOW_NEED_DATA),
                state.check_health_changes(
                    &settings,
                    &Some(backup_pad),
                    prefer_primary,
                    cur_running_time,
                ),
            )
        } else if let (true, Some(backup_pad)) = (timeout, &backup_pad) {
            (
                self.backup_buffer(&mut *state, &settings, backup_pad),
                state.check_health_changes(
                    &settings,
                    &Some(backup_pad),
                    prefer_primary,
                    cur_running_time,
                ),
            )
        } else {
            // Otherwise there's not much we can do at this point
            gst_debug!(
                CAT,
                obj: agg,
                "Got no buffer on sinkpad and have no fallback sinkpad"
            );
            (
                Err(gst_base::AGGREGATOR_FLOW_NEED_DATA),
                state.check_health_changes(
                    &settings,
                    &backup_pad,
                    prefer_primary,
                    cur_running_time,
                ),
            )
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FallbackSwitch {
    const NAME: &'static str = "FallbackSwitch";
    type Type = super::FallbackSwitch;
    type ParentType = gst_base::Aggregator;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad =
            gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ, Some("sink")).build();

        Self {
            primary_sinkpad: sinkpad,
            primary_state: RwLock::new(PadInputState::default()),
            fallback_sinkpad: RwLock::new(None),
            fallback_state: RwLock::new(PadInputState::default()),
            active_sinkpad: Mutex::new(None),
            output_state: Mutex::new(OutputState::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for FallbackSwitch {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_uint64(
                    "timeout",
                    "Timeout",
                    "Timeout in nanoseconds",
                    0,
                    std::u64::MAX - 1,
                    DEFAULT_TIMEOUT.nseconds() as u64,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_object(
                    "active-pad",
                    "Active Pad",
                    "Currently active pad. Writes are ignored if auto-switch=true",
                    gst::Pad::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_boolean(
                    "auto-switch",
                    "Automatically switch pads",
                    "Automatically switch pads (If true, prefer primary sink, otherwise manual selection via the active-pad property)",
                    DEFAULT_AUTO_SWITCH,
                    glib::ParamFlags::READWRITE| gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_enum(
                    "primary-health",
                    "Primary stream state",
                    "Reports the health of the primary stream on the sink pad",
                    StreamHealth::static_type(),
                    DEFAULT_STREAM_HEALTH as i32,
                    glib::ParamFlags::READABLE,
                ),
                glib::ParamSpec::new_enum(
                    "fallback-health",
                    "Fallback stream state",
                    "Reports the health of the fallback stream on the fallback_sink pad",
                    StreamHealth::static_type(),
                    DEFAULT_STREAM_HEALTH as i32,
                    glib::ParamFlags::READABLE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.primary_sinkpad).unwrap();
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing timeout from {} to {}",
                    settings.timeout,
                    timeout
                );
                settings.timeout = timeout;
                drop(settings);
            }
            "active-pad" => {
                let settings = self.settings.lock().unwrap();
                if settings.auto_switch {
                    gst_warning!(
                        CAT,
                        obj: obj,
                        "active-pad property setting ignored, because auto-switch=true"
                    );
                } else {
                    let active_pad = value
                        .get::<Option<gst::Pad>>()
                        .expect("type checked upstream");
                    /* Trigger a pad switch if needed */
                    let mut cur_active_pad = self.active_sinkpad.lock().unwrap();
                    if *cur_active_pad != active_pad {
                        *cur_active_pad = active_pad;
                    }
                    drop(cur_active_pad);
                }
                drop(settings);
            }
            "auto-switch" => {
                let mut settings = self.settings.lock().unwrap();
                settings.auto_switch = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            "active-pad" => {
                let active_pad = self.active_sinkpad.lock().unwrap().clone();
                active_pad.to_value()
            }
            "auto-switch" => {
                let settings = self.settings.lock().unwrap();
                settings.auto_switch.to_value()
            }
            "primary-health" => {
                let state = self.output_state.lock().unwrap();
                state.primary.stream_health.to_value()
            }
            "fallback-health" => {
                let state = self.output_state.lock().unwrap();
                state.fallback.stream_health.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for FallbackSwitch {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Fallback Switch",
                "Generic",
                "Allows switching to a fallback input after a given timeout",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::with_gtype(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let fallbacksink_pad_template = gst::PadTemplate::with_gtype(
                "fallback_sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            vec![
                src_pad_template,
                sink_pad_template,
                fallbacksink_pad_template,
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        templ: &gst::PadTemplate,
        name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let fallback_sink_templ = element.pad_template("fallback_sink").unwrap();
        if templ != &fallback_sink_templ
            || (name.is_some() && name.as_deref() != Some("fallback_sink"))
        {
            gst_error!(CAT, obj: element, "Wrong pad template or name");
            return None;
        }

        let mut fallback_sinkpad = self.fallback_sinkpad.write().unwrap();
        if fallback_sinkpad.is_some() {
            gst_error!(CAT, obj: element, "Already have a fallback sinkpad");
            return None;
        }

        let sinkpad = gst::PadBuilder::<gst_base::AggregatorPad>::from_template(
            &templ,
            Some("fallback_sink"),
        )
        .build();

        *fallback_sinkpad = Some(sinkpad.clone());
        drop(fallback_sinkpad);

        let mut state = self.output_state.lock().unwrap();
        state.fallback = PadOutputState::default();
        drop(state);

        element.add_pad(&sinkpad).unwrap();

        Some(sinkpad.upcast())
    }

    fn release_pad(&self, element: &Self::Type, pad: &gst::Pad) {
        let mut fallback_sinkpad = self.fallback_sinkpad.write().unwrap();

        if fallback_sinkpad.as_ref().map(|p| p.upcast_ref()) == Some(pad) {
            *fallback_sinkpad = None;
            drop(fallback_sinkpad);
            element.remove_pad(pad).unwrap();
            gst_debug!(CAT, obj: element, "Removed fallback sinkpad {:?}", pad);
        }
        *self.fallback_state.write().unwrap() = PadInputState::default();
        *self.active_sinkpad.lock().unwrap() = None;
    }
}

impl AggregatorImpl for FallbackSwitch {
    fn start(&self, _agg: &Self::Type) -> Result<(), gst::ErrorMessage> {
        *self.output_state.lock().unwrap() = OutputState::default();

        *self.primary_state.write().unwrap() = PadInputState::default();
        *self.fallback_state.write().unwrap() = PadInputState::default();

        Ok(())
    }

    fn stop(&self, _agg: &Self::Type) -> Result<(), gst::ErrorMessage> {
        *self.active_sinkpad.lock().unwrap() = None;

        Ok(())
    }

    fn sink_event_pre_queue(
        &self,
        agg: &Self::Type,
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
        agg: &Self::Type,
        agg_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Caps(caps) => {
                let caps = caps.caps_owned();
                gst_debug!(CAT, obj: agg_pad, "Received caps {}", caps);

                let audio_info;
                let video_info;
                if caps.structure(0).unwrap().name() == "audio/x-raw" {
                    audio_info = gst_audio::AudioInfo::from_caps(&caps).ok();
                    video_info = None;
                } else if caps.structure(0).unwrap().name() == "video/x-raw" {
                    audio_info = None;
                    video_info = gst_video::VideoInfo::from_caps(&caps).ok();
                } else {
                    audio_info = None;
                    video_info = None;
                }

                let new_pad_state = PadInputState {
                    caps: Some(caps),
                    audio_info,
                    video_info,
                };

                if agg_pad == &self.primary_sinkpad {
                    *self.primary_state.write().unwrap() = new_pad_state;
                } else if Some(agg_pad) == self.fallback_sinkpad.read().unwrap().as_ref() {
                    *self.fallback_state.write().unwrap() = new_pad_state;
                }

                self.parent_sink_event(agg, agg_pad, event)
            }
            _ => self.parent_sink_event(agg, agg_pad, event),
        }
    }

    fn next_time(&self, agg: &Self::Type) -> Option<gst::ClockTime> {
        /* At each iteration, we have a preferred pad and a backup pad. If autoswitch is true,
         * the sinkpad is always preferred, otherwise it's the active sinkpad as set by the app.
         * The backup pad is the other one (may be None if there's no fallback pad yet).
         *
         * If we have a buffer on the preferred pad then the timeout is always going to be immediately,
         * i.e. 0. We want to output that buffer immediately, no matter what.
         *
         * Otherwise if we have a backup sinkpad and it has a buffer, then the timeout is going
         * to be that buffer's running time. We will then either output the buffer or drop it, depending on
         * its distance from the last output time
         */
        let settings = self.settings.lock().unwrap();
        let active_sinkpad = self.active_sinkpad.lock().unwrap();
        let fallback_sinkpad = self.fallback_sinkpad.read().unwrap();

        let prefer_primary = settings.auto_switch
            || active_sinkpad.is_none()
            || active_sinkpad.as_ref() == Some(self.primary_sinkpad.upcast_ref::<gst::Pad>());

        let (preferred_pad, backup_pad) = if prefer_primary {
            (&self.primary_sinkpad, fallback_sinkpad.as_ref())
        } else {
            (
                fallback_sinkpad.as_ref().unwrap(),
                Some(&self.primary_sinkpad),
            )
        };

        if preferred_pad.peek_buffer().is_some() {
            gst_debug!(
                CAT,
                obj: agg,
                "Have buffer on sinkpad {}, immediate timeout",
                preferred_pad.name()
            );
            Some(gst::ClockTime::ZERO)
        } else if self.primary_sinkpad.is_eos() {
            gst_debug!(CAT, obj: agg, "Sinkpad is EOS, immediate timeout");
            Some(gst::ClockTime::ZERO)
        } else if let Some((buffer, backup_sinkpad)) = backup_pad
            .as_ref()
            .and_then(|p| p.peek_buffer().map(|buffer| (buffer, p)))
        {
            if buffer.pts().is_none() {
                gst_error!(CAT, obj: agg, "Only buffers with PTS supported");
                // Trigger aggregate immediately to error out immediately
                return Some(gst::ClockTime::ZERO);
            }

            let segment = match backup_sinkpad.segment().downcast::<gst::ClockTime>() {
                Ok(segment) => segment,
                Err(_) => {
                    gst_error!(CAT, obj: agg, "Only TIME segments supported");
                    // Trigger aggregate immediately to error out immediately
                    return Some(gst::ClockTime::ZERO);
                }
            };

            let running_time = segment.to_running_time(buffer.dts_or_pts());
            gst_debug!(
                CAT,
                obj: agg,
                "Have buffer on {} pad, timeout at {}",
                backup_sinkpad.name(),
                running_time.display(),
            );
            running_time
        } else {
            gst_debug!(CAT, obj: agg, "No buffer available on either input");
            gst::ClockTime::NONE
        }
    }

    // Clip the raw audio/video buffers we have to the segment boundaries to ensure that
    // calculating the running times later works correctly
    fn clip(
        &self,
        agg: &Self::Type,
        agg_pad: &gst_base::AggregatorPad,
        mut buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        let segment = match agg_pad.segment().downcast::<gst::ClockTime>() {
            Ok(segment) => segment,
            Err(_) => {
                gst_error!(CAT, obj: agg, "Only TIME segments supported");
                return Some(buffer);
            }
        };

        let pts = buffer.pts();
        if pts.is_none() {
            gst_error!(CAT, obj: agg, "Only buffers with PTS supported");
            return Some(buffer);
        }

        let primary_state = self.primary_state.read().unwrap();
        let fallback_state = self.fallback_state.read().unwrap();

        let pad_state = if agg_pad == &self.primary_sinkpad {
            &primary_state
        } else if Some(agg_pad) == self.fallback_sinkpad.read().unwrap().as_ref() {
            &fallback_state
        } else {
            unreachable!()
        };

        if pad_state.audio_info.is_none() && pad_state.video_info.is_none() {
            // No clipping possible for non-raw formats
            return Some(buffer);
        }

        let duration = if let Some(duration) = buffer.duration() {
            Some(duration)
        } else if let Some(ref audio_info) = pad_state.audio_info {
            gst::ClockTime::SECOND.mul_div_floor(
                buffer.size() as u64,
                audio_info.rate() as u64 * audio_info.bpf() as u64,
            )
        } else if let Some(ref video_info) = pad_state.video_info {
            if *video_info.fps().numer() > 0 {
                gst::ClockTime::SECOND.mul_div_floor(
                    *video_info.fps().denom() as u64,
                    *video_info.fps().numer() as u64,
                )
            } else {
                gst::ClockTime::NONE
            }
        } else {
            unreachable!()
        };

        gst_debug!(
            CAT,
            obj: agg_pad,
            "Clipping buffer {:?} with PTS {} and duration {}",
            buffer,
            pts.display(),
            duration.display(),
        );
        if let Some(ref audio_info) = pad_state.audio_info {
            gst_audio::audio_buffer_clip(
                buffer,
                segment.upcast_ref(),
                audio_info.rate(),
                audio_info.bpf(),
            )
        } else if pad_state.video_info.is_some() {
            let stop = pts.zip(duration).map(|(pts, duration)| pts + duration);
            segment.clip(pts, stop).map(|(start, stop)| {
                {
                    let buffer = buffer.make_mut();
                    buffer.set_pts(start);
                    if duration.is_some() {
                        buffer.set_duration(
                            stop.zip(start)
                                .and_then(|(stop, start)| stop.checked_sub(start)),
                        );
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
        agg: &Self::Type,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(CAT, obj: agg, "Aggregate called: timeout {}", timeout);

        let (res, (primary_health_change, fallback_health_change)) = self.next_buffer(agg, timeout);

        if primary_health_change {
            gst_debug!(
                CAT,
                obj: agg,
                "Primary pad health now {}",
                &primary_health_change
            );
            agg.notify("primary-health");
        }
        if fallback_health_change {
            gst_debug!(
                CAT,
                obj: agg,
                "Fallback pad health now {}",
                &fallback_health_change
            );
            agg.notify("fallback-health");
        }

        let (mut buffer, active_caps, pad_change) = res?;

        let current_src_caps = agg.static_pad("src").unwrap().current_caps();
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

    fn negotiate(&self, _agg: &Self::Type) -> bool {
        true
    }
}
