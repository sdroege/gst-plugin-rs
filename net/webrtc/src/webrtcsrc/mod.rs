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
 * or directly using `playbin3`:
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

#[cfg(feature = "whip")]
glib::wrapper! {
    pub struct WhipServerSrc(ObjectSubclass<imp::whip::WhipServerSrc>) @extends BaseWebRTCSrc, gst::Bin, gst::Element, gst::Object, @implements gst::URIHandler, gst::ChildProxy;
}

#[cfg(feature = "livekit")]
glib::wrapper! {
    pub struct LiveKitWebRTCSrc(ObjectSubclass<imp::livekit::LiveKitWebRTCSrc>) @extends BaseWebRTCSrc, gst::Bin, gst::Element, gst::Object, gst::ChildProxy;
}

glib::wrapper! {
    pub struct WebRTCSrcPad(ObjectSubclass<pad::WebRTCSrcPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

#[cfg(feature = "livekit")]
glib::wrapper! {
    pub struct LiveKitWebRTCSrcPad(ObjectSubclass<imp::livekit::LiveKitWebRTCSrcPad>) @extends WebRTCSrcPad, gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    BaseWebRTCSrc::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    WebRTCSignallerRole::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    WebRTCSrcPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    #[cfg(feature = "livekit")]
    LiveKitWebRTCSrcPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    Signallable::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        plugin,
        "webrtcsrc",
        gst::Rank::PRIMARY,
        WebRTCSrc::static_type(),
    )?;

    #[cfg(feature = "whip")]
    gst::Element::register(
        plugin,
        "whipserversrc",
        gst::Rank::PRIMARY,
        WhipServerSrc::static_type(),
    )?;

    #[cfg(feature = "livekit")]
    /**
     * element-livekitwebrtcsrc:
     *
     * The `livekitwebrtcsrc` plays streams from a LiveKit room.
     *
     * The element can either subscribe to the streams published by a single
     * peer in the room using the same `signaller::producer-peer-id` child
     * property that other webrtcsrc elements use or auto-subscribe to all peers
     * in a room by not specifying anything for that property. When in
     * auto-subscribe mode, you can use
     * `signaller::excluded-producer-peer-ids=<a,b,c>` to ignore peers `a`, `b`,
     * and `c` while subscribing to all other members of the room.
     *
     * ## Sample Pipeline
     *
     * First, start the livekit server with the `--dev` flag to enable the test credentials.
     *
     * Next, publish a stream:
     *
     * ```shell
     * gst-launch-1.0 \
     * videotestsrc is-live=1 \
     * ! video/x-raw,width=640,height=360,framerate=15/1 \
     * ! timeoverlay ! videoconvert ! queue \
     * ! livekitwebrtcsink name=sink \
     *     signaller::ws-url=ws://127.0.0.1:7880 \
     *     signaller::api-key=devkey \
     *     signaller::secret-key=secret \
     *     signaller::room-name=testroom \
     *     signaller::identity=gst-producer \
     *     signaller::participant-name=gst-producer \
     *     video-caps='video/x-vp8'
     * ```
     *
     * Finally, watch the stream:
     *
     * ```shell
     * gst-launch-1.0 \
     * livekitwebrtcsrc \
     *   signaller::ws-url=ws://127.0.0.1:7880 \
     *   signaller::api-key=devkey \
     *   signaller::secret-key=secret \
     *   signaller::room-name=testroom \
     *   signaller::identity=gst-consumer \
     *   signaller::participant-name=gst-consumer \
     * ! queue ! videoconvert ! autovideosink
     * ```
     */
    gst::Element::register(
        plugin,
        "livekitwebrtcsrc",
        gst::Rank::NONE,
        LiveKitWebRTCSrc::static_type(),
    )?;

    Ok(())
}
