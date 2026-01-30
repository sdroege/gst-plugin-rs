// Copyright (C) 2022 LTN Global Communications, Inc.
// Contact: Jan Alexander Steffens (heftig) <jan.steffens@ltnglobal.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{
    glib::{self, translate::IntoGlib},
    prelude::*,
    subclass::prelude::*,
};
use parking_lot::{Condvar, Mutex, MutexGuard};
use std::sync::LazyLock;
use std::{collections::VecDeque, sync::mpsc};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "livesync",
        gst::DebugColorFlags::empty(),
        Some("debug category for the livesync element"),
    )
});

fn audio_info_from_caps(
    caps: &gst::CapsRef,
) -> Result<Option<gst_audio::AudioInfo>, glib::BoolError> {
    caps.structure(0)
        .is_some_and(|s| s.has_name("audio/x-raw"))
        .then(|| gst_audio::AudioInfo::from_caps(caps))
        .transpose()
}

fn duration_from_caps(caps: &gst::CapsRef) -> Option<gst::ClockTime> {
    caps.structure(0)
        .filter(|s| s.name().starts_with("video/") || s.name().starts_with("image/"))
        .and_then(|s| s.get::<gst::Fraction>("framerate").ok())
        .filter(|framerate| framerate.denom() > 0 && framerate.numer() > 0)
        .and_then(|framerate| {
            gst::ClockTime::SECOND.mul_div_round(framerate.denom() as u64, framerate.numer() as u64)
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufferLateness {
    OnTime,
    LateUnderThreshold,
    LateOverThreshold,
}

#[derive(Debug)]
enum Item {
    Buffer(gst::Buffer, Option<RunningTimeRange>, BufferLateness),
    Event(gst::Event),
    // SAFETY: Item needs to wait until the query and the receiver has returned
    Query(std::ptr::NonNull<gst::QueryRef>, mpsc::SyncSender<bool>),
}

impl Item {
    fn is_buffer(&self) -> bool {
        matches!(*self, Item::Buffer(_, _, _))
    }
}

// SAFETY: Need to be able to pass *mut gst::QueryRef
unsafe impl Send for Item {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunningTimeRange {
    start: gst::ClockTime,
    end: gst::ClockTime,
}

#[derive(Debug)]
pub struct LiveSync {
    state: Mutex<State>,
    cond: Condvar,
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
}

#[derive(Debug)]
struct State {
    /// See `PROP_LATENCY`
    latency: gst::ClockTime,

    /// See `PROP_LATE_THRESHOLD`
    late_threshold: Option<gst::ClockTime>,

    /// See `PROP_SINGLE_SEGMENT`
    single_segment: bool,

    /// See `PROP_SYNC`
    sync: bool,

    /// Latency reported by upstream
    upstream_latency: Option<gst::ClockTime>,

    /// Whether we're in PLAYING state
    playing: bool,

    /// Whether our sinkpad is EOS
    eos: bool,

    /// Flow state of our srcpad
    srcresult: Result<gst::FlowSuccess, gst::FlowError>,

    /// Wait operation for our next buffer
    clock_id: Option<gst::SingleShotClockId>,

    /// Segment of our sinkpad
    in_segment: Option<(gst::FormattedSegment<gst::ClockTime>, gst::Seqnum)>,

    /// Segment to be applied to the srcpad on the next queued buffer
    pending_segment: Option<(gst::FormattedSegment<gst::ClockTime>, gst::Seqnum)>,

    /// Segment of our srcpad
    out_segment: Option<(gst::FormattedSegment<gst::ClockTime>, gst::Seqnum)>,

    /// Caps of our sinkpad
    in_caps: Option<gst::Caps>,

    /// Caps to be applied to the srcpad on the next queued buffer
    pending_caps: Option<gst::Caps>,

    /// Audio format of our sinkpad
    in_audio_info: Option<gst_audio::AudioInfo>,

    /// Audio format of our srcpad
    out_audio_info: Option<gst_audio::AudioInfo>,

    /// Duration from caps on our sinkpad
    in_duration: Option<gst::ClockTime>,

    /// Duration from caps on our srcpad
    out_duration: Option<gst::ClockTime>,

    /// Queue between sinkpad and srcpad
    queue: VecDeque<Item>,

    /// Current buffer of our srcpad
    out_buffer: Option<gst::Buffer>,

    /// Whether our last output buffer was a duplicate
    out_buffer_duplicate: bool,

    /// Running time of our sinkpad
    in_last_rt_range: Option<RunningTimeRange>,

    /// Running time of our srcpad
    out_last_rt_range: Option<RunningTimeRange>,

    /// See `PROP_SILENT`
    silent: bool,

    /// See `PROP_IN`
    num_in: u64,

    /// See `PROP_DROP`
    num_drop: u64,

    /// See `PROP_OUT`
    num_out: u64,

    /// See `PROP_DUPLICATE`
    num_duplicate: u64,
}

const PROP_LATENCY: &str = "latency";
const PROP_LATE_THRESHOLD: &str = "late-threshold";
const PROP_SINGLE_SEGMENT: &str = "single-segment";
const PROP_SYNC: &str = "sync";

const PROP_IN: &str = "in";
const PROP_DROP: &str = "drop";
const PROP_OUT: &str = "out";
const PROP_DUPLICATE: &str = "duplicate";
const PROP_SILENT: &str = "silent";

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::ZERO;
const MINIMUM_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(8);
const DEFAULT_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(100);
const MAXIMUM_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(10);
const MINIMUM_LATE_THRESHOLD: gst::ClockTime = gst::ClockTime::ZERO;
const DEFAULT_LATE_THRESHOLD: Option<gst::ClockTime> = Some(gst::ClockTime::from_seconds(2));
const DEFAULT_SILENT: bool = true;

impl Default for State {
    fn default() -> Self {
        Self {
            latency: DEFAULT_LATENCY,
            late_threshold: DEFAULT_LATE_THRESHOLD,
            single_segment: false,
            sync: true,
            upstream_latency: None,
            playing: false,
            eos: false,
            srcresult: Err(gst::FlowError::Flushing),
            clock_id: None,
            in_segment: None,
            pending_segment: None,
            out_segment: None,
            in_caps: None,
            pending_caps: None,
            in_duration: None,
            out_duration: None,
            in_audio_info: None,
            out_audio_info: None,
            queue: VecDeque::with_capacity(32),
            out_buffer: None,
            out_buffer_duplicate: false,
            in_last_rt_range: None,
            out_last_rt_range: None,
            silent: DEFAULT_SILENT,
            num_in: 0,
            num_drop: 0,
            num_out: 0,
            num_duplicate: 0,
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for LiveSync {
    const NAME: &'static str = "GstLiveSync";
    type Type = super::LiveSync;
    type ParentType = gst::Element;

    fn with_class(class: &Self::Class) -> Self {
        let sinkpad = gst::Pad::builder_from_template(&class.pad_template("sink").unwrap())
            .activatemode_function(|pad, parent, mode, active| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::loggable_error!(CAT, "sink_activatemode panicked")),
                    |livesync| livesync.sink_activatemode(pad, mode, active),
                )
            })
            .event_function(|pad, parent, event| {
                Self::catch_panic_pad_function(
                    parent,
                    || false,
                    |livesync| livesync.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(
                    parent,
                    || false,
                    |livesync| livesync.sink_query(pad, query),
                )
            })
            .chain_function(|pad, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |livesync| livesync.sink_chain(pad, buffer),
                )
            })
            .flags(
                gst::PadFlags::PROXY_CAPS
                    | gst::PadFlags::PROXY_ALLOCATION
                    | gst::PadFlags::PROXY_SCHEDULING,
            )
            .build();

        let srcpad = gst::Pad::builder_from_template(&class.pad_template("src").unwrap())
            .activatemode_function(|pad, parent, mode, active| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::loggable_error!(CAT, "src_activatemode panicked")),
                    |livesync| livesync.src_activatemode(pad, mode, active),
                )
            })
            .event_function(|pad, parent, event| {
                Self::catch_panic_pad_function(
                    parent,
                    || false,
                    |livesync| livesync.src_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(
                    parent,
                    || false,
                    |livesync| livesync.src_query(pad, query),
                )
            })
            .flags(
                gst::PadFlags::PROXY_CAPS
                    | gst::PadFlags::PROXY_ALLOCATION
                    | gst::PadFlags::PROXY_SCHEDULING,
            )
            .build();

        Self {
            state: Default::default(),
            cond: Condvar::new(),
            sinkpad,
            srcpad,
        }
    }
}

impl ObjectImpl for LiveSync {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<[glib::ParamSpec; 9]> = LazyLock::new(|| {
            [
                glib::ParamSpecUInt64::builder(PROP_LATENCY)
                    .nick("Latency")
                    .blurb(
                        "Additional latency to allow upstream to take longer to \
                         produce buffers for the current position (in nanoseconds)",
                    )
                    .maximum(i64::MAX as u64)
                    .default_value(DEFAULT_LATENCY.into_glib())
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder(PROP_LATE_THRESHOLD)
                    .nick("Late threshold")
                    .blurb(
                        "Maximum time spent (in nanoseconds) before \
                         accepting one late buffer; -1 = never",
                    )
                    .minimum(MINIMUM_LATE_THRESHOLD.into_glib())
                    .default_value(DEFAULT_LATE_THRESHOLD.into_glib())
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder(PROP_SINGLE_SEGMENT)
                    .nick("Single segment")
                    .blurb("Timestamp buffers and eat segments so as to appear as one segment")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder(PROP_SYNC)
                    .nick("Sync")
                    .blurb("Synchronize buffers to the clock")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder(PROP_IN)
                    .nick("Frames input")
                    .blurb("Number of incoming frames accepted")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder(PROP_DROP)
                    .nick("Frames dropped")
                    .blurb("Number of incoming frames dropped")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder(PROP_OUT)
                    .nick("Frames output")
                    .blurb("Number of outgoing frames produced")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder(PROP_DUPLICATE)
                    .nick("Frames duplicated")
                    .blurb("Number of outgoing frames duplicated")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder(PROP_SILENT)
                    .nick("Silent")
                    .blurb("Don't emit notify for dropped and duplicated frames")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut state = self.state.lock();
        match pspec.name() {
            PROP_LATENCY => {
                state.latency = value.get().unwrap();
                let _ = self.obj().post_message(gst::message::Latency::new());
            }

            PROP_LATE_THRESHOLD => {
                state.late_threshold = value.get().unwrap();
            }

            PROP_SINGLE_SEGMENT => {
                state.single_segment = value.get().unwrap();
            }

            PROP_SYNC => {
                state.sync = value.get().unwrap();
            }

            PROP_SILENT => {
                state.silent = value.get().unwrap();
            }

            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let state = self.state.lock();
        match pspec.name() {
            PROP_LATENCY => state.latency.to_value(),
            PROP_LATE_THRESHOLD => state.late_threshold.to_value(),
            PROP_SINGLE_SEGMENT => state.single_segment.to_value(),
            PROP_SYNC => state.sync.to_value(),
            PROP_IN => state.num_in.to_value(),
            PROP_DROP => state.num_drop.to_value(),
            PROP_OUT => state.num_out.to_value(),
            PROP_DUPLICATE => state.num_duplicate.to_value(),
            PROP_SILENT => state.silent.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for LiveSync {}

impl ElementImpl for LiveSync {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Live Synchronizer",
                "Filter",
                "Outputs livestream, inserting gap frames when input lags",
                "Jan Alexander Steffens (heftig) <jan.steffens@ltnglobal.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<[gst::PadTemplate; 2]> = LazyLock::new(|| {
            // Accept raw or intra-only streams
            let caps = gst::Caps::builder_full()
                .structure(gst::Structure::new_empty("audio/x-raw"))
                .structure_with_any_features(gst::Structure::new_empty("video/x-raw"))
                .structure_with_any_features(gst::Structure::new_empty("video/x-bayer"))
                .structure(gst::Structure::new_empty("image/jpeg"))
                .structure(gst::Structure::new_empty("image/png"))
                .build();

            [
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {transition:?}");

        if transition == gst::StateChange::PausedToPlaying {
            let mut state = self.state.lock();
            state.playing = true;
            self.cond.notify_all();
        }

        let success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PlayingToPaused => {
                let mut state = self.state.lock();
                state.playing = false;
            }

            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock();
                state.num_in = 0;
                state.num_drop = 0;
                state.num_out = 0;
                state.num_duplicate = 0;
                let notify = !state.silent;
                drop(state);
                if notify {
                    self.obj().notify(PROP_DROP);
                    self.obj().notify(PROP_DUPLICATE);
                }
            }

            _ => {}
        }

        match (transition, success) {
            (
                gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused,
                gst::StateChangeSuccess::Success,
            ) => Ok(gst::StateChangeSuccess::NoPreroll),
            (_, s) => Ok(s),
        }
    }

    fn provide_clock(&self) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}

impl State {
    /// Calculate the running time the buffer covers, including latency
    fn running_time_range(
        &self,
        buf: &gst::BufferRef,
        segment: &gst::FormattedSegment<gst::ClockTime>,
    ) -> Option<RunningTimeRange> {
        let mut timestamp_start = buf.pts()?;

        if !self.single_segment {
            timestamp_start = segment
                .to_running_time(timestamp_start)
                .unwrap_or(gst::ClockTime::ZERO);
            timestamp_start += self.latency + self.upstream_latency.unwrap();
        } else {
            timestamp_start += self.upstream_latency.unwrap();
        }

        Some(RunningTimeRange {
            start: timestamp_start,
            end: timestamp_start + buf.duration().unwrap(),
        })
    }

    fn pending_events(&self) -> bool {
        self.pending_caps.is_some() || self.pending_segment.is_some()
    }

    fn queue_size(&self) -> Option<gst::ClockTime> {
        let first_ts = self.queue.iter().find_map(|item| match item {
            Item::Buffer(_, Some(RunningTimeRange { start, .. }), _) => Some(*start),
            _ => None,
        });
        let first_ts = first_ts?;

        let last_ts = self
            .queue
            .iter()
            .rev()
            .find_map(|item| match item {
                Item::Buffer(_, Some(RunningTimeRange { start, .. }), _) => Some(*start),
                _ => None,
            })
            .unwrap();

        Some(last_ts.saturating_sub(first_ts))
    }

    fn stream_enqueued(&self) -> bool {
        self.queue.iter().any(|item| {
            let Item::Event(evt) = item else {
                return false;
            };
            evt.type_() == gst::EventType::StreamStart || evt.type_() == gst::EventType::Segment
        })
    }
}

impl LiveSync {
    fn sink_activatemode(
        &self,
        pad: &gst::Pad,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if mode != gst::PadMode::Push {
            return Err(gst::loggable_error!(CAT, "Wrong scheduling mode"));
        }

        if !active {
            self.set_flushing(&mut self.state.lock());

            let lock = pad.stream_lock();
            self.sink_reset(&mut self.state.lock());
            drop(lock);
        }

        Ok(())
    }

    fn src_activatemode(
        &self,
        pad: &gst::Pad,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if mode != gst::PadMode::Push {
            return Err(gst::loggable_error!(CAT, "Wrong scheduling mode"));
        }

        if active {
            self.start_src_task(&mut self.state.lock())
                .map_err(|e| gst::LoggableError::new(*CAT, e))?;
        } else {
            let mut state = self.state.lock();
            self.set_flushing(&mut state);
            self.src_reset(&mut state);
            drop(state);

            pad.stop_task()?;
        }

        Ok(())
    }

    fn set_flushing(&self, state: &mut State) {
        state.srcresult = Err(gst::FlowError::Flushing);
        if let Some(clock_id) = state.clock_id.take() {
            clock_id.unschedule();
        }

        // Ensure we drop any query response sender to unblock the sinkpad
        state.queue.clear();
        self.cond.notify_all();
    }

    fn sink_flush(&self, state: &mut State) {
        state.eos = false;
        state.in_segment = None;
        state.in_last_rt_range = None;
    }

    fn sink_reset(&self, state: &mut State) {
        self.sink_flush(state);
        state.in_caps = None;
        state.in_audio_info = None;
        state.in_duration = None;
    }

    fn src_flush(&self, state: &mut State) {
        state.pending_segment = None;
        state.out_segment = None;
        state.pending_caps = None;
        state.out_buffer = None;
        state.out_buffer_duplicate = false;
        state.out_last_rt_range = None;
    }

    fn src_reset(&self, state: &mut State) {
        self.src_flush(state);
        state.out_audio_info = None;
        state.out_duration = None;
    }

    fn sink_event(&self, pad: &gst::Pad, mut event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Incoming {event:?}");

        {
            let state = self.state.lock();
            if state.single_segment {
                let event = event.make_mut();
                let latency = state.latency.nseconds() as i64;
                event.set_running_time_offset(event.running_time_offset() + latency);
            }
        }

        let mut is_restart = false;
        let mut is_eos = false;

        match event.view() {
            gst::EventView::FlushStart(_) => {
                let ret = self.srcpad.push_event(event);

                self.set_flushing(&mut self.state.lock());

                if let Err(e) = self.srcpad.pause_task() {
                    gst::error!(CAT, obj = pad, "Failed to pause task: {e}");
                    return false;
                }

                return ret;
            }

            gst::EventView::FlushStop(_) => {
                let ret = self.srcpad.push_event(event);

                let mut state = self.state.lock();
                self.sink_flush(&mut state);
                self.src_flush(&mut state);

                if let Err(e) = self.start_src_task(&mut state) {
                    gst::error!(CAT, obj = pad, "Failed to start task: {e}");
                    return false;
                }

                return ret;
            }

            gst::EventView::StreamStart(_) => is_restart = true,

            gst::EventView::Segment(e) => {
                is_restart = true;

                let Some(evt_segment) = e.segment().downcast_ref() else {
                    gst::error!(CAT, obj = pad, "Got non-TIME segment");
                    return false;
                };
                let evt_seqnum = event.seqnum();

                let mut state = self.state.lock();
                if let Some((in_segment, in_seqnum)) = state.in_segment.take()
                    && in_seqnum != evt_seqnum
                {
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "Replacing previous Segment seqnum {in_seqnum:?} {in_segment:?} \
                        with {evt_seqnum:?} {evt_segment:?}",
                    );
                }

                state.in_segment = Some((evt_segment.clone(), evt_seqnum));
            }

            gst::EventView::Eos(_) | gst::EventView::SegmentDone(_) => {
                let evt_seqnum = event.seqnum();
                let mut state = self.state.lock();
                if let Some((_in_segment, in_seqnum)) = state.in_segment.take()
                    && in_seqnum != evt_seqnum
                {
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "Received {:?} with {evt_seqnum:?}, expected {in_seqnum:?}",
                        event.type_()
                    );
                }
                is_eos = true;
            }

            gst::EventView::Caps(c) => {
                let caps = c.caps_owned();

                let audio_info = match audio_info_from_caps(&caps) {
                    Ok(ai) => ai,
                    Err(e) => {
                        gst::error!(CAT, obj = pad, "Failed to parse audio caps: {e}");
                        return false;
                    }
                };

                let duration = duration_from_caps(&caps);

                let mut state = self.state.lock();
                state.in_caps = Some(caps);
                state.in_audio_info = audio_info;
                state.in_duration = duration;
            }

            gst::EventView::Gap(_) => {
                gst::debug!(CAT, obj = pad, "Got gap event");
                return true;
            }

            _ => {}
        }

        if !event.is_serialized() {
            return gst::Pad::event_default(pad, Some(&*self.obj()), event);
        }

        let mut state = self.state.lock();

        if is_restart {
            state.eos = false;

            if state.srcresult == Err(gst::FlowError::Eos)
                && let Err(e) = self.start_src_task(&mut state)
            {
                gst::error!(CAT, obj = pad, "Failed to start task: {e}");
                return false;
            }
        }

        if state.eos {
            gst::trace!(CAT, obj = pad, "Refusing event, we are EOS: {event:?}");
            return false;
        }

        if is_eos {
            state.eos = true;
        }

        if let Err(err) = state.srcresult {
            // Following GstQueue's behavior:
            // > For EOS events, that are not followed by data flow, we still
            // > return FALSE here though and report an error.
            if is_eos && !matches!(err, gst::FlowError::Flushing | gst::FlowError::Eos) {
                self.flow_error(err);
            }

            return false;
        }

        gst::trace!(CAT, obj = pad, "Queueing {event:?}");
        state.queue.push_back(Item::Event(event));
        self.cond.notify_all();

        true
    }

    fn src_event(&self, pad: &gst::Pad, mut event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Incoming {event:?}");

        {
            let state = self.state.lock();
            if state.single_segment {
                let event = event.make_mut();
                let latency = state.latency.nseconds() as i64;
                event.set_running_time_offset(event.running_time_offset() - latency);
            }
        }

        match event.view() {
            gst::EventView::Reconfigure(_) => {
                {
                    let mut state = self.state.lock();
                    if state.srcresult == Err(gst::FlowError::NotLinked)
                        && let Err(e) = self.start_src_task(&mut state)
                    {
                        gst::error!(CAT, obj = pad, "Failed to start task: {e}");
                    }
                }

                self.sinkpad.push_event(event)
            }

            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling {query:?}");

        if query.is_serialized() {
            let (sender, receiver) = mpsc::sync_channel(1);

            let mut state = self.state.lock();
            if state.srcresult.is_err() {
                return false;
            }

            gst::trace!(CAT, obj = pad, "Queueing {query:?}");
            state
                .queue
                .push_back(Item::Query(std::ptr::NonNull::from(query), sender));
            self.cond.notify_all();
            drop(state);

            // If the sender gets dropped, we will also unblock
            receiver.recv().unwrap_or(false)
        } else {
            gst::Pad::query_default(pad, Some(&*self.obj()), query)
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling {query:?}");

        match query.view_mut() {
            gst::QueryViewMut::Latency(_) => {
                if !gst::Pad::query_default(pad, Some(&*self.obj()), query) {
                    return false;
                }

                let q = match query.view_mut() {
                    gst::QueryViewMut::Latency(q) => q,
                    _ => unreachable!(),
                };

                let mut state = self.state.lock();

                let (live, up_min, up_max) = q.result();

                gst::debug!(
                    CAT,
                    obj = pad,
                    "Upstream latency query response: live {live} min {up_min} max {}",
                    up_max.display()
                );

                // Note: `Pad::query_default` ignores latency query result for non-live upstream
                //       so we don't need to do it ourselves.

                if state.sync && up_max.opt_lt(up_min).unwrap_or(false) {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Problematic latency detected upstream of livesync sync=true: \
                        max {} < min {up_min}. Add queues or other buffering elements.",
                        up_max.display()
                    );
                }

                let min = up_min + state.latency;
                let max = up_max.opt_add(state.latency);

                gst::debug!(
                    CAT,
                    obj = pad,
                    "Reporting latency: live {live} min {min} max {}",
                    max.display()
                );

                q.set(true, min, max);

                state.upstream_latency = Some(up_min);
                true
            }

            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj = pad, "Incoming {buffer:?}");

        let origin_ts = buffer.pts();

        let mut state = self.state.lock();

        if state.eos {
            gst::debug!(CAT, obj = pad, "Refusing buffer, we are EOS {buffer:?}");
            return Err(gst::FlowError::Eos);
        }

        if state.upstream_latency.is_none() {
            gst::debug!(CAT, obj = pad, "Have no upstream latency yet, querying");

            let mut q = gst::query::Latency::new();
            if MutexGuard::unlocked(&mut state, || pad.peer_query(&mut q)) {
                let (live, mut min, max) = q.result();

                gst::debug!(
                    CAT,
                    obj = pad,
                    "Latency query response: live {live} min {min} max {}",
                    max.display()
                );

                if !live {
                    gst::debug!(CAT, obj = pad, "Ignoring non-live upstream latency");
                    min = gst::ClockTime::ZERO;
                    // we're not using it now, but if we did, we would also need:
                    // max = gst::ClockTime::NONE;
                } else if state.sync && max.opt_lt(min).unwrap_or(false) {
                    gst::warning!(
                        CAT,
                        obj = pad,
                        "Problematic latency detected upstream of livesync sync=true: \
                        max {} < min {min}. Add queues or other buffering elements.",
                        max.display()
                    );
                }

                state.upstream_latency = Some(min);
            } else {
                gst::warning!(
                    CAT,
                    obj = pad,
                    "Can't query upstream latency -- assuming zero"
                );
                state.upstream_latency = Some(gst::ClockTime::ZERO);
            }
        }

        while state.srcresult.is_ok()
            && let Some(queue_size) = state.queue_size()
        {
            if queue_size > state.latency {
                gst::trace!(
                    CAT,
                    obj = pad,
                    "Queue filled, waiting... size {queue_size}, latency {}",
                    state.latency
                );
                self.cond.wait(&mut state);
            } else {
                break;
            }
        }

        if let Err(err) = state.srcresult {
            if err == gst::FlowError::Eos && state.pending_events() || state.stream_enqueued() {
                gst::log!(
                    CAT,
                    obj = pad,
                    "Accepting buffer in presence of {err:?} because a new stream is pending {buffer:?}",
                );
            } else {
                gst::debug!(CAT, obj = pad, "Refusing buffer due to {err:?} {buffer:?}");
                return Err(err);
            }
        }

        let buf_mut = buffer.make_mut();

        if buf_mut.pts().is_none() {
            gst::warning!(
                CAT,
                obj = pad,
                "Incoming buffer has no timestamps {buf_mut:?}",
            );
        }

        if let Some(audio_info) = &state.in_audio_info {
            let Some(calc_duration) = audio_info
                .convert::<Option<gst::ClockTime>>(gst::format::Bytes::from_usize(buf_mut.size()))
                .flatten()
            else {
                gst::error!(
                    CAT,
                    obj = pad,
                    "Failed to calculate duration of {buf_mut:?}",
                );
                return Err(gst::FlowError::Error);
            };

            if let Some(buf_duration) = buf_mut.duration() {
                let diff = if buf_duration < calc_duration {
                    calc_duration - buf_duration
                } else {
                    buf_duration - calc_duration
                };

                let sample_duration = gst::ClockTime::SECOND
                    .mul_div_round(1, audio_info.rate().into())
                    .unwrap();

                if diff > sample_duration {
                    gst::warning!(
                        CAT,
                        obj = pad,
                        "Correcting duration on audio buffer from {buf_duration} to {calc_duration} {buf_mut:?}",
                    );

                    buf_mut.set_duration(calc_duration);
                }
            } else {
                gst::debug!(
                    CAT,
                    obj = pad,
                    "Patching incoming buffer with duration {calc_duration} {buf_mut:?}",
                );

                buf_mut.set_duration(calc_duration);
            }
        } else if buf_mut.duration().is_none() {
            let duration = state.in_duration.map_or(DEFAULT_DURATION, |dur| {
                dur.clamp(MINIMUM_DURATION, MAXIMUM_DURATION)
            });

            gst::debug!(
                CAT,
                obj = pad,
                "Patching incoming buffer with duration {duration} {buf_mut:?}",
            );
            buf_mut.set_duration(duration);
        }

        // At this stage we should really really have a segment
        let (segment, _seq_num) = state.in_segment.as_ref().ok_or_else(|| {
            gst::error!(CAT, obj = pad, "Missing segment");
            gst::FlowError::Error
        })?;

        if state.single_segment {
            let pts = segment
                .to_running_time_full(buf_mut.pts())
                .and_then(|r| r.positive())
                .or_else(|| self.obj().current_running_time());

            buf_mut.set_pts(pts.map(|t| t + state.latency));
        }

        let rt_range = state.running_time_range(buf_mut, segment);
        let lateness = self.buffer_is_backwards(&state, buf_mut, rt_range);

        if lateness == BufferLateness::LateUnderThreshold {
            gst::debug!(
                CAT,
                obj = pad,
                "Discarding late buffer ts {:?}, {rt_range:?}, \
                origin ts {origin_ts:?} {buf_mut:?}",
                buf_mut.pts(),
            );
            state.num_drop += 1;
            let notify_drop = !state.silent;
            drop(state);

            if notify_drop {
                self.obj().notify(PROP_DROP);
            }

            return Ok(gst::FlowSuccess::Ok);
        }

        gst::trace!(
            CAT,
            obj = pad,
            "Queueing buffer ts {:?}, {rt_range:?}, \
            origin ts {origin_ts:?} ({lateness:?}) {buffer:?}",
            buffer.pts(),
        );
        state
            .queue
            .push_back(Item::Buffer(buffer, rt_range, lateness));
        state.in_last_rt_range = rt_range;
        self.cond.notify_all();

        // If we're not strictly syncing to the clock but output buffers as soon as they arrive
        // then also wake up the source pad task now in case it's waiting on the clock.
        if !state.sync
            && let Some(clock_id) = state.clock_id.take()
        {
            clock_id.unschedule();
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn start_src_task(&self, state: &mut State) -> Result<(), glib::BoolError> {
        gst::debug!(CAT, imp = self, "Starting loop");

        state.srcresult = Ok(gst::FlowSuccess::Ok);

        let imp = self.ref_counted();
        let ret = self.srcpad.start_task(move || imp.src_loop());

        if ret.is_err() {
            state.srcresult = Err(gst::FlowError::Error);
        }

        ret
    }

    fn src_loop(&self) {
        let Err(mut err) = self.src_loop_inner() else {
            return;
        };

        let eos = {
            let mut state = self.state.lock();

            match state.srcresult {
                // Can be set to Flushing by another thread
                Err(e) => err = e,

                // Communicate our flow return
                Ok(_) => state.srcresult = Err(err),
            }
            state.clock_id = None;
            self.cond.notify_all();

            state.eos
        };

        // Following GstQueue's behavior:
        // > let app know about us giving up if upstream is not expected to do so
        // > EOS is already taken care of elsewhere
        if eos && !matches!(err, gst::FlowError::Flushing | gst::FlowError::Eos) {
            gst::warning!(
                CAT,
                imp = self,
                "Loop returned {err} and we are not flushing & EOS hasn't been pushed"
            );
            self.flow_error(err);
        }

        gst::log!(CAT, imp = self, "Loop stopping");
        let _ = self.srcpad.pause_task();
    }

    fn src_loop_inner(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock();
        while state.srcresult.is_ok()
            && (!state.playing || (state.queue.is_empty() && state.out_buffer.is_none()))
        {
            self.cond.wait(&mut state);
        }
        state.srcresult?;

        // Synchronize buffers to the clock if requested to do so,
        // or when the queue is currently empty
        // and we might have to introduce a gap buffer.
        // But don't delay pushing events & queries.
        if (state.sync || state.queue.front().is_none_or(Item::is_buffer))
            && let Some(last_rt_range) = state.out_last_rt_range
        {
            let sync_rt = last_rt_range.end;

            let element = self.obj();

            let base_time = element.base_time().ok_or_else(|| {
                gst::error!(CAT, imp = self, "Missing base time");
                gst::FlowError::Flushing
            })?;

            let clock = element.clock().ok_or_else(|| {
                gst::error!(CAT, imp = self, "Missing clock");
                gst::FlowError::Flushing
            })?;

            let clock_id = clock.new_single_shot_id(base_time + sync_rt);
            state.clock_id = Some(clock_id.clone());

            gst::trace!(
                CAT,
                imp = self,
                "Waiting for clock to reach {} / running time {sync_rt}",
                clock_id.time(),
            );

            let (res, jitter) = MutexGuard::unlocked(&mut state, || clock_id.wait());
            gst::trace!(
                CAT,
                imp = self,
                "Clock returned {res:?} {}{}",
                if jitter.is_negative() { "-" } else { "" },
                gst::ClockTime::from_nseconds(jitter.unsigned_abs())
            );

            state.clock_id = None;
            state.srcresult?;
        }

        let in_item = state.queue.pop_front();
        gst::trace!(CAT, imp = self, "Unqueueing {in_item:?}");

        let in_buffer = match in_item {
            None => None,

            Some(Item::Buffer(buffer, rt_range, lateness)) => {
                // Synchronize on the first buffer with its start running time
                if let Some(RunningTimeRange { start, .. }) = state
                    .out_last_rt_range
                    .is_none()
                    .then_some(rt_range)
                    .flatten()
                {
                    state.out_last_rt_range = Some(RunningTimeRange { start, end: start });
                    gst::trace!(
                        CAT,
                        imp = self,
                        "Re-queueing first {buffer:?}, running time {start}"
                    );
                    state
                        .queue
                        .push_front(Item::Buffer(buffer, rt_range, lateness));

                    return Ok(gst::FlowSuccess::Ok);
                } else if self.buffer_is_early(&state, &buffer, rt_range) {
                    gst::trace!(
                        CAT,
                        imp = self,
                        "Re-queueing early {buffer:?}, running time {}",
                        rt_range.map(|rt_range| rt_range.start).display(),
                    );
                    state
                        .queue
                        .push_front(Item::Buffer(buffer, rt_range, lateness));
                    None
                } else {
                    self.cond.notify_all();
                    Some((buffer, rt_range, lateness))
                }
            }

            Some(Item::Event(event)) => {
                let mut push = true;

                match event.view() {
                    gst::EventView::Segment(e) => {
                        let segment = e.segment().downcast_ref().unwrap();
                        gst::debug!(CAT, imp = self, "pending {segment:?}");
                        state.pending_segment = Some((segment.clone(), e.seqnum()));
                        push = false;
                    }

                    gst::EventView::Eos(_) | gst::EventView::SegmentDone(_) => {
                        state.out_buffer = None;
                        state.out_buffer_duplicate = false;
                        state.out_last_rt_range = None;

                        self.cond.notify_all();

                        gst::debug!(CAT, imp = self, "Upstream notified {:?}", event.type_());
                        state.srcresult = Err(gst::FlowError::Eos);

                        if let Some((out_segment, out_seqnum)) = state.out_segment.as_ref() {
                            let out_event = match event.type_() {
                                gst::EventType::Eos => {
                                    gst::event::Eos::builder().seqnum(*out_seqnum).build()
                                }
                                gst::EventType::SegmentDone => {
                                    gst::event::SegmentDone::builder(out_segment.position())
                                        .seqnum(*out_seqnum)
                                        .build()
                                }
                                other => unreachable!("{other:?}"),
                            };
                            MutexGuard::unlocked(&mut state, || {
                                gst::debug!(CAT, imp = self, "Pushing {out_event:?}");
                                self.srcpad.push_event(out_event);
                            });
                        }

                        return Err(gst::FlowError::Eos);
                    }

                    gst::EventView::Caps(e) => {
                        state.pending_caps = Some(e.caps_owned());
                        push = false;
                    }

                    _ => {}
                }

                self.cond.notify_all();
                drop(state);

                if push {
                    self.srcpad.push_event(event);
                }

                return Ok(gst::FlowSuccess::Ok);
            }

            Some(Item::Query(mut query, sender)) => {
                self.cond.notify_all();
                drop(state);

                // SAFETY: The other thread is waiting for us to handle the query
                let res = self.srcpad.peer_query(unsafe { query.as_mut() });
                sender.send(res).ok();

                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let mut caps = None;
        let mut segment = None;

        let mut notify_dup = false;
        let mut notify_drop = false;

        match in_buffer {
            Some((mut buffer, rt_range, BufferLateness::OnTime)) => {
                state.num_in += 1;

                if state.out_buffer.is_none() || state.out_buffer_duplicate {
                    // We are just starting or done bridging a gap
                    buffer.make_mut().set_flags(gst::BufferFlags::DISCONT);
                }

                state.out_buffer = Some(buffer);
                state.out_buffer_duplicate = false;
                state.out_last_rt_range = rt_range;

                caps = state.pending_caps.take();
                segment = state.pending_segment.take();
            }

            Some((buffer, _rt_range, BufferLateness::LateOverThreshold))
                if !state.pending_events() =>
            {
                gst::debug!(CAT, imp = self, "Accepting late {buffer:?}");
                state.num_in += 1;

                self.patch_output_buffer(&mut state, Some(buffer))?;
                notify_dup = !state.silent;
            }

            Some((buffer, _rt_range, BufferLateness::LateOverThreshold)) => {
                // Cannot accept late-over-threshold buffers while we have pending events
                gst::debug!(CAT, imp = self, "Discarding late {buffer:?}");
                state.num_drop += 1;
                notify_drop = !state.silent;

                self.patch_output_buffer(&mut state, None)?;
                notify_dup = !state.silent;
            }

            None => {
                self.patch_output_buffer(&mut state, None)?;
                notify_dup = !state.silent;
            }

            Some((_, _, BufferLateness::LateUnderThreshold)) => {
                // Is discarded before queueing
                unreachable!();
            }
        }

        let buffer = state.out_buffer.clone().unwrap();
        let sync_ts = state
            .out_last_rt_range
            .map_or(gst::ClockTime::ZERO, |t| t.start);

        if let Some(caps) = caps {
            gst::debug!(CAT, imp = self, "Sending new caps: {caps}");

            let event = gst::event::Caps::new(&caps);
            MutexGuard::unlocked(&mut state, || self.srcpad.push_event(event));
            state.srcresult?;

            state.out_audio_info = audio_info_from_caps(&caps).unwrap();
            state.out_duration = duration_from_caps(&caps);
        }

        if let Some((mut segment, seqnum)) = segment {
            if !state.single_segment {
                if let Some(stop) = segment.stop() {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Removing stop {stop} from outgoing segment",
                    );
                    segment.set_stop(gst::ClockTime::NONE);
                }

                gst::debug!(CAT, imp = self, "Forwarding segment: {segment:?}");

                let event = gst::event::Segment::new(&segment);
                MutexGuard::unlocked(&mut state, || self.srcpad.push_event(event));
                state.srcresult.inspect_err(|err| {
                    gst::info!(CAT, imp = self, "Error pushing Segment event: {err:?}");
                })?;
            } else if state.out_segment.is_none() {
                // Create live segment
                let mut live_segment = gst::FormattedSegment::<gst::ClockTime>::new();
                live_segment.set_position(sync_ts);

                gst::debug!(CAT, imp = self, "Sending new segment: {live_segment:?}");

                let event = gst::event::Segment::new(&live_segment);
                MutexGuard::unlocked(&mut state, || self.srcpad.push_event(event));
                state.srcresult.inspect_err(|err| {
                    gst::info!(CAT, imp = self, "Error pushing Segment event: {err:?}");
                })?;
            }

            state.out_segment = Some((segment, seqnum));
        }

        state
            .out_segment
            .as_mut()
            .expect("defined at this stage")
            .0
            .set_position(buffer.dts_or_pts().opt_add(buffer.duration()));

        state.num_out += 1;
        drop(state);

        if notify_dup {
            self.obj().notify(PROP_DUPLICATE);
        }

        if notify_drop {
            self.obj().notify(PROP_DROP);
        }

        gst::trace!(CAT, imp = self, "Pushing {buffer:?}");
        self.srcpad.push(buffer).inspect_err(|err| {
            gst::info!(CAT, imp = self, "Error pushing buffer: {err:?}");
        })
    }

    fn buffer_is_backwards(
        &self,
        state: &State,
        buf: &gst::BufferRef,
        rt_range: Option<RunningTimeRange>,
    ) -> BufferLateness {
        let Some(rt_range) = rt_range else {
            return BufferLateness::OnTime;
        };

        let Some(out_last_rt_range) = state.out_last_rt_range else {
            return BufferLateness::OnTime;
        };

        if rt_range.end > out_last_rt_range.end {
            return BufferLateness::OnTime;
        }

        gst::debug!(
            CAT,
            imp = self,
            "Running time regresses: buffer ends at rt {}, expected {}, {buf:?}",
            rt_range.end,
            out_last_rt_range.end,
        );

        let late_threshold = match state.late_threshold {
            Some(gst::ClockTime::ZERO) => return BufferLateness::LateOverThreshold,
            Some(t) => t,
            None => return BufferLateness::LateUnderThreshold,
        };

        let Some(in_last_rt_range) = state.in_last_rt_range else {
            return BufferLateness::LateUnderThreshold;
        };

        if rt_range.start > in_last_rt_range.end + late_threshold {
            BufferLateness::LateOverThreshold
        } else {
            BufferLateness::LateUnderThreshold
        }
    }

    fn buffer_is_early(
        &self,
        state: &State,
        buffer: &gst::Buffer,
        rt_range: Option<RunningTimeRange>,
    ) -> bool {
        let Some(rt_range) = rt_range else {
            return false;
        };

        let Some(out_last_rt_range) = state.out_last_rt_range else {
            return false;
        };

        // When out_last_rt_range is set, we also have an out_buffer unless it is the first buffer
        if state.out_buffer.is_none() {
            return false;
        }

        // Use the duration we would insert as a gap filler in patch_output_buffer()
        let slack = state.out_duration.map_or(DEFAULT_DURATION, |dur| {
            dur.clamp(MINIMUM_DURATION, MAXIMUM_DURATION)
        });

        if rt_range.start < out_last_rt_range.end + slack {
            return false;
        }

        // This buffer would start beyond another buffer duration after our
        // last emitted buffer ended

        gst::debug!(
            CAT,
            imp = self,
            "Running time is too early: buffer starts at {}, expected {}, {buffer:?}",
            rt_range.start,
            out_last_rt_range.end,
        );

        true
    }

    /// Produces a message like GST_ELEMENT_FLOW_ERROR does
    fn flow_error(&self, err: gst::FlowError) {
        let details = gst::Structure::builder("details")
            .field("flow-return", err.into_glib())
            .build();
        gst::element_imp_error!(
            self,
            gst::StreamError::Failed,
            ("Internal data flow error."),
            ["streaming task paused, reason {err} ({err:?})"],
            details: details
        );
    }

    /// Patches the output buffer for repeating, setting out_buffer, out_buffer_duplicate and
    /// out_last_rt_range
    fn patch_output_buffer(
        &self,
        state: &mut State,
        source: Option<gst::Buffer>,
    ) -> Result<(), gst::FlowError> {
        let out_buffer = state.out_buffer.as_mut().unwrap();
        let mut duplicate = state.out_buffer_duplicate;

        let duration = out_buffer.duration().unwrap();
        let pts = out_buffer.pts().map(|t| t + duration);

        if let Some(source) = source {
            gst::debug!(
                CAT,
                imp = self,
                "Repeating buffer ts {:?}, dur {:?} using source {source:?}",
                out_buffer.pts(),
                out_buffer.duration(),
            );
            *out_buffer = source;
            duplicate = false;
        } else {
            gst::debug!(
                CAT,
                imp = self,
                "Repeating buffer ts {:?}, dur {:?}",
                out_buffer.pts(),
                out_buffer.duration(),
            );
        }

        let buffer = out_buffer.make_mut();

        if !duplicate {
            let duration_is_valid =
                (MINIMUM_DURATION..=MAXIMUM_DURATION).contains(&buffer.duration().unwrap());

            if state.out_duration.is_some() || !duration_is_valid {
                // Resize the buffer if caps gave us a duration
                // or the current duration is unreasonable

                let duration = state.out_duration.map_or(DEFAULT_DURATION, |dur| {
                    dur.clamp(MINIMUM_DURATION, MAXIMUM_DURATION)
                });

                if let Some(audio_info) = &state.out_audio_info {
                    let Some(size) = audio_info
                        .convert::<Option<gst::format::Bytes>>(duration)
                        .flatten()
                        .and_then(|bytes| usize::try_from(bytes).ok())
                    else {
                        gst::error!(CAT, imp = self, "Failed to calculate size of repeat buffer");
                        return Err(gst::FlowError::Error);
                    };

                    buffer.replace_all_memory(gst::Memory::with_size(size));
                }

                buffer.set_duration(duration);
                gst::debug!(
                    CAT,
                    imp = self,
                    "Patched output buffer duration to {duration}"
                );
            }

            if let Some(audio_info) = &state.out_audio_info {
                let mut map_info = buffer.map_writable().map_err(|e| {
                    gst::error!(CAT, imp = self, "Failed to map buffer: {e}");
                    gst::FlowError::Error
                })?;
                audio_info
                    .format_info()
                    .fill_silence(map_info.as_mut_slice());
            }
        }

        buffer.set_pts(pts);
        buffer.set_flags(gst::BufferFlags::GAP);
        buffer.unset_flags(gst::BufferFlags::DISCONT);

        state.out_buffer_duplicate = true;
        let (out_segment, _seqnum) = state.out_segment.as_ref().unwrap();
        state.out_last_rt_range =
            state.running_time_range(state.out_buffer.as_ref().unwrap(), out_segment);
        state.num_duplicate += 1;
        Ok(())
    }
}
