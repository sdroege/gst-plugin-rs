// Copyright (C) 2022 François Laignel <fengalin@free.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::future::BoxFuture;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::LazyLock;

use std::mem::size_of;
use std::sync::Mutex;
use std::time::Duration;
#[cfg(feature = "tuning")]
use std::time::Instant;

use crate::runtime::prelude::*;
use crate::runtime::{self, task, timer, PadSrc, Task};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-audiotestsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing audio test src"),
    )
});

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: Duration = Duration::ZERO;
const DEFAULT_BUFFER_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(10);
const DEFAULT_DO_TIMESTAMP: bool = false;
const DEFAULT_IS_LIVE: bool = false;
const DEFAULT_NUM_BUFFERS: i32 = -1;

const DEFAULT_CHANNELS: usize = 1;
const DEFAULT_FREQ: f32 = 440.0;
const DEFAULT_VOLUME: f32 = 0.8;
const DEFAULT_RATE: u32 = 44_100;

#[cfg(feature = "tuning")]
const RAMPUP_BUFFER_COUNT: u32 = 500;
#[cfg(feature = "tuning")]
const LOG_BUFFER_INTERVAL: u32 = 2000;

static DEFAULT_CAPS: LazyLock<gst::Caps> = LazyLock::new(|| {
    gst_audio::AudioCapsBuilder::new_interleaved()
        .format(gst_audio::AUDIO_FORMAT_S16)
        .rate_range(8_000..i32::MAX)
        .channels_range(1..i32::MAX)
        .build()
});

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: Duration,
    do_timestamp: bool,
    is_live: bool,
    buffer_duration: gst::ClockTime,
    num_buffers: Option<u32>,
    #[cfg(feature = "tuning")]
    is_main_elem: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            do_timestamp: DEFAULT_DO_TIMESTAMP,
            is_live: DEFAULT_IS_LIVE,
            buffer_duration: DEFAULT_BUFFER_DURATION,
            num_buffers: None,
            #[cfg(feature = "tuning")]
            is_main_elem: false,
        }
    }
}

#[derive(Clone, Debug)]
struct AudioTestSrcPadHandler;
impl PadSrcHandler for AudioTestSrcPadHandler {
    type ElementImpl = AudioTestSrc;

    fn src_query(self, pad: &gst::Pad, imp: &Self::ElementImpl, query: &mut gst::QueryRef) -> bool {
        gst::debug!(CAT, obj = pad, "Received {query:?}");

        if let gst::QueryViewMut::Latency(q) = query.view_mut() {
            let settings = imp.settings.lock().unwrap();
            let min_latency = if settings.is_live {
                settings.buffer_duration
            } else {
                gst::ClockTime::ZERO
            };

            q.set(
                settings.is_live,
                min_latency,
                min_latency
                    + runtime::Context::current().map_or(gst::ClockTime::ZERO, |ctx| {
                        gst::ClockTime::try_from(ctx.wait_duration()).unwrap()
                    }),
            );

            return true;
        }

        gst::Pad::query_default(pad, Some(&*imp.obj()), query)
    }
}

#[derive(Debug, Copy, Clone)]
enum Negotiation {
    Unchanged,
    Changed,
}

impl Negotiation {
    fn has_changed(self) -> bool {
        matches!(self, Negotiation::Changed)
    }
}

#[derive(Debug)]
struct AudioTestSrcTask {
    elem: super::AudioTestSrc,
    buffer_pool: gst::BufferPool,
    rate: u32,
    channels: usize,
    do_timestamp: bool,
    is_live: bool,
    buffer_duration: gst::ClockTime,
    need_initial_events: bool,
    step: f32,
    accumulator: f32,
    last_buffer_end: Option<gst::ClockTime>,
    caps: gst::Caps,
    buffer_count: u32,
    num_buffers: Option<u32>,
    #[cfg(feature = "tuning")]
    is_main_elem: bool,
    #[cfg(feature = "tuning")]
    parked_duration_init: Option<Duration>,
    #[cfg(feature = "tuning")]
    log_start: Instant,
}

impl AudioTestSrcTask {
    fn new(elem: super::AudioTestSrc) -> Self {
        AudioTestSrcTask {
            elem,
            buffer_pool: gst::BufferPool::new(),
            rate: DEFAULT_RATE,
            channels: DEFAULT_CHANNELS,
            do_timestamp: DEFAULT_DO_TIMESTAMP,
            is_live: DEFAULT_IS_LIVE,
            buffer_duration: DEFAULT_BUFFER_DURATION,
            need_initial_events: true,
            step: 0.0,
            accumulator: 0.0,
            last_buffer_end: None,
            caps: gst::Caps::new_empty(),
            buffer_count: 0,
            num_buffers: None,
            #[cfg(feature = "tuning")]
            is_main_elem: false,
            #[cfg(feature = "tuning")]
            parked_duration_init: None,
            #[cfg(feature = "tuning")]
            log_start: Instant::now(),
        }
    }

