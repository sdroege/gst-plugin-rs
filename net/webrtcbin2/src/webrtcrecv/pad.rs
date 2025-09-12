// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;

#[derive(Default)]
pub struct WebRTCRecvSrcPad {}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCRecvSrcPad {
    const NAME: &'static str = "GstWebRTCRecvSrcPad";
    type Type = super::WebRTCRecvSrcPad;
    type ParentType = gst::Pad;
}

impl ObjectImpl for WebRTCRecvSrcPad {}

impl GstObjectImpl for WebRTCRecvSrcPad {}

impl PadImpl for WebRTCRecvSrcPad {}
