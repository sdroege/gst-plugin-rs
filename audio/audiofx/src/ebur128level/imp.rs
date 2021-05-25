// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info};
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use std::i32;
use std::sync::atomic;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use byte_slice_cast::*;

use smallvec::SmallVec;

use atomic_refcell::AtomicRefCell;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ebur128level",
        gst::DebugColorFlags::empty(),
        Some("EBU R128 Level"),
    )
});

#[glib::gflags("EbuR128LevelMode")]
enum Mode {
    #[gflags(name = "Calculate momentary loudness (400ms)", nick = "momentary")]
    MOMENTARY = 0b00000001,
    #[gflags(name = "Calculate short-term loudness (3s)", nick = "short-term")]
    SHORT_TERM = 0b00000010,
    #[gflags(
        name = "Calculate relative threshold and global loudness",
        nick = "global"
    )]
    GLOBAL = 0b00000100,
    #[gflags(name = "Calculate loudness range", nick = "loudness-range")]
    LOUDNESS_RANGE = 0b00001000,
    #[gflags(name = "Calculate sample peak", nick = "sample-peak")]
    SAMPLE_PEAK = 0b000100000,
    #[gflags(name = "Calculate true peak", nick = "true-peak")]
    TRUE_PEAK = 0b00100000,
}

impl From<Mode> for ebur128::Mode {
    fn from(mode: Mode) -> Self {
        // Should use histogram mode as otherwise the history will grow forever
        let mut ebur128_mode = ebur128::Mode::HISTOGRAM;
        if mode.contains(Mode::MOMENTARY) {
            ebur128_mode.set(ebur128::Mode::M, true);
        }
        if mode.contains(Mode::SHORT_TERM) {
            ebur128_mode.set(ebur128::Mode::S, true);
        }
        if mode.contains(Mode::GLOBAL) {
            ebur128_mode.set(ebur128::Mode::I, true);
        }
        if mode.contains(Mode::LOUDNESS_RANGE) {
            ebur128_mode.set(ebur128::Mode::LRA, true);
        }
        if mode.contains(Mode::SAMPLE_PEAK) {
            ebur128_mode.set(ebur128::Mode::SAMPLE_PEAK, true);
        }
        if mode.contains(Mode::TRUE_PEAK) {
            ebur128_mode.set(ebur128::Mode::TRUE_PEAK, true);
        }

        ebur128_mode
    }
}

const DEFAULT_MODE: Mode = Mode::all();
const DEFAULT_POST_MESSAGES: bool = true;
const DEFAULT_INTERVAL: gst::ClockTime = gst::ClockTime::SECOND;

#[derive(Debug, Clone, Copy)]
struct Settings {
    mode: Mode,
    post_messages: bool,
    interval: gst::ClockTime,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: DEFAULT_MODE,
            post_messages: DEFAULT_POST_MESSAGES,
            interval: DEFAULT_INTERVAL,
        }
    }
}

struct State {
    info: gst_audio::AudioInfo,
    ebur128: ebur128::EbuR128,
    num_frames: u64,
    interval_frames: gst::ClockTime,
    interval_frames_remaining: gst::ClockTime,
}

#[derive(Default)]
pub struct EbuR128Level {
    settings: Mutex<Settings>,
    state: AtomicRefCell<Option<State>>,
    reset: atomic::AtomicBool,
}

