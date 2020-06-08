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

// Example command-line:
//
// gst-launch-1.0 cccombiner name=ccc ! cea608overlay ! autovideosink \
//   videotestsrc ! video/x-raw, width=1280, height=720 ! queue ! ccc.sink \
//   filesrc location=input.srt ! subparse ! tttocea608 ! queue ! ccc.caption

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::prelude::*;

use std::sync::Mutex;

use pango::prelude::*;

use crate::caption_frame::{CaptionFrame, Status};

lazy_static! {
    static ref CAT: gst::DebugCategory = {
        gst::DebugCategory::new(
            "cea608overlay",
            gst::DebugColorFlags::empty(),
            Some("CEA 608 overlay element"),
        )
    };
}

struct State {
    video_info: Option<gst_video::VideoInfo>,
    layout: Option<pango::Layout>,
    caption_frame: CaptionFrame,
    composition: Option<gst_video::VideoOverlayComposition>,
    left_alignment: i32,
    attach: bool,
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
        }
    }
}

unsafe impl Send for State {}

struct Cea608Overlay {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl Cea608Overlay {
    fn set_pad_functions(sinkpad: &gst::Pad, _srcpad: &gst::Pad) {
        sinkpad.set_chain_function(|pad, parent, buffer| {
            Cea608Overlay::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |overlay, element| overlay.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            Cea608Overlay::catch_panic_pad_function(
                parent,
                || false,
                |overlay, element| overlay.sink_event(pad, element, event),
            )
        });
    }

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
        element: &gst::Element,
        state: &mut State,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let video_info = state.video_info.as_ref().unwrap();
        let fontmap = match pangocairo::FontMap::new() {
            Some(fontmap) => Ok(fontmap),
            None => {
                gst_element_error!(
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
                gst_element_error!(
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
            let (_ink_rect, logical_rect) = layout.get_extents();
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

        state.left_alignment = left_alignment;
        state.layout = Some(layout);

        Ok(gst::FlowSuccess::Ok)
    }

    fn overlay_text(&self, text: &str, state: &mut State) {
        let video_info = state.video_info.as_ref().unwrap();
        let layout = state.layout.as_ref().unwrap();
        layout.set_text(text);
        let (_ink_rect, logical_rect) = layout.get_extents();
        let height = logical_rect.height / pango::SCALE;
        let width = logical_rect.width / pango::SCALE;

        // No text actually needs rendering
        if width == 0 || height == 0 {
            state.composition = None;
            return;
        }

        let mut buffer = gst::Buffer::with_size((width * height) as usize * 4).unwrap();

        gst_video::VideoMeta::add(
            buffer.get_mut().unwrap(),
            gst_video::VideoFrameFlags::NONE,
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Bgra,
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Argb,
            width as u32,
            height as u32,
        )
        .unwrap();
        let buffer = buffer.into_mapped_buffer_writable().unwrap();
        let buffer = {
            let buffer_ptr = unsafe { buffer.get_buffer().as_ptr() };
            let surface = cairo::ImageSurface::create_for_data(
                buffer,
                cairo::Format::ARgb32,
                width as i32,
                height,
                width as i32 * 4,
            )
            .unwrap();

            let cr = cairo::Context::new(&surface);

            // Clear background
            cr.set_operator(cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            cr.paint();

            // Render text outline
            cr.save();
            cr.set_operator(cairo::Operator::Over);

            cr.set_source_rgba(0.0, 0.0, 0.0, 1.0);

            pangocairo::functions::layout_path(&cr, &layout);
            cr.stroke();
            cr.restore();

            // Render text
            cr.save();
            cr.set_source_rgba(255.0, 255.0, 255.0, 1.0);

            pangocairo::functions::show_layout(&cr, &layout);

            cr.restore();
            drop(cr);

            // Safety: The surface still owns a mutable reference to the buffer but our reference
            // to the surface here is the last one. After dropping the surface the buffer would be
            // freed, so we keep an additional strong reference here before dropping the surface,
            // which is then returned. As such it's guaranteed that nothing is using the buffer
            // anymore mutably.
            unsafe {
                assert_eq!(
                    cairo_sys::cairo_surface_get_reference_count(surface.to_raw_none()),
                    1
                );
                let buffer = glib::translate::from_glib_none(buffer_ptr);
                drop(surface);
                buffer
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
        element: &gst::Element,
        state: &mut State,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let video_info = match state.video_info.as_ref() {
            Some(video_info) => Ok(video_info),
            None => {
                gst_element_error!(
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
            .get_features(0)
            .map(|f| f.contains(&gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION))
            .unwrap_or(false);

        if !upstream_has_meta {
            let mut caps_clone = caps.clone();
            let overlay_caps = caps_clone.make_mut();

            if let Some(features) = overlay_caps.get_mut_features(0) {
                features.add(&gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
                if let Some(peercaps) = self.srcpad.peer_query_caps(Some(&caps_clone)) {
                    downstream_accepts_meta = !peercaps.is_empty();
                    if downstream_accepts_meta {
                        caps = caps_clone;
                    }
                }
            }
        }

        state.attach = upstream_has_meta || downstream_accepts_meta;

        self.recalculate_layout(element, state)?;

        if !self.srcpad.push_event(gst::Event::new_caps(&caps).build()) {
            Err(gst::FlowError::NotNegotiated)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();

        if self.srcpad.check_reconfigure() {
            self.negotiate(element, &mut state)?;
        }

        for meta in buffer.iter_meta::<gst_video::VideoCaptionMeta>() {
            if meta.get_caption_type() == gst_video::VideoCaptionType::Cea608Raw {
                let data = meta.get_data();
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

                            self.overlay_text(&text, &mut state);
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

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(c) => {
                let mut state = self.state.lock().unwrap();
                state.video_info = gst_video::VideoInfo::from_caps(c.get_caps()).ok();
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

impl ObjectSubclass for Cea608Overlay {
    const NAME: &'static str = "RsCea608Overlay";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        sinkpad.set_pad_flags(gst::PadFlags::PROXY_CAPS);
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));
        srcpad.set_pad_flags(gst::PadFlags::PROXY_CAPS);

        Cea608Overlay::set_pad_functions(&sinkpad, &srcpad);

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Cea 608 overlay",
            "Video/Overlay/Subtitle",
            "Renders CEA 608 closed caption meta over raw video frames",
            "Mathieu Duponchelle <mathieu@centricular.com>",
        );

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
        klass.add_pad_template(sink_pad_template);

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);
    }
}

impl ObjectImpl for Cea608Overlay {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for Cea608Overlay {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady => {
                // Reset the whole state
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cea608overlay",
        gst::Rank::Primary,
        Cea608Overlay::get_type(),
    )
}
