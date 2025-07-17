// Copyright (C) 2022 François Laignel <fengalin@free.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use byte_slice_cast::*;
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
const DEFAULT_SAMPLES_PER_BUFFER: u32 = 1024;
const DEFAULT_RATE: u32 = 44_100;
const DEFAULT_CHANNELS: usize = 1;
const DEFAULT_FREQ: u32 = 440;
const DEFAULT_VOLUME: f64 = 0.8;
const DEFAULT_MUTE: bool = false;
const DEFAULT_IS_LIVE: bool = false;
const DEFAULT_NUM_BUFFERS: i32 = -1;

#[cfg(feature = "tuning")]
const RAMPUP_BUFFER_COUNT: u32 = 500;
#[cfg(feature = "tuning")]
const LOG_BUFFER_INTERVAL: u32 = 2000;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: Duration,
    samples_per_buffer: u32,
    freq: u32,
    volume: f64,
    mute: bool,
    is_live: bool,
    num_buffers: Option<u32>,
    #[cfg(feature = "tuning")]
    is_main_elem: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            samples_per_buffer: DEFAULT_SAMPLES_PER_BUFFER,
            freq: DEFAULT_FREQ,
            volume: DEFAULT_VOLUME,
            mute: DEFAULT_MUTE,
            is_live: DEFAULT_IS_LIVE,
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

    fn src_query(
        self,
        pad: &gst::Pad,
        elem: &Self::ElementImpl,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst::debug!(CAT, obj = pad, "Received {query:?}");

        if query.is_serialized() {
            // See comment in runtime::pad::PadSrcHandler
            return false;
        }

        if let gst::QueryViewMut::Latency(q) = query.view_mut() {
            let rate = {
                let caps = elem.caps.lock().unwrap();
                let Some(caps) = caps.as_ref() else {
                    gst::debug!(CAT, imp = elem, "No caps yet");
                    return false;
                };

                let s = caps.structure(0).unwrap();
                s.get::<i32>("rate").expect("negotiated")
            };

            let settings = elem.settings.lock().unwrap();
            // timers can be up to 1/2 x context-wait late
            let context_wait = gst::ClockTime::try_from(settings.context_wait).unwrap();
            let latency = gst::ClockTime::SECOND
                .mul_div_floor(settings.samples_per_buffer as u64, rate as u64)
                .unwrap()
                + context_wait / 2;

            gst::debug!(CAT, imp = elem, "Returning latency {latency}");
            q.set(settings.is_live, latency, gst::ClockTime::NONE);

            return true;
        }

        gst::Pad::query_default(pad, Some(&*elem.obj()), query)
    }
}

#[derive(Debug)]
struct AudioTestSrcTask {
    elem: super::AudioTestSrc,
    segment: gst::FormattedSegment<gst::format::Time>,
    need_initial_events: bool,

    volume: f64,
    freq: f64,
    rate: u32,
    channels: usize,
    is_live: bool,
    samples_per_buffer: u32,
    bytes_per_buffer: usize,
    buffer_duration: gst::ClockTime,
    sample_offset: u64,
    sample_stop: Option<u64>,
    step: f64,
    accumulator: f64,

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
            segment: gst::FormattedSegment::<gst::format::Time>::new(),
            need_initial_events: true,

            volume: DEFAULT_VOLUME,
            freq: DEFAULT_FREQ as f64,
            rate: DEFAULT_RATE,
            channels: DEFAULT_CHANNELS,
            is_live: DEFAULT_IS_LIVE,
            bytes_per_buffer: (DEFAULT_SAMPLES_PER_BUFFER as usize)
                * DEFAULT_CHANNELS
                * size_of::<i16>(),
            samples_per_buffer: DEFAULT_SAMPLES_PER_BUFFER,
            buffer_duration: gst::ClockTime::SECOND
                .mul_div_floor(DEFAULT_SAMPLES_PER_BUFFER as u64, DEFAULT_RATE as u64)
                .unwrap(),
            sample_offset: 0,
            sample_stop: None,
            step: 0.0,
            accumulator: 0.0,

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

    async fn negotiate(&mut self) -> Result<(), gst::ErrorMessage> {
        let imp = self.elem.imp();
        let pad = imp.src_pad.gst_pad();

        if !pad.check_reconfigure() {
            return Ok(());
        }

        let pad_template = self.elem.pad_template("src").unwrap();
        let default_caps = pad_template.caps();
        let mut caps = pad.peer_query_caps(Some(default_caps));
        gst::debug!(CAT, imp = imp, "Peer returned {caps:?}");

        if caps.is_empty() {
            pad.mark_reconfigure();
            let err = gst::error_msg!(gst::CoreError::Pad, ["No common Caps"]);
            gst::error!(CAT, imp = imp, "{err}");
            return Err(err);
        }

        if caps.is_any() {
            gst::debug!(CAT, imp = imp, "Using our own Caps");
            caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format(gst_audio::AUDIO_FORMAT_S16)
                .channels(DEFAULT_CHANNELS as i32)
                .rate(DEFAULT_RATE as i32)
                .build();
        }

        self.set_caps(caps).await
    }

