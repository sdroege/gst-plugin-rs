//
// Copyright (C) 2021 Bilal Elmoussaoui <bil.elmoussaoui@gmail.com>
// Copyright (C) 2021 Jordan Petridis <jordan@centricular.com>
// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gtk::prelude::*;
use gtk::{gdk, glib};
use std::convert::AsRef;

#[derive(Debug)]
pub struct Frame(pub gst_video::VideoFrame<gst_video::video_frame::Readable>);

#[derive(Debug)]
pub struct Paintable {
    pub paintable: gdk::Paintable,
    pub pixel_aspect_ratio: f64,
}

impl Paintable {
    pub fn width(&self) -> i32 {
        f64::round(self.paintable.intrinsic_width() as f64 * self.pixel_aspect_ratio) as i32
    }

    pub fn height(&self) -> i32 {
        self.paintable.intrinsic_height()
    }
}

impl AsRef<[u8]> for Frame {
    fn as_ref(&self) -> &[u8] {
        self.0.plane_data(0).unwrap()
    }
}

impl From<Frame> for Paintable {
    fn from(f: Frame) -> Paintable {
        let format = match f.0.format() {
            gst_video::VideoFormat::Bgra => gdk::MemoryFormat::B8g8r8a8,
            gst_video::VideoFormat::Argb => gdk::MemoryFormat::A8r8g8b8,
            gst_video::VideoFormat::Rgba => gdk::MemoryFormat::R8g8b8a8,
            gst_video::VideoFormat::Abgr => gdk::MemoryFormat::A8b8g8r8,
            gst_video::VideoFormat::Rgb => gdk::MemoryFormat::R8g8b8,
            gst_video::VideoFormat::Bgr => gdk::MemoryFormat::B8g8r8,
            _ => unreachable!(),
        };
        let width = f.0.width() as i32;
        let height = f.0.height() as i32;
        let rowstride = f.0.plane_stride()[0] as usize;

        let pixel_aspect_ratio =
            (*f.0.info().par().numer() as f64) / (*f.0.info().par().denom() as f64);

        Paintable {
            paintable: gdk::MemoryTexture::new(
                width,
                height,
                format,
                &glib::Bytes::from_owned(f),
                rowstride,
            )
            .upcast(),
            pixel_aspect_ratio,
        }
    }
}

impl Frame {
    pub fn new(buffer: &gst::Buffer, info: &gst_video::VideoInfo) -> Self {
        let video_frame =
            gst_video::VideoFrame::from_buffer_readable(buffer.clone(), info).unwrap();
        Self(video_frame)
    }

    pub fn width(&self) -> u32 {
        self.0.width()
    }

    pub fn height(&self) -> u32 {
        self.0.height()
    }
}
