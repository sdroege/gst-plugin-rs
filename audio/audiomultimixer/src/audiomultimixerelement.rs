// GStreamer multi audio mixer element
//
// Copyright (C) 2022-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// ---------------------------------------------------------------------------
// Internal AudioAggregator-based mixer element that creates output buffers
// containing all the output channels for all outputs as per the per-output
// mix specification.
//
// A separate internal splitter element will then split these buffers and send
// only the relevant channels to each output.
//
//
// +---------------------------- outputs ------------------------------------+
// |               src_00   src_01   src_02
// |               ch0 ch1  ch0 ch1  ch0 ch1
// |
// | sink_00 ch0   0.0 0.0  0.5 0.0  0.5 0.0
// |         ch1   0.0 0.0  0.0 0.5  0.0 0.5
// |
// | sink_01 ch0   0.5 0.0  0.0 0.0  0.5 0.0
// |         ch1   0.0 0.5  0.0 0.0  0.0 0.5
// |
// | sink_02 ch0   0.5 0.0  0.5 0.0  0.0 0.0
// |         ch1   0.0 0.5  0.0 0.5  0.0 0.0
// |
// i sink_03 ch0   0.0 0.0  0.0 0.0  0.0 0.0
// n         ch1   0.0 0.0  0.0 0.0  0.0 0.0
// p
// u
// t
// s
//
// |
// |
// +
//
// mix_00,
//   channel_0=(structure)channel-mix,sink_01c0=1.0,sink_02c0=1.0,
//   channel_1=(structure)channel-mix,sink_01c1=1.0,sink_02c1=1.0,
//
// ---------------------------------------------------------------------------

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_audio::AudioFormat;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;

use std::iter::zip;
use std::num::NonZeroU32;
use std::sync::{Arc, LazyLock, Mutex};

use byte_slice_cast::*;

use crate::splitmeta::{OutputChannelSplit, SplitMeta};

#[derive(Debug)]
pub struct MixContribution {
    input: u32,
    channel: u32,
}

#[derive(Debug)]
pub struct OutputChannelContributions {
    contributions: Vec<MixContribution>,
}

impl OutputChannelContributions {
    fn new() -> Self {
        Self {
            contributions: vec![],
        }
    }

    fn clear(&mut self) {
        self.contributions.clear();
    }

    #[allow(dead_code)]
    fn remove_contributions_for_input(&mut self, input: u32) {
        self.contributions
            .retain(|contribution| contribution.input != input);
    }

    fn add_contribution(&mut self, input: u32, channel: u32) {
        // If we already have an existing contribution entry, update that
        for contribution in &mut self.contributions {
            if contribution.input == input && contribution.channel == channel {
                return;
            }
        }
        self.contributions.push(MixContribution { input, channel });
    }
}

#[derive(Debug)]
pub struct OutputMix {
    // Output pad number
    output_num: u32,

    channels: Vec<OutputChannelContributions>,
}

