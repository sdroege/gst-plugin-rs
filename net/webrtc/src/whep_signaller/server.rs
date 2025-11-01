// SPDX-License-Identifier: MPL-2.0

use crate::signaller::{Signallable, SignallableImpl};
use crate::utils::{build_link_header, wait_async, WaitError};
use crate::RUNTIME;
use gst::glib::{self, RustClosure};
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_sdp::SDPMessage;
use gst_webrtc::{WebRTCICEGatheringState, WebRTCSessionDescription};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;

use std::net::SocketAddr;
use tokio::sync::mpsc;
use url::Url;
use warp::{http, Filter, Reply};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "whep-server-signaller",
        gst::DebugColorFlags::empty(),
        Some("WHEP Server Signaller"),
    )
});

const DEFAULT_TIMEOUT: u32 = 30;
const DEFAULT_SEND_COUNTER_OFFER: bool = false;

const ROOT: &str = "whep";
const ENDPOINT_PATH: &str = "endpoint";
const RESOURCE_PATH: &str = "resource";
const DEFAULT_HOST_ADDR: &str = "http://127.0.0.1:9090";
const DEFAULT_STUN_SERVER: Option<&str> = Some("stun://stun.l.google.com:19303");
const CONTENT_SDP: &str = "application/sdp";
const CONTENT_TRICKLE_ICE: &str = "application/trickle-ice-sdpfrag";

struct Settings {
    stun_server: Option<String>,
    turn_servers: gst::Array,
    host_addr: Url,
    timeout: u32,
    shutdown_signal: Option<tokio::sync::oneshot::Sender<()>>,
    server_handle: Option<tokio::task::JoinHandle<()>>,
    sdp_response: HashMap<String, mpsc::Sender<Option<SDPMessage>>>,
    send_counter_offer: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host_addr: Url::parse(DEFAULT_HOST_ADDR).unwrap(),
            stun_server: DEFAULT_STUN_SERVER.map(String::from),
            turn_servers: gst::Array::new(Vec::new() as Vec<glib::SendValue>),
            timeout: DEFAULT_TIMEOUT,
            shutdown_signal: None,
            server_handle: None,
            sdp_response: HashMap::new(),
            send_counter_offer: false,
        }
    }
}

#[derive(Default)]
pub struct WhepServer {
    settings: Mutex<Settings>,
}

impl WhepServer {
    pub fn on_webrtcbin_ready(&self) -> RustClosure {
        glib::closure!(|signaller: &super::WhepServerSignaller,
                        session_id: &str,
                        webrtcbin: &gst::Element| {
            webrtcbin.connect_notify(
                Some("ice-gathering-state"),
                glib::clone!(
                    #[weak]
                    signaller,
                    #[to_owned]
                    session_id,
                    move |webrtcbin, _pspec| {
                        let state =
                            webrtcbin.property::<WebRTCICEGatheringState>("ice-gathering-state");

                        match state {
                            WebRTCICEGatheringState::Gathering => {
                                gst::info!(CAT, obj = signaller, "ICE gathering started");
                            }
                            WebRTCICEGatheringState::Complete => {
                                gst::info!(
                                    CAT,
                                    obj = signaller,
                                    "ICE gathering complete for {session_id}"
                                );
                                let ans: Option<gst_sdp::SDPMessage>;
                                let mut settings = signaller.imp().settings.lock().unwrap();
                                if let Some(answer_desc) = webrtcbin
                                    .property::<Option<WebRTCSessionDescription>>(
                                        "local-description",
                                    )
                                {
                                    ans = Some(answer_desc.sdp().to_owned());
                                } else {
                                    ans = None;
                                }

                                let tx = settings
                                    .sdp_response
                                    .remove(&session_id)
                                    .expect("SDP answer Sender needs to be valid");

                                RUNTIME.spawn(glib::clone!(
                                    #[strong]
                                    signaller,
                                    async move {
                                        if let Err(e) = tx.send(ans).await {
                                            gst::error!(
                                                CAT,
                                                obj = signaller,
                                                "Failed to send SDP {e}"
                                            );
                                        }
                                    }
                                ));
                            }
                            _ => (),
                        }
                    }
                ),
            );
        })
    }

