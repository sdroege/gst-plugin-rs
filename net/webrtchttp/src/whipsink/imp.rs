// Copyright (C) 2022,  Asymptotic Inc.
//      Author: Taruntej Kanakamalla <taruntej@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::utils::{
    build_reqwest_client, parse_redirect_location, set_ice_servers, wait, WaitError,
};
use crate::GstRsWebRTCICETransportPolicy;
use futures::future;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::ErrorMessage;
use gst_sdp::*;
use gst_webrtc::*;
use once_cell::sync::Lazy;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::StatusCode;
use std::sync::Mutex;
use std::thread::{spawn, JoinHandle};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("whipsink", gst::DebugColorFlags::empty(), Some("WHIP Sink"))
});

const DEFAULT_ICE_TRANSPORT_POLICY: GstRsWebRTCICETransportPolicy =
    GstRsWebRTCICETransportPolicy::All;
const MAX_REDIRECTS: u8 = 10;
const DEFAULT_TIMEOUT: u32 = 15;

#[derive(Debug, Clone)]
struct Settings {
    whip_endpoint: Option<String>,
    use_link_headers: bool,
    auth_token: Option<String>,
    turn_server: Option<String>,
    stun_server: Option<String>,
    ice_transport_policy: GstRsWebRTCICETransportPolicy,
    timeout: u32,
}

#[allow(clippy::derivable_impls)]
impl Default for Settings {
    fn default() -> Self {
        Self {
            whip_endpoint: None,
            use_link_headers: false,
            auth_token: None,
            stun_server: None,
            turn_server: None,
            ice_transport_policy: DEFAULT_ICE_TRANSPORT_POLICY,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Debug)]
enum State {
    Stopped,
    Post {
        redirects: u8,
        thread_handle: Option<JoinHandle<()>>,
    },
    Running {
        whip_resource_url: String,
        thread_handle: Option<JoinHandle<()>>,
    },
}

impl Default for State {
    fn default() -> Self {
        Self::Stopped
    }
}

pub struct WhipSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    webrtcbin: gst::Element,
    canceller: Mutex<Option<future::AbortHandle>>,
}

impl Default for WhipSink {
    fn default() -> Self {
        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .name("whip-webrtcbin")
            .build()
            .expect("Failed to create webrtcbin");
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            webrtcbin,
            canceller: Mutex::new(None),
        }
    }
}

impl BinImpl for WhipSink {}

impl GstObjectImpl for WhipSink {}

impl ElementImpl for WhipSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WHIP Sink Bin",
                "Sink/Network/WebRTC",
                "A bin to stream media using the WebRTC HTTP Ingestion Protocol (WHIP)",
                "Taruntej Kanakamalla <taruntej@asymptotic.io>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::builder("application/x-rtp").build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &sink_caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let wb_sink_pad = self.webrtcbin.request_pad(templ, name, caps)?;
        let sink_pad = gst::GhostPad::new(Some(&wb_sink_pad.name()), gst::PadDirection::Sink);

        sink_pad.set_target(Some(&wb_sink_pad)).unwrap();
        self.obj().add_pad(&sink_pad).unwrap();

        Some(sink_pad.upcast())
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::NullToReady {
            /*
             * Fail the state change if WHIP endpoint has not been set by the
             * time ReadyToPaused transition happens. This prevents us from
             * having to check this everywhere else.
             */
            let settings = self.settings.lock().unwrap();

            if settings.whip_endpoint.is_none() {
                gst::error!(CAT, imp: self, "WHIP endpoint URL must be set");
                return Err(gst::StateChangeError);
            }

            /*
             * Check if we have a valid URL. We can be assured any further URL
             * handling won't fail due to invalid URLs.
             */
            if let Err(e) = reqwest::Url::parse(settings.whip_endpoint.as_ref().unwrap().as_str()) {
                gst::error!(
                    CAT,
                    imp: self,
                    "WHIP endpoint URL could not be parsed: {}",
                    e
                );

                return Err(gst::StateChangeError);
            }
            drop(settings);
        }

        if transition == gst::StateChange::PausedToReady {
            // Interrupt requests in progress, if any
            if let Some(canceller) = &*self.canceller.lock().unwrap() {
                canceller.abort();
            }

            let state = self.state.lock().unwrap();
            if let State::Running { .. } = *state {
                // Release server-side resources
                drop(state);
                self.terminate_session();
            }
        }

        self.parent_change_state(transition)
    }
}