impl OutputMix {
    pub(crate) fn new(output_num: u32, n_channels: NonZeroU32) -> Self {
        let n_channels = n_channels.get();

        let mut channels = Vec::with_capacity(n_channels as usize);
        for _ in 0..n_channels {
            channels.push(OutputChannelContributions::new());
        }

        Self {
            output_num,
            channels,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn clear(&mut self) {
        for channel in &mut self.channels {
            channel.clear();
        }
    }

    pub(crate) fn add_mono_contribution(&mut self, input: u32) {
        self.channels[0].add_contribution(input, 0);
    }

    // Can probably make this nicer, and/or more generic, but for now everything is stereo
    #[allow(dead_code)]
    pub(crate) fn add_stereo_contribution(&mut self, input: u32) {
        self.channels[0].add_contribution(input, 0);
        self.channels[1].add_contribution(input, 1);
    }

    #[allow(dead_code)]
    pub(crate) fn n_contributions(&self) -> usize {
        self.channels
            .iter()
            .map(|output_channel_contribution| output_channel_contribution.contributions.len())
            .max()
            .unwrap_or(0)
    }
}

#[derive(Default, Debug)]
pub struct OutputConfiguration {
    outputs: Vec<OutputMix>,
}

impl OutputConfiguration {
    pub(crate) fn add_output_mix(&mut self, mix: OutputMix) {
        for output in &mut self.outputs {
            if output.output_num == mix.output_num {
                *output = mix;
                return;
            }
        }
        self.outputs.push(mix);
    }
}

#[derive(Default)]
struct State {
    // Caps of the first sinkpad. This defines the allowed caps
    // of all sink pads as well as the srcpad caps
    configured_info: Option<gst_audio::AudioInfo>,

    output_config: OutputConfiguration,

    pending_config: Option<OutputConfiguration>,
}

impl OutputConfiguration {
    fn n_output_channels(&self) -> usize {
        self.outputs
            .iter()
            .fold(0, |acc, output| acc + output.channels.len())
    }

    // IMPROVE: maybe use SmallVec? or perhaps we could rewrite this as an iterator?
    pub(crate) fn get_output_contributions_for_input_channel(
        &self,
        input: u32,
        channel: u32,
    ) -> Vec<bool> {
        let out_channels = self.n_output_channels();

        let mut out_contribs = vec![false; out_channels];

        let mut i = 0; // output channel index (in output buffer, across all outputs)

        for out_mix in &self.outputs {
            for out_chan in &out_mix.channels {
                for mix_contrib in &out_chan.contributions {
                    if mix_contrib.input == input && mix_contrib.channel == channel {
                        out_contribs[i] = true;
                    }
                }
                i += 1;
            }
        }

        out_contribs
    }
}

#[derive(Default)]
pub struct MultiMixerElement {
    state: Arc<Mutex<State>>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "audiomultimixerelement",
        gst::DebugColorFlags::empty(),
        Some("Audio multi mixer element"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for MultiMixerElement {
    const NAME: &'static str = "GstAudioMultiMixerElement";
    type Type = super::MultiMixerElement;
    type ParentType = gst_audio::AudioAggregator;
}

impl ObjectImpl for MultiMixerElement {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj().set_ignore_inactive_pads(true);
    }
}

impl GstObjectImpl for MultiMixerElement {}

impl ElementImpl for MultiMixerElement {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Audio multi mixer element",
                "Generic/Audio",
                "Audio multi mixer element",
                "Tim-Philipp Müller <tim@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        // TODO: allow other different channel counts, and add more formats
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let input_caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format_list([gst_audio::AUDIO_FORMAT_S16, gst_audio::AUDIO_FORMAT_F32])
                .rate_range(1..)
                .channels(1) // only support mono for now
                .build();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &input_caps,
                gst_audio::AudioAggregatorPad::static_type(),
            )
            .unwrap();

            // We always use f32 as the internal mixing format for now regardless of the input format
            let output_caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format(gst_audio::AUDIO_FORMAT_F32)
                .rate_range(1..)
                .channels_range(1..i16::MAX as i32)
                .build();

            let src_pad_template = gst::PadTemplate::with_gtype(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &output_caps,
                gst_audio::AudioAggregatorPad::static_type(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AggregatorImpl for MultiMixerElement {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.configured_info = None;
        drop(state);

        self.parent_start()
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.configured_info = None;
        drop(state);

        self.parent_stop()
    }

    fn sink_event(&self, aggregator_pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Caps(ev) => {
                let caps = ev.caps_owned();

                let Ok(info) = gst_audio::AudioInfo::from_caps(&caps) else {
                    return false;
                };

                gst::info!(CAT, obj = aggregator_pad, "Have caps {caps}");

                let mut state = self.state.lock().unwrap();
                if let Some(ref configured_info) = state.configured_info {
                    gst::info!(
                        CAT,
                        obj = aggregator_pad,
                        "Configured info {configured_info:?}"
                    );
                    // All sink pads must have the same rate and channel count (but format can differ)
                    if info.rate() != configured_info.rate()
                        || info.channels() != configured_info.channels()
                    {
                        return false;
                    }
                } else {
                    state.configured_info = Some(info);
                }

                self.obj().set_sink_caps(
                    aggregator_pad
                        .downcast_ref::<gst_audio::AudioAggregatorPad>()
                        .unwrap(),
                    &caps,
                );

                true
            }
            _ => self.parent_sink_event(aggregator_pad, event),
        }
    }

    fn sink_query(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let filter = q.filter();

                let state = self.state.lock().unwrap();
                let mut allowed_caps = if let Some(ref configured_info) = state.configured_info {
                    // TODO: would be nice to support stereo, but for now same restriction as below
                    assert_eq!(configured_info.channels(), 1);
                    let caps = configured_info.to_caps().unwrap();
                    drop(state);
                    caps
                } else {
                    drop(state);

                    let caps = self.obj().src_pad().peer_query_caps(None);

                    let caps = caps.intersect_with_mode(
                        &self.obj().src_pad().pad_template_caps(),
                        gst::CapsIntersectMode::First,
                    );

                    if caps.is_empty() {
                        q.set_result(&caps);
                        return true;
                    }

                    caps
                };

                let allowed_caps = allowed_caps.make_mut();
                for s in allowed_caps.iter_mut() {
                    s.set("channels", 1i32); // TODO: would be nice to support stereo
                    s.remove_field("channel-mask");
                    // we accept any input format, not just our mixing format
                    s.remove_field("format");
                }

                let allowed_caps = allowed_caps.intersect_with_mode(
                    &aggregator_pad.pad_template_caps(),
                    gst::CapsIntersectMode::First,
                );

                if let Some(filter) = filter {
                    q.set_result(
                        &filter.intersect_with_mode(&allowed_caps, gst::CapsIntersectMode::First),
                    );
                } else {
                    q.set_result(&allowed_caps);
                }

                true
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                let allowed_caps = aggregator_pad.query_caps(None);
                let caps = q.caps();
                q.set_result(allowed_caps.can_intersect(caps));
                true
            }
            _ => self.parent_sink_query(aggregator_pad, query),
        }
    }

    fn src_query(&self, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let templ = self.obj().src_pad().pad_template_caps();
                let filter = q.filter();

                if let Some(filter) = filter {
                    q.set_result(
                        &filter.intersect_with_mode(&templ, gst::CapsIntersectMode::First),
                    );
                } else {
                    q.set_result(&templ);
                }

                true
            }
            _ => self.parent_src_query(query),
        }
    }

    fn negotiate(&self) -> bool {
        if audio_aggregator_has_pending_output_buffer_hack(self.obj().upcast_ref()) {
            gst::info!(
                CAT,
                imp = self,
                "Have pending output buffer, renegotiating afterwards"
            );
            self.obj().src_pad().mark_reconfigure();
            return true;
        }

        let mut state = self.state.lock().unwrap();

        if let Some(pending_config) = state.pending_config.take() {
            gst::info!(CAT, imp = self, "updating config to: {:?}", pending_config);
            state.output_config = pending_config;
        }

        let out_channels = state.output_config.n_output_channels() as i32;

        gst::info!(CAT, imp = self, "out_channels: {}", out_channels);

        // We require the same caps on every sinkpad so just take the caps of the first
        // sinkpad and update those for the srcpad
        let Some(mut output_caps) = state
            .configured_info
            .as_ref()
            .and_then(|info| info.to_caps().ok())
        else {
            // FIXME: Should probably generate default caps here for force-live?
            gst::warning!(CAT, imp = self, "Have no caps yet");
            return false;
        };

        let mut_ref = output_caps.get_mut().unwrap();
        // out_channels could be 0 in case of force-live=true and there are no inputs yet,
        // and while we could handle that fine the audio aggregator base class does not.
        mut_ref.set("channels", std::cmp::max(1, out_channels));
        mut_ref.set("channel-mask", gst::Bitmask::new(0));
        // Our internal mixing format is always f32
        mut_ref.set("format", gst_audio::AUDIO_FORMAT_F32.to_str());

        drop(state);

        gst::debug!(CAT, imp = self, "Setting caps {output_caps:?}");

        //if self.negotiated_src_caps(&output_caps).is_ok() {
        if audio_aggregator_negotiated_src_caps_hack(self.obj().upcast_ref(), &output_caps) {
            self.obj().set_src_caps(&output_caps);

            // TODO: Should also call self.prepare_allocator() here now, needs
            // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/11425

            true
        } else {
            false
        }
    }
}

