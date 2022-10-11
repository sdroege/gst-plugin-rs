use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct TestSink(ObjectSubclass<imp::TestSink>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-standalone-test-sink",
        gst::Rank::None,
        TestSink::static_type(),
    )
}
