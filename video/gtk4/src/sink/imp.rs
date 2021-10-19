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

use super::SinkEvent;
use crate::sink::frame::Frame;
use crate::sink::paintable::Paintable;

use glib::translate::*;
use glib::Sender;
use gtk::prelude::*;
use gtk::{gdk, glib};

use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gst_gl::prelude::GLContextExt as GstGLContextExt;
use gst_gl::prelude::*;
use gst_video::subclass::prelude::*;

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::utils;
use fragile::Fragile;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "gstgtk4paintablesink",
        gst::DebugColorFlags::empty(),
        Some("GTK4 Paintable sink"),
    )
});

#[derive(Default, Debug)]
pub struct PaintableSink {
    paintable: Mutex<Option<Fragile<Paintable>>>,
    info: Mutex<Option<gst_video::VideoInfo>>,
    sender: Mutex<Option<Sender<SinkEvent>>>,
    pending_frame: Mutex<Option<Frame>>,
    gst_display: Mutex<Option<gst_gl::GLDisplay>>,
    gst_app_context: Mutex<Option<gst_gl::GLContext>>,
    gst_context: Mutex<Option<gst_gl::GLContext>>,
    cached_caps: Mutex<Option<gst::Caps>>,
    have_gl_context: AtomicBool,
}

impl Drop for PaintableSink {
    fn drop(&mut self) {
        let mut paintable = self.paintable.lock().unwrap();
        if let Some(paintable) = paintable.take() {
            glib::MainContext::default().invoke(|| drop(paintable))
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PaintableSink {
    const NAME: &'static str = "GstGtk4PaintableSink";
    type Type = super::PaintableSink;
    type ParentType = gst_video::VideoSink;
}

impl ObjectImpl for PaintableSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecObject::builder::<gdk::Paintable>("paintable")
                    .nick("Paintable")
                    .blurb("The Paintable the sink renders to")
                    .read_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "paintable" => {
                let mut paintable = self.paintable.lock().unwrap();
                if paintable.is_none() {
                    self.create_paintable(&mut paintable);
                }

                let paintable = match &*paintable {
                    Some(ref paintable) => paintable,
                    None => {
                        gst::error!(CAT, imp: self, "Failed to create paintable");
                        return None::<&gdk::Paintable>.to_value();
                    }
                };

                // Getter must be called from the main thread
                match paintable.try_get() {
                    Ok(paintable) => paintable.to_value(),
                    Err(_) => {
                        gst::error!(
                            CAT,
                            imp: self,
                            "Can't retrieve Paintable from non-main thread"
                        );
                        None::<&gdk::Paintable>.to_value()
                    }
                }
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for PaintableSink {}

impl ElementImpl for PaintableSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "GTK 4 Paintable Sink",
                "Sink/Video",
                "A GTK 4 Paintable sink",
                "Bilal Elmoussaoui <bil.elmoussaoui@gmail.com>, Jordan Petridis <jordan@centricular.com>, Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            // Those are the supported formats by a gdk::Texture
            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                for features in [
                    None,
                    Some(&["memory:GLMemory", "meta:GstVideoOverlayComposition"][..]),
                    Some(&["memory:GLMemory"][..]),
                    Some(&["memory:SystemMemory", "meta:GstVideoOverlayComposition"][..]),
                    Some(&["meta:GstVideoOverlayComposition"][..]),
                ] {
                    let mut c = gst_video::video_make_raw_caps(&[
                        gst_video::VideoFormat::Bgra,
                        gst_video::VideoFormat::Argb,
                        gst_video::VideoFormat::Rgba,
                        gst_video::VideoFormat::Abgr,
                        gst_video::VideoFormat::Rgb,
                        gst_video::VideoFormat::Bgr,
                    ])
                    .build();

                    if let Some(features) = features {
                        c.get_mut()
                            .unwrap()
                            .set_features_simple(Some(gst::CapsFeatures::new(features)));

                        if features.contains(&"memory:GLMemory") {
                            c.get_mut()
                                .unwrap()
                                .set_simple(&[("texture-target", &"2D")])
                        }
                    }

                    caps.append(c);
                }
            }

            vec![gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap()]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let mut paintable = self.paintable.lock().unwrap();

                if paintable.is_none() {
                    self.create_paintable(&mut paintable);
                }

                if paintable.is_none() {
                    gst::error!(CAT, imp: self, "Failed to create paintable");
                    return Err(gst::StateChangeError);
                }
            }
            _ => (),
        }

