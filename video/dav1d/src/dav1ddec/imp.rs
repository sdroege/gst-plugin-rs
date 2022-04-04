// Copyright (C) 2019 Philippe Normand <philn@igalia.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use once_cell::sync::Lazy;

use atomic_refcell::{AtomicRefCell, AtomicRefMut};
use std::i32;
use std::sync::Mutex;

const DEFAULT_N_THREADS: u32 = 0;
const DEFAULT_MAX_FRAME_DELAY: i64 = -1;

struct State {
    decoder: dav1d::Decoder,
    input_state: gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    output_info: Option<gst_video::VideoInfo>,
    video_meta_supported: bool,
    n_cpus: usize,
}

// We make our own settings object so we don't have to deal with a Sync impl for dav1d::Settings
struct Settings {
    n_threads: u32,
    max_frame_delay: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            n_threads: DEFAULT_N_THREADS,
            max_frame_delay: DEFAULT_MAX_FRAME_DELAY,
        }
    }
}

#[derive(Default)]
pub struct Dav1dDec {
    state: AtomicRefCell<Option<State>>,
    settings: Mutex<Settings>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "dav1ddec",
        gst::DebugColorFlags::empty(),
        Some("Dav1d AV1 decoder"),
    )
});

impl Dav1dDec {
    // FIXME: drop this once we have API from dav1d to query this value
    // https://code.videolan.org/videolan/dav1d/-/merge_requests/1407
    fn estimate_frame_delay(&self, max_frame_delay: u32, n_threads: u32) -> u32 {
        if max_frame_delay > 0 {
            std::cmp::min(max_frame_delay, n_threads)
        } else {
            let n_tc = n_threads as f64;
            std::cmp::min(8, n_tc.sqrt().ceil() as u32)
        }
    }

    fn gst_video_format_from_dav1d_picture(
        &self,
        element: &super::Dav1dDec,
        pic: &dav1d::Picture,
    ) -> gst_video::VideoFormat {
        let bpc = pic.bits_per_component();
        let format_desc = match (pic.pixel_layout(), bpc) {
            (dav1d::PixelLayout::I400, Some(dav1d::BitsPerComponent(8))) => "GRAY8",
            #[cfg(target_endian = "little")]
            (dav1d::PixelLayout::I400, Some(dav1d::BitsPerComponent(16))) => "GRAY16_LE",
            #[cfg(target_endian = "big")]
            (dav1d::PixelLayout::I400, Some(dav1d::BitsPerComponent(16))) => "GRAY16_BE",
            // (dav1d::PixelLayout::I400, Some(dav1d::BitsPerComponent(10))) => "GRAY10_LE32",
            (dav1d::PixelLayout::I420, _) => "I420",
            (dav1d::PixelLayout::I422, Some(dav1d::BitsPerComponent(8))) => "Y42B",
            (dav1d::PixelLayout::I422, _) => "I422",
            (dav1d::PixelLayout::I444, _) => "Y444",
            (layout, bpc) => {
                gst::warning!(
                    CAT,
                    obj: element,
                    "Unsupported dav1d format {:?}/{:?}",
                    layout,
                    bpc
                );
                return gst_video::VideoFormat::Unknown;
            }
        };

        let f = if format_desc.starts_with("GRAY") {
            format_desc.into()
        } else {
            match bpc {
                Some(b) => match b.0 {
                    8 => format_desc.into(),
                    _ => {
                        let endianness = if cfg!(target_endian = "little") {
                            "LE"
                        } else {
                            "BE"
                        };
                        format!("{f}_{b}{e}", f = format_desc, b = b.0, e = endianness)
                    }
                },
                None => format_desc.into(),
            }
        };
        f.parse::<gst_video::VideoFormat>().unwrap_or_else(|_| {
            gst::warning!(CAT, obj: element, "Unsupported dav1d format: {}", f);
            gst_video::VideoFormat::Unknown
        })
    }

