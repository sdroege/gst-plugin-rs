// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_error, gst_info, gst_log, gst_trace, gst_warning};
use gst_video::prelude::*;

use once_cell::sync::Lazy;

use std::sync::Mutex;

use pango::prelude::*;

use crate::caption_frame::{CaptionFrame, Status};
use crate::ccutils::extract_cdp;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cea608overlay",
        gst::DebugColorFlags::empty(),
        Some("CEA 608 overlay element"),
    )
});

const DEFAULT_FIELD: i32 = -1;
const DEFAULT_BLACK_BACKGROUND: bool = false;

#[derive(Debug)]
struct Settings {
    field: i32,
    black_background: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            field: DEFAULT_FIELD,
            black_background: DEFAULT_BLACK_BACKGROUND,
        }
    }
}

struct State {
    video_info: Option<gst_video::VideoInfo>,
    layout: Option<pango::Layout>,
    caption_frame: CaptionFrame,
    composition: Option<gst_video::VideoOverlayComposition>,
    left_alignment: i32,
    attach: bool,
    selected_field: Option<u8>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            video_info: None,
            layout: None,
            caption_frame: CaptionFrame::default(),
            composition: None,
            left_alignment: 0,
            attach: false,
            selected_field: None,
        }
    }
}

unsafe impl Send for State {}

pub struct Cea608Overlay {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Cea608Overlay {
    // FIXME: we want to render the text in the largest 32 x 15 characters
    // that will fit the viewport. This is a truly terrible way to determine
    // the appropriate font size, but we only need to run that on resolution
    // changes, and the API that would allow us to precisely control the
    // line height has not yet been exposed by the bindings:
    //
    // https://blogs.gnome.org/mclasen/2019/07/27/more-text-rendering-updates/
    //
    // TODO: switch to the API presented in this post once it's been exposed
    fn recalculate_layout(
        &self,
        element: &super::Cea608Overlay,
        state: &mut State,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let video_info = state.video_info.as_ref().unwrap();
        let fontmap = match pangocairo::FontMap::new() {
            Some(fontmap) => Ok(fontmap),
            None => {
                gst::element_error!(
                    element,
                    gst::LibraryError::Failed,
                    ["Failed to create pangocairo font map"]
                );
                Err(gst::FlowError::Error)
            }
        }?;
        let context = match fontmap.create_context() {
            Some(context) => Ok(context),
            None => {
                gst::element_error!(
                    element,
                    gst::LibraryError::Failed,
                    ["Failed to create font map context"]
                );
                Err(gst::FlowError::Error)
            }
        }?;
        context.set_language(&pango::Language::from_string("en_US"));
        context.set_base_dir(pango::Direction::Ltr);
        let layout = pango::Layout::new(&context);
        layout.set_alignment(pango::Alignment::Left);
        let mut font_desc = pango::FontDescription::from_string(&"monospace");

        let mut font_size = 1;
        let mut left_alignment = 0;
        loop {
            font_desc.set_size(font_size * pango::SCALE);
            layout.set_font_description(Some(&font_desc));
            layout.set_text(
                &"12345678901234567890123456789012\n2\n3\n4\n5\n6\n7\n8\n9\n0\n1\n2\n3\n4\n5",
            );
            let (_ink_rect, logical_rect) = layout.extents();
            if logical_rect.width > video_info.width() as i32 * pango::SCALE
                || logical_rect.height > video_info.height() as i32 * pango::SCALE
            {
                font_desc.set_size((font_size - 1) * pango::SCALE);
                layout.set_font_description(Some(&font_desc));
                break;
            }
            left_alignment = (video_info.width() as i32 - logical_rect.width / pango::SCALE) / 2;
            font_size += 1;
        }

        if self.settings.lock().unwrap().black_background {
            let attrs = pango::AttrList::new();
            let attr = pango::Attribute::new_background(0, 0, 0);
            attrs.insert(attr);
            layout.set_attributes(Some(&attrs));
        }

        state.left_alignment = left_alignment;
        state.layout = Some(layout);

        Ok(gst::FlowSuccess::Ok)
    }

