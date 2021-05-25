// Copyright (C) 2020 Natanael Mojica <neithanmo@gmail.com>
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
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{
    element_error, error_msg, gst_debug, gst_error, gst_info, gst_log, gst_warning, loggable_error,
};
use gst_base::subclass::base_transform::GenerateOutputSuccess;
use gst_base::subclass::prelude::*;

use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::Mutex;
use std::{f64, i32};

use byte_slice_cast::*;

use csound::{Csound, MessageType};

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "csoundfilter",
        gst::DebugColorFlags::empty(),
        Some("Audio Filter based on Csound"),
    )
});

const SCORE_OFFSET_DEFAULT: f64 = 0f64;
const DEFAULT_LOOP: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    pub loop_: bool,
    pub location: Option<String>,
    pub csd_text: Option<String>,
    pub offset: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            loop_: DEFAULT_LOOP,
            location: None,
            csd_text: None,
            offset: SCORE_OFFSET_DEFAULT,
        }
    }
}

struct State {
    in_info: gst_audio::AudioInfo,
    out_info: gst_audio::AudioInfo,
    adapter: gst_base::UniqueAdapter,
    ksmps: u32,
}

pub struct CsoundFilter {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
    csound: Mutex<Csound>,
    compiled: AtomicBool,
}

impl State {
    // Considering an input of size: input_size and the user's ksmps,
    // calculates the equivalent output_size
    fn max_output_size(&self, input_size: usize) -> usize {
        let in_samples = input_size / self.in_info.bpf() as usize;
        let in_process_samples = in_samples - (in_samples % self.ksmps as usize);
        in_process_samples * self.out_info.bpf() as usize
    }

    fn bytes_to_read(&mut self, output_size: usize) -> usize {
        // The max amount of bytes at the input that We would need
        // for filling an output buffer of size *output_size*
        (output_size / self.out_info.bpf() as usize) * self.in_info.bpf() as usize
    }

    // returns the spin capacity in bytes
    fn spin_capacity(&self) -> usize {
        (self.ksmps * self.in_info.bpf()) as _
    }

    fn needs_more_data(&self) -> bool {
        self.adapter.available() < self.spin_capacity()
    }

    fn samples_to_time(&self, samples: u64) -> Option<gst::ClockTime> {
        samples
            .mul_div_round(*gst::ClockTime::SECOND, self.in_info.rate() as u64)
            .map(gst::ClockTime::from_nseconds)
    }

    fn current_pts(&self) -> Option<gst::ClockTime> {
        // get the last seen pts and the amount of bytes
        // since then
        let (prev_pts, distance) = self.adapter.prev_pts();

        // Use the distance to get the amount of samples
        // and with it calculate the time-offset which
        // can be added to the prev_pts to get the
        // pts at the beginning of the adapter.
        let samples = distance / self.in_info.bpf() as u64;
        prev_pts
            .zip(self.samples_to_time(samples))
            .map(|(prev_pts, time_offset)| prev_pts + time_offset)
    }

    fn buffer_duration(&self, buffer_size: u64) -> Option<gst::ClockTime> {
        let samples = buffer_size / self.out_info.bpf() as u64;
        self.samples_to_time(samples)
    }
}

impl CsoundFilter {
    fn process(&self, csound: &mut Csound, idata: &[f64], odata: &mut [f64]) -> bool {
        let spin = csound.get_spin().unwrap();
        let spout = csound.get_spout().unwrap();

        let in_chunks = idata.chunks_exact(spin.len());
        let out_chuncks = odata.chunks_exact_mut(spout.len());
        let mut end_score = false;
        for (ichunk, ochunk) in in_chunks.zip(out_chuncks) {
            spin.copy_from_slice(ichunk);
            end_score = csound.perform_ksmps();
            spout.copy_to_slice(ochunk);
        }

        end_score
    }

