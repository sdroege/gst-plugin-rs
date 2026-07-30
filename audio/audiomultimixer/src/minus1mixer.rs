// GStreamer minus-1 audio mixer
//
// Copyright (C) 2022-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-minus1mixer
 *
 * Minus-1 mixer element. Creates for each input an output mix that contains all the other inputs.
 *
 * Input pads are request pads, and for every new input pad "sink_%u" the minus1mixer will
 * automatically create a matching "minus_%u" source pad that will contain the mix of all the
 * other inputs, i.e. all inputs except "sink_%u" itself. If there is only one sink pad this
 * will output silence instead.
 *
 * Input pads can be added and removed at runtime at any time.
 *
 * The sample rate is not negotiated, as this is not possible to do in a race-free manner. It
 * must instead be configured at element creation time via the "sample-rate" construct-only
 * property. The default sample rate is 48kHz.
 *
 * Only mono input is supported for the time being.
 *
 * Both F32 and S16 are supported as input and output audio formats. Different inputs may have
 * different input formats (as long as it's one of the two supported ones), and different outputs
 * can have different output formats (as long as it's one of the two supported ones). The internal
 * mixing is currently always done in F32 format regardless of the negotiated input and output
 * formats.
 *
 * ## Example pipelines
 *
 * |[
 * gst-launch-1.0 minus1mixer name=m1m  \
 *   audiotestsrc freq=440 ! m1m.sink_0 \
 *   audiotestsrc wave=ticks ! m1m.sink_1 \
 *   audiotestsrc freq=220 ! m1m.sink_2  \
 *   m1m.minus_0 ! audioconvert ! pulsesink
 * ]| This should play back the mix of inputs 1 and 2 (ticks and 220Hz sine wave).
 *
 * |[
 * gst-launch-1.0 minus1mixer name=m1m  \
 *   audiotestsrc freq=440 ! m1m.sink_0 \
 *   audiotestsrc wave=ticks ! m1m.sink_1 \
 *   audiotestsrc freq=220 ! m1m.sink_2  \
 *   m1m.minus_1 ! audioconvert ! pulsesink
 * ]| This should play back the mix of inputs 0 and 2 (440Hz and 220Hz sine waves).
 *
 * |[
 * gst-launch-1.0 minus1mixer name=m1m  \
 *   audiotestsrc freq=440 ! m1m.sink_0 \
 *   audiotestsrc wave=ticks ! m1m.sink_1 \
 *   audiotestsrc freq=220 ! m1m.sink_2  \
 *   m1m.minus_2 ! audioconvert ! pulsesink
 * ]| This should play back the mix of inputs 0 and 1 (440Hz sine wave and ticks).
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::num::NonZeroU32;
use std::sync::{Arc, LazyLock, Mutex};

use crate::audiomultimixerelement::{OutputConfiguration, OutputMix};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "minus1mixer",
        gst::DebugColorFlags::empty(),
        Some("Minus-1 audio mixer"),
    )
});

const DEFAULT_SAMPLE_RATE: i32 = 48000;

#[derive(Debug, Clone, Copy)]
struct Settings {
    pub sample_rate: i32,

    // Need to store force_live which is construct-only so we can set it later
    // on the mixer in the constructed vfunc when we create the mixer.
    pub force_live: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            sample_rate: DEFAULT_SAMPLE_RATE,
            force_live: false,
        }
    }
}

struct State {
    // internal multimixerelement
    mixer_element: gst::Element,
    // internal multimixersplitter
    splitter: gst::Element,
    // streams
    streams: Vec<u32>,
}

#[derive(Default)]
pub struct Minus1Mixer {
    state: Arc<Mutex<Option<State>>>,
    settings: Mutex<Settings>,
}

impl Minus1Mixer {}

#[glib::object_subclass]
impl ObjectSubclass for Minus1Mixer {
    const NAME: &'static str = "GstMinus1Mixer";
    type Type = super::Minus1Mixer;
    type ParentType = gst::Bin;
}

impl BinImpl for Minus1Mixer {}

impl ObjectImpl for Minus1Mixer {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();

        let settings = self.settings.lock().unwrap();

        let mixer_element = glib::object::Object::builder::<crate::MultiMixerElement>()
            .property("force-live", settings.force_live)
            .build();

        let splitter = glib::object::Object::builder::<crate::MultiMixerSplitter>()
            .property("sample-rate", settings.sample_rate)
            .build();

        obj.add(&mixer_element).unwrap();
        obj.add(&splitter).unwrap();

        mixer_element.link(&splitter).unwrap();

        for prop_name in [
            "ignore-inactive-pads",
            "discont-wait",
            "alignment-threshold",
            "output-buffer-duration-fraction",
            "output-buffer-duration",
            "start-time",
            "start-time-selection",
            "min-upstream-latency",
            "latency",
        ] {
            obj.bind_property(prop_name, &mixer_element, prop_name)
                .build();
        }

        let mut state_guard = self.state.lock().unwrap();