#[repr(C)]
struct Priv {
    mutex: glib::ffi::GMutex,

    alignment_threshold: gst::ffi::GstClockTime,
    discont_wait: gst::ffi::GstClockTime,

    output_buffer_duration_n: i32,
    output_buffer_duration_d: i32,

    samples_per_buffer: u32,
    error_per_buffer: u32,
    accumulated_error: u32,
    current_blocksize: u32,

    current_buffer: *mut gst::ffi::GstBuffer,
    // FIXME: cut off
}

// FIXME: Workaround for https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/11486
fn audio_aggregator_has_pending_output_buffer_hack(agg: &gst_audio::AudioAggregator) -> bool {
    unsafe {
        let ptr = agg.as_ptr();
        let priv_ = (*ptr).priv_ as *mut Priv;

        glib::ffi::g_mutex_lock(&mut (*priv_).mutex);
        glib::ffi::g_mutex_lock(&mut (*(ptr as *mut gst::ffi::GstObject)).lock);

        // There must be no buffer pending when we change caps
        let res = !(*priv_).current_buffer.is_null();

        glib::ffi::g_mutex_unlock(&mut (*(ptr as *mut gst::ffi::GstObject)).lock);
        glib::ffi::g_mutex_unlock(&mut (*priv_).mutex);

        res
    }
}

