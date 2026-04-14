// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on onvifmetadataoverlay, hsvdetectorm and gdkpixbufoverlay

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use image::{DynamicImage, ImageReader, Limits};

use std::sync::{LazyLock, Mutex};

use crate::buffer::Wrapper;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "imagersoverlay",
        gst::DebugColorFlags::empty(),
        Some("image-rs overlay"),
    )
});

#[derive(Default)]
struct State {
    composition: Option<gst_video::VideoOverlayComposition>,
    location: Option<String>,
    image: Option<gst::Buffer>,
    update_composition: bool,
    allow_attaching: bool,
}

#[derive(glib::Enum, Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
#[enum_type(name = "GstImageRsOverlayPositioningMode")]
#[repr(u32)]
pub enum PositioningMode {
    #[default]
    PixelsRelativeToEdges = 0,
    PixelsAbsolute = 1,
}

struct Settings {
    location: Option<String>,
    offset_x: i32,
    offset_y: i32,
    relative_x: f64,
    relative_y: f64,
    coef_x: f64,
    coef_y: f64,
    positioning_mode: PositioningMode,
    overlay_width: u32,
    overlay_height: u32,
    alpha: f32,
    max_alloc: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            location: Default::default(),
            offset_x: Default::default(),
            offset_y: Default::default(),
            relative_x: Default::default(),
            relative_y: Default::default(),
            coef_x: Default::default(),
            coef_y: Default::default(),
            positioning_mode: Default::default(),
            overlay_width: Default::default(),
            overlay_height: Default::default(),
            alpha: 1.0,
            max_alloc: Default::default(),
        }
    }
}

#[derive(Default)]
pub struct ImageRsOverlay {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl ImageRsOverlay {
    fn update_composition(&self, state: &mut State) {
        if !state.update_composition {
            return;
        }

        let in_info = self.obj().input_video_info().unwrap();
        let video_width = i64::from(in_info.width());
        let video_height = i64::from(in_info.height());

        state.composition = None;

        let settings = self.settings.lock().unwrap();
        if settings.alpha == 0.0 {
            state.update_composition = false;
            return;
        }

        let Some(overlay_pixels) = state.image.as_ref() else {
            state.update_composition = false;
            return;
        };

        let overlay_meta = overlay_pixels.meta::<gst_video::VideoMeta>().unwrap();
        let width: i64 = if settings.overlay_width == 0 {
            overlay_meta.width()
        } else {
            settings.overlay_width
        }
        .into();
        let height: i64 = if settings.overlay_height == 0 {
            overlay_meta.height()
        } else {
            settings.overlay_height
        }
        .into();

        let x = if settings.positioning_mode == PositioningMode::PixelsAbsolute {
            settings.offset_x as i64
                + (settings.relative_x * video_width as f64) as i64
                + (settings.coef_x * video_width as f64) as i64
        } else {
            if settings.offset_x < 0 {
                video_width + settings.offset_x as i64 - width
                    + (settings.relative_x * video_width as f64) as i64
            } else {
                settings.offset_x as i64 + (settings.relative_x * video_width as f64) as i64
            }
        }
        .clamp(i32::MIN as i64, i32::MAX as i64);
        let y = if settings.positioning_mode == PositioningMode::PixelsAbsolute {
            settings.offset_y as i64
                + (settings.relative_y * video_height as f64) as i64
                + (settings.coef_y * video_height as f64) as i64
        } else {
            if settings.offset_y < 0 {
                video_height + settings.offset_y as i64 - height
                    + (settings.relative_y * video_height as f64) as i64
            } else {
                settings.offset_y as i64 + (settings.relative_y * video_height as f64) as i64
            }
        }
        .clamp(i32::MIN as i64, i32::MAX as i64);

        gst::debug!(
            CAT,
            imp = self,
            "overlay image dimensions: {} x {}, alpha={}",
            overlay_meta.width(),
            overlay_meta.height(),
            settings.alpha
        );

        gst::debug!(
            CAT,
            imp = self,
            "properties: x,y: {},{} ({}%,{}%) coef ({}%,{}%) - WxH: {}x{}",
            settings.offset_x,
            settings.offset_y,
            settings.relative_x * 100.0,
            settings.relative_y * 100.0,
            settings.coef_x * 100.0,
            settings.coef_y * 100.0,
            settings.overlay_width,
            settings.overlay_height
        );
        gst::debug!(
            CAT,
            imp = self,
            "overlay rendered: {width} x {height} @ {x},{y} (onto {video_width} x {video_height})"
        );

        let mut rect = gst_video::VideoOverlayRectangle::new_raw(
            overlay_pixels,
            x as i32,
            y as i32,
            width as u32,
            height as u32,
            gst_video::VideoOverlayFormatFlags::empty(),
        );
        if settings.alpha != 1.0 {
            rect.make_mut().set_global_alpha(settings.alpha);
        }

        drop(settings);

        state.composition = Some(rect.into());
        state.update_composition = false;

        gst::debug!(CAT, imp = self, "Composition updated");
    }

