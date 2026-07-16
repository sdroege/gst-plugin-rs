// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-colorlut
 * @see_also: d3d12colorlut
 *
 * Parses Adobe Cube LUT files and applies the parsed color LUT to video frames.
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 filesrc location=test.mp4 ! decodebin ! video/x-raw ! videoconvert ! colorlut location=PATH/TO/LUT.cube !  videoconvert ! autovideosink
 * ]|
 *
 * This pipeline converts decoded video frames to a format supported by
 * `colorlut` using `videoconvert`, then applies the parsed LUT. The processed
 * video frames are then rendered by videosink.
 *
 * Since: plugins-rs-0.16
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_base::subclass::prelude::*;
use gst_video::VideoFormat;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use crate::parser::*;

use byte_slice_cast::*;
use std::sync::{LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("colorlut", gst::DebugColorFlags::empty(), Some("Color LUT"))
});

#[derive(Default)]
struct Settings {
    location: Option<String>,
}

#[derive(Default)]
struct State {
    lut: Option<CubeLut>,
}

#[derive(Default)]
pub struct ColorLut {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for ColorLut {
    const NAME: &'static str = "GstColorLut";
    type Type = super::ColorLut;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for ColorLut {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Location of the LUT file to read from")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "location" => {
                let mut settings = self.settings.lock().unwrap();
                settings.location = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => {
                let settings = self.settings.lock().unwrap();
                settings.location.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ColorLut {}

impl ElementImpl for ColorLut {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Color LUT",
                "Filter/Effect/Video",
                "Apply color lookup table",
                "Seungha Yang <seungha@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let formats = if cfg!(target_endian = "big") {
                [
                    VideoFormat::Rgba64Be,
                    VideoFormat::Rgba64Le,
                    VideoFormat::Rgba,
                ]
            } else {
                [
                    VideoFormat::Rgba64Le,
                    VideoFormat::Rgba64Be,
                    VideoFormat::Rgba,
                ]
            };
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list(formats)
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for ColorLut {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let location = self
            .settings
            .lock()
            .unwrap()
            .location
            .clone()
            .ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["LUT file location is not configured"]
                )
            })?;

        let lut = CubeLut::parse_file(&location).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Read,
                ["Failed to parse LUT file {location}: {err}"]
            )
        })?;

        gst::trace!(CAT, imp = self, "Parsed LUT: {lut:?}");

        *self.state.lock().unwrap() = State { lut: Some(lut) };

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State::default();
        Ok(())
    }
}

