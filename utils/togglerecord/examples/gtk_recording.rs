// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
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

use gio::prelude::*;
use gtk::prelude::*;
use std::cell::RefCell;

fn create_pipeline() -> (
    gst::Pipeline,
    gst::Pad,
    gst::Pad,
    gst::Element,
    gst::Element,
    gtk::Widget,
) {
    let pipeline = gst::Pipeline::new(None);

    let video_src = gst::ElementFactory::make("videotestsrc", None).unwrap();
    video_src.set_property("is-live", &true).unwrap();
    video_src.set_property_from_str("pattern", "ball");

    let timeoverlay = gst::ElementFactory::make("timeoverlay", None).unwrap();
    timeoverlay
        .set_property("font-desc", &"Monospace 20")
        .unwrap();

    let video_tee = gst::ElementFactory::make("tee", None).unwrap();
    let video_queue1 = gst::ElementFactory::make("queue", None).unwrap();
    let video_queue2 = gst::ElementFactory::make("queue", None).unwrap();

    let video_convert1 = gst::ElementFactory::make("videoconvert", None).unwrap();
    let video_convert2 = gst::ElementFactory::make("videoconvert", None).unwrap();

    let (video_sink, video_widget) =
        if let Ok(gtkglsink) = gst::ElementFactory::make("gtkglsink", None) {
            let glsinkbin = gst::ElementFactory::make("glsinkbin", None).unwrap();
            glsinkbin.set_property("sink", &gtkglsink).unwrap();

            let widget = gtkglsink.property("widget").unwrap();
            (glsinkbin, widget.get::<gtk::Widget>().unwrap())
        } else {
            let sink = gst::ElementFactory::make("gtksink", None).unwrap();
            let widget = sink.property("widget").unwrap();
            (sink, widget.get::<gtk::Widget>().unwrap())
        };

    let video_enc = gst::ElementFactory::make("x264enc", None).unwrap();
    video_enc.set_property("rc-lookahead", &10i32).unwrap();
    video_enc.set_property("key-int-max", &30u32).unwrap();
    let video_parse = gst::ElementFactory::make("h264parse", None).unwrap();

    let audio_src = gst::ElementFactory::make("audiotestsrc", None).unwrap();
    audio_src.set_property("is-live", &true).unwrap();
    audio_src.set_property_from_str("wave", "ticks");

    let audio_tee = gst::ElementFactory::make("tee", None).unwrap();
    let audio_queue1 = gst::ElementFactory::make("queue", None).unwrap();
    let audio_queue2 = gst::ElementFactory::make("queue", None).unwrap();

    let audio_convert1 = gst::ElementFactory::make("audioconvert", None).unwrap();
    let audio_convert2 = gst::ElementFactory::make("audioconvert", None).unwrap();

    let audio_sink = gst::ElementFactory::make("autoaudiosink", None).unwrap();

    let audio_enc = gst::ElementFactory::make("lamemp3enc", None).unwrap();
    let audio_parse = gst::ElementFactory::make("mpegaudioparse", None).unwrap();

    let togglerecord = gst::ElementFactory::make("togglerecord", None).unwrap();

    let mux_queue1 = gst::ElementFactory::make("queue", None).unwrap();
    let mux_queue2 = gst::ElementFactory::make("queue", None).unwrap();

    let mux = gst::ElementFactory::make("mp4mux", None).unwrap();

    let file_sink = gst::ElementFactory::make("filesink", None).unwrap();
    file_sink
        .set_property("location", &"recording.mp4")
        .unwrap();
    file_sink.set_property("async", &false).unwrap();
    file_sink.set_property("sync", &false).unwrap();

    pipeline
        .add_many(&[
            &video_src,
            &timeoverlay,
            &video_tee,
            &video_queue1,
            &video_queue2,
            &video_convert1,
            &video_convert2,
            &video_sink,
            &video_enc,
            &video_parse,
            &audio_src,
            &audio_tee,
            &audio_queue1,
            &audio_queue2,
            &audio_convert1,
            &audio_convert2,
            &audio_sink,
            &audio_enc,
            &audio_parse,
            &togglerecord,
            &mux_queue1,
            &mux_queue2,
            &mux,
            &file_sink,
        ])
        .unwrap();

    gst::Element::link_many(&[
        &video_src,
        &timeoverlay,
        &video_tee,
        &video_queue1,
        &video_convert1,
        &video_sink,
    ])
    .unwrap();

    gst::Element::link_many(&[
        &video_tee,
        &video_queue2,
        &video_convert2,
        &video_enc,
        &video_parse,
    ])
    .unwrap();

    video_parse
        .link_pads(Some("src"), &togglerecord, Some("sink"))
        .unwrap();
    togglerecord
        .link_pads(Some("src"), &mux_queue1, Some("sink"))
        .unwrap();
    mux_queue1
        .link_pads(Some("src"), &mux, Some("video_%u"))
        .unwrap();

    gst::Element::link_many(&[
        &audio_src,
        &audio_tee,
        &audio_queue1,
        &audio_convert1,
        &audio_sink,
    ])
    .unwrap();

    gst::Element::link_many(&[
        &audio_tee,
        &audio_queue2,
        &audio_convert2,
        &audio_enc,
        &audio_parse,
    ])
    .unwrap();

    audio_parse
        .link_pads(Some("src"), &togglerecord, Some("sink_0"))
        .unwrap();
    togglerecord
        .link_pads(Some("src_0"), &mux_queue2, Some("sink"))
        .unwrap();
    mux_queue2
        .link_pads(Some("src"), &mux, Some("audio_%u"))
        .unwrap();

    gst::Element::link_many(&[&mux, &file_sink]).unwrap();

    (
        pipeline,
        video_queue2.static_pad("sink").unwrap(),
        audio_queue2.static_pad("sink").unwrap(),
        togglerecord,
        video_sink,
        video_widget,
    )
}

