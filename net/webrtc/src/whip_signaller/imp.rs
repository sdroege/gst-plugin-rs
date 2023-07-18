// SPDX-License-Identifier: MPL-2.0

use crate::signaller::{Signallable, SignallableImpl};
use crate::utils::{
    build_reqwest_client, parse_redirect_location, set_ice_servers, wait, wait_async, WaitError,
};
use crate::RUNTIME;
use async_recursion::async_recursion;
use gst::glib;
use gst::glib::once_cell::sync::Lazy;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_webrtc::{WebRTCICEGatheringState, WebRTCSessionDescription};
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::StatusCode;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtc-whip-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC WHIP signaller"),
    )
});

const MAX_REDIRECTS: u8 = 10;
const DEFAULT_TIMEOUT: u32 = 15;

#[derive(Debug)]
enum WhipClientState {
    Stopped,
    Post { redirects: u8 },
    Running { whip_resource_url: String },
}

impl Default for WhipClientState {
    fn default() -> Self {
        Self::Stopped
    }
}

#[derive(Clone)]
struct WhipClientSettings {
    whip_endpoint: Option<String>,
    use_link_headers: bool,
    auth_token: Option<String>,
    timeout: u32,
}

impl Default for WhipClientSettings {
    fn default() -> Self {
        Self {
            whip_endpoint: None,
            use_link_headers: false,
            auth_token: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

#[derive(Default)]
pub struct WhipClient {
    state: Mutex<WhipClientState>,
    settings: Mutex<WhipClientSettings>,
    canceller: Mutex<Option<futures::future::AbortHandle>>,
}

impl WhipClient {
    fn raise_error(&self, msg: String) {
        self.obj()
            .emit_by_name::<()>("error", &[&format!("Error: {msg}")]);
    }

    fn handle_future_error(&self, err: WaitError) {
        match err {
            WaitError::FutureAborted => {
                gst::warning!(CAT, imp: self, "Future aborted")
            }
            WaitError::FutureError(err) => self.raise_error(err.to_string()),
        };
    }

    async fn send_offer(&self, webrtcbin: &gst::Element) {
        {
            let mut state = self.state.lock().unwrap();
            *state = WhipClientState::Post { redirects: 0 };
            drop(state);
        }

        let local_desc =
            webrtcbin.property::<Option<WebRTCSessionDescription>>("local-description");

        let offer_sdp = match local_desc {
            None => {
                self.raise_error("Local description is not set".to_string());
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

        let timeout;
        let endpoint;
        {
            let settings = self.settings.lock().unwrap();
            timeout = settings.timeout;
            endpoint =
                reqwest::Url::parse(settings.whip_endpoint.as_ref().unwrap().as_str()).unwrap();
            drop(settings);
        }

        if let Err(e) = wait_async(
            &self.canceller,
            self.do_post(offer_sdp, webrtcbin, endpoint),
            timeout,
        )
        .await
        {
            self.handle_future_error(e);
        }
    }

    #[async_recursion]
    async fn do_post(
        &self,
        offer: gst_webrtc::WebRTCSessionDescription,
        webrtcbin: &gst::Element,
        endpoint: reqwest::Url,
    ) {
        let auth_token;

        {
            let settings = self.settings.lock().unwrap();
            auth_token = settings.auth_token.clone();
            drop(settings);
        }

        #[allow(unused_mut)]
        let mut redirects;

        {
            let state = self.state.lock().unwrap();
            redirects = match *state {
                WhipClientState::Post { redirects } => redirects,
                _ => {
                    self.raise_error("Trying to do POST in unexpected state".to_string());
                    return;
                }
            };
            drop(state);
        }

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

        if let Some(token) = auth_token.as_ref() {
            let bearer_token = "Bearer ".to_owned() + token;
            headermap.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(bearer_token.as_str())
                    .expect("Failed to set auth token to header"),
            );
        }

        let res = client
            .request(reqwest::Method::POST, endpoint.clone())
            .headers(headermap)
            .body(body)
            .send()
            .await;

        match res {
            Ok(resp) => {
                self.parse_endpoint_response(offer, resp, redirects, webrtcbin)
                    .await
            }
            Err(err) => self.raise_error(err.to_string()),
        }
    }

    async fn parse_endpoint_response(
        &self,
        offer: gst_webrtc::WebRTCSessionDescription,
        resp: reqwest::Response,
        redirects: u8,
        webrtcbin: &gst::Element,
    ) {
        gst::debug!(CAT, imp: self, "Parsing endpoint response");

        let endpoint;
        let use_link_headers;

        {
            let settings = self.settings.lock().unwrap();
            endpoint =
                reqwest::Url::parse(settings.whip_endpoint.as_ref().unwrap().as_str()).unwrap();
            use_link_headers = settings.use_link_headers;
            drop(settings);
        }

        gst::debug!(CAT, "response status: {}", resp.status());

        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => {
                if use_link_headers {
                    if let Err(e) = set_ice_servers(webrtcbin, resp.headers()) {
                        self.raise_error(e.to_string());
                        return;
                    };
                }

                // Get the url of the resource from 'location' header.
                // The resource created is expected be a relative path
                // and not an absolute path
                // So we want to construct the full url of the resource
                // using the endpoint url i.e., replace the end point path with
                // resource path
                let location = match resp.headers().get(reqwest::header::LOCATION) {
                    Some(location) => location,
                    None => {
                        self.raise_error(
                            "Location header field should be present for WHIP resource URL"
                                .to_string(),
                        );
                        return;
                    }
                };

                let location = match location.to_str() {
                    Ok(loc) => loc,
                    Err(e) => {
                        self.raise_error(format!("Failed to convert location to string: {e}"));
                        return;
                    }
                };

                let url = reqwest::Url::parse(endpoint.as_str()).unwrap();

                gst::debug!(CAT, imp: self, "WHIP resource: {:?}", location);

                let url = match url.join(location) {
                    Ok(joined_url) => joined_url,
                    Err(err) => {
                        self.raise_error(format!("URL join operation failed: {err:?}"));
                        return;
                    }
                };

                {
                    let mut state = self.state.lock().unwrap();
                    *state = match *state {
                        WhipClientState::Post { redirects: _r } => WhipClientState::Running {
                            whip_resource_url: url.to_string(),
                        },
                        _ => {
                            self.raise_error("Expected to be in POST state".to_string());
                            return;
                        }
                    };
                    drop(state);
                }

                match resp.bytes().await {
                    Ok(ans_bytes) => match gst_sdp::SDPMessage::parse_buffer(&ans_bytes) {
                        Ok(ans_sdp) => {
                            let answer = gst_webrtc::WebRTCSessionDescription::new(
                                gst_webrtc::WebRTCSDPType::Answer,
                                ans_sdp,
                            );
                            self.obj()
                                .emit_by_name::<()>("session-description", &[&"unique", &answer]);
                        }
                        Err(err) => {
                            self.raise_error(format!("Could not parse answer SDP: {err}"));
                        }
                    },
                    Err(err) => self.raise_error(err.to_string()),
                }
            }

            s if s.is_redirection() => {
                gst::debug!(CAT, "redirected");

                if redirects < MAX_REDIRECTS {
                    match parse_redirect_location(resp.headers(), &endpoint) {
                        Ok(redirect_url) => {
                            {
                                let mut state = self.state.lock().unwrap();
                                *state = match *state {
                                    WhipClientState::Post { redirects: _r } => {
                                        WhipClientState::Post {
                                            redirects: redirects + 1,
                                        }
                                    }
                                    /*
                                     * As per section 4.6 of the specification, redirection is
                                     * not required to be supported for the PATCH and DELETE
                                     * requests to the final WHEP resource URL. Only the initial
                                     * POST request may support redirection.
                                     */
                                    WhipClientState::Running { .. } => {
                                        self.raise_error(
                                            "Unexpected redirection in RUNNING state".to_string(),
                                        );
                                        return;
                                    }
                                    WhipClientState::Stopped => unreachable!(),
                                };
                                drop(state);
                            }

                            gst::debug!(
                                CAT,
                                imp: self,
                                "Redirecting endpoint to {}",
                                redirect_url.as_str()
                            );

                            self.do_post(offer, webrtcbin, redirect_url).await
                        }
                        Err(e) => self.raise_error(e.to_string()),
                    }
                } else {
                    self.raise_error("Too many redirects. Unable to connect.".to_string());
                }
            }

            s => {
                match resp.bytes().await {
                    Ok(r) => {
                        let res = r.escape_ascii().to_string();

                        // FIXME: Check and handle 'Retry-After' header in case of server error
                        self.raise_error(format!("Unexpected response: {} - {}", s.as_str(), res));
                    }
                    Err(err) => self.raise_error(err.to_string()),
                }
            }
        }
    }

    fn terminate_session(&self) {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();
        let timeout = settings.timeout;

        let resource_url = match *state {
            WhipClientState::Running {
                whip_resource_url: ref resource_url,
            } => resource_url.clone(),
            _ => {
                self.raise_error("Terminated in unexpected state".to_string());
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

impl SignallableImpl for WhipClient {
    fn start(&self) {
        if self.settings.lock().unwrap().whip_endpoint.is_none() {
            self.raise_error("WHIP endpoint URL must be set".to_string());
            return;
        }

        self.obj().connect_closure(
            "consumer-added",
            false,
            glib::closure!(|signaller: &super::WhipClientSignaller,
                            _consumer_identifier: &str,
                            webrtcbin: &gst::Element| {
                let obj_weak = signaller.downgrade();
                webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
                    let Some(obj) = obj_weak.upgrade() else {
                        return;
                    };

                    let state =
                        webrtcbin.property::<WebRTCICEGatheringState>("ice-gathering-state");

                    match state {
                        WebRTCICEGatheringState::Gathering => {
                            gst::info!(CAT, obj: obj, "ICE gathering started");
                        }
                        WebRTCICEGatheringState::Complete => {
                            gst::info!(CAT, obj: obj, "ICE gathering complete");

                            let webrtcbin = webrtcbin.clone();

                            RUNTIME.spawn(async move {
                                /* Note that we check for a valid WHIP endpoint in change_state */
                                obj.imp().send_offer(&webrtcbin).await
                            });
                        }
                        _ => (),
                    }
                });
            }),
        );

        self.obj().emit_by_name::<()>(
            "session-requested",
            &[
                &"unique",
                &"unique",
                &None::<gst_webrtc::WebRTCSessionDescription>,
            ],
        );
    }

    fn stop(&self) {
        // Interrupt requests in progress, if any
        if let Some(canceller) = &*self.canceller.lock().unwrap() {
            canceller.abort();
        }

        let state = self.state.lock().unwrap();
        if let WhipClientState::Running { .. } = *state {
            // Release server-side resources
            drop(state);
        }
    }

    fn end_session(&self, session_id: &str) {
        assert_eq!(session_id, "unique");

        // Interrupt requests in progress, if any
        if let Some(canceller) = &*self.canceller.lock().unwrap() {
            canceller.abort();
        }

        let state = self.state.lock().unwrap();
        if let WhipClientState::Running { .. } = *state {
            // Release server-side resources
            drop(state);
            self.terminate_session();
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WhipClient {
    const NAME: &'static str = "GstWhipClientSignaller";
    type Type = super::WhipClientSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for WhipClient {
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
                settings.whip_endpoint = value.get().unwrap();
            }
            "use-link-headers" => {
                let mut settings = self.settings.lock().unwrap();
                settings.use_link_headers = value.get().unwrap();
            }
            "auth-token" => {
                let mut settings = self.settings.lock().unwrap();
                settings.auth_token = value.get().unwrap();
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                settings.timeout = value.get().unwrap();
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
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            _ => unimplemented!(),
        }
    }
}