    async fn negotiate(&mut self) -> Result<Negotiation, gst::ErrorMessage> {
        let imp = self.elem.imp();
        let pad = imp.src_pad.gst_pad();

        if !pad.check_reconfigure() {
            return Ok(Negotiation::Unchanged);
        }

        let mut caps = pad.peer_query_caps(Some(&DEFAULT_CAPS));
        gst::debug!(CAT, imp = imp, "Peer returned {caps:?}");

        if caps.is_empty() {
            pad.mark_reconfigure();
            let err = gst::error_msg!(gst::CoreError::Pad, ["No common Caps"]);
            gst::error!(CAT, imp = imp, "{err}");
            return Err(err);
        }

        if caps.is_any() {
            gst::debug!(CAT, imp = imp, "Using our own Caps");
            caps = DEFAULT_CAPS.clone();
        }

        {
            let caps = caps.make_mut();
            let s = caps.structure_mut(0).ok_or_else(|| {
                let err = gst::error_msg!(gst::CoreError::Pad, ["Invalid peer Caps structure"]);
                gst::error!(CAT, imp = imp, "{err}");
                err
            })?;

            s.fixate_field_nearest_int("rate", DEFAULT_RATE as i32);
            self.rate = s.get::<i32>("rate").unwrap() as u32;
            self.step = 2.0 * std::f32::consts::PI * DEFAULT_FREQ / (self.rate as f32);

            s.fixate_field_nearest_int("channels", DEFAULT_CHANNELS as i32);
            self.channels = s.get::<i32>("channels").unwrap() as usize;

            if self.channels > 2 {
                s.set(
                    "channel-mask",
                    gst::Bitmask::from(gst_audio::AudioChannelPosition::fallback_mask(
                        self.channels as u32,
                    )),
                );
            }
        }

        caps.fixate();
        gst::debug!(CAT, imp = imp, "fixated to {caps:?}");

        imp.src_pad.push_event(gst::event::Caps::new(&caps)).await;

        self.caps = caps;

        Ok(Negotiation::Changed)
    }
}

impl TaskImpl for AudioTestSrcTask {
    type Item = gst::Buffer;

    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        gst::log!(CAT, obj = self.elem, "Preparing Task");

        let imp = self.elem.imp();
        let settings = imp.settings.lock().unwrap();
        self.do_timestamp = settings.do_timestamp;
        self.is_live = settings.is_live;
        self.buffer_duration = settings.buffer_duration;
        self.num_buffers = settings.num_buffers;

        #[cfg(feature = "tuning")]
        {
            self.is_main_elem = settings.is_main_elem;
        }