    async fn patch_handler(
        &self,
        id: String,
        body: bytes::Bytes,
        headers: http::HeaderMap,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let mut send_counter_offer = self.settings.lock().unwrap().send_counter_offer;
        let mut ice_trickle = false;

        for h in headers {
            if let Some(name) = h.0 {
                match name {
                    http::header::CONTENT_TYPE => match h.1.to_str() {
                        Ok(CONTENT_SDP) => {
                            send_counter_offer &= true;
                        }
                        Ok(CONTENT_TRICKLE_ICE) => {
                            ice_trickle = true;
                        }
                        Ok(t) => gst::info!(CAT, imp = self, "Unhandled content type: {t}"),
                        Err(e) => gst::error!(CAT, imp = self, "Error getting content type {e}"),
                    },
                    _ => gst::info!(CAT, imp = self, "Unhandled header: {:?}", name),
                }
            }
        }

        if send_counter_offer {
            match gst_sdp::SDPMessage::parse_buffer(body.as_ref()) {
                Ok(answer_sdp) => {
                    let answer = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Answer,
                        answer_sdp,
                    );
                    self.obj()
                        .emit_by_name::<()>("session-description", &[&id, &answer]);

                    let reply = warp::reply::reply();
                    let res = warp::reply::with_status(reply, http::StatusCode::NO_CONTENT);
                    return Ok(res.into_response());
                }
                Err(err) => {
                    gst::error!(CAT, imp = self, "Could not parse answer SDP: {err}");
                    let reply = warp::reply::reply();
                    let res = warp::reply::with_status(reply, http::StatusCode::NOT_ACCEPTABLE);
                    return Ok(res.into_response());
                }
            }
        } else if ice_trickle {
            // FIXME: implement ICE Trickle and ICE restart
            // emit signal `handle-ice` to for ICE trickle
            //FIXME: add state checking once ICE trickle is implemented
        }

