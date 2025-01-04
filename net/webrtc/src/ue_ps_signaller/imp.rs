// SPDX-License-Identifier: MPL-2.0

use super::protocol as p;
use crate::RUNTIME;
use crate::signaller::{Signallable, SignallableImpl};
use crate::utils::{create_tls_connector, gvalue_to_json};
use anyhow::{Error, anyhow};
use async_tungstenite::tungstenite::Message as WsMessage;
use async_tungstenite::tungstenite::client::IntoClientRequest;
use async_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use futures::channel::mpsc;
use futures::prelude::*;
use gst::glib;
use gst::glib::prelude::*;
use gst::subclass::prelude::*;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::str::FromStr;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use tokio::{task, time::timeout};
use url::Url;

const DEFAULT_INSECURE_TLS: bool = false;

pub struct Settings {
    uri: Url,
    streamer_id: Option<String>,
    cafile: Option<String>,
    headers: Option<gst::Structure>,
    insecure_tls: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            uri: Url::from_str("ws://127.0.0.1:8888").unwrap(),
            streamer_id: None,
            cafile: Default::default(),
            headers: None,
            insecure_tls: DEFAULT_INSECURE_TLS,
        }
    }
}

#[derive(Default)]
pub struct Signaller {
    state: Mutex<State>,
    medias: Mutex<Vec<String>>,
    settings: Mutex<Settings>,
}

#[derive(Default)]
struct State {
    /// Sender for the websocket messages
    websocket_sender: Option<mpsc::Sender<p::Message>>,
    connect_task_handle: Option<task::JoinHandle<()>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    receive_task_handle: Option<task::JoinHandle<()>>,
    producers: HashSet<String>,
    streamer_id: Option<String>,
}

pub static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc-ue-ps-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC signaller"),
    )
});

impl Signaller {
    fn uri(&self) -> Url {
        self.settings.lock().unwrap().uri.clone()
    }

    fn set_uri(&self, uri: &str) -> Result<(), Error> {
        let mut settings = self.settings.lock().unwrap();
        let uri = Url::from_str(uri).map_err(|err| anyhow!("{err:?}"))?;

        settings.uri = uri;

        Ok(())
    }

    fn streamer_id(&self) -> Option<String> {
        self.settings.lock().unwrap().streamer_id.clone()
    }

    fn set_streamer_id(&self, streamer_id: Option<String>) -> Result<(), Error> {
        let mut settings = self.settings.lock().unwrap();

        settings.streamer_id = streamer_id;

        Ok(())
    }

    async fn connect(&self) -> Result<(), Error> {
        let (cafile, insecure_tls) = {
            let settings = self.settings.lock().unwrap();
            (settings.cafile.clone(), settings.insecure_tls)
        };

        if insecure_tls {
            gst::warning!(CAT, imp = self, "insecure tls connections are allowed");
        }

        let connector = create_tls_connector(cafile, insecure_tls)
            .map_ok(Some)
            .await?;
        let mut uri = self.uri();
        uri.set_query(None);

        gst::info!(CAT, imp = self, "connecting to {}", uri.to_string());

        let mut req = uri.into_client_request()?;
        let req_headers = req.headers_mut();
        if let Some(headers) = self.headers() {
            for (key, value) in headers {
                req_headers.insert(
                    HeaderName::from_bytes(key.as_bytes()).unwrap(),
                    HeaderValue::from_bytes(value.as_bytes()).unwrap(),
                );
            }
        }

        let (ws, _) = timeout(
            // FIXME: Make the timeout configurable
            Duration::from_secs(20),
            async_tungstenite::tokio::connect_async_with_tls_connector(req, connector),
        )
        .await??;

        gst::info!(CAT, imp = self, "connected");

        // Channel for asynchronously sending out websocket message
        let (mut ws_sink, mut ws_stream) = ws.split();

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (websocket_sender, mut websocket_receiver) = mpsc::channel::<p::Message>(1000);
        let send_task_handle = RUNTIME.spawn(glib::clone!(
            #[to_owned(rename_to = this)]
            self,
            async move {
                let mut res = Ok(());
                while let Some(msg) = websocket_receiver.next().await {
                    gst::log!(CAT, "Sending websocket message {:?}", msg);
                    res = ws_sink
                        .send(WsMessage::text(serde_json::to_string(&msg).unwrap()))
                        .await;

                    if let Err(ref err) = res {
                        gst::error!(CAT, imp = this, "Quitting send loop: {err}");
                        break;
                    }
                }

                gst::debug!(CAT, imp = this, "Done sending");

                let _ = ws_sink.close(None).await;

                res.map_err(Into::into)
            }
        ));

        let obj = self.obj();
        let meta =
            if let Some(meta) = obj.emit_by_name::<Option<gst::Structure>>("request-meta", &[]) {
                gvalue_to_json(&meta.to_value())
            } else {
                None
            };

        let receive_task_handle = RUNTIME.spawn(glib::clone!(
            #[to_owned(rename_to = this)]
            self,
            async move {
                while let Some(msg) = tokio_stream::StreamExt::next(&mut ws_stream).await {
                    if let ControlFlow::Break(_) = this.handle_message(msg, &meta) {
                        break;
                    }
                }

                let msg = "Stopped websocket receiving";
                gst::info!(CAT, imp = this, "{msg}");
            }
        ));

        let mut state = self.state.lock().unwrap();
        state.websocket_sender = Some(websocket_sender);
        state.send_task_handle = Some(send_task_handle);
        state.receive_task_handle = Some(receive_task_handle);

        Ok(())
    }

