// Copyright (C) 2017,2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base;
use gst_base::subclass::prelude::*;
use gst_video;

use std::i32;
use std::sync::Mutex;

use once_cell::sync::Lazy;

// Default values of properties
const DEFAULT_INVERT: bool = false;
const DEFAULT_SHIFT: u32 = 0;

// Property value storage
#[derive(Debug, Clone, Copy)]
struct Settings {
    invert: bool,
    shift: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            invert: DEFAULT_INVERT,
            shift: DEFAULT_SHIFT,
        }
    }
}

// Metadata for the properties
static PROPERTIES: [subclass::Property; 2] = [
    subclass::Property("invert", |name| {
        glib::ParamSpec::boolean(
            name,
            "Invert",
            "Invert grayscale output",
            DEFAULT_INVERT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("shift", |name| {
        glib::ParamSpec::uint(
            name,
            "Shift",
            "Shift grayscale output (wrapping around)",
            0,
            255,
            DEFAULT_SHIFT,
            glib::ParamFlags::READWRITE,
        )
    }),
];

// Stream-specific state, i.e. video format configuration
struct State {
    in_info: gst_video::VideoInfo,
    out_info: gst_video::VideoInfo,
}

// Struct containing all the element data
struct Rgb2Gray {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rsrgb2gray",
        gst::DebugColorFlags::empty(),
        Some("Rust RGB-GRAY converter"),
    )
});

impl Rgb2Gray {
    // Converts one pixel of BGRx to a grayscale value, shifting and/or
    // inverting it as configured
    #[inline]
    fn bgrx_to_gray(in_p: &[u8], shift: u8, invert: bool) -> u8 {
        // See https://en.wikipedia.org/wiki/YUV#SDTV_with_BT.601
        const R_Y: u32 = 19595; // 0.299 * 65536
        const G_Y: u32 = 38470; // 0.587 * 65536
        const B_Y: u32 = 7471; // 0.114 * 65536

        assert_eq!(in_p.len(), 4);

        let b = u32::from(in_p[0]);
        let g = u32::from(in_p[1]);
        let r = u32::from(in_p[2]);

        let gray = ((r * R_Y) + (g * G_Y) + (b * B_Y)) / 65536;
        let gray = (gray as u8).wrapping_add(shift);

        if invert {
            255 - gray
        } else {
            gray
        }
    }
}

// This trait registers our type with the GObject object system and
// provides the entry points for creating a new instance and setting
// up the class data
impl ObjectSubclass for Rgb2Gray {
    const NAME: &'static str = "RsRgb2Gray";
    type ParentType = gst_base::BaseTransform;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    // This macro provides some boilerplate
    glib_object_subclass!();

    // Called when a new instance is to be created. We need to return an instance
    // of our struct here.
    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(None),
        }
    }

    // Called exactly once when registering the type. Used for
    // setting up metadata for all instances, e.g. the name and
    // classification and the pad templates with their caps.
    //
    // Actual instances can create pads based on those pad templates
    // with a subset of the caps given here. In case of basetransform,
    // a "src" and "sink" pad template are required here and the base class
    // will automatically instantiate pads for them.
    //
    // Our element here can convert BGRx to BGRx or GRAY8, both being grayscale.
    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        // Set the element specific metadata. This information is what
        // is visible from gst-inspect-1.0 and can also be programatically
        // retrieved from the gst::Registry after initial registration
        // without having to load the plugin in memory.
        klass.set_metadata(
            "RGB-GRAY Converter",
            "Filter/Effect/Converter/Video",
            "Converts RGB to GRAY or grayscale RGB",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        // Create and add pad templates for our sink and source pad. These
        // are later used for actually creating the pads and beforehand
        // already provide information to GStreamer about all possible
        // pads that could exist for this type.

        // On the src pad, we can produce BGRx and GRAY8 of any
        // width/height and with any framerate
        let caps = gst::Caps::new_simple(
            "video/x-raw",
            &[
                (
                    "format",
                    &gst::List::new(&[
                        &gst_video::VideoFormat::Bgrx.to_str(),
                        &gst_video::VideoFormat::Gray8.to_str(),
                    ]),
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
        // The src pad template must be named "src" for basetransform
        // and specific a pad that is always there
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        // On the sink pad, we can accept BGRx of any
        // width/height and with any framerate
        let caps = gst::Caps::new_simple(
            "video/x-raw",
            &[
                ("format", &gst_video::VideoFormat::Bgrx.to_str()),
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
        // The sink pad template must be named "sink" for basetransform
        // and specific a pad that is always there
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

        // Configure basetransform so that we are never running in-place,
        // don't passthrough on same caps and also never call transform_ip
        // in passthrough mode (which does not matter for us here).
        //
        // We could work in-place for BGRx->BGRx but don't do here for simplicity
        // for now.
        klass.configure(
            gst_base::subclass::BaseTransformMode::NeverInPlace,
            false,
            false,
        );
    }
}

// Implementation of glib::Object virtual methods
impl ObjectImpl for Rgb2Gray {
    // This macro provides some boilerplate.
    glib_object_impl!();

    // Called whenever a value of a property is changed. It can be called
    // at any time from any thread.
    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        let element = obj.downcast_ref::<gst_base::BaseTransform>().unwrap();

        match *prop {
            subclass::Property("invert", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let invert = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing invert from {} to {}",
                    settings.invert,
                    invert
                );
                settings.invert = invert;
            }
            subclass::Property("shift", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let shift = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing shift from {} to {}",
                    settings.shift,
                    shift
                );
                settings.shift = shift;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("invert", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.invert.to_value())
            }
            subclass::Property("shift", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.shift.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

// Implementation of gst::Element virtual methods
impl ElementImpl for Rgb2Gray {}

// Implementation of gst_base::BaseTransform virtual methods
impl BaseTransformImpl for Rgb2Gray {
    // Called for converting caps from one pad to another to account for any
    // changes in the media format this element is performing.
    //
    // In our case that means that:
    fn transform_caps(
        &self,
        element: &gst_base::BaseTransform,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let other_caps = if direction == gst::PadDirection::Src {
            // For src to sink, no matter if we get asked for BGRx or GRAY8 caps, we can only
            // accept corresponding BGRx caps on the sinkpad. We will only ever get BGRx and GRAY8
            // caps here as input.
            let mut caps = caps.clone();

            for s in caps.make_mut().iter_mut() {
                s.set("format", &gst_video::VideoFormat::Bgrx.to_str());
            }

            caps
        } else {
            // For the sink to src case, we will only get BGRx caps and for each of them we could
            // output the same caps or the same caps as GRAY8. We prefer GRAY8 (put it first), and
            // at a later point the caps negotiation mechanism of GStreamer will decide on which
            // one to actually produce.
            let mut gray_caps = gst::Caps::new_empty();

            {
                let gray_caps = gray_caps.get_mut().unwrap();

                for s in caps.iter() {
                    let mut s_gray = s.to_owned();
                    s_gray.set("format", &gst_video::VideoFormat::Gray8.to_str());
                    gray_caps.append_structure(s_gray);
                }
                gray_caps.append(caps.clone());
            }

            gray_caps
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

    // Returns the size of one processing unit (i.e. a frame in our case) corresponding
    // to the given caps. This is used for allocating a big enough output buffer and
    // sanity checking the input buffer size, among other things.
    fn get_unit_size(&self, _element: &gst_base::BaseTransform, caps: &gst::Caps) -> Option<usize> {
        gst_video::VideoInfo::from_caps(caps)
            .map(|info| info.size())
            .ok()
    }

    // Called whenever the input/output caps are changing, i.e. in the very beginning before data
    // flow happens and whenever the situation in the pipeline is changing. All buffers after this
    // call have the caps given here.
    //
    // We simply remember the resulting VideoInfo from the caps to be able to use this for knowing
    // the width, stride, etc when transforming buffers
    fn set_caps(
        &self,
        element: &gst_base::BaseTransform,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        let in_info = match gst_video::VideoInfo::from_caps(incaps) {
            Err(_) => return Err(gst_loggable_error!(CAT, "Failed to parse input caps")),
            Ok(info) => info,
        };
        let out_info = match gst_video::VideoInfo::from_caps(outcaps) {
            Err(_) => return Err(gst_loggable_error!(CAT, "Failed to parse output caps")),
            Ok(info) => info,
        };

        gst_debug!(
            CAT,
            obj: element,
            "Configured for caps {} to {}",
            incaps,
            outcaps
        );

        *self.state.lock().unwrap() = Some(State { in_info, out_info });

        Ok(())
    }

    // Called when shutting down the element so we can release all stream-related state
    // There's also start(), which is called whenever starting the element again
    fn stop(&self, element: &gst_base::BaseTransform) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.lock().unwrap().take();

        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    // Does the actual transformation of the input buffer to the output buffer
    fn transform(
        &self,
        element: &gst_base::BaseTransform,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Keep a local copy of the values of all our properties at this very moment. This
        // ensures that the mutex is never locked for long and the application wouldn't
        // have to block until this function returns when getting/setting property values
        let settings = *self.settings.lock().unwrap();

        // Get a locked reference to our state, i.e. the input and output VideoInfo
        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or_else(|| {
            gst_element_error!(element, gst::CoreError::Negotiation, ["Have no state yet"]);
            gst::FlowError::NotNegotiated
        })?;

        // Map the input buffer as a VideoFrameRef. This is similar to directly mapping
        // the buffer with inbuf.map_readable() but in addition extracts various video
        // specific metadata and sets up a convenient data structure that directly gives
        // pointers to the different planes and has all the information about the raw
        // video frame, like width, height, stride, video format, etc.
        //
        // This fails if the buffer can't be read or is invalid in relation to the video
        // info that is passed here
        let in_frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(inbuf.as_ref(), &state.in_info)
                .map_err(|_| {
                    gst_element_error!(
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
                    gst_element_error!(
                        element,
                        gst::CoreError::Failed,
                        ["Failed to map output buffer writable"]
                    );
                    gst::FlowError::Error
                },
            )?;

        // Keep the various metadata we need for working with the video frames in
        // local variables. This saves some typing below.
        let width = in_frame.width() as usize;
        let in_stride = in_frame.plane_stride()[0] as usize;
        let in_data = in_frame.plane_data(0).unwrap();
        let out_stride = out_frame.plane_stride()[0] as usize;
        let out_format = out_frame.format();
        let out_data = out_frame.plane_data_mut(0).unwrap();

        // First check the output format. Our input format is always BGRx but the output might
        // be BGRx or GRAY8. Based on what it is we need to do processing slightly differently.
        if out_format == gst_video::VideoFormat::Bgrx {
            // Some assertions about our assumptions how the data looks like. This is only there
            // to give some further information to the compiler, in case these can be used for
            // better optimizations of the resulting code.
            //
            // If any of the assertions were not true, the code below would fail cleanly.
            assert_eq!(in_data.len() % 4, 0);
            assert_eq!(out_data.len() % 4, 0);
            assert_eq!(out_data.len() / out_stride, in_data.len() / in_stride);

            let in_line_bytes = width * 4;
            let out_line_bytes = width * 4;

            assert!(in_line_bytes <= in_stride);
            assert!(out_line_bytes <= out_stride);

            // Iterate over each line of the input and output frame, mutable for the output frame.
            // Each input line has in_stride bytes, each output line out_stride. We use the
            // chunks_exact/chunks_exact_mut iterators here for getting a chunks of that many bytes per
            // iteration and zip them together to have access to both at the same time.
            for (in_line, out_line) in in_data
                .chunks_exact(in_stride)
                .zip(out_data.chunks_exact_mut(out_stride))
            {
                // Next iterate the same way over each actual pixel in each line. Every pixel is 4
                // bytes in the input and output, so we again use the chunks_exact/chunks_exact_mut iterators
                // to give us each pixel individually and zip them together.
                //
                // Note that we take a sub-slice of the whole lines: each line can contain an
                // arbitrary amount of padding at the end (e.g. for alignment purposes) and we
                // don't want to process that padding.
                for (in_p, out_p) in in_line[..in_line_bytes]
                    .chunks_exact(4)
                    .zip(out_line[..out_line_bytes].chunks_exact_mut(4))
                {
                    assert_eq!(out_p.len(), 4);

                    // Use our above-defined function to convert a BGRx pixel with the settings to
                    // a grayscale value. Then store the same value in the red/green/blue component
                    // of the pixel.
                    let gray = Rgb2Gray::bgrx_to_gray(in_p, settings.shift as u8, settings.invert);
                    out_p[0] = gray;
                    out_p[1] = gray;
                    out_p[2] = gray;
                }
            }
        } else if out_format == gst_video::VideoFormat::Gray8 {
            assert_eq!(in_data.len() % 4, 0);
            assert_eq!(out_data.len() / out_stride, in_data.len() / in_stride);

            let in_line_bytes = width * 4;
            let out_line_bytes = width;

            assert!(in_line_bytes <= in_stride);
            assert!(out_line_bytes <= out_stride);

            // Iterate over each line of the input and output frame, mutable for the output frame.
            // Each input line has in_stride bytes, each output line out_stride. We use the
            // chunks_exact/chunks_exact_mut iterators here for getting a chunks of that many bytes per
            // iteration and zip them together to have access to both at the same time.
            for (in_line, out_line) in in_data
                .chunks_exact(in_stride)
                .zip(out_data.chunks_exact_mut(out_stride))
            {
                // Next iterate the same way over each actual pixel in each line. Every pixel is 4
                // bytes in the input and 1 byte in the output, so we again use the
                // chunks_exact/chunks_exact_mut iterators to give us each pixel individually and zip them
                // together.
                //
                // Note that we take a sub-slice of the whole lines: each line can contain an
                // arbitrary amount of padding at the end (e.g. for alignment purposes) and we
                // don't want to process that padding.
                for (in_p, out_p) in in_line[..in_line_bytes]
                    .chunks_exact(4)
                    .zip(out_line[..out_line_bytes].iter_mut())
                {
                    // Use our above-defined function to convert a BGRx pixel with the settings to
                    // a grayscale value. Then store the value in the grayscale output directly.
                    let gray = Rgb2Gray::bgrx_to_gray(in_p, settings.shift as u8, settings.invert);
                    *out_p = gray;
                }
            }
        } else {
            unimplemented!();
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

// Registers the type for our element, and then registers in GStreamer under
// the name "rsrgb2gray" for being able to instantiate it via e.g.
// gst::ElementFactory::make().
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsrgb2gray",
        gst::Rank::None,
        Rgb2Gray::get_type(),
    )
}