        let reply = warp::reply::reply();
        let res = warp::reply::with_status(reply, http::StatusCode::NOT_IMPLEMENTED);
        Ok(res.into_response())
    }

    async fn delete_handler(&self, id: String) -> Result<impl warp::Reply, warp::Rejection> {
        if self
            .obj()
            .emit_by_name::<bool>("session-ended", &[&id.as_str()])
        {
            //do nothing
            // FIXME: revisit once the return values are changed in webrtcsink/imp.rs and webrtcsrc/imp.rs
        }

        gst::info!(CAT, imp = self, "Ended session {id}");
        Ok(warp::reply::reply().into_response())
    }

    async fn options_handler(&self) -> Result<impl warp::reply::Reply, warp::Rejection> {
        let mut links = http::HeaderMap::new();
        let settings = self.settings.lock().unwrap();

        if let Some(stun) = &settings.stun_server {
            match build_link_header(stun.as_str()) {
                Ok(stun_link) => {
                    links.append(
                        http::header::LINK,
                        http::HeaderValue::from_str(stun_link.as_str()).unwrap(),
                    );
                }
                Err(e) => {
                    gst::error!(CAT, imp = self, "Failed to parse {stun:?} : {e:?}");
                }
            }
        }

        if !settings.turn_servers.is_empty() {
            for turn_server in settings.turn_servers.iter() {
                if let Ok(turn) = turn_server.get::<String>() {
                    gst::debug!(CAT, imp = self, "turn server: {}", turn.as_str());
                    match build_link_header(turn.as_str()) {
                        Ok(turn_link) => {
                            links.append(
                                http::header::LINK,
                                http::HeaderValue::from_str(turn_link.as_str()).unwrap(),
                            );
                        }
                        Err(e) => {
                            gst::error!(CAT, imp = self, "Failed to parse {turn_server:?} : {e:?}");
                        }
                    }
                } else {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Failed to get String value of {turn_server:?}"
                    );
                }
            }
        }

        let mut res = http::Response::builder()
            .header("Access-Post", CONTENT_SDP)
            .body(bytes::Bytes::new())
            .unwrap();

        let headers = res.headers_mut();
        headers.extend(links);

        Ok(res)
    }

    async fn post_handler(
        &self,
        body: bytes::Bytes,
        id: Option<String>,
    ) -> Result<impl warp::reply::Reply, warp::Rejection> {
        let session_id = match id {
            Some(id) => {
                gst::debug!(CAT, imp = self, "got session id {id} from the URL");
                id
            }
            None => {
                gst::info!(CAT, imp = self, "no session id in the URL, generating UUID");
                uuid::Uuid::new_v4().to_string()
            }
        };

        let (tx, mut rx) = mpsc::channel::<Option<SDPMessage>>(1);

        let (wait_timeout, send_counter_offer) = {
            let mut settings = self.settings.lock().unwrap();
            let wait_timeout = settings.timeout;
            let send_counter_offer = settings.send_counter_offer;
            settings.sdp_response.insert(session_id.clone(), tx);
            drop(settings);
            (wait_timeout, send_counter_offer)
        };

        let resp_code = if send_counter_offer {
            self.obj().emit_by_name::<()>(
                "session-requested",
                &[
                    &session_id,
                    &session_id,
                    &None::<gst_webrtc::WebRTCSessionDescription>,
                ],
            );
            http::StatusCode::NOT_ACCEPTABLE
        } else {
            match gst_sdp::SDPMessage::parse_buffer(body.as_ref()) {
                Ok(offer_sdp) => {
                    let offer = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Offer,
                        offer_sdp,
                    );
                    self.obj().emit_by_name::<()>(
                        "session-requested",
                        &[&session_id, &session_id, &offer],
                    );
                    http::StatusCode::CREATED
                }
                Err(err) => {
                    gst::error!(CAT, imp = self, "Could not parse offer SDP: {err}");
                    let reply = warp::reply::reply();
                    let res = warp::reply::with_status(reply, http::StatusCode::NOT_ACCEPTABLE);
                    return Ok(res.into_response());
                }
            }
        };

        let canceller = Mutex::new(None);
        let result = wait_async(&canceller, rx.recv(), wait_timeout).await;

        let response_sdp = match result {
            Ok(resp) => match resp {
                Some(a) => a,
                None => {
                    let err = "Channel closed, can't receive SDP".to_owned();
                    let res = http::Response::builder()
                        .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                        .body(err.into())
                        .unwrap();

                    return Ok(res);
                }
            },
            Err(e) => {
                let err = match e {
                    WaitError::FutureAborted => "Aborted".to_owned(),
                    WaitError::FutureError(err) => err.to_string(),
                };
                let res = http::Response::builder()
                    .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                    .body(err.into())
                    .unwrap();

                return Ok(res);
            }
        };

        let settings = self.settings.lock().unwrap();
        let mut links = http::HeaderMap::new();

        if let Some(stun) = &settings.stun_server {
            match build_link_header(stun.as_str()) {
                Ok(stun_link) => {
                    links.append(
                        http::header::LINK,
                        http::HeaderValue::from_str(stun_link.as_str()).unwrap(),
                    );
                }
                Err(e) => {
                    gst::error!(CAT, imp = self, "Failed to parse {stun:?} : {e:?}");
                }
            }
        }

        if !settings.turn_servers.is_empty() {
            for turn_server in settings.turn_servers.iter() {
                if let Ok(turn) = turn_server.get::<String>() {
                    gst::debug!(CAT, imp = self, "turn server: {}", turn.as_str());
                    match build_link_header(turn.as_str()) {
                        Ok(turn_link) => {
                            links.append(
                                http::header::LINK,
                                http::HeaderValue::from_str(turn_link.as_str()).unwrap(),
                            );
                        }
                        Err(e) => {
                            gst::error!(CAT, imp = self, "Failed to parse {turn_server:?} : {e:?}");
                        }
                    }
                } else {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed to get String value of {turn_server:?}"
                    );
                }
            }
        }

        // Note: including the ETag in the original "201 Created" response is only REQUIRED
        // if the WHEP resource supports ICE restarts and OPTIONAL otherwise.
        let sdp_text = if let Some(sdp) = response_sdp {
            match sdp.as_text() {
                Ok(text) => {
                    gst::debug!(CAT, imp = self, "{text:?}");
                    // TODO: any case we want to reject the WHEP player's offer and provide counter offer, based on our answer
                    Ok(text)
                }
                Err(e) => {
                    gst::error!(CAT, imp = self, "{e:?}");
                    Err(format!("Failed to get SDP answer: {e:?}"))
                }
            }
        } else {
            let e = "SDP Answer is empty!".to_string();
            gst::error!(CAT, imp = self, "{e:?}");
            Err(e)
        };

        let res = match sdp_text {
            Ok(sdp) => {
                let resource_url = "/".to_owned() + ROOT + "/" + RESOURCE_PATH + "/" + &session_id;
                let mut res = http::Response::builder()
                    .status(resp_code)
                    .header(http::header::CONTENT_TYPE, CONTENT_SDP)
                    .header("location", resource_url)
                    .body(sdp.into());

                if let Ok(ref mut r) = res {
                    r.headers_mut().extend(links)
                }
                res
            }
            Err(e) => {
                // If sdp_text is an error. Send error code and error string in the response
                http::Response::builder()
                    .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                    .body(e.into())
            }
        };

        match res {
            Err(e) => {
                gst::error!(CAT, imp = self, "Error building response : {e}");
                Err(warp::reject())
            }
            Ok(r) => Ok(r),
        }
    }

    fn serve(&self) -> Option<tokio::task::JoinHandle<()>> {
        let mut settings = self.settings.lock().unwrap();
        let addr: SocketAddr;
        match settings.host_addr.socket_addrs(|| None) {
            Ok(v) => {
                // pick the first vector item
                addr = v[0];
                gst::info!(CAT, imp = self, "using {addr:?} as address");
            }
            Err(e) => {
                gst::error!(CAT, imp = self, "error getting addr from uri  {e:?}");
                self.obj()
                    .emit_by_name::<()>("error", &[&format!("Unable to start WHEP Server: {e:?}")]);
                return None;
            }
        }

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        settings.shutdown_signal = Some(tx);
        drop(settings);

        let api = self.filter();

        let jh = RUNTIME.spawn(async move {
            warp::serve(api)
                .bind(addr)
                .await
                .graceful(async move {
                    match rx.await {
                        Ok(_) => gst::debug!(CAT, "Server shut down signal received"),
                        Err(e) => gst::error!(CAT, "{e:?}: Sender dropped"),
                    }
                })
                .run()
                .await;

            gst::debug!(CAT, "Stopped the server task...");
        });

        gst::debug!(CAT, imp = self, "Started the server...");
        Some(jh)
    }

    fn set_host_addr(&self, host_addr: &str) -> Result<(), url::ParseError> {
        let mut settings = self.settings.lock().unwrap();
        settings.host_addr = Url::parse(host_addr)?;
        Ok(())
    }

    fn filter(&self) -> impl Filter<Extract = impl warp::Reply> + Clone + Send + Sync + 'static {
        let prefix = warp::path(ROOT);

        // POST /endpoint
        let post_filter = warp::post()
            .and(warp::path(ENDPOINT_PATH))
            .and(warp::path::end())
            .and(warp::header::exact(
                http::header::CONTENT_TYPE.as_str(),
                CONTENT_SDP,
            ))
            .and(warp::body::bytes())
            .and_then(glib::clone!(
                #[weak(rename_to = s)]
                self,
                #[upgrade_or_panic]
                move |body| async move { s.post_handler(body, None).await }
            ));

        // POST /endpoint/<session-id>
        let post_filter_with_id = warp::post()
            .and(warp::path(ENDPOINT_PATH))
            .and(warp::path::param::<String>())
            .and(warp::path::end())
            .and(warp::header::exact(
                http::header::CONTENT_TYPE.as_str(),
                CONTENT_SDP,
            ))
            .and(warp::body::bytes())
            .and_then(glib::clone!(
                #[weak(rename_to = self_)]
                self,
                #[upgrade_or_panic]
                move |id, body| async move { self_.post_handler(body, Some(id)).await }
            ));

        // OPTIONS /endpoint
        let options_filter = warp::options()
            .and(warp::path(ENDPOINT_PATH))
            .and(warp::path::end())
            .and_then(glib::clone!(
                #[weak(rename_to = s)]
                self,
                #[upgrade_or_panic]
                move || async move { s.options_handler().await }
            ));

        // PATCH /resource/:id
        let patch_filter = warp::patch()
            .and(warp::path(RESOURCE_PATH))
            .and(warp::path::param::<String>())
            .and(warp::path::end())
            .and(warp::body::bytes())
            .and(warp::header::headers_cloned())
            .and_then(glib::clone!(
                #[weak(rename_to = s)]
                self,
                #[upgrade_or_panic]
                move |id, body, headers| async move { s.patch_handler(id, body, headers).await }
            ));

        // DELETE /resource/:id
        let delete_filter = warp::delete()
            .and(warp::path(RESOURCE_PATH))
            .and(warp::path::param::<String>())
            .and(warp::path::end())
            .and_then(glib::clone!(
                #[weak(rename_to = s)]
                self,
                #[upgrade_or_panic]
                move |id| async move { s.delete_handler(id).await }
            ));

        prefix
            .and(post_filter.or(post_filter_with_id))
            .or(prefix.and(options_filter))
            .or(prefix.and(patch_filter))
            .or(prefix.and(delete_filter))
    }
}

