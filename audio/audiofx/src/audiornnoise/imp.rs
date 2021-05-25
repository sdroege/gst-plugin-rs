// Copyright (C) 2020 Philippe Normand <philn@igalia.com>
// Copyright (C) 2020 Natanael Mojica <neithanmo@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error};
use gst_base::subclass::base_transform::BaseTransformImplExt;
use gst_base::subclass::base_transform::GenerateOutputSuccess;
use gst_base::subclass::prelude::*;

use nnnoiseless::DenoiseState;

use byte_slice_cast::*;

use once_cell::sync::Lazy;

use atomic_refcell::AtomicRefCell;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "audiornnoise",
        gst::DebugColorFlags::empty(),
        Some("Rust Audio Denoise Filter"),
    )
});

const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

struct ChannelDenoiser {
    denoiser: Box<DenoiseState<'static>>,
    frame_chunk: Box<[f32; FRAME_SIZE]>,
    out_chunk: Box<[f32; FRAME_SIZE]>,
}

struct State {
    in_info: gst_audio::AudioInfo,
    denoisers: Vec<ChannelDenoiser>,
    adapter: gst_base::UniqueAdapter,
}

#[derive(Default)]
pub struct AudioRNNoise {
    state: AtomicRefCell<Option<State>>,
}

impl State {
    // The following three functions are copied from the csound filter.
    fn buffer_duration(&self, buffer_size: u64) -> Option<gst::ClockTime> {
        let samples = buffer_size / self.in_info.bpf() as u64;
        self.samples_to_time(samples)
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

    fn needs_more_data(&self) -> bool {
        self.adapter.available() < (FRAME_SIZE * self.in_info.bpf() as usize)
    }

    fn process(&mut self, input_plane: &[f32], output_plane: &mut [f32]) {
        let channels = self.in_info.channels() as usize;
        let size = FRAME_SIZE * channels;

        for (out_frame, in_frame) in output_plane.chunks_mut(size).zip(input_plane.chunks(size)) {
            for (index, item) in in_frame.iter().enumerate() {
                let channel_index = index % channels;
                let channel_denoiser = &mut self.denoisers[channel_index];
                let pos = index / channels;
                channel_denoiser.frame_chunk[pos] = *item;
            }

            for i in (in_frame.len() / channels)..(size / channels) {
                for c in 0..channels {
                    let channel_denoiser = &mut self.denoisers[c];
                    channel_denoiser.frame_chunk[i] = 0.0;
                }
            }

            // FIXME: The first chunks coming out of the denoisers contains some
            // fade-in artifacts. We might want to discard those.
            for channel_denoiser in &mut self.denoisers {
                channel_denoiser.denoiser.process_frame(
                    &mut channel_denoiser.out_chunk[..],
                    &channel_denoiser.frame_chunk[..],
                );
            }

            for (index, item) in out_frame.iter_mut().enumerate() {
                let channel_index = index % channels;
                let channel_denoiser = &self.denoisers[channel_index];
                let pos = index / channels;
                *item = channel_denoiser.out_chunk[pos];
            }
        }
    }
}

impl AudioRNNoise {
    fn drain(&self, element: &super::AudioRNNoise) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state_lock = self.state.borrow_mut();
        let state = state_lock.as_mut().unwrap();

        let available = state.adapter.available();
        if available == 0 {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut buffer = gst::Buffer::with_size(available).map_err(|e| {
            gst_error!(
                CAT,
                obj: element,
                "Failed to allocate buffer at EOS {:?}",
                e
            );
            gst::FlowError::Flushing
        })?;

        let duration = state.buffer_duration(available as _);
        let pts = state.current_pts();

        {
            let ibuffer = state.adapter.take_buffer(available).unwrap();
            let in_map = ibuffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let in_data = in_map.as_slice_of::<f32>().unwrap();

            let buffer = buffer.get_mut().unwrap();
            buffer.set_duration(duration);
            buffer.set_pts(pts);

            let mut out_map = buffer.map_writable().map_err(|_| gst::FlowError::Error)?;
            let mut out_data = out_map.as_mut_slice_of::<f32>().unwrap();

            state.process(in_data, &mut out_data);
        }

        let srcpad = element.static_pad("src").unwrap();
        srcpad.push(buffer)
    }

