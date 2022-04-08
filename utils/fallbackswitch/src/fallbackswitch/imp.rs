// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
// Copyright (C) 2021 Jan Schmidt <jan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{debug, log, trace};

use once_cell::sync::Lazy;

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

const PROP_PRIORITY: &str = "priority";
const PROP_IS_HEALTHY: &str = "is-healthy";

const PROP_ACTIVE_PAD: &str = "active-pad";
const PROP_AUTO_SWITCH: &str = "auto-switch";
const PROP_IMMEDIATE_FALLBACK: &str = "immediate-fallback";
const PROP_LATENCY: &str = "latency";
const PROP_MIN_UPSTREAM_LATENCY: &str = "min-upstream-latency";
const PROP_TIMEOUT: &str = "timeout";

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbackswitch",
        gst::DebugColorFlags::empty(),
        Some("Automatic priority-based input selector"),
    )
});

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum CapsInfo {
    None,
    Audio(gst_audio::AudioInfo),
    Video(gst_video::VideoInfo),
}

#[derive(Clone, Debug)]
struct Settings {
    timeout: gst::ClockTime,
    latency: gst::ClockTime,
    min_upstream_latency: gst::ClockTime,
    immediate_fallback: bool,
    auto_switch: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            timeout: gst::ClockTime::SECOND,
            latency: gst::ClockTime::ZERO,
            min_upstream_latency: gst::ClockTime::ZERO,
            immediate_fallback: false,
            auto_switch: true,
        }
    }
}

#[derive(Debug)]
struct State {
    active_sinkpad: Option<super::FallbackSwitchSinkPad>,
    upstream_latency: gst::ClockTime,
    timed_out: bool,
    switched_pad: bool,
    discont_pending: bool,
    first: bool,

    output_running_time: Option<gst::ClockTime>,

    timeout_running_time: gst::ClockTime,
    timeout_clock_id: Option<gst::ClockId>,
}

impl Default for State {
    fn default() -> State {
        State {
            active_sinkpad: None,
            upstream_latency: gst::ClockTime::ZERO,
            timed_out: false,
            switched_pad: false,
            discont_pending: true,
            first: true,

            output_running_time: None,

            timeout_running_time: gst::ClockTime::ZERO,
            timeout_clock_id: None,
        }
    }
}

impl State {
    fn cancel_timeout(&mut self) {
        /* clear any previous timeout */
        if let Some(clock_id) = self.timeout_clock_id.take() {
            clock_id.unschedule();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel_timeout();
    }
}

#[derive(Debug)]
pub struct FallbackSwitchSinkPad {
    state: Mutex<SinkState>,
    settings: Mutex<SinkSettings>,
}

#[glib::object_subclass]
impl ObjectSubclass for FallbackSwitchSinkPad {
    const NAME: &'static str = "FallbackSwitchSinkPad";
    type Type = super::FallbackSwitchSinkPad;
    type ParentType = gst::Pad;