impl SignallableImpl for WhepServer {
    fn start(&self) {
        gst::info!(CAT, imp = self, "starting the WHEP server");
        let jh = self.serve();
        let mut settings = self.settings.lock().unwrap();
        settings.server_handle = jh;
    }

    fn stop(&self) {
        let mut settings = self.settings.lock().unwrap();

        let handle = settings
            .server_handle
            .take()
            .expect("Server handle should be set");

        let tx = settings
            .shutdown_signal
            .take()
            .expect("Shutdown signal Sender needs to be valid");

        if tx.send(()).is_err() {
            gst::error!(
                CAT,
                imp = self,
                "Failed to send shutdown signal. Receiver dropped"
            );
        }

        gst::debug!(CAT, imp = self, "Await server handle to join");
        RUNTIME.block_on(async {
            if let Err(e) = handle.await {
                gst::error!(CAT, imp = self, "Failed to join server handle: {e:?}");
            };
        });

        gst::info!(CAT, imp = self, "stopped the WHEP server");
    }

    fn end_session(&self, _session_id: &str) {
        //FIXME: send any events to the client
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WhepServer {
    const NAME: &'static str = "GstWhepServerSignaller";
    type Type = super::WhepServerSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for WhepServer {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("host-addr")
                    .nick("Host address")
                    .blurb("The host address of the WHEP endpoint e.g., http://127.0.0.1:9090")
                    .default_value(DEFAULT_HOST_ADDR)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("stun-server")
                    .nick("STUN Server")
                    .blurb("The STUN server of the form stun://hostname:port")
                    .default_value(DEFAULT_STUN_SERVER)
                    .build(),
                gst::ParamSpecArray::builder("turn-servers")
                    .nick("List of TURN Servers to use")
                    .blurb("The TURN servers of the form <\"turn(s)://username:password@host:port\", \"turn(s)://username1:password1@host1:port1\">")
                    .element_spec(&glib::ParamSpecString::builder("turn-server")
                        .nick("TURN Server")
                        .blurb("The TURN server of the form turn(s)://username:password@host:port.")
                        .build()
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout WHEP endpoint requests (0 = No timeout).")
                    .maximum(3600)
                    .default_value(DEFAULT_TIMEOUT)
                    .build(),
                glib::ParamSpecBoolean::builder("manual-sdp-munging")
                    .nick("Manual SDP munging")
                    .blurb("Whether the signaller manages SDP munging itself")
                    .default_value(false)
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("send-counter-offer")
                    .nick("Send Counter offer")
                    .blurb("Reject the offer sent by the WHEP player and propose a counter offer")
                    .default_value(DEFAULT_SEND_COUNTER_OFFER)
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "host-addr" => {
                if let Err(e) =
                    self.set_host_addr(value.get::<&str>().expect("type checked upstream"))
                {
                    gst::error!(CAT, "Couldn't set the host address as {e:?}, fallback to the default value {DEFAULT_HOST_ADDR:?}");
                }
            }
            "stun-server" => {
                let mut settings = self.settings.lock().unwrap();
                settings.stun_server = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
            }
            "turn-servers" => {
                let mut settings = self.settings.lock().unwrap();
                settings.turn_servers = value.get::<gst::Array>().expect("type checked upstream")
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                settings.timeout = value.get().unwrap();
            }
            "send-counter-offer" => {
                let mut settings = self.settings.lock().unwrap();
                settings.send_counter_offer = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "host-addr" => settings.host_addr.to_string().to_value(),
            "stun-server" => settings.stun_server.to_value(),
            "turn-servers" => settings.turn_servers.to_value(),
            "timeout" => settings.timeout.to_value(),
            "manual-sdp-munging" => false.to_value(),
            "send-counter-offer" => settings.send_counter_offer.to_value(),
            _ => unimplemented!(),
        }
    }
}