        let res = self.parent_change_state(transition);

        match transition {
            gst::StateChange::PausedToReady => {
                let _ = self.info.lock().unwrap().take();
                let _ = self.pending_frame.lock().unwrap().take();
            }
            _ => (),
        }

        res
    }
}

impl BaseSinkImpl for PaintableSink {
    fn caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        let cached_caps = self
            .cached_caps
            .lock()
            .expect("Failed to lock cached caps mutex")
            .clone();

        let mut tmp_caps = cached_caps.unwrap_or_else(|| {
            let templ = Self::pad_templates();
            templ[0].caps().clone()
        });

        gst::debug!(CAT, imp: self, "Advertising our own caps: {:?}", &tmp_caps);

        if let Some(filter_caps) = filter {
            gst::debug!(
                CAT,
                imp: self,
                "Intersecting with filter caps: {:?}",
                &filter_caps
            );

            tmp_caps = filter_caps.intersect_with_mode(&tmp_caps, gst::CapsIntersectMode::First);
        };

        gst::debug!(CAT, imp: self, "Returning caps: {:?}", &tmp_caps);
        Some(tmp_caps)
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp: self, "Setting caps {:?}", caps);

        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps"))?;

        self.info.lock().unwrap().replace(video_info);

        Ok(())
    }

    fn propose_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp: self, "Proposing Allocation query");

        self.parent_propose_allocation(query)?;

        query.add_allocation_meta::<gst_video::VideoMeta>(None);
        // TODO: Provide a preferred "window size" here for higher-resolution rendering
        query.add_allocation_meta::<gst_video::VideoOverlayCompositionMeta>(None);

        {
            // Early return if there is no context initialized
            let gst_context_guard = self.gst_context.lock().unwrap();
            if gst_context_guard.is_none() {
                gst::debug!(
                    CAT,
                    imp: self,
                    "Found no GL Context during propose_allocation."
                );
                return Ok(());
            }
        }

        // GL specific things
        let (caps, need_pool) = query.get_owned();

        if caps.is_empty() {
            return Err(gst::loggable_error!(CAT, "No caps where specified."));
        }

        if let Some(f) = caps.features(0) {
            if !f.contains("memory:GLMemory") {
                gst::debug!(
                    CAT,
                    imp: self,
                    "No 'memory:GLMemory' feature in caps: {}",
                    caps
                )
            }
        }

        let info = gst_video::VideoInfo::from_caps(&caps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to get VideoInfo from caps"))?;

        let size = info.size() as u32;

        {
            let gst_context = { self.gst_context.lock().unwrap().clone().unwrap() };
            let buffer_pool = gst_gl::GLBufferPool::new(&gst_context);

            if need_pool {
                gst::debug!(CAT, imp: self, "Creating new Pool");

                let mut config = buffer_pool.config();
                config.set_params(Some(&caps), size, 0, 0);
                config.add_option("GstBufferPoolOptionGLSyncMeta");

                if let Err(err) = buffer_pool.set_config(config) {
                    return Err(gst::loggable_error!(
                        CAT,
                        format!("Failed to set config in the GL BufferPool.: {}", err)
                    ));
                }
            }

            // we need at least 2 buffer because we hold on to the last one
            query.add_allocation_pool(Some(&buffer_pool), size, 2, 0);

            if gst_context.check_feature("GL_ARB_sync")
                || gst_context.check_feature("GL_EXT_EGL_sync")
            {
                query.add_allocation_meta::<gst_gl::GLSyncMeta>(None)
            }
        }

        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, imp: self, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Context(q) => {
                // Avoid holding the locks while we respond to the query
                // The objects are ref-counted anyway.
                let gst_display = { self.gst_display.lock().unwrap().clone() };
                if let Some(display) = gst_display {
                    let (app_ctx, gst_ctx) = {
                        (
                            self.gst_app_context.lock().unwrap().clone(),
                            self.gst_context.lock().unwrap().clone(),
                        )
                    };
                    assert_ne!(app_ctx, None);
                    assert_ne!(gst_ctx, None);

                    return gst_gl::functions::gl_handle_context_query(
                        &*self.instance(),
                        q,
                        Some(&display),
                        gst_ctx.as_ref(),
                        app_ctx.as_ref(),
                    );
                }

                BaseSinkImplExt::parent_query(self, query)
            }
            _ => BaseSinkImplExt::parent_query(self, query),
        }
    }
}

