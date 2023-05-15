// Copyright (C) 2021-2024 Tomasz Andrzejak <andreiltd@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use hrtf::{HrirSphere, HrtfContext, HrtfProcessor, Vec3};

use std::io::Error;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use byte_slice_cast::*;
use rayon::{ThreadPool, prelude::*};

use crate::{SpatialObject, thread::thread_pool};

use std::sync::LazyLock;
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "hrtfrender",
        gst::DebugColorFlags::empty(),
        Some("Head-Related Transfer Function Renderer"),
    )
});

const DEFAULT_INTERPOLATION_STEPS: u64 = 8;
const DEFAULT_BLOCK_LENGTH: u64 = 512;
const DEFAULT_USE_RAYON: bool = false;

#[derive(Clone)]
struct Settings {
    interpolation_steps: u64,
    block_length: u64,
    use_rayon: bool,
    spatial_objects: Option<Vec<SpatialObject>>,
    hrir_raw_bytes: Option<glib::Bytes>,
    hrir_file_location: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interpolation_steps: DEFAULT_INTERPOLATION_STEPS,
            block_length: DEFAULT_BLOCK_LENGTH,
            use_rayon: DEFAULT_USE_RAYON,
            spatial_objects: None,
            hrir_raw_bytes: None,
            hrir_file_location: None,
        }
    }
}

impl Settings {
    fn position(&self, channel: usize) -> Result<Vec3, gst::FlowError> {
        Ok(self
            .spatial_objects
            .as_ref()
            .ok_or(gst::FlowError::NotNegotiated)?[channel]
            .position
            .to_right_handed()
            .to_vec3()
            .into())
    }

    fn distance_gain(&self, channel: usize) -> Result<f32, gst::FlowError> {
        Ok(self
            .spatial_objects
            .as_ref()
            .ok_or(gst::FlowError::NotNegotiated)?[channel]
            .distance_gain)
    }

    fn sphere(&self, rate: u32) -> Result<hrtf::HrirSphere, hrtf::HrtfError> {
        if let Some(bytes) = &self.hrir_raw_bytes {
            return HrirSphere::new(bytes.as_byte_slice(), rate);
        }

        if let Some(path) = &self.hrir_file_location {
            return HrirSphere::from_file(PathBuf::from(path), rate);
        }

        Err(Error::other("Impulse response not set").into())
    }
}

struct ChannelProcessor {
    prev_left_samples: Vec<f32>,
    prev_right_samples: Vec<f32>,
    prev_sample_vector: Option<Vec3>,
    prev_distance_gain: Option<f32>,
    indata_scratch: Box<[f32]>,
    outdata_scratch: Box<[(f32, f32)]>,
    processor: HrtfProcessor,
}

struct State {
    ininfo: gst_audio::AudioInfo,
    outinfo: gst_audio::AudioInfo,
    adapter: gst_base::UniqueAdapter,
    block_samples: usize,
    channel_processors: Vec<ChannelProcessor>,
}

impl State {
    fn input_block_size(&self) -> usize {
        self.block_samples * self.ininfo.bpf() as usize
    }

    fn output_block_size(&self) -> usize {
        self.block_samples * self.outinfo.bpf() as usize
    }

    fn reset_processors(&mut self) {
        for cp in self.channel_processors.iter_mut() {
            cp.prev_left_samples.fill(0.0);
            cp.prev_right_samples.fill(0.0);
        }
    }
}

#[derive(Default)]
pub struct HrtfRender {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
    thread_pool: Mutex<Option<Arc<ThreadPool>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for HrtfRender {
    const NAME: &'static str = "GstHrtfRender";
    type Type = super::HrtfRender;
    type ParentType = gst_base::BaseTransform;
}

impl HrtfRender {
    fn rayon_thread_pool(&self) -> Result<Arc<ThreadPool>, gst::FlowError> {
        let mut thread_pool_guard = self.thread_pool.lock().unwrap();

        if thread_pool_guard.is_none() {
            *thread_pool_guard = Some(thread_pool().map_err(|err| {
                gst::element_imp_error!(
                    self,
                    gst::CoreError::Failed,
                    ["Could not create rayon thread pool: {:?}", err]
                );

                gst::FlowError::Error
            })?);
        }

        Ok(thread_pool_guard.as_ref().unwrap().clone())
    }

