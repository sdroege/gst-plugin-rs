// GStreamer RTP Raw Video Payloader/Depayloader - Video Frame Utility Functions
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst_video::{VideoFormat, VideoFrame, VideoFrameExt, video_frame::Writable};

struct Yuva {
    y: u8,
    u: u8,
    v: u8,
    _a: u8,
}

// Exact values aren't really important, just that we initialise the data
const BLACK: Yuva = Yuva {
    y: 16,
    u: 128,
    v: 128,
    _a: 255,
};

pub(crate) fn clear_frame(vframe: &mut VideoFrame<Writable>) {
    let width = vframe.width() as usize;

    let format = vframe.format();
    let format_info = vframe.format_info();

    let stride = vframe.plane_stride()[0] as usize;
    let pstride = vframe.comp_pstride(0) as usize;

    // RGB variants: we can just splat zeroes
    if format_info.is_rgb() {
        let data = vframe.plane_data_mut(0).unwrap();
        for line in data.chunks_exact_mut(stride) {
            line[0..][..width * pstride].fill(0);
        }
        return;
    }

    match format {
        // Uyvy: packed 4:2:2 YUV (U0-Y0-V0-Y1 U2-Y2-V2-Y3 U4 ...)
        VideoFormat::Uyvy => {
            let data = vframe.plane_data_mut(0).unwrap();
            for line in data.chunks_exact_mut(stride) {
                for pixel in line[0..][..width * 2].chunks_exact_mut(4) {
                    pixel[0] = BLACK.u;
                    pixel[1] = BLACK.y;
                    pixel[2] = BLACK.v;
                    pixel[3] = BLACK.y;
                }
            }
        }

        // Uyvp: packed 10-bit 4:2:2 YUV (U0-Y0-V0-Y1 U2-Y2-V2-Y3 U4 ...)
        VideoFormat::Uyvp => {
            let data = vframe.plane_data_mut(0).unwrap();
            for line in data.chunks_exact_mut(stride) {
                for macro_pixel in line.chunks_exact_mut(5) {
                    macro_pixel[0] = 0x80; // easier to hard-code this than do lots of bitshifting
                    macro_pixel[1] = 0x84;
                    macro_pixel[2] = 0x08;
                    macro_pixel[3] = 0x08;
                    macro_pixel[4] = 0x40;
                }
            }
        }

        // V308: packed 4:4:4 YUV (Y-U-V ...)
        VideoFormat::V308 => {
            let data = vframe.plane_data_mut(0).unwrap();
            for line in data.chunks_exact_mut(stride) {
                for pixel in line[0..][..width * 3].chunks_exact_mut(3) {
                    pixel[0] = BLACK.y;
                    pixel[1] = BLACK.u;
                    pixel[2] = BLACK.v;
                }
            }
        }

        // I420: planar 4:2:0
        VideoFormat::I420 => {
            /* Y plane */
            {
                let data = vframe.plane_data_mut(0).unwrap();
                for line in data.chunks_exact_mut(stride) {
                    line[0..][..width].fill(BLACK.y);
                }
            }
            /* U plane */
            {
                let stride_u = vframe.plane_stride()[1] as usize;
                let data = vframe.plane_data_mut(1).unwrap();
                for line in data.chunks_exact_mut(stride_u) {
                    line[0..][..width / 2].fill(BLACK.u);
                }
            }
            /* V plane */
            {
                let stride_v = vframe.plane_stride()[2] as usize;
                let data = vframe.plane_data_mut(2).unwrap();
                for line in data.chunks_exact_mut(stride_v) {
                    line[0..][..width / 2].fill(BLACK.v);
                }
            }
        }

        // Y41b: planar 4:1:1
        VideoFormat::Y41b => {
            /* Y plane */
            {
                let data = vframe.plane_data_mut(0).unwrap();
                for line in data.chunks_exact_mut(stride) {
                    line[0..][..width].fill(BLACK.y);
                }
            }
            /* U plane */
            {
                let stride_u = vframe.plane_stride()[1] as usize;
                let data = vframe.plane_data_mut(1).unwrap();
                for line in data.chunks_exact_mut(stride_u) {
                    line[0..][..width / 4].fill(BLACK.u);
                }
            }
            /* V plane */
            {
                let stride_v = vframe.plane_stride()[2] as usize;
                let data = vframe.plane_data_mut(2).unwrap();
                for line in data.chunks_exact_mut(stride_v) {
                    line[0..][..width / 4].fill(BLACK.v);
                }
            }
        }
        _ => unreachable!("Video format {format:?} unhandled"),
    }
}