    fn compile_score(&self) -> std::result::Result<(), gst::ErrorMessage> {
        let csound = self.csound.lock().unwrap();
        let settings = self.settings.lock().unwrap();
        if let Some(ref location) = settings.location {
            csound
                .compile_csd(location)
                .map_err(|e| error_msg!(gst::LibraryError::Failed, [e]))?;
        } else if let Some(ref text) = settings.csd_text {
            csound
                .compile_csd_text(text)
                .map_err(|e| error_msg!(gst::LibraryError::Failed, [e]))?;
        } else {
            return Err(error_msg!(
                gst::LibraryError::Failed,
                ["No Csound score specified to compile. Use either location or csd-text but not both"]
            ));
        }

        self.compiled.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn message_callback(msg_type: MessageType, msg: &str) {
        match msg_type {
            MessageType::CSOUNDMSG_ERROR => gst_error!(CAT, "{}", msg),
            MessageType::CSOUNDMSG_WARNING => gst_warning!(CAT, "{}", msg),
            MessageType::CSOUNDMSG_ORCH => gst_info!(CAT, "{}", msg),
            MessageType::CSOUNDMSG_REALTIME => gst_log!(CAT, "{}", msg),
            MessageType::CSOUNDMSG_DEFAULT => gst_log!(CAT, "{}", msg),
            MessageType::CSOUNDMSG_STDOUT => gst_log!(CAT, "{}", msg),
        }
    }

    fn drain(&self, element: &super::CsoundFilter) -> Result<gst::FlowSuccess, gst::FlowError> {
        let csound = self.csound.lock().unwrap();
        let mut state_lock = self.state.lock().unwrap();
        let state = state_lock.as_mut().unwrap();

        let avail = state.adapter.available();

        // Complete processing blocks should have been processed in the transform call
        assert!(avail < state.spin_capacity());

        if avail == 0 {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut spin = csound.get_spin().unwrap();
        let spout = csound.get_spout().unwrap();

        let out_bytes =
            (avail / state.in_info.channels() as usize) * state.out_info.channels() as usize;

        let mut buffer = gst::Buffer::with_size(out_bytes).map_err(|e| {
            gst_error!(
                CAT,
                obj: element,
                "Failed to allocate buffer at EOS {:?}",
                e
            );
            gst::FlowError::Flushing
        })?;

        let buffer_mut = buffer.get_mut().ok_or(gst::FlowError::NotSupported)?;

        let pts = state.current_pts();
        let duration = state.buffer_duration(out_bytes as _);

        buffer_mut.set_pts(pts);
        buffer_mut.set_duration(duration);

        let srcpad = element.static_pad("src").unwrap();

        let adapter_map = state.adapter.map(avail).unwrap();
        let data = adapter_map
            .as_ref()
            .as_slice_of::<f64>()
            .map_err(|_| gst::FlowError::NotSupported)?;

        let mut omap = buffer_mut
            .map_writable()
            .map_err(|_| gst::FlowError::NotSupported)?;
        let odata = omap
            .as_mut_slice_of::<f64>()
            .map_err(|_| gst::FlowError::NotSupported)?;

        spin.clear();
        spin.copy_from_slice(data);
        csound.perform_ksmps();
        spout.copy_to_slice(odata);

        drop(adapter_map);
        drop(omap);

        state.adapter.flush(avail);
        // Drop the locks before pushing buffers into the srcpad
        drop(state_lock);
        drop(csound);

        srcpad.push(buffer)
    }

    fn generate_output(
        &self,
        element: &super::CsoundFilter,
        state: &mut State,
    ) -> Result<GenerateOutputSuccess, gst::FlowError> {
        let output_size = state.max_output_size(state.adapter.available());

        let mut output = gst::Buffer::with_size(output_size).map_err(|_| gst::FlowError::Error)?;
        let outbuf = output.get_mut().ok_or(gst::FlowError::Error)?;

        let pts = state.current_pts();
        let duration = state.buffer_duration(output_size as _);

        outbuf.set_pts(pts);
        outbuf.set_duration(duration);

        gst_log!(
            CAT,
            obj: element,
            "Generating output at: {} - duration: {}",
            pts.display(),
            duration.display(),
        );

        // Get the required amount of bytes to be read from
        // the adapter to fill an ouput buffer of size output_size
        let bytes_to_read = state.bytes_to_read(output_size);

        let indata = state
            .adapter
            .map(bytes_to_read)
            .map_err(|_| gst::FlowError::Error)?;
        let idata = indata
            .as_ref()
            .as_slice_of::<f64>()
            .map_err(|_| gst::FlowError::Error)?;

        let mut omap = outbuf.map_writable().map_err(|_| gst::FlowError::Error)?;
        let odata = omap
            .as_mut_slice_of::<f64>()
            .map_err(|_| gst::FlowError::Error)?;

        let mut csound = self.csound.lock().unwrap();
        let end_score = self.process(&mut csound, idata, odata);

        drop(indata);
        drop(omap);
        state.adapter.flush(bytes_to_read);

        if end_score {
            let settings = self.settings.lock().unwrap();
            if settings.loop_ {
                csound.set_score_offset_seconds(settings.offset);
                csound.rewind_score();
            } else {
                // clear the adapter here because our eos event handler
                // will try to flush it calling csound.perform()
                // which does not make sense since
                // the end of score has been reached.
                state.adapter.clear();
                return Err(gst::FlowError::Eos);
            }
        }

        Ok(GenerateOutputSuccess::Buffer(output))
    }
}

#[glib::object_subclass]
impl ObjectSubclass for CsoundFilter {
    const NAME: &'static str = "CsoundFilter";
    type Type = super::CsoundFilter;
    type ParentType = gst_base::BaseTransform;

    fn new() -> Self {
        let csound = Csound::new();
        // create the csound instance and configure
        csound.message_string_callback(Self::message_callback);
        // Disable all default handling of sound I/O by csound internal library
        // by giving to it a hardware buffer size of zero, and setting a state,
        // higher than zero.
        csound.set_host_implemented_audioIO(1, 0);
        // We don't want csound to write samples to our HW
        csound.set_option("--nosound").unwrap();
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(None),
            csound: Mutex::new(csound),
            compiled: AtomicBool::new(false),
        }
    }
}

impl ObjectImpl for CsoundFilter {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_boolean(
                    "loop",
                    "Loop",
                    "loop over the score (can be changed in PLAYING or PAUSED state)",
                    DEFAULT_LOOP,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_string(
                    "location",
                    "Location",
                    "Location of the csd file to be used by csound.
                    Use either location or CSD-text but not both at the same time, if so and error would be triggered",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string(
                    "csd-text",
                    "CSD-text",
                    "The content of a csd file passed as a String.
                    Use either location or csd-text but not both at the same time, if so and error would be triggered",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_double(
                    "score-offset",
                    "Score Offset",
                    "Score offset in seconds to start the performance",
                    0.0,
                    f64::MAX,
                    SCORE_OFFSET_DEFAULT,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
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
            "loop" => {
                let mut settings = self.settings.lock().unwrap();
                settings.loop_ = value.get().expect("type checked upstream");
            }
            "location" => {
                let mut settings = self.settings.lock().unwrap();
                if self.state.lock().unwrap().is_none() {
                    settings.location = match value.get::<Option<String>>() {
                        Ok(location) => location,
                        _ => unreachable!("type checked upstream"),
                    };
                }
            }
            "csd-text" => {
                let mut settings = self.settings.lock().unwrap();
                if self.state.lock().unwrap().is_none() {
                    settings.csd_text = match value.get::<Option<String>>() {
                        Ok(text) => text,
                        _ => unreachable!("type checked upstream"),
                    };
                }
            }
            "score_offset" => {
                let mut settings = self.settings.lock().unwrap();
                settings.offset = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "loop" => {
                let settings = self.settings.lock().unwrap();
                settings.loop_.to_value()
            }
            "location" => {
                let settings = self.settings.lock().unwrap();
                settings.location.to_value()
            }
            "csd-text" => {
                let settings = self.settings.lock().unwrap();
                settings.csd_text.to_value()
            }
            "score_offset" => {
                let settings = self.settings.lock().unwrap();
                settings.offset.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for CsoundFilter {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Audio filter",
                "Filter/Effect/Audio",
                "Implement an audio filter/effects using Csound",
                "Natanael Mojica <neithanmo@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_simple(
                "audio/x-raw",
                &[
                    ("format", &gst_audio::AUDIO_FORMAT_F64.to_str()),
                    ("rate", &gst::IntRange::<i32>::new(1, i32::MAX)),
                    ("channels", &gst::IntRange::<i32>::new(1, i32::MAX)),
                    ("layout", &"interleaved"),
                ],
            );
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

impl BaseTransformImpl for CsoundFilter {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self, _element: &Self::Type) -> std::result::Result<(), gst::ErrorMessage> {
        self.compile_score()?;

        let csound = self.csound.lock().unwrap();
        let settings = self.settings.lock().unwrap();
        csound.set_score_offset_seconds(settings.offset);

        if let Err(e) = csound.start() {
            return Err(error_msg!(gst::LibraryError::Failed, [e]));
        }

        Ok(())
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let csound = self.csound.lock().unwrap();
        csound.stop();
        csound.reset();
        let _ = self.state.lock().unwrap().take();

        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn sink_event(&self, element: &Self::Type, event: gst::Event) -> bool {
        use gst::EventView;

        if let EventView::Eos(_) = event.view() {
            gst_log!(CAT, obj: element, "Handling Eos");
            if self.drain(element).is_err() {
                return false;
            }
        }
        self.parent_sink_event(element, event)
    }

    fn transform_caps(
        &self,
        element: &Self::Type,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let compiled = self.compiled.load(Ordering::SeqCst);

        let mut other_caps = {
            // Our caps proposal
            let mut new_caps = caps.clone();
            if compiled {
                let csound = self.csound.lock().unwrap();
                // Use the sample rate and channels configured in the csound score
                let sr = csound.get_sample_rate() as i32;
                let ichannels = csound.input_channels() as i32;
                let ochannels = csound.output_channels() as i32;
                for s in new_caps.make_mut().iter_mut() {
                    s.set("format", &gst_audio::AUDIO_FORMAT_F64.to_str());
                    s.set("rate", &sr);

                    // replace the channel property with our values,
                    // if they are not supported, the negotiation will fail.
                    if direction == gst::PadDirection::Src {
                        s.set("channels", &ichannels);
                    } else {
                        s.set("channels", &ochannels);
                    }
                    // Csound does not have a concept of channel-mask
                    s.remove_field("channel-mask");
                }
            }
            new_caps
        };

        gst_debug!(
            CAT,
            obj: element,
            "Transformed caps from {} to {} in direction {:?}",
            caps,
            other_caps,
            direction
        );

        if let Some(filter) = filter {
            other_caps = filter.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First);
        }

        Some(other_caps)
    }

    fn set_caps(
        &self,
        element: &Self::Type,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        // Flush previous state
        if self.state.lock().unwrap().is_some() {
            self.drain(element)
                .map_err(|e| loggable_error!(CAT, "Error flusing previous state data {:?}", e))?;
        }

        let in_info = gst_audio::AudioInfo::from_caps(incaps)
            .map_err(|_| loggable_error!(CAT, "Failed to parse input caps"))?;
        let out_info = gst_audio::AudioInfo::from_caps(outcaps)
            .map_err(|_| loggable_error!(CAT, "Failed to parse output caps"))?;

        let csound = self.csound.lock().unwrap();

        let ichannels = in_info.channels();
        let ochannels = out_info.channels();
        let rate = in_info.rate();

        // Check if the negotiated caps are the right ones
        if rate != out_info.rate() || rate != csound.get_sample_rate() as u32 {
            return Err(loggable_error!(
                CAT,
                "Failed to negotiate caps: invalid sample rate {}",
                rate
            ));
        } else if ichannels != csound.input_channels() {
            return Err(loggable_error!(
                CAT,
                "Failed to negotiate caps: input channels {} not supported",
                ichannels
            ));
        } else if ochannels != csound.output_channels() {
            return Err(loggable_error!(
                CAT,
                "Failed to negotiate caps: output channels {} not supported",
                ochannels
            ));
        }

        let ksmps = csound.get_ksmps();

        let adapter = gst_base::UniqueAdapter::new();

        let mut state_lock = self.state.lock().unwrap();
        *state_lock = Some(State {
            in_info,
            out_info,
            adapter,
            ksmps,
        });

        Ok(())
    }

    fn generate_output(
        &self,
        element: &Self::Type,
    ) -> Result<GenerateOutputSuccess, gst::FlowError> {
        // Check if there are enough data in the queued buffer and adapter,
        // if it is not the case, just notify the parent class to not generate
        // an output
        if let Some(buffer) = self.take_queued_buffer() {
            if buffer.flags() == gst::BufferFlags::DISCONT {
                self.drain(element)?;
            }

            let mut state_guard = self.state.lock().unwrap();
            let state = state_guard.as_mut().ok_or_else(|| {
                element_error!(
                    element,
                    gst::CoreError::Negotiation,
                    ["Can not generate an output without State"]
                );
                gst::FlowError::NotNegotiated
            })?;

            state.adapter.push(buffer);
            if !state.needs_more_data() {
                return self.generate_output(element, state);
            }
        }
        gst_log!(CAT, "No enough data to generate output");
        Ok(GenerateOutputSuccess::NoOutput)
    }
}
