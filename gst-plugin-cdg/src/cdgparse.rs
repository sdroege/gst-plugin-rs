// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::subclass::prelude::*;
use gst::SECOND_VAL;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer::format::Bytes;
use gstreamer_base as gst_base;
use std::convert::TryInto;

use crate::constants::{
    CDG_COMMAND, CDG_HEIGHT, CDG_MASK, CDG_PACKET_PERIOD, CDG_PACKET_SIZE, CDG_WIDTH,
};

const CDG_CMD_MEMORY_PRESET: u8 = 1;
const CDG_CMD_MEMORY_LOAD_COLOR_TABLE_1: u8 = 30;
const CDG_CMD_MEMORY_LOAD_COLOR_TABLE_2: u8 = 31;

struct CdgParse;

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "cdgparse",
        gst::DebugColorFlags::empty(),
        Some("CDG parser"),
    );
}

impl ObjectSubclass for CdgParse {
    const NAME: &'static str = "CdgParse";
    type ParentType = gst_base::BaseParse;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "CDG parser",
            "Codec/Parser/Video",
            "CDG parser",
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
            "video/x-cdg",
            &[
                ("width", &(CDG_WIDTH as i32)),
                ("height", &(CDG_HEIGHT as i32)),
                ("framerate", &gst::Fraction::new(0, 1)),
                ("parsed", &true),
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

impl ObjectImpl for CdgParse {
    glib_object_impl!();
}

impl ElementImpl for CdgParse {}

fn bytes_to_time(bytes: Bytes) -> gst::ClockTime {
    match bytes {
        Bytes(Some(bytes)) => {
            let nb = bytes / CDG_PACKET_SIZE as u64;
            gst::ClockTime(nb.mul_div_round(SECOND_VAL, CDG_PACKET_PERIOD))
        }
        Bytes(None) => gst::ClockTime::none(),
    }
}

fn time_to_bytes(time: gst::ClockTime) -> Bytes {
    match time.nseconds() {
        Some(time) => {
            let bytes = time.mul_div_round(CDG_PACKET_PERIOD * CDG_PACKET_SIZE as u64, SECOND_VAL);
            Bytes(bytes)
        }
        None => Bytes(None),
    }
}

impl BaseParseImpl for CdgParse {
    fn start(&self, element: &gst_base::BaseParse) -> Result<(), gst::ErrorMessage> {
        element.set_min_frame_size(CDG_PACKET_SIZE as u32);

        /* Set duration */
        let mut query = gst::Query::new_duration(gst::Format::Bytes);
        let pad = element.get_src_pad();
        if pad.query(&mut query) {
            let size = query.get_result();
            let duration = bytes_to_time(size.try_into().unwrap());
            element.set_duration(duration, 0);
        }

        Ok(())
    }

    fn handle_frame(
        &self,
        element: &gst_base::BaseParse,
        frame: gst_base::BaseParseFrame,
    ) -> Result<(gst::FlowSuccess, u32), gst::FlowError> {
        let pad = element.get_src_pad();
        if pad.get_current_caps().is_none() {
            // Set src pad caps
            let src_caps = gst::Caps::new_simple(
                "video/x-cdg",
                &[
                    ("width", &(CDG_WIDTH as i32)),
                    ("height", &(CDG_HEIGHT as i32)),
                    ("framerate", &gst::Fraction::new(0, 1)),
                    ("parsed", &true),
                ],
            );

            pad.push_event(gst::Event::new_caps(&src_caps).build());
        }

        // Scan for CDG instruction
        let input = frame.get_buffer().unwrap();
        let skip = {
            let map = input.map_readable().ok_or_else(|| {
                gst_element_error!(
                    element,
                    gst::CoreError::Failed,
                    ["Failed to map input buffer readable"]
                );
                gst::FlowError::Error
            })?;
            let data = map.as_slice();

            data.iter()
                .enumerate()
                .find(|(_, byte)| (*byte & CDG_MASK == CDG_COMMAND))
                .map(|(i, _)| i)
                .unwrap_or_else(|| input.get_size()) // skip the whole buffer
                as u32
        };

        if skip != 0 {
            // Skip to the start of the CDG packet
            return Ok((gst::FlowSuccess::Ok, skip));
        }

        let (keyframe, header) = {
            let map = input.map_readable().ok_or_else(|| {
                gst_element_error!(
                    element,
                    gst::CoreError::Failed,
                    ["Failed to map input buffer readable"]
                );
                gst::FlowError::Error
            })?;
            let data = map.as_slice();

            match data[1] & CDG_MASK {
                // consider memory preset as keyframe as it clears the screen
                CDG_CMD_MEMORY_PRESET => (true, false),
                // mark palette commands as headers
                CDG_CMD_MEMORY_LOAD_COLOR_TABLE_1 | CDG_CMD_MEMORY_LOAD_COLOR_TABLE_2 => {
                    (false, true)
                }
                _ => (false, false),
            }
        };

        let pts = bytes_to_time(Bytes(Some(frame.get_offset())));
        let buffer = frame.get_buffer().unwrap();
        buffer.set_pts(pts);

        if !keyframe {
            buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        if header {
            buffer.set_flags(gst::BufferFlags::HEADER);
        }

        gst_debug!(CAT, obj: element, "Found frame pts={}", pts);

        element.finish_frame(frame, CDG_PACKET_SIZE as u32)?;

        Ok((gst::FlowSuccess::Ok, skip))
    }

    fn convert<V: Into<gst::GenericFormattedValue>>(
        &self,
        _element: &gst_base::BaseParse,
        src_val: V,
        dest_format: gst::Format,
    ) -> Option<gst::GenericFormattedValue> {
        let src_val = src_val.into();

        match (src_val, dest_format) {
            (gst::GenericFormattedValue::Bytes(bytes), gst::Format::Time) => {
                Some(bytes_to_time(bytes).into())
            }
            (gst::GenericFormattedValue::Time(time), gst::Format::Bytes) => {
                Some(time_to_bytes(time).into())
            }
            _ => None,
        }
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cdgparse",
        gst::Rank::Primary,
        CdgParse::get_type(),
    )
}
