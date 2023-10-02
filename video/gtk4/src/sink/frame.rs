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

use gst_video::prelude::*;

#[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
use gst_gl::prelude::*;
use gtk::{gdk, glib};
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
enum MappedFrame {
    SysMem(gst_video::VideoFrame<gst_video::video_frame::Readable>),
    #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
    GL {
        frame: gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>,
        wrapped_context: gst_gl::GLContext,
    },
}

impl MappedFrame {
    fn buffer(&self) -> &gst::BufferRef {
        match self {
            MappedFrame::SysMem(frame) => frame.buffer(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
            MappedFrame::GL { frame, .. } => frame.buffer(),
        }
    }

    fn width(&self) -> u32 {
        match self {
            MappedFrame::SysMem(frame) => frame.width(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
            MappedFrame::GL { frame, .. } => frame.width(),
        }
    }

    fn height(&self) -> u32 {
        match self {
            MappedFrame::SysMem(frame) => frame.height(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
            MappedFrame::GL { frame, .. } => frame.height(),
        }
    }

    fn format_info(&self) -> gst_video::VideoFormatInfo {
        match self {
            MappedFrame::SysMem(frame) => frame.format_info(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
            MappedFrame::GL { frame, .. } => frame.format_info(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Frame {
    frame: MappedFrame,
    overlays: Vec<Overlay>,
}

#[derive(Debug)]
struct Overlay {
    frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    global_alpha: f32,
}

#[derive(Debug)]
pub(crate) struct Texture {
    pub texture: gdk::Texture,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub global_alpha: f32,
    pub has_alpha: bool,
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
        (frame.info().par().numer() as f64) / (frame.info().par().denom() as f64);

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

#[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
fn video_frame_to_gl_texture(
    frame: gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>,
    cached_textures: &mut HashMap<usize, gdk::Texture>,
    used_textures: &mut HashSet<usize>,
    gdk_context: &gdk::GLContext,
    wrapped_context: &gst_gl::GLContext,
) -> (gdk::Texture, f64) {
    let texture_id = frame.texture_id(0).expect("Invalid texture id") as usize;

    let pixel_aspect_ratio =
        (frame.info().par().numer() as f64) / (frame.info().par().denom() as f64);

    if let Some(texture) = cached_textures.get(&(texture_id)) {
        used_textures.insert(texture_id);
        return (texture.clone(), pixel_aspect_ratio);
    }

    let width = frame.width();
    let height = frame.height();

    let sync_meta = frame.buffer().meta::<gst_gl::GLSyncMeta>().unwrap();
    sync_meta.wait(wrapped_context);

    let texture = unsafe {
        gdk::GLTexture::with_release_func(
            gdk_context,
            texture_id as u32,
            width as i32,
            height as i32,
            move || {
                // Unmap and drop the GStreamer GL texture once GTK is done with it and not earlier
                drop(frame);
            },
        )
        .upcast::<gdk::Texture>()
    };

    cached_textures.insert(texture_id, texture.clone());
    used_textures.insert(texture_id);

    (texture, pixel_aspect_ratio)
}

impl Frame {
    pub(crate) fn into_textures(
        self,
        #[allow(unused_variables)] gdk_context: Option<&gdk::GLContext>,
        cached_textures: &mut HashMap<usize, gdk::Texture>,
    ) -> Vec<Texture> {
        let mut textures = Vec::with_capacity(1 + self.overlays.len());
        let mut used_textures = HashSet::with_capacity(1 + self.overlays.len());

        let width = self.frame.width();
        let height = self.frame.height();
        let has_alpha = self.frame.format_info().has_alpha();
        let (texture, pixel_aspect_ratio) = match self.frame {
            MappedFrame::SysMem(frame) => {
                video_frame_to_memory_texture(frame, cached_textures, &mut used_textures)
            }
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
            MappedFrame::GL {
                frame,
                wrapped_context,
            } => {
                let Some(gdk_context) = gdk_context else {
                    // This will fail badly if the video frame was actually mapped as GL texture
                    // but this case can't really happen as we only do that if we actually have a
                    // GDK GL context.
                    unreachable!();
                };
                video_frame_to_gl_texture(
                    frame,
                    cached_textures,
                    &mut used_textures,
                    gdk_context,
                    &wrapped_context,
                )
            }
        };

        textures.push(Texture {
            texture,
            x: 0.0,
            y: 0.0,
            width: width as f32 * pixel_aspect_ratio as f32,
            height: height as f32,
            global_alpha: 1.0,
            has_alpha,
        });

        for overlay in self.overlays {
            let has_alpha = overlay.frame.format_info().has_alpha();
            let (texture, _pixel_aspect_ratio) =
                video_frame_to_memory_texture(overlay.frame, cached_textures, &mut used_textures);

            textures.push(Texture {
                texture,
                x: overlay.x as f32,
                y: overlay.y as f32,
                width: overlay.width as f32,
                height: overlay.height as f32,
                global_alpha: overlay.global_alpha,
                has_alpha,
            });
        }

        // Remove textures that were not used this time
        cached_textures.retain(|id, _| used_textures.contains(id));

        textures
    }
}

impl Frame {
    pub(crate) fn new(
        buffer: &gst::Buffer,
        info: &gst_video::VideoInfo,
        #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))] wrapped_context: Option<
            &gst_gl::GLContext,
        >,
        #[allow(unused_variables)]
        #[cfg(not(any(target_os = "macos", target_os = "windows", feature = "gst_gl")))]
        wrapped_context: Option<&()>,
    ) -> Result<Self, gst::FlowError> {
        // Empty buffers get filtered out in show_frame
        debug_assert!(buffer.n_memory() > 0);

        let mut frame;

        #[cfg(not(any(target_os = "macos", target_os = "windows", feature = "gst_gl")))]
        {
            frame = Self {
                frame: MappedFrame::SysMem(
                    gst_video::VideoFrame::from_buffer_readable(buffer.clone(), info)
                        .map_err(|_| gst::FlowError::Error)?,
                ),
                overlays: vec![],
            };
        }
        #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst_gl"))]
        {
            // Check we received a buffer with GL memory and if the context of that memory
            // can share with the wrapped context around the GDK GL context.
            //
            // If not it has to be uploaded to the GPU.
            let memory_ctx = buffer
                .peek_memory(0)
                .downcast_memory_ref::<gst_gl::GLBaseMemory>()
                .and_then(|m| {
                    let ctx = m.context();
                    if wrapped_context
                        .map_or(false, |wrapped_context| wrapped_context.can_share(ctx))
                    {
                        Some(ctx)
                    } else {
                        None
                    }
                });

            if let Some(memory_ctx) = memory_ctx {
                // If there is no GLSyncMeta yet then we need to add one here now, which requires
                // obtaining a writable buffer.
                let mapped_frame = if buffer.meta::<gst_gl::GLSyncMeta>().is_some() {
                    gst_gl::GLVideoFrame::from_buffer_readable(buffer.clone(), info)
                        .map_err(|_| gst::FlowError::Error)?
                } else {
                    let mut buffer = buffer.clone();
                    {
                        let buffer = buffer.make_mut();
                        gst_gl::GLSyncMeta::add(buffer, memory_ctx);
                    }
                    gst_gl::GLVideoFrame::from_buffer_readable(buffer, info)
                        .map_err(|_| gst::FlowError::Error)?
                };

                // Now that it's guaranteed that there is a sync meta and the frame is mapped, set
                // a sync point so we can ensure that the texture is ready later when making use of
                // it as gdk::GLTexture.
                let meta = mapped_frame.buffer().meta::<gst_gl::GLSyncMeta>().unwrap();
                meta.set_sync_point(memory_ctx);

                frame = Self {
                    frame: MappedFrame::GL {
                        frame: mapped_frame,
                        wrapped_context: wrapped_context.unwrap().clone(),
                    },
                    overlays: vec![],
                };
            } else {
                frame = Self {
                    frame: MappedFrame::SysMem(
                        gst_video::VideoFrame::from_buffer_readable(buffer.clone(), info)
                            .map_err(|_| gst::FlowError::Error)?,
                    ),
                    overlays: vec![],
                };
            }
        }

        frame.overlays = frame
            .frame
            .buffer()
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .flat_map(|meta| {
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
            .collect();

        Ok(frame)
    }
}