    fn overlay_text(&self, element: &super::Cea608Overlay, text: &str, state: &mut State) {
        let video_info = state.video_info.as_ref().unwrap();
        let layout = state.layout.as_ref().unwrap();
        layout.set_text(text);
        let (_ink_rect, logical_rect) = layout.extents();
        let height = logical_rect.height / pango::SCALE;
        let width = logical_rect.width / pango::SCALE;

        // No text actually needs rendering
        if width == 0 || height == 0 {
            state.composition = None;
            return;
        }

        let render_buffer = || -> Option<gst::Buffer> {
            let mut buffer = gst::Buffer::with_size((width * height) as usize * 4).ok()?;

            gst_video::VideoMeta::add(
                buffer.get_mut().unwrap(),
                gst_video::VideoFrameFlags::empty(),
                #[cfg(target_endian = "little")]
                gst_video::VideoFormat::Bgra,
                #[cfg(target_endian = "big")]
                gst_video::VideoFormat::Argb,
                width as u32,
                height as u32,
            )
            .ok()?;
            let buffer = buffer.into_mapped_buffer_writable().unwrap();

            // Pass ownership of the buffer to the cairo surface but keep around
            // a raw pointer so we can later retrieve it again when the surface
            // is done
            let buffer_ptr = unsafe { buffer.buffer().as_ptr() };
            let surface = cairo::ImageSurface::create_for_data(
                buffer,
                cairo::Format::ARgb32,
                width,
                height,
                width * 4,
            )
            .ok()?;

            let cr = cairo::Context::new(&surface).ok()?;

            // Clear background
            cr.set_operator(cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            cr.paint().ok()?;

            // Render text outline
            cr.save().ok()?;
            cr.set_operator(cairo::Operator::Over);

            cr.set_source_rgba(0.0, 0.0, 0.0, 1.0);

            pangocairo::functions::layout_path(&cr, &layout);
            cr.stroke().ok()?;
            cr.restore().ok()?;

            // Render text
            cr.save().ok()?;
            cr.set_source_rgba(255.0, 255.0, 255.0, 1.0);

            pangocairo::functions::show_layout(&cr, &layout);

            cr.restore().ok()?;
            drop(cr);

            // Safety: The surface still owns a mutable reference to the buffer but our reference
            // to the surface here is the last one. After dropping the surface the buffer would be
            // freed, so we keep an additional strong reference here before dropping the surface,
            // which is then returned. As such it's guaranteed that nothing is using the buffer
            // anymore mutably.
            unsafe {
                assert_eq!(
                    cairo::ffi::cairo_surface_get_reference_count(surface.to_raw_none()),
                    1
                );
                let buffer = glib::translate::from_glib_none(buffer_ptr);
                drop(surface);
                buffer
            }
        };

        let buffer = match render_buffer() {
            Some(buffer) => buffer,
            None => {
                gst_error!(CAT, obj: element, "Failed to render buffer");
                state.composition = None;
                return;
            }
        };

        let rect = gst_video::VideoOverlayRectangle::new_raw(
            &buffer,
            state.left_alignment,
            (video_info.height() as i32 - height) / 2,
            width as u32,
            height as u32,
            gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
        );

        state.composition = match gst_video::VideoOverlayComposition::new(Some(&rect)) {
            Ok(composition) => Some(composition),
            Err(_) => None,
        };
    }

    fn negotiate(
        &self,
        element: &super::Cea608Overlay,
        state: &mut State,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let video_info = match state.video_info.as_ref() {
            Some(video_info) => Ok(video_info),
            None => {
                gst::element_error!(
                    element,
                    gst::CoreError::Negotiation,
                    ["Element hasn't received valid video caps at negotiation time"]
                );
                Err(gst::FlowError::NotNegotiated)
            }
        }?;

        let mut caps = video_info.to_caps().unwrap();
        let mut downstream_accepts_meta = false;

        let upstream_has_meta = caps
            .features(0)
            .map(|f| f.contains(&gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION))
            .unwrap_or(false);

        if !upstream_has_meta {
            let mut caps_clone = caps.clone();
            let overlay_caps = caps_clone.make_mut();

            if let Some(features) = overlay_caps.features_mut(0) {
                features.add(&gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
                let peercaps = self.srcpad.peer_query_caps(Some(&caps_clone));
                downstream_accepts_meta = !peercaps.is_empty();
                if downstream_accepts_meta {
                    caps = caps_clone;
                }
            }
        }

        state.attach = upstream_has_meta || downstream_accepts_meta;

        let _ = state.layout.take();

        if !self.srcpad.push_event(gst::event::Caps::new(&caps)) {
            Err(gst::FlowError::NotNegotiated)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn decode_cc_data(
        &self,
        pad: &gst::Pad,
        element: &super::Cea608Overlay,
        state: &mut State,
        data: &[u8],
    ) {
        if data.len() % 3 != 0 {
            gst_warning!(CAT, "cc_data length is not a multiple of 3, truncating");
        }

        for triple in data.chunks_exact(3) {
            let cc_valid = (triple[0] & 0x04) == 0x04;
            let cc_type = triple[0] & 0x03;

            if cc_valid {
                if cc_type == 0x00 || cc_type == 0x01 {
                    if state.selected_field.is_none() {
                        state.selected_field = Some(cc_type);
                        gst_info!(
                            CAT,
                            obj: element,
                            "Selected field {} automatically",
                            cc_type
                        );
                    }

                    if Some(cc_type) == state.selected_field {
                        match state
                            .caption_frame
                            .decode((triple[1] as u16) << 8 | triple[2] as u16, 0.0)
                        {
                            Ok(Status::Ready) => {
                                let text = match state.caption_frame.to_text(true) {
                                    Ok(text) => text,
                                    Err(_) => {
                                        gst_error!(
                                            CAT,
                                            obj: pad,
                                            "Failed to convert caption frame to text"
                                        );
                                        continue;
                                    }
                                };

                                self.overlay_text(element, &text, state);
                            }
                            _ => (),
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }

    fn decode_s334_1a(
        &self,
        pad: &gst::Pad,
        element: &super::Cea608Overlay,
        state: &mut State,
        data: &[u8],
    ) {
        if data.len() % 3 != 0 {
            gst_warning!(CAT, "cc_data length is not a multiple of 3, truncating");
        }

        for triple in data.chunks_exact(3) {
            let cc_type = triple[0] & 0x01;
            if state.selected_field.is_none() {
                state.selected_field = Some(cc_type);
                gst_info!(
                    CAT,
                    obj: element,
                    "Selected field {} automatically",
                    cc_type
                );
            }

            if Some(cc_type) == state.selected_field {
                match state
                    .caption_frame
                    .decode((triple[1] as u16) << 8 | triple[2] as u16, 0.0)
                {
                    Ok(Status::Ready) => {
                        let text = match state.caption_frame.to_text(true) {
                            Ok(text) => text,
                            Err(_) => {
                                gst_error!(
                                    CAT,
                                    obj: pad,
                                    "Failed to convert caption frame to text"
                                );
                                continue;
                            }
                        };

                        self.overlay_text(element, &text, state);
                    }
                    _ => (),
                }
            }
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::Cea608Overlay,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();

        if self.srcpad.check_reconfigure() {
            self.negotiate(element, &mut state)?;
        }

        if state.layout.is_none() {
            self.recalculate_layout(element, &mut state)?;
        }

        for meta in buffer.iter_meta::<gst_video::VideoCaptionMeta>() {
            if meta.caption_type() == gst_video::VideoCaptionType::Cea708Cdp {
                match extract_cdp(meta.data()) {
                    Ok(data) => {
                        self.decode_cc_data(pad, element, &mut state, data);
                    }
                    Err(e) => {
                        gst_warning!(CAT, "{}", &e.to_string());
                        gst::element_warning!(element, gst::StreamError::Decode, [&e.to_string()]);
                    }
                }
            } else if meta.caption_type() == gst_video::VideoCaptionType::Cea708Raw {
                self.decode_cc_data(pad, element, &mut state, meta.data());
            } else if meta.caption_type() == gst_video::VideoCaptionType::Cea608S3341a {
                self.decode_s334_1a(pad, element, &mut state, meta.data());
            } else if meta.caption_type() == gst_video::VideoCaptionType::Cea608Raw {
                let data = meta.data();
                assert!(data.len() % 2 == 0);
                for i in 0..data.len() / 2 {
                    match state
                        .caption_frame
                        .decode((data[i * 2] as u16) << 8 | data[i * 2 + 1] as u16, 0.0)
                    {
                        Ok(Status::Ready) => {
                            let text = match state.caption_frame.to_text(true) {
                                Ok(text) => text,
                                Err(_) => {
                                    gst_error!(
                                        CAT,
                                        obj: pad,
                                        "Failed to convert caption frame to text"
                                    );
                                    continue;
                                }
                            };

                            self.overlay_text(element, &text, &mut state);
                        }
                        _ => (),
                    }
                }
            }
        }

        if let Some(composition) = &state.composition {
            let buffer = buffer.make_mut();
            if state.attach {
                gst_video::VideoOverlayCompositionMeta::add(buffer, &composition);
            } else {
                let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(
                    buffer,
                    state.video_info.as_ref().unwrap(),
                )
                .unwrap();
                match composition.blend(&mut frame) {
                    Err(_) => {
                        gst_error!(CAT, obj: pad, "Failed to blend composition");
                    }
                    _ => (),
                }
            }
        }
        drop(state);

        self.srcpad.push(buffer)
    }

    fn sink_event(
        &self,
        pad: &gst::Pad,
        element: &super::Cea608Overlay,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(c) => {
                let mut state = self.state.lock().unwrap();
                state.video_info = gst_video::VideoInfo::from_caps(c.caps()).ok();
                self.srcpad.check_reconfigure();
                match self.negotiate(element, &mut state) {
                    Ok(_) => true,
                    Err(_) => {
                        self.srcpad.mark_reconfigure();
                        true
                    }
                }
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                state.caption_frame = CaptionFrame::default();
                state.composition = None;
                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Cea608Overlay {
    const NAME: &'static str = "RsCea608Overlay";
    type Type = super::Cea608Overlay;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                Cea608Overlay::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |overlay, element| overlay.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Cea608Overlay::catch_panic_pad_function(
                    parent,
                    || false,
                    |overlay, element| overlay.sink_event(pad, element, event),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .flags(gst::PadFlags::PROXY_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for Cea608Overlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_int(
                    "field",
                    "Field",
                    "The field to render the caption for when available, (-1=automatic)",
                    -1,
                    1,
                    DEFAULT_FIELD,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_boolean(
                    "black-background",
                    "Black background",
                    "Whether a black background should be drawn behind text",
                    DEFAULT_BLACK_BACKGROUND,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "field" => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();

                settings.field = value.get().expect("type checked upstream");
                state.selected_field = match settings.field {
                    -1 => None,
                    val => Some(val as u8),
                };
            }
            "black-background" => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();

                settings.black_background = value.get().expect("type checked upstream");
                let _ = state.layout.take();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "field" => {
                let settings = self.settings.lock().unwrap();
                settings.field.to_value()
            }
            "black-background" => {
                let settings = self.settings.lock().unwrap();
                settings.black_background.to_value()
            }
            _ => unimplemented!(),
        }
    }
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for Cea608Overlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Cea 608 overlay",
                "Video/Overlay/Subtitle",
                "Renders CEA 608 closed caption meta over raw video frames",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst_video::VideoFormat::iter_raw()
                .into_video_caps()
                .unwrap()
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

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady => {
                // Reset the whole state
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                let settings = self.settings.lock().unwrap();
                state.selected_field = match settings.field {
                    -1 => None,
                    val => Some(val as u8),
                };
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}
