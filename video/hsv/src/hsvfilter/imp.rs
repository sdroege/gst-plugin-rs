// Copyright (C) 2020 Julien Bardagi <julien.bardagi@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib::subclass;
use glib::subclass::prelude::*;
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
const DEFAULT_HUE_SHIFT: f32 = 0.0;
const DEFAULT_SATURATION_MUL: f32 = 1.0;
const DEFAULT_SATURATION_OFF: f32 = 0.0;
const DEFAULT_VALUE_MUL: f32 = 1.0;
const DEFAULT_VALUE_OFF: f32 = 0.0;

// Property value storage
#[derive(Debug, Clone, Copy)]
struct Settings {
    hue_shift: f32,
    saturation_mul: f32,
    saturation_off: f32,
    value_mul: f32,
    value_off: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hue_shift: DEFAULT_HUE_SHIFT,
            saturation_mul: DEFAULT_SATURATION_MUL,
            saturation_off: DEFAULT_SATURATION_OFF,
            value_mul: DEFAULT_VALUE_MUL,
            value_off: DEFAULT_VALUE_OFF,
        }
    }
}

// Metadata for the properties
static PROPERTIES: [subclass::Property; 5] = [
    subclass::Property("hue-shift", |name| {
        glib::ParamSpec::float(
            name,
            "Hue shift",
            "Hue shifting in degrees",
            f32::MIN,
            f32::MAX,
            DEFAULT_HUE_SHIFT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("saturation-mul", |name| {
        glib::ParamSpec::float(
            name,
            "Saturation multiplier",
            "Saturation multiplier to apply to the saturation value (before offset)",
            f32::MIN,
            f32::MAX,
            DEFAULT_SATURATION_MUL,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("saturation-off", |name| {
        glib::ParamSpec::float(
            name,
            "Saturation offset",
            "Saturation offset to add to the saturation value (after multiplier)",
            f32::MIN,
            f32::MAX,
            DEFAULT_SATURATION_OFF,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("value-mul", |name| {
        glib::ParamSpec::float(
            name,
            "Value multiplier",
            "Value multiplier to apply to the value (before offset)",
            f32::MIN,
            f32::MAX,
            DEFAULT_VALUE_MUL,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("value-off", |name| {
        glib::ParamSpec::float(
            name,
            "Value offset",
            "Value offset to add to the value (after multiplier)",
            f32::MIN,
            f32::MAX,
            DEFAULT_VALUE_OFF,
            glib::ParamFlags::READWRITE,
        )
    }),
];

// Stream-specific state, i.e. video format configuration
struct State {
    info: gst_video::VideoInfo,
}

// Struct containing all the element data
pub struct HsvFilter {
    settings: Mutex<Settings>,
    state: AtomicRefCell<Option<State>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "hsvfilter",
        gst::DebugColorFlags::empty(),
        Some("Rust HSV transformation filter"),
    )
});

impl ObjectSubclass for HsvFilter {
    const NAME: &'static str = "HsvFilter";
    type Type = super::HsvFilter;
    type ParentType = gst_base::BaseTransform;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    // Boilerplate macro
    glib::object_subclass!();

    // Creates a new instance.
    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: AtomicRefCell::new(None),
        }
    }

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "HSV filter",
            "Filter/Effect/Converter/Video",
            "Works within the HSV colorspace to apply tranformations to incoming frames",
            "Julien Bardagi <julien.bardagi@gmail.com>",
        );

        // src pad capabilities
        let caps = gst::Caps::new_simple(
            "video/x-raw",
            &[
                (
                    "format",
                    &gst::List::new(&[&gst_video::VideoFormat::Rgbx.to_str()]),
                ),
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
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        // Install all our properties
        klass.install_properties(&PROPERTIES);

        klass.configure(
            gst_base::subclass::BaseTransformMode::AlwaysInPlace,
            false,
            false,
        );
    }
}

impl ObjectImpl for HsvFilter {
    fn set_property(&self, obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("hue-shift", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let hue_shift = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing hue-shift from {} to {}",
                    settings.hue_shift,
                    hue_shift
                );
                settings.hue_shift = hue_shift;
            }
            subclass::Property("saturation-mul", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let saturation_mul = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing saturation-mul from {} to {}",
                    settings.saturation_mul,
                    saturation_mul
                );
                settings.saturation_mul = saturation_mul;
            }
            subclass::Property("saturation-off", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let saturation_off = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing saturation-off from {} to {}",
                    settings.saturation_off,
                    saturation_off
                );
                settings.saturation_off = saturation_off;
            }
            subclass::Property("value-mul", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let value_mul = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing value-mul from {} to {}",
                    settings.value_mul,
                    value_mul
                );
                settings.value_mul = value_mul;
            }
            subclass::Property("value-off", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let value_off = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing value-off from {} to {}",
                    settings.value_off,
                    value_off
                );
                settings.value_off = value_off;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    fn get_property(&self, _obj: &Self::Type, id: usize) -> glib::Value {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("hue-shift", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.hue_shift.to_value()
            }
            subclass::Property("saturation-mul", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.saturation_mul.to_value()
            }
            subclass::Property("saturation-off", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.saturation_off.to_value()
            }
            subclass::Property("value-mul", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.value_mul.to_value()
            }
            subclass::Property("value-off", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.value_off.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for HsvFilter {}

impl BaseTransformImpl for HsvFilter {
    fn get_unit_size(&self, _element: &Self::Type, caps: &gst::Caps) -> Option<usize> {
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
        let _in_info = match gst_video::VideoInfo::from_caps(incaps) {
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

        *self.state.borrow_mut() = Some(State { info: out_info });

        Ok(())
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        // Drop state
        *self.state.borrow_mut() = None;

        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn transform_ip(
        &self,
        element: &Self::Type,
        buf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = *self.settings.lock().unwrap();

        let mut state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(buf, &state.info)
            .map_err(|_| {
                gst::element_error!(
                    element,
                    gst::CoreError::Failed,
                    ["Failed to map output buffer writable"]
                );
                gst::FlowError::Error
            })?;

        let width = frame.width() as usize;
        let stride = frame.plane_stride()[0] as usize;
        let format = frame.format();
        let data = frame.plane_data_mut(0).unwrap();

        assert_eq!(format, gst_video::VideoFormat::Rgbx);
        assert_eq!(data.len() % 4, 0);

        let line_bytes = width * 4;

        for line in data.chunks_exact_mut(stride) {
            for p in line[..line_bytes].chunks_exact_mut(4) {
                assert_eq!(p.len(), 4);

                let mut hsv =
                    hsvutils::from_rgb(p[..3].try_into().expect("slice with incorrect length"));
                hsv[0] = (hsv[0] + settings.hue_shift) % 360.0;
                if hsv[0] < 0.0 {
                    hsv[0] += 360.0;
                }
                hsv[1] = hsvutils::Clamp::clamp(
                    settings.saturation_mul * hsv[1] + settings.saturation_off,
                    0.0,
                    1.0,
                );
                hsv[2] = hsvutils::Clamp::clamp(
                    settings.value_mul * hsv[2] + settings.value_off,
                    0.0,
                    1.0,
                );

                p[..3].copy_from_slice(&hsvutils::to_rgb(&hsv));
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