    async fn set_caps(&mut self, mut caps: gst::Caps) -> Result<(), gst::ErrorMessage> {
        use std::ops::Rem;

        let imp = self.elem.imp();
        gst::debug!(CAT, imp = imp, "Configuring for caps {caps}");

        {
            let caps = caps.make_mut();
            let s = caps.structure_mut(0).ok_or_else(|| {
                let err = gst::error_msg!(gst::CoreError::Pad, ["Invalid peer Caps structure"]);
                gst::error!(CAT, imp = imp, "{err}");
                err
            })?;

            let old_rate = self.rate as u64;
            s.fixate_field_nearest_int("rate", DEFAULT_RATE as i32);
            self.rate = s.get::<i32>("rate").unwrap() as u32;

            if self.rate != old_rate as u32 {
                self.elem.call_async(|elem| {
                    let _ = elem.post_message(gst::message::Latency::new());
                });
            }

            self.step = 2.0 * std::f64::consts::PI * self.freq / (self.rate as f64);

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

            // Update sample offset and accumulator based on the previous values and the
            // sample rate change, if any
            let old_sample_offset = self.sample_offset;
            let sample_offset = old_sample_offset
                .mul_div_floor(self.rate as u64, old_rate)
                .unwrap();

            let old_sample_stop = self.sample_stop;
            self.sample_stop =
                old_sample_stop.map(|v| v.mul_div_floor(self.rate as u64, old_rate).unwrap());

            self.accumulator = (sample_offset as f64).rem(self.step);

            self.buffer_duration = gst::ClockTime::SECOND
                .mul_div_floor(self.samples_per_buffer as u64, self.rate as u64)
                .unwrap();

            self.bytes_per_buffer =
                (self.samples_per_buffer as usize) * self.channels * size_of::<i16>();
        }

        caps.fixate();
        gst::debug!(CAT, imp = imp, "fixated to {caps:?}");

        imp.src_pad.push_event(gst::event::Caps::new(&caps)).await;

        *imp.caps.lock().unwrap() = Some(caps);

        Ok(())
    }
}

impl TaskImpl for AudioTestSrcTask {
    type Item = gst::Buffer;

    async fn prepare(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.elem, "Preparing Task");

        let imp = self.elem.imp();
        let settings = imp.settings.lock().unwrap();
        self.is_live = settings.is_live;
        self.samples_per_buffer = settings.samples_per_buffer;
        self.num_buffers = settings.num_buffers;
        self.freq = settings.freq as f64;

        #[cfg(feature = "tuning")]
        {
            self.is_main_elem = settings.is_main_elem;
        }

