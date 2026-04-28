use gst::glib;

mod imp;

pub const STAMP: u8 = 42;

glib::wrapper! {
    pub struct Stamper(ObjectSubclass<imp::Stamper>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

impl Default for Stamper {
    fn default() -> Self {
        glib::Object::new()
    }
}

glib::wrapper! {
    pub struct StampChecker(ObjectSubclass<imp::StampChecker>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

impl Default for StampChecker {
    fn default() -> Self {
        glib::Object::new()
    }
}