    fn generate_output(
        &self,
        _element: &super::AudioRNNoise,
        state: &mut State,
    ) -> Result<GenerateOutputSuccess, gst::FlowError> {
        let available = state.adapter.available();
        let bpf = state.in_info.bpf() as usize;
        let output_size = available - (available % (FRAME_SIZE * bpf));
        let duration = state.buffer_duration(output_size as _);
        let pts = state.current_pts();

        let mut buffer = gst::Buffer::with_size(output_size).map_err(|_| gst::FlowError::Error)?;

        {
            let ibuffer = state
                .adapter
                .take_buffer(output_size)
                .map_err(|_| gst::FlowError::Error)?;
            let in_map = ibuffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let in_data = in_map.as_slice_of::<f32>().unwrap();

            let buffer = buffer.get_mut().unwrap();
            buffer.set_duration(duration);
            buffer.set_pts(pts);

            let mut out_map = buffer.map_writable().map_err(|_| gst::FlowError::Error)?;
            let mut out_data = out_map.as_mut_slice_of::<f32>().unwrap();

            state.process(in_data, &mut out_data);
        }

        Ok(GenerateOutputSuccess::Buffer(buffer))
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AudioRNNoise {
    const NAME: &'static str = "AudioRNNoise";
    type Type = super::AudioRNNoise;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for AudioRNNoise {}

impl ElementImpl for AudioRNNoise {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Audio denoise",
                "Filter/Effect/Audio",
                "Removes noise from an audio stream",
                "Philippe Normand <philn@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_simple(
                "audio/x-raw",
                &[
                    ("format", &gst_audio::AUDIO_FORMAT_F32.to_str()),
                    ("rate", &48000),
                    ("channels", &gst::IntRange::<i32>::new(1, std::i32::MAX)),
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

impl BaseTransformImpl for AudioRNNoise {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn set_caps(
        &self,
        element: &Self::Type,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        // Flush previous state
        if self.state.borrow_mut().is_some() {
            self.drain(element).map_err(|e| {
                gst::loggable_error!(CAT, "Error flusing previous state data {:?}", e)
            })?;
        }
        if incaps != outcaps {
            return Err(gst::loggable_error!(
                CAT,
                "Input and output caps are not the same"
            ));
        }

        gst_debug!(CAT, obj: element, "Set caps to {}", incaps);

        let in_info = gst_audio::AudioInfo::from_caps(incaps)
            .map_err(|e| gst::loggable_error!(CAT, "Failed to parse input caps {:?}", e))?;

        let mut denoisers = vec![];
        for _i in 0..in_info.channels() {
            denoisers.push(ChannelDenoiser {
                denoiser: DenoiseState::new(),
                frame_chunk: Box::new([0.0; FRAME_SIZE]),
                out_chunk: Box::new([0.0; FRAME_SIZE]),
            })
        }

        let mut state_lock = self.state.borrow_mut();
        *state_lock = Some(State {
            in_info,
            denoisers,
            adapter: gst_base::UniqueAdapter::new(),
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

            let mut state_guard = self.state.borrow_mut();
            let state = state_guard.as_mut().ok_or_else(|| {
                gst::element_error!(
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
        Ok(GenerateOutputSuccess::NoOutput)
    }

    fn sink_event(&self, element: &Self::Type, event: gst::Event) -> bool {
        use gst::EventView;

        if let EventView::Eos(_) = event.view() {
            gst_debug!(CAT, obj: element, "Handling EOS");
            if self.drain(element).is_err() {
                return false;
            }
        }
        self.parent_sink_event(element, event)
    }

    fn query(
        &self,
        element: &Self::Type,
        direction: gst::PadDirection,
        query: &mut gst::QueryRef,
    ) -> bool {
        if direction == gst::PadDirection::Src {
            if let gst::QueryView::Latency(ref mut q) = query.view_mut() {
                let sink_pad = element.static_pad("sink").expect("Sink pad not found");
                let mut upstream_query = gst::query::Latency::new();
                if sink_pad.peer_query(&mut upstream_query) {
                    let (live, mut min, mut max) = upstream_query.result();
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Peer latency: live {} min {} max {}",
                        live,
                        min,
                        max.display(),
                    );

                    min += gst::ClockTime::from_seconds((FRAME_SIZE / 48000) as u64);
                    max = max
                        .map(|max| max + gst::ClockTime::from_seconds((FRAME_SIZE / 48000) as u64));
                    q.set(live, min, max);
                    return true;
                }
            }
        }
        BaseTransformImplExt::parent_query(self, element, direction, query)
    }

    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.borrow_mut().take();

        Ok(())
    }
}
