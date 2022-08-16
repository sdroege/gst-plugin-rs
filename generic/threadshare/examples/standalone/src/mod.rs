use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct TestSrc(ObjectSubclass<imp::TestSrc>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-standalone-test-src",
        gst::Rank::None,
        TestSrc::static_type(),
    )
}