    fn load_image(&self, state: &mut State) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        if state.location.as_ref() == settings.location.as_ref() {
            gst::trace!(CAT, imp = self, "No need to update");
            return Ok(());
        }
        state.location = None;
        state.image = None;

        let Some(location) = settings.location.clone() else {
            gst::debug!(CAT, imp = self, "No location set");
            return Ok(());
        };

        let mut reader = ImageReader::open(&location).map_err(|v| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Could not load overlay image: {}", v]
            )
        })?;

        if settings.max_alloc != 0 {
            let mut limits = Limits::default();
            limits.max_alloc = Some(settings.max_alloc);
            reader.limits(limits);
        }

        drop(settings);

        let mut argb_image = match reader.decode().map_err(|v| {
            gst::error_msg!(
                gst::StreamError::Decode,
                ["Could not decode overlay image container: {}", v]
            )
        })? {
            DynamicImage::ImageRgba8(v) => v,
            v => v.to_rgba8(),
        };
        // image-rs always outputs image in RGBA channel order (individual
        // channels respecting the native endianness).
        // FIXME: use the upstream into_raw_bgr function for converting to
        // BGRA, when it is released
        // https://github.com/image-rs/image/commit/38456b67a943f39dfad7ab35589afe7a86ea4643
        {
            for pix in argb_image.as_chunks_mut::<4>().0 {
                pix.swap(0, 2);
            }
        }

        // FIXME upstream: The underlying blending seems to simply assume 8-bit
        // sRGB, BGRA pixel format. The VideoFormatInfo could be created and
        // sent here (once color correctness and HDR are supported).
        let (width, height) = argb_image.dimensions();

        let stride = [i32::try_from(argb_image.as_flat_samples().layout.height_stride).unwrap()];
        let mut buffer = Wrapper::Image(argb_image.into()).into_gst_buffer();

        gst_video::VideoMeta::add_full(
            buffer.get_mut().unwrap(),
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Bgra,
            width,
            height,
            &[0],
            &stride,
        )
        .unwrap();

        state.location = Some(location);
        state.image = Some(buffer);
        state.update_composition = true;

        gst::info!(CAT, imp = self, "Updated pixbuf, {width} x {height}");

        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ImageRsOverlay {
    const NAME: &'static str = "GstImageRsOverlay";
    type Type = super::Overlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for ImageRsOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("location")
                    .blurb("Location of image file to overlay")
                    .default_value(None)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecInt::builder("offset-x")
                    .nick("X Offset")
                    .blurb("For positive value, horizontal offset of overlay image in pixels from left of video image. For negative value, horizontal offset of overlay image in pixels from right of video image")
                    .default_value(0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecInt::builder("offset-y")
                    .nick("Y Offset")
                    .blurb("For positive value, vertical offset of overlay image in pixels from top of video image. For negative value, vertical offset of overlay image in pixels from bottom of video image")
                    .default_value(0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("relative-x")
                    .nick("Relative X Offset")
                    .blurb("Horizontal offset of overlay image in fractions of video image width, from top-left corner of video image (in relative positioning)")
                    .minimum(-1.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("relative-y")
                    .nick("Relative Y Offset")
                    .blurb("Vertical offset of overlay image in fractions of video image width, from top-left corner of video image (in relative positioning)")
                    .minimum(-1.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("overlay-width")
                    .nick("Overlay Width")
                    .blurb("Width of overlay image in pixels (0 = same as overlay image)")
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("overlay-height")
                    .nick("Overlay Height")
                    .blurb("Height of overlay image in pixels (0 = same as overlay image")
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("alpha")
                    .nick("Alpha")
                    .blurb("Global alpha of overlay image")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(1.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder("max-alloc-bytes")
                    .nick("Memory allocation limits")
                    .blurb("Max. amount of data to allocate for decoding (bytes, 0=disable)")
                    .default_value(128 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("positioning-mode", Settings::default().positioning_mode)
                    .nick("Positioning mode")
                    .blurb("Positioning mode of offset-x and offset-y properties")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("coef-x")
                    .nick("Relative X Offset")
                    .blurb("Horizontal offset of overlay image in fractions of video image width, from top-left corner of video image (absolute positioning)")
                    .minimum(-1.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("coef-y")
                    .nick("Relative Y Offset")
                    .blurb("Vertical offset of overlay image in fractions of video image height, from top-left corner of video image (absolute positioning)")
                    .minimum(-1.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "location" => settings.location.to_value(),
            "offset-x" => settings.offset_x.to_value(),
            "offset-y" => settings.offset_y.to_value(),
            "relative-x" => settings.relative_x.to_value(),
            "relative-y" => settings.relative_y.to_value(),
            "coef-x" => settings.coef_x.to_value(),
            "coef-y" => settings.coef_y.to_value(),
            "positioning-mode" => settings.positioning_mode.to_value(),
            "overlay-width" => settings.overlay_width.to_value(),
            "overlay-height" => settings.overlay_height.to_value(),
            "alpha" => settings.alpha.to_value(),
            "max-alloc-bytes" => settings.max_alloc.to_value(),
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut state = self.state.lock().unwrap();
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "location" => {
                let value = value.get().expect("type checked upstream");
                settings.location = Some(value);
                state.update_composition = true;
            }
            "offset-x" => {
                let value = value.get().expect("type checked upstream");
                settings.offset_x = value;
                state.update_composition = true;
            }
            "offset-y" => {
                let value = value.get().expect("type checked upstream");
                settings.offset_y = value;
                state.update_composition = true;
            }
            "relative-x" => {
                let value = value.get().expect("type checked upstream");
                settings.relative_x = value;
                state.update_composition = true;
            }
            "relative-y" => {
                let value = value.get().expect("type checked upstream");
                settings.relative_y = value;
                state.update_composition = true;
            }
            "coef-x" => {
                let value = value.get().expect("type checked upstream");
                settings.coef_x = value;
                state.update_composition = true;
            }
            "coef-y" => {
                let value = value.get().expect("type checked upstream");
                settings.coef_y = value;
                state.update_composition = true;
            }
            "positioning-mode" => {
                let value = value.get().expect("type checked upstream");
                settings.positioning_mode = value;
                state.update_composition = true;
            }
            "overlay-width" => {
                let value = value.get().expect("type checked upstream");
                settings.overlay_width = value;
                state.update_composition = true;
            }
            "overlay-height" => {
                let value = value.get().expect("type checked upstream");
                settings.overlay_height = value;
                state.update_composition = true;
            }
            "alpha" => {
                let value = value.get().expect("type checked upstream");
                settings.alpha = value;
                state.update_composition = true;
            }
            "max-alloc-bytes" => {
                let value = value.get::<u64>().expect("type checked upstream");
                settings.max_alloc = value;
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ImageRsOverlay {}

impl ElementImpl for ImageRsOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs overlay",
                "Video/Overlay",
                "Renders images decoded with image-rs over raw video frames",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let mut caps = gst::Caps::new_empty();
            {
                let c = caps.get_mut().unwrap();
                c.append(gst_video::VideoCapsBuilder::new().any_features().build());
                c.append(gst_video::VideoCapsBuilder::new().build());
            }
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

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for ImageRsOverlay {
    /// GstBaseTransform expects, for in-place transforms like
    /// the one required by this plugin, that a transform_frame_ip
    /// function is provided AND that a transform function is
    /// NOT provided.
    ///
    /// See gst_base_transform_init and default_generate_output
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;

    // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/merge_requests/3024#note_3522645
    // for the properties below
    /// Disable verbatim forwarding when caps do not change
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    /// transform_ip's buffer on passthrough is read-only, so
    /// it breaks enabling overlay at runtime e.g. with tests
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let is_location_empty = self.settings.lock().unwrap().location.is_none();

        let old_is_passthrough = self.obj().is_passthrough();

        if !is_location_empty {
            self.obj().set_passthrough(false);
        } else {
            gst::info!(CAT, imp = self, "no image location set, doing nothing");
            self.obj().set_passthrough(true);
        }

        if old_is_passthrough != self.obj().is_passthrough() {
            gst::debug!(
                CAT,
                imp = self,
                "Passthrough: {}",
                self.obj().is_passthrough()
            );
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.composition = None;
        state.image = None;
        gst::debug!(CAT, imp = self, "Image removed");
        Ok(())
    }

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let stream_time = self
            .obj()
            .segment()
            .downcast::<gst::ClockTime>()
            .ok()
            .and_then(|v| v.to_stream_time(inbuf.pts()));
        if let Some(stream_time) = stream_time
            && let Err(e) = self.obj().sync_values(stream_time)
        {
            gst::log!(CAT, imp = self, "{e}");
        }

        // now properties have been sync'ed; maybe need to update composition
        {
            let mut state = self.state.lock().unwrap();
            if let Err(err) = self.load_image(&mut state) {
                self.post_error_message(err);
                return;
            }
            if state.update_composition {
                self.update_composition(&mut state);
                // determine passthrough mode so the buffer is writable if needed
                // when passed into _transform_ip
                self.obj().set_passthrough(state.composition.is_none());
            }
        };
        gst::debug!(
            CAT,
            imp = self,
            "Passthrough: {}",
            self.obj().is_passthrough()
        );
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        gst::debug!(
            CAT,
            imp = self,
            "transforming caps {} in direction {:?} with filter {:?}",
            caps,
            direction,
            filter
        );
        let tmp = if direction == gst::PadDirection::Sink {
            let mut tmp: gst::Caps =
                caps.iter_with_features()
                    .fold(gst::Caps::new_empty(), |mut tmp, (s, f)| {
                        let s = s.to_owned();
                        let mut f = f.to_owned();

                        if !f.is_any()
                            && !f.contains(
                                gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION,
                            )
                        {
                            f.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
                        }

                        let c = gst::Caps::builder_full()
                            .structure_with_features(s, f)
                            .build();
                        tmp.get_mut().unwrap().append(c);
                        tmp
                    });
            let mut caps = caps.copy();
            caps.get_mut().unwrap().filter_map_in_place(|f, _| {
                if f.is_any()
                    || f.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
                    || f.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION)
                {
                    gst::CapsFilterMapAction::Keep
                } else {
                    gst::CapsFilterMapAction::Remove
                }
            });
            tmp.merge(caps);
            tmp
        } else {
            let tmp = caps
                .iter_with_features()
                .fold(gst::Caps::new_empty(), |mut tmp, (s, f)| {
                    let s = s.to_owned();
                    let mut f = f.to_owned();

                    f.remove(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);

                    let c = gst::Caps::builder_full()
                        .structure_with_features(s, f)
                        .build();
                    tmp.get_mut().unwrap().append(c);
                    tmp
                });
            let mut caps = caps.copy();
            caps.get_mut().unwrap().filter_map_in_place(|f, _| {
                if f.is_any()
                    || f.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
                    || f.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION)
                {
                    gst::CapsFilterMapAction::Keep
                } else {
                    gst::CapsFilterMapAction::Remove
                }
            });
            caps.merge(tmp);
            caps
        };

        gst::debug!(CAT, imp = self, "filter {filter:?}, expanded caps {tmp}");

        let res = if let Some(filter) = filter {
            filter.intersect_with_mode(&tmp, gst::CapsIntersectMode::First)
        } else {
            tmp
        };

        gst::debug!(CAT, imp = self, "returning caps: {res}");

        Some(res)
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let res = self.parent_set_caps(incaps, outcaps);
        if res.is_ok() {
            let mut state = self.state.lock().unwrap();
            state.allow_attaching = outcaps.features(0).is_none_or(|features| {
                features.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION)
            });
        }
        res
    }
}

impl VideoFilterImpl for ImageRsOverlay {
    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        use gst_video::video_frame::IsVideoFrame;

        let state = self.state.lock().unwrap();
        if let Some(v) = &state.composition {
            if state.allow_attaching {
                // FIXME upstream: frame.buffer() returns a IMMUTABLE
                // reference, not respecting the T parameter of VideoFrameRef.
                // This makes it impossible to use from VideoFilterImpl.
                // https://gitlab.freedesktop.org/gstreamer/gstreamer-rs/-/issues/549
                let buffer = unsafe { gst::BufferRef::from_mut_ptr(frame.as_raw().buffer) };
                gst_video::VideoOverlayCompositionMeta::add(buffer, v);
            } else {
                v.blend(frame).map_err(|v| {
                    gst::error!(CAT, imp = self, "Blending failed: {v}");
                    gst::FlowError::Error
                })?
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }
}