        Ok(())
    }

    async fn start(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.elem, "Starting Task");

        if self.need_initial_events {
            gst::debug!(CAT, obj = self.elem, "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            self.elem.imp().src_pad.push_event(stream_start_evt).await;
        }

        self.negotiate().await?;

        if self.need_initial_events {
            let segment_evt = gst::event::Segment::new(&self.segment);
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

    async fn pause(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.elem, "Pausing Task");

        Ok(())
    }

    async fn stop(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.elem, "Stopping Task");

        self.need_initial_events = true;
        self.sample_offset = 0;
        self.sample_stop = None;
        self.accumulator = 0.0;

        Ok(())
    }

    async fn try_next(&mut self) -> Result<gst::Buffer, gst::FlowError> {
        let Ok(mut buffer) = gst::Buffer::with_size(self.bytes_per_buffer) else {
            gst::error!(CAT, obj = self.elem, "Failed to create buffer");
            return Err(gst::FlowError::Flushing);
        };
        let buffer_mut = buffer.get_mut().unwrap();

        let n_samples = if let Some(sample_stop) = self.sample_stop {
            if sample_stop <= self.sample_offset {
                gst::log!(CAT, obj = self.elem, "At EOS");
                return Err(gst::FlowError::Eos);
            }

            sample_stop - self.sample_offset
        } else {
            self.samples_per_buffer as u64
        };

        let pts = self
            .sample_offset
            .mul_div_floor(*gst::ClockTime::SECOND, self.rate as u64)
            .map(gst::ClockTime::from_nseconds)
            .unwrap();
        let next_pts = (self.sample_offset + n_samples)
            .mul_div_floor(*gst::ClockTime::SECOND, self.rate as u64)
            .map(gst::ClockTime::from_nseconds)
            .unwrap();
        buffer_mut.set_pts(pts);
        buffer_mut.set_duration(next_pts - pts);

        {
            let mut mapped = buffer_mut.map_writable().unwrap();
            let data = mapped.as_mut_slice_of::<i16>().unwrap();
            for chunk in data.chunks_exact_mut(self.channels) {
                let value = (self.accumulator.sin() * self.volume * (i16::MAX as f64)) as i16;
                for sample in chunk {
                    *sample = value;
                }

                self.accumulator += self.step;
                if self.accumulator >= 2.0 * std::f64::consts::PI {
                    self.accumulator = -2.0 * std::f64::consts::PI;
                }
            }
        }

        self.sample_offset += n_samples;

        if self.is_live {
            let running_time = self
                .segment
                .to_running_time(buffer.pts().opt_add(buffer.duration()));

            let Some(cur_rt) = self.elem.current_running_time() else {
                // Let the scheduler share time with other tasks
                runtime::executor::yield_now().await;
                return Ok(buffer);
            };

            let Ok(Some(delay)) = running_time.opt_checked_sub(cur_rt) else {
                // Let the scheduler share time with other tasks
                runtime::executor::yield_now().await;
                return Ok(buffer);
            };

            // Wait for all samples to fit in last time slice
            timer::delay_for_at_least(delay.into()).await;
        } else {
            // Let the scheduler share time with other tasks
            runtime::executor::yield_now().await;
        }

        Ok(buffer)
    }

    async fn handle_item(&mut self, buffer: gst::Buffer) -> Result<(), gst::FlowError> {
        let imp = self.elem.imp();

        gst::debug!(CAT, imp = imp, "Pushing {buffer:?}");
        imp.src_pad.push(buffer).await?;
        gst::log!(CAT, imp = imp, "Successfully pushed buffer");

        self.buffer_count += 1;

        #[cfg(feature = "tuning")]
        if self.is_main_elem {
            if let Some(parked_duration_init) = self.parked_duration_init {
                if self.buffer_count % LOG_BUFFER_INTERVAL == 0 {
                    let parked_duration = runtime::Context::current().unwrap().parked_duration()
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

    async fn handle_loop_error(&mut self, err: gst::FlowError) -> task::Trigger {
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
}

#[derive(Debug)]
pub struct AudioTestSrc {
    src_pad: PadSrc,
    task: Task,
    caps: Mutex<Option<gst::Caps>>,
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
            caps: Default::default(),
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
                glib::ParamSpecUInt::builder("samples-per-buffer")
                    .nick("Samples Per Buffer")
                    .blurb("Number of samples per output buffer")
                    .minimum(1)
                    .default_value(DEFAULT_SAMPLES_PER_BUFFER)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("freq")
                    .nick("Frequency")
                    .blurb("Frequency")
                    .minimum(1)
                    .default_value(DEFAULT_FREQ)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("volume")
                    .nick("Volume")
                    .blurb("Output volume")
                    .maximum(10.0)
                    .default_value(DEFAULT_VOLUME)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("mute")
                    .nick("Mute")
                    .blurb("Mute")
                    .default_value(DEFAULT_MUTE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("is-live")
                    .nick("Is Live")
                    .blurb("(Pseudo) live output")
                    .default_value(DEFAULT_IS_LIVE)
                    .mutable_ready()
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
            "samples-per-buffer" => {
                let mut settings = self.settings.lock().unwrap();
                settings.samples_per_buffer = value.get().expect("type checked upstream");
                drop(settings);

                let _ = self
                    .obj()
                    .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
            }
            "freq" => {
                settings.freq = value.get().expect("type checked upstream");
            }
            "volume" => {
                settings.volume = value.get().expect("type checked upstream");
            }
            "mute" => {
                settings.mute = value.get().expect("type checked upstream");
            }
            "is-live" => {
                settings.is_live = value.get().expect("type checked upstream");
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
            "samples-per-buffer" => settings.samples_per_buffer.to_value(),
            "freq" => settings.freq.to_value(),
            "volume" => settings.volume.to_value(),
            "mute" => settings.mute.to_value(),
            "is-live" => settings.is_live.to_value(),
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
            let caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format(gst_audio::AUDIO_FORMAT_S16)
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
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
            gst::StateChange::PausedToReady => {
                self.stop().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare();
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                self.start().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }
}
