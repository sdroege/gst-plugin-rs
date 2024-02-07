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

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::sink::frame::{Frame, Texture};

use std::cell::RefCell;
use std::collections::HashMap;

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "gtk4paintable",
        gst::DebugColorFlags::empty(),
        Some("GTK4 Paintable Sink Paintable"),
    )
});

#[derive(Debug)]
pub struct Paintable {
    paintables: RefCell<Vec<Texture>>,
    cached_textures: RefCell<HashMap<usize, gdk::Texture>>,
    gl_context: RefCell<Option<gdk::GLContext>>,
    #[cfg(not(feature = "gtk_v4_10"))]
    premult_shader: gsk::GLShader,
}

impl Default for Paintable {
    fn default() -> Self {
        Self {
            paintables: Default::default(),
            cached_textures: Default::default(),
            gl_context: Default::default(),
            #[cfg(not(feature = "gtk_v4_10"))]
            premult_shader: gsk::GLShader::from_bytes(&glib::Bytes::from_static(include_bytes!(
                "premult.glsl"
            ))),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Paintable {
    const NAME: &'static str = "GstGtk4Paintable";
    type Type = super::Paintable;
    type Interfaces = (gdk::Paintable,);
}

impl ObjectImpl for Paintable {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecObject::builder::<gdk::GLContext>("gl-context")
                    .nick("GL Context")
                    .blurb("GL context to use for rendering")
                    .construct_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "gl-context" => self.gl_context.borrow().to_value(),
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "gl-context" => {
                *self.gl_context.borrow_mut() = value.get::<Option<gtk::gdk::GLContext>>().unwrap();
            }
            _ => unimplemented!(),
        }
    }
}

impl PaintableImpl for Paintable {
    fn intrinsic_height(&self) -> i32 {
        if let Some(paintable) = self.paintables.borrow().first() {
            f32::round(paintable.height) as i32
        } else {
            0
        }
    }

    fn intrinsic_width(&self) -> i32 {
        if let Some(paintable) = self.paintables.borrow().first() {
            f32::round(paintable.width) as i32
        } else {
            0
        }
    }

    fn intrinsic_aspect_ratio(&self) -> f64 {
        if let Some(paintable) = self.paintables.borrow().first() {
            paintable.width as f64 / paintable.height as f64
        } else {
            0.0
        }
    }

    fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
        let snapshot = snapshot.downcast_ref::<gtk::Snapshot>().unwrap();

        let paintables = self.paintables.borrow();

