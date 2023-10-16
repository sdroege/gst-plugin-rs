// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;

/**
 * SECTION:element-webrtcsink
 * @symbols:
 *   - GstBaseWebRTCSink
 *   - GstRSWebRTCSignallableIface
 *
 * `webrtcsink` is an element that can be used to serve media streams
 * to multiple consumers through WebRTC.
 *
 * It uses a signaller that implements the protocol supported by the default
 * signalling server we additionally provide, take a look at the subclasses of
 * #GstBaseWebRTCSink for other supported protocols, or implement your own.
 *
 * See the [documentation of the plugin](plugin-rswebrtc) for more information
 * on features and usage.
 */

/**
 * GstBaseWebRTCSink:
 * @title: Base class for WebRTC producers
 *
 * Base class for WebRTC sinks to implement and provide their own protocol for.
 */

/**
 * GstRSWebRTCSignallableIface:
 * @title: Interface for WebRTC signalling protocols
 *
 * Interface that WebRTC elements can implement their own protocol with.
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

mod homegrown_cc;

mod imp;

glib::wrapper! {
    pub struct BaseWebRTCSink(ObjectSubclass<imp::BaseWebRTCSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

glib::wrapper! {
    pub struct WebRTCSink(ObjectSubclass<imp::WebRTCSink>) @extends BaseWebRTCSink, gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

glib::wrapper! {
    pub struct AwsKvsWebRTCSink(ObjectSubclass<imp::AwsKvsWebRTCSink>) @extends BaseWebRTCSink, gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

glib::wrapper! {
    pub struct WhipWebRTCSink(ObjectSubclass<imp::WhipWebRTCSink>) @extends BaseWebRTCSink, gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

glib::wrapper! {
    pub struct LiveKitWebRTCSink(ObjectSubclass<imp::LiveKitWebRTCSink>) @extends BaseWebRTCSink, gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

glib::wrapper! {
    pub struct JanusVRWebRTCSink(ObjectSubclass<imp::JanusVRWebRTCSink>) @extends BaseWebRTCSink, gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

#[derive(thiserror::Error, Debug)]
pub enum WebRTCSinkError {
    #[error("no session with id")]
    NoSessionWithId(String),
    #[error("consumer refused media")]
    ConsumerRefusedMedia { session_id: String, media_idx: u32 },
    #[error("consumer did not provide valid payload for media")]
    ConsumerNoValidPayload { session_id: String, media_idx: u32 },
    #[error("SDP mline index is currently mandatory")]
    MandatorySdpMlineIndex,
    #[error("duplicate session id")]
    DuplicateSessionId(String),
    #[error("error setting up consumer pipeline")]
    SessionPipelineError {
        session_id: String,
        peer_id: String,
        details: String,
    },
}

impl Default for BaseWebRTCSink {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl BaseWebRTCSink {
    pub fn with_signaller(signaller: Signallable) -> Self {
        let ret: BaseWebRTCSink = glib::Object::new();

        let ws = ret.imp();
        ws.set_signaller(signaller).unwrap();

        ret
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstWebRTCSinkCongestionControl")]
pub enum WebRTCSinkCongestionControl {
    #[enum_value(name = "Disabled: no congestion control is applied", nick = "disabled")]
    Disabled,
    #[enum_value(name = "Homegrown: simple sender-side heuristic", nick = "homegrown")]
    Homegrown,
    #[enum_value(name = "Google Congestion Control algorithm", nick = "gcc")]
    GoogleCongestionControl,
}

#[glib::flags(name = "GstWebRTCSinkMitigationMode")]
enum WebRTCSinkMitigationMode {
    #[flags_value(name = "No mitigation applied", nick = "none")]
    NONE = 0b00000000,
    #[flags_value(name = "Lowered resolution", nick = "downscaled")]
    DOWNSCALED = 0b00000001,
    #[flags_value(name = "Lowered framerate", nick = "downsampled")]
    DOWNSAMPLED = 0b00000010,
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    BaseWebRTCSink::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    WebRTCSinkCongestionControl::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "webrtcsink",
        gst::Rank::NONE,
        WebRTCSink::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "awskvswebrtcsink",
        gst::Rank::NONE,
        AwsKvsWebRTCSink::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "whipclientsink",
        gst::Rank::NONE,
        WhipWebRTCSink::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "livekitwebrtcsink",
        gst::Rank::NONE,
        LiveKitWebRTCSink::static_type(),
    )?;
    /**
     * element-janusvrwebrtcsink:
     *
     * The `JanusVRWebRTCSink` is a plugin that integrates with the [Video Room plugin](https://janus.conf.meetecho.com/docs/videoroom) of the [Janus Gateway](https://github.com/meetecho/janus-gateway). It basically streams whatever data you pipe to it (video, audio) into WebRTC using Janus as the signaller.
     *
     * ## How to use it
     *
     * You'll need to have:
     *
     * - A Janus server endpoint;
     * - Any WebRTC browser application that uses Janus as the signaller, eg: the `html` folder of [janus-gateway repository](https://github.com/meetecho/janus-gateway).
     *
     * You can pipe the video like this (if you don't happen to run Janus locally, you can set the endpoint
     * like this: `signaller::janus-endpoint=ws://127.0.0.1:8188`):
     *
     * ```bash
     * $ gst-launch-1.0 videotestsrc ! janusvrwebrtcsink signaller::room-id=1234
     * ```
     *
     * And for audio (yes you can do both at the same time, you just need to pipe it properly).
     *
     * ```bash
     * $ gst-launch-1.0 audiotestsrc ! janusvrwebrtcsink signaller::room-id=1234
     * ```
     *
     * And you can set the display name via `signaller::display-name`, eg:
     *
     * ```bash
     * $ gst-launch-1.0 videotestsrc ! janusvrwebrtcsink signaller::room-id=1234 signaller::display-name=ana
     * ```
     *
     * You should see the GStreamer `videotestsrc`/`audiotestsrc` output in your browser now!
     *
     * If for some reason you can't run Janus locally, you can use their open [demo webpage](https://janus.conf.meetecho.com/demos/videoroom.html), and point to its WebSocket server:
     *
     * ```bash
     * $ gst-launch-1.0 videotestsrc ! janusvrwebrtcsink signaller::room-id=1234 signaller::janus-endpoint=wss://janus.conf.meetecho.com/ws
     * ```
     *
     * ## Reference links
     *
     * - [Janus REST/WebSockets docs](https://janus.conf.meetecho.com/docs/rest.html)
     * - [Example implementation in GStreamer](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/269ab858813e670d521cc4b6a71cc0ec4a6e70ed/subprojects/gst-examples/webrtc/janus/rust/src/janus.rs)
     *
     * ## Notes
     *
     * - This plugin supports both the legacy Video Room plugin as well as the `multistream` one;
     * - If you see a warning in the logs related to `rtpgccbwe`, you're probably missing the `gst-plugin-rtp` in your system.
     */
    gst::Element::register(
        Some(plugin),
        "janusvrwebrtcsink",
        gst::Rank::NONE,
        JanusVRWebRTCSink::static_type(),
    )?;

    Ok(())
}
