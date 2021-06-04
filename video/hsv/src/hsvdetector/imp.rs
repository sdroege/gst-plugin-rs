// Copyright (C) 2020 Julien Bardagi <julien.bardagi@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst::{gst_debug, gst_info};
use gst_base::subclass::prelude::*;

use atomic_refcell::AtomicRefCell;
use std::i32;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use std::convert::TryInto;

use super::super::hsvutils;

// Default values of properties
const DEFAULT_HUE_REF: f32 = 0.0;
const DEFAULT_HUE_VAR: f32 = 10.0;
const DEFAULT_SATURATION_REF: f32 = 0.0;
const DEFAULT_SATURATION_VAR: f32 = 0.15;
const DEFAULT_VALUE_REF: f32 = 0.0;
const DEFAULT_VALUE_VAR: f32 = 0.3;

// Property value storage
#[derive(Debug, Clone, Copy)]
struct Settings {
    hue_ref: f32,
    hue_var: f32,
    saturation_ref: f32,
    saturation_var: f32,
    value_ref: f32,
    value_var: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hue_ref: DEFAULT_HUE_REF,
            hue_var: DEFAULT_HUE_VAR,
            saturation_ref: DEFAULT_SATURATION_REF,
            saturation_var: DEFAULT_SATURATION_VAR,
            value_ref: DEFAULT_VALUE_REF,
            value_var: DEFAULT_VALUE_VAR,
        }
    }
}

// Stream-specific state, i.e. video format configuration
struct State {
    in_info: gst_video::VideoInfo,
    out_info: gst_video::VideoInfo,
}

