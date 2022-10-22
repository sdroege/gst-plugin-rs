use gst::prelude::*;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use std::cell::RefCell;

fn create_ui(app: &gtk::Application) {
    let pipeline = gst::Pipeline::default();
    let src = gst::ElementFactory::make("videotestsrc").build().unwrap();

    let overlay = gst::ElementFactory::make("clockoverlay")
        .property("font-desc", "Monospace 42")
        .build()
        .unwrap();

    let sink = gst::ElementFactory::make("gtk4paintablesink")
        .build()
        .unwrap();
    let paintable = sink.property::<gdk::Paintable>("paintable");

    pipeline.add_many(&[&src, &overlay, &sink]).unwrap();
    src.link_filtered(
        &overlay,
        &gst_video::VideoCapsBuilder::new()
            .width(640)
            .height(480)
            .build(),
    )
    .unwrap();
    overlay.link(&sink).unwrap();

    let window = gtk::ApplicationWindow::new(app);
    window.set_default_size(640, 480);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let picture = gtk::Picture::new();
    let label = gtk::Label::new(Some("Position: 00:00:00"));

    picture.set_paintable(Some(&paintable));
    vbox.append(&picture);
    vbox.append(&label);

    window.set_child(Some(&vbox));
    window.show();

    app.add_window(&window);

    let pipeline_weak = pipeline.downgrade();
    let timeout_id = glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
        let pipeline = match pipeline_weak.upgrade() {
            Some(pipeline) => pipeline,
            None => return glib::Continue(true),
        };

        let position = pipeline.query_position::<gst::ClockTime>();
        label.set_text(&format!("Position: {:.0}", position.display()));
        glib::Continue(true)
    });

    let bus = pipeline.bus().unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");

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
                    err.src().map(|s| s.path_string()),
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

    let timeout_id = RefCell::new(Some(timeout_id));
    let pipeline = RefCell::new(Some(pipeline));
    app.connect_shutdown(move |_| {
        window.close();

        if let Some(pipeline) = pipeline.borrow_mut().take() {
            pipeline
                .set_state(gst::State::Null)
                .expect("Unable to set the pipeline to the `Null` state");
            pipeline.bus().unwrap().remove_watch().unwrap();
        }

        if let Some(timeout_id) = timeout_id.borrow_mut().take() {
            timeout_id.remove();
        }
    });
}

fn main() {
    gst::init().unwrap();
    gtk::init().unwrap();

    gstgtk4::plugin_register_static().expect("Failed to register gstgtk4 plugin");

    {
        let app = gtk::Application::new(None, gio::ApplicationFlags::FLAGS_NONE);

        app.connect_activate(create_ui);
        app.run();
    }

    unsafe {
        gst::deinit();
    }
}
