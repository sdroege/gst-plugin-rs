// Copyright (C) 2022,  Asymptotic Inc.
//      Author: Taruntej Kanakamalla <taruntej@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::future;
use futures::prelude::*;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::ErrorMessage;
use gst_sdp::*;
use gst_webrtc::*;
use once_cell::sync::Lazy;
use parse_link_header;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::redirect::Policy;
use reqwest::StatusCode;
use std::fmt::Display;
use std::sync::Mutex;
use tokio::runtime;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("whipsink", gst::DebugColorFlags::empty(), Some("WHIP Sink"))
});

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

const MAX_REDIRECTS: u8 = 10;

#[derive(Debug, Clone)]
struct Settings {
    whip_endpoint: Option<String>,
    use_link_headers: bool,
    auth_token: Option<String>,
}

#[allow(clippy::derivable_impls)]
impl Default for Settings {
    fn default() -> Self {
        Self {
            whip_endpoint: None,
            use_link_headers: false,
            auth_token: None,
        }
    }
}

#[derive(Debug, Clone)]
enum State {
    Stopped,
    Options { redirects: u8 },
    Post { redirects: u8 },
    Running { whip_resource_url: String },
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
        let ret = self.parent_change_state(transition);
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

        ret
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
                    .blurb("Use link headers to configure ice-servers from the WHIP server response to the POST or OPTIONS request.
                        If set to TRUE and the WHIP server returns valid ice-servers,
                        this property overrides the ice-servers values set using the stun-server and turn-server properties.")
                    .mutable_ready()
                    .build(),

                glib::ParamSpecString::builder("auth-token")
                    .nick("Authorization Token")
                    .blurb("Authentication token to use, will be sent in the HTTP Header as 'Bearer <auth-token>'")
                    .mutable_ready()
                    .build()
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
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SINK);
        obj.add(&self.webrtcbin).unwrap();

        // The spec requires all m= lines to be bundled (section 4.2)
        self.webrtcbin
            .set_property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle);

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
                let mut state = whipsink.state.lock().unwrap();
                *state = State::Options { redirects: 0 };
                drop(state);

                if let Err(e) = whipsink.lookup_ice_servers(endpoint.unwrap()) {
                    gst::element_error!(
                        ele,
                        gst::ResourceError::Failed,
                        ["Error in 'lookup_ice_servers' - {}", e.to_string()]
                    );
                }

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
                    if let Err(e) = whipsink.send_offer(offer_sdp) {
                        gst::element_error!(
                            ele,
                            gst::ResourceError::Failed,
                            ["Error in 'send_offer' - {}", e.to_string()]
                        );
                    }
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
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WhipSink {
    const NAME: &'static str = "GstWhipSink";
    type Type = super::WhipSink;
    type ParentType = gst::Bin;
}
impl WhipSink {
    fn lookup_ice_servers(&self, endpoint: reqwest::Url) -> Result<(), ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();

