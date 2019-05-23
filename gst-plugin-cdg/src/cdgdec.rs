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
use glib::Cast;
use gst;
use gst::subclass::prelude::*;
use gst::{ClockTime, SECOND_VAL};
use gst_video::prelude::VideoDecoderExtManual;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_video as gst_video;
use image::GenericImage;
use muldiv::MulDiv;
use std::sync::Mutex;

const CDG_PACKET_SIZE: i32 = 24;
// 75 sectors/sec * 4 packets/sector = 300 packets/sec
const CDG_PACKET_PERIOD: u64 = 300;

const CDG_WIDTH: u32 = 300;
const CDG_HEIGHT: u32 = 216;

struct CdgDec {
    cat: gst::DebugCategory,
    cdg_inter: Mutex<cdg_renderer::CdgInterpreter>,
    output_info: Mutex<Option<gst_video::VideoInfo>>,
}

impl ObjectSubclass for CdgDec {
    const NAME: &'static str = "CdgDec";
    type ParentType = gst_video::VideoDecoder;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "cdgdec",
                gst::DebugColorFlags::empty(),
                Some("CDG decoder"),
            ),
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

        let sink_caps = gst::Caps::new_simple("video/x-cdg", &[]);
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
                ("format", &gst_video::VideoFormat::Rgba.to_string()),
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

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let dec = obj.downcast_ref::<gst_video::VideoDecoder>().unwrap();
        dec.set_packetized(false);
    }
}

impl ElementImpl for CdgDec {}

impl VideoDecoderImpl for CdgDec {
    fn start(&self, element: &gst_video::VideoDecoder) -> Result<(), gst::ErrorMessage> {
        let mut out_info = self.output_info.lock().unwrap();
        *out_info = None;

        self.parent_start(element)
    }

    fn parse(
        &self,
        element: &gst_video::VideoDecoder,
        _frame: &gst_video::VideoCodecFrame,
        adapter: &gst_base::Adapter,
        _at_eos: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // FIXME: scan for CDG header
        if adapter.available() >= CDG_PACKET_SIZE as usize {
            element.add_to_frame(CDG_PACKET_SIZE);
            element.have_frame()
        } else {
            Ok(gst::FlowSuccess::CustomSuccess)
        }
    }

    fn handle_frame(
        &self,
        element: &gst_video::VideoDecoder,
        frame: gst_video::VideoCodecFrame,
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
            let map = input.map_readable().ok_or_else(|| {
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

        element.allocate_output_frame(&frame, None)?;
        {
            let output = frame.get_output_buffer().unwrap();
            let info = self.output_info.lock().unwrap();

            let mut out_frame =
                gst_video::VideoFrameRef::from_buffer_ref_writable(output, info.as_ref().unwrap())
                    .ok_or_else(|| {
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
                    pixel[0] = p.data[0];
                    pixel[1] = p.data[1];
                    pixel[2] = p.data[2];
                    pixel[3] = p.data[3];
                }
            }
        }

        let pts = {
            // FIXME: this won't work when seeking
            let nb = frame.get_decode_frame_number() as u64;
            let ns = nb.mul_div_round(SECOND_VAL, CDG_PACKET_PERIOD).unwrap();
            ClockTime::from_nseconds(ns)
        };

        gst_debug!(self.cat, obj: element, "Finish frame pts={}", pts);

        frame.set_pts(pts);
        element.finish_frame(frame)
    }

    fn decide_allocation(
        &self,
        element: &gst_video::VideoDecoder,
        query: &mut gst::QueryRef,
    ) -> Result<(), gst::ErrorMessage> {
        self.parent_decide_allocation(element, query)?;

        if let gst::query::QueryView::Allocation(allocation) = query.view() {
            let pools = allocation.get_allocation_pools();
            if let Some((ref pool, _, _, _)) = pools.first() {
                if let Some(pool) = pool {
                    let mut config = pool.get_config();
                    config.add_option(&gst_video::BUFFER_POOL_OPTION_VIDEO_META);
                    pool.set_config(config).unwrap();
                }
            }
        }

        Ok(())
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(Some(plugin), "cdgdec", 0, CdgDec::get_type())
}
