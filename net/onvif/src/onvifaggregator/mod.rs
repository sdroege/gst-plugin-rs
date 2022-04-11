use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OnvifAggregator(ObjectSubclass<imp::OnvifAggregator>) @extends gst_base::Aggregator, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "onvifaggregator",
        gst::Rank::Primary,
        OnvifAggregator::static_type(),
    )
}
