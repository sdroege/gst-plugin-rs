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

#[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
use gst_gl::prelude::*;
use gtk::{gdk, glib};
use std::{
    collections::{HashMap, HashSet},
    ops,
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VideoInfo {
    VideoInfo(gst_video::VideoInfo),
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    DmaDrm(gst_video::VideoInfoDmaDrm),
}

impl From<gst_video::VideoInfo> for VideoInfo {
    fn from(v: gst_video::VideoInfo) -> Self {
        VideoInfo::VideoInfo(v)
    }
}

#[cfg(all(target_os = "linux", feature = "dmabuf"))]
impl From<gst_video::VideoInfoDmaDrm> for VideoInfo {
    fn from(v: gst_video::VideoInfoDmaDrm) -> Self {
        VideoInfo::DmaDrm(v)
    }
}

impl ops::Deref for VideoInfo {
    type Target = gst_video::VideoInfo;

    fn deref(&self) -> &Self::Target {
        match self {
            VideoInfo::VideoInfo(info) => info,
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            VideoInfo::DmaDrm(info) => info,
        }
    }
}

impl VideoInfo {
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    fn dma_drm(&self) -> Option<&gst_video::VideoInfoDmaDrm> {
        match self {
            VideoInfo::VideoInfo(..) => None,
            VideoInfo::DmaDrm(info) => Some(info),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum TextureCacheId {
    Memory(usize),
    #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
    GL(usize),
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    DmaBuf([i32; 4]),
}

#[derive(Debug)]
enum MappedFrame {
    SysMem(gst_video::VideoFrame<gst_video::video_frame::Readable>),
    #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
    GL {
        frame: gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>,
        wrapped_context: gst_gl::GLContext,
    },
    #[cfg(all(target_os = "linux", feature = "dmabuf"))]
    DmaBuf {
        buffer: gst::Buffer,
        info: gst_video::VideoInfoDmaDrm,
        n_planes: u32,
        fds: [i32; 4],
        offsets: [usize; 4],
        strides: [usize; 4],
        width: u32,
        height: u32,
    },
}

impl MappedFrame {
    fn buffer(&self) -> &gst::BufferRef {
        match self {
            MappedFrame::SysMem(frame) => frame.buffer(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
            MappedFrame::GL { frame, .. } => frame.buffer(),
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            MappedFrame::DmaBuf { buffer, .. } => buffer,
        }
    }

    fn width(&self) -> u32 {
        match self {
            MappedFrame::SysMem(frame) => frame.width(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
            MappedFrame::GL { frame, .. } => frame.width(),
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            MappedFrame::DmaBuf { info, .. } => info.width(),
        }
    }

    fn height(&self) -> u32 {
        match self {
            MappedFrame::SysMem(frame) => frame.height(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
            MappedFrame::GL { frame, .. } => frame.height(),
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            MappedFrame::DmaBuf { info, .. } => info.height(),
        }
    }

    fn format_info(&self) -> gst_video::VideoFormatInfo {
        match self {
            MappedFrame::SysMem(frame) => frame.format_info(),
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
            MappedFrame::GL { frame, .. } => frame.format_info(),
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            MappedFrame::DmaBuf { info, .. } => info.format_info(),
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

fn video_format_to_memory_format(f: gst_video::VideoFormat) -> gdk::MemoryFormat {
    match f {
        #[cfg(feature = "gtk_v4_14")]
        gst_video::VideoFormat::Bgrx => gdk::MemoryFormat::B8g8r8x8,
        #[cfg(feature = "gtk_v4_14")]
        gst_video::VideoFormat::Xrgb => gdk::MemoryFormat::X8r8g8b8,
        #[cfg(feature = "gtk_v4_14")]
        gst_video::VideoFormat::Rgbx => gdk::MemoryFormat::R8g8b8x8,
        #[cfg(feature = "gtk_v4_14")]
        gst_video::VideoFormat::Xbgr => gdk::MemoryFormat::X8b8g8r8,
        gst_video::VideoFormat::Bgra => gdk::MemoryFormat::B8g8r8a8,
        gst_video::VideoFormat::Argb => gdk::MemoryFormat::A8r8g8b8,
        gst_video::VideoFormat::Rgba => gdk::MemoryFormat::R8g8b8a8,
        gst_video::VideoFormat::Abgr => gdk::MemoryFormat::A8b8g8r8,
        gst_video::VideoFormat::Rgb => gdk::MemoryFormat::R8g8b8,
        gst_video::VideoFormat::Bgr => gdk::MemoryFormat::B8g8r8,
        _ => unreachable!(),
    }
}

fn video_frame_to_memory_texture(
    frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    cached_textures: &mut HashMap<TextureCacheId, gdk::Texture>,
    used_textures: &mut HashSet<TextureCacheId>,
) -> (gdk::Texture, f64) {
    let ptr = frame.plane_data(0).unwrap().as_ptr() as usize;

    let pixel_aspect_ratio =
        (frame.info().par().numer() as f64) / (frame.info().par().denom() as f64);

    if let Some(texture) = cached_textures.get(&TextureCacheId::Memory(ptr)) {
        used_textures.insert(TextureCacheId::Memory(ptr));
        return (texture.clone(), pixel_aspect_ratio);
    }

    let format = video_format_to_memory_format(frame.format());
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

    cached_textures.insert(TextureCacheId::Memory(ptr), texture.clone());
    used_textures.insert(TextureCacheId::Memory(ptr));

    (texture, pixel_aspect_ratio)
}

#[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
fn video_frame_to_gl_texture(
    frame: gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>,
    cached_textures: &mut HashMap<TextureCacheId, gdk::Texture>,
    used_textures: &mut HashSet<TextureCacheId>,
    gdk_context: &gdk::GLContext,
    #[allow(unused)] wrapped_context: &gst_gl::GLContext,
) -> (gdk::Texture, f64) {
    let texture_id = frame.texture_id(0).expect("Invalid texture id") as usize;

    let pixel_aspect_ratio =
        (frame.info().par().numer() as f64) / (frame.info().par().denom() as f64);

    if let Some(texture) = cached_textures.get(&TextureCacheId::GL(texture_id)) {
        used_textures.insert(TextureCacheId::GL(texture_id));
        return (texture.clone(), pixel_aspect_ratio);
    }

    let width = frame.width();
    let height = frame.height();

    let sync_meta = frame.buffer().meta::<gst_gl::GLSyncMeta>().unwrap();

    let texture = unsafe {
        #[cfg(feature = "gtk_v4_12")]
        {
            let format = {
                let format = video_format_to_memory_format(frame.format());
                #[cfg(feature = "gtk_v4_14")]
                {
                    use gtk::prelude::*;

                    if gdk_context.api() != gdk::GLAPI::GLES || gdk_context.version().0 < 3 {
                        // Map alpha formats to the pre-multiplied versions because we pre-multiply
                        // ourselves if not GLES3 with the new GL renderer is used as the GTK GL
                        // backend does not natively support non-premultiplied formats.
                        match format {
                            gdk::MemoryFormat::B8g8r8a8 => gdk::MemoryFormat::B8g8r8a8Premultiplied,
                            gdk::MemoryFormat::A8r8g8b8 => gdk::MemoryFormat::A8r8g8b8Premultiplied,
                            gdk::MemoryFormat::R8g8b8a8 => gdk::MemoryFormat::R8g8b8a8Premultiplied,
                            gdk::MemoryFormat::A8b8g8r8 => gdk::MemoryFormat::A8r8g8b8Premultiplied,
                            gdk::MemoryFormat::R8g8b8 | gdk::MemoryFormat::B8g8r8 => format,
                            gdk::MemoryFormat::B8g8r8x8
                            | gdk::MemoryFormat::X8r8g8b8
                            | gdk::MemoryFormat::R8g8b8x8
                            | gdk::MemoryFormat::X8b8g8r8 => format,
                            _ => unreachable!(),
                        }
                    } else {
                        format
                    }
                }

                #[cfg(not(feature = "gtk_v4_14"))]
                {
                    // Map alpha formats to the pre-multiplied versions because we pre-multiply
                    // ourselves in pre-4.14 versions as the GTK GL backend does not natively
                    // support non-premultiplied formats
                    match format {
                        gdk::MemoryFormat::B8g8r8a8 => gdk::MemoryFormat::B8g8r8a8Premultiplied,
                        gdk::MemoryFormat::A8r8g8b8 => gdk::MemoryFormat::A8r8g8b8Premultiplied,
                        gdk::MemoryFormat::R8g8b8a8 => gdk::MemoryFormat::R8g8b8a8Premultiplied,
                        gdk::MemoryFormat::A8b8g8r8 => gdk::MemoryFormat::A8r8g8b8Premultiplied,
                        gdk::MemoryFormat::R8g8b8 | gdk::MemoryFormat::B8g8r8 => format,
                        _ => unreachable!(),
                    }
                }
            };
            let sync_point = (*sync_meta.as_ptr()).data;

            gdk::GLTextureBuilder::new()
                .set_context(Some(gdk_context))
                .set_id(texture_id as u32)
                .set_width(width as i32)
                .set_height(height as i32)
                .set_format(format)
                .set_sync(Some(sync_point))
                .build_with_release_func(move || {
                    // Unmap and drop the GStreamer GL texture once GTK is done with it and not earlier
                    drop(frame);
                })
        }
        #[cfg(not(feature = "gtk_v4_12"))]
        {
            sync_meta.wait(wrapped_context);

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
        }
        .upcast::<gdk::Texture>()
    };

    cached_textures.insert(TextureCacheId::GL(texture_id), texture.clone());
    used_textures.insert(TextureCacheId::GL(texture_id));

    (texture, pixel_aspect_ratio)
}

#[cfg(all(target_os = "linux", feature = "dmabuf"))]
#[allow(clippy::too_many_arguments)]
fn video_frame_to_dmabuf_texture(
    buffer: gst::Buffer,
    cached_textures: &mut HashMap<TextureCacheId, gdk::Texture>,
    used_textures: &mut HashSet<TextureCacheId>,
    info: &gst_video::VideoInfoDmaDrm,
    n_planes: u32,
    fds: &[i32; 4],
    offsets: &[usize; 4],
    strides: &[usize; 4],
    width: u32,
    height: u32,
) -> Result<(gdk::Texture, f64), glib::Error> {
    let pixel_aspect_ratio = (info.par().numer() as f64) / (info.par().denom() as f64);

    if let Some(texture) = cached_textures.get(&TextureCacheId::DmaBuf(*fds)) {
        used_textures.insert(TextureCacheId::DmaBuf(*fds));
        return Ok((texture.clone(), pixel_aspect_ratio));
    }

    let builder = gdk::DmabufTextureBuilder::new();
    builder.set_display(&gdk::Display::default().unwrap());
    builder.set_fourcc(info.fourcc());
    builder.set_modifier(info.modifier());
    builder.set_width(width);
    builder.set_height(height);
    builder.set_n_planes(n_planes);
    for plane in 0..(n_planes as usize) {
        builder.set_fd(plane as u32, fds[plane]);
        builder.set_offset(plane as u32, offsets[plane] as u32);
        builder.set_stride(plane as u32, strides[plane] as u32);
    }

    let texture = unsafe {
        builder.build_with_release_func(move || {
            drop(buffer);
        })?
    };

    cached_textures.insert(TextureCacheId::DmaBuf(*fds), texture.clone());
    used_textures.insert(TextureCacheId::DmaBuf(*fds));

    Ok((texture, pixel_aspect_ratio))
}

impl Frame {
    pub(crate) fn into_textures(
        self,
        #[allow(unused_variables)] gdk_context: Option<&gdk::GLContext>,
        cached_textures: &mut HashMap<TextureCacheId, gdk::Texture>,
    ) -> Result<Vec<Texture>, glib::Error> {
        let mut textures = Vec::with_capacity(1 + self.overlays.len());
        let mut used_textures = HashSet::with_capacity(1 + self.overlays.len());

        let width = self.frame.width();
        let height = self.frame.height();
        let has_alpha = self.frame.format_info().has_alpha();
        let (texture, pixel_aspect_ratio) = match self.frame {
            MappedFrame::SysMem(frame) => {
                video_frame_to_memory_texture(frame, cached_textures, &mut used_textures)
            }
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
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
            #[cfg(all(target_os = "linux", feature = "dmabuf"))]
            MappedFrame::DmaBuf {
                buffer,
                info,
                n_planes,
                fds,
                offsets,
                strides,
                width,
                height,
            } => video_frame_to_dmabuf_texture(
                buffer,
                cached_textures,
                &mut used_textures,
                &info,
                n_planes,
                &fds,
                &offsets,
                &strides,
                width,
                height,
            )?,
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

        Ok(textures)
    }
}

impl Frame {
    pub(crate) fn new(
        buffer: &gst::Buffer,
        info: &VideoInfo,
        #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))] wrapped_context: Option<
            &gst_gl::GLContext,
        >,
        #[allow(unused_variables)]
        #[cfg(not(any(target_os = "macos", target_os = "windows", feature = "gst-gl")))]
        wrapped_context: Option<&()>,
    ) -> Result<Self, gst::FlowError> {
        // Empty buffers get filtered out in show_frame
        debug_assert!(buffer.n_memory() > 0);

        #[allow(unused_mut)]
        let mut frame = None;

        #[cfg(all(target_os = "linux", feature = "dmabuf"))]
        {
            // Check we received a buffer with dmabuf memory and if so do some checks before
            // passing it onwards
            if frame.is_none()
                && buffer
                    .peek_memory(0)
                    .is_memory_type::<gst_allocators::DmaBufMemory>()
            {
                if let Some((vmeta, info)) =
                    Option::zip(buffer.meta::<gst_video::VideoMeta>(), info.dma_drm())
                {
                    let mut fds = [-1i32; 4];
                    let mut offsets = [0; 4];
                    let mut strides = [0; 4];
                    let n_planes = vmeta.n_planes() as usize;

                    let vmeta_offsets = vmeta.offset();
                    let vmeta_strides = vmeta.stride();

                    for plane in 0..n_planes {
                        let Some((range, skip)) =
                            buffer.find_memory(vmeta_offsets[plane]..(vmeta_offsets[plane] + 1))
                        else {
                            break;
                        };

                        let mem = buffer.peek_memory(range.start);
                        let Some(mem) = mem.downcast_memory_ref::<gst_allocators::DmaBufMemory>()
                        else {
                            break;
                        };

                        let fd = mem.fd();
                        fds[plane] = fd;
                        offsets[plane] = mem.offset() + skip;
                        strides[plane] = vmeta_strides[plane] as usize;
                    }

                    // All fds valid?
                    if fds[0..n_planes].iter().all(|fd| *fd != -1) {
                        frame = Some(MappedFrame::DmaBuf {
                            buffer: buffer.clone(),
                            info: info.clone(),
                            n_planes: n_planes as u32,
                            fds,
                            offsets,
                            strides,
                            width: vmeta.width(),
                            height: vmeta.height(),
                        });
                    }
                }
            }
            #[cfg(any(target_os = "macos", target_os = "windows", feature = "gst-gl"))]
            {
                if frame.is_none() {
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

                        frame = Some(MappedFrame::GL {
                            frame: mapped_frame,
                            wrapped_context: wrapped_context.unwrap().clone(),
                        });
                    }
                }
            }
        }

        let mut frame = Self {
            frame: match frame {
                Some(frame) => frame,
                None => MappedFrame::SysMem(
                    gst_video::VideoFrame::from_buffer_readable(buffer.clone(), info)
                        .map_err(|_| gst::FlowError::Error)?,
                ),
            },
            overlays: vec![],
        };

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