impl VideoSinkImpl for PaintableSink {
    fn show_frame(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, imp: self, "Rendering buffer {:?}", buffer);

        // Empty buffer, nothing to render
        if buffer.n_memory() == 0 {
            gst::trace!(
                CAT,
                imp: self,
                "Empty buffer, nothing to render. Returning."
            );
            return Ok(gst::FlowSuccess::Ok);
        };

        let info = self.info.lock().unwrap();
        let info = info.as_ref().ok_or_else(|| {
            gst::error!(CAT, imp: self, "Received no caps yet");
            gst::FlowError::NotNegotiated
        })?;

        let frame = Frame::new(buffer, info, self.have_gl_context.load(Ordering::Relaxed))
            .map_err(|err| {
                gst::error!(CAT, imp: self, "Failed to map video frame");
                err
            })?;
        self.pending_frame.lock().unwrap().replace(frame);

        let sender = self.sender.lock().unwrap();
        let sender = sender.as_ref().ok_or_else(|| {
            gst::error!(CAT, imp: self, "Have no main thread sender");
            gst::FlowError::Error
        })?;

        sender.send(SinkEvent::FrameChanged).map_err(|_| {
            gst::error!(CAT, imp: self, "Have main thread receiver shut down");
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}

impl PaintableSink {
    fn pending_frame(&self) -> Option<Frame> {
        self.pending_frame.lock().unwrap().take()
    }

    fn do_action(&self, action: SinkEvent) -> glib::Continue {
        let paintable = self.paintable.lock().unwrap().clone();
        let paintable = match paintable {
            Some(paintable) => paintable,
            None => return glib::Continue(false),
        };

        match action {
            SinkEvent::FrameChanged => {
                gst::trace!(CAT, imp: self, "Frame changed");
                paintable.get().handle_frame_changed(self.pending_frame())
            }
        }

        glib::Continue(true)
    }

    fn configure_caps(&self) {
        let mut tmp_caps = Self::pad_templates()[0].caps().clone();

        // Filter out GL caps from the template pads if we have no context
        if !self.have_gl_context.load(Ordering::Relaxed) {
            tmp_caps = tmp_caps
                .iter_with_features()
                .filter(|(_, features)| !features.contains("memory:GLMemory"))
                .map(|(s, c)| (s.to_owned(), c.to_owned()))
                .collect::<gst::Caps>();
        }

        self.cached_caps
            .lock()
            .expect("Failed to lock Mutex")
            .replace(tmp_caps);
    }

    fn create_paintable(&self, paintable_storage: &mut MutexGuard<Option<Fragile<Paintable>>>) {
        let ctx = self.realize_context();

        let ctx = if let Some(c) = ctx {
            if let Ok(c) = self.initialize_gl_wrapper(c) {
                self.have_gl_context.store(true, Ordering::Relaxed);
                Some(c)
            } else {
                None
            }
        } else {
            None
        };

        self.configure_caps();
        self.initialize_paintable(ctx, paintable_storage);
    }

    fn initialize_paintable(
        &self,
        gl_context: Option<Fragile<gdk::GLContext>>,
        paintable_storage: &mut MutexGuard<Option<Fragile<Paintable>>>,
    ) {
        gst::debug!(CAT, imp: self, "Initializing paintable");

        let paintable = utils::invoke_on_main_thread(|| {
            // grab the context out of the fragile
            let ctx = gl_context.map(|f| f.into_inner());
            Fragile::new(Paintable::new(ctx))
        });

        // The channel for the SinkEvents
        let (sender, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);
        let sink = self.instance();
        receiver.attach(
            None,
            glib::clone!(
                @weak sink => @default-return glib::Continue(false),
                move |action| sink.imp().do_action(action)
            ),
        );

        **paintable_storage = Some(paintable);

        *self.sender.lock().unwrap() = Some(sender);
    }

    fn realize_context(&self) -> Option<Fragile<gdk::GLContext>> {
        gst::debug!(CAT, imp: self, "Realizing GDK GL Context");

        let weak = self.instance().downgrade();
        let cb = move || -> Option<Fragile<gdk::GLContext>> {
            let obj = weak
                .upgrade()
                .expect("Failed to upgrade Weak ref during gl initialization.");

            gst::debug!(CAT, obj: &obj, "Realizing GDK GL Context from main context");

            // This can return NULL but only happens in 2 situations:
            // * If the function is called before gtk_init
            // * If the function is called after gdk_display_close(default_display)
            // Both of which are treated as programming errors.
            //
            // However, when we are building the docs, gtk_init doesn't get called
            // and this would cause the documentation generation to error.
            // Thus its okayish to return None here and fallback to software
            // rendering, since this path isn't going to be used by applications
            // anyway.
            //
            // FIXME: add a couple more gtk_init checks across the codebase where
            // applicable since this is no longer going to panic.
            let display = gdk::Display::default()?;
            let ctx = display.create_gl_context();

            if let Ok(ctx) = ctx {
                gst::info!(CAT, obj: &obj, "Realizing GDK GL Context",);

                if ctx.realize().is_ok() {
                    gst::info!(CAT, obj: &obj, "Successfully realized GDK GL Context",);
                    return Some(Fragile::new(ctx));
                } else {
                    gst::warning!(CAT, obj: &obj, "Failed to realize GDK GL Context",);
                }
            } else {
                gst::warning!(CAT, obj: &obj, "Failed to create GDK GL Context",);
            };

            None
        };

        utils::invoke_on_main_thread(cb)
    }

    fn initialize_gl_wrapper(
        &self,
        context: Fragile<gdk::GLContext>,
    ) -> Result<Fragile<gdk::GLContext>, glib::Error> {
        gst::info!(CAT, imp: self, "Initializing GDK GL Context");
        let self_ = self.instance().clone();
        utils::invoke_on_main_thread(move || self_.imp().initialize_gl(context))
    }

    fn initialize_gl(
        &self,
        context: Fragile<gdk::GLContext>,
    ) -> Result<Fragile<gdk::GLContext>, glib::Error> {
        let ctx = context.get();
        let display = gtk::prelude::GLContextExt::display(ctx)
            .expect("Failed to get GDK Display from GDK Context.");
        ctx.make_current();

        let mut app_ctx_guard = self.gst_app_context.lock().unwrap();
        let mut display_ctx_guard = self.gst_display.lock().unwrap();

        match ctx.type_().name() {
            #[cfg(all(target_os = "linux", feature = "x11egl"))]
            "GdkX11GLContextEGL" => {
                self.initialize_x11egl(display, &mut display_ctx_guard, &mut app_ctx_guard)?;
            }
            #[cfg(all(target_os = "linux", feature = "x11glx"))]
            "GdkX11GLContextGLX" => {
                self.initialize_x11glx(display, &mut display_ctx_guard, &mut app_ctx_guard)?;
            }
            #[cfg(all(target_os = "linux", feature = "wayland"))]
            "GdkWaylandGLContext" => {
                self.initialize_waylandegl(display, &mut display_ctx_guard, &mut app_ctx_guard)?;
            }
            _ => {
                gst::error!(
                    CAT,
                    imp: self,
                    "Unsupported GDK display {} for GL",
                    &display,
                );
                return Err(glib::Error::new(
                    gst::ResourceError::Failed,
                    &format!("Unsupported GDK display {display} for GL"),
                ));
            }
        };

        // This should have been initialized once we are done with the platform checks
        assert!(app_ctx_guard.is_some());

        match app_ctx_guard.as_ref().unwrap().activate(true) {
            Ok(_) => gst::info!(CAT, imp: self, "Successfully activated GL Context."),
            Err(_) => {
                gst::error!(CAT, imp: self, "Failed to activate GL context",);
                return Err(glib::Error::new(
                    gst::ResourceError::Failed,
                    "Failed to activate GL context",
                ));
            }
        };

        match app_ctx_guard.as_ref().unwrap().fill_info() {
            Ok(_) => {
                match app_ctx_guard.as_ref().unwrap().activate(true) {
                    Ok(_) => gst::info!(
                        CAT,
                        imp: self,
                        "Successfully activated GL Context after fill_info"
                    ),
                    Err(_) => {
                        gst::error!(CAT, imp: self, "Failed to activate GL context",);
                        return Err(glib::Error::new(
                            gst::ResourceError::Failed,
                            "Failed to activate GL context after fill_info",
                        ));
                    }
                };
            }
            Err(err) => {
                gst::error!(
                    CAT,
                    imp: self,
                    "Failed to fill info on the GL Context: {}",
                    &err
                );
                return Err(err);
            }
        };

        match display_ctx_guard
            .as_ref()
            .unwrap()
            .create_context(app_ctx_guard.as_ref().unwrap())
        {
            Ok(gst_context) => {
                let mut gst_ctx_guard = self.gst_context.lock().unwrap();
                gst::info!(CAT, imp: self, "Successfully initialized GL Context");
                gst_ctx_guard.replace(gst_context);
                Ok(context)
            }
            Err(err) => {
                gst::error!(CAT, imp: self, "Could not create GL context: {}", &err);
                Err(err)
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "x11egl"))]
    fn initialize_x11egl(
        &self,
        display: gdk::Display,
        display_ctx_guard: &mut Option<gst_gl::GLDisplay>,
        app_ctx_guard: &mut Option<gst_gl::GLContext>,
    ) -> Result<(), glib::Error> {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL with for x11 EGL backend and display."
        );

        let platform = gst_gl::GLPlatform::EGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx != 0 {
            unsafe {
                let d = display.downcast::<gdk_x11::X11Display>().unwrap();
                let x11_display = gdk_x11::ffi::gdk_x11_display_get_egl_display(d.to_glib_none().0);
                assert!(!x11_display.is_null());

                let gst_display =
                    gst_gl_egl::ffi::gst_gl_display_egl_new_with_egl_display(x11_display);
                assert!(!gst_display.is_null());
                let gst_display: gst_gl::GLDisplay =
                    from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

                let gst_app_context =
                    gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

                assert!(gst_app_context.is_some());

                display_ctx_guard.replace(gst_display);
                app_ctx_guard.replace(gst_app_context.unwrap());

                Ok(())
            }
        } else {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext",);
            Err(glib::Error::new(
                gst::ResourceError::Failed,
                "Failed to get handle from GdkGLContext",
            ))
        }
    }

    #[cfg(all(target_os = "linux", feature = "x11glx"))]
    fn initialize_x11glx(
        &self,
        display: gdk::Display,
        display_ctx_guard: &mut Option<gst_gl::GLDisplay>,
        app_ctx_guard: &mut Option<gst_gl::GLContext>,
    ) -> Result<(), glib::Error> {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL with for x11 GLX backend and display."
        );

        let platform = gst_gl::GLPlatform::GLX;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx != 0 {
            unsafe {
                let d = display.downcast::<gdk_x11::X11Display>().unwrap();
                let x11_display = gdk_x11::ffi::gdk_x11_display_get_xdisplay(d.to_glib_none().0);
                assert!(!x11_display.is_null());

                let gst_display = gst_gl_x11::ffi::gst_gl_display_x11_new_with_display(x11_display);
                assert!(!gst_display.is_null());
                let gst_display: gst_gl::GLDisplay =
                    from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

                let gst_app_context =
                    gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

                assert!(gst_app_context.is_some());

                display_ctx_guard.replace(gst_display);
                app_ctx_guard.replace(gst_app_context.unwrap());

                Ok(())
            }
        } else {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext",);
            Err(glib::Error::new(
                gst::ResourceError::Failed,
                "Failed to get handle from GdkGLContext",
            ))
        }
    }

