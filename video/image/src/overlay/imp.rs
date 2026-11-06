// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on onvifmetadataoverlay, hsvdetectorm, gdkpixbufoverlay and overlaycompositor

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use image::{DynamicImage, ImageReader, Limits};

use std::sync::{LazyLock, Mutex};

use crate::buffer::ImageWrapper;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "imagersoverlay",
        gst::DebugColorFlags::empty(),
        Some("image-rs overlay"),
    )
});

#[derive(Default)]
struct State {
    segment: gst::FormattedSegment<gst::ClockTime>,
    video_info: Option<gst_video::VideoInfo>,
    upstream_caps: Option<gst::Caps>,
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

#[derive(Clone)]
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

pub struct ImageRsOverlay {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl ImageRsOverlay {
    fn query_caps(&self, pad: &gst::Pad, filter: Option<&gst::CapsRef>) -> gst::Caps {
        let caps = if pad.direction() == gst::PadDirection::Sink {
            self.srcpad.peer_query_caps(Some(&pad.pad_template_caps()))
        } else {
            self.sinkpad.peer_query_caps(Some(&pad.pad_template_caps()))
        };

        let mut result = caps.clone();
        for (structure, features) in caps.iter_with_features() {
            if features.is_any() {
                continue;
            }

            let mut features = features.to_owned();
            if pad.direction() == gst::PadDirection::Sink {
                features.remove(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
            } else {
                features.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
            }

            result
                .make_mut()
                .append_structure_full(structure.to_owned(), Some(features));
        }

        if let Some(filter) = filter {
            filter.intersect_with_mode(&result, gst::CapsIntersectMode::First)
        } else {
            result
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj = pad, "Handling buffer {buffer:?}");

        if self.srcpad.check_reconfigure() {
            self.negotiate()?;
        }

        let stream_time = self
            .state
            .lock()
            .unwrap()
            .segment
            .to_stream_time(buffer.pts());

        if let Some(stream_time) = stream_time
            && let Err(e) = self.obj().sync_values(stream_time)
        {
            gst::log!(CAT, imp = self, "{e}");
        }

        let mut state = self.state.lock().unwrap();

        if self.settings.lock().unwrap().location.is_some() {
            // now properties have been sync'ed; maybe need to update composition
            if state.update_composition && self.load_image(&mut state).is_ok() {
                self.update_composition(&mut state);
            }

            if let Some(composition) = &state.composition {
                let buffer = buffer.make_mut();
                if state.allow_attaching {
                    gst_video::VideoOverlayCompositionMeta::add(buffer, composition);
                } else {
                    let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(
                        buffer,
                        state.video_info.as_ref().unwrap(),
                    )
                    .unwrap();

                    if composition.blend(&mut frame).is_err() {
                        gst::error!(CAT, obj = pad, "Failed to blend composition");
                    }
                }
            }
        }

        drop(state);

        self.srcpad.push(buffer)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {event:?}");
        match event.view() {
            EventView::Caps(c) => match gst_video::VideoInfo::from_caps(c.caps()) {
                Ok(video_info) => {
                    {
                        let mut state = self.state.lock().unwrap();
                        state.upstream_caps = Some(c.caps_owned());
                        state.video_info = Some(video_info);
                        state.update_composition = true;
                        self.srcpad.check_reconfigure();
                    }
                    match self.negotiate() {
                        Ok(_) => true,
                        Err(_) => {
                            self.srcpad.mark_reconfigure();
                            true
                        }
                    }
                }
                Err(_) => {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Element hasn't received valid video caps at negotiation time"
                    );
                    false
                }
            },
            EventView::Segment(s) => {
                match s.segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Format,
                            ["Only Time segments supported, got {:?}", segment.format(),]
                        );
                        return false;
                    }
                    Ok(segment) => {
                        self.state.lock().unwrap().segment = segment;
                    }
                };
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Caps(q) => {
                q.set_result(&self.query_caps(pad, q.filter()));
                true
            }
            QueryViewMut::Allocation(q) => {
                let result = self.srcpad.peer_query(q);
                if result
                    && q.find_allocation_meta::<gst_video::VideoOverlayCompositionMeta>()
                        .is_none()
                {
                    q.add_allocation_meta::<gst_video::VideoOverlayCompositionMeta>(None);
                }
                result
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn negotiate(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut caps = match self.state.lock().unwrap().upstream_caps.clone() {
            Some(v) => v,
            None => return Err(gst::FlowError::NotNegotiated),
        };

        let mut downstream_accepts_meta = false;

        let upstream_has_meta = caps
            .features(0)
            .map(|f| f.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION))
            .unwrap_or(false);

        if !upstream_has_meta {
            let mut caps_clone = caps.clone();
            let overlay_caps = caps_clone.make_mut();

            if let Some(features) = overlay_caps.features_mut(0) {
                features.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
                let peercaps = self.srcpad.peer_query_caps(Some(&caps_clone));
                downstream_accepts_meta = !peercaps.is_empty();
                if downstream_accepts_meta {
                    caps = caps_clone;
                }
            }
        }

        gst::debug!(
            CAT,
            imp = self,
            "upstream has meta: {}, downstream accepts meta: {}",
            upstream_has_meta,
            downstream_accepts_meta
        );

        if !self.srcpad.push_event(gst::event::Caps::new(&caps)) {
            gst::info!(CAT, imp = self, "Failed to push caps event");
        }

        if upstream_has_meta || downstream_accepts_meta {
            let mut query = gst::query::Allocation::new(Some(&caps), false);

            if !self.srcpad.peer_query(&mut query)
                && self.srcpad.pad_flags().contains(gst::PadFlags::FLUSHING)
            {
                return Err(gst::FlowError::NotNegotiated);
            }

            let allow_attaching = query
                .find_allocation_meta::<gst_video::VideoOverlayCompositionMeta>()
                .is_some();

            gst::debug!(CAT, imp = self, "attach meta: {}", allow_attaching);

            self.state.lock().unwrap().allow_attaching = allow_attaching;
        } else {
            self.state.lock().unwrap().allow_attaching = false;
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn update_composition(&self, state: &mut State) {
        if !state.update_composition {
            return;
        }

        let in_info = state.video_info.as_ref().unwrap();
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

    fn load_image(&self, state: &mut State) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap();

        if state.location.as_ref() == settings.location.as_ref() {
            gst::trace!(CAT, imp = self, "No need to update");
            return Ok(gst::FlowSuccess::Ok);
        }
        state.location = None;
        state.image = None;

        let Some(location) = settings.location.clone() else {
            gst::debug!(CAT, imp = self, "No location set");
            return Ok(gst::FlowSuccess::Ok);
        };

        let mut reader = ImageReader::open(&location).map_err(|v| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::OpenRead,
                ["Could not load overlay image: {v}"]
            );
            gst::FlowError::Error
        })?;

        if settings.max_alloc != 0 {
            let mut limits = Limits::default();
            limits.max_alloc = Some(settings.max_alloc);
            reader.limits(limits);
        }

        drop(settings);

        let mut argb_image = match reader.decode().map_err(|v| {
            gst::element_imp_error!(
                self,
                gst::StreamError::Decode,
                ["Could not decode overlay image container: {v}"]
            );
            gst::FlowError::Error
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
        let mut buffer = gst::Buffer::from_slice(ImageWrapper(argb_image.into()));

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

        Ok(gst::FlowSuccess::Ok)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ImageRsOverlay {
    const NAME: &'static str = "GstImageRsOverlay";
    type Type = super::Overlay;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                ImageRsOverlay::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |overlay| overlay.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                ImageRsOverlay::catch_panic_pad_function(
                    parent,
                    || false,
                    |overlay| overlay.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, event| {
                ImageRsOverlay::catch_panic_pad_function(
                    parent,
                    || false,
                    |overlay| overlay.query(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .query_function(|pad, parent, event| {
                ImageRsOverlay::catch_panic_pad_function(
                    parent,
                    || false,
                    |overlay| overlay.query(pad, event),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
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
                let value: String = value.get().expect("type checked upstream");
                if value.is_empty() {
                    settings.location = None;
                } else {
                    settings.location = Some(value);
                }
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

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
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

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                state.update_composition = true;
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}