// Struct containing all the element data
#[derive(Default)]
pub struct HsvDetector {
    settings: Mutex<Settings>,
    state: AtomicRefCell<Option<State>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "hsvdetector",
        gst::DebugColorFlags::empty(),
        Some("Rust HSV-based detection filter"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for HsvDetector {
    const NAME: &'static str = "HsvDetector";
    type Type = super::HsvDetector;
    type ParentType = gst_base::BaseTransform;
}

fn video_input_formats() -> Vec<glib::SendValue> {
    let values = [
        gst_video::VideoFormat::Rgbx,
        gst_video::VideoFormat::Xrgb,
        gst_video::VideoFormat::Bgrx,
        gst_video::VideoFormat::Xbgr,
        gst_video::VideoFormat::Rgb,
        gst_video::VideoFormat::Bgr,
    ];
    values.iter().map(|i| i.to_str().to_send_value()).collect()
}

fn video_output_formats() -> Vec<glib::SendValue> {
    let values = [
        gst_video::VideoFormat::Rgba,
        gst_video::VideoFormat::Argb,
        gst_video::VideoFormat::Bgra,
        gst_video::VideoFormat::Abgr,
    ];
    values.iter().map(|i| i.to_str().to_send_value()).collect()
}

impl HsvDetector {
    #[inline]
    fn hsv_detect<CF, DF>(
        &self,
        in_frame: &gst_video::video_frame::VideoFrameRef<&gst::buffer::BufferRef>,
        out_frame: &mut gst_video::video_frame::VideoFrameRef<&mut gst::buffer::BufferRef>,
        to_hsv: CF,
        apply_alpha: DF,
    ) where
        CF: Fn(&[u8]) -> [f32; 3],
        DF: Fn(&[u8], &mut [u8], u8),
    {
        let settings = self.settings.lock().unwrap();

        // Keep the various metadata we need for working with the video frames in
        // local variables. This saves some typing below.
        let width = in_frame.width() as usize;
        let in_stride = in_frame.plane_stride()[0] as usize;
        let in_data = in_frame.plane_data(0).unwrap();
        let out_stride = out_frame.plane_stride()[0] as usize;
        let out_data = out_frame.plane_data_mut(0).unwrap();
        let nb_input_channels = in_frame.format_info().pixel_stride()[0] as usize;

        assert_eq!(out_data.len() / out_stride, in_data.len() / in_stride);
        assert_eq!(in_data.len() % nb_input_channels, 0);

        let in_line_bytes = width * nb_input_channels;
        let out_line_bytes = width * 4;

        assert!(in_line_bytes <= in_stride);
        assert!(out_line_bytes <= out_stride);

        for (in_line, out_line) in in_data
            .chunks_exact(in_stride)
            .zip(out_data.chunks_exact_mut(out_stride))
        {
            for (in_p, out_p) in in_line[..in_line_bytes]
                .chunks_exact(nb_input_channels)
                .zip(out_line[..out_line_bytes].chunks_exact_mut(4))
            {
                let hsv = to_hsv(in_p);

                // We handle hue being circular here
                let ref_hue_offset = 180.0 - settings.hue_ref;
                let mut shifted_hue = hsv[0] + ref_hue_offset;

                if shifted_hue < 0.0 {
                    shifted_hue += 360.0;
                }

                shifted_hue %= 360.0;

                if (shifted_hue - 180.0).abs() <= settings.hue_var
                    && (hsv[1] - settings.saturation_ref).abs() <= settings.saturation_var
                    && (hsv[2] - settings.value_ref).abs() <= settings.value_var
                {
                    apply_alpha(in_p, out_p, 255);
                } else {
                    apply_alpha(in_p, out_p, 0);
                };
            }
        }
    }
}

impl ObjectImpl for HsvDetector {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_float(
                    "hue-ref",
                    "Hue reference",
                    "Hue reference in degrees",
                    f32::MIN,
                    f32::MAX,
                    DEFAULT_HUE_REF,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_float(
                    "hue-var",
                    "Hue variation",
                    "Allowed hue variation from the reference hue angle, in degrees",
                    0.0,
                    180.0,
                    DEFAULT_HUE_VAR,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_float(
                    "saturation-ref",
                    "Saturation reference",
                    "Reference saturation value",
                    0.0,
                    1.0,
                    DEFAULT_SATURATION_REF,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_float(
                    "saturation-var",
                    "Saturation variation",
                    "Allowed saturation variation from the reference value",
                    0.0,
                    1.0,
                    DEFAULT_SATURATION_VAR,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_float(
                    "value-ref",
                    "Value reference",
                    "Reference value value",
                    0.0,
                    1.0,
                    DEFAULT_VALUE_REF,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_float(
                    "value-var",
                    "Value variation",
                    "Allowed value variation from the reference value",
                    0.0,
                    1.0,
                    DEFAULT_VALUE_VAR,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "hue-ref" => {
                let mut settings = self.settings.lock().unwrap();
                let hue_ref = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing hue-ref from {} to {}",
                    settings.hue_ref,
                    hue_ref
                );
                settings.hue_ref = hue_ref;
            }
            "hue-var" => {
                let mut settings = self.settings.lock().unwrap();
                let hue_var = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing hue-var from {} to {}",
                    settings.hue_var,
                    hue_var
                );
                settings.hue_var = hue_var;
            }
            "saturation-ref" => {
                let mut settings = self.settings.lock().unwrap();
                let saturation_ref = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing saturation-ref from {} to {}",
                    settings.saturation_ref,
                    saturation_ref
                );
                settings.saturation_ref = saturation_ref;
            }
            "saturation-var" => {
                let mut settings = self.settings.lock().unwrap();
                let saturation_var = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing saturation-var from {} to {}",
                    settings.saturation_var,
                    saturation_var
                );
                settings.saturation_var = saturation_var;
            }
            "value-ref" => {
                let mut settings = self.settings.lock().unwrap();
                let value_ref = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing value-ref from {} to {}",
                    settings.value_ref,
                    value_ref
                );
                settings.value_ref = value_ref;
            }
            "value-var" => {
                let mut settings = self.settings.lock().unwrap();
                let value_var = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing value-var from {} to {}",
                    settings.value_var,
                    value_var
                );
                settings.value_var = value_var;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "hue-ref" => {
                let settings = self.settings.lock().unwrap();
                settings.hue_ref.to_value()
            }
            "hue-var" => {
                let settings = self.settings.lock().unwrap();
                settings.hue_var.to_value()
            }
            "saturation-ref" => {
                let settings = self.settings.lock().unwrap();
                settings.saturation_ref.to_value()
            }
            "saturation-var" => {
                let settings = self.settings.lock().unwrap();
                settings.saturation_var.to_value()
            }
            "value-ref" => {
                let settings = self.settings.lock().unwrap();
                settings.value_ref.to_value()
            }
            "value-var" => {
                let settings = self.settings.lock().unwrap();
                settings.value_var.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for HsvDetector {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "HSV detector",
                "Filter/Effect/Converter/Video",
                "Works within the HSV colorspace to mark positive pixels",
                "Julien Bardagi <julien.bardagi@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_simple(
                "video/x-raw",
                &[
                    ("format", &gst::List::from_owned(video_output_formats())),
                    ("width", &gst::IntRange::<i32>::new(0, i32::MAX)),
                    ("height", &gst::IntRange::<i32>::new(0, i32::MAX)),
                    (
                        "framerate",
                        &gst::FractionRange::new(
                            gst::Fraction::new(0, 1),
                            gst::Fraction::new(i32::MAX, 1),
                        ),
                    ),
                ],
            );

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            // sink pad capabilities
            let caps = gst::Caps::new_simple(
                "video/x-raw",
                &[
                    ("format", &gst::List::from_owned(video_input_formats())),
                    ("width", &gst::IntRange::<i32>::new(0, i32::MAX)),
                    ("height", &gst::IntRange::<i32>::new(0, i32::MAX)),
                    (
                        "framerate",
                        &gst::FractionRange::new(
                            gst::Fraction::new(0, 1),
                            gst::Fraction::new(i32::MAX, 1),
                        ),
                    ),
                ],
            );

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for HsvDetector {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_caps(
        &self,
        element: &Self::Type,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut other_caps = caps.clone();
        if direction == gst::PadDirection::Src {
            for s in other_caps.make_mut().iter_mut() {
                s.set("format", &gst::List::from_owned(video_input_formats()));
            }
        } else {
            for s in other_caps.make_mut().iter_mut() {
                s.set("format", &gst::List::from_owned(video_output_formats()));
            }
        };

        gst_debug!(
            CAT,
            obj: element,
            "Transformed caps from {} to {} in direction {:?}",
            caps,
            other_caps,
            direction
        );

        // In the end we need to filter the caps through an optional filter caps to get rid of any
        // unwanted caps.
        if let Some(filter) = filter {
            Some(filter.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First))
        } else {
            Some(other_caps)
        }
    }

    fn unit_size(&self, _element: &Self::Type, caps: &gst::Caps) -> Option<usize> {
        gst_video::VideoInfo::from_caps(caps)
            .map(|info| info.size())
            .ok()
    }

    fn set_caps(
        &self,
        element: &Self::Type,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        let in_info = match gst_video::VideoInfo::from_caps(incaps) {
            Err(_) => return Err(gst::loggable_error!(CAT, "Failed to parse input caps")),
            Ok(info) => info,
        };
        let out_info = match gst_video::VideoInfo::from_caps(outcaps) {
            Err(_) => return Err(gst::loggable_error!(CAT, "Failed to parse output caps")),
            Ok(info) => info,
        };

        gst_debug!(
            CAT,
            obj: element,
            "Configured for caps {} to {}",
            incaps,
            outcaps
        );

        *self.state.borrow_mut() = Some(State { in_info, out_info });

        Ok(())
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        // Drop state
        *self.state.borrow_mut() = None;

        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn transform(
        &self,
        element: &Self::Type,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        let in_frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(inbuf.as_ref(), &state.in_info)
                .map_err(|_| {
                    gst::element_error!(
                        element,
                        gst::CoreError::Failed,
                        ["Failed to map input buffer readable"]
                    );
                    gst::FlowError::Error
                })?;

        // And now map the output buffer writable, so we can fill it.
        let mut out_frame =
            gst_video::VideoFrameRef::from_buffer_ref_writable(outbuf, &state.out_info).map_err(
                |_| {
                    gst::element_error!(
                        element,
                        gst::CoreError::Failed,
                        ["Failed to map output buffer writable"]
                    );
                    gst::FlowError::Error
                },
            )?;

        match state.in_info.format() {
            gst_video::VideoFormat::Rgbx | gst_video::VideoFormat::Rgb => {
                match state.out_info.format() {
                    gst_video::VideoFormat::Rgba => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[..3].copy_from_slice(&in_p[..3]);
                                out_p[3] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Argb => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[1..4].copy_from_slice(&in_p[..3]);
                                out_p[0] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Bgra => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[0] = in_p[2];
                                out_p[1] = in_p[1];
                                out_p[2] = in_p[0];
                                out_p[3] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Abgr => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[1] = in_p[2];
                                out_p[2] = in_p[1];
                                out_p[3] = in_p[0];
                                out_p[0] = val;
                            },
                        );
                    }
                    _ => unreachable!(),
                }
            }
            gst_video::VideoFormat::Xrgb => {
                match state.out_info.format() {
                    gst_video::VideoFormat::Rgba => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[1..4].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[..3].copy_from_slice(&in_p[1..4]);
                                out_p[3] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Argb => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[1..4].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[1..4].copy_from_slice(&in_p[1..4]);
                                out_p[0] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Bgra => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[1..4].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[0] = in_p[3];
                                out_p[1] = in_p[2];
                                out_p[2] = in_p[1];
                                out_p[3] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Abgr => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_rgb(
                                    in_p[1..4].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[1] = in_p[3];
                                out_p[2] = in_p[2];
                                out_p[3] = in_p[1];
                                out_p[0] = val;
                            },
                        );
                    }
                    _ => unreachable!(),
                };
            }
            gst_video::VideoFormat::Bgrx | gst_video::VideoFormat::Bgr => {
                match state.out_info.format() {
                    gst_video::VideoFormat::Rgba => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_bgr(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[0] = in_p[2];
                                out_p[1] = in_p[1];
                                out_p[2] = in_p[0];
                                out_p[3] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Argb => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_bgr(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[1] = in_p[2];
                                out_p[2] = in_p[1];
                                out_p[3] = in_p[0];
                                out_p[0] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Bgra => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_bgr(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[..3].copy_from_slice(&in_p[..3]);
                                out_p[3] = val;
                            },
                        );
                    }
                    gst_video::VideoFormat::Abgr => {
                        self.hsv_detect(
                            &in_frame,
                            &mut out_frame,
                            |in_p| {
                                hsvutils::from_bgr(
                                    in_p[..3].try_into().expect("slice with incorrect length"),
                                )
                            },
                            |in_p, out_p, val| {
                                out_p[1..4].copy_from_slice(&in_p[..3]);
                                out_p[0] = val;
                            },
                        );
                    }
                    _ => unreachable!(),
                }
            }
            gst_video::VideoFormat::Xbgr => match state.out_info.format() {
                gst_video::VideoFormat::Rgba => {
                    self.hsv_detect(
                        &in_frame,
                        &mut out_frame,
                        |in_p| {
                            hsvutils::from_bgr(
                                in_p[1..4].try_into().expect("slice with incorrect length"),
                            )
                        },
                        |in_p, out_p, val| {
                            out_p[0] = in_p[3];
                            out_p[1] = in_p[2];
                            out_p[2] = in_p[1];
                            out_p[3] = val;
                        },
                    );
                }
                gst_video::VideoFormat::Argb => {
                    self.hsv_detect(
                        &in_frame,
                        &mut out_frame,
                        |in_p| {
                            hsvutils::from_bgr(
                                in_p[1..4].try_into().expect("slice with incorrect length"),
                            )
                        },
                        |in_p, out_p, val| {
                            out_p[1] = in_p[3];
                            out_p[2] = in_p[2];
                            out_p[3] = in_p[1];
                            out_p[0] = val;
                        },
                    );
                }
                gst_video::VideoFormat::Bgra => {
                    self.hsv_detect(
                        &in_frame,
                        &mut out_frame,
                        |in_p| {
                            hsvutils::from_bgr(
                                in_p[1..4].try_into().expect("slice with incorrect length"),
                            )
                        },
                        |in_p, out_p, val| {
                            out_p[..3].copy_from_slice(&in_p[1..4]);
                            out_p[3] = val;
                        },
                    );
                }
                gst_video::VideoFormat::Abgr => {
                    self.hsv_detect(
                        &in_frame,
                        &mut out_frame,
                        |in_p| {
                            hsvutils::from_bgr(
                                in_p[1..4].try_into().expect("slice with incorrect length"),
                            )
                        },
                        |in_p, out_p, val| {
                            out_p[1..4].copy_from_slice(&in_p[1..4]);
                            out_p[0] = val;
                        },
                    );
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };

        Ok(gst::FlowSuccess::Ok)
    }
}