    #[cfg(all(target_os = "linux", feature = "wayland"))]
    fn initialize_waylandegl(
        &self,
        display: gdk::Display,
        display_ctx_guard: &mut Option<gst_gl::GLDisplay>,
        app_ctx_guard: &mut Option<gst_gl::GLContext>,
    ) -> Result<(), glib::Error> {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL with for Wayland EGL backend and display."
        );

        let platform = gst_gl::GLPlatform::EGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        // FIXME: bindings
        if gl_ctx != 0 {
            unsafe {
                // let wayland_display = gdk_wayland::WaylandDisplay::wl_display(display.downcast());
                // get the ptr directly since we are going to use it raw
                let d = display.downcast::<gdk_wayland::WaylandDisplay>().unwrap();
                let wayland_display =
                    gdk_wayland::ffi::gdk_wayland_display_get_wl_display(d.to_glib_none().0);
                assert!(!wayland_display.is_null());

                let gst_display =
                    gst_gl_wayland::ffi::gst_gl_display_wayland_new_with_display(wayland_display);
                assert!(!gst_display.is_null());
                let gst_display: gst_gl::GLDisplay =
                    from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

                let gst_app_context =
                    gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

                assert!(gst_app_context.is_some());

                display_ctx_guard.replace(gst_display);
                app_ctx_guard.replace(gst_app_context.unwrap());

                Ok(())
            }
        } else {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext",);
            Err(glib::Error::new(
                gst::ResourceError::Failed,
                "Failed to get handle from GdkGLContext",
            ))
        }
    }
}