    fn handle_resolution_change<'s>(
        &'s self,
        element: &super::Dav1dDec,
        mut state_guard: AtomicRefMut<'s, Option<State>>,
        pic: &dav1d::Picture,
    ) -> Result<AtomicRefMut<'s, Option<State>>, gst::FlowError> {
        let state = state_guard.as_mut().unwrap();

        let format = self.gst_video_format_from_dav1d_picture(element, pic);
        if format == gst_video::VideoFormat::Unknown {
            return Err(gst::FlowError::NotNegotiated);
        }

        let need_negotiate = {
            match state.output_info {
                Some(ref i) => {
                    (i.width() != pic.width())
                        || (i.height() != pic.height() || (i.format() != format))
                }
                None => true,
            }
        };
        if !need_negotiate {
            return Ok(state_guard);
        }

        gst::info!(
            CAT,
            obj: element,
            "Negotiating format {:?} picture dimensions {}x{}",
            format,
            pic.width(),
            pic.height()
        );

        let input_state = state.input_state.clone();
        drop(state_guard);

        let output_state =
            element.set_output_state(format, pic.width(), pic.height(), Some(&input_state))?;
        element.negotiate(output_state)?;
        let out_state = element.output_state().unwrap();

        state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().unwrap();
        state.output_info = Some(out_state.info());

        Ok(state_guard)
    }

    fn flush_decoder(&self, element: &super::Dav1dDec, state_guard: &mut Option<State>) {
        gst::info!(CAT, obj: element, "Flushing decoder");

        let state = state_guard.as_mut().unwrap();
        state.decoder.flush();
    }

    fn send_data(
        &self,
        element: &super::Dav1dDec,
        state_guard: &mut AtomicRefMut<Option<State>>,
        input_buffer: gst::Buffer,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<std::ops::ControlFlow<(), ()>, gst::FlowError> {
        gst::trace!(
            CAT,
            obj: element,
            "Sending data to decoder for frame {}",
            frame.system_frame_number()
        );

        let state = state_guard.as_mut().unwrap();

        let timestamp = frame.dts().map(|ts| *ts as i64);
        let duration = frame.duration().map(|d| *d as i64);

        let frame_number = Some(frame.system_frame_number() as i64);

        let input_data = input_buffer
            .into_mapped_buffer_readable()
            .map_err(|_| gst::FlowError::Error)?;

        match state
            .decoder
            .send_data(input_data, frame_number, timestamp, duration)
        {
            Ok(()) => {
                gst::trace!(CAT, obj: element, "Decoder returned OK");
                Ok(std::ops::ControlFlow::Break(()))
            }
            Err(err) if err.is_again() => {
                gst::trace!(CAT, obj: element, "Decoder returned EAGAIN");
                Ok(std::ops::ControlFlow::Continue(()))
            }
            Err(err) => {
                gst::error!(CAT, "Sending data failed (error code: {})", err);
                element.release_frame(frame);
                return gst_video::video_decoder_error!(
                    element,
                    1,
                    gst::StreamError::Decode,
                    ["Sending data failed (error code {})", err]
                )
                .map(|_| std::ops::ControlFlow::Break(()));
            }
        }
    }

    fn send_pending_data(
        &self,
        element: &super::Dav1dDec,
        state_guard: &mut AtomicRefMut<Option<State>>,
    ) -> Result<std::ops::ControlFlow<(), ()>, gst::FlowError> {
        gst::trace!(CAT, obj: element, "Sending pending data to decoder");

        let state = state_guard.as_mut().unwrap();

        match state.decoder.send_pending_data() {
            Ok(()) => {
                gst::trace!(CAT, obj: element, "Decoder returned OK");
                Ok(std::ops::ControlFlow::Break(()))
            }
            Err(err) if err.is_again() => {
                gst::trace!(CAT, obj: element, "Decoder returned EAGAIN");
                Ok(std::ops::ControlFlow::Continue(()))
            }
            Err(err) => {
                gst::error!(CAT, "Sending data failed (error code: {})", err);
                return gst_video::video_decoder_error!(
                    element,
                    1,
                    gst::StreamError::Decode,
                    ["Sending data failed (error code {})", err]
                )
                .map(|_| std::ops::ControlFlow::Break(()));
            }
        }
    }

    fn decoded_picture_as_buffer(
        &self,
        element: &super::Dav1dDec,
        state_guard: &mut AtomicRefMut<Option<State>>,
        pic: &dav1d::Picture,
        output_state: gst_video::VideoCodecState<gst_video::video_codec_state::Readable>,
    ) -> Result<gst::Buffer, gst::FlowError> {
        let mut offsets = vec![];
        let mut strides = vec![];
        let mut acc_offset: usize = 0;

        let state = state_guard.as_mut().unwrap();
        let video_meta_supported = state.video_meta_supported;

        let info = output_state.info();
        let mut out_buffer = gst::Buffer::new();
        let mut_buffer = out_buffer.get_mut().unwrap();

        let components = if info.is_yuv() {
            const YUV_COMPONENTS: [dav1d::PlanarImageComponent; 3] = [
                dav1d::PlanarImageComponent::Y,
                dav1d::PlanarImageComponent::U,
                dav1d::PlanarImageComponent::V,
            ];
            &YUV_COMPONENTS[..]
        } else if info.is_gray() {
            const GRAY_COMPONENTS: [dav1d::PlanarImageComponent; 1] =
                [dav1d::PlanarImageComponent::Y];

            &GRAY_COMPONENTS[..]
        } else {
            unreachable!();
        };
        for &component in components {
            let dest_stride: u32 = info.stride()[component as usize].try_into().unwrap();
            let plane = pic.plane(component);
            let (src_stride, height) = pic.plane_data_geometry(component);
            let mem = if video_meta_supported || src_stride == dest_stride {
                gst::Memory::from_slice(plane)
            } else {
                gst::trace!(
                    gst::CAT_PERFORMANCE,
                    obj: element,
                    "Copying decoded video frame component {:?}",
                    component
                );

                let src_slice = plane.as_ref();
                let mem = gst::Memory::with_size((dest_stride * height) as usize);
                let mut writable_mem = mem
                    .into_mapped_memory_writable()
                    .map_err(|_| gst::FlowError::Error)?;
                let len = std::cmp::min(src_stride, dest_stride) as usize;

                for (out_line, in_line) in writable_mem
                    .as_mut_slice()
                    .chunks_exact_mut(dest_stride.try_into().unwrap())
                    .zip(src_slice.chunks_exact(src_stride.try_into().unwrap()))
                {
                    out_line.copy_from_slice(&in_line[..len]);
                }
                writable_mem.into_memory()
            };
            let mem_size = mem.size();
            mut_buffer.append_memory(mem);

            strides.push(src_stride as i32);
            offsets.push(acc_offset);
            acc_offset += mem_size;
        }

        if video_meta_supported {
            gst_video::VideoMeta::add_full(
                out_buffer.get_mut().unwrap(),
                gst_video::VideoFrameFlags::empty(),
                info.format(),
                info.width(),
                info.height(),
                &offsets,
                &strides[..],
            )
            .unwrap();
        }

        let duration = pic.duration() as u64;
        if duration > 0 {
            out_buffer
                .get_mut()
                .unwrap()
                .set_duration(gst::ClockTime::from_nseconds(duration));
        }

        Ok(out_buffer)
    }

    fn handle_picture<'s>(
        &'s self,
        element: &super::Dav1dDec,
        mut state_guard: AtomicRefMut<'s, Option<State>>,
        pic: &dav1d::Picture,
    ) -> Result<AtomicRefMut<'s, Option<State>>, gst::FlowError> {
        gst::trace!(CAT, obj: element, "Handling picture {}", pic.offset());

        state_guard = self.handle_resolution_change(element, state_guard, pic)?;

        let output_state = element
            .output_state()
            .expect("Output state not set. Shouldn't happen!");
        let offset = pic.offset() as i32;

        if let Some(mut frame) = element.frame(offset) {
            let output_buffer =
                self.decoded_picture_as_buffer(element, &mut state_guard, pic, output_state)?;
            frame.set_output_buffer(output_buffer);
            drop(state_guard);
            element.finish_frame(frame)?;
            Ok(self.state.borrow_mut())
        } else {
            gst::warning!(CAT, obj: element, "No frame found for offset {}", offset);
            Ok(state_guard)
        }
    }

    fn drop_decoded_pictures(&self, element: &super::Dav1dDec, state_guard: &mut Option<State>) {
        while let Ok(Some(pic)) = self.pending_pictures(element, state_guard) {
            gst::debug!(CAT, obj: element, "Dropping picture {}", pic.offset());
            drop(pic);
        }
    }

    fn pending_pictures(
        &self,
        element: &super::Dav1dDec,
        state_guard: &mut Option<State>,
    ) -> Result<Option<dav1d::Picture>, gst::FlowError> {
        gst::trace!(CAT, obj: element, "Retrieving pending picture");

        let state = state_guard.as_mut().unwrap();

        match state.decoder.get_picture() {
            Ok(pic) => {
                gst::trace!(CAT, obj: element, "Retrieved picture {}", pic.offset());
                Ok(Some(pic))
            }
            Err(err) if err.is_again() => {
                gst::trace!(CAT, obj: element, "Decoder needs more data");
                Ok(None)
            }
            Err(err) => {
                gst::error!(
                    CAT,
                    obj: element,
                    "Retrieving decoded picture failed (error code {})",
                    err
                );

                gst_video::video_decoder_error!(
                    element,
                    1,
                    gst::StreamError::Decode,
                    ["Retrieving decoded picture failed (error code {})", err]
                )
                .map(|_| None)
            }
        }
    }

    fn forward_pending_pictures<'s>(
        &'s self,
        element: &super::Dav1dDec,
        mut state_guard: AtomicRefMut<'s, Option<State>>,
    ) -> Result<AtomicRefMut<Option<State>>, gst::FlowError> {
        while let Some(pic) = self.pending_pictures(element, &mut state_guard)? {
            state_guard = self.handle_picture(element, state_guard, &pic)?;
        }

        Ok(state_guard)
    }
}