    fn new() -> Self {
        Self {
            state: Mutex::new(SinkState::default()),
            settings: Mutex::new(SinkSettings::default()),
        }
    }
}

impl GstObjectImpl for FallbackSwitchSinkPad {}

impl ObjectImpl for FallbackSwitchSinkPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::new(
                    PROP_PRIORITY,
                    "Stream Priority",
                    "Selection priority for this stream",
                    0,
                    std::u32::MAX,
                    SinkSettings::default().priority,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecBoolean::new(
                    PROP_IS_HEALTHY,
                    "Stream Health",
                    "Whether this stream is healthy",
                    false,
                    glib::ParamFlags::READABLE,
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
            PROP_PRIORITY => {
                let mut settings = self.settings.lock();
                let priority = value.get().expect("type checked upstream");
                settings.priority = priority;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            PROP_PRIORITY => {
                let settings = self.settings.lock();
                settings.priority.to_value()
            }
            PROP_IS_HEALTHY => {
                let state = self.state.lock();
                state.is_healthy.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl PadImpl for FallbackSwitchSinkPad {}

impl FallbackSwitchSinkPad {}

#[derive(Clone, Debug, Default)]
struct SinkSettings {
    priority: u32,
}

#[derive(Debug)]
struct SinkState {
    is_healthy: bool,

    segment: gst::FormattedSegment<gst::ClockTime>,
    caps_info: CapsInfo,

    current_running_time: Option<gst::ClockTime>,
    eos: bool,
    flushing: bool,
    clock_id: Option<gst::SingleShotClockId>,
}

impl Default for SinkState {
    fn default() -> Self {
        Self {
            is_healthy: false,

            segment: gst::FormattedSegment::new(),
            caps_info: CapsInfo::None,

            current_running_time: gst::ClockTime::NONE,
            eos: false,
            flushing: false,
            clock_id: None,
        }
    }
}

impl SinkState {
    fn flush_start(&mut self) {
        self.flushing = true;
        if let Some(clock_id) = self.clock_id.take() {
            clock_id.unschedule();
        }
    }
    fn cancel_wait(&mut self) {
        if let Some(clock_id) = self.clock_id.take() {
            clock_id.unschedule();
        }
    }
    fn reset(&mut self) {
        self.flushing = false;
        self.eos = false;
        self.caps_info = CapsInfo::None;
    }

    fn get_sync_time(
        &self,
        buffer: &gst::Buffer,
    ) -> (Option<gst::ClockTime>, Option<gst::ClockTime>) {
        let last_ts = self.current_running_time;
        let duration = buffer.duration().unwrap_or(gst::ClockTime::ZERO);

        let start_ts = match buffer.dts_or_pts() {
            Some(ts) => ts,
            None => return (last_ts, last_ts),
        };
        let end_ts = start_ts.saturating_add(duration);

        let (start_ts, end_ts) = match self.segment.clip(start_ts, end_ts) {
            Some((start_ts, end_ts)) => (start_ts, end_ts),
            None => return (None, None),
        };
        let start_ts = self.segment.to_running_time(start_ts);
        let end_ts = self.segment.to_running_time(end_ts);

        (start_ts, end_ts)
    }

    fn schedule_clock(
        &mut self,
        element: &super::FallbackSwitch,
        running_time: Option<gst::ClockTime>,
        extra_time: gst::ClockTime,
    ) -> Option<gst::SingleShotClockId> {
        let clock = match element.clock() {
            None => return None,
            Some(clock) => clock,
        };

        let base_time = element.base_time();
        let wait_until = match running_time
            .zip(base_time)
            .map(|(running_time, base_time)| running_time + base_time)
        {
            Some(wait_until) => wait_until,
            None => return None,
        };
        let wait_until = wait_until.saturating_add(extra_time);

        let now = clock.time();

        /* If the buffer is already late, skip the clock wait */
        if now.map_or(true, |now| wait_until < now) {
            debug!(
                CAT,
                obj: element,
                "Skipping buffer wait until {} - clock already {:?}",
                wait_until,
                now
            );
            return None;
        }

        debug!(
            CAT,
            obj: element,
            "Scheduling buffer wait until {} = {:?} + extra {:?} + base time {:?}",
            wait_until,
            running_time,
            extra_time,
            base_time
        );

        let clock_id = clock.new_single_shot_id(wait_until);
        self.clock_id = Some(clock_id.clone());
        Some(clock_id)
    }

    fn is_healthy(&self, state: &State, settings: &Settings) -> bool {
        match self.current_running_time {
            Some(current_running_time) => {
                current_running_time >= state.timeout_running_time.saturating_sub(settings.timeout)
                    && current_running_time <= state.timeout_running_time
            }
            None => false,
        }
    }
}

#[derive(Debug)]
pub struct FallbackSwitch {
    state: Mutex<State>,
    settings: Mutex<Settings>,
    src_pad: gst::Pad,
    sink_pad_serial: AtomicU32,
}

impl GstObjectImpl for FallbackSwitch {}

impl FallbackSwitch {
    fn set_active_pad(&self, state: &mut State, pad: &super::FallbackSwitchSinkPad) {
        if state.active_sinkpad.as_ref() == Some(pad) {
            return;
        };

        state.active_sinkpad = Some(pad.clone());
        state.switched_pad = true;
        state.discont_pending = true;

        let mut pad_state = pad.imp().state.lock();
        pad_state.cancel_wait();
        drop(pad_state);

        debug!(CAT, obj: pad, "Now active pad");
    }

    fn handle_timeout(
        &self,
        element: &super::FallbackSwitch,
        state: &mut State,
        settings: &Settings,
    ) {
        debug!(
            CAT,
            obj: element,
            "timeout fired - looking for a pad to switch to"
        );

        /* Advance the output running time to this timeout */
        state.output_running_time = Some(state.timeout_running_time);

        let mut best_priority = 0u32;
        let mut best_pad = None;

        for pad in element.sink_pads() {
            /* Don't consider the active sinkpad */
            let pad = pad.downcast_ref::<super::FallbackSwitchSinkPad>().unwrap();
            let pad_imp = FallbackSwitchSinkPad::from_instance(pad);
            if Some(pad) == state.active_sinkpad.as_ref() {
                continue;
            }
            let pad_state = pad_imp.state.lock();
            let pad_settings = pad_imp.settings.lock().clone();
            #[allow(clippy::collapsible_if)]
            /* If this pad has data that arrived within the 'timeout' window
             * before the timeout fired, we can switch to it */
            if pad_state.is_healthy(state, settings) {
                if best_pad.is_none() || pad_settings.priority < best_priority {
                    best_pad = Some(pad.clone());
                    best_priority = pad_settings.priority;
                }
            }
        }

        if let Some(best_pad) = best_pad {
            debug!(
                CAT,
                obj: element,
                "Found viable pad to switch to: {:?}",
                best_pad
            );
            self.set_active_pad(state, &best_pad)
        } else {
            state.timed_out = true;
        }
    }

    fn on_timeout(
        &self,
        element: &super::FallbackSwitch,
        clock_id: &gst::ClockId,
        settings: &Settings,
    ) {
        let mut state = self.state.lock();

        if state.timeout_clock_id.as_ref() != Some(clock_id) {
            /* Timeout fired late, ignore it. */
            debug!(CAT, obj: element, "Late timeout callback. Ignoring");
            return;
        }
        self.handle_timeout(element, &mut state, settings);
    }

    fn cancel_waits(&self, element: &super::FallbackSwitch) {
        for pad in element.sink_pads() {
            let sink_pad = FallbackSwitchSinkPad::from_instance(pad.downcast_ref().unwrap());
            let mut pad_state = sink_pad.state.lock();
            pad_state.cancel_wait();
        }
    }

    fn schedule_timeout(
        &self,
        element: &super::FallbackSwitch,
        state: &mut State,
        settings: &Settings,
        running_time: gst::ClockTime,
    ) {
        /* clear any previous timeout */
        if let Some(clock_id) = state.timeout_clock_id.take() {
            clock_id.unschedule();
        }

        let clock = match element.clock() {
            None => return,
            Some(clock) => clock,
        };

        let timeout_running_time = running_time
            .saturating_add(state.upstream_latency + settings.timeout + settings.latency);

        let base_time = element.base_time();

        let wait_until = match Some(timeout_running_time)
            .zip(base_time)
            .map(|(running_time, base_time)| running_time + base_time)
        {
            Some(wait_until) => wait_until,
            None => return,
        };

        state.timeout_running_time = timeout_running_time;

        /* If we're already running behind, fire the timeout immediately */
        let now = clock.time();
        if now.map_or(false, |now| wait_until <= now) {
            self.handle_timeout(element, state, settings);
            return;
        }

        debug!(CAT, obj: element, "Scheduling timeout for {}", wait_until);
        let timeout_id = clock.new_single_shot_id(wait_until);

        state.timeout_clock_id = Some(timeout_id.clone().into());
        state.timed_out = false;

        let element_weak = element.downgrade();
        timeout_id
            .wait_async(move |_clock, _time, clock_id| {
                let element = match element_weak.upgrade() {
                    None => return,
                    Some(element) => element,
                };
                let fallbackswitch = FallbackSwitch::from_instance(&element);
                let settings = fallbackswitch.settings.lock().clone();
                fallbackswitch.on_timeout(&element, clock_id, &settings);
            })
            .expect("Failed to wait async");
    }

    fn sink_chain(
        &self,
        pad: &super::FallbackSwitchSinkPad,
        element: &super::FallbackSwitch,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        /* There are 4 cases coming in:
         *  1. This is not the active pad but is higher priority:
         *    - become the active pad, then goto 4.
         *  2. This is not the active pad, but the output timed out due to all pads running
         *     late.
         *    - become the active pad, then goto 4.
         *  3. This is not the active pad, but might become the active pad
         *    - Wait for the buffer end time (or buffer start time + timeout if there's no
         *    duration). If we get woken early, and became the active pad, then output the
         *    buffer.
         *  4. This is the active pad:
         *    - sleep until the buffer running time, then check if we're still active
         */

        /* First see if we should become the active pad */
        let mut state = self.state.lock();
        let settings = self.settings.lock().clone();
        let pad = pad.downcast_ref().unwrap();
        let pad_imp = FallbackSwitchSinkPad::from_instance(pad);

        if state.active_sinkpad.as_ref() != Some(pad) && settings.auto_switch {
            let pad_settings = pad_imp.settings.lock().clone();
            let mut switch_to_pad = state.timed_out;

            switch_to_pad |= if let Some(active_sinkpad) = &state.active_sinkpad {
                let active_sinkpad_imp = active_sinkpad.imp();
                let active_pad_settings = active_sinkpad_imp.settings.lock().clone();
                (pad_settings.priority < active_pad_settings.priority)
                    || (state.first && settings.immediate_fallback)
            } else {
                match settings.immediate_fallback {
                    true => true,
                    false => pad_settings.priority == 0,
                }
            };
            if state.first {
                state.first = false;
            }

            drop(pad_settings);

            if switch_to_pad {
                state.timed_out = false;
                self.set_active_pad(&mut state, pad)
            }
        };

        /* This might be the active_sinkpad now */
        let is_active = state.active_sinkpad.as_ref() == Some(pad);

        let mut pad_state = pad_imp.state.lock();
        let (start_running_time, end_running_time) = pad_state.get_sync_time(&buffer);

        log!(
            CAT,
            obj: pad,
            "Handling buffer {:?} run ts start {:?} end {:?} pad active {}",
            buffer,
            start_running_time,
            end_running_time,
            is_active
        );

        #[allow(clippy::blocks_in_if_conditions)]
        let output_clockid = if is_active {
            pad_state.schedule_clock(
                element,
                start_running_time,
                state.upstream_latency + settings.latency,
            )
        } else if end_running_time.map_or(false, |end_running_time| {
            end_running_time < state.timeout_running_time
        }) {
            log!(
                CAT,
                obj: pad,
                "Dropping trailing buffer {:?} before timeout {}",
                buffer,
                state.timeout_running_time
            );

            return Ok(gst::FlowSuccess::Ok);
        } else {
            pad_state.schedule_clock(
                element,
                end_running_time,
                state.upstream_latency + settings.timeout + settings.latency,
            )
        };

        if let Some(running_time) = start_running_time {
            pad_state.current_running_time = Some(running_time);
        }
        drop(pad_state);

        /* Before sleeping, ensure there is a timeout to switch active pads,
         * in case the initial active pad never receives a buffer */
        if let Some(running_time) = start_running_time {
            if state.timeout_clock_id.is_none() && !is_active {
                self.schedule_timeout(element, &mut state, &settings, running_time);
            }
        }

        let mut state = if let Some(clock_id) = &output_clockid {
            drop(state);

            let (_res, _) = clock_id.wait();
            self.state.lock()
        } else {
            state
        };

        let pad_state = pad_imp.state.lock();
        if pad_state.flushing {
            debug!(CAT, obj: element, "Flushing");
            return Err(gst::FlowError::Flushing);
        }
        drop(pad_state);

        let is_active = state.active_sinkpad.as_ref() == Some(pad);
        let switched_pad = state.switched_pad;
        let discont_pending = state.discont_pending;

        if is_active {
            if start_running_time < state.output_running_time {
                log!(
                    CAT,
                    obj: pad,
                    "Dropping trailing buffer {:?} before timeout {}",
                    buffer,
                    state.timeout_running_time
                );

                return Ok(gst::FlowSuccess::Ok);
            }

            if start_running_time.is_some() {
                state.output_running_time = start_running_time;
            }

            if let Some(end_running_time) = end_running_time {
                self.schedule_timeout(element, &mut state, &settings, end_running_time);
            } else {
                state.cancel_timeout();
            }

            state.switched_pad = false;
            state.discont_pending = false;
        }

        let mut pad_state = pad_imp.state.lock();
        pad_state.is_healthy = pad_state.is_healthy(&state, &settings);
        drop(pad_state);

        if !is_active {
            log!(CAT, obj: pad, "Dropping {:?}", buffer);
            return Ok(gst::FlowSuccess::Ok);
        }
        let _stream_lock = self.src_pad.stream_lock();
        drop(state);

        if switched_pad {
            let _ = pad.push_event(gst::event::Reconfigure::new());
            pad.sticky_events_foreach(|event| {
                self.src_pad.push_event(event.clone());
                std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
            });

            element.notify(PROP_ACTIVE_PAD);
        }

        if discont_pending && !buffer.flags().contains(gst::BufferFlags::DISCONT) {
            let buffer = buffer.make_mut();
            buffer.set_flags(gst::BufferFlags::DISCONT);
        }

        /* TODO: Clip raw video and audio buffers to avoid going backward? */

        log!(CAT, obj: pad, "Forwarding {:?}", buffer);
        self.src_pad.push(buffer)
    }

    fn sink_chain_list(
        &self,
        pad: &super::FallbackSwitchSinkPad,
        element: &super::FallbackSwitch,
        list: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        log!(CAT, obj: pad, "Handling buffer list {:?}", list);
        // TODO: Keep the list intact and forward it in one go (or broken into several
        // pieces if needed) when outputting to the active pad
        for buffer in list.iter_owned() {
            self.sink_chain(pad, element, buffer)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(
        &self,
        pad: &super::FallbackSwitchSinkPad,
        element: &super::FallbackSwitch,
        event: gst::Event,
    ) -> bool {
        let mut state = self.state.lock();
        let forward = state.active_sinkpad.as_ref() == Some(pad);

        let mut pad_state = pad.imp().state.lock();

        match event.view() {
            gst::EventView::Caps(caps) => {
                let caps = caps.caps();
                debug!(CAT, obj: pad, "Received caps {}", caps);

                let caps_info = match caps.structure(0).unwrap().name() {
                    "audio/x-raw" => {
                        CapsInfo::Audio(gst_audio::AudioInfo::from_caps(caps).unwrap())
                    }
                    "video/x-raw" => {
                        CapsInfo::Video(gst_video::VideoInfo::from_caps(caps).unwrap())
                    }
                    _ => CapsInfo::None,
                };

                pad_state.caps_info = caps_info;
            }
            gst::EventView::Segment(e) => {
                let segment = match e.segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst::element_error!(
                            element,
                            gst::StreamError::Format,
                            ["Only TIME segments supported, got {:?}", segment.format(),]
                        );
                        return false;
                    }
                    Ok(segment) => segment,
                };

                pad_state.segment = segment;
            }
            gst::EventView::FlushStart(_) => {
                pad_state.flush_start();
            }
            gst::EventView::FlushStop(_) => {
                pad_state.reset();
                state.first = true;
            }
            _ => {}
        }

        let fwd_sticky = if forward && state.switched_pad && event.is_serialized() {
            state.switched_pad = false;
            true
        } else {
            false
        };

        drop(pad_state);
        let _stream_lock = forward.then(|| self.src_pad.stream_lock());
        drop(state);

        if forward {
            if fwd_sticky {
                let _ = pad.push_event(gst::event::Reconfigure::new());
                pad.sticky_events_foreach(|event| {
                    self.src_pad.push_event(event.clone());
                    std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
                });

                element.notify(PROP_ACTIVE_PAD);
            }
            self.src_pad.push_event(event);
        }
        true
    }

    fn sink_query(
        &self,
        pad: &super::FallbackSwitchSinkPad,
        element: &super::FallbackSwitch,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        log!(CAT, obj: pad, "Handling query {:?}", query);

        let forward = match query.view() {
            QueryView::Context(_) => true,
            QueryView::Position(_) => true,
            QueryView::Duration(_) => true,
            QueryView::Caps(_) => true,
            QueryView::Allocation(_) => {
                let state = self.state.lock();
                /* Forward allocation only for the active sink pad,
                 * for others switching will send a reconfigure event upstream
                 */
                state.active_sinkpad.as_ref() == Some(pad)
            }
            _ => {
                pad.query_default(Some(element), query);
                false
            }
        };

        if forward {
            log!(CAT, obj: pad, "Forwarding query {:?}", query);
            self.src_pad.peer_query(query)
        } else {
            false
        }
    }

    fn reset(&self, _element: &super::FallbackSwitch) {
        let mut state = self.state.lock();
        *state = State::default();
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::FallbackSwitch,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        log!(CAT, obj: pad, "Handling {:?}", query);

        match query.view_mut() {
            QueryViewMut::Latency(ref mut q) => {
                let mut ret = true;
                let mut min_latency = gst::ClockTime::ZERO;
                let mut max_latency = gst::ClockTime::NONE;

                for pad in element.sink_pads() {
                    let mut peer_query = gst::query::Latency::new();

                    ret = pad.peer_query(&mut peer_query);

                    if ret {
                        let (live, min, max) = peer_query.result();
                        if live {
                            min_latency = min.max(min_latency);
                            max_latency = max
                                .zip(max_latency)
                                .map(|(max, max_latency)| max.min(max_latency))
                                .or(max);
                        }
                    }
                }

                let mut state = self.state.lock();
                let settings = self.settings.lock().clone();
                min_latency = min_latency.max(settings.min_upstream_latency);
                state.upstream_latency = min_latency;
                log!(CAT, obj: pad, "Upstream latency {}", min_latency);

                q.set(true, min_latency + settings.latency, max_latency);

                ret
            }
            QueryViewMut::Caps(_) => {
                let sinkpad = {
                    let state = self.state.lock();
                    state.active_sinkpad.clone()
                };

                if let Some(sinkpad) = sinkpad {
                    sinkpad.peer_query(query)
                } else {
                    pad.query_default(Some(element), query)
                }
            }
            _ => {
                let sinkpad = {
                    let state = self.state.lock();
                    state.active_sinkpad.clone()
                };

                if let Some(sinkpad) = sinkpad {
                    sinkpad.peer_query(query)
                } else {
                    true
                }
            }
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FallbackSwitch {
    const NAME: &'static str = "FallbackSwitch";
    type Type = super::FallbackSwitch;
    type ParentType = gst::Element;
    type Interfaces = (gst::ChildProxy,);

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .query_function(|pad, parent, query| {
                FallbackSwitch::catch_panic_pad_function(
                    parent,
                    || false,
                    |fallbackswitch, element| fallbackswitch.src_query(pad, element, query),
                )
            })
            .build();

        Self {
            src_pad: srcpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
            sink_pad_serial: AtomicU32::new(0),
        }
    }
}

impl ObjectImpl for FallbackSwitch {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecObject::new(
                    PROP_ACTIVE_PAD,
                    "Active Pad",
                    "Currently active pad",
                    gst::Pad::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecUInt64::new(
                    PROP_TIMEOUT,
                    "Input timeout",
                    "Timeout on an input before switching to a lower priority input.",
                    0,
                    std::u64::MAX - 1,
                    Settings::default().timeout.nseconds(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecUInt64::new(
                    PROP_LATENCY,
                    "Latency",
                    "Additional latency in live mode to allow upstream to take longer to produce buffers for the current position (in nanoseconds)",
                    0,
                    std::u64::MAX - 1,
                    Settings::default().latency.nseconds(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt64::new(
                    PROP_MIN_UPSTREAM_LATENCY,
                    "Minimum Upstream Latency",
                    "When sources with a higher latency are expected to be plugged in dynamically after the fallbackswitch has started playing, this allows overriding the minimum latency reported by the initial source(s). This is only taken into account when larger than the actually reported minimum latency. (nanoseconds)",
                    0,
                    std::u64::MAX - 1,
                    Settings::default().min_upstream_latency.nseconds(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecBoolean::new(
                    PROP_IMMEDIATE_FALLBACK,
                    "Immediate fallback",
                    "Forward lower-priority streams immediately at startup, when the stream with priority 0 is slow to start up and immediate output is required",
                    Settings::default().immediate_fallback,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecBoolean::new(
                    PROP_AUTO_SWITCH,
                    "Automatically switch pads",
                    "Automatically switch pads (If true, use the priority pad property, otherwise manual selection via the active-pad property)",
                    Settings::default().auto_switch,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            PROP_ACTIVE_PAD => {
                let settings = self.settings.lock();
                if settings.auto_switch {
                    gst::warning!(
                        CAT,
                        obj: obj,
                        "active-pad property setting ignored, because auto-switch=true"
                    );
                } else {
                    let active_pad = value
                        .get::<Option<gst::Pad>>()
                        .expect("type checked upstream");
                    /* Trigger a pad switch if needed */
                    if let Some(active_pad) = active_pad {
                        self.set_active_pad(
                            &mut self.state.lock(),
                            active_pad
                                .downcast_ref::<super::FallbackSwitchSinkPad>()
                                .unwrap(),
                        );
                    }
                }
                drop(settings);
            }
            PROP_TIMEOUT => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");

                settings.timeout = new_value;
                debug!(CAT, obj: obj, "Timeout now {}", settings.timeout);
                drop(settings);
                let _ = obj.post_message(gst::message::Latency::builder().src(obj).build());
            }
            PROP_LATENCY => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");

                settings.latency = new_value;
                drop(settings);
                let _ = obj.post_message(gst::message::Latency::builder().src(obj).build());
            }
            PROP_MIN_UPSTREAM_LATENCY => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");

                settings.min_upstream_latency = new_value;
                drop(settings);
                let _ = obj.post_message(gst::message::Latency::builder().src(obj).build());
            }
            PROP_IMMEDIATE_FALLBACK => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                settings.immediate_fallback = new_value;
            }
            PROP_AUTO_SWITCH => {
                let mut settings = self.settings.lock();
                let new_value = value.get().expect("type checked upstream");
                settings.auto_switch = new_value;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            PROP_ACTIVE_PAD => {
                let state = self.state.lock();
                let active_pad = state.active_sinkpad.clone();
                active_pad.to_value()
            }
            PROP_TIMEOUT => {
                let settings = self.settings.lock();
                settings.timeout.to_value()
            }
            PROP_LATENCY => {
                let settings = self.settings.lock();
                settings.latency.to_value()
            }
            PROP_MIN_UPSTREAM_LATENCY => {
                let settings = self.settings.lock();
                settings.min_upstream_latency.to_value()
            }
            PROP_IMMEDIATE_FALLBACK => {
                let settings = self.settings.lock();
                settings.immediate_fallback.to_value()
            }
            PROP_AUTO_SWITCH => {
                let settings = self.settings.lock();
                settings.auto_switch.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.src_pad).unwrap();
        obj.set_element_flags(gst::ElementFlags::REQUIRE_CLOCK);
    }
}

impl ElementImpl for FallbackSwitch {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Priority-based input selector",
                "Generic",
                "Priority-based automatic input selector element",
                "Jan Schmidt <jan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
                super::FallbackSwitchSinkPad::static_type(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::PlayingToPaused => {
                self.cancel_waits(element);
            }
            gst::StateChange::ReadyToNull => {
                self.reset(element);
            }
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock();
                let prev_active_pad = state.active_sinkpad.clone();
                *state = State::default();
                let pads = element.sink_pads();

                if let Some(pad) = pads.first() {
                    let pad = pad.downcast_ref::<super::FallbackSwitchSinkPad>().unwrap();

                    state.active_sinkpad = Some(pad.clone());
                    state.switched_pad = true;
                    state.discont_pending = true;
                    drop(state);

                    if prev_active_pad.as_ref() != Some(pad) {
                        element.notify(PROP_ACTIVE_PAD);
                    }
                }
                for pad in pads {
                    let pad = pad.downcast_ref::<super::FallbackSwitchSinkPad>().unwrap();
                    let pad_imp = pad.imp();
                    *pad_imp.state.lock() = SinkState::default();
                }
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                *self.state.lock() = State::default();
                for pad in element.sink_pads() {
                    let pad = pad.downcast_ref::<super::FallbackSwitchSinkPad>().unwrap();
                    let pad_imp = pad.imp();
                    *pad_imp.state.lock() = SinkState::default();
                }
            }
            _ => (),
        }

        Ok(success)
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut state = self.state.lock();

        let pad_serial = self.sink_pad_serial.fetch_add(1, Ordering::SeqCst);

        let pad = gst::PadBuilder::<super::FallbackSwitchSinkPad>::from_template(
            templ,
            Some(format!("sink_{}", pad_serial).as_str()),
        )
        .chain_function(|pad, parent, buffer| {
            FallbackSwitch::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |fallbackswitch, element| fallbackswitch.sink_chain(pad, element, buffer),
            )
        })
        .chain_list_function(|pad, parent, bufferlist| {
            FallbackSwitch::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |fallbackswitch, element| fallbackswitch.sink_chain_list(pad, element, bufferlist),
            )
        })
        .event_function(|pad, parent, event| {
            FallbackSwitch::catch_panic_pad_function(
                parent,
                || false,
                |fallbackswitch, element| fallbackswitch.sink_event(pad, element, event),
            )
        })
        .query_function(|pad, parent, query| {
            FallbackSwitch::catch_panic_pad_function(
                parent,
                || false,
                |fallbackswitch, element| fallbackswitch.sink_query(pad, element, query),
            )
        })
        .build();

        pad.set_active(true).unwrap();
        element.add_pad(&pad).unwrap();

        if state.active_sinkpad.is_none() {
            state.active_sinkpad = Some(pad.clone());
            state.switched_pad = true;
            state.discont_pending = true;
            element.notify(PROP_ACTIVE_PAD);
        }

        let mut pad_settings = pad.imp().settings.lock();
        pad_settings.priority = pad_serial;
        drop(pad_settings);
        drop(state);

        let _ = element.post_message(gst::message::Latency::builder().src(element).build());

        element.child_added(&pad, &pad.name());
        Some(pad.upcast())
    }

    fn release_pad(&self, element: &Self::Type, pad: &gst::Pad) {
        element.remove_pad(pad).unwrap();

        element.child_removed(pad, &pad.name());
        let _ = element.post_message(gst::message::Latency::builder().src(element).build());
    }
}

// Implementation of gst::ChildProxy virtual methods.
//
// This allows accessing the pads and their properties from e.g. gst-launch.
impl ChildProxyImpl for FallbackSwitch {
    fn children_count(&self, object: &Self::Type) -> u32 {
        object.num_pads() as u32
    }

    fn child_by_name(&self, object: &Self::Type, name: &str) -> Option<glib::Object> {
        object
            .pads()
            .into_iter()
            .find(|p| p.name() == name)
            .map(|p| p.upcast())
    }

    fn child_by_index(&self, object: &Self::Type, index: u32) -> Option<glib::Object> {
        object
            .pads()
            .into_iter()
            .nth(index as usize)
            .map(|p| p.upcast())
    }
}