fn create_ui(app: &gtk::Application) {
    let (pipeline, video_pad, audio_pad, togglerecord, video_sink, video_widget) =
        create_pipeline();

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_default_size(320, 240);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.pack_start(&video_widget, true, true, 0);

    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let position_label = gtk::Label::new(Some("Position: 00:00:00"));
    hbox.pack_start(&position_label, true, true, 5);
    let recorded_duration_label = gtk::Label::new(Some("Recorded: 00:00:00"));
    hbox.pack_start(&recorded_duration_label, true, true, 5);
    vbox.pack_start(&hbox, false, false, 5);

    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let record_button = gtk::Button::with_label("Record");
    hbox.pack_start(&record_button, true, true, 5);
    let finish_button = gtk::Button::with_label("Finish");
    hbox.pack_start(&finish_button, true, true, 5);
    vbox.pack_start(&hbox, false, false, 5);

    window.add(&vbox);
    window.show_all();

    app.add_window(&window);

    let video_sink_weak = video_sink.downgrade();
    let togglerecord_weak = togglerecord.downgrade();
    let timeout_id = glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        let video_sink = match video_sink_weak.upgrade() {
            Some(video_sink) => video_sink,
            None => return glib::Continue(true),
        };

        let togglerecord = match togglerecord_weak.upgrade() {
            Some(togglerecord) => togglerecord,
            None => return glib::Continue(true),
        };

        let position = video_sink
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);
        position_label.set_text(&format!("Position: {:.1}", position));

        let recording_duration = togglerecord
            .static_pad("src")
            .unwrap()
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);
        recorded_duration_label.set_text(&format!("Recorded: {:.1}", recording_duration));

        glib::Continue(true)
    });

    let togglerecord_weak = togglerecord.downgrade();
    record_button.connect_clicked(move |button| {
        let togglerecord = match togglerecord_weak.upgrade() {
            Some(togglerecord) => togglerecord,
            None => return,
        };

        let recording = !togglerecord
            .property("record")
            .unwrap()
            .get::<bool>()
            .unwrap();
        togglerecord.set_property("record", &recording).unwrap();

        button.set_label(if recording { "Stop" } else { "Record" });
    });

    let record_button_weak = record_button.downgrade();
    finish_button.connect_clicked(move |button| {
        let record_button = match record_button_weak.upgrade() {
            Some(record_button) => record_button,
            None => return,
        };

        record_button.set_sensitive(false);
        button.set_sensitive(false);

        video_pad.send_event(gst::event::Eos::new());
        audio_pad.send_event(gst::event::Eos::new());
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

    gsttogglerecord::plugin_register_static().expect("Failed to register togglerecord plugin");

    let app = gtk::Application::new(None, gio::ApplicationFlags::FLAGS_NONE);

    app.connect_activate(create_ui);
    app.run();
}