impl ObjectImpl for WhipSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecString::builder("whip-endpoint")
                    .nick("WHIP Endpoint")
                    .blurb("The WHIP server endpoint to POST SDP offer to.
                        e.g.: https://example.com/whip/endpoint/room1234")
                    .mutable_ready()
                    .build(),

                glib::ParamSpecBoolean::builder("use-link-headers")
                    .nick("Use Link Headers")
                    .blurb("Use link headers to configure ice-servers from the WHIP server response to the POST request.
                        If set to TRUE and the WHIP server returns valid ice-servers,
                        this property overrides the ice-servers values set using the stun-server and turn-server properties.")
                    .mutable_ready()
                    .build(),

                glib::ParamSpecString::builder("auth-token")
                    .nick("Authorization Token")
                    .blurb("Authentication token to use, will be sent in the HTTP Header as 'Bearer <auth-token>'")
                    .mutable_ready()
                    .build(),

                glib::ParamSpecString::builder("stun-server")
                    .nick("STUN Server")
                    .blurb("The STUN server of the form stun://hostname:port")
                    .build(),

                glib::ParamSpecString::builder("turn-server")
                    .nick("TURN Server")
                    .blurb("The TURN server of the form turn(s)://username:password@host:port.")
                    .build(),

                glib::ParamSpecEnum::builder::<GstRsWebRTCICETransportPolicy>("ice-transport-policy", DEFAULT_ICE_TRANSPORT_POLICY)
                    .nick("ICE transport policy")
                    .blurb("The policy to apply for ICE transport")
                    .build(),

                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout WHIP endpoint requests (0 = No timeout).")
                    .maximum(3600)
                    .default_value(DEFAULT_TIMEOUT)
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "whip-endpoint" => {
                let mut settings = self.settings.lock().unwrap();
                settings.whip_endpoint = value.get().expect("WHIP endpoint should be a string");
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
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                settings.timeout = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "whip-endpoint" => {
                let settings = self.settings.lock().unwrap();
                settings.whip_endpoint.to_value()
            }
            "use-link-headers" => {
                let settings = self.settings.lock().unwrap();
                settings.use_link_headers.to_value()
            }
            "auth-token" => {
                let settings = self.settings.lock().unwrap();
                settings.auth_token.to_value()
            }
            "stun-server" => {
                let settings = self.settings.lock().unwrap();
                settings.stun_server.to_value()
            }
            "turn-server" => {
                let settings = self.settings.lock().unwrap();
                settings.turn_server.to_value()
            }
            "ice-transport-policy" => {
                let settings = self.settings.lock().unwrap();
                settings.ice_transport_policy.to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SINK);

        // The spec requires all m= lines to be bundled (section 4.2)
        self.webrtcbin
            .set_property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle);

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

                        let mut state = self_.state.lock().unwrap();
                        let self_ref = self_.ref_counted();

                        gst::debug!(CAT, imp: self_, "Spawning thread to send offer");
                        let handle = spawn(move || self_ref.send_offer());

                        *state = State::Post {
                            redirects: 0,
                            thread_handle: Some(handle),
                        };
                        drop(state);
                    }
                    _ => (),
                }
            });

        self.webrtcbin.connect("on-negotiation-needed", false, {
            move |args| {
                let webrtcbin = args[0].get::<gst::Element>().unwrap();
                let ele = match webrtcbin
                    .parent()
                    .map(|p| p.downcast::<Self::Type>().unwrap())
                {
                    Some(e) => e,
                    None => return None,
                };

                let whipsink = ele.imp();
                let settings = whipsink.settings.lock().unwrap();
                if settings.whip_endpoint.is_none() {
                    gst::element_error!(
                        ele,
                        gst::ResourceError::NotFound,
                        ["Endpoint URL must be set"]
                    );
                    return None;
                }

                let endpoint =
                    reqwest::Url::parse(settings.whip_endpoint.as_ref().unwrap().as_str());
                if let Err(e) = endpoint {
                    gst::element_error!(
                        ele,
                        gst::ResourceError::Failed,
                        ["Could not parse endpoint URL :{}", e]
                    );
                    return None;
                }

                drop(settings);

                // Promise for 'create-offer' signal emitted to webrtcbin
                // Closure is called when the promise is fulfilled
                let promise = gst::Promise::with_change_func(move |reply| {
                    let ele = match webrtcbin
                        .parent()
                        .map(|p| p.downcast::<Self::Type>().unwrap())
                    {
                        Some(ele) => ele,
                        None => return,
                    };

                    let whipsink = ele.imp();

                    let offer_sdp = match reply {
                        Ok(Some(sdp)) => sdp
                            .value("offer")
                            .expect("structure must have an offer key")
                            .get::<gst_webrtc::WebRTCSessionDescription>()
                            .expect("offer must be an SDP"),
                        Ok(None) => {
                            gst::element_error!(
                                ele,
                                gst::LibraryError::Failed,
                                ["create-offer::Promise returned with no reply"]
                            );
                            return;
                        }
                        Err(e) => {
                            gst::element_error!(
                                ele,
                                gst::LibraryError::Failed,
                                ["create-offer::Promise returned with error {:?}", e]
                            );
                            return;
                        }
                    };

                    whipsink.webrtcbin.emit_by_name::<()>(
                        "set-local-description",
                        &[&offer_sdp, &None::<gst::Promise>],
                    );
                });

                whipsink
                    .webrtcbin
                    .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);

                None
            }
        });

        self.webrtcbin.connect("on-new-transceiver", false, {
            move |args| {
                let trans = args[1].get::<gst_webrtc::WebRTCRTPTransceiver>().unwrap();
                // We only ever send data
                trans.set_direction(gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly);
                None
            }
        });

        obj.add(&self.webrtcbin).unwrap();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WhipSink {
    const NAME: &'static str = "GstWhipSink";
    type Type = super::WhipSink;
    type ParentType = gst::Bin;
}