        future::ok(()).boxed()
    }

    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj = self.elem, "Starting Task");

            if self.need_initial_events {
                gst::debug!(CAT, obj = self.elem, "Pushing initial events");

                let stream_id =
                    format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
                let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                    .group_id(gst::GroupId::next())
                    .build();
                self.elem.imp().src_pad.push_event(stream_start_evt).await;
            }

            if self.negotiate().await?.has_changed() {
                let bytes_per_buffer = (self.rate as u64)
                    * self.buffer_duration.mseconds()
                    * self.channels as u64
                    * size_of::<i16>() as u64
                    / 1_000;

                let mut pool_config = self.buffer_pool.config();
                pool_config
                    .as_mut()
                    .set_params(Some(&self.caps), bytes_per_buffer as u32, 2, 6);
                self.buffer_pool.set_config(pool_config).unwrap();
            }

            assert!(!self.caps.is_empty());
            self.buffer_pool.set_active(true).unwrap();

            if self.need_initial_events {
                let segment_evt =
                    gst::event::Segment::new(&gst::FormattedSegment::<gst::format::Time>::new());
                self.elem.imp().src_pad.push_event(segment_evt).await;

                self.need_initial_events = false;
            }

            self.buffer_count = 0;

            #[cfg(feature = "tuning")]
            if self.is_main_elem {
                self.parked_duration_init = None;
            }

            Ok(())
        }
        .boxed()
    }

    fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        gst::log!(CAT, obj = self.elem, "Pausing Task");
        self.buffer_pool.set_active(false).unwrap();

        future::ok(()).boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        gst::log!(CAT, obj = self.elem, "Stopping Task");

        self.need_initial_events = true;
        self.accumulator = 0.0;
        self.last_buffer_end = None;

        future::ok(()).boxed()
    }

    fn try_next(&mut self) -> BoxFuture<'_, Result<gst::Buffer, gst::FlowError>> {
        let mut buffer = match self.buffer_pool.acquire_buffer(None) {
            Ok(buffer) => buffer,
            Err(err) => {
                gst::error!(CAT, obj = self.elem, "Failed to acquire buffer {}", err);
                return future::err(err).boxed();
            }
        };

        let buffer_mut = buffer.get_mut().unwrap();

        let start = if self.is_live | self.do_timestamp {
            self.last_buffer_end
                .or_else(|| self.elem.current_running_time())
        } else {
            None
        };

        {
            use std::io::Write;

            let mut mapped = buffer_mut.map_writable().unwrap();
            let slice = mapped.as_mut_slice();
            slice
                .chunks_mut(self.channels * size_of::<i16>())
                .for_each(|frame| {
                    let sample = ((self.accumulator.sin() * DEFAULT_VOLUME * (i16::MAX as f32))
                        as i16)
                        .to_ne_bytes();

                    frame.chunks_mut(size_of::<i16>()).for_each(|mut channel| {
                        let _ = channel.write(&sample).unwrap();
                    });

                    self.accumulator += self.step;
                    if self.accumulator >= 2.0 * std::f32::consts::PI {
                        self.accumulator = -2.0 * std::f32::consts::PI;
                    }
                });
        }

        if self.do_timestamp {
            buffer_mut.set_pts(start);
            buffer_mut.set_duration(self.buffer_duration);
        }

        self.last_buffer_end = start.opt_add(self.buffer_duration);

        async move {
            if self.is_live {
                if let Some(delay) = self
                    .last_buffer_end
                    .unwrap()
                    .checked_sub(self.elem.current_running_time().unwrap())
                {
                    // Wait for all samples to fit in last time slice
                    timer::delay_for_at_least(delay.into()).await;
                }
            } else {
                // Let the scheduler share time with other tasks
                runtime::executor::yield_now().await;
            }

            Ok(buffer)
        }
        .boxed()
    }

    fn handle_item(&mut self, buffer: gst::Buffer) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let imp = self.elem.imp();

            gst::debug!(CAT, imp = imp, "Pushing {buffer:?}");
            imp.src_pad.push(buffer).await?;
            gst::log!(CAT, imp = imp, "Successfully pushed buffer");

            self.buffer_count += 1;

            #[cfg(feature = "tuning")]
            if self.is_main_elem {
                if let Some(parked_duration_init) = self.parked_duration_init {
                    if self.buffer_count % LOG_BUFFER_INTERVAL == 0 {
                        let parked_duration =
                            runtime::Context::current().unwrap().parked_duration()
                                - parked_duration_init;

                        gst::info!(
                            CAT,
                            "Parked: {:5.2?}%",
                            parked_duration.as_nanos() as f32 * 100.0
                                / self.log_start.elapsed().as_nanos() as f32,
                        );
                    }
                } else if self.buffer_count == RAMPUP_BUFFER_COUNT {
                    self.parked_duration_init =
                        Some(runtime::Context::current().unwrap().parked_duration());
                    self.log_start = Instant::now();

                    gst::info!(CAT, "Ramp up complete");
                }
            }

            if self.num_buffers.opt_eq(self.buffer_count) == Some(true) {
                return Err(gst::FlowError::Eos);
            }

            Ok(())
        }
        .boxed()
    }

    fn handle_loop_error(&mut self, err: gst::FlowError) -> BoxFuture<'_, task::Trigger> {
        async move {
            match err {
                gst::FlowError::Flushing => {
                    gst::debug!(CAT, obj = self.elem, "Flushing");

                    task::Trigger::FlushStart
                }
                gst::FlowError::Eos => {
                    gst::debug!(CAT, obj = self.elem, "EOS");
                    self.elem
                        .imp()
                        .src_pad
                        .push_event(gst::event::Eos::new())
                        .await;

                    task::Trigger::Stop
                }
                err => {
                    gst::error!(CAT, obj = self.elem, "Got error {err}");
                    gst::element_error!(
                        &self.elem,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason {}", err]
                    );

                    task::Trigger::Error
                }
            }
        }
        .boxed()
    }
}

#[derive(Debug)]
pub struct AudioTestSrc {
    src_pad: PadSrc,
    task: Task,
    settings: Mutex<Settings>,
}