        let state = State {
            mixer_element: mixer_element.into(),
            splitter: splitter.into(),
            streams: vec![],
        };

        *state_guard = Some(state);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecInt::builder("sample-rate")
                    .nick("Sample Rate")
                    .blurb("Sample rate to use")
                    .minimum(1i32)
                    .maximum(i32::MAX)
                    .default_value(DEFAULT_SAMPLE_RATE)
                    .construct_only()
                    .build(),

                // AudioAggregator properties we proxy for convenience and discoverability

                glib::ParamSpecBoolean::builder("force-live")
                    .nick("Force live")
                    .blurb("Always operate in live mode and aggregate on timeout regardless of whether any live sources are linked upstream")
                    .default_value(false)
                    .construct_only()
                    .build(),

                glib::ParamSpecBoolean::builder("ignore-inactive-pads")
                    .nick("Ignore inactive pads")
                    .blurb("Avoid timing out waiting for inactive pads")
                    .default_value(false)
                    .build(),

                glib::ParamSpecUInt64::builder("discont-wait")
                    .nick("Discont Wait")
                    .blurb("Window of time in nanoseconds to wait before creating a discontinuity")
                    .minimum(0u64)
                    .maximum(u64::MAX - 1)
                    .default_value(gst::ClockTime::from_mseconds(1000).nseconds())
                    .mutable_playing()
                    .build(),

                glib::ParamSpecUInt64::builder("alignment-threshold")
                    .nick("Alignment Threshold")
                    .blurb("Timestamp alignment threshold in nanoseconds")
                    .minimum(0u64)
                    .maximum(u64::MAX - 1)
                    .default_value(gst::ClockTime::from_mseconds(40).nseconds())
                    .mutable_playing()
                    .build(),

                gst::ParamSpecFraction::builder("output-buffer-duration-fraction")
                    .nick("Output buffer duration fraction")
                    .blurb("Output block size in nanoseconds, expressed as a fraction")
                    .minimum(gst::Fraction::new(1, i32::MAX))
                    .maximum(gst::Fraction::new(i32::MAX, 1))
                    .default_value(gst::Fraction::new(1, 100))
                    .mutable_ready()
                    .build(),

                glib::ParamSpecUInt64::builder("output-buffer-duration")
                    .nick("Output Buffer Duration")
                    .blurb("Output block size in nanoseconds")
                    .minimum(1u64)
                    .maximum(u64::MAX - 1)
                    .default_value(gst::ClockTime::from_mseconds(10).nseconds())
                    .mutable_ready()
                    .build(),

                // Aggregator properties we proxy for convenience and discoverability

                glib::ParamSpecUInt64::builder("start-time")
                    .nick("Start Time")
                    .blurb("Start time to use if start-time-selection=set")
                    .minimum(0u64)
                    .maximum(u64::MAX)
                    .default_value(u64::MAX)
                    .build(),

                glib::ParamSpecEnum::builder_with_default("start-time-selection", gst_base::AggregatorStartTimeSelection::Zero)
                    .nick("Start Time Selection")
                    .blurb("Decides which start time is output")
                    .build(),

                glib::ParamSpecUInt64::builder("min-upstream-latency")
                    .nick("Minimum upstream latency")
                    .blurb("When sources with a higher latency are expected to be plugged \
                            in dynamically after the aggregator has started playing, \
                            this allows overriding the minimum latency reported by the \
                            initial source(s). This is only taken into account when larger \
                            than the actually reported minimum latency. (nanoseconds)")
                    .minimum(0u64)
                    .maximum(u64::MAX)
                    .default_value(0u64)
                    .build(),

                glib::ParamSpecUInt64::builder("latency")
                    .nick("Buffer latency")
                    .blurb("Additional latency in live mode to allow upstream \
                            to take longer to produce buffers for the current \
                            position (in nanoseconds)")
                    .minimum(0u64)
                    .maximum(u64::MAX)
                    .default_value(0u64)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "sample-rate" => {
                let mut settings = self.settings.lock().unwrap();
                settings.sample_rate = value.get::<i32>().unwrap();
            }

            "force-live" => {
                let mut settings = self.settings.lock().unwrap();
                settings.force_live = value.get::<bool>().unwrap();
            }

            "ignore-inactive-pads"
            | "discont-wait"
            | "alignment-threshold"
            | "output-buffer-duration-fraction"
            | "output-buffer-duration"
            | "start-time"
            | "start-time-selection"
            | "min-upstream-latency"
            | "latency" => {
                let state_guard = self.state.lock().unwrap();
                let state = state_guard.as_ref().expect("state");

                state.mixer_element.set_property(pspec.name(), value);
            }

            name => unimplemented!("{name}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "sample-rate" => {
                let settings = self.settings.lock().unwrap();
                settings.sample_rate.to_value()
            }

            "force-live" => {
                let settings = self.settings.lock().unwrap();
                settings.force_live.to_value()
            }

