// Copyright (C) 2022, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::GstRsWebRTCICETransportPolicy;
use futures::future;
use futures::prelude::*;
use tokio::runtime;

use gst::{element_imp_error, glib, prelude::*, subclass::prelude::*, ErrorMessage};
use gst_sdp::*;
use gst_webrtc::*;

use bytes::Bytes;
use once_cell::sync::Lazy;
use parse_link_header;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::StatusCode;

use std::fmt::Display;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "whepsrc",
        gst::DebugColorFlags::empty(),
        Some("WHEP Source"),
    )
});

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

const DEFAULT_ICE_TRANSPORT_POLICY: GstRsWebRTCICETransportPolicy =
    GstRsWebRTCICETransportPolicy::All;
const MAX_REDIRECTS: u8 = 10;

#[derive(Debug, Clone)]
struct Settings {
    video_caps: gst::Caps,
    audio_caps: gst::Caps,
    turn_server: Option<String>,
    stun_server: Option<String>,
    whep_endpoint: Option<String>,
    auth_token: Option<String>,
    use_link_headers: bool,
    ice_transport_policy: GstRsWebRTCICETransportPolicy,
}

#[allow(clippy::derivable_impls)]
impl Default for Settings {
    fn default() -> Self {
        Self {
            video_caps: [
                "video/x-vp8",
                "video/x-h264",
                "video/x-vp9",
                "video/x-h265",
                "video/x-av1",
            ]
            .iter()
            .map(|s| gst::Structure::new_empty(s))
            .collect::<gst::Caps>(),
            audio_caps: ["audio/x-opus"]
                .iter()
                .map(|s| gst::Structure::new_empty(s))
                .collect::<gst::Caps>(),
            stun_server: None,
            turn_server: None,
            whep_endpoint: None,
            auth_token: None,
            use_link_headers: false,
            ice_transport_policy: DEFAULT_ICE_TRANSPORT_POLICY,
        }
    }
}

#[derive(Debug, Clone)]
enum State {
    Stopped,
    Post { redirects: u8 },
    Running { whep_resource: String },
}

impl Default for State {
    fn default() -> Self {
        Self::Stopped
    }
}

pub struct WhepSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    webrtcbin: gst::Element,
    canceller: Mutex<Option<future::AbortHandle>>,
    client: reqwest::Client,
}

impl Default for WhepSrc {
    fn default() -> Self {
        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .build()
            .expect("Failed to create webrtcbin");

        // We'll handle redirects manually since the default redirect handler does not
        // reuse the authentication token on the redirected server
        let pol = reqwest::redirect::Policy::none();
        let client = build_reqwest_client(pol);

        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            webrtcbin,
            canceller: Mutex::new(None),
            client,
        }
    }
}

impl GstObjectImpl for WhepSrc {}

impl ElementImpl for WhepSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WHEP Source Bin",
                "Source/Network/WebRTC",
                "A bin to stream media using the WebRTC HTTP Egress Protocol (WHEP)",
                "Sanchayan Maity <sanchayan@asymptotic.io>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_empty_simple("application/x-rtp"),
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::NullToReady {
            /*
             * Fail the state change if WHEP endpoint has not been set by the
             * time ReadyToPaused transition happens. This prevents us from
             * having to check this everywhere else.
             */
            let settings = self.settings.lock().unwrap();

            if settings.whep_endpoint.is_none() {
                gst::error!(CAT, imp: self, "WHEP endpoint URL must be set");
                return Err(gst::StateChangeError);
            }

            /*
             * Check if we have a valid URL. We can be assured any further URL
             * handling won't fail due to invalid URLs.
             */
            if let Err(e) = reqwest::Url::parse(settings.whep_endpoint.as_ref().unwrap().as_str()) {
                gst::error!(
                    CAT,
                    imp: self,
                    "WHEP endpoint URL could not be parsed: {}",
                    e
                );
                return Err(gst::StateChangeError);
            }

            drop(settings);
        }

        if transition == gst::StateChange::PausedToReady {
            if let Some(canceller) = &*self.canceller.lock().unwrap() {
                canceller.abort();
            }

            let state = self.state.lock().unwrap();
            if let State::Running { .. } = *state {
                drop(state);
                self.terminate_session();
            }

            for pad in self.obj().src_pads() {
                gst::debug!(CAT, imp: self, "Removing pad: {}", pad.name());

                // No need to deactivate pad here. Parent GstBin will deactivate
                // the pad. Only remove the pad.
                if let Err(e) = self.obj().remove_pad(&pad) {
                    gst::error!(CAT, imp: self, "Failed to remove pad {}: {}", pad.name(), e);
                }
            }
        }

        let ret = self.parent_change_state(transition);

        ret
    }
}

