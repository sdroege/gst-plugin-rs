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
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct Frame {
    pub frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    pub overlays: Vec<Overlay>,
}

#[derive(Debug)]
pub struct Overlay {
    pub frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub global_alpha: f32,
}

#[derive(Debug)]
pub struct Texture {
    pub texture: gdk::Texture,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub global_alpha: f32,
}

struct FrameWrapper(gst_video::VideoFrame<gst_video::video_frame::Readable>);
impl AsRef<[u8]> for FrameWrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.plane_data(0).unwrap()
    }
}

fn video_frame_to_memory_texture(
    frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    cached_textures: &mut HashMap<usize, gdk::Texture>,
    used_textures: &mut HashSet<usize>,
) -> (gdk::Texture, f64) {
    let texture_id = frame.plane_data(0).unwrap().as_ptr() as usize;

    let pixel_aspect_ratio =
        (*frame.info().par().numer() as f64) / (*frame.info().par().denom() as f64);

    if let Some(texture) = cached_textures.get(&texture_id) {
        used_textures.insert(texture_id);
        return (texture.clone(), pixel_aspect_ratio);
    }

    let format = match frame.format() {
        gst_video::VideoFormat::Bgra => gdk::MemoryFormat::B8g8r8a8,
        gst_video::VideoFormat::Argb => gdk::MemoryFormat::A8r8g8b8,
        gst_video::VideoFormat::Rgba => gdk::MemoryFormat::R8g8b8a8,
        gst_video::VideoFormat::Abgr => gdk::MemoryFormat::A8b8g8r8,
        gst_video::VideoFormat::Rgb => gdk::MemoryFormat::R8g8b8,
        gst_video::VideoFormat::Bgr => gdk::MemoryFormat::B8g8r8,
        _ => unreachable!(),
    };
    let width = frame.width();
    let height = frame.height();
    let rowstride = frame.plane_stride()[0] as usize;

    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        format,
        &glib::Bytes::from_owned(FrameWrapper(frame)),
        rowstride,
    )
    .upcast::<gdk::Texture>();

    cached_textures.insert(texture_id, texture.clone());
    used_textures.insert(texture_id);

    (texture, pixel_aspect_ratio)
}

impl Frame {
    pub fn into_textures(self, cached_textures: &mut HashMap<usize, gdk::Texture>) -> Vec<Texture> {
        let mut textures = Vec::with_capacity(1 + self.overlays.len());
        let mut used_textures = HashSet::with_capacity(1 + self.overlays.len());

        let width = self.frame.width();
        let height = self.frame.height();
        let (texture, pixel_aspect_ratio) =
            video_frame_to_memory_texture(self.frame, cached_textures, &mut used_textures);

        textures.push(Texture {
            texture,
            x: 0.0,
            y: 0.0,
            width: width as f32 * pixel_aspect_ratio as f32,
            height: height as f32,
            global_alpha: 1.0,
        });

        for overlay in self.overlays {
            let (texture, _pixel_aspect_ratio) =
                video_frame_to_memory_texture(overlay.frame, cached_textures, &mut used_textures);

            textures.push(Texture {
                texture,
                x: overlay.x as f32,
                y: overlay.y as f32,
                width: overlay.width as f32,
                height: overlay.height as f32,
                global_alpha: overlay.global_alpha,
            });
        }

        // Remove textures that were not used this time
        cached_textures.retain(|id, _| used_textures.contains(id));

        textures
    }
}

impl Frame {
    pub fn new(buffer: &gst::Buffer, info: &gst_video::VideoInfo) -> Result<Self, gst::FlowError> {
        let frame = gst_video::VideoFrame::from_buffer_readable(buffer.clone(), info)
            .map_err(|_| gst::FlowError::Error)?;

        let overlays = frame
            .buffer()
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .map(|meta| {
                meta.overlay()
                    .iter()
                    .filter_map(|rect| {
                        let buffer = rect
                            .pixels_unscaled_argb(gst_video::VideoOverlayFormatFlags::GLOBAL_ALPHA);
                        let (x, y, width, height) = rect.render_rectangle();
                        let global_alpha = rect.global_alpha();

                        let vmeta = buffer.meta::<gst_video::VideoMeta>().unwrap();
                        let info = gst_video::VideoInfo::builder(
                            vmeta.format(),
                            vmeta.width(),
                            vmeta.height(),
                        )
                        .build()
                        .unwrap();
                        let frame =
                            gst_video::VideoFrame::from_buffer_readable(buffer, &info).ok()?;

                        Some(Overlay {
                            frame,
                            x,
                            y,
                            width,
                            height,
                            global_alpha,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .flatten()
            .collect();

        Ok(Self { frame, overlays })
    }
}
