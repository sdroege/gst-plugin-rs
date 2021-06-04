// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::gst_debug;
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use image::GenericImageView;
use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::constants::{CDG_HEIGHT, CDG_WIDTH};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("cdgdec", gst::DebugColorFlags::empty(), Some("CDG decoder"))
});

#[derive(Default)]
pub struct CdgDec {
    cdg_inter: Mutex<Box<cdg_renderer::CdgInterpreter>>,
    output_info: Mutex<Option<gst_video::VideoInfo>>,
}

#[glib::object_subclass]
impl ObjectSubclass for CdgDec {
    const NAME: &'static str = "CdgDec";
    type Type = super::CdgDec;
    type ParentType = gst_video::VideoDecoder;
}

impl ObjectImpl for CdgDec {}

impl ElementImpl for CdgDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "CDG decoder",
                "Decoder/Video",
                "CDG decoder",
                "Guillaume Desmottes <guillaume.desmottes@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::new_simple("video/x-cdg", &[("parsed", &true)]);
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
                    ("format", &gst_video::VideoFormat::Rgba.to_str()),
                    ("width", &(CDG_WIDTH as i32)),
                    ("height", &(CDG_HEIGHT as i32)),
                    ("framerate", &gst::Fraction::new(0, 1)),
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

impl VideoDecoderImpl for CdgDec {
    fn start(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut out_info = self.output_info.lock().unwrap();
        *out_info = None;

        self.parent_start(element)
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        {
            let mut cdg_inter = self.cdg_inter.lock().unwrap();
            cdg_inter.reset(true);
        }
        self.parent_stop(element)
    }

    fn handle_frame(
        &self,
        element: &Self::Type,
        mut frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        {
            let mut out_info = self.output_info.lock().unwrap();
            if out_info.is_none() {
                let output_state = element.set_output_state(
                    gst_video::VideoFormat::Rgba,
                    CDG_WIDTH,
                    CDG_HEIGHT,
                    None,
                )?;

                element.negotiate(output_state)?;

                let out_state = element.output_state().unwrap();
                *out_info = Some(out_state.info());
            }
        }

        let cmd = {
            let input = frame.input_buffer().unwrap();
            let map = input.map_readable().map_err(|_| {
                gst::element_error!(
                    element,
                    gst::CoreError::Failed,
                    ["Failed to map input buffer readable"]
                );
                gst::FlowError::Error
            })?;
            let data = map.as_slice();

            cdg::decode_subchannel_cmd(&data)
        };

        let cmd = match cmd {
            Some(cmd) => cmd,
            None => {
                // Not a CDG command
                element.release_frame(frame);
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let mut cdg_inter = self.cdg_inter.lock().unwrap();
        cdg_inter.handle_cmd(cmd);

        element.allocate_output_frame(&mut frame, None)?;
        {
            let output = frame.output_buffer_mut().unwrap();
            let info = self.output_info.lock().unwrap();

            let mut out_frame =
                gst_video::VideoFrameRef::from_buffer_ref_writable(output, info.as_ref().unwrap())
                    .map_err(|_| {
                        gst::element_error!(
                            element,
                            gst::CoreError::Failed,
                            ["Failed to map output buffer writable"]
                        );
                        gst::FlowError::Error
                    })?;

            let out_stride = out_frame.plane_stride()[0] as usize;

            for (y, line) in out_frame
                .plane_data_mut(0)
                .unwrap()
                .chunks_exact_mut(out_stride)
                .take(CDG_HEIGHT as usize)
                .enumerate()
            {
                for (x, pixel) in line
                    .chunks_exact_mut(4)
                    .take(CDG_WIDTH as usize)
                    .enumerate()
                {
                    let p = cdg_inter.get_pixel(x as u32, y as u32);
                    pixel.copy_from_slice(&p.0);
                }
            }
        }

        gst_debug!(
            CAT,
            obj: element,
            "Finish frame pts={}",
            frame.pts().display()
        );

        element.finish_frame(frame)
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
                }
            }
        }

        self.parent_decide_allocation(element, query)
    }

    fn flush(&self, element: &Self::Type) -> bool {
        gst_debug!(CAT, obj: element, "flushing, reset CDG interpreter");

        let mut cdg_inter = self.cdg_inter.lock().unwrap();
        cdg_inter.reset(false);
        true
    }
}
