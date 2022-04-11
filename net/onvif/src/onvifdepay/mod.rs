use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OnvifDepay(ObjectSubclass<imp::OnvifDepay>) @extends gst_rtp::RTPBaseDepayload, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtponvifdepay",
        gst::Rank::Primary,
        OnvifDepay::static_type(),
    )
}
