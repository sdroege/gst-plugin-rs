// SPDX-License-Identifier: MPL-2.0
/**
 * SECTION:element-webrtcsrc
 * @symbols:
 *   - GstWebRTCSrcPad
 *
 * `webrtcsrc` is the source counterpart of the #webrtcsink element and can be
 * used to receive streams from it, it can also be used to easily playback WebRTC
 * streams coming from a web browser.
 *
 * To try the element, you should run #webrtcsink as described in its documentation,
 * finding its `peer-id` (in the signalling server logs for example) and then
 * run:
 *
 * ``` bash
 * gst-launch-1.0 webrtcsrc signaller::producer-peer-id=<webrtcsink-peer-id> ! videoconvert ! autovideosink
 * ```
 *
 * or directly using `playbin`:
 *
 * ``` bash
 * gst-launch-1.0 playbin3 uri="gstwebrtc://localhost:8443?peer-id=<webrtcsink-peer-id>"
 * ```
 *
 * ## Decoding
 *
 * To be able to precisely negotiate the WebRTC SDP, `webrtcsrc` is able to decode streams.
 * During SDP negotiation we expose our pads based on the peer offer and right after query caps
 * to see what downstream supports.
 * In practice in `uridecodebinX` or `playbinX`, decoding will happen
 * in `decodebinX` but for the case where a `videoconvert` is placed after a `video_XX` pad,
 * decoding will happen inside `webrtcsrc`.
 *
 * Since: 0.10
 */
mod imp;
mod pad;

use crate::signaller::Signallable;
use crate::signaller::WebRTCSignallerRole;
use gst::prelude::*;
use gst::{glib, prelude::StaticType};

glib::wrapper! {
    pub struct BaseWebRTCSrc(ObjectSubclass<imp::BaseWebRTCSrc>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub struct WebRTCSrc(ObjectSubclass<imp::WebRTCSrc>) @extends BaseWebRTCSrc, gst::Bin, gst::Element, gst::Object, @implements gst::URIHandler, gst::ChildProxy;
}

glib::wrapper! {
    pub struct WebRTCSrcPad(ObjectSubclass<pad::WebRTCSrcPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    BaseWebRTCSrc::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    WebRTCSignallerRole::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    WebRTCSrcPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    Signallable::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        plugin,
        "webrtcsrc",
        gst::Rank::PRIMARY,
        WebRTCSrc::static_type(),
    )
}