        if !paintables.is_empty() {
            gst::trace!(CAT, imp: self, "Snapshotting frame");

            let (frame_width, frame_height) =
                paintables.first().map(|p| (p.width, p.height)).unwrap();

            let mut scale_x = width / frame_width as f64;
            let mut scale_y = height / frame_height as f64;
            let mut trans_x = 0.0;
            let mut trans_y = 0.0;

            // TODO: Property for keeping aspect ratio or not
            if (scale_x - scale_y).abs() > f64::EPSILON {
                if scale_x > scale_y {
                    trans_x =
                        ((frame_width as f64 * scale_x) - (frame_width as f64 * scale_y)) / 2.0;
                    scale_x = scale_y;
                } else {
                    trans_y =
                        ((frame_height as f64 * scale_y) - (frame_height as f64 * scale_x)) / 2.0;
                    scale_y = scale_x;
                }
            }

            if trans_x != 0.0 || trans_y != 0.0 {
                snapshot.append_color(
                    &gdk::RGBA::BLACK,
                    &graphene::Rect::new(0f32, 0f32, width as f32, height as f32),
                );
            }

            snapshot.translate(&graphene::Point::new(trans_x as f32, trans_y as f32));
            snapshot.scale(scale_x as f32, scale_y as f32);

            for Texture {
                texture,
                x,
                y,
                width: paintable_width,
                height: paintable_height,
                global_alpha,
                has_alpha,
            } in &*paintables
            {
                snapshot.push_opacity(*global_alpha as f64);

                let bounds = graphene::Rect::new(*x, *y, *paintable_width, *paintable_height);

                // Only premultiply GL textures that expect to be in premultiplied RGBA format.
                //
                // For GTK 4.14 or newer we use the correct format directly when building the
                // texture, but only if a GLES3+ context is used. In that case the NGL renderer is
                // used by GTK, which supports non-premultiplied formats correctly and fast.
                //
                // For GTK 4.10-4.12, or 4.14 and newer if a GLES2 context is used, we use a
                // self-mask to pre-multiply the alpha.
                //
                // For GTK before 4.10, we use a GL shader and hope that it works.
                #[cfg(feature = "gtk_v4_10")]
                {
                    let context_requires_premult = {
                        #[cfg(feature = "gtk_v4_14")]
                        {
                            self.gl_context.borrow().as_ref().map_or(false, |context| {
                                context.api() != gdk::GLAPI::GLES || context.version().0 < 3
                            })
                        }

                        #[cfg(not(feature = "gtk_v4_14"))]
                        {
                            true
                        }
                    };

                    let do_premult =
                        context_requires_premult && texture.is::<gdk::GLTexture>() && *has_alpha;
                    if do_premult {
                        snapshot.push_mask(gsk::MaskMode::Alpha);
                        snapshot.append_texture(texture, &bounds);
                        snapshot.pop(); // pop mask

                        // color matrix to set alpha of the source to 1.0 as it was
                        // already applied via the mask just above.
                        snapshot.push_color_matrix(
                            &graphene::Matrix::from_float({
                                [
                                    1.0, 0.0, 0.0, 0.0, //
                                    0.0, 1.0, 0.0, 0.0, //
                                    0.0, 0.0, 1.0, 0.0, //
                                    0.0, 0.0, 0.0, 0.0,
                                ]
                            }),
                            &graphene::Vec4::new(0.0, 0.0, 0.0, 1.0),
                        );
                    }

                    snapshot.append_texture(texture, &bounds);

                    if do_premult {
                        snapshot.pop(); // pop color matrix
                        snapshot.pop(); // pop mask 2
                    }
                }
                #[cfg(not(feature = "gtk_v4_10"))]
                {
                    let do_premult =
                        texture.is::<gdk::GLTexture>() && *has_alpha && gtk::micro_version() < 13;
                    if do_premult {
                        snapshot.push_gl_shader(
                            &self.premult_shader,
                            &bounds,
                            gsk::ShaderArgsBuilder::new(&self.premult_shader, None).to_args(),
                        );
                    }

                    snapshot.append_texture(texture, &bounds);

                    if do_premult {
                        snapshot.gl_shader_pop_texture(); // pop texture appended above from the shader
                        snapshot.pop(); // pop shader
                    }
                }

                snapshot.pop(); // pop opacity
            }
        } else {
            gst::trace!(CAT, imp: self, "Snapshotting black frame");
            snapshot.append_color(
                &gdk::RGBA::BLACK,
                &graphene::Rect::new(0f32, 0f32, width as f32, height as f32),
            );
        }
    }
}

impl Paintable {
    pub(super) fn handle_frame_changed(&self, frame: Option<Frame>) {
        let context = self.gl_context.borrow();
        if let Some(frame) = frame {
            gst::trace!(CAT, imp: self, "Received new frame");

            let new_paintables =
                frame.into_textures(context.as_ref(), &mut self.cached_textures.borrow_mut());
            let new_size = new_paintables
                .first()
                .map(|p| (f32::round(p.width) as u32, f32::round(p.height) as u32))
                .unwrap();

            let old_paintables = self.paintables.replace(new_paintables);
            let old_size = old_paintables
                .first()
                .map(|p| (f32::round(p.width) as u32, f32::round(p.height) as u32));

            if Some(new_size) != old_size {
                gst::debug!(
                    CAT,
                    imp: self,
                    "Size changed from {old_size:?} to {new_size:?}",
                );
                self.obj().invalidate_size();
            }

            self.obj().invalidate_contents();
        }
    }

    pub(super) fn handle_flush_frames(&self) {
        gst::debug!(CAT, imp: self, "Flushing frames");
        self.paintables.borrow_mut().clear();
        self.cached_textures.borrow_mut().clear();
        self.obj().invalidate_size();
        self.obj().invalidate_contents();
    }
}