impl WhipSink {
    fn send_offer(&self) {
        let settings = self.settings.lock().unwrap();

        /* Note that we check for a valid WHIP endpoint in change_state */
        let endpoint = reqwest::Url::parse(settings.whip_endpoint.as_ref().unwrap().as_str());
        if let Err(e) = endpoint {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Failed,
                ["Could not parse endpoint URL: {}", e]
            );
            return;
        }

        drop(settings);
        let mut state = self.state.lock().unwrap();
        *state = State::Post {
            redirects: 0,
            thread_handle: None,
        };
        drop(state);

        let local_desc = self
            .webrtcbin
            .property::<Option<WebRTCSessionDescription>>("local-description");

        let offer_sdp = match local_desc {
            None => {
                gst::element_imp_error!(
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

        match self.do_post(offer_sdp, endpoint.unwrap()) {
            Ok(_) => (),
            Err(e) => {
                gst::element_imp_error!(
                    self,
                    gst::ResourceError::Failed,
                    ["Failed to send offer: {}", e]
                );
            }
        }
    }

    fn do_post(
        &self,
        offer: gst_webrtc::WebRTCSessionDescription,
        endpoint: reqwest::Url,
    ) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();
        let timeout = settings.timeout;

        let redirects = match *state {
            State::Post {
                redirects,
                thread_handle: ref _h,
            } => redirects,
            _ => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Trying to POST in unexpected state"]
                ));
            }
        };
        drop(state);

        // Default policy for redirect does not share the auth token to new location
        // So disable inbuilt redirecting and do a recursive call upon 3xx response code
        let pol = reqwest::redirect::Policy::none();
        let client = build_reqwest_client(pol);

        let sdp = offer.sdp();
        let body = sdp.as_text().unwrap();

        gst::debug!(CAT, imp: self, "Using endpoint {}", endpoint.as_str());
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

        let future = async {
            client
                .request(reqwest::Method::POST, endpoint.as_ref())
                .headers(headermap)
                .body(body)
                .send()
                .await
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["POST request failed: {:?}", err]
                    )
                })
        };

        drop(settings);

        match wait(&self.canceller, future, timeout) {
            Ok(resp) => self.parse_endpoint_response(offer, endpoint, redirects, resp),
            Err(err) => match err {
                WaitError::FutureAborted => Ok(()),
                WaitError::FutureError(e) => Err(e),
            },
        }
    }

    fn parse_endpoint_response(
        &self,
        offer: gst_webrtc::WebRTCSessionDescription,
        endpoint: reqwest::Url,
        redirects: u8,
        resp: reqwest::Response,
    ) -> Result<(), ErrorMessage> {
        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => {
                let settings = self
                    .settings
                    .lock()
                    .expect("Failed to acquire settings lock");
                let timeout = settings.timeout;

                if settings.use_link_headers {
                    set_ice_servers(&self.webrtcbin, resp.headers())?;
                }
                drop(settings);

                // Get the url of the resource from 'location' header.
                // The resource created is expected be a relative path
                // and not an absolute path
                // So we want to construct the full url of the resource
                // using the endpoint url i.e., replace the end point path with
                // resource path
                let location = match resp.headers().get(reqwest::header::LOCATION) {
                    Some(location) => location,
                    None => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Location header field should be present for WHIP resource URL"]
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

                gst::debug!(CAT, imp: self, "WHIP resource: {:?}", location);

                let url = url.join(location).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["URL join operation failed: {:?}", err]
                    )
                })?;

                let mut state = self.state.lock().unwrap();
                *state = match *state {
                    State::Post {
                        redirects: _r,
                        thread_handle: ref mut h,
                    } => State::Running {
                        whip_resource_url: url.to_string(),
                        thread_handle: h.take(),
                    },
                    _ => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Expected to be in POST state"]
                        ));
                    }
                };
                drop(state);

                let future = async {
                    resp.bytes().await.map_err(|err| {
                        gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to get response body: {:?}", err]
                        )
                    })
                };

                match wait(&self.canceller, future, timeout) {
                    Ok(ans_bytes) => match sdp_message::SDPMessage::parse_buffer(&ans_bytes) {
                        Ok(ans_sdp) => {
                            let answer = gst_webrtc::WebRTCSessionDescription::new(
                                gst_webrtc::WebRTCSDPType::Answer,
                                ans_sdp,
                            );
                            self.webrtcbin.emit_by_name::<()>(
                                "set-remote-description",
                                &[&answer, &None::<gst::Promise>],
                            );
                            Ok(())
                        }

                        Err(e) => Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Could not parse answer SDP: {}", e]
                        )),
                    },
                    Err(err) => match err {
                        WaitError::FutureAborted => Ok(()),
                        WaitError::FutureError(e) => Err(e),
                    },
                }
            }

            s if s.is_redirection() => {
                if redirects < MAX_REDIRECTS {
                    let mut state = self.state.lock().unwrap();
                    *state = match *state {
                        State::Post {
                            redirects: _r,
                            thread_handle: ref mut h,
                        } => State::Post {
                            redirects: redirects + 1,
                            thread_handle: h.take(),
                        },
                        /*
                         * As per section 4.6 of the specification, redirection is
                         * not required to be supported for the PATCH and DELETE
                         * requests to the final WHEP resource URL. Only the initial
                         * POST request may support redirection.
                         */
                        State::Running { .. } => {
                            return Err(gst::error_msg!(
                                gst::ResourceError::Failed,
                                ["Unexpected redirection in RUNNING state"]
                            ));
                        }
                        State::Stopped => unreachable!(),
                    };
                    drop(state);

                    match parse_redirect_location(resp.headers(), &endpoint) {
                        Ok(redirect_url) => {
                            gst::debug!(
                                CAT,
                                imp: self,
                                "Redirecting endpoint to {}",
                                redirect_url.as_str()
                            );
                            self.do_post(offer, redirect_url)
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Too many redirects. Unable to connect to do POST"]
                    ))
                }
            }

            s => {
                let future = async {
                    resp.bytes().await.map_err(|err| {
                        gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to get response body: {:?}", err]
                        )
                    })
                };

                let settings = self
                    .settings
                    .lock()
                    .expect("Failed to acquire settings lock");
                let timeout = settings.timeout;
                drop(settings);

                let resp = wait(&self.canceller, future, timeout)
                    .map(|x| x.escape_ascii().to_string())
                    .unwrap_or_else(|_| "(no further details)".to_string());

                // FIXME: Check and handle 'Retry-After' header in case of server error
                Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Server returned error: {} - {}", s.as_str(), resp]
                ))
            }
        }
    }

    fn terminate_session(&self) {
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let timeout = settings.timeout;
        let resource_url;

        (*state, resource_url) = match *state {
            State::Running {
                whip_resource_url: ref resource_url,
                thread_handle: ref mut h,
            } => {
                if let Some(th) = h.take() {
                    match th.join() {
                        Ok(_) => {
                            gst::debug!(CAT, imp: self, "Send offer thread joined successfully");
                        }
                        Err(e) => gst::error!(CAT, imp: self, "Failed to join thread: {:?}", e),
                    }
                }
                (State::Stopped, resource_url.clone())
            }
            _ => {
                gst::element_imp_error!(
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

        gst::debug!(CAT, imp: self, "DELETE request on {}", resource_url);
        let client = build_reqwest_client(reqwest::redirect::Policy::default());
        let future = async {
            client
                .delete(resource_url.clone())
                .headers(headermap)
                .send()
                .await
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["DELETE request failed {}: {:?}", resource_url, err]
                    )
                })
        };

        let res = wait(&self.canceller, future, timeout);
        match res {
            Ok(r) => {
                gst::debug!(CAT, imp: self, "Response to DELETE : {}", r.status());
            }
            Err(e) => match e {
                WaitError::FutureAborted => {
                    gst::warning!(CAT, imp: self, "DELETE request aborted")
                }
                WaitError::FutureError(e) => {
                    gst::error!(CAT, imp: self, "Error on DELETE request : {}", e)
                }
            },
        };
    }
}