impl AudioTestSrc {
    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        let settings = self.settings.lock().unwrap();
        let context =
            runtime::Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;
        drop(settings);

        self.task
            .prepare(AudioTestSrcTask::new(self.obj().clone()), context)
            .block_on()?;

        gst::debug!(CAT, imp = self, "Prepared");

        Ok(())
    }

    fn unprepare(&self) {
        gst::debug!(CAT, imp = self, "Unpreparing");
        self.task.unprepare().block_on().unwrap();
        gst::debug!(CAT, imp = self, "Unprepared");
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping");
        self.task.stop().block_on()?;
        gst::debug!(CAT, imp = self, "Stopped");

        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting");
        self.task.start().block_on()?;
        gst::debug!(CAT, imp = self, "Started");

        Ok(())
    }

    fn pause(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Pausing");
        self.task.pause().block_on()?;
        gst::debug!(CAT, imp = self, "Paused");

        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AudioTestSrc {
    const NAME: &'static str = "TsAudioTestSrc";
    type Type = super::AudioTestSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap()),
                AudioTestSrcPadHandler,
            ),
            task: Task::default(),
            settings: Default::default(),
        }
    }
}

impl ObjectImpl for AudioTestSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("context")
                    .nick("Context")
                    .blurb("Context name to share threads with")
                    .default_value(Some(DEFAULT_CONTEXT))
                    .build(),
                glib::ParamSpecUInt::builder("context-wait")
                    .nick("Context Wait")
                    .blurb("Throttle poll loop to run at most once every this many ms")
                    .maximum(1000)
                    .default_value(DEFAULT_CONTEXT_WAIT.as_millis() as u32)
                    .build(),
                glib::ParamSpecBoolean::builder("do-timestamp")
                    .nick("Do timestamp")
                    .blurb("Apply current stream time to buffers")
                    .build(),
                glib::ParamSpecBoolean::builder("is-live")
                    .nick("Is live")
                    .blurb("Whether to act as a live source")
                    .build(),
                glib::ParamSpecUInt::builder("buffer-duration")
                    .nick("Buffer duration")
                    .blurb("Buffer duration in ms")
                    .default_value(DEFAULT_BUFFER_DURATION.mseconds() as u32)
                    .build(),
                glib::ParamSpecInt::builder("num-buffers")
                    .nick("Num Buffers")
                    .blurb("Number of buffers to output before sending EOS (-1 = unlimited)")
                    .minimum(-1i32)
                    .default_value(DEFAULT_NUM_BUFFERS)
                    .build(),
                #[cfg(feature = "tuning")]
                glib::ParamSpecBoolean::builder("main-elem")
                    .nick("Main Element")
                    .blurb("Declare this element as the main one")
                    .write_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => {
                settings.context = value
                    .get::<Option<String>>()
                    .unwrap()
                    .unwrap_or_else(|| DEFAULT_CONTEXT.into());
            }
            "context-wait" => {
                settings.context_wait = Duration::from_millis(value.get::<u32>().unwrap().into());
            }
            "do-timestamp" => {
                settings.do_timestamp = value.get::<bool>().unwrap();
            }
            "is-live" => {
                settings.is_live = value.get::<bool>().unwrap();
            }
            "buffer-duration" => {
                settings.buffer_duration = (value.get::<u32>().unwrap() as u64).mseconds();
            }
            "num-buffers" => {
                let value = value.get::<i32>().unwrap();
                settings.num_buffers = if value > 0 { Some(value as u32) } else { None };
            }
            #[cfg(feature = "tuning")]
            "main-elem" => {
                settings.is_main_elem = value.get::<bool>().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "do-timestamp" => settings.do_timestamp.to_value(),
            "is-live" => settings.is_live.to_value(),
            "buffer-duration" => (settings.buffer_duration.mseconds() as u32).to_value(),
            "num-buffers" => settings
                .num_buffers
                .and_then(|val| val.try_into().ok())
                .unwrap_or(-1i32)
                .to_value(),
            #[cfg(feature = "tuning")]
            "main-elem" => settings.is_main_elem.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(self.src_pad.gst_pad()).unwrap();
        obj.set_element_flags(gst::ElementFlags::SOURCE);
    }
}

impl GstObjectImpl for AudioTestSrc {}

impl ElementImpl for AudioTestSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing audio test source",
                "Source/Test",
                "Thread-sharing audio test source",
                "François Laignel <fengalin@free.fr>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &DEFAULT_CAPS,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare();
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                self.pause().map_err(|_| gst::StateChangeError)?;
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                self.start().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                self.stop().map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        Ok(success)
    }
}