// FIXME: Workaround for audioaggregator problems fixed in 1.28.3
// https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/11477
// https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/11482
// https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/11484
fn audio_aggregator_negotiated_src_caps_hack(
    agg: &gst_audio::AudioAggregator,
    caps: &gst::Caps,
) -> bool {
    let info = gst_audio::AudioInfo::from_caps(caps).unwrap();

    unsafe {
        use glib::translate::*;

        let ptr = agg.as_ptr();
        let priv_ = (*ptr).priv_ as *mut Priv;

        glib::ffi::g_mutex_lock(&mut (*priv_).mutex);
        glib::ffi::g_mutex_lock(&mut (*(ptr as *mut gst::ffi::GstObject)).lock);
        let srcpad = (*ptr).parent.srcpad as *mut gst_audio::ffi::GstAudioAggregatorPad;
        (*srcpad).info = *info.to_glib_none().0;

        // There must be no buffer pending when we change caps
        assert!((*priv_).current_buffer.is_null());

        glib::ffi::g_mutex_unlock(&mut (*(ptr as *mut gst::ffi::GstObject)).lock);
        glib::ffi::g_mutex_unlock(&mut (*priv_).mutex);
    }

    true
}

impl AudioAggregatorImpl for MultiMixerElement {
    // This will be called before any of the buffer aggregation (mixing) happens,
    // but after any re-negotiation was processed and new pending output config
    // applied in Aggregator::update_src_caps(), so the config is current here.
    fn create_output_buffer(&self, num_frames: u32) -> Option<gst::Buffer> {
        let mut buf = self.parent_create_output_buffer(num_frames)?;

        let state = self.state.lock().unwrap();

        let mut output_split = vec![];
        let mut channel_offset = 0;

        for output in &state.output_config.outputs {
            let n_channels = output.channels.len();

            output_split.push(OutputChannelSplit::new(
                output.output_num,
                channel_offset,
                n_channels,
            ));

            channel_offset += n_channels;
        }

        SplitMeta::add(buf.get_mut().unwrap(), output_split);

        Some(buf)
    }

