use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OnvifOverlay(ObjectSubclass<imp::OnvifOverlay>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "onvifoverlay",
        gst::Rank::Primary,
        OnvifOverlay::static_type(),
    )
}