    fn identify(&self, _meta: &Option<serde_json::Value>) {
        let Some(streamer_id) = self.streamer_id() else {
            self.obj().emit_by_name::<()>(
                "error",
                &[&format!(
                    "{:?}",
                    anyhow!("Error: signaller::streamer-id was not set")
                )],
            );
            return;
        };

        self.send(p::Message::EndpointId(p::EndpointId {
            id: streamer_id,
            protocol_version: Some("1.0.0".to_string()),
        }));
    }

    fn headers(&self) -> Option<HashMap<String, String>> {
        self.settings
            .lock()
            .unwrap()
            .headers
            .as_ref()
            .map(|structure| {
                let mut hash = HashMap::new();

                for (key, value) in structure.iter() {
                    if let Ok(Ok(value_str)) = value.transform::<String>().map(|v| v.get()) {
                        gst::log!(CAT, imp = self, "headers '{}' -> '{}'", key, value_str);
                        hash.insert(key.to_string(), value_str);
                    } else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Failed to convert headers '{}' to string ('{:?}')",
                            key,
                            value
                        );
                    }
                }

                hash
            })
    }

    fn send(&self, msg: p::Message) {
        let state = self.state.lock().unwrap();
        if let Some(mut sender) = state.websocket_sender.clone() {
            RUNTIME.spawn(glib::clone!(
                #[to_owned(rename_to = this)]
                self,
                async move {
                    if let Err(err) = sender.send(msg).await {
                        this.obj()
                            .emit_by_name::<()>("error", &[&format!("Error: {}", err)]);
                    }
                }
            ));
        }
    }

    fn handle_message(
        &self,
        msg: Result<WsMessage, async_tungstenite::tungstenite::Error>,
        meta: &Option<serde_json::Value>,
    ) -> ControlFlow<()> {
        match msg {
            Ok(WsMessage::Text(msg)) => {
                gst::trace!(CAT, imp = self, "Received message {}", msg);

                if let Ok(msg) = serde_json::from_str::<p::Message>(&msg) {
                    match msg {
                        p::Message::Identify(_) => {
                            self.identify(meta);
                        }
                        p::Message::EndpointIdConfirm(endpoint_id_confirm) => {
                            let mut state = self.state.lock().unwrap();
                            state.streamer_id = Some(endpoint_id_confirm.committed_id);
                            drop(state);
                        }
                        p::Message::PlayerConnected(player_connected) => {
                            self.obj().emit_by_name::<()>(
                                "session-requested",
                                &[
                                    &player_connected.player_id,
                                    &player_connected.player_id,
                                    &None::<gst_webrtc::WebRTCSessionDescription>,
                                ],
                            );
                        }
                        p::Message::PlayerDisconnected(player_disconnected) => {
                            gst::info!(
                                CAT,
                                imp = self,
                                "Session {} ended",
                                player_disconnected.player_id
                            );

                            self.obj().emit_by_name::<bool>(
                                "session-ended",
                                &[&player_disconnected.player_id],
                            );
                        }
                        p::Message::Offer(offer) => {
                            if let Some(player_id) = &offer.player_id {
                                let (sdp, desc_type) =
                                    (offer.sdp, gst_webrtc::WebRTCSDPType::Offer);
                                let sdp = match gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()) {
                                    Ok(sdp) => sdp,
                                    Err(err) => {
                                        self.obj().emit_by_name::<()>(
                                            "error",
                                            &[&format!("Error parsing SDP: {sdp} {err:?}")],
                                        );

                                        return ControlFlow::Break(());
                                    }
                                };

                                let desc =
                                    gst_webrtc::WebRTCSessionDescription::new(desc_type, sdp);
                                self.obj()
                                    .emit_by_name::<()>("session-description", &[player_id, &desc]);
                            }
                        }
                        p::Message::Answer(offer) => {
                            if let Some(player_id) = &offer.player_id {
                                let (sdp, desc_type) =
                                    (offer.sdp, gst_webrtc::WebRTCSDPType::Answer);
                                let sdp = match gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()) {
                                    Ok(sdp) => sdp,
                                    Err(err) => {
                                        self.obj().emit_by_name::<()>(
                                            "error",
                                            &[&format!("Error parsing SDP: {sdp} {err:?}")],
                                        );

                                        return ControlFlow::Break(());
                                    }
                                };

                                let desc =
                                    gst_webrtc::WebRTCSessionDescription::new(desc_type, sdp);
                                self.obj()
                                    .emit_by_name::<()>("session-description", &[player_id, &desc]);
                            }
                        }
                        p::Message::IceCandidate(ice_candidate) => {
                            if let (Some(player_id), Some(candidate)) =
                                (&ice_candidate.player_id, &ice_candidate.candidate)
                            {
                                if let Ok(sdp_m_line_index) =
                                    u32::try_from(candidate.sdp_m_line_index)
                                {
                                    self.obj().emit_by_name::<()>(
                                        "handle-ice",
                                        &[
                                            player_id,
                                            &sdp_m_line_index,
                                            &candidate.sdp_mid,
                                            &candidate.candidate,
                                        ],
                                    );
                                } else {
                                    gst::warning!(
                                        CAT,
                                        imp = self,
                                        "Invalid sdp_m_line_index: {}",
                                        candidate.sdp_m_line_index
                                    );
                                };
                            }
                        }
                        p::Message::Ping(ping) => {
                            self.send(p::Message::Pong(p::Pong { time: ping.time }));
                        }
                        _ => {
                            gst::warning!(CAT, imp = self, "Unhandled message {:#?}", msg);
                        }
                    }
                } else {
                    gst::error!(CAT, imp = self, "Unknown message from server: {}", msg);

                    self.obj().emit_by_name::<()>(
                        "error",
                        &[&format!("Unknown message from server: {}", msg)],
                    );
                }
            }
            Ok(WsMessage::Close(reason)) => {
                gst::info!(CAT, imp = self, "websocket connection closed: {:?}", reason);
                return ControlFlow::Break(());
            }
            Ok(_) => (),
            Err(err) => {
                self.obj()
                    .emit_by_name::<()>("error", &[&format!("Error receiving: {}", err)]);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "GstPixelStreamingWebRTCSignaller";
    type Type = super::UePsSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for Signaller {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("manual-sdp-munging")
                    .nick("Manual SDP munging")
                    .blurb("Whether the signaller manages SDP munging itself")
                    .default_value(false)
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("uri")
                    .nick("Signaller URI")
                    .blurb("URI for connecting to the signaller server")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("producer-peer-id")
                    .nick("Producer peer id")
                    .blurb("The peer id of the producer transmitted to the signaller server")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("cafile")
                    .nick("Certificate Authority (CA) file")
                    .blurb("Certificate file used in TLS session")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("streamer-id")
                    .nick("Streamer id")
                    .blurb("The streamer id transmitted to the signaller server")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("headers")
                    .nick("HTTP headers")
                    .blurb("HTTP headers sent during the connection handshake")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                /**
                 * GstWebRTCSignaller::insecure-tls:
                 *
                 * Enables insecure TLS connections. Disabled by default.
                 */
                glib::ParamSpecBoolean::builder("insecure-tls")
                    .nick("Insecure TLS")
                    .blurb("Whether insecure TLS connections are allowed")
                    .default_value(DEFAULT_INSECURE_TLS)
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
            ]
        });

        PROPS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "uri" => {
                if let Err(e) = self.set_uri(value.get::<&str>().expect("type checked upstream")) {
                    gst::error!(CAT, "Couldn't set URI: {e:?}");
                }
            }
            "streamer-id" => {
                if let Err(e) = self
                    .set_streamer_id(Some(value.get::<String>().expect("type checked upstream")))
                {
                    gst::error!(CAT, "Couldn't set streamer-id: {e:?}");
                }
            }
            "cafile" => {
                self.settings.lock().unwrap().cafile = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
            }
            "headers" => {
                self.settings.lock().unwrap().headers = value
                    .get::<Option<gst::Structure>>()
                    .expect("type checked upstream")
            }
            "insecure-tls" => {
                self.settings.lock().unwrap().insecure_tls =
                    value.get::<bool>().expect("type checked upstream")
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "manual-sdp-munging" => false.to_value(),
            "uri" => settings.uri.to_string().to_value(),
            "streamer-id" => self.state.lock().unwrap().streamer_id.to_value(),
            "cafile" => settings.cafile.to_value(),
            "headers" => settings.headers.to_value(),
            "insecure-tls" => settings.insecure_tls.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl SignallableImpl for Signaller {
    fn start(&self) {
        gst::info!(CAT, imp = self, "Starting");

        let mut state = self.state.lock().unwrap();
        let connect_task_handle = RUNTIME.spawn(glib::clone!(
            #[to_owned(rename_to = this)]
            self,
            async move {
                if let Err(err) = this.connect().await {
                    this.obj()
                        .emit_by_name::<()>("error", &[&format!("Error receiving: {}", err)]);
                }
            }
        ));

        state.connect_task_handle = Some(connect_task_handle);
    }

    fn stop(&self) {
        gst::info!(CAT, imp = self, "Stopping now");

        let mut state = self.state.lock().unwrap();

        // First make sure the connect task is stopped if it is still
        // running
        let connect_task_handle = state.connect_task_handle.take();
        if let Some(handle) = connect_task_handle {
            RUNTIME.block_on(async move {
                handle.abort();
                let _ = handle.await;
            });
        }

        let send_task_handle = state.send_task_handle.take();
        let receive_task_handle = state.receive_task_handle.take();
        if let Some(mut sender) = state.websocket_sender.take() {
            RUNTIME.block_on(async move {
                sender.close_channel();

                if let Some(handle) = send_task_handle
                    && let Err(err) = handle.await
                {
                    gst::warning!(CAT, imp = self, "Error while joining send task: {}", err);
                }

                if let Some(handle) = receive_task_handle {
                    handle.abort();
                    let _ = handle.await;
                }
            });
        }
        state.producers.clear();
    }

    fn send_sdp(&self, session_id: &str, sdp: &gst_webrtc::WebRTCSessionDescription) {
        gst::debug!(CAT, imp = self, "Sending SDP {sdp:#?}");

        // store medias "mid" for each medias (or "" if no mid)
        let mut medias = self.medias.lock().unwrap();
        medias.clear();
        for media in sdp.sdp().medias() {
            let value = media.attribute_val("mid");
            medias.push(value.map(|s| s.to_string()).unwrap_or_default());
        }
        drop(medias);

        let msg = {
            if sdp.type_() == gst_webrtc::WebRTCSDPType::Offer {
                p::Message::Offer(p::Offer {
                    sdp: sdp.sdp().as_text().unwrap(),
                    player_id: Some(session_id.to_string()),
                    sfu: None,
                })
            } else {
                p::Message::Answer(p::Answer {
                    sdp: sdp.sdp().as_text().unwrap(),
                    player_id: Some(session_id.to_string()),
                })
            }
        };

        self.send(msg);
    }

    fn add_ice(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: u32,
        _sdp_mid: Option<String>,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Adding ice candidate {candidate:?} for {sdp_m_line_index:?} on session {session_id}"
        );

        let medias = self.medias.lock().unwrap();
        let index = sdp_m_line_index as usize;
        let sdp_mid = medias.get(index).cloned();
        drop(medias);

        let Ok(sdp_m_line_index) = sdp_m_line_index.try_into() else {
            gst::warning!(
                CAT,
                imp = self,
                "Invalid sdp_m_line_index: {}",
                sdp_m_line_index
            );
            return;
        };

        let msg = p::Message::IceCandidate(p::IceCandidate {
            player_id: Some(session_id.to_string()),
            candidate: Some(p::IceCandidateData {
                candidate: candidate.to_string(),
                sdp_mid: sdp_mid.unwrap_or("".to_string()),
                sdp_m_line_index,
                username_fragment: None,
            }),
        });

        self.send(msg);
    }

    fn end_session(&self, session_id: &str) {
        gst::debug!(CAT, imp = self, "Signalling session done {}", session_id);

        let state = self.state.lock().unwrap();
        let session_id = session_id.to_string();
        if let Some(mut sender) = state.websocket_sender.clone() {
            RUNTIME.spawn(glib::clone!(
                #[to_owned(rename_to = this)]
                self,
                async move {
                    if let Err(err) = sender
                        .send(p::Message::DisconnectPlayer(p::DisconnectPlayer {
                            player_id: session_id.to_string(),
                            reason: None,
                        }))
                        .await
                    {
                        this.obj()
                            .emit_by_name::<()>("error", &[&format!("Error: {}", err)]);
                    }
                }
            ));
        }
    }
}
