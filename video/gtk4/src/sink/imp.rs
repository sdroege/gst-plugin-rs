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

use glib::{thread_guard::ThreadGuard, Sender};
use gtk::prelude::*;
use gtk::{gdk, glib};

use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use once_cell::sync::Lazy;
use std::sync::{Mutex, MutexGuard};

use crate::utils;

#[cfg(any(target_os = "macos", feature = "gst_gl"))]
use gst_gl::prelude::GLContextExt as GstGLContextExt;
#[cfg(any(target_os = "macos", feature = "gst_gl"))]
use gst_gl::prelude::*;
#[cfg(any(target_os = "macos", feature = "gst_gl"))]
use std::sync::atomic::{AtomicBool, Ordering};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "gtk4paintablesink",
        gst::DebugColorFlags::empty(),
        Some("GTK4 Paintable sink"),
    )
});

#[derive(Default)]
pub struct PaintableSink {
    paintable: Mutex<Option<ThreadGuard<Paintable>>>,
    info: Mutex<Option<gst_video::VideoInfo>>,
    sender: Mutex<Option<Sender<SinkEvent>>>,
    pending_frame: Mutex<Option<Frame>>,
    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    gst_display: Mutex<Option<gst_gl::GLDisplay>>,
    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    gst_app_context: Mutex<Option<gst_gl::GLContext>>,
    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    gst_context: Mutex<Option<gst_gl::GLContext>>,
    cached_caps: Mutex<Option<gst::Caps>>,
    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
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
                if paintable.is_owner() {
                    paintable.get_ref().to_value()
                } else {
                    gst::error!(
                        CAT,
                        imp: self,
                        "Can't retrieve Paintable from non-main thread"
                    );
                    None::<&gdk::Paintable>.to_value()
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

                drop(paintable);

                #[cfg(any(target_os = "macos", feature = "gst_gl"))]
                {
                    if self.have_gl_context.load(Ordering::Relaxed) {
                        if self.initialize_gl_wrapper() {
                            // We must have a display at this point.
                            let display = self.gst_display.lock().unwrap().clone().unwrap();
                            gst_gl::gl_element_propagate_display_context(&*self.obj(), &display);
                        } else {
                            self.have_gl_context.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
            _ => (),
        }

        let res = self.parent_change_state(transition);

        match transition {
            gst::StateChange::PausedToReady => {
                let _ = self.info.lock().unwrap().take();
                let _ = self.pending_frame.lock().unwrap().take();

                // Flush frames from the GDK paintable but don't wait
                // for this to finish as this can other deadlock.
                let self_ = self.to_owned();
                glib::MainContext::default().invoke(move || {
                    let paintable = self_.paintable.lock().unwrap();
                    if let Some(paintable) = &*paintable {
                        paintable.get_ref().handle_flush_frames();
                    }
                });
            }
            #[cfg(any(target_os = "macos", feature = "gst_gl"))]
            gst::StateChange::ReadyToNull => {
                let _ = self.gst_context.lock().unwrap().take();
                let _ = self.gst_app_context.lock().unwrap().take();
                let _ = self.gst_display.lock().unwrap().take();
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

        #[cfg(not(any(target_os = "macos", feature = "gst_gl")))]
        {
            Ok(())
        }

        #[cfg(any(target_os = "macos", feature = "gst_gl"))]
        {
            // Early return if there is no context initialized
            let gst_context = match &*self.gst_context.lock().unwrap() {
                None => {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "Found no GL Context during propose_allocation."
                    );
                    return Ok(());
                }
                Some(gst_context) => gst_context.clone(),
            };

            // GL specific things
            let (caps, need_pool) = query.get_owned();
            if caps.is_empty() || caps.is_any() {
                return Ok(());
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
            let buffer_pool = if need_pool {
                let buffer_pool = gst_gl::GLBufferPool::new(&gst_context);
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

                Some(buffer_pool)
            } else {
                None
            };

            // we need at least 2 buffer because we hold on to the last one
            query.add_allocation_pool(buffer_pool.as_ref(), size, 2, 0);

            if gst_context.check_feature("GL_ARB_sync")
                || gst_context.check_feature("GL_EXT_EGL_sync")
            {
                query.add_allocation_meta::<gst_gl::GLSyncMeta>(None)
            }

            Ok(())
        }
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, imp: self, "Handling query {:?}", query);

        match query.view_mut() {
            #[cfg(any(target_os = "macos", feature = "gst_gl"))]
            gst::QueryViewMut::Context(q) => {
                // Avoid holding the locks while we respond to the query
                // The objects are ref-counted anyway.
                let (gst_display, app_ctx, gst_ctx) = (
                    self.gst_display.lock().unwrap().clone(),
                    self.gst_app_context.lock().unwrap().clone(),
                    self.gst_context.lock().unwrap().clone(),
                );

                if let (Some(gst_display), Some(app_ctx), Some(gst_ctx)) =
                    (gst_display, app_ctx, gst_ctx)
                {
                    return gst_gl::functions::gl_handle_context_query(
                        &*self.obj(),
                        q,
                        Some(&gst_display),
                        Some(&gst_ctx),
                        Some(&app_ctx),
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

        let have_gl_context = {
            #[cfg(not(any(target_os = "macos", feature = "gst_gl")))]
            {
                false
            }
            #[cfg(any(target_os = "macos", feature = "gst_gl"))]
            {
                self.have_gl_context.load(Ordering::Relaxed)
            }
        };
        let frame = Frame::new(buffer, info, have_gl_context).map_err(|err| {
            gst::error!(CAT, imp: self, "Failed to map video frame");
            err
        })?;
        self.pending_frame.lock().unwrap().replace(frame);

        let sender = self.sender.lock().unwrap();
        let sender = sender.as_ref().ok_or_else(|| {
            gst::error!(CAT, imp: self, "Have no main thread sender");
            gst::FlowError::Flushing
        })?;

        sender.send(SinkEvent::FrameChanged).map_err(|_| {
            gst::error!(CAT, imp: self, "Have main thread receiver shut down");
            gst::FlowError::Flushing
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}

impl PaintableSink {
    fn pending_frame(&self) -> Option<Frame> {
        self.pending_frame.lock().unwrap().take()
    }

    fn do_action(&self, action: SinkEvent) -> glib::Continue {
        let paintable = self.paintable.lock().unwrap();
        let paintable = match &*paintable {
            Some(paintable) => paintable,
            None => return glib::Continue(false),
        };

        match action {
            SinkEvent::FrameChanged => {
                gst::trace!(CAT, imp: self, "Frame changed");
                paintable
                    .get_ref()
                    .handle_frame_changed(self.pending_frame())
            }
        }

        glib::Continue(true)
    }

    fn configure_caps(&self) {
        #[allow(unused_mut)]
        let mut tmp_caps = Self::pad_templates()[0].caps().clone();

        #[cfg(any(target_os = "macos", feature = "gst_gl"))]
        {
            // Filter out GL caps from the template pads if we have no context
            if !self.have_gl_context.load(Ordering::Relaxed) {
                tmp_caps = tmp_caps
                    .iter_with_features()
                    .filter(|(_, features)| !features.contains("memory:GLMemory"))
                    .map(|(s, c)| (s.to_owned(), c.to_owned()))
                    .collect::<gst::Caps>();
            }
        }

        self.cached_caps
            .lock()
            .expect("Failed to lock Mutex")
            .replace(tmp_caps);
    }

    fn create_paintable(&self, paintable_storage: &mut MutexGuard<Option<ThreadGuard<Paintable>>>) {
        #[allow(unused_mut)]
        let mut ctx = None;

        #[cfg(any(target_os = "macos", feature = "gst_gl"))]
        {
            if let Some(c) = self.realize_context() {
                self.have_gl_context.store(true, Ordering::Relaxed);
                ctx = Some(c);
            }
        }

        self.configure_caps();
        self.initialize_paintable(ctx, paintable_storage);
    }

    fn initialize_paintable(
        &self,
        gl_context: Option<ThreadGuard<gdk::GLContext>>,
        paintable_storage: &mut MutexGuard<Option<ThreadGuard<Paintable>>>,
    ) {
        gst::debug!(CAT, imp: self, "Initializing paintable");

        let paintable = utils::invoke_on_main_thread(|| {
            // grab the context out of the fragile
            let ctx = gl_context.map(|f| f.into_inner());
            ThreadGuard::new(Paintable::new(ctx))
        });

        // The channel for the SinkEvents
        let (sender, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);
        let self_ = self.to_owned();
        receiver.attach(
            None,
            glib::clone!(
                @weak self_ => @default-return glib::Continue(false),
                move |action| self_.do_action(action)
            ),
        );

        **paintable_storage = Some(paintable);

        *self.sender.lock().unwrap() = Some(sender);
    }

    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    fn realize_context(&self) -> Option<ThreadGuard<gdk::GLContext>> {
        gst::debug!(CAT, imp: self, "Realizing GDK GL Context");

        let self_ = self.to_owned();
        utils::invoke_on_main_thread(move || -> Option<ThreadGuard<gdk::GLContext>> {
            gst::debug!(
                CAT,
                imp: self_,
                "Realizing GDK GL Context from main context"
            );

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
            let ctx = match display.create_gl_context() {
                Ok(ctx) => ctx,
                Err(err) => {
                    gst::warning!(CAT, imp: self_, "Failed to create GDK GL Context: {err}");
                    return None;
                }
            };

            match ctx.type_().name() {
                #[cfg(all(target_os = "linux", feature = "x11egl"))]
                "GdkX11GLContextEGL" => (),
                #[cfg(all(target_os = "linux", feature = "x11glx"))]
                "GdkX11GLContextGLX" => (),
                #[cfg(all(target_os = "linux", feature = "wayland"))]
                "GdkWaylandGLContext" => (),
                #[cfg(target_os = "macos")]
                "GdkMacosGLContext" => (),
                display => {
                    gst::error!(CAT, imp: self_, "Unsupported GDK display {display} for GL");
                    return None;
                }
            }

            gst::info!(CAT, imp: &self_, "Realizing GDK GL Context",);

            match ctx.realize() {
                Ok(_) => {
                    gst::info!(CAT, imp: self_, "Successfully realized GDK GL Context",);
                    Some(ThreadGuard::new(ctx))
                }
                Err(err) => {
                    gst::warning!(CAT, imp: self_, "Failed to realize GDK GL Context: {err}",);
                    None
                }
            }
        })
    }

    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    fn initialize_gl_wrapper(&self) -> bool {
        gst::info!(CAT, imp: self, "Initializing GDK GL Context");
        let self_ = self.to_owned();
        utils::invoke_on_main_thread(move || self_.initialize_gl())
    }

    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    fn initialize_gl(&self) -> bool {
        let ctx = {
            let paintable = self.paintable.lock().unwrap();
            // Impossible to not have a paintable and GL context at this point
            paintable.as_ref().unwrap().get_ref().context().unwrap()
        };

        let display = gtk::prelude::GLContextExt::display(&ctx)
            .expect("Failed to get GDK Display from GDK Context.");
        ctx.make_current();

        let mut app_ctx_guard = self.gst_app_context.lock().unwrap();
        let mut display_guard = self.gst_display.lock().unwrap();

        match ctx.type_().name() {
            #[cfg(all(target_os = "linux", feature = "x11egl"))]
            "GdkX11GLContextEGL" => {
                self.initialize_x11egl(display, &mut display_guard, &mut app_ctx_guard);
            }
            #[cfg(all(target_os = "linux", feature = "x11glx"))]
            "GdkX11GLContextGLX" => {
                self.initialize_x11glx(display, &mut display_guard, &mut app_ctx_guard);
            }
            #[cfg(all(target_os = "linux", feature = "wayland"))]
            "GdkWaylandGLContext" => {
                self.initialize_waylandegl(display, &mut display_guard, &mut app_ctx_guard);
            }
            #[cfg(target_os = "macos")]
            "GdkMacosGLContext" => {
                self.initialize_macosgl(display, &mut display_guard, &mut app_ctx_guard);
            }
            _ => {
                unreachable!("Unsupported GDK display {display} for GL");
            }
        };

        // This should have been initialized once we are done with the platform checks
        let app_ctx = match &*app_ctx_guard {
            None => {
                assert!(display_guard.is_none());
                return false;
            }
            Some(app_ctx) => app_ctx,
        };

        let display = match &*display_guard {
            None => return false,
            Some(display) => display,
        };

        match app_ctx.activate(true) {
            Ok(_) => gst::info!(CAT, imp: self, "Successfully activated GL Context."),
            Err(_) => {
                gst::error!(CAT, imp: self, "Failed to activate GL context",);
                *app_ctx_guard = None;
                *display_guard = None;
                return false;
            }
        };

        if let Err(err) = app_ctx.fill_info() {
            gst::error!(
                CAT,
                imp: self,
                "Failed to fill info on the GL Context: {err}",
            );
            // Deactivate the context upon failure
            if app_ctx.activate(false).is_err() {
                gst::error!(
                    CAT,
                    imp: self,
                    "Failed to deactivate the context after failing fill info",
                );
            }
            *app_ctx_guard = None;
            *display_guard = None;

            return false;
        }

        if app_ctx.activate(false).is_err() {
            gst::error!(CAT, imp: self, "Failed to deactivate GL context",);
            *app_ctx_guard = None;
            *display_guard = None;
            return false;
        }

        gst::info!(
            CAT,
            imp: self,
            "Successfully deactivated GL Context after fill_info"
        );

        let gst_context = match display.create_context(app_ctx) {
            Ok(gst_context) => gst_context,
            Err(err) => {
                gst::error!(CAT, imp: self, "Could not create GL context: {err}");
                *app_ctx_guard = None;
                *display_guard = None;
                return false;
            }
        };

        match display.add_context(&gst_context) {
            Ok(_) => {
                let mut gst_ctx_guard = self.gst_context.lock().unwrap();
                gst::info!(CAT, imp: self, "Successfully initialized GL Context");
                gst_ctx_guard.replace(gst_context);
                true
            }
            Err(_) => {
                gst::error!(CAT, imp: self, "Could not add GL context to display");
                *app_ctx_guard = None;
                *display_guard = None;
                false
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "x11egl"))]
    fn initialize_x11egl(
        &self,
        display: gdk::Display,
        display_guard: &mut Option<gst_gl::GLDisplay>,
        app_ctx_guard: &mut Option<gst_gl::GLContext>,
    ) {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for x11 EGL backend and display."
        );

        let platform = gst_gl::GLPlatform::EGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext",);
            return;
        }

        // FIXME: bindings
        unsafe {
            use glib::translate::*;

            let d = display.downcast::<gdk_x11::X11Display>().unwrap();
            let x11_display = gdk_x11::ffi::gdk_x11_display_get_egl_display(d.to_glib_none().0);
            if x11_display.is_null() {
                gst::error!(CAT, imp: self, "Failed to get EGL display");
                return;
            }

            let gst_display = gst_gl_egl::ffi::gst_gl_display_egl_new_with_egl_display(x11_display);
            let gst_display =
                gst_gl::GLDisplay::from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

            let gst_app_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);
            let gst_app_context = match gst_app_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return;
                }
                Some(gst_app_context) => gst_app_context,
            };

            display_guard.replace(gst_display);
            app_ctx_guard.replace(gst_app_context);
        }
    }

    #[cfg(all(target_os = "linux", feature = "x11glx"))]
    fn initialize_x11glx(
        &self,
        display: gdk::Display,
        display_guard: &mut Option<gst_gl::GLDisplay>,
        app_ctx_guard: &mut Option<gst_gl::GLContext>,
    ) {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for x11 GLX backend and display."
        );

        let platform = gst_gl::GLPlatform::GLX;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext",);
            return;
        }

        // FIXME: bindings
        unsafe {
            use glib::translate::*;

            let d = display.downcast::<gdk_x11::X11Display>().unwrap();
            let x11_display = gdk_x11::ffi::gdk_x11_display_get_xdisplay(d.to_glib_none().0);
            if x11_display.is_null() {
                gst::error!(CAT, imp: self, "Failed to get X11 display");
                return;
            }

            let gst_display = gst_gl_x11::ffi::gst_gl_display_x11_new_with_display(x11_display);
            let gst_display =
                gst_gl::GLDisplay::from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

            let gst_app_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);
            let gst_app_context = match gst_app_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return;
                }
                Some(gst_app_context) => gst_app_context,
            };

            display_guard.replace(gst_display);
            app_ctx_guard.replace(gst_app_context);
        }
    }

