// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
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

use gio::prelude::*;

use gst::glib;
use gst::prelude::*;

use gtk::prelude::*;

use std::cell::RefCell;

const MAIN_PIPELINE: &str = "videotestsrc is-live=true pattern=ball";
const FALLBACK_PIPELINE: &str = "videotestsrc is-live=true pattern=snow";

//const MAIN_PIPELINE: &str = "videotestsrc is-live=true pattern=ball ! x264enc tune=zerolatency";
//const FALLBACK_PIPELINE: &str = "videotestsrc is-live=true pattern=snow ! x264enc tune=zerolatency";

fn create_pipeline() -> (gst::Pipeline, gst::Pad, gst::Element, gtk::Widget) {
    let pipeline = gst::Pipeline::new(None);

    let video_src = gst::parse_bin_from_description(MAIN_PIPELINE, true)
        .unwrap()
        .upcast();
    let fallback_video_src = gst::parse_bin_from_description(&FALLBACK_PIPELINE, true)
        .unwrap()
        .upcast();

    let fallbackswitch = gst::ElementFactory::make("fallbackswitch", None).unwrap();
    fallbackswitch
        .set_property("timeout", &gst::ClockTime::SECOND)
        .unwrap();

    let decodebin = gst::ElementFactory::make("decodebin", None).unwrap();
    let videoconvert = gst::ElementFactory::make("videoconvert", None).unwrap();

    let videoconvert_clone = videoconvert.clone();
    decodebin.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap();
        let s = caps.structure(0).unwrap();

        let sinkpad = videoconvert_clone.static_pad("sink").unwrap();

        if s.name() == "video/x-raw" && !sinkpad.is_linked() {
            pad.link(&sinkpad).unwrap();
        }
    });

    let (video_sink, video_widget) =
        //if let Some(gtkglsink) = gst::ElementFactory::make("gtkglsink", None) {
        //    let glsinkbin = gst::ElementFactory::make("glsinkbin", None).unwrap();
        //    glsinkbin.set_property("sink", &gtkglsink).unwrap();

        //    let widget = gtkglsink.get_property("widget").unwrap();
        //    (glsinkbin, widget.get::<gtk::Widget>().unwrap().unwrap())
        //} else
        {
            let sink = gst::ElementFactory::make("gtksink", None).unwrap();
            let widget = sink.property("widget").unwrap();
            (sink, widget.get::<gtk::Widget>().unwrap())
        };

    pipeline
        .add_many(&[
            &video_src,
            &fallback_video_src,
            &fallbackswitch,
            &decodebin,
            &videoconvert,
            &video_sink,
        ])
        .unwrap();

    video_src
        .link_pads(Some("src"), &fallbackswitch, Some("sink"))
        .unwrap();
    fallback_video_src
        .link_pads(Some("src"), &fallbackswitch, Some("fallback_sink"))
        .unwrap();
    fallbackswitch
        .link_pads(Some("src"), &decodebin, Some("sink"))
        .unwrap();
    videoconvert
        .link_pads(Some("src"), &video_sink, Some("sink"))
        .unwrap();

    (
        pipeline,
        video_src.static_pad("src").unwrap(),
        video_sink,
        video_widget,
    )
}

fn create_ui(app: &gtk::Application) {
    let (pipeline, video_src_pad, video_sink, video_widget) = create_pipeline();

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_default_size(320, 240);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.pack_start(&video_widget, true, true, 0);

    let position_label = gtk::Label::new(Some("Position: 00:00:00"));
    vbox.pack_start(&position_label, true, true, 5);

    let drop_button = gtk::ToggleButton::with_label("Drop Signal");
    vbox.pack_start(&drop_button, true, true, 5);

    window.add(&vbox);
    window.show_all();

    app.add_window(&window);

    let video_sink_weak = video_sink.downgrade();
    let timeout_id = glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        let video_sink = match video_sink_weak.upgrade() {
            Some(video_sink) => video_sink,
            None => return glib::Continue(true),
        };

        let position = video_sink
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);
        position_label.set_text(&format!("Position: {:.1}", position));

        glib::Continue(true)
    });

    let video_src_pad_weak = video_src_pad.downgrade();
    let drop_id = RefCell::new(None);
    drop_button.connect_toggled(move |drop_button| {
        let video_src_pad = match video_src_pad_weak.upgrade() {
            Some(video_src_pad) => video_src_pad,
            None => return,
        };

        let drop = drop_button.is_active();
        if drop {
            let mut drop_id = drop_id.borrow_mut();
            if drop_id.is_none() {
                *drop_id = video_src_pad
                    .add_probe(gst::PadProbeType::BUFFER, |_, _| gst::PadProbeReturn::Drop);
            }
        } else if let Some(drop_id) = drop_id.borrow_mut().take() {
            video_src_pad.remove_probe(drop_id);
        }
    });

    let app_weak = app.downgrade();
    window.connect_delete_event(move |_, _| {
        let app = match app_weak.upgrade() {
            Some(app) => app,
            None => return Inhibit(false),
        };

        app.quit();
        Inhibit(false)
    });

    let bus = pipeline.bus().unwrap();
    let app_weak = app.downgrade();
    bus.add_watch_local(move |_, msg| {
        use gst::MessageView;

        let app = match app_weak.upgrade() {
            Some(app) => app,
            None => return glib::Continue(false),
        };

        match msg.view() {
            MessageView::Eos(..) => app.quit(),
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    msg.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                app.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    })
    .expect("Failed to add bus watch");

    pipeline.set_state(gst::State::Playing).unwrap();

    // Pipeline reference is owned by the closure below, so will be
    // destroyed once the app is destroyed
    let timeout_id = RefCell::new(Some(timeout_id));
    app.connect_shutdown(move |_| {
        pipeline.set_state(gst::State::Null).unwrap();

        bus.remove_watch().unwrap();

        if let Some(timeout_id) = timeout_id.borrow_mut().take() {
            glib::source_remove(timeout_id);
        }
    });
}

fn main() {
    gst::init().unwrap();
    gtk::init().unwrap();

    gstfallbackswitch::plugin_register_static().expect("Failed to register fallbackswitch plugin");

    let app = gtk::Application::new(None, gio::ApplicationFlags::FLAGS_NONE);

    app.connect_activate(create_ui);
    app.run();
}
