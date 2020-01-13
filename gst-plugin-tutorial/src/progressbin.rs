// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::gobject_sys;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::Mutex;

// This enum may be used to control what type of output the progressbin should produce.
// It also serves the secondary purpose of illustrating how to add enum-type properties
// to a plugin written in rust.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, GEnum)]
#[repr(u32)]
#[genum(type_name = "GstProgressBinOutput")]
pub(crate) enum ProgressBinOutput {
    #[genum(
        name = "Println: Outputs the progress using a println! macro.",
        nick = "println"
    )]
    Println = 0,
    #[genum(
        name = "Debug Category: Outputs the progress as info logs under the element's debug category.",
        nick = "debug-category"
    )]
    DebugCategory = 1,
}

const DEFAULT_OUTPUT_TYPE: ProgressBinOutput = ProgressBinOutput::Println;

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "progressbin",
        gst::DebugColorFlags::empty(),
        Some("Rust Progress Reporter"),
    );
}

// Struct containing all the element data
struct ProgressBin {
    progress: gst::Element,
    srcpad: gst::GhostPad,
    sinkpad: gst::GhostPad,
    // We put the output_type property behind a mutex, as we want
    // change it in the set_property function, which can be called
    // from any thread.
    output_type: Mutex<ProgressBinOutput>,
}

// Metadata for the element's properties
static PROPERTIES: [subclass::Property; 1] = [subclass::Property("output", |name| {
    glib::ParamSpec::enum_(
        name,
        "Output",
        "Defines the output type of the progressbin",
        ProgressBinOutput::static_type(),
        DEFAULT_OUTPUT_TYPE as i32,
        glib::ParamFlags::READWRITE,
    )
})];

// This trait registers our type with the GObject object system and
// provides the entry points for creating a new instance and setting
// up the class data
impl ObjectSubclass for ProgressBin {
    const NAME: &'static str = "RsProgressBin";
    type ParentType = gst::Bin;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    // This macro provides some boilerplate.
    glib_object_subclass!();

    // Called when a new instance is to be created. We need to return an instance
    // of our struct here and also get the class struct passed in case it's needed
    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        // Create our two ghostpads from the templates that were registered with
        // the class. We don't provide a target for them yet because we can only
        // do so after the progressreport element was added to the bin.
        //
        // We do that and adding the pads inside glib::Object::constructed() later.
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::GhostPad::new_no_target_from_template(Some("sink"), &templ).unwrap();
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::GhostPad::new_no_target_from_template(Some("src"), &templ).unwrap();

        // Create the progressreport element.
        let progress = gst::ElementFactory::make("progressreport", Some("progress")).unwrap();
        // Don't let progressreport print to stdout itself
        progress.set_property("silent", &true).unwrap();

        // Return an instance of our struct
        Self {
            progress,
            srcpad,
            sinkpad,
            output_type: Mutex::new(ProgressBinOutput::Println),
        }
    }

    // Called exactly once when registering the type. Used for
    // setting up metadata for all instances, e.g. the name and
    // classification and the pad templates with their caps.
    //
    // Actual instances can create pads based on those pad templates
    // with a subset of the caps given here.
    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        // Set the element specific metadata. This information is what
        // is visible from gst-inspect-1.0 and can also be programatically
        // retrieved from the gst::Registry after initial registration
        // without having to load the plugin in memory.
        klass.set_metadata(
            "ProgressBin",
            "Generic",
            "Prints progress information to stdout",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        // Create and add pad templates for our sink and source pad. These
        // are later used for actually creating the pads and beforehand
        // already provide information to GStreamer about all possible
        // pads that could exist for this type.

        // Our element can accept any possible caps on both pads
        let caps = gst::Caps::new_any();
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
    }
}

// Implementation of glib::Object virtual methods
impl ObjectImpl for ProgressBin {
    // This macro provides some boilerplate
    glib_object_impl!();

    // Called whenever a value of a property is changed. It can be called
    // at any time from any thread.
    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        let element = obj.downcast_ref::<gst_base::BaseTransform>().unwrap();

        match *prop {
            subclass::Property("output", ..) => {
                let mut output_type = self.output_type.lock().unwrap();
                let new_output_type = value
                    .get_some::<ProgressBinOutput>()
                    .expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing output from {:?} to {:?}",
                    output_type,
                    new_output_type
                );
                *output_type = new_output_type;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("output", ..) => {
                let output_type = self.output_type.lock().unwrap();
                Ok(output_type.to_value())
            }
            _ => unimplemented!(),
        }
    }

    // Called right after construction of a new instance
    fn constructed(&self, obj: &glib::Object) {
        // Call the parent class' ::constructed() implementation first
        self.parent_constructed(obj);

        // Here we actually add the pads we created in ProgressBin::new() to the
        // element so that GStreamer is aware of their existence.
        let bin = obj.downcast_ref::<gst::Bin>().unwrap();

        // Add the progressreport element to the bin.
        bin.add(&self.progress).unwrap();

        // Then set the ghost pad targets to the corresponding pads of the progressreport element.
        self.sinkpad
            .set_target(Some(&self.progress.get_static_pad("sink").unwrap()))
            .unwrap();
        self.srcpad
            .set_target(Some(&self.progress.get_static_pad("src").unwrap()))
            .unwrap();

        // And finally add the two ghostpads to the bin.
        bin.add_pad(&self.sinkpad).unwrap();
        bin.add_pad(&self.srcpad).unwrap();
    }
}

// Implementation of gst::Element virtual methods
impl ElementImpl for ProgressBin {}

// Implementation of gst::Bin virtual methods
impl BinImpl for ProgressBin {
    fn handle_message(&self, bin: &gst::Bin, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            // If this is the progressreport message, we print the status
            // to stdout. Otherwise we pass through to the default message
            // handling of the parent class, i.e. forwarding to the parent
            // bins and the application.
            MessageView::Element(ref msg)
                if msg.get_src().as_ref() == Some(self.progress.upcast_ref())
                    && msg
                        .get_structure()
                        .map(|s| s.get_name() == "progress")
                        .unwrap_or(false) =>
            {
                let s = msg.get_structure().unwrap();
                if let Ok(percent) = s.get_some::<f64>("percent-double") {
                    let output_type = self.output_type.lock().unwrap();
                    match *output_type {
                        ProgressBinOutput::Println => println!("progress: {:5.1}%", percent),
                        ProgressBinOutput::DebugCategory => {
                            gst_info!(CAT, "progress: {:5.1}%", percent);
                        }
                    };
                }
            }
            _ => self.parent_handle_message(bin, msg),
        }
    }
}

// Registers the type for our element, and then registers in GStreamer under
// the name "rsprogressbin" for being able to instantiate it via e.g.
// gst::ElementFactory::make().
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsprogressbin",
        gst::Rank::None,
        ProgressBin::get_type(),
    )
}