impl ObjectImpl for WhepSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecBoxed::builder::<gst::Caps>("video-caps")
                    .nick("Video caps")
                    .blurb("Governs what video codecs will be proposed")
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("audio-caps")
                    .nick("Audio caps")
                    .blurb("Governs what audio codecs will be proposed")
                    .build(),
                glib::ParamSpecString::builder("stun-server")
                    .nick("STUN Server")
                    .blurb("The STUN server of the form stun://hostname:port")
                    .build(),
                glib::ParamSpecString::builder("turn-server")
                    .nick("TURN Server")
                    .blurb("The TURN server of the form turn(s)://username:password@host:port.")
                    .build(),
                glib::ParamSpecString::builder("whep-endpoint")
                    .nick("WHEP Endpoint")
                    .blurb("The WHEP server endpoint to POST SDP offer to.")
                    .build(),
                glib::ParamSpecBoolean::builder("use-link-headers")
                    .nick("Use Link Headers")
                    .blurb("Use link headers to configure STUN/TURN servers if present in WHEP endpoint response.")
                    .build(),
                glib::ParamSpecString::builder("auth-token")
                    .nick("Authorization Token")
                    .blurb("Authentication token to use, will be sent in the HTTP Header as 'Bearer <auth-token>'")
                    .build(),
                glib::ParamSpecEnum::builder::<GstRsWebRTCICETransportPolicy>("ice-transport-policy", DEFAULT_ICE_TRANSPORT_POLICY)
                    .nick("ICE transport policy")
                    .blurb("The policy to apply for ICE transport")
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "video-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.video_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_empty);
            }
            "audio-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.audio_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_empty);
            }
            "stun-server" => {
                let mut settings = self.settings.lock().unwrap();
                settings.stun_server = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                self.webrtcbin
                    .set_property("stun-server", settings.stun_server.as_ref());
            }
            "turn-server" => {
                let mut settings = self.settings.lock().unwrap();
                settings.turn_server = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                self.webrtcbin
                    .set_property("turn-server", settings.turn_server.as_ref());
            }
            "whep-endpoint" => {
                let mut settings = self.settings.lock().unwrap();
                settings.whep_endpoint = value.get().expect("WHEP endpoint should be a string");
            }
            "use-link-headers" => {
                let mut settings = self.settings.lock().unwrap();
                settings.use_link_headers = value
                    .get()
                    .expect("use-link-headers should be a boolean value");
            }
            "auth-token" => {
                let mut settings = self.settings.lock().unwrap();
                settings.auth_token = value.get().expect("Auth token should be a string");
            }
            "ice-transport-policy" => {
                let mut settings = self.settings.lock().unwrap();
                settings.ice_transport_policy = value
                    .get::<GstRsWebRTCICETransportPolicy>()
                    .expect("ice-transport-policy should be an enum value");

                if settings.ice_transport_policy == GstRsWebRTCICETransportPolicy::Relay {
                    self.webrtcbin
                        .set_property_from_str("ice-transport-policy", "relay");
                } else {
                    self.webrtcbin
                        .set_property_from_str("ice-transport-policy", "all");
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "video-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.video_caps.to_value()
            }
            "audio-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.audio_caps.to_value()
            }
            "stun-server" => {
                let settings = self.settings.lock().unwrap();
                settings.stun_server.to_value()
            }
            "turn-server" => {
                let settings = self.settings.lock().unwrap();
                settings.turn_server.to_value()
            }
            "whep-endpoint" => {
                let settings = self.settings.lock().unwrap();
                settings.whep_endpoint.to_value()
            }
            "use-link-headers" => {
                let settings = self.settings.lock().unwrap();
                settings.use_link_headers.to_value()
            }
            "auth-token" => {
                let settings = self.settings.lock().unwrap();
                settings.auth_token.to_value()
            }
            "ice-transport-policy" => {
                let settings = self.settings.lock().unwrap();
                settings.ice_transport_policy.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        self.obj()
            .set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        self.obj().set_element_flags(gst::ElementFlags::SOURCE);

        self.setup_webrtcbin();

        self.obj().add(&self.webrtcbin).unwrap();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WhepSrc {
    const NAME: &'static str = "GstWhepSrc";
    type Type = super::WhepSrc;
    type ParentType = gst::Bin;
}

impl BinImpl for WhepSrc {
    fn handle_message(&self, message: gst::Message) {
        use gst::MessageView;
        match message.view() {
            MessageView::Eos(_) | MessageView::Error(_) => {
                self.terminate_session();
                self.parent_handle_message(message)
            }
            _ => self.parent_handle_message(message),
        }
    }
}

impl WhepSrc {
    fn setup_webrtcbin(&self) {
        // The specification requires all m= lines to be bundled (section 4.5)
        self.webrtcbin
            .set_property("bundle-policy", WebRTCBundlePolicy::MaxBundle);

        let self_weak = self.downgrade();
        self.webrtcbin
            .connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
                let self_ = match self_weak.upgrade() {
                    Some(self_) => self_,
                    None => return,
                };

                let state = webrtcbin.property::<WebRTCICEGatheringState>("ice-gathering-state");

                match state {
                    WebRTCICEGatheringState::Gathering => {
                        gst::info!(CAT, imp: self_, "ICE gathering started")
                    }
                    WebRTCICEGatheringState::Complete => {
                        gst::info!(CAT, imp: self_, "ICE gathering completed");

                        self_.whep_offer();
                    }
                    _ => (),
                }
            });

        let self_weak = self.downgrade();
        self.webrtcbin
            .connect_notify(Some("ice-connection-state"), move |webrtcbin, _pspec| {
                let self_ = match self_weak.upgrade() {
                    Some(self_) => self_,
                    None => return,
                };

                let state = webrtcbin.property::<WebRTCICEConnectionState>("ice-connection-state");

                match state {
                    WebRTCICEConnectionState::New => (),
                    WebRTCICEConnectionState::Checking => {
                        gst::info!(CAT, imp: self_, "ICE connecting...")
                    }
                    WebRTCICEConnectionState::Connected => {
                        gst::info!(CAT, imp: self_, "ICE connected")
                    }
                    WebRTCICEConnectionState::Completed => {
                        gst::info!(CAT, imp: self_, "ICE completed")
                    }
                    WebRTCICEConnectionState::Failed => {
                        self_.terminate_session();
                        element_imp_error!(self_, gst::ResourceError::Failed, ["ICE failed"]);
                    }
                    WebRTCICEConnectionState::Disconnected => (),
                    WebRTCICEConnectionState::Closed => (),
                    _ => (),
                }
            });

        let self_weak = self.downgrade();
        self.webrtcbin
            .connect_notify(Some("connection-state"), move |webrtcbin, _pspec| {
                let self_ = match self_weak.upgrade() {
                    Some(self_) => self_,
                    None => return,
                };

                let state = webrtcbin.property::<WebRTCPeerConnectionState>("connection-state");

                match state {
                    WebRTCPeerConnectionState::New => (),
                    WebRTCPeerConnectionState::Connecting => {
                        gst::info!(CAT, imp: self_, "PeerConnection connecting...")
                    }
                    WebRTCPeerConnectionState::Connected => {
                        gst::info!(CAT, imp: self_, "PeerConnection connected")
                    }
                    WebRTCPeerConnectionState::Disconnected => (),
                    WebRTCPeerConnectionState::Failed => {
                        self_.terminate_session();
                        element_imp_error!(
                            self_,
                            gst::ResourceError::Failed,
                            ["PeerConnection failed"]
                        );
                    }
                    WebRTCPeerConnectionState::Closed => (),
                    _ => (),
                }
            });

        let self_weak = self.downgrade();
        self.webrtcbin.connect_pad_added(move |_, pad| {
            let self_ = match self_weak.upgrade() {
                Some(self_) => self_,
                None => return,
            };

            gst::debug!(
                CAT,
                imp: self_,
                "Pad added with name: {} and caps: {:?}",
                pad.name(),
                pad.current_caps()
            );

            let templ = self_.obj().pad_template("src_%u").unwrap();
            let src_pad = gst::GhostPad::builder_with_template(&templ, Some(&pad.name()))
                .build_with_target(pad)
                .unwrap();

            src_pad.set_target(Some(pad)).unwrap();
            src_pad
                .set_active(true)
                .expect("Pad activation should succeed");

            self_.obj().add_pad(&src_pad).expect("Failed to add pad");
        });

        let self_weak = self.downgrade();
        self.webrtcbin.connect("on-negotiation-needed", false, {
            move |_| {
                let self_ = match self_weak.upgrade() {
                    Some(self_) => self_,
                    None => return None,
                };

                let settings = self_.settings.lock().unwrap();

                let endpoint =
                    reqwest::Url::parse(settings.whep_endpoint.as_ref().unwrap().as_str());
                if let Err(e) = endpoint {
                    element_imp_error!(
                        self_,
                        gst::ResourceError::Failed,
                        ["Could not parse WHEP endpoint URL :{}", e]
                    );
                    return None;
                }

                drop(settings);

                let mut state = self_.state.lock().unwrap();
                *state = State::Post { redirects: 0 };
                drop(state);

                if let Err(e) = self_.initial_post_request(endpoint.unwrap()) {
                    element_imp_error!(
                        self_,
                        gst::ResourceError::Failed,
                        ["Error in initial post request - {}", e.to_string()]
                    );
                    return None;
                }

                None
            }
        });
    }

    fn sdp_message_parse(&self, sdp_bytes: Bytes) -> Result<(), ErrorMessage> {
        let sdp = sdp_message::SDPMessage::parse_buffer(&sdp_bytes).or_else(|_| {
            Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Could not parse answer SDP"]
            ))
        })?;

        let remote_sdp = WebRTCSessionDescription::new(WebRTCSDPType::Answer, sdp);

        gst::debug!(
            CAT,
            imp: self,
            "Setting remote description: {:?}",
            remote_sdp.sdp().as_text()
        );

        self.webrtcbin.emit_by_name::<()>(
            "set-remote-description",
            &[&remote_sdp, &None::<gst::Promise>],
        );

        for media in remote_sdp.sdp().medias() {
            let c = media.attribute_val("candidate");

            if let Some(candidate) = c {
                let m_line_index = 0u32;
                let c = format!("candidate:{candidate}");

                gst::debug!(CAT, imp: self, "Adding ICE candidate from offer: {:?}", c);

                self.webrtcbin
                    .emit_by_name::<()>("add-ice-candidate", &[&m_line_index, &c]);
            }
        }

        Ok(())
    }

    fn parse_endpoint_response(
        &self,
        endpoint: reqwest::Url,
        redirects: u8,
        resp: reqwest::Response,
    ) -> Result<(), ErrorMessage> {
        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => {
                gst::info!(CAT, imp: self, "SDP offer successfully send");
                Ok(())
            }

            StatusCode::CREATED => {
                gst::debug!(CAT, imp: self, "Response headers: {:?}", resp.headers());

                let settings = self
                    .settings
                    .lock()
                    .expect("Failed to acquire settings lock");
                if settings.use_link_headers {
                    self.set_ice_servers(resp.headers());
                }
                drop(settings);

                /* See section 4.2 of the WHEP specification */
                let location = match resp.headers().get(reqwest::header::LOCATION) {
                    Some(location) => location,
                    None => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Location header field should be present for WHEP resource URL"]
                        ));
                    }
                };

                let location = match location.to_str() {
                    Ok(loc) => loc,
                    Err(e) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to convert location to string {}", e]
                        ));
                    }
                };

                let url = reqwest::Url::parse(endpoint.as_str()).unwrap();

                gst::debug!(CAT, imp: self, "WHEP resource: {:?}", location);

                let url = url.join(location).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["URL join operation failed: {:?}", err]
                    )
                })?;

                let mut state = self.state.lock().unwrap();
                *state = State::Running {
                    whep_resource: url.to_string(),
                };
                drop(state);

                let ans_bytes = match self.wait(resp.bytes()) {
                    Ok(ans) => ans,
                    Err(e) => return Err(e),
                };

                self.sdp_message_parse(ans_bytes)
            }

            status if status.is_redirection() => {
                if redirects < MAX_REDIRECTS {
                    let mut state = self.state.lock().unwrap();
                    /*
                     * As per section 4.6 of the specification, redirection is
                     * not required to be supported for the PATCH and DELETE
                     * requests to the final WHEP resource URL. Only the initial
                     * POST request may support redirection.
                     */
                    if let State::Running { .. } = *state {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Unexpected redirection in RUNNING state"]
                        ));
                    }

                    *state = State::Post {
                        redirects: redirects + 1,
                    };

                    drop(state);

                    match parse_redirect_location(resp.headers(), &endpoint) {
                        Ok(redirect_url) => {
                            gst::warning!(
                                CAT,
                                imp: self,
                                "Redirecting endpoint to {}",
                                redirect_url.as_str()
                            );
                            self.initial_post_request(redirect_url)
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Too many redirects. Unable to connect."]
                    ))
                }
            }

            s => {
                // FIXME: Check and handle 'Retry-After' header in case of server error
                Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    [
                        "Unexpected response: {} - {}",
                        s.as_str(),
                        self.wait(resp.bytes())
                            .map(|x| x.escape_ascii().to_string())
                            .unwrap_or_else(|_| "(no further details)".to_string())
                    ]
                ))
            }
        }
    }

    fn generate_offer(&self) {
        let self_weak = self.downgrade();
        let promise = gst::Promise::with_change_func(move |reply| {
            let self_ = match self_weak.upgrade() {
                Some(self_) => self_,
                None => return,
            };

            let reply = match reply {
                Ok(Some(reply)) => reply,
                Ok(None) => {
                    element_imp_error!(
                        self_,
                        gst::LibraryError::Failed,
                        ["generate offer::Promise returned with no reply"]
                    );
                    return;
                }
                Err(e) => {
                    element_imp_error!(
                        self_,
                        gst::LibraryError::Failed,
                        ["generate offer::Promise returned with error {:?}", e]
                    );
                    return;
                }
            };

            if let Ok(offer_sdp) = reply
                .value("offer")
                .map(|offer| offer.get::<gst_webrtc::WebRTCSessionDescription>().unwrap())
            {
                gst::debug!(
                    CAT,
                    imp: self_,
                    "Setting local description: {:?}",
                    offer_sdp.sdp().as_text()
                );

                self_.webrtcbin.emit_by_name::<()>(
                    "set-local-description",
                    &[&offer_sdp, &None::<gst::Promise>],
                );
            } else {
                gst::error!(
                    CAT,
                    imp: self_,
                    "Reply without an offer: {}",
                    reply
                );
                element_imp_error!(
                    self_,
                    gst::LibraryError::Failed,
                    ["generate offer::Promise returned with no reply"]
                );
            }
        });

        let settings = self.settings.lock().unwrap();

        gst::debug!(
            CAT,
            imp: self,
            "Audio caps: {:?} Video caps: {:?}",
            settings.audio_caps,
            settings.video_caps
        );

        /*
         * Since we will be recvonly we need to add a transceiver without which
         * WebRTC bin does not generate ICE candidates.
         */
        self.webrtcbin.emit_by_name::<WebRTCRTPTransceiver>(
            "add-transceiver",
            &[
                &WebRTCRTPTransceiverDirection::Recvonly,
                &settings.audio_caps,
            ],
        );

        self.webrtcbin.emit_by_name::<WebRTCRTPTransceiver>(
            "add-transceiver",
            &[
                &WebRTCRTPTransceiverDirection::Recvonly,
                &settings.video_caps,
            ],
        );

        drop(settings);

        self.webrtcbin
            .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    }

    fn initial_post_request(&self, endpoint: reqwest::Url) -> Result<(), ErrorMessage> {
        let state = self.state.lock().unwrap();

        gst::info!(CAT, imp: self, "WHEP endpoint url: {}", endpoint.as_str());

        let _ = match *state {
            State::Post { redirects } => redirects,
            _ => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Trying to do POST in unexpected state"]
                ));
            }
        };
        drop(state);

        self.generate_offer();

        Ok(())
    }

    fn whep_offer(&self) {
        let local_desc = self
            .webrtcbin
            .property::<Option<WebRTCSessionDescription>>("local-description");

        let offer_sdp = match local_desc {
            None => {
                element_imp_error!(
                    self,
                    gst::ResourceError::Failed,
                    ["Local description is not set"]
                );
                return;
            }
            Some(offer) => offer,
        };

        gst::debug!(
            CAT,
            imp: self,
            "Sending offer SDP: {:?}",
            offer_sdp.sdp().as_text()
        );

        if let Err(e) = self.send_sdp(offer_sdp.sdp()) {
            element_imp_error!(
                self,
                gst::ResourceError::Failed,
                ["Error in sending answer - {}", e.to_string()]
            );
        }
    }

    // Taken from WHIP sink
    fn set_ice_servers(&self, headermap: &HeaderMap) {
        for link in headermap.get_all("link").iter() {
            let link = link
                .to_str()
                .expect("Header value should contain only visible ASCII strings");

            let item_map = match parse_link_header::parse_with_rel(link) {
                Ok(map) => map,
                Err(e) => {
                    gst::warning!(
                        CAT,
                        imp: self,
                        "Failed to set ICE server {} due to {:?}",
                        link,
                        e
                    );
                    continue;
                }
            };

            let link = match item_map.contains_key("ice-server") {
                true => item_map.get("ice-server").unwrap(),
                false => continue, // Not a link header we care about
            };

            // Note: webrtcbin needs ice servers to be in the below format
            // <scheme>://<user:pass>@<url>
            // and the ice-servers (link headers) received from the whip server might be
            // in the format <scheme>:<host> with username and password as separate params.
            // Constructing these with 'url' crate also require a format/parse
            // for changing <scheme>:<host> to <scheme>://<user>:<password>@<host>.
            // So preferred to use the String rather

            let mut ice_server_url;

            // check if uri has ://
            if link.uri.has_authority() {
                // use raw_uri as is
                // username and password in the link.uri.params ignored
                ice_server_url = link.raw_uri.as_str().to_string();
            } else {
                // construct url as '<scheme>://<user:pass>@<url>'
                ice_server_url = format!("{}://", link.uri.scheme());
                if let Some(user) = link.params.get("username") {
                    ice_server_url += user.as_str();
                    if let Some(pass) = link.params.get("credential") {
                        ice_server_url = ice_server_url + ":" + pass.as_str();
                    }
                    ice_server_url += "@";
                }

                // the raw_uri contains the ice-server in the form <scheme>:<url>
                // so strip the scheme and the ':' from the beginning of raw_uri and use
                // the rest of raw_uri to append it the url which will be in the form
                // <scheme>://<user:pass>@<url> as expected
                ice_server_url += link
                    .raw_uri
                    .strip_prefix((link.uri.scheme().to_owned() + ":").as_str())
                    .expect("strip 'scheme:' from raw uri");
            }

            gst::info!(
                CAT,
                imp: self,
                "Setting STUN/TURN server {}",
                ice_server_url
            );

            // It's nicer to not collapse the `else if` and its inner `if`
            #[allow(clippy::collapsible_if)]
            if link.uri.scheme() == "stun" {
                self.webrtcbin
                    .set_property_from_str("stun-server", ice_server_url.as_str());
            } else if link.uri.scheme().starts_with("turn") {
                if !self
                    .webrtcbin
                    .emit_by_name::<bool>("add-turn-server", &[&ice_server_url.as_str()])
                {
                    gst::error!(
                        CAT,
                        imp: self,
                        "Failed to set turn server {}",
                        ice_server_url
                    );
                }
            }
        }
    }

    fn send_sdp(&self, sdp: SDPMessage) -> Result<(), gst::ErrorMessage> {
        let sess_desc = WebRTCSessionDescription::new(WebRTCSDPType::Offer, sdp);
        let settings = self.settings.lock().unwrap();

        let endpoint = reqwest::Url::parse(settings.whep_endpoint.as_ref().unwrap().as_str());

        if let Err(e) = endpoint {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Could not parse endpoint URL: {}", e]
            ));
        }

        drop(settings);

        self.do_post(sess_desc, endpoint.unwrap())
    }

    fn do_post(
        &self,
        offer: WebRTCSessionDescription,
        endpoint: reqwest::Url,
    ) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        let sdp = offer.sdp();
        let body = sdp.as_text().unwrap();

        gst::info!(CAT, imp: self, "Using endpoint {}", endpoint.as_str());

        let mut headermap = HeaderMap::new();
        headermap.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/sdp"),
        );

        if let Some(token) = &settings.auth_token {
            let bearer_token = "Bearer ".to_owned() + token;
            headermap.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(bearer_token.as_str())
                    .expect("Failed to set auth token to header"),
            );
        }

        drop(settings);

        gst::debug!(
            CAT,
            imp: self,
            "Url for HTTP POST request: {}",
            endpoint.as_str()
        );

        let future = self
            .client
            .request(reqwest::Method::POST, endpoint.clone())
            .headers(headermap)
            .body(body)
            .send();

        let resp = self.wait(future)?;

        self.parse_endpoint_response(endpoint, 0, resp)
    }

    fn terminate_session(&self) {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();

        let resource_url = match *state {
            State::Running {
                whep_resource: ref whep_resource_url,
            } => whep_resource_url.clone(),
            _ => {
                element_imp_error!(
                    self,
                    gst::ResourceError::Failed,
                    ["Terminated in unexpected state"]
                );
                return;
            }
        };

        drop(state);

        let mut headermap = HeaderMap::new();
        if let Some(token) = &settings.auth_token {
            let bearer_token = "Bearer ".to_owned() + token.as_str();
            headermap.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(bearer_token.as_str())
                    .expect("Failed to set auth token to header"),
            );
        }

        drop(settings);

        gst::debug!(CAT, imp: self, "DELETE request on {}", resource_url);

        /* DELETE request goes to the WHEP resource URL. See section 3 of the specification. */
        let client = build_reqwest_client(reqwest::redirect::Policy::default());
        let future = client.delete(resource_url).headers(headermap).send();

        let res = self.wait(future);
        match res {
            Ok(r) => {
                gst::debug!(CAT, imp: self, "Response to DELETE : {}", r.status());
            }
            Err(e) => {
                gst::error!(CAT, imp: self, "Error on DELETE request : {}", e);
            }
        };
    }

    fn wait<F, T, E>(&self, future: F) -> Result<T, ErrorMessage>
    where
        F: Send + Future<Output = Result<T, E>>,
        T: Send + 'static,
        E: Send + Display,
    {
        let mut canceller = self.canceller.lock().unwrap();
        let (abort_handle, abort_registration) = future::AbortHandle::new_pair();

        canceller.replace(abort_handle);
        drop(canceller);

        let future = async {
            match future::Abortable::new(future, abort_registration).await {
                Ok(Ok(res)) => Ok(res),

                Ok(Err(err)) => Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Future resolved with an error {}", err.to_string()]
                )),

                Err(future::Aborted) => Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Canceller called before future resolved"]
                )),
            }
        };

        let res = {
            let _enter = RUNTIME.enter();
            futures::executor::block_on(future)
        };

        let _ = self.canceller.lock().unwrap().take();
        res
    }
}

fn parse_redirect_location(
    headermap: &HeaderMap,
    old_url: &reqwest::Url,
) -> Result<reqwest::Url, ErrorMessage> {
    let location = headermap.get(reqwest::header::LOCATION).unwrap();
    if let Err(e) = location.to_str() {
        return Err(gst::error_msg!(
            gst::ResourceError::Failed,
            [
                "Failed to convert the redirect location to string {}",
                e.to_string()
            ]
        ));
    }
    let location = location.to_str().unwrap();

    if location.to_ascii_lowercase().starts_with("http") {
        // Location URL is an absolute path
        reqwest::Url::parse(location)
            .map_err(|e| gst::error_msg!(gst::ResourceError::Failed, ["{}", e.to_string()]))
    } else {
        // Location URL is a relative path
        let mut new_url = old_url.clone();
        new_url.set_path(location);
        Ok(new_url)
    }
}

fn build_reqwest_client(pol: Policy) -> reqwest::Client {
    let client_builder = reqwest::Client::builder();
    client_builder.redirect(pol).build().unwrap()
}
