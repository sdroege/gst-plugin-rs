// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::format::Bytes;
use gst::glib;
use gst::gst_debug;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::convert::TryInto;

use crate::constants::{
    CDG_COMMAND, CDG_HEIGHT, CDG_MASK, CDG_PACKET_PERIOD, CDG_PACKET_SIZE, CDG_WIDTH,
};

const CDG_CMD_MEMORY_PRESET: u8 = 1;
const CDG_CMD_MEMORY_LOAD_COLOR_TABLE_1: u8 = 30;
const CDG_CMD_MEMORY_LOAD_COLOR_TABLE_2: u8 = 31;

#[derive(Default)]
pub struct CdgParse;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cdgparse",
        gst::DebugColorFlags::empty(),
        Some("CDG parser"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for CdgParse {
    const NAME: &'static str = "CdgParse";
    type Type = super::CdgParse;
    type ParentType = gst_base::BaseParse;
}

impl ObjectImpl for CdgParse {}

impl ElementImpl for CdgParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "CDG parser",
                "Codec/Parser/Video",
                "CDG parser",
                "Guillaume Desmottes <guillaume.desmottes@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::new_simple("video/x-cdg", &[]);
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

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

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

fn bytes_to_time(bytes: Bytes) -> gst::ClockTime {
    let nb = bytes.0 / CDG_PACKET_SIZE as u64;
    gst::ClockTime::from_nseconds(
        nb.mul_div_round(*gst::ClockTime::SECOND, CDG_PACKET_PERIOD)
            .unwrap(),
    )
}

fn time_to_bytes(time: gst::ClockTime) -> Bytes {
    Bytes(
        time.nseconds()
            .mul_div_round(
                CDG_PACKET_PERIOD * CDG_PACKET_SIZE as u64,
                *gst::ClockTime::SECOND,
            )
            .unwrap(),
    )
}

impl BaseParseImpl for CdgParse {
    fn start(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        element.set_min_frame_size(CDG_PACKET_SIZE as u32);

        /* Set duration */
        let mut query = gst::query::Duration::new(gst::Format::Bytes);
        let pad = element.src_pad();
        if pad.query(&mut query) {
            let size = query.result();
            let bytes: Option<Bytes> = size.try_into().unwrap();
            element.set_duration(bytes.map(bytes_to_time), 0);
        }

        Ok(())
    }

    fn handle_frame(
        &self,
        element: &Self::Type,
        mut frame: gst_base::BaseParseFrame,
    ) -> Result<(gst::FlowSuccess, u32), gst::FlowError> {
        let pad = element.src_pad();
        if pad.current_caps().is_none() {
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

            pad.push_event(gst::event::Caps::new(&src_caps));
        }

        // Scan for CDG instruction
        let input = frame.buffer().unwrap();
        let skip = {
            let map = input.map_readable().map_err(|_| {
                gst::element_error!(
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
                .unwrap_or_else(|| input.size()) // skip the whole buffer
                as u32
        };

        if skip != 0 {
            // Skip to the start of the CDG packet
            return Ok((gst::FlowSuccess::Ok, skip));
        }

        let (keyframe, header) = {
            let map = input.map_readable().map_err(|_| {
                gst::element_error!(
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

        let pts = bytes_to_time(Bytes(frame.offset()));
        let buffer = frame.buffer_mut().unwrap();
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
        _element: &Self::Type,
        src_val: V,
        dest_format: gst::Format,
    ) -> Option<gst::GenericFormattedValue> {
        let src_val = src_val.into();

        match (src_val, dest_format) {
            (gst::GenericFormattedValue::Bytes(bytes), gst::Format::Time) => {
                Some(bytes.map(bytes_to_time).into())
            }
            (gst::GenericFormattedValue::Time(time), gst::Format::Bytes) => {
                Some(time.map(time_to_bytes).into())
            }
            _ => None,
        }
    }
}