    fn process(
        &self,
        outbuf: &mut gst::BufferRef,
        state: &mut State,
        settings: &Settings,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut outbuf =
            gst_audio::AudioBufferRef::from_buffer_ref_writable(outbuf, &state.outinfo).map_err(
                |err| {
                    gst::error!(CAT, imp = self, "Failed to map buffer : {}", err);
                    gst::FlowError::Error
                },
            )?;

        let outdata = outbuf
            .plane_data_mut(0)
            .unwrap()
            .as_mut_slice_of::<f32>()
            .unwrap();

        // prefill output with zeros so we can mix processed samples into it
        outdata.fill(0.0);

        let inblksz = state.input_block_size();
        let channels = state.ininfo.channels();

        let mut written_samples = 0;

        while state.adapter.available() >= inblksz {
            let inbuf = state.adapter.take_buffer(inblksz).map_err(|_| {
                gst::error!(CAT, imp = self, "Failed to map buffer");
                gst::FlowError::Error
            })?;

            let inbuf = inbuf.map_readable().map_err(|_| {
                gst::error!(CAT, imp = self, "Failed to map buffer");
                gst::FlowError::Error
            })?;

            let indata = inbuf.as_slice_of::<f32>().map_err(|_| {
                gst::error!(CAT, imp = self, "Failed to map buffer");
                gst::FlowError::Error
            })?;

            let process_channel =
                |(i, cp): (usize, &mut ChannelProcessor)| -> Result<(), gst::FlowError> {
                    let new_distance_gain = settings.distance_gain(i)?;
                    let new_sample_vector = settings.position(i)?;

                    // Deinterleave the current channel to the scratch buffer
                    for (x, y) in Iterator::zip(
                        indata.iter().skip(i).step_by(channels as usize),
                        cp.indata_scratch.iter_mut(),
                    ) {
                        *y = *x;
                    }

                    cp.processor.process_samples(HrtfContext {
                        source: &cp.indata_scratch,
                        output: &mut cp.outdata_scratch,
                        new_sample_vector,
                        new_distance_gain,
                        prev_sample_vector: cp.prev_sample_vector.unwrap_or(new_sample_vector),
                        prev_distance_gain: cp.prev_distance_gain.unwrap_or(new_distance_gain),
                        prev_left_samples: &mut cp.prev_left_samples,
                        prev_right_samples: &mut cp.prev_right_samples,
                    });

                    cp.prev_sample_vector = Some(new_sample_vector);
                    cp.prev_distance_gain = Some(new_distance_gain);

                    Ok(())
                };
            if settings.use_rayon {
                let thread_pool = self.rayon_thread_pool()?;

                thread_pool.install(|| -> Result<(), gst::FlowError> {
                    state
                        .channel_processors
                        .par_iter_mut()
                        .enumerate()
                        .try_for_each(process_channel)
                })?;
            } else {
                state
                    .channel_processors
                    .iter_mut()
                    .enumerate()
                    .try_for_each(&process_channel)?;
            }

            // unpack output scratch to output buffer
            state.channel_processors.iter_mut().for_each(|cp| {
                for (x, y) in Iterator::zip(
                    cp.outdata_scratch.iter(),
                    outdata[2 * written_samples..].chunks_exact_mut(2),
                ) {
                    y[0] += x.0;
                    y[1] += x.1;
                }

                // HRTF is mixing processed samples with samples in output buffer, we need to
                // reset scratch so it is not mixed with the next frame
                cp.outdata_scratch.fill((0.0, 0.0));
            });

            written_samples += state.block_samples;
        }

        // we only support stereo output, we can assert that we filled the whole
        // output buffer with stereo frames
        assert_eq!(outdata.len(), written_samples * 2);

        Ok(gst::FlowSuccess::Ok)
    }

