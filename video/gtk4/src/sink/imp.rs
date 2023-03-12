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

// Global GL context that is created by the first sink and kept around until the end of the
// process. This is provided to other elements in the pipeline to make sure they create GL contexts
// that are sharing with the GTK GL context.
#[cfg(any(target_os = "macos", feature = "gst_gl"))]
enum GLContext {
    Uninitialized,
    Unsupported,
    Initialized {
        display: gst_gl::GLDisplay,
        wrapped_context: gst_gl::GLContext,
        gdk_context: ThreadGuard<gdk::GLContext>,
    },
}

#[cfg(any(target_os = "macos", feature = "gst_gl"))]
static GL_CONTEXT: Mutex<GLContext> = Mutex::new(GLContext::Uninitialized);

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
    cached_caps: Mutex<Option<gst::Caps>>,
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
                    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
                    Some(gst::CapsFeatures::new([
                        "memory:GLMemory",
                        "meta:GstVideoOverlayComposition",
                    ])),
                    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
                    Some(gst::CapsFeatures::new(["memory:GLMemory"])),
                    Some(gst::CapsFeatures::new([
                        "memory:SystemMemory",
                        "meta:GstVideoOverlayComposition",
                    ])),
                    Some(gst::CapsFeatures::new(["meta:GstVideoOverlayComposition"])),
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
                        let c = c.get_mut().unwrap();

                        if features.contains("memory:GLMemory") {
                            c.set("texture-target", "2D")
                        }
                        c.set_features_simple(Some(features));
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

                // Notify the pipeline about the GL display and wrapped context so that any other
                // elements in the pipeline ideally use the same / create GL contexts that are
                // sharing with this one.
                #[cfg(any(target_os = "macos", feature = "gst_gl"))]
                {
                    let gl_context = GL_CONTEXT.lock().unwrap();
                    if let GLContext::Initialized {
                        display,
                        wrapped_context,
                        ..
                    } = &*gl_context
                    {
                        let display = display.clone();
                        let wrapped_context = wrapped_context.clone();
                        drop(gl_context);

                        gst_gl::gl_element_propagate_display_context(&*self.obj(), &display);
                        let mut ctx = gst::Context::new("gst.gl.app_context", true);
                        {
                            let ctx = ctx.get_mut().unwrap();
                            ctx.structure_mut().set("context", &wrapped_context);
                        }
                        let _ = self.obj().post_message(
                            gst::message::HaveContext::builder(ctx)
                                .src(&*self.obj())
                                .build(),
                        );
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

        gst::debug!(CAT, imp: self, "Advertising our own caps: {tmp_caps:?}");

        if let Some(filter_caps) = filter {
            gst::debug!(
                CAT,
                imp: self,
                "Intersecting with filter caps: {filter_caps:?}",
            );

            tmp_caps = filter_caps.intersect_with_mode(&tmp_caps, gst::CapsIntersectMode::First);
        };

        gst::debug!(CAT, imp: self, "Returning caps: {tmp_caps:?}");
        Some(tmp_caps)
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp: self, "Setting caps {caps:?}");

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

        #[cfg(any(target_os = "macos", feature = "gst_gl"))]
        {
            if let GLContext::Initialized {
                wrapped_context, ..
            } = &*GL_CONTEXT.lock().unwrap()
            {
                if wrapped_context.check_feature("GL_ARB_sync")
                    || wrapped_context.check_feature("GL_EXT_EGL_sync")
                {
                    query.add_allocation_meta::<gst_gl::GLSyncMeta>(None)
                }
            }
        }

        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, imp: self, "Handling query {:?}", query);

        match query.view_mut() {
            #[cfg(any(target_os = "macos", feature = "gst_gl"))]
            gst::QueryViewMut::Context(q) => {
                // Avoid holding the locks while we respond to the query
                // The objects are ref-counted anyway.
                let mut display_clone = None;
                let mut wrapped_context_clone = None;
                if let GLContext::Initialized {
                    display,
                    wrapped_context,
                    ..
                } = &*GL_CONTEXT.lock().unwrap()
                {
                    display_clone = Some(display.clone());
                    wrapped_context_clone = Some(wrapped_context.clone());
                }

                if let (Some(display), Some(wrapped_context)) =
                    (display_clone, wrapped_context_clone)
                {
                    return gst_gl::functions::gl_handle_context_query(
                        &*self.obj(),
                        q,
                        Some(&display),
                        None::<&gst_gl::GLContext>,
                        Some(&wrapped_context),
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

        let wrapped_context = {
            #[cfg(not(any(target_os = "macos", feature = "gst_gl")))]
            {
                None
            }
            #[cfg(any(target_os = "macos", feature = "gst_gl"))]
            {
                let gl_context = GL_CONTEXT.lock().unwrap();
                if let GLContext::Initialized {
                    wrapped_context, ..
                } = &*gl_context
                {
                    Some(wrapped_context.clone())
                } else {
                    None
                }
            }
        };
        let frame = Frame::new(buffer, info, wrapped_context.as_ref()).map_err(|err| {
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
            if !matches!(&*GL_CONTEXT.lock().unwrap(), GLContext::Initialized { .. }) {
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
        #[cfg(any(target_os = "macos", feature = "gst_gl"))]
        {
            self.initialize_gl_context();
        }

        self.configure_caps();
        self.initialize_paintable(paintable_storage);
    }

    fn initialize_paintable(
        &self,
        paintable_storage: &mut MutexGuard<Option<ThreadGuard<Paintable>>>,
    ) {
        gst::debug!(CAT, imp: self, "Initializing paintable");

        // The channel for the SinkEvents
        let (sender, receiver) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let self_ = self.to_owned();

        let paintable = utils::invoke_on_main_thread(move || {
            // Attach the receiver from the main thread to make sure it is called
            // from a place where it can acquire the default main context.
            receiver.attach(
                Some(&glib::MainContext::default()),
                glib::clone!(
                    @weak self_ => @default-return glib::Continue(false),
                    move |action| self_.do_action(action)
                ),
            );

            #[cfg(any(target_os = "macos", feature = "gst_gl"))]
            {
                let gdk_context = if let GLContext::Initialized { gdk_context, .. } =
                    &*GL_CONTEXT.lock().unwrap()
                {
                    Some(gdk_context.get_ref().clone())
                } else {
                    None
                };
                ThreadGuard::new(Paintable::new(gdk_context))
            }
            #[cfg(not(any(target_os = "macos", feature = "gst_gl")))]
            {
                ThreadGuard::new(Paintable::new(None))
            }
        });

        **paintable_storage = Some(paintable);

        *self.sender.lock().unwrap() = Some(sender);
    }

    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    fn initialize_gl_context(&self) {
        gst::debug!(CAT, imp: self, "Realizing GDK GL Context");

        let self_ = self.to_owned();
        utils::invoke_on_main_thread(move || {
            self_.initialize_gl_context_main();
        });
    }

    #[cfg(any(target_os = "macos", feature = "gst_gl"))]
    fn initialize_gl_context_main(&self) {
        gst::debug!(CAT, imp: self, "Realizing GDK GL Context from main thread");

        let mut gl_context_guard = GL_CONTEXT.lock().unwrap();
        if !matches!(&*gl_context_guard, GLContext::Uninitialized) {
            gst::debug!(CAT, imp: self, "Already initialized GL context before");
            return;
        }
        *gl_context_guard = GLContext::Unsupported;

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
        let gdk_display = match gdk::Display::default() {
            Some(display) => display,
            None => {
                gst::warning!(CAT, imp: self, "Failed to retrieve GDK display");
                return;
            }
        };
        let gdk_context = match gdk_display.create_gl_context() {
            Ok(gdk_context) => gdk_context,
            Err(err) => {
                gst::warning!(CAT, imp: self, "Failed to create GDK GL Context: {err}");
                return;
            }
        };

        match gdk_context.type_().name() {
            #[cfg(all(target_os = "linux", feature = "x11egl"))]
            "GdkX11GLContextEGL" => (),
            #[cfg(all(target_os = "linux", feature = "x11glx"))]
            "GdkX11GLContextGLX" => (),
            #[cfg(all(target_os = "linux", feature = "wayland"))]
            "GdkWaylandGLContext" => (),
            #[cfg(target_os = "macos")]
            "GdkMacosGLContext" => (),
            display => {
                gst::error!(CAT, imp: self, "Unsupported GDK display {display} for GL");
                return;
            }
        }

        gst::info!(CAT, imp: self, "Realizing GDK GL Context",);

        if let Err(err) = gdk_context.realize() {
            gst::warning!(CAT, imp: self, "Failed to realize GDK GL Context: {err}");
            return;
        }

        gst::info!(CAT, imp: self, "Successfully realized GDK GL Context");

        gdk_context.make_current();

        let res = match gdk_context.type_().name() {
            #[cfg(all(target_os = "linux", feature = "x11egl"))]
            "GdkX11GLContextEGL" => self.initialize_x11egl(gdk_display),
            #[cfg(all(target_os = "linux", feature = "x11glx"))]
            "GdkX11GLContextGLX" => self.initialize_x11glx(gdk_display),
            #[cfg(all(target_os = "linux", feature = "wayland"))]
            "GdkWaylandGLContext" => self.initialize_waylandegl(gdk_display),
            #[cfg(target_os = "macos")]
            "GdkMacosGLContext" => self.initialize_macosgl(gdk_display),
            display_type => {
                unreachable!("Unsupported GDK display {display_type} for GL");
            }
        };

        let (display, wrapped_context) = match res {
            Some((display, wrapped_context)) => (display, wrapped_context),
            None => {
                return;
            }
        };

        match wrapped_context.activate(true) {
            Ok(_) => gst::info!(CAT, imp: self, "Successfully activated GL Context"),
            Err(_) => {
                gst::error!(CAT, imp: self, "Failed to activate GL context",);
                return;
            }
        };

        if let Err(err) = wrapped_context.fill_info() {
            gst::error!(
                CAT,
                imp: self,
                "Failed to fill info on the GL Context: {err}",
            );
            // Deactivate the context upon failure
            if wrapped_context.activate(false).is_err() {
                gst::error!(
                    CAT,
                    imp: self,
                    "Failed to deactivate the context after failing fill info",
                );
            }
            return;
        }

        gst::info!(CAT, imp: self, "Successfully initialized GL Context");

        *gl_context_guard = GLContext::Initialized {
            display,
            wrapped_context,
            gdk_context: ThreadGuard::new(gdk_context),
        };
    }

    #[cfg(all(target_os = "linux", feature = "x11egl"))]
    fn initialize_x11egl(
        &self,
        display: gdk::Display,
    ) -> Option<(gst_gl::GLDisplay, gst_gl::GLContext)> {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for x11 EGL backend and display"
        );

        let platform = gst_gl::GLPlatform::EGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext");
            return None;
        }

        // FIXME: bindings
        unsafe {
            use glib::translate::*;

            let display = display.downcast::<gdk_x11::X11Display>().unwrap();
            let x11_display =
                gdk_x11::ffi::gdk_x11_display_get_egl_display(display.to_glib_none().0);
            if x11_display.is_null() {
                gst::error!(CAT, imp: self, "Failed to get EGL display");
                return None;
            }

            let gst_display = gst_gl_egl::ffi::gst_gl_display_egl_new_with_egl_display(x11_display);
            let gst_display =
                gst_gl::GLDisplay::from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

            let wrapped_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);
            let wrapped_context = match wrapped_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return None;
                }
                Some(wrapped_context) => wrapped_context,
            };

            Some((gst_display, wrapped_context))
        }
    }

    #[cfg(all(target_os = "linux", feature = "x11glx"))]
    fn initialize_x11glx(
        &self,
        display: gdk::Display,
    ) -> Option<(gst_gl::GLDisplay, gst_gl::GLContext)> {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for x11 GLX backend and display"
        );

        let platform = gst_gl::GLPlatform::GLX;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext");
            return None;
        }

        // FIXME: bindings
        unsafe {
            use glib::translate::*;

            let display = display.downcast::<gdk_x11::X11Display>().unwrap();
            let x11_display = gdk_x11::ffi::gdk_x11_display_get_xdisplay(display.to_glib_none().0);
            if x11_display.is_null() {
                gst::error!(CAT, imp: self, "Failed to get X11 display");
                return None;
            }

            let gst_display = gst_gl_x11::ffi::gst_gl_display_x11_new_with_display(x11_display);
            let gst_display =
                gst_gl::GLDisplay::from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

            let wrapped_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);
            let wrapped_context = match wrapped_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return None;
                }
                Some(wrapped_context) => wrapped_context,
            };

            Some((gst_display, wrapped_context))
        }
    }

    #[cfg(all(target_os = "linux", feature = "wayland"))]
    fn initialize_waylandegl(
        &self,
        display: gdk::Display,
    ) -> Option<(gst_gl::GLDisplay, gst_gl::GLContext)> {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for Wayland EGL backend and display"
        );

        let platform = gst_gl::GLPlatform::EGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext");
            return None;
        }

        // FIXME: bindings
        unsafe {
            use glib::translate::*;

            // let wayland_display = gdk_wayland::WaylandDisplay::wl_display(display.downcast());
            // get the ptr directly since we are going to use it raw
            let display = display.downcast::<gdk_wayland::WaylandDisplay>().unwrap();
            let wayland_display =
                gdk_wayland::ffi::gdk_wayland_display_get_wl_display(display.to_glib_none().0);
            if wayland_display.is_null() {
                gst::error!(CAT, imp: self, "Failed to get Wayland display");
                return None;
            }

            let gst_display =
                gst_gl_wayland::ffi::gst_gl_display_wayland_new_with_display(wayland_display);
            let gst_display =
                gst_gl::GLDisplay::from_glib_full(gst_display as *mut gst_gl::ffi::GstGLDisplay);

            let wrapped_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

            let wrapped_context = match wrapped_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return None;
                }
                Some(wrapped_context) => wrapped_context,
            };

            Some((gst_display, wrapped_context))
        }
    }

    #[cfg(target_os = "macos")]
    fn initialize_macosgl(
        &self,
        display: gdk::Display,
    ) -> Option<(gst_gl::GLDisplay, gst_gl::GLContext)> {
        gst::info!(
            CAT,
            imp: self,
            "Initializing GL for macOS backend and display"
        );

        let platform = gst_gl::GLPlatform::CGL;
        let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
        let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

        if gl_ctx == 0 {
            gst::error!(CAT, imp: self, "Failed to get handle from GdkGLContext");
            return None;
        }

        let gst_display = gst_gl::GLDisplay::new();
        unsafe {
            let wrapped_context =
                gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

            let wrapped_context = match wrapped_context {
                None => {
                    gst::error!(CAT, imp: self, "Failed to create wrapped GL context");
                    return;
                }
                Some(wrapped_context) => wrapped_context,
            };

            Some((gst_display, wrapped_context))
        }
    }
}