            "ignore-inactive-pads"
            | "discont-wait"
            | "alignment-threshold"
            | "output-buffer-duration-fraction"
            | "output-buffer-duration"
            | "start-time"
            | "start-time-selection"
            | "min-upstream-latency"
            | "latency" => {
                let state_guard = self.state.lock().unwrap();
                let state = state_guard.as_ref().expect("state");

                state.mixer_element.property(pspec.name())
            }

            name => unimplemented!("{name}"),
        }
    }
}

impl GstObjectImpl for Minus1Mixer {}

impl ElementImpl for Minus1Mixer {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Minus-1 Audio Mixer",
                "Audio/Mixer",
                "Minus-1 Audio Mixer",
                "Tim-Philipp Müller <tim@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format_list([gst_audio::AUDIO_FORMAT_S16, gst_audio::AUDIO_FORMAT_F32])
                .rate_range(1..)
                .channels(1) // only support mono for now
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "minus_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().expect("state");

        let mixer_templ = state.mixer_element.pad_template("sink_%u").unwrap();

        let mixer_pad = state.mixer_element.request_pad(&mixer_templ, name, caps)?;

        let new_sink_pad = gst::GhostPad::builder_from_template_with_target(templ, &mixer_pad)
            .unwrap()
            .name(mixer_pad.name())
            .build();

        self.obj().add_pad(&new_sink_pad).unwrap();
        new_sink_pad.set_active(true).unwrap();

        let n = mixer_pad
            .name()
            .strip_prefix("sink_")
            .unwrap()
            .parse::<u32>()
            .unwrap();

        let splitter_templ = state.splitter.pad_template("src_%u").unwrap();

        let splitter_pad =
            state
                .splitter
                .request_pad(&splitter_templ, Some(&format!("src_{n}")), caps)?;

        let src_templ = self.obj().pad_template("minus_%u").unwrap();

        let minus1_pad =
            gst::GhostPad::builder_from_template_with_target(&src_templ, &splitter_pad)
                .unwrap()
                .name(format!("minus_{n}"))
                .build();
        self.obj().add_pad(&minus1_pad).unwrap();
        new_sink_pad.set_active(true).unwrap();

        gst::info!(
            CAT,
            imp = self,
            "New input {} -> {}",
            &mixer_pad.name(),
            &minus1_pad.name(),
        );

        state.streams.push(n);

        self.update_output_config(state);

        Some(new_sink_pad.upcast())
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().expect("state");

        if pad.direction() != gst::PadDirection::Sink {
            gst::error!(
                CAT,
                imp = self,
                "You must release the sink pad(s), in which case the\
                corresponding source pad will be removed automatically!"
            );
            return;
        }

        let n = pad
            .name()
            .strip_prefix("sink_")
            .unwrap()
            .parse::<u32>()
            .unwrap();

        let ghost_pad = pad.downcast_ref::<gst::GhostPad>().unwrap();
        if let Some(mixer_sink_pad) = ghost_pad.target() {
            state.mixer_element.release_request_pad(&mixer_sink_pad);
        }

        // Remove automatically-added minus_X sometimes source pad
        let minus1_name = format!("minus_{n}");
        let minus1_pad = self.obj().static_pad(&minus1_name).unwrap();

        let src_ghost_pad = minus1_pad.downcast_ref::<gst::GhostPad>().unwrap();
        if let Some(splitter_src_pad) = src_ghost_pad.target() {
            // ... but first release the splitter source pad so the splitter can make sure that
            // the pad removal and deactivation doesn't cause flow-flushing returns anywhere.
            state.splitter.release_request_pad(&splitter_src_pad);
        }

        minus1_pad.set_active(false).unwrap();
        self.obj().remove_pad(&minus1_pad).unwrap();

        pad.set_active(false).unwrap();
        self.obj().remove_pad(pad).unwrap();

        gst::info!(CAT, imp = self, "Released input sink_{n} -> {minus1_name}");

        // Wasteful since we continue after we found the value, but we have small lengths
        state.streams.retain(|&val| val != n);

        self.update_output_config(state);
    }
}

impl Minus1Mixer {
    fn update_output_config(&self, state: &State) {
        let mut output_config = OutputConfiguration::default();

        for &output_num in &state.streams {
            let mut output_mix = OutputMix::new(output_num, NonZeroU32::new(1).unwrap());

            for &input_num in &state.streams {
                if input_num != output_num {
                    output_mix.add_mono_contribution(input_num);
                }
            }

            output_config.add_output_mix(output_mix);
        }

        gst::info!(CAT, imp = self, "New output config: {:?}", &output_config,);

        for &input_num in &state.streams {
            for c in 0..1 {
                // FIXME: always mono for now
                gst::info!(
                    CAT,
                    imp = self,
                    "output config for input {input_num} channel {c}: {:?}",
                    output_config.get_output_contributions_for_input_channel(input_num, c)
                );
            }
        }

        let mixer_element = state
            .mixer_element
            .downcast_ref::<super::MultiMixerElement>()
            .unwrap();

        mixer_element
            .imp()
            .set_output_config(mixer_element, output_config);
    }
}
