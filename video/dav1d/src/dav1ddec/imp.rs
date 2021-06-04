// Copyright (C) 2019 Philippe Normand <philn@igalia.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_trace, gst_warning};
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use once_cell::sync::Lazy;

use std::convert::TryInto;
use std::i32;
use std::str::FromStr;
use std::sync::Mutex;

#[derive(Default)]
struct NegotiationInfos {
    input_state:
        Option<gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>>,
    output_info: Option<gst_video::VideoInfo>,
    video_meta_supported: bool,
}

#[derive(Default)]
pub struct Dav1dDec {
    decoder: Mutex<dav1d::Decoder>,
    negotiation_infos: Mutex<NegotiationInfos>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "dav1ddec",
        gst::DebugColorFlags::empty(),
        Some("Dav1d AV1 decoder"),
    )
});

impl Dav1dDec {
    pub fn gst_video_format_from_dav1d_picture(
        &self,
        pic: &dav1d::Picture,
    ) -> gst_video::VideoFormat {
        let bpc = pic.bits_per_component();
        let format_desc = match (pic.pixel_layout(), bpc) {
            // (dav1d::PixelLayout::I400, Some(dav1d::BitsPerComponent(8))) => "GRAY8",
            // (dav1d::PixelLayout::I400, Some(dav1d::BitsPerComponent(10))) => "GRAY10_LE32",
            (dav1d::PixelLayout::I400, _) => return gst_video::VideoFormat::Unknown,
            (dav1d::PixelLayout::I420, _) => "I420",
            (dav1d::PixelLayout::I422, Some(dav1d::BitsPerComponent(8))) => "Y42B",
            (dav1d::PixelLayout::I422, _) => "I422",
            (dav1d::PixelLayout::I444, _) => "Y444",
            (dav1d::PixelLayout::Unknown, _) => {
                gst_warning!(CAT, "Unsupported dav1d format");
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
        gst_video::VideoFormat::from_str(&f).unwrap_or_else(|_| {
            gst_warning!(CAT, "Unsupported dav1d format: {}", f);
            gst_video::VideoFormat::Unknown
        })
    }

    pub fn handle_resolution_change(
        &self,
        element: &super::Dav1dDec,
        pic: &dav1d::Picture,
        format: gst_video::VideoFormat,
    ) -> Result<(), gst::FlowError> {
        let negotiate = {
            let negotiation_infos = self.negotiation_infos.lock().unwrap();
            match negotiation_infos.output_info {
                Some(ref i) => {
                    (i.width() != pic.width())
                        || (i.height() != pic.height() || (i.format() != format))
                }
                None => true,
            }
        };
        if !negotiate {
            return Ok(());
        }
        gst_info!(
            CAT,
            obj: element,
            "Negotiating format picture dimensions {}x{}",
            pic.width(),
            pic.height()
        );
        let output_state = {
            let negotiation_infos = self.negotiation_infos.lock().unwrap();
            let input_state = negotiation_infos.input_state.as_ref();
            element.set_output_state(format, pic.width(), pic.height(), input_state)
        }?;
        element.negotiate(output_state)?;
        let out_state = element.output_state().unwrap();
        {
            let mut negotiation_infos = self.negotiation_infos.lock().unwrap();
            negotiation_infos.output_info = Some(out_state.info());
        }

        Ok(())
    }

    fn flush_decoder(&self) {
        let decoder = self.decoder.lock().unwrap();
        decoder.flush();
    }

    fn decode(
        &self,
        input_buffer: &gst::BufferRef,
        frame: &gst_video::VideoCodecFrame,
    ) -> Result<Vec<(dav1d::Picture, gst_video::VideoFormat)>, gst::FlowError> {
        let mut decoder = self.decoder.lock().unwrap();
        let timestamp = frame.dts().map(|ts| *ts as i64);
        let duration = frame.duration().map(|d| *d as i64);

        let frame_number = Some(frame.system_frame_number() as i64);
        let input_data = input_buffer
            .map_readable()
            .map_err(|_| gst::FlowError::Error)?;
        let pictures = decoder
            .decode(input_data, frame_number, timestamp, duration, || {})
            .map_err(|e| {
                gst_error!(CAT, "Decoding failed (error code: {})", e);
                gst::FlowError::Error
            })?;

        let mut decoded_pictures = vec![];
        for pic in pictures {
            let format = self.gst_video_format_from_dav1d_picture(&pic);
            if format != gst_video::VideoFormat::Unknown {
                decoded_pictures.push((pic, format));
            } else {
                return Err(gst::FlowError::NotNegotiated);
            }
        }
        Ok(decoded_pictures)
    }

    pub fn decoded_picture_as_buffer(
        &self,
        pic: &dav1d::Picture,
        output_state: gst_video::VideoCodecState<gst_video::video_codec_state::Readable>,
    ) -> Result<gst::Buffer, gst::FlowError> {
        let mut offsets = vec![];
        let mut strides = vec![];
        let mut acc_offset: usize = 0;

        let video_meta_supported = self.negotiation_infos.lock().unwrap().video_meta_supported;

        let info = output_state.info();
        let mut out_buffer = gst::Buffer::new();
        let mut_buffer = out_buffer.get_mut().unwrap();

        // FIXME: For gray support we would need to deal only with the Y component.
        assert!(info.is_yuv());
        for component in [
            dav1d::PlanarImageComponent::Y,
            dav1d::PlanarImageComponent::U,
            dav1d::PlanarImageComponent::V,
        ]
        .iter()
        {
            let dest_stride: u32 = info.stride()[*component as usize].try_into().unwrap();
            let plane = pic.plane(*component);
            let (src_stride, height) = pic.plane_data_geometry(*component);
            let mem = if video_meta_supported || src_stride == dest_stride {
                gst::Memory::from_slice(plane)
            } else {
                gst_trace!(
                    gst::CAT_PERFORMANCE,
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

    fn handle_picture(
        &self,
        element: &super::Dav1dDec,
        pic: &dav1d::Picture,
        format: gst_video::VideoFormat,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.handle_resolution_change(element, &pic, format)?;

        let output_state = element
            .output_state()
            .expect("Output state not set. Shouldn't happen!");
        let offset = pic.offset() as i32;
        if let Some(mut frame) = element.frame(offset) {
            let output_buffer = self.decoded_picture_as_buffer(&pic, output_state)?;
            frame.set_output_buffer(output_buffer);
            element.finish_frame(frame)?;
        } else {
            gst_warning!(CAT, obj: element, "No frame found for offset {}", offset);
        }

        self.forward_pending_pictures(element)
    }

    fn drop_decoded_pictures(&self) {
        let mut decoder = self.decoder.lock().unwrap();
        while let Ok(pic) = decoder.get_picture() {
            gst_debug!(CAT, "Dropping picture");
            drop(pic);
        }
    }

    fn pending_pictures(
        &self,
    ) -> Result<Vec<(dav1d::Picture, gst_video::VideoFormat)>, gst::FlowError> {
        let mut decoder = self.decoder.lock().unwrap();
        let mut pictures = vec![];
        while let Ok(pic) = decoder.get_picture() {
            let format = self.gst_video_format_from_dav1d_picture(&pic);
            if format == gst_video::VideoFormat::Unknown {
                return Err(gst::FlowError::NotNegotiated);
            }
            pictures.push((pic, format));
        }
        Ok(pictures)
    }

    fn forward_pending_pictures(
        &self,
        element: &super::Dav1dDec,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        for (pic, format) in self.pending_pictures()? {
            self.handle_picture(element, &pic, format)?;
        }
        Ok(gst::FlowSuccess::Ok)
    }
}

fn video_output_formats() -> Vec<glib::SendValue> {
    let values = [
        // gst_video::VideoFormat::Gray8,
        gst_video::VideoFormat::I420,
        gst_video::VideoFormat::Y42b,
        gst_video::VideoFormat::Y444,
        // #[cfg(target_endian = "little")]
        // gst_video::VideoFormat::Gray10Le32,
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

impl ObjectImpl for Dav1dDec {}

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
            let sink_caps = gst::Caps::new_simple("video/x-av1", &[]);
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::new_simple(
                "video/x-raw",
                &[
                    ("format", &gst::List::from_owned(video_output_formats())),
                    ("width", &gst::IntRange::<i32>::new(1, i32::MAX)),
                    ("height", &gst::IntRange::<i32>::new(1, i32::MAX)),
                    (
                        "framerate",
                        &gst::FractionRange::new(
                            gst::Fraction::new(0, 1),
                            gst::Fraction::new(i32::MAX, 1),
                        ),
                    ),
                ],
            );
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
    fn start(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        {
            let mut infos = self.negotiation_infos.lock().unwrap();
            infos.output_info = None;
        }

        self.parent_start(element)
    }

    fn set_format(
        &self,
        element: &Self::Type,
        state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        {
            let mut infos = self.negotiation_infos.lock().unwrap();
            infos.input_state = Some(state.clone());
        }

        self.parent_set_format(element, state)
    }

    fn handle_frame(
        &self,
        element: &Self::Type,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let input_buffer = frame.input_buffer().expect("frame without input buffer");
        for (pic, format) in self.decode(input_buffer, &frame)? {
            self.handle_picture(element, &pic, format)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn flush(&self, element: &Self::Type) -> bool {
        gst_info!(CAT, obj: element, "Flushing");
        self.flush_decoder();
        self.drop_decoded_pictures();
        true
    }

    fn drain(&self, element: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_info!(CAT, obj: element, "Draining");
        self.flush_decoder();
        self.forward_pending_pictures(element)?;
        self.parent_drain(element)
    }

    fn finish(&self, element: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_info!(CAT, obj: element, "Finishing");
        self.flush_decoder();
        self.forward_pending_pictures(element)?;
        self.parent_finish(element)
    }

    fn decide_allocation(
        &self,
        element: &Self::Type,
        query: &mut gst::QueryRef,
    ) -> Result<(), gst::ErrorMessage> {
        if let gst::query::QueryView::Allocation(allocation) = query.view() {
            if allocation
                .find_allocation_meta::<gst_video::VideoMeta>()
                .is_some()
            {
                let pools = allocation.allocation_pools();
                if let Some((Some(ref pool), _, _, _)) = pools.first() {
                    let mut config = pool.config();
                    config.add_option(&gst_video::BUFFER_POOL_OPTION_VIDEO_META);
                    pool.set_config(config)
                        .map_err(|e| gst::error_msg!(gst::CoreError::Negotiation, [&e.message]))?;
                    self.negotiation_infos.lock().unwrap().video_meta_supported = true;
                }
            }
        }

        self.parent_decide_allocation(element, query)
    }
}