    fn drain(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = &self.settings.lock().unwrap();

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        let avail = state.adapter.available();

        if avail == 0 {
            return Ok(gst::FlowSuccess::Ok);
        }

        let inblksz = state.input_block_size();
        let outblksz = state.output_block_size();
        let inbpf = state.ininfo.bpf() as usize;
        let outbpf = state.outinfo.bpf() as usize;

        let inputsz = inblksz - avail;
        let outputsz = avail / inbpf * outbpf;

        let mut inbuf = gst::Buffer::with_size(inputsz).map_err(|_| gst::FlowError::Error)?;
        let inbuf_mut = inbuf.get_mut().ok_or(gst::FlowError::Error)?;

        let mut map = inbuf_mut
            .map_writable()
            .map_err(|_| gst::FlowError::Error)?;
        let data = map
            .as_mut_slice_of::<f32>()
            .map_err(|_| gst::FlowError::Error)?;

        data.fill(0.0);
        drop(map);

        let (pts, offset, duration) = {
            let samples_to_time = |samples: u64| {
                samples
                    .mul_div_round(*gst::ClockTime::SECOND, state.ininfo.rate() as u64)
                    .map(gst::ClockTime::from_nseconds)
            };

            let (prev_pts, distance) = state.adapter.prev_pts();
            let distance_samples = distance / inbpf as u64;
            let pts = prev_pts.opt_add(samples_to_time(distance_samples));

            let (prev_offset, _) = state.adapter.prev_offset();
            let offset = prev_offset.checked_add(distance_samples).unwrap_or(0);

            let duration_samples = outputsz / outbpf;
            let duration = samples_to_time(duration_samples as u64);

            (pts, offset, duration)
        };

        state.adapter.push(inbuf);

        let mut outbuf = gst::Buffer::with_size(outblksz).map_err(|_| gst::FlowError::Error)?;
        let outbuf_mut = outbuf.get_mut().unwrap();

        self.process(outbuf_mut, state, settings)?;

        outbuf_mut.set_size(outputsz);
        outbuf_mut.set_pts(pts);
        outbuf_mut.set_offset(offset);
        outbuf_mut.set_duration(duration);

        state.reset_processors();

        drop(state_guard);
        self.obj().src_pad().push(outbuf)
    }
}

impl ObjectImpl for HrtfRender {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoxed::builder::<glib::Bytes>("hrir-raw")
                    .nick("Head Transform Impulse Response")
                    .blurb("Head Transform Impulse Response raw bytes")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("hrir-file")
                    .nick("Head Transform Impulse Response")
                    .blurb("Head Transform Impulse Response file location to read from")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("interpolation-steps")
                    .nick("Interpolation Steps")
                    .blurb("Interpolation Steps is the amount of slices to cut source to")
                    .maximum(u64::MAX - 1)
                    .default_value(DEFAULT_INTERPOLATION_STEPS)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("block-length")
                    .nick("Block Length")
                    .blurb("Block Length is the length of each slice")
                    .maximum(u64::MAX - 1)
                    .default_value(DEFAULT_BLOCK_LENGTH)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("use-rayon")
                    .nick("Use Rayon")
                    .blurb("Use Rayon to process input channels in parallel")
                    .default_value(DEFAULT_USE_RAYON)
                    .mutable_ready()
                    .build(),
                gst::ParamSpecArray::builder("spatial-objects")
                    .element_spec(
                        &glib::ParamSpecBoxed::builder::<gst::Structure>("spatial-object")
                            .nick("Spatial Object")
                            .blurb("Spatial Object Metadata")
                            .build(),
                    )
                    .nick("Spatial Objects")
                    .blurb("Spatial object Metadata to apply on input channels")
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "hrir-raw" => {
                let mut settings = self.settings.lock().unwrap();
                settings.hrir_raw_bytes = value.get().expect("type checked upstream");
            }
            "hrir-file" => {
                let mut settings = self.settings.lock().unwrap();
                settings.hrir_file_location = value.get().expect("type checked upstream");
            }
            "interpolation-steps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interpolation_steps = value.get().expect("type checked upstream");
            }
            "block-length" => {
                let mut settings = self.settings.lock().unwrap();
                settings.block_length = value.get().expect("type checked upstream");
            }
            "use-rayon" => {
                let mut settings = self.settings.lock().unwrap();
                settings.use_rayon = value.get().expect("type checked upstream");
            }
            "spatial-objects" => {
                let mut settings = self.settings.lock().unwrap();

                let objs = value
                    .get::<gst::Array>()
                    .expect("type checked upstream")
                    .iter()
                    .map(|v| {
                        let s = v.get::<gst::Structure>().expect("type checked upstream");
                        SpatialObject::from(s)
                    })
                    .collect::<Vec<_>>();

                let mut state_guard = self.state.lock().unwrap();

                if let Some(state) = state_guard.as_mut()
                    && objs.len() != state.ininfo.channels() as usize
                {
                    gst::warning!(
                        CAT,
                        "Could not update spatial objects, expected {} channels, got {}",
                        state.ininfo.channels(),
                        objs.len()
                    );
                    return;
                }

                settings.spatial_objects = if objs.is_empty() { None } else { Some(objs) };
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "hrir-raw" => {
                let settings = self.settings.lock().unwrap();
                settings.hrir_raw_bytes.to_value()
            }
            "hrir-file" => {
                let settings = self.settings.lock().unwrap();
                settings.hrir_file_location.to_value()
            }
            "interpolation-steps" => {
                let settings = self.settings.lock().unwrap();
                settings.interpolation_steps.to_value()
            }
            "block-length" => {
                let settings = self.settings.lock().unwrap();
                settings.block_length.to_value()
            }
            "use-rayon" => {
                let settings = self.settings.lock().unwrap();
                settings.use_rayon.to_value()
            }
            "spatial-objects" => {
                let settings = self.settings.lock().unwrap();

                settings
                    .spatial_objects
                    .as_ref()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .map(|x| gst::Structure::from(*x).to_send_value())
                    .collect::<gst::Array>()
                    .to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for HrtfRender {}

impl ElementImpl for HrtfRender {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Head-Related Transfer Function (HRTF) renderer",
                "Filter/Effect/Audio",
                "Renders spatial sounds to a given position",
                "Tomasz Andrzejak <andreiltd@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .channels(2)
                .format(gst_audio::AUDIO_FORMAT_F32)
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            let sink_caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .channels_range(1..=64)
                .format(gst_audio::AUDIO_FORMAT_F32)
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for HrtfRender {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = &self.settings.lock().unwrap();

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        state.adapter.push(inbuf.clone());

        if state.adapter.available() >= state.input_block_size() {
            return self.process(outbuf, state, settings);
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn transform_size(
        &self,
        _direction: gst::PadDirection,
        _caps: &gst::Caps,
        size: usize,
        _othercaps: &gst::Caps,
    ) -> Option<usize> {
        assert_ne!(_direction, gst::PadDirection::Src);

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut()?;

        let othersize = {
            let full_blocks = (size + state.adapter.available()) / (state.input_block_size());
            full_blocks * state.output_block_size()
        };

        gst::log!(
            CAT,
            imp = self,
            "Adapter size: {}, input size {}, transformed size {}",
            state.adapter.available(),
            size,
            othersize,
        );

        Some(othersize)
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut other_caps = {
            let mut new_caps = caps.clone();

            for s in new_caps.make_mut().iter_mut() {
                s.set("format", gst_audio::AUDIO_FORMAT_F32.to_str());
                s.set("layout", "interleaved");

                if direction == gst::PadDirection::Sink {
                    s.set("channels", 2);
                    s.set("channel-mask", gst::Bitmask(0x3));
                } else {
                    let settings = self.settings.lock().unwrap();
                    if let Some(objs) = &settings.spatial_objects {
                        s.set("channels", objs.len() as i32);
                    } else {
                        s.set("channels", gst::IntRange::new(1, i32::MAX));
                    }

                    s.remove_field("channel-mask");
                }
            }
            new_caps
        };

        if let Some(filter) = filter {
            other_caps = filter.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First);
        }

        gst::debug!(
            CAT,
            imp = self,
            "Transformed caps from {} to {} in direction {:?}",
            caps,
            other_caps,
            direction
        );

        Some(other_caps)
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let ininfo = gst_audio::AudioInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse input caps"))?;

        let outinfo = gst_audio::AudioInfo::from_caps(outcaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse output caps"))?;

        let settings = &mut self.settings.lock().unwrap();

        if settings.spatial_objects.is_none() {
            let Some(positions) = ininfo.positions() else {
                return Err(gst::loggable_error!(CAT, "Cannot infer object positions"));
            };

            settings.spatial_objects = Some(
                positions
                    .iter()
                    .copied()
                    .map(SpatialObject::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| gst::loggable_error!(CAT, "Unsupported channel position"))?,
            );
        }

        if settings.spatial_objects.as_ref().unwrap().len() != ininfo.channels() as usize {
            return Err(gst::loggable_error!(CAT, "Wrong number of spatial objects"));
        }

        let sphere = settings
            .sphere(ininfo.rate())
            .map_err(|e| gst::loggable_error!(CAT, "Failed to load sphere {:?}", e))?;

        let steps = settings.interpolation_steps as usize;
        let blklen = settings.block_length as usize;

        let block_samples = blklen
            .checked_mul(steps)
            .ok_or_else(|| gst::loggable_error!(CAT, "Not enough memory for frame allocation"))?;

        let channel_processors = (0..ininfo.channels())
            .map(|_| ChannelProcessor {
                prev_left_samples: vec![0.0; block_samples],
                prev_right_samples: vec![0.0; block_samples],
                prev_sample_vector: None,
                prev_distance_gain: None,
                indata_scratch: vec![0.0; block_samples].into_boxed_slice(),
                outdata_scratch: vec![(0.0, 0.0); block_samples].into_boxed_slice(),
                processor: HrtfProcessor::new(sphere.to_owned(), steps, blklen),
            })
            .collect();

        *self.state.lock().unwrap() = Some(State {
            ininfo,
            outinfo,
            block_samples,
            channel_processors,
            adapter: gst_base::UniqueAdapter::new(),
        });

        gst::debug!(CAT, imp = self, "Configured for caps {}", incaps);

        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        use gst::EventView;

        gst::debug!(CAT, imp = self, "Handling event {:?}", event);

        match event.view() {
            EventView::FlushStop(_) => {
                let mut state_guard = self.state.lock().unwrap();

                if let Some(state) = state_guard.as_mut() {
                    let avail = state.adapter.available();
                    state.adapter.flush(avail);
                    state.reset_processors();
                }
            }
            EventView::Eos(_) if self.drain().is_err() => {
                gst::warning!(CAT, "Failed to drain internal buffer");
                gst::element_imp_warning!(
                    self,
                    gst::CoreError::Event,
                    ["Failed to drain internal buffer"]
                );
            }
            _ => {}
        }

        self.parent_sink_event(event)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        if settings.use_rayon {
            // get global thread pool
            *self.thread_pool.lock().unwrap() = Some(thread_pool()?);
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.lock().unwrap().take();
        // Drop thread pool
        let _ = self.thread_pool.lock().unwrap().take();

        Ok(())
    }
}
