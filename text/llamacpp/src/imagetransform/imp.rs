// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*, subclass::prelude::*};
use llama_cpp_2::{
    llama_batch::LlamaBatch,
    mtmd::{MtmdBitmap, MtmdContext, MtmdInputChunkType, MtmdInputText, mtmd_default_marker},
};

use std::{
    borrow::Cow,
    str::FromStr,
    sync::{LazyLock, Mutex},
};

use gst_video::{VideoFormat, VideoFrameRef, VideoInfo, prelude::*};

use crate::basetransform::{
    LlamaBaseTransform, LlamaBaseTransformImpl, LlamaBaseTransformImplExt,
    imp::{Message, ModelState},
};

// Default values of properties
const DEFAULT_MMPROJ_PATH: Option<&str> = None;

struct Settings {
    mmproj_path: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mmproj_path: DEFAULT_MMPROJ_PATH.map(str::to_string),
        }
    }
}

#[derive(Default)]
pub struct ImageTransform {
    settings: Mutex<Settings>,
    video_info: Mutex<Option<VideoInfo>>,
}

/// Remove the stride padding from RGB video frame rows.
///
/// GStreamer may pad rows to a stride larger than `width * 3`, while
/// `MtmdBitmap::from_image_data` requires a tightly packed buffer.
fn compact_rgb_rows(data: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let row_bytes = width * 3;
    let mut rgb = Vec::with_capacity(row_bytes * height);
    for row in data.chunks_exact(stride).take(height) {
        rgb.extend_from_slice(&row[..row_bytes]);
    }
    rgb
}

/// Decodes the given image buffer into an `MtmdBitmap`.
///
/// If `video_info` is `Some` the buffer is interpreted as a raw RGB video frame, otherwise it is
/// decoded as an image container format (e.g. JPEG or PNG).
fn decode_image(
    mtmd: &MtmdContext,
    video_info: Option<&VideoInfo>,
    buf: &gst::BufferRef,
) -> Result<MtmdBitmap, gst::FlowError> {
    match video_info {
        Some(video_info) => {
            if video_info.format() != VideoFormat::Rgb {
                gst::error!(
                    crate::CAT,
                    "Unsupported video format: {:?}",
                    video_info.format()
                );
                return Err(gst::FlowError::Error);
            }
            let frame = match VideoFrameRef::from_buffer_ref_readable(buf, video_info) {
                Ok(frame) => frame,
                Err(err) => {
                    gst::error!(crate::CAT, "Failed to map video frame: {err}");
                    return Err(gst::FlowError::Error);
                }
            };
            let data = match frame.plane_data(0) {
                Ok(data) => data,
                Err(err) => {
                    gst::error!(crate::CAT, "Failed to get video frame data: {err}");
                    return Err(gst::FlowError::Error);
                }
            };
            let width = video_info.width() as usize;
            let height = video_info.height() as usize;
            let stride = frame.plane_stride()[0] as usize;
            let rgb = if stride == width * 3 {
                Cow::Borrowed(data)
            } else {
                Cow::Owned(compact_rgb_rows(data, width, height, stride))
            };
            match MtmdBitmap::from_image_data(video_info.width(), video_info.height(), &rgb) {
                Ok(bitmap) => Ok(bitmap),
                Err(err) => {
                    gst::error!(crate::CAT, "Failed to create bitmap: {err}");
                    Err(gst::FlowError::Error)
                }
            }
        }
        None => {
            let map = match buf.map_readable() {
                Ok(map) => map,
                Err(_) => {
                    gst::error!(crate::CAT, "Failed to map buffer");
                    return Err(gst::FlowError::Error);
                }
            };
            decode_bitmap(mtmd, &map)
        }
    }
}