#[glib::object_subclass]
impl ObjectSubclass for EbuR128Level {
    const NAME: &'static str = "EbuR128Level";
    type Type = super::EbuR128Level;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for EbuR128Level {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder("reset", &[], glib::Type::UNIT.into())
                    .action()
                    .class_handler(|_token, args| {
                        let this = args[0].get::<super::EbuR128Level>().unwrap();
                        let imp = EbuR128Level::from_instance(&this);

                        gst_info!(CAT, obj: &this, "Resetting measurements",);
                        imp.reset.store(true, atomic::Ordering::SeqCst);

                        None
                    })
                    .build(),
            ]
        });

        &*SIGNALS
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_flags(
                    "mode",
                    "Mode",
                    "Selection of metrics to calculate",
                    Mode::static_type(),
                    DEFAULT_MODE.bits() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_boolean(
                    "post-messages",
                    "Post Messages",
                    "Whether to post messages on the bus for each interval",
                    DEFAULT_POST_MESSAGES,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_uint64(
                    "interval",
                    "Interval",
                    "Interval in nanoseconds for posting messages",
                    0,
                    u64::MAX - 1,
                    DEFAULT_INTERVAL.nseconds(),
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
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "mode" => {
                let mode = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing mode from {:?} to {:?}",
                    settings.mode,
                    mode
                );
                settings.mode = mode;
            }
            "post-messages" => {
                let post_messages = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing post-messages from {} to {}",
                    settings.post_messages,
                    post_messages
                );
                settings.post_messages = post_messages;
            }
            "interval" => {
                let interval =
                    gst::ClockTime::from_nseconds(value.get().expect("type checked upstream"));
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing interval from {} to {}",
                    settings.interval,
                    interval,
                );
                settings.interval = interval;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "mode" => settings.mode.to_value(),
            "post-messages" => settings.post_messages.to_value(),
            "interval" => settings.interval.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for EbuR128Level {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "EBU R128 Loudness Level Measurement",
                "Filter/Analyzer/Audio",
                "Measures different loudness metrics according to EBU R128",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("audio/x-raw")
                .field(
                    "format",
                    &gst::List::new(&[
                        &gst_audio::AUDIO_FORMAT_S16.to_str(),
                        &gst_audio::AUDIO_FORMAT_S32.to_str(),
                        &gst_audio::AUDIO_FORMAT_F32.to_str(),
                        &gst_audio::AUDIO_FORMAT_F64.to_str(),
                    ]),
                )
                .field(
                    "layout",
                    &gst::List::new(&[&"interleaved", &"non-interleaved"]),
                )
                // Limit from ebur128
                .field("rate", &gst::IntRange::<i32>::new(1, 2_822_400))
                // Limit from ebur128
                .field("channels", &gst::IntRange::<i32>::new(1, 64))
                .build();
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

impl BaseTransformImpl for EbuR128Level {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn set_caps(
        &self,
        element: &Self::Type,
        incaps: &gst::Caps,
        _outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        let info = match gst_audio::AudioInfo::from_caps(incaps) {
            Err(_) => return Err(gst::loggable_error!(CAT, "Failed to parse input caps")),
            Ok(info) => info,
        };

        gst_debug!(CAT, obj: element, "Configured for caps {}", incaps,);

        let settings = *self.settings.lock().unwrap();

        let mut ebur128 = ebur128::EbuR128::new(info.channels(), info.rate(), settings.mode.into())
            .map_err(|err| gst::loggable_error!(CAT, "Failed to create EBU R128: {}", err))?;

        // Map channel positions if we can to give correct weighting
        if let Some(positions) = info.positions() {
            let channel_map = positions
                .iter()
                .map(|p| {
                    match p {
                        gst_audio::AudioChannelPosition::Mono => ebur128::Channel::DualMono,
                        gst_audio::AudioChannelPosition::FrontLeft => ebur128::Channel::Left,
                        gst_audio::AudioChannelPosition::FrontRight => ebur128::Channel::Right,
                        gst_audio::AudioChannelPosition::FrontCenter => ebur128::Channel::Center,
                        gst_audio::AudioChannelPosition::Lfe1
                        | gst_audio::AudioChannelPosition::Lfe2 => ebur128::Channel::Unused,
                        gst_audio::AudioChannelPosition::RearLeft => ebur128::Channel::Mp135,
                        gst_audio::AudioChannelPosition::RearRight => ebur128::Channel::Mm135,
                        gst_audio::AudioChannelPosition::FrontLeftOfCenter => {
                            ebur128::Channel::MpSC
                        }
                        gst_audio::AudioChannelPosition::FrontRightOfCenter => {
                            ebur128::Channel::MmSC
                        }
                        gst_audio::AudioChannelPosition::RearCenter => ebur128::Channel::Mp180,
                        gst_audio::AudioChannelPosition::SideLeft => ebur128::Channel::Mp090,
                        gst_audio::AudioChannelPosition::SideRight => ebur128::Channel::Mm090,
                        gst_audio::AudioChannelPosition::TopFrontLeft => ebur128::Channel::Up030,
                        gst_audio::AudioChannelPosition::TopFrontRight => ebur128::Channel::Um030,
                        gst_audio::AudioChannelPosition::TopFrontCenter => ebur128::Channel::Up000,
                        gst_audio::AudioChannelPosition::TopCenter => ebur128::Channel::Tp000,
                        gst_audio::AudioChannelPosition::TopRearLeft => ebur128::Channel::Up135,
                        gst_audio::AudioChannelPosition::TopRearRight => ebur128::Channel::Um135,
                        gst_audio::AudioChannelPosition::TopSideLeft => ebur128::Channel::Up090,
                        gst_audio::AudioChannelPosition::TopSideRight => ebur128::Channel::Um090,
                        gst_audio::AudioChannelPosition::TopRearCenter => ebur128::Channel::Up180,
                        gst_audio::AudioChannelPosition::BottomFrontCenter => {
                            ebur128::Channel::Bp000
                        }
                        gst_audio::AudioChannelPosition::BottomFrontLeft => ebur128::Channel::Bp045,
                        gst_audio::AudioChannelPosition::BottomFrontRight => {
                            ebur128::Channel::Bm045
                        }
                        gst_audio::AudioChannelPosition::WideLeft => {
                            ebur128::Channel::Mp135 // Mp110?
                        }
                        gst_audio::AudioChannelPosition::WideRight => {
                            ebur128::Channel::Mm135 // Mm110?
                        }
                        gst_audio::AudioChannelPosition::SurroundLeft => {
                            ebur128::Channel::Mp135 // Mp110?
                        }
                        gst_audio::AudioChannelPosition::SurroundRight => {
                            ebur128::Channel::Mm135 // Mm110?
                        }
                        gst_audio::AudioChannelPosition::Invalid
                        | gst_audio::AudioChannelPosition::None => ebur128::Channel::Unused,
                        val => {
                            gst_debug!(
                                CAT,
                                obj: element,
                                "Unknown channel position {:?}, ignoring channel",
                                val
                            );
                            ebur128::Channel::Unused
                        }
                    }
                })
                .collect::<Vec<_>>();
            ebur128
                .set_channel_map(&channel_map)
                .map_err(|err| gst::loggable_error!(CAT, "Failed to set channel map: {}", err))?;
        } else {
            // Weight all channels equally if we have no channel map
            let channel_map = std::iter::repeat(ebur128::Channel::Center)
                .take(info.channels() as usize)
                .collect::<Vec<_>>();
            ebur128
                .set_channel_map(&channel_map)
                .map_err(|err| gst::loggable_error!(CAT, "Failed to set channel map: {}", err))?;
        }

        let interval_frames = settings
            .interval
            .mul_div_floor(info.rate() as u64, *gst::ClockTime::SECOND)
            .unwrap();

        *self.state.borrow_mut() = Some(State {
            info,
            ebur128,
            num_frames: 0,
            interval_frames,
            interval_frames_remaining: interval_frames,
        });

        Ok(())
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.borrow_mut().take();

        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn transform_ip_passthrough(
        &self,
        element: &Self::Type,
        buf: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = *self.settings.lock().unwrap();

        let mut state_guard = self.state.borrow_mut();
        let mut state = state_guard.as_mut().ok_or_else(|| {
            gst::element_error!(element, gst::CoreError::Negotiation, ["Have no state yet"]);
            gst::FlowError::NotNegotiated
        })?;

        let mut timestamp = buf.pts();
        let segment = element.segment().downcast::<gst::ClockTime>().ok();

        let buf = gst_audio::AudioBufferRef::from_buffer_ref_readable(&buf, &state.info).map_err(
            |_| {
                gst::element_error!(element, gst::ResourceError::Read, ["Failed to map buffer"]);
                gst::FlowError::Error
            },
        )?;

        let mut frames = Frames::from_audio_buffer(element, &buf)?;
        while frames.num_frames() > 0 {
            if self
                .reset
                .compare_exchange(
                    true,
                    false,
                    atomic::Ordering::SeqCst,
                    atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                state.ebur128.reset();
                state.interval_frames_remaining = state.interval_frames;
                state.num_frames = 0;
            }

            let to_process = u64::min(
                state.interval_frames_remaining.nseconds(),
                frames.num_frames() as u64,
            );

            frames
                .process(to_process, &mut state.ebur128)
                .map_err(|err| {
                    gst::element_error!(
                        element,
                        gst::ResourceError::Read,
                        ["Failed to process buffer: {}", err]
                    );
                    gst::FlowError::Error
                })?;

            state.interval_frames_remaining -= gst::ClockTime::from_nseconds(to_process);
            state.num_frames += to_process;

            // The timestamp we report in messages is always the timestamp until which measurements
            // are included, not the starting timestamp.
            timestamp = timestamp.map(|ts| {
                ts + to_process
                    .mul_div_floor(*gst::ClockTime::SECOND, state.info.rate() as u64)
                    .map(gst::ClockTime::from_nseconds)
                    .unwrap()
            });

            // Post a message whenever an interval is full
            if state.interval_frames_remaining.is_zero() {
                state.interval_frames_remaining = state.interval_frames;

                if settings.post_messages {
                    let running_time = segment.as_ref().and_then(|s| s.to_running_time(timestamp));
                    let stream_time = segment.as_ref().and_then(|s| s.to_stream_time(timestamp));

                    let mut s = gst::Structure::builder("ebur128-level")
                        .field("timestamp", &timestamp)
                        .field("running-time", &running_time)
                        .field("stream-time", &stream_time)
                        .build();

                    if state.ebur128.mode().contains(ebur128::Mode::M) {
                        match state.ebur128.loudness_momentary() {
                            Ok(loudness) => s.set("momentary-loudness", &loudness),
                            Err(err) => gst_error!(
                                CAT,
                                obj: element,
                                "Failed to get momentary loudness: {}",
                                err
                            ),
                        }
                    }

                    if state.ebur128.mode().contains(ebur128::Mode::S) {
                        match state.ebur128.loudness_shortterm() {
                            Ok(loudness) => s.set("shortterm-loudness", &loudness),
                            Err(err) => gst_error!(
                                CAT,
                                obj: element,
                                "Failed to get shortterm loudness: {}",
                                err
                            ),
                        }
                    }

                    if state.ebur128.mode().contains(ebur128::Mode::I) {
                        match state.ebur128.loudness_global() {
                            Ok(loudness) => s.set("global-loudness", &loudness),
                            Err(err) => gst_error!(
                                CAT,
                                obj: element,
                                "Failed to get global loudness: {}",
                                err
                            ),
                        }

                        match state.ebur128.relative_threshold() {
                            Ok(threshold) => s.set("relative-threshold", &threshold),
                            Err(err) => gst_error!(
                                CAT,
                                obj: element,
                                "Failed to get relative threshold: {}",
                                err
                            ),
                        }
                    }

                    if state.ebur128.mode().contains(ebur128::Mode::LRA) {
                        match state.ebur128.loudness_range() {
                            Ok(range) => s.set("loudness-range", &range),
                            Err(err) => gst_error!(
                                CAT,
                                obj: element,
                                "Failed to get loudness range: {}",
                                err
                            ),
                        }
                    }

                    if state.ebur128.mode().contains(ebur128::Mode::SAMPLE_PEAK) {
                        let peaks = (0..state.info.channels())
                            .map(|c| state.ebur128.sample_peak(c).map(|p| p.to_send_value()))
                            .collect::<Result<Vec<_>, _>>();

                        match peaks {
                            Ok(peaks) => s.set("sample-peak", &gst::Array::from_owned(peaks)),
                            Err(err) => {
                                gst_error!(CAT, obj: element, "Failed to get sample peaks: {}", err)
                            }
                        }
                    }

                    if state.ebur128.mode().contains(ebur128::Mode::TRUE_PEAK) {
                        let peaks = (0..state.info.channels())
                            .map(|c| state.ebur128.true_peak(c).map(|p| p.to_send_value()))
                            .collect::<Result<Vec<_>, _>>();

                        match peaks {
                            Ok(peaks) => s.set("true-peak", &gst::Array::from_owned(peaks)),
                            Err(err) => {
                                gst_error!(CAT, obj: element, "Failed to get true peaks: {}", err)
                            }
                        }
                    }

                    gst_debug!(CAT, obj: element, "Posting message {}", s);

                    let msg = gst::message::Element::builder(s).src(element).build();

                    // Release lock while posting the message to avoid deadlocks
                    drop(state_guard);

                    let _ = element.post_message(msg);

                    state_guard = self.state.borrow_mut();
                    state = state_guard.as_mut().ok_or_else(|| {
                        gst::element_error!(
                            element,
                            gst::CoreError::Negotiation,
                            ["Have no state yet"]
                        );
                        gst::FlowError::NotNegotiated
                    })?;
                }
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

/// Helper struct to handle the different sample formats and layouts generically.
enum Frames<'a> {
    S16(&'a [i16], usize),
    S32(&'a [i32], usize),
    F32(&'a [f32], usize),
    F64(&'a [f64], usize),
    S16P(SmallVec<[&'a [i16]; 64]>),
    S32P(SmallVec<[&'a [i32]; 64]>),
    F32P(SmallVec<[&'a [f32]; 64]>),
    F64P(SmallVec<[&'a [f64]; 64]>),
}

impl<'a> Frames<'a> {
    /// Create a new frames wrapper that allows chunked processing.
    fn from_audio_buffer(
        element: &super::EbuR128Level,
        buf: &'a gst_audio::AudioBufferRef<&'a gst::BufferRef>,
    ) -> Result<Self, gst::FlowError> {
        match (buf.format(), buf.layout()) {
            (gst_audio::AUDIO_FORMAT_S16, gst_audio::AudioLayout::Interleaved) => Ok(Frames::S16(
                interleaved_channel_data_into_slice(element, buf)?,
                buf.channels() as usize,
            )),
            (gst_audio::AUDIO_FORMAT_S32, gst_audio::AudioLayout::Interleaved) => Ok(Frames::S32(
                interleaved_channel_data_into_slice(element, buf)?,
                buf.channels() as usize,
            )),
            (gst_audio::AUDIO_FORMAT_F32, gst_audio::AudioLayout::Interleaved) => Ok(Frames::F32(
                interleaved_channel_data_into_slice(element, buf)?,
                buf.channels() as usize,
            )),
            (gst_audio::AUDIO_FORMAT_F64, gst_audio::AudioLayout::Interleaved) => Ok(Frames::F64(
                interleaved_channel_data_into_slice(element, buf)?,
                buf.channels() as usize,
            )),
            (gst_audio::AUDIO_FORMAT_S16, gst_audio::AudioLayout::NonInterleaved) => Ok(
                Frames::S16P(non_interleaved_channel_data_into_slices(element, buf)?),
            ),
            (gst_audio::AUDIO_FORMAT_S32, gst_audio::AudioLayout::NonInterleaved) => Ok(
                Frames::S32P(non_interleaved_channel_data_into_slices(element, buf)?),
            ),
            (gst_audio::AUDIO_FORMAT_F32, gst_audio::AudioLayout::NonInterleaved) => Ok(
                Frames::F32P(non_interleaved_channel_data_into_slices(element, buf)?),
            ),
            (gst_audio::AUDIO_FORMAT_F64, gst_audio::AudioLayout::NonInterleaved) => Ok(
                Frames::F64P(non_interleaved_channel_data_into_slices(element, buf)?),
            ),
            _ => Err(gst::FlowError::NotNegotiated),
        }
    }

    /// Get the number of remaining frames.
    fn num_frames(&self) -> usize {
        match self {
            Frames::S16(frames, channels) => frames.len() / channels,
            Frames::S32(frames, channels) => frames.len() / channels,
            Frames::F32(frames, channels) => frames.len() / channels,
            Frames::F64(frames, channels) => frames.len() / channels,
            Frames::S16P(frames) => frames[0].len(),
            Frames::S32P(frames) => frames[0].len(),
            Frames::F32P(frames) => frames[0].len(),
            Frames::F64P(frames) => frames[0].len(),
        }
    }

    /// Process `num_frames` with `ebur128` and advance to the next frames.
    fn process(
        &mut self,
        num_frames: u64,
        ebur128: &mut ebur128::EbuR128,
    ) -> Result<(), ebur128::Error> {
        match self {
            Frames::S16(frames, channels) => {
                let (first, second) = frames.split_at(num_frames as usize * *channels);
                ebur128.add_frames_i16(first)?;
                *frames = second;

                Ok(())
            }
            Frames::S32(frames, channels) => {
                let (first, second) = frames.split_at(num_frames as usize * *channels);
                ebur128.add_frames_i32(first)?;
                *frames = second;

                Ok(())
            }
            Frames::F32(frames, channels) => {
                let (first, second) = frames.split_at(num_frames as usize * *channels);
                ebur128.add_frames_f32(first)?;
                *frames = second;

                Ok(())
            }
            Frames::F64(frames, channels) => {
                let (first, second) = frames.split_at(num_frames as usize * *channels);
                ebur128.add_frames_f64(first)?;
                *frames = second;

                Ok(())
            }
            Frames::S16P(channels) => {
                let (first, second) = split_vec(channels, num_frames as usize);
                ebur128.add_frames_planar_i16(&first)?;
                *channels = second;

                Ok(())
            }
            Frames::S32P(channels) => {
                let (first, second) = split_vec(channels, num_frames as usize);
                ebur128.add_frames_planar_i32(&first)?;
                *channels = second;

                Ok(())
            }
            Frames::F32P(channels) => {
                let (first, second) = split_vec(channels, num_frames as usize);
                ebur128.add_frames_planar_f32(&first)?;
                *channels = second;

                Ok(())
            }
            Frames::F64P(channels) => {
                let (first, second) = split_vec(channels, num_frames as usize);
                ebur128.add_frames_planar_f64(&first)?;
                *channels = second;

                Ok(())
            }
        }
    }
}

/// Converts an interleaved audio buffer into a typed slice.
fn interleaved_channel_data_into_slice<'a, T: FromByteSlice>(
    element: &super::EbuR128Level,
    buf: &'a gst_audio::AudioBufferRef<&gst::BufferRef>,
) -> Result<&'a [T], gst::FlowError> {
    buf.plane_data(0)
        .map_err(|err| {
            gst_error!(CAT, obj: element, "Failed to get audio data: {}", err);
            gst::FlowError::Error
        })?
        .as_slice_of::<T>()
        .map_err(|err| {
            gst_error!(CAT, obj: element, "Failed to handle audio data: {}", err);
            gst::FlowError::Error
        })
}

/// Converts a non-interleaved audio buffer into a vector of typed slices.
fn non_interleaved_channel_data_into_slices<'a, T: FromByteSlice>(
    element: &super::EbuR128Level,
    buf: &'a gst_audio::AudioBufferRef<&gst::BufferRef>,
) -> Result<SmallVec<[&'a [T]; 64]>, gst::FlowError> {
    (0..buf.channels())
        .map(|c| {
            buf.plane_data(c)
                .map_err(|err| {
                    gst_error!(CAT, obj: element, "Failed to get audio data: {}", err);
                    gst::FlowError::Error
                })?
                .as_slice_of::<T>()
                .map_err(|err| {
                    gst_error!(CAT, obj: element, "Failed to handle audio data: {}", err);
                    gst::FlowError::Error
                })
        })
        .collect::<Result<_, _>>()
}

/// Split a vector of slices into a tuple of slices with each slice split at `split_at`.
#[allow(clippy::type_complexity)]
fn split_vec<'a, 'b, T: Copy>(
    vec: &'b SmallVec<[&'a [T]; 64]>,
    split_at: usize,
) -> (SmallVec<[&'a [T]; 64]>, SmallVec<[&'a [T]; 64]>) {
    let VecPair(first, second) = vec
        .iter()
        .map(|vec| vec.split_at(split_at))
        .collect::<VecPair<_>>();
    (first, second)
}

/// Helper struct to collect from an iterator on pairs into two vectors.
struct VecPair<T>(SmallVec<[T; 64]>, SmallVec<[T; 64]>);

impl<T> std::iter::FromIterator<(T, T)> for VecPair<T> {
    fn from_iter<I: IntoIterator<Item = (T, T)>>(iter: I) -> Self {
        let mut first_vec = SmallVec::new();
        let mut second_vec = SmallVec::new();
        for (first, second) in iter {
            first_vec.push(first);
            second_vec.push(second);
        }

        VecPair(first_vec, second_vec)
    }
}