    #[cfg(all(target_os = "linux", feature = "wayland"))]
    fn initialize_waylandegl(
        &self,
        display: gdk::Display,
        display_guard: &mut Option<gst_gl::GLDisplay>,
        app_ctx_guard: &mut Option<gst_gl::GLContext>,
    ) {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for Wayland EGL backend and display."
        );

        let platform = gst_gl::GLPlatform::EGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext",);
            return;
        }

        // FIXME: bindings
        unsafe {
            use glib::translate::*;

            // let wayland_display = gdk_wayland::WaylandDisplay::wl_display(display.downcast());
            // get the ptr directly since we are going to use it raw
            let d = display.downcast::<gdk_wayland::WaylandDisplay>().unwrap();
            let wayland_display =
                gdk_wayland::ffi::gdk_wayland_display_get_wl_display(d.to_glib_none().0);
            if wayland_display.is_null() {
                gst::error!(CAT, imp: self, "Failed to get Wayland display");
                return;
            }

            let gst_display =
                gst_gl_wayland::ffi::gst_gl_display_wayland_new_with_display(wayland_display);
            let gst_display =
                gst_gl::GLDisplay::from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

            let gst_app_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

            let gst_app_context = match gst_app_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return;
                }
                Some(gst_app_context) => gst_app_context,
            };

            display_guard.replace(gst_display);
            app_ctx_guard.replace(gst_app_context);
        }
    }

    #[cfg(target_os = "macos")]
    fn initialize_macosgl(
        &self,
        display: gdk::Display,
        display_guard: &mut Option<gst_gl::GLDisplay>,
        app_ctx_guard: &mut Option<gst_gl::GLContext>,
    ) {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for macOS backend and display."
        );

        let platform = gst_gl::GLPlatform::CGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext",);
            return;
        }

        let gst_display = gst_gl::GLDisplay::new();
        unsafe {
            let gst_app_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

            let gst_app_context = match gst_app_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return;
                }
                Some(gst_app_context) => gst_app_context,
            };

            display_guard.replace(gst_display);
            app_ctx_guard.replace(gst_app_context);
        }
    }
}
