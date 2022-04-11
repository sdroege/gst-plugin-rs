use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OnvifPay(ObjectSubclass<imp::OnvifPay>) @extends gst_rtp::RTPBasePayload, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtponvifpay",
        gst::Rank::Primary,
        OnvifPay::static_type(),
    )
}