fn video_output_formats() -> Vec<glib::SendValue> {
    let values = [
        gst_video::VideoFormat::Gray8,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Gray16Le,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Gray16Be,
        // #[cfg(target_endian = "little")]
        // gst_video::VideoFormat::Gray10Le32,
        gst_video::VideoFormat::I420,
        gst_video::VideoFormat::Y42b,
        gst_video::VideoFormat::Y444,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::I42010le,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::I42210le,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Y44410le,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::I42010be,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::I42210be,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Y44410be,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::I42012le,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::I42212le,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Y44412le,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::I42012be,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::I42212be,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Y44412be,
    ];
    values.iter().map(|i| i.to_str().to_send_value()).collect()
}

#[glib::object_subclass]
impl ObjectSubclass for Dav1dDec {
    const NAME: &'static str = "RsDav1dDec";
    type Type = super::Dav1dDec;
    type ParentType = gst_video::VideoDecoder;
}

impl ObjectImpl for Dav1dDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::new(
                    "n-threads",
                    "Number of threads",
                    "Number of threads to use while decoding (set to 0 to use number of logical cores)",
                    0,
                    std::u32::MAX,
                    DEFAULT_N_THREADS,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecInt64::new(
                    "max-frame-delay",
                    "Maximum frame delay",
                    "Maximum delay in frames for the decoder (set to 1 for low latency, 0 to be equal to the number of logical cores. -1 to choose between these two based on pipeline liveness)",
                    -1,
                    std::u32::MAX.into(),
                    DEFAULT_MAX_FRAME_DELAY,
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
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "n-threads" => {
                settings.n_threads = value.get().expect("type checked upstream");
            }
            "max-frame-delay" => {
                settings.max_frame_delay = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "n-threads" => settings.n_threads.to_value(),
            "max-frame-delay" => settings.max_frame_delay.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Dav1dDec {}

impl ElementImpl for Dav1dDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Dav1d AV1 Decoder",
                "Codec/Decoder/Video",
                "Decode AV1 video streams with dav1d",
                "Philippe Normand <philn@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = if gst::version() >= (1, 19, 0, 0) {
                gst::Caps::builder("video/x-av1")
                    .field("stream-format", "obu-stream")
                    .field("alignment", gst::List::new(["frame", "tu"]))
                    .build()
            } else {
                gst::Caps::builder("video/x-av1").build()
            };
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("video/x-raw")
                .field("format", gst::List::from(video_output_formats()))
                .field("width", gst::IntRange::new(1, i32::MAX))
                .field("height", gst::IntRange::new(1, i32::MAX))
                .field(
                    "framerate",
                    gst::FractionRange::new(
                        gst::Fraction::new(0, 1),
                        gst::Fraction::new(i32::MAX, 1),
                    ),
                )
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl VideoDecoderImpl for Dav1dDec {
    fn src_query(&self, element: &Self::Type, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Latency(q) = query.view_mut() {
            let state_guard = self.state.borrow();
            let max_frame_delay = {
                let settings = self.settings.lock().unwrap();
                settings.max_frame_delay
            };

            match *state_guard {
                Some(ref state) => match state.output_info {
                    Some(ref info) => {
                        let mut upstream_latency = gst::query::Latency::new();
                        let sinkpad = &element.static_pad("sink").expect("Failed to get sink pad");

                        if sinkpad.peer_query(&mut upstream_latency) {
                            let (live, mut min, mut max) = upstream_latency.result();
                            // For autodetection: 1 if live, else whatever dav1d gives us
                            let frame_latency: u64 = if max_frame_delay < 0 && live {
                                1
                            } else {
                                self.estimate_frame_delay(
                                    max_frame_delay as u32,
                                    state.n_cpus as u32,
                                )
                                .into()
                            };

                            let latency = frame_latency
                                * (info.fps().denom() as u64)
                                * gst::ClockTime::SECOND
                                / (info.fps().numer() as u64);

                            gst::debug!(CAT, obj: element, "Reporting latency of {}", latency);

                            min += latency;
                            max = max.opt_add(latency);
                            q.set(live, min, max);

                            true
                        } else {
                            // peer latency query failed
                            false
                        }
                    }
                    // output info not available => fps unknown
                    None => false,
                },
                // no state yet
                None => false,
            }
        } else {
            VideoDecoderImplExt::parent_src_query(self, element, query)
        }
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        {
            let mut state_guard = self.state.borrow_mut();
            *state_guard = None;
        }

        self.parent_stop(element)
    }

    fn set_format(
        &self,
        element: &Self::Type,
        input_state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        let mut state_guard = self.state.borrow_mut();
        let settings = self.settings.lock().unwrap();
        let mut decoder_settings = dav1d::Settings::new();
        let max_frame_delay: u32;
        let n_cpus = num_cpus::get();

        gst::info!(CAT, obj: element, "Detected {} logical CPUs", n_cpus);

        if settings.max_frame_delay == -1 {
            let mut latency_query = gst::query::Latency::new();
            let mut is_live = false;
            let sinkpad = &element.static_pad("sink").expect("Failed to get sink pad");

            if sinkpad.peer_query(&mut latency_query) {
                is_live = latency_query.result().0;
            }

            max_frame_delay = if is_live { 1 } else { 0 };
        } else {
            max_frame_delay = settings.max_frame_delay.try_into().unwrap();
        }

        gst::info!(
            CAT,
            obj: element,
            "Creating decoder with n-threads={} and max-frame-delay={}",
            settings.n_threads,
            max_frame_delay
        );
        decoder_settings.set_n_threads(settings.n_threads);
        decoder_settings.set_max_frame_delay(max_frame_delay);

        let decoder = dav1d::Decoder::with_settings(&decoder_settings).map_err(|err| {
            gst::loggable_error!(CAT, "Failed to create decoder instance: {}", err)
        })?;

        *state_guard = Some(State {
            decoder,
            input_state: input_state.clone(),
            output_info: None,
            video_meta_supported: false,
            n_cpus,
        });

        self.parent_set_format(element, input_state)
    }

    fn handle_frame(
        &self,
        element: &Self::Type,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let input_buffer = frame
            .input_buffer_owned()
            .expect("frame without input buffer");

        {
            let mut state_guard = self.state.borrow_mut();
            state_guard = self.forward_pending_pictures(element, state_guard)?;
            if self.send_data(element, &mut state_guard, input_buffer, frame)?
                == std::ops::ControlFlow::Continue(())
            {
                loop {
                    state_guard = self.forward_pending_pictures(element, state_guard)?;
                    if self.send_pending_data(element, &mut state_guard)?
                        == std::ops::ControlFlow::Break(())
                    {
                        break;
                    }
                }
            }
            let _state_guard = self.forward_pending_pictures(element, state_guard)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn flush(&self, element: &Self::Type) -> bool {
        gst::info!(CAT, obj: element, "Flushing");

        {
            let mut state_guard = self.state.borrow_mut();
            if state_guard.is_some() {
                self.flush_decoder(element, &mut state_guard);
                self.drop_decoded_pictures(element, &mut state_guard);
            }
        }

        true
    }

    fn drain(&self, element: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::info!(CAT, obj: element, "Draining");

        {
            let mut state_guard = self.state.borrow_mut();
            if state_guard.is_some() {
                self.flush_decoder(element, &mut state_guard);
                let _state_guard = self.forward_pending_pictures(element, state_guard)?;
            }
        }

        self.parent_drain(element)
    }

    fn finish(&self, element: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::info!(CAT, obj: element, "Finishing");

        {
            let mut state_guard = self.state.borrow_mut();
            if state_guard.is_some() {
                self.flush_decoder(element, &mut state_guard);
                let _state_guard = self.forward_pending_pictures(element, state_guard)?;
            }
        }

        self.parent_finish(element)
    }

    fn decide_allocation(
        &self,
        element: &Self::Type,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        {
            let mut state_guard = self.state.borrow_mut();
            let state = state_guard.as_mut().unwrap();
            state.video_meta_supported = query
                .find_allocation_meta::<gst_video::VideoMeta>()
                .is_some();
        }

        self.parent_decide_allocation(element, query)
    }
}
