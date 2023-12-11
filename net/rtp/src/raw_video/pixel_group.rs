// GStreamer RTP Raw Video Payloader/Depayloader - Pixel Group Utility Functions
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst_video::{VideoFormat, VideoInfo};

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) struct PixelGroup {
    size: u8,
    x_inc: u8,
    y_inc: u8,
    direct: bool,
}

impl PixelGroup {
    pub(crate) fn from_video_info(vinfo: &VideoInfo) -> Result<Self, ()> {
        let mut pixel_group = match vinfo.format() {
            VideoFormat::Rgb => PixelGroup {
                size: 3,
                x_inc: 1,
                y_inc: 1,
                direct: true,
            },
            VideoFormat::Rgba => PixelGroup {
                size: 4,
                x_inc: 1,
                y_inc: 1,
                direct: true,
            },
            VideoFormat::Bgr => PixelGroup {
                size: 3,
                x_inc: 1,
                y_inc: 1,
                direct: true,
            },
            VideoFormat::Bgra => PixelGroup {
                size: 4,
                x_inc: 1,
                y_inc: 1,
                direct: true,
            },
            VideoFormat::V308 => PixelGroup {
                size: 3,
                x_inc: 1,
                y_inc: 1,
                direct: false, // Need to swizzle component order
            },
            VideoFormat::Uyvy => PixelGroup {
                size: 4,
                x_inc: 2,
                y_inc: 1,
                direct: true,
            },
            VideoFormat::Uyvp => PixelGroup {
                size: 5,
                x_inc: 2,
                y_inc: 1,
                direct: true,
            },
            VideoFormat::I420 => PixelGroup {
                size: 6,
                x_inc: 2,
                y_inc: 2,
                direct: false, // Need to re-pack from multiple planes
            },
            VideoFormat::Y41b => PixelGroup {
                size: 6,
                x_inc: 4,
                y_inc: 1,
                direct: false, // Need to re-pack from multiple planes
            },
            _ => return Err(()),
        };

        if vinfo.is_interlaced() {
            pixel_group.y_inc *= 2;
        }

        Ok(pixel_group)
    }

    pub(crate) fn size(&self) -> usize {
        self.size as usize
    }

    pub(crate) fn x_inc(&self) -> usize {
        self.x_inc as usize
    }

    pub(crate) fn y_inc(&self) -> usize {
        self.y_inc as usize
    }

    #[allow(dead_code)]
    pub(crate) fn is_direct(&self) -> bool {
        self.direct
    }
}