fn decode_bitmap(mtmd: &MtmdContext, data: &[u8]) -> Result<MtmdBitmap, gst::FlowError> {
    match MtmdBitmap::from_buffer(mtmd, data, false) {
        Ok(bitmap) => Ok(bitmap),
        Err(err) => {
            gst::error!(crate::CAT, "Failed to decode image: {err}");
            Err(gst::FlowError::Error)
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ImageTransform {
    const NAME: &'static str = "GstLlamaCppImageTransform";
    type Type = super::ImageTransform;
    type ParentType = LlamaBaseTransform;
}

impl ObjectImpl for ImageTransform {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("mmproj-path")
                    .nick("MMProj Path")
                    .blurb("Path to the GGUF multimodal projection model file")
                    .default_value(DEFAULT_MMPROJ_PATH)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "mmproj-path" => {
                let mut settings = self.settings.lock().unwrap();
                let mmproj_path = value.get().expect("type checked upstream");
                gst::info!(
                    crate::CAT,
                    imp = self,
                    "Changing mmproj-path from {:?} to {:?}",
                    settings.mmproj_path,
                    mmproj_path
                );
                settings.mmproj_path = mmproj_path;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "mmproj-path" => {
                let settings = self.settings.lock().unwrap();
                settings.mmproj_path.as_deref().to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ImageTransform {}

impl ElementImpl for ImageTransform {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "llama.cpp Image Transform",
                "Text/LLM/Video",
                "Transforms an image to text based on a vision LLM and a system prompt",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::from_str(
                "image/jpeg;image/png;video/x-raw,format=RGB,width=[1, max],height=[1, max]",
            )
            .unwrap();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl LlamaBaseTransformImpl for ImageTransform {
    const REQUIRE_MMPROJ: bool = true;

    fn transform_buffer(
        &self,
        buf: &gst::BufferRef,
    ) -> Result<(String, Option<MtmdBitmap>), gst::FlowError> {
        let video_info = self.video_info.lock().unwrap().clone();

        let obj = self.obj();
        let bitmap = obj
            .upcast_ref::<LlamaBaseTransform>()
            .imp()
            .with_mtmd(|mtmd| decode_image(mtmd, video_info.as_ref(), buf))?;

        Ok((mtmd_default_marker().to_string(), Some(bitmap)))
    }

    fn transform_prompt(
        &self,
        model: &mut ModelState,
        messages: &[Message],
        prompt: &str,
    ) -> Result<String, gst::FlowError> {
        let Some(mtmd) = &mut model.mtmd else {
            gst::error!(
                crate::CAT,
                imp = self,
                "No multimodal projection model loaded"
            );
            return Err(gst::FlowError::Error);
        };

        // The rendered prompt contains one media marker per image message
        let bitmaps: Vec<&MtmdBitmap> = messages.iter().filter_map(|m| m.image.as_ref()).collect();

        let chunks = match mtmd.tokenize(
            MtmdInputText {
                text: prompt.to_string(),
                add_special: true,
                parse_special: true,
            },
            &bitmaps,
        ) {
            Ok(chunks) => chunks,
            Err(err) => {
                gst::error!(crate::CAT, imp = self, "Failed to tokenize prompt: {err}");
                return Err(gst::FlowError::Error);
            }
        };

        if crate::CAT.above_threshold(gst::DebugLevel::Memdump) {
            gst::memdump!(crate::CAT, imp = self, "Prompt: {prompt}");
        }

        let Some(last_chunk) = chunks.len().checked_sub(1).and_then(|i| chunks.get(i)) else {
            gst::error!(crate::CAT, imp = self, "No chunks after tokenizing prompt");
            return Err(gst::FlowError::Error);
        };
        // Image chunks never produce logits, so the last chunk must be non-empty text
        if last_chunk.chunk_type() != MtmdInputChunkType::Text || last_chunk.n_tokens() == 0 {
            gst::error!(
                crate::CAT,
                imp = self,
                "Last prompt chunk is not non-empty text, cannot sample from it"
            );
            return Err(gst::FlowError::Error);
        }

        // Pass the total token count as batch size so each chunk is
        // processed in a single llama_decode() call
        let n_past = match chunks.eval_chunks(
            mtmd,
            &mut model.context.0,
            0,
            0,
            chunks.total_tokens() as i32,
            true,
        ) {
            Ok(n_past) => n_past,
            Err(err) => {
                gst::error!(crate::CAT, imp = self, "Failed to process prompt: {err}");
                return Err(gst::FlowError::Error);
            }
        };

        let mut batch = LlamaBatch::new(1, 1);

        // Decode until end of generation
        let obj = self.obj();
        obj.upcast_ref::<LlamaBaseTransform>().imp().generate(
            model,
            &mut batch,
            n_past,
            last_chunk.n_tokens() as i32 - 1,
        )
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        if let gst::EventView::Caps(caps) = event.view() {
            let caps = caps.caps();
            let video_info = caps
                .structure(0)
                .filter(|s| s.name() == "video/x-raw")
                .and_then(|_| VideoInfo::from_caps(caps).ok());
            *self.video_info.lock().unwrap() = video_info;
        }

        self.parent_sink_event(pad, event)
    }

    fn mmproj_path(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();
        settings.mmproj_path.clone()
    }
}