    #[allow(clippy::too_many_arguments)]
    fn aggregate_one_buffer(
        &self,
        pad: &gst_audio::AudioAggregatorPad,
        inbuf: &gst::BufferRef,
        in_offset: u32,
        outbuf: &mut gst::BufferRef,
        out_offset: u32,
        num_frames: u32,
    ) -> bool {
        let state = self.state.lock().unwrap();

        let audio_info = pad.audio_info().expect("audioinfo");

        let in_channels = audio_info.channels() as usize;
        let out_channels = state.output_config.n_output_channels();

        // Add assert to help compiler optimise loops below.
        // TODO: would be nice to support stereo too.
        assert_eq!(in_channels, 1);

        // in_offset and out_offset are in "frames", which is the size of a sample times
        // the number of channels. This means we need to multiply by number of channels to
        // get to the right offset in frames if have a flat array of audio samples in memory.
        let in_offset = in_offset as usize * in_channels;
        let out_offset = out_offset as usize * out_channels;
        let num_frames = num_frames as usize;

        let n = pad
            .name()
            .strip_prefix("sink_")
            .unwrap()
            .parse::<u32>()
            .unwrap();

        gst::log!(
            CAT,
            imp = self,
            "Aggregation time! \
            input {n} in_offset: {in_offset}, \
            out_offset: {out_offset}, num_frames: {num_frames}, \
            out_chans: {out_channels}"
        );

        #[allow(clippy::too_many_arguments)]
        fn mix_one_buffer<T: byte_slice_cast::FromByteSlice + Copy>(
            imp: &MultiMixerElement,
            output_config: &OutputConfiguration,
            n: u32,
            conv_scale: f32,
            inbuf: &gst::BufferRef,
            in_channels: usize,
            in_offset: usize,
            outbuf: &mut gst::BufferRef,
            out_channels: usize,
            out_offset: usize,
            num_frames: usize,
        ) where
            f32: From<T>,
        {
            let in_map = inbuf.map_readable().unwrap();
            let in_slice = in_map.as_slice_of::<T>().unwrap();

            let in_samples = &in_slice[in_offset..];
            let mut in_frames = in_samples.chunks_exact(in_channels);

            // Our internal mixing format is always f32
            let mut out_map = outbuf.map_writable().unwrap();
            let out_slice = out_map.as_mut_slice_of::<f32>().unwrap();
            let out_samples = &mut out_slice[out_offset..];
            let mut out_frames = out_samples.chunks_exact_mut(out_channels);

            assert!(in_frames.len() >= num_frames);
            assert!(out_frames.len() >= num_frames);

            // We're processing one input here in this aggregate_one function.
            //
            // Each channel of this input may contribute to one or more output mixes,
            // which are all contained one after another in the output buffer.
            for ch in 0..in_channels {
                // Get Vec<bool> containing the contribution (yes/no) of this input's input channel
                // for each output channel in the output buffer.
                let out_contribs =
                    output_config.get_output_contributions_for_input_channel(n, ch as u32);

                gst::log!(
                    CAT,
                    imp = imp,
                    "  input {n} ch{ch} contribs: {:?}",
                    out_contribs
                );

                assert_eq!(out_contribs.len(), out_channels);

                // Step through input and output buffer one sample frame at a time.
                //
                // Input and output frames will likely have different number of channels,
                // since the output buffer contains all channels for all output mixes.
                //
                for (out_frame, in_frame) in zip(&mut out_frames, &mut in_frames).take(num_frames) {
                    // We're only interested in mixing this one particular input sample of our input
                    // frame, the other channels will be handled in the next outer loop iteration.
                    let in_sample = f32::from(in_frame[ch]) / conv_scale;

                    // Step through all channels of all output mixes in the output buffer,
                    // and add this sample's contribution to the output samples.
                    for (sample, &contrib) in zip(out_frame.iter_mut(), &out_contribs) {
                        if contrib {
                            *sample += in_sample;
                        }
                    }
                }
            }
        }

        match audio_info.format() {
            AudioFormat::F32le => mix_one_buffer::<f32>(
                self,
                &state.output_config,
                n,
                1.0,
                inbuf,
                in_channels,
                in_offset,
                outbuf,
                out_channels,
                out_offset,
                num_frames,
            ),

            AudioFormat::S16le => mix_one_buffer::<i16>(
                self,
                &state.output_config,
                n,
                32768.0,
                inbuf,
                in_channels,
                in_offset,
                outbuf,
                out_channels,
                out_offset,
                num_frames,
            ),

            format => unimplemented!("{format:?}"),
        }

        true
    }
}

impl MultiMixerElement {
    pub(crate) fn set_output_config(
        &self,
        element: &super::MultiMixerElement,
        config: OutputConfiguration,
    ) {
        let mut state = self.state.lock().unwrap();

        gst::info!(CAT, obj = element, "pending config: {:?}", config);

        let _ = state.pending_config.insert(config);

        // reconfigure the output later from the output thread
        self.obj().src_pad().mark_reconfigure();
    }
}