        let redirects = match *state {
            State::Options { redirects } => redirects,
            _ => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Trying to do OPTIONS in unexpected state"]
                ));
            }
        };
        drop(state);

        if !settings.use_link_headers {
            // We're not configured to use OPTIONS, so we're done
            return Ok(());
        }

        // We'll handle redirects manually since the default redirect handler does not
        // reuse the authentication token on the redirected server
        let pol = reqwest::redirect::Policy::none();
        let client = build_reqwest_client(pol);

        let mut headermap = HeaderMap::new();
        if let Some(token) = &settings.auth_token {
            let bearer_token = "Bearer ".to_owned() + token;
            drop(settings);
            headermap.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(&bearer_token)
                    .expect("Auth token should only contain characters valid for an HTTP header"),
            );
        }

        let future = client
            .request(reqwest::Method::OPTIONS, endpoint.as_ref())
            .headers(headermap)
            .send();

        let resp = match self.wait(future) {
            Ok(r) => r,
            Err(e) => {
                return Err(e);
            }
        };

        match resp.status() {
            StatusCode::NO_CONTENT => {
                self.set_ice_servers(resp.headers());
                Ok(())
            }
            status if status.is_redirection() => {
                if redirects < MAX_REDIRECTS {
                    let mut state = self.state.lock().unwrap();
                    *state = State::Options {
                        redirects: redirects + 1,
                    };
                    drop(state);

                    match parse_redirect_location(resp.headers(), &endpoint) {
                        Ok(redirect_url) => {
                            gst::debug!(CAT, "Redirecting endpoint to {}", redirect_url.as_str());
                            self.lookup_ice_servers(redirect_url)
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Too many redirects. Unable to connect to do OPTIONS request"]
                    ))
                }
            }
            status => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                [
                    "lookup_ice_servers - Unexpected response {} {:?}",
                    status,
                    self.wait(resp.bytes()).unwrap()
                ]
            )),
        }
    }

    fn set_ice_servers(&self, headermap: &HeaderMap) {
        for link in headermap.get_all("link").iter() {
            let link = link
                .to_str()
                .expect("Header value should contain only visible ASCII strings");
            // FIXME: The Demo WHIP Server appends an extra ; at the end of each link
            // but not needed as per https://datatracker.ietf.org/doc/html/rfc8288#section-3.5
            let link = link.trim_matches(';');

            let item_map = parse_link_header::parse_with_rel(link);
            if let Err(e) = item_map {
                gst::error!(CAT, "set_ice_servers {} - Error {:?}", link, e);
                continue;
            }

            let item_map = item_map.unwrap();
            if !item_map.contains_key("ice-server") {
                // Not a link header we care about
                continue;
            }

            let link = item_map.get("ice-server").unwrap();

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

            gst::debug!(CAT, "Setting STUN/TURN server {}", ice_server_url);

            // It's icer to not collapse the `else if` and its inner `if`
            #[allow(clippy::collapsible_if)]
            if link.uri.scheme() == "stun" {
                self.webrtcbin
                    .set_property_from_str("stun-server", ice_server_url.as_str());
            } else if link.uri.scheme().starts_with("turn") {
                if !self
                    .webrtcbin
                    .emit_by_name::<bool>("add-turn-server", &[&ice_server_url.as_str()])
                {
                    gst::error!(CAT, "Falied to set turn server {}", ice_server_url);
                }
            }
        }
    }

    fn send_offer(
        &self,
        offer: gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        self.webrtcbin
            .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

        if settings.whip_endpoint.is_none() {
            return Err(gst::error_msg!(
                gst::ResourceError::NotFound,
                ["Endpoint URL must be set"]
            ));
        }

        let endpoint = reqwest::Url::parse(settings.whip_endpoint.as_ref().unwrap().as_str());
        if let Err(e) = endpoint {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Could not parse endpoint URL: {}", e]
            ));
        }

        drop(settings);
        let mut state = self.state.lock().unwrap();
        *state = State::Post { redirects: 0 };
        drop(state);

        let answer = self.do_post(offer, endpoint.unwrap())?;

        self.webrtcbin
            .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);

        Ok(())
    }

    fn do_post(
        &self,
        offer: gst_webrtc::WebRTCSessionDescription,
        endpoint: reqwest::Url,
    ) -> Result<gst_webrtc::WebRTCSessionDescription, gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();

        let redirects = match *state {
            State::Post { redirects } => redirects,
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

        gst::debug!(CAT, "Using endpoint {}", endpoint.as_str());
        let mut headermap = HeaderMap::new();
        headermap.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/sdp"),
        );

        if let Some(token) = &settings.auth_token {
            let bearer_token = "Bearer ".to_owned() + token;
            drop(settings);
            headermap.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(bearer_token.as_str())
                    .expect("Failed to set auth token to header"),
            );
        }

        let future = client
            .request(reqwest::Method::POST, endpoint.as_ref())
            .headers(headermap)
            .body(body)
            .send();

        let resp = match self.wait(future) {
            Ok(r) => r,
            Err(e) => {
                return Err(e);
            }
        };

        let res = match resp.status() {
            StatusCode::OK | StatusCode::CREATED => {
                // XXX: this is a long function, we should factor it out.
                // Note: Not taking care of 'Link' headers in POST response
                // because we do a mandatory OPTIONS request before 'create-offer'
                // and update the ICE servers from response to OPTIONS

                // Get the url of the resource from 'location' header.
                // The resource created is expected be a relative path
                // and not an absolute path
                // So we want to construct the full url of the resource
                // using the endpoint url i.e., replace the end point path with
                // resource path
                let location = resp.headers().get(reqwest::header::LOCATION).unwrap();
                let mut url = reqwest::Url::parse(endpoint.as_str()).unwrap();

                url.set_path(location.to_str().unwrap());
                let mut state = self.state.lock().unwrap();
                *state = State::Running {
                    whip_resource_url: url.to_string(),
                };
                drop(state);

                let ans_bytes = match self.wait(resp.bytes()) {
                    Ok(ans) => ans.to_vec(),
                    Err(e) => return Err(e),
                };
                match sdp_message::SDPMessage::parse_buffer(&ans_bytes) {
                    Ok(ans_sdp) => {
                        let answer = gst_webrtc::WebRTCSessionDescription::new(
                            gst_webrtc::WebRTCSDPType::Answer,
                            ans_sdp,
                        );
                        Ok(answer)
                    }

                    Err(e) => Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Could not parse answer SDP: {}", e]
                    )),
                }
            }

            s if s.is_redirection() => {
                if redirects < MAX_REDIRECTS {
                    let mut state = self.state.lock().unwrap();
                    *state = State::Post {
                        redirects: redirects + 1,
                    };
                    drop(state);

                    match parse_redirect_location(resp.headers(), &endpoint) {
                        Ok(redirect_url) => {
                            gst::debug!(CAT, "Redirecting endpoint to {}", redirect_url.as_str());
                            self.do_post(offer, redirect_url)
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Too many redirects. Unable to connect to do POST"]
                    ))
                }
            }

            s if s.is_server_error() => {
                // FIXME: Check and handle 'Retry-After' header in case of server error
                Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    [
                        "Server returned error: {} - {}",
                        s.as_str(),
                        self.wait(resp.bytes())
                            .map(|x| x.escape_ascii().to_string())
                            .unwrap_or_else(|_| "(no further details)".to_string())
                    ]
                ))
            }

            s => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                [
                    "Unexpected response {:?} {:?}",
                    s,
                    self.wait(resp.bytes()).unwrap()
                ]
            )),
        };

        res
    }

    fn terminate_session(&self) {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();
        let resource_url = match *state {
            State::Running {
                ref whip_resource_url,
            } => whip_resource_url.clone(),
            _ => {
                gst::error!(CAT, "Terminated in unexpected state");
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

        gst::debug!(CAT, "DELETE request on {}", resource_url);
        let client = build_reqwest_client(reqwest::redirect::Policy::default());
        let future = client.delete(resource_url).headers(headermap).send();

        let res = self.wait(future);
        match res {
            Ok(r) => {
                gst::debug!(CAT, "Response to DELETE : {}", r.status());
            }
            Err(e) => {
                gst::error!(CAT, "{}", e);
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

        // make abortable
        let future = async {
            match future::Abortable::new(future, abort_registration).await {
                // Future resolved successfully
                Ok(Ok(res)) => Ok(res),

                // Future resolved with an error
                Ok(Err(err)) => Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Future resolved with an error {}", err.to_string()]
                )),

                // Canceller called before future resolved
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

        /* Clear out the canceller */
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
        // location url is an absolute path
        reqwest::Url::parse(location)
            .map_err(|e| gst::error_msg!(gst::ResourceError::Failed, ["{}", e.to_string()]))
    } else {
        // location url is a relative path
        let mut new_url = old_url.clone();
        new_url.set_path(location);
        Ok(new_url)
    }
}

fn build_reqwest_client(pol: Policy) -> reqwest::Client {
    let client_builder = reqwest::Client::builder();
    client_builder.redirect(pol).build().unwrap()
}
