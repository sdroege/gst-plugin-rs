// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use cdg;
use cdg_renderer;
use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::subclass::prelude::*;
use gst_video::prelude::VideoDecoderExtManual;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use gstreamer_video as gst_video;
use image::GenericImageView;
use std::sync::Mutex;

use crate::constants::{CDG_HEIGHT, CDG_WIDTH};

struct CdgDec {
    cdg_inter: Mutex<cdg_renderer::CdgInterpreter>,
    output_info: Mutex<Option<gst_video::VideoInfo>>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory =
        gst::DebugCategory::new("cdgdec", gst::DebugColorFlags::empty(), Some("CDG decoder"),);
}

impl ObjectSubclass for CdgDec {
    const NAME: &'static str = "CdgDec";
    type ParentType = gst_video::VideoDecoder;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            cdg_inter: Mutex::new(cdg_renderer::CdgInterpreter::new()),
            output_info: Mutex::new(None),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "CDG decoder",
            "Decoder/Video",
            "CDG decoder",
            "Guillaume Desmottes <guillaume.desmottes@collabora.com>",
        );

        let sink_caps = gst::Caps::new_simple("video/x-cdg", &[("parsed", &true)]);
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &sink_caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

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
        klass.add_pad_template(src_pad_template);
    }
}

impl ObjectImpl for CdgDec {
    glib_object_impl!();
}

impl ElementImpl for CdgDec {}

impl VideoDecoderImpl for CdgDec {
    fn start(&self, element: &gst_video::VideoDecoder) -> Result<(), gst::ErrorMessage> {
        let mut out_info = self.output_info.lock().unwrap();
        *out_info = None;

        self.parent_start(element)
    }

    fn stop(&self, element: &gst_video::VideoDecoder) -> Result<(), gst::ErrorMessage> {
        {
            let mut cdg_inter = self.cdg_inter.lock().unwrap();
            cdg_inter.reset(true);
        }
        self.parent_stop(element)
    }

    fn handle_frame(
        &self,
        element: &gst_video::VideoDecoder,
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

                let out_state = element.get_output_state().unwrap();
                *out_info = Some(out_state.get_info());
            }
        }

        let cmd = {
            let input = frame.get_input_buffer().unwrap();
            let map = input.map_readable().map_err(|_| {
                gst_element_error!(
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
            let output = frame.get_output_buffer_mut().unwrap();
            let info = self.output_info.lock().unwrap();

            let mut out_frame =
                gst_video::VideoFrameRef::from_buffer_ref_writable(output, info.as_ref().unwrap())
                    .map_err(|_| {
                        gst_element_error!(
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

        gst_debug!(CAT, obj: element, "Finish frame pts={}", frame.get_pts());

        element.finish_frame(frame)
    }

    fn decide_allocation(
        &self,
        element: &gst_video::VideoDecoder,
        query: &mut gst::QueryRef,
    ) -> Result<(), gst::ErrorMessage> {
        if let gst::query::QueryView::Allocation(allocation) = query.view() {
            if allocation
                .find_allocation_meta::<gst_video::VideoMeta>()
                .is_some()
            {
                let pools = allocation.get_allocation_pools();
                if let Some((ref pool, _, _, _)) = pools.first() {
                    if let Some(pool) = pool {
                        let mut config = pool.get_config();
                        config.add_option(&gst_video::BUFFER_POOL_OPTION_VIDEO_META);
                        pool.set_config(config).map_err(|e| {
                            gst::gst_error_msg!(gst::CoreError::Negotiation, [&e.message])
                        })?;
                    }
                }
            }
        }

        self.parent_decide_allocation(element, query)
    }

    fn flush(&self, element: &gst_video::VideoDecoder) -> bool {
        gst_debug!(CAT, obj: element, "flushing, reset CDG interpreter");

        let mut cdg_inter = self.cdg_inter.lock().unwrap();
        cdg_inter.reset(false);
        true
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cdgdec",
        gst::Rank::Primary,
        CdgDec::get_type(),
    )
}