impl VideoFilterImpl for ColorLut {
    fn transform_frame(
        &self,
        in_frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
        out_frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();

        let lut = state.lut.as_ref().ok_or_else(|| {
            gst::error!(CAT, imp = self, "No LUT configured");
            gst::FlowError::Error
        })?;

        match in_frame.format() {
            VideoFormat::Rgba => transform_rgba(lut, in_frame, out_frame),
            VideoFormat::Rgba64Le => transform_rgba64::<true>(lut, in_frame, out_frame),
            VideoFormat::Rgba64Be => transform_rgba64::<false>(lut, in_frame, out_frame),
            _ => unreachable!(),
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

fn transform_rgba(
    lut: &CubeLut,
    src: &gst_video::VideoFrameRef<&gst::BufferRef>,
    dst: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
) {
    match &lut.kind {
        CubeLutKind::Lut1D { .. } => transform_rgba_1d(lut, src, dst),
        CubeLutKind::Lut3D { .. } => transform_rgba_3d(lut, src, dst),
    }
}

fn transform_rgba_1d(
    lut: &CubeLut,
    src: &gst_video::VideoFrameRef<&gst::BufferRef>,
    dst: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
) {
    let width_in_bytes = src.width() as usize * 4;
    let height = src.height() as usize;

    let src_stride = src.plane_stride()[0] as usize;
    let dst_stride = dst.plane_stride()[0] as usize;

    let src_data = src.plane_data(0).unwrap();
    let dst_data = dst.plane_data_mut(0).unwrap();

    for (src_row, dst_row) in Iterator::zip(
        src_data.chunks(src_stride).take(height),
        dst_data.chunks_mut(dst_stride).take(height),
    ) {
        let src_row = &src_row[..width_in_bytes];
        let dst_row = &mut dst_row[..width_in_bytes];

        for (s, d) in Iterator::zip(src_row.chunks_exact(4), dst_row.chunks_exact_mut(4)) {
            for c in 0..3 {
                d[c] = apply_1d(lut, c, s[c]);
            }
            d[3] = s[3];
        }
    }
}

fn transform_rgba_3d(
    lut: &CubeLut,
    src: &gst_video::VideoFrameRef<&gst::BufferRef>,
    dst: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
) {
    let width_in_bytes = src.width() as usize * 4;
    let height = src.height() as usize;

    let src_stride = src.plane_stride()[0] as usize;
    let dst_stride = dst.plane_stride()[0] as usize;

    let src_data = src.plane_data(0).unwrap();
    let dst_data = dst.plane_data_mut(0).unwrap();

    for (src_row, dst_row) in Iterator::zip(
        src_data.chunks(src_stride).take(height),
        dst_data.chunks_mut(dst_stride).take(height),
    ) {
        let src_row = &src_row[..width_in_bytes];
        let dst_row = &mut dst_row[..width_in_bytes];

        for (s, d) in Iterator::zip(src_row.chunks_exact(4), dst_row.chunks_exact_mut(4)) {
            let out = apply_3d(lut, s[0], s[1], s[2]);
            d[..3].copy_from_slice(&out);
            d[3] = s[3];
        }
    }
}

fn transform_rgba64<const LE: bool>(
    lut: &CubeLut,
    src: &gst_video::VideoFrameRef<&gst::BufferRef>,
    dst: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
) {
    match &lut.kind {
        CubeLutKind::Lut1D { .. } => transform_rgba64_1d::<LE>(lut, src, dst),
        CubeLutKind::Lut3D { .. } => transform_rgba64_3d::<LE>(lut, src, dst),
    }
}

fn transform_rgba64_1d<const LE: bool>(
    lut: &CubeLut,
    src: &gst_video::VideoFrameRef<&gst::BufferRef>,
    dst: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
) {
    let width = src.width() as usize;
    let height = src.height() as usize;

    let src_stride_in_u16 = src.plane_stride()[0] as usize / 2;
    let dst_stride_in_u16 = dst.plane_stride()[0] as usize / 2;

    let src_data = src.plane_data(0).unwrap();
    let dst_data = dst.plane_data_mut(0).unwrap();

    let src_data = src_data.as_slice_of::<u16>().unwrap();
    let dst_data = dst_data.as_mut_slice_of::<u16>().unwrap();

    for (src_row, dst_row) in Iterator::zip(
        src_data.chunks(src_stride_in_u16).take(height),
        dst_data.chunks_mut(dst_stride_in_u16).take(height),
    ) {
        let src_row = &src_row[..width * 4];
        let dst_row = &mut dst_row[..width * 4];

        for (s, d) in Iterator::zip(src_row.chunks_exact(4), dst_row.chunks_exact_mut(4)) {
            for c in 0..3 {
                let v = if LE {
                    u16::from_le(s[c])
                } else {
                    u16::from_be(s[c])
                };

                let out = apply_1d_u16(lut, c, v);

                d[c] = if LE { out.to_le() } else { out.to_be() };
            }

            // Preserve alpha
            d[3] = s[3];
        }
    }
}

fn transform_rgba64_3d<const LE: bool>(
    lut: &CubeLut,
    src: &gst_video::VideoFrameRef<&gst::BufferRef>,
    dst: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
) {
    let width = src.width() as usize;
    let height = src.height() as usize;

    let src_stride_in_u16 = src.plane_stride()[0] as usize / 2;
    let dst_stride_in_u16 = dst.plane_stride()[0] as usize / 2;

    let src_data = src.plane_data(0).unwrap();
    let dst_data = dst.plane_data_mut(0).unwrap();

    let src_data = src_data.as_slice_of::<u16>().unwrap();
    let dst_data = dst_data.as_mut_slice_of::<u16>().unwrap();

    for (src_row, dst_row) in Iterator::zip(
        src_data.chunks(src_stride_in_u16).take(height),
        dst_data.chunks_mut(dst_stride_in_u16).take(height),
    ) {
        let src_row = &src_row[..width * 4];
        let dst_row = &mut dst_row[..width * 4];

        for (s, d) in Iterator::zip(src_row.chunks_exact(4), dst_row.chunks_exact_mut(4)) {
            let (r, g, b) = if LE {
                (u16::from_le(s[0]), u16::from_le(s[1]), u16::from_le(s[2]))
            } else {
                (u16::from_be(s[0]), u16::from_be(s[1]), u16::from_be(s[2]))
            };

            let out = apply_3d_u16(lut, r, g, b);

            if LE {
                d[0] = out[0].to_le();
                d[1] = out[1].to_le();
                d[2] = out[2].to_le();
            } else {
                d[0] = out[0].to_be();
                d[1] = out[1].to_be();
                d[2] = out[2].to_be();
            }

            // Preserve alpha
            d[3] = s[3];
        }
    }
}

fn apply_1d(lut: &CubeLut, component: usize, value: u8) -> u8 {
    let CubeLutKind::Lut1D { size, r, g, b } = &lut.kind else {
        unreachable!();
    };

    let table = match component {
        0 => r,
        1 => g,
        2 => b,
        _ => unreachable!(),
    };

    let x = norm_comp(lut, component, value) * (*size as f32 - 1.0);
    float_to_u8(sample_1d(table, x))
}

fn apply_1d_u16(lut: &CubeLut, component: usize, value: u16) -> u16 {
    let CubeLutKind::Lut1D { size, r, g, b } = &lut.kind else {
        unreachable!();
    };

    let table = match component {
        0 => r,
        1 => g,
        2 => b,
        _ => unreachable!(),
    };

    let x = norm_comp_u16(lut, component, value) * (*size as f32 - 1.0);
    float_to_u16(sample_1d(table, x))
}

fn apply_3d(lut: &CubeLut, r: u8, g: u8, b: u8) -> [u8; 3] {
    let CubeLutKind::Lut3D(lut3d) = &lut.kind else {
        unreachable!();
    };

    let size = lut3d.size();

    let x = norm_comp(lut, 0, r) * (size as f32 - 1.0);
    let y = norm_comp(lut, 1, g) * (size as f32 - 1.0);
    let z = norm_comp(lut, 2, b) * (size as f32 - 1.0);

    let out = sample_3d(lut3d, x, y, z);

    [
        float_to_u8(out[0]),
        float_to_u8(out[1]),
        float_to_u8(out[2]),
    ]
}

fn apply_3d_u16(lut: &CubeLut, r: u16, g: u16, b: u16) -> [u16; 3] {
    let CubeLutKind::Lut3D(lut3d) = &lut.kind else {
        unreachable!();
    };

    let size = lut3d.size();

    let x = norm_comp_u16(lut, 0, r) * (size as f32 - 1.0);
    let y = norm_comp_u16(lut, 1, g) * (size as f32 - 1.0);
    let z = norm_comp_u16(lut, 2, b) * (size as f32 - 1.0);

    let out = sample_3d(lut3d, x, y, z);

    [
        float_to_u16(out[0]),
        float_to_u16(out[1]),
        float_to_u16(out[2]),
    ]
}

fn norm_comp(lut: &CubeLut, component: usize, value: u8) -> f32 {
    let v = value as f32 / 255.0;
    (v * lut.domain_scale[component] + lut.domain_offset[component]).clamp(0.0, 1.0)
}

fn norm_comp_u16(lut: &CubeLut, component: usize, value: u16) -> f32 {
    let v = value as f32 / 65535.0;
    (v * lut.domain_scale[component] + lut.domain_offset[component]).clamp(0.0, 1.0)
}

// linear
fn sample_1d(lut: &[f32], x: f32) -> f32 {
    let max_idx = lut.len() - 1;

    let x0 = (x.floor() as usize).min(max_idx);
    let x1 = (x0 + 1).min(max_idx);
    let t = x - x0 as f32;

    lut[x0] + (lut[x1] - lut[x0]) * t
}

// trilinear
fn sample_3d(lut: &Lut3D, x: f32, y: f32, z: f32) -> [f32; 4] {
    let max_idx = lut.size() - 1;

    let x0 = (x.floor() as usize).min(max_idx);
    let y0 = (y.floor() as usize).min(max_idx);
    let z0 = (z.floor() as usize).min(max_idx);

    let x1 = (x0 + 1).min(max_idx);
    let y1 = (y0 + 1).min(max_idx);
    let z1 = (z0 + 1).min(max_idx);

    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let tz = z - z0 as f32;

    let c000 = lut.at(x0, y0, z0);
    let c100 = lut.at(x1, y0, z0);
    let c010 = lut.at(x0, y1, z0);
    let c110 = lut.at(x1, y1, z0);
    let c001 = lut.at(x0, y0, z1);
    let c101 = lut.at(x1, y0, z1);
    let c011 = lut.at(x0, y1, z1);
    let c111 = lut.at(x1, y1, z1);

    let c00 = lerp4(c000, c100, tx);
    let c10 = lerp4(c010, c110, tx);
    let c01 = lerp4(c001, c101, tx);
    let c11 = lerp4(c011, c111, tx);

    let c0 = lerp4(c00, c10, ty);
    let c1 = lerp4(c01, c11, ty);

    lerp4(c0, c1, tz)
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

fn float_to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn float_to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}
