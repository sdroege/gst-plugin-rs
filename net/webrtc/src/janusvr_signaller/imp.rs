// SPDX-License-Identifier: MPL-2.0

use crate::{
    signaller::{Signallable, SignallableImpl},
    webrtcsink::JanusVRSignallerState,
    RUNTIME,
};

use anyhow::{anyhow, Error};
use async_tungstenite::tungstenite;
use futures::channel::mpsc;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use gst::glib;
use gst::glib::{subclass::Signal, Properties};
use gst::prelude::*;
use gst::subclass::prelude::*;
use http::Uri;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::ops::ControlFlow;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use tokio::{task, time::timeout};
use tungstenite::Message as WsMessage;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc-janusvr-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC Janus Video Room signaller"),
    )
});

fn transaction_id() -> String {
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .map(char::from)
        .take(30)
        .collect()
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(untagged)]
/// Ids are either u64 (default) or string in Janus, depending of the
/// `string_ids` configuration in the videoroom plugin config file.
pub(crate) enum JanusId {
    Str(String),
    Num(u64),
}

// Should never reach the panic as Janus will error if trying to use a room ID of the wrong type
impl JanusId {
    fn as_string(&self) -> String {
        match self {
            JanusId::Str(s) => s.clone(),
            JanusId::Num(_) => panic!("IDs from Janus are meant to be strings, not numbers"),
        }
    }

    fn as_num(&self) -> u64 {
        match self {
            JanusId::Str(_) => panic!("IDs from Janus are meant to be numbers, not strings"),
            JanusId::Num(n) => *n,
        }
    }
}

impl std::fmt::Display for JanusId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JanusId::Str(s) => write!(f, "{s}"),
            JanusId::Num(n) => write!(f, "{n}"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
enum Jsep {
    Offer {
        sdp: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        trickle: Option<bool>,
    },
    Answer {
        sdp: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        trickle: Option<bool>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Candidate {
    candidate: String,
    #[serde(rename = "sdpMLineIndex")]
    sdp_m_line_index: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "janus")]
#[serde(rename_all = "lowercase")]
enum OutgoingMessage {
    KeepAlive {
        transaction: String,
        session_id: u64,
        apisecret: Option<String>,
    },
    Create {
        transaction: String,
        apisecret: Option<String>,
    },
    Attach {
        transaction: String,
        plugin: String,
        session_id: u64,
        apisecret: Option<String>,
    },
    Message {
        transaction: String,
        session_id: u64,
        handle_id: u64,
        apisecret: Option<String>,
        body: MessageBody,
        #[serde(skip_serializing_if = "Option::is_none")]
        jsep: Option<Jsep>,
    },
    Trickle {
        transaction: String,
        session_id: u64,
        handle_id: u64,
        apisecret: Option<String>,
        candidate: Candidate,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "request")]
#[serde(rename_all = "snake_case")]
enum MessageBody {
    Join(Join),
    Publish,
    Leave,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "ptype")]
#[serde(rename_all = "lowercase")]
enum Join {
    Publisher {
        room: JanusId,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<JanusId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
struct InnerError {
    code: i32,
    reason: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct InnerHangup {
    session_id: JanusId,
    sender: JanusId,
    reason: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct InnerSlowLink {
    session_id: u64,
    sender: u64,
    opaque_id: Option<String>,
    mid: String,
    media: String,
    uplink: bool,
    lost: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct RoomJoined {
    room: JanusId,
    id: JanusId,
}

#[derive(Serialize, Deserialize, Debug)]
struct RoomEvent {
    room: Option<JanusId>,
    error_code: Option<i32>,
    error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct RoomDestroyed {
    room: JanusId,
}

#[derive(Serialize, Deserialize, Debug)]
struct RoomTalking {
    room: JanusId,
    id: JanusId,
    #[serde(rename = "audio-level-dBov-avg")]
    audio_level: f32,
}

#[derive(Serialize, Deserialize, Debug)]
struct SlowLink {
    #[serde(rename = "current-bitrate")]
    current_bitrate: u32,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "videoroom", rename_all = "kebab-case")]
enum VideoRoomData {
    Joined(RoomJoined),
    Event(RoomEvent),
    Destroyed(RoomDestroyed),
    Talking(RoomTalking),
    StoppedTalking(RoomTalking),
    #[serde(rename = "slow_link")]
    SlowLink(SlowLink),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "plugin")]
enum PluginData {
    #[serde(rename = "janus.plugin.videoroom")]
    VideoRoom { data: VideoRoomData },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct DataHolder {
    id: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct SuccessMsg {
    transaction: Option<String>,
    session_id: Option<u64>,
    data: Option<DataHolder>,
}

#[derive(Serialize, Deserialize, Debug)]
struct EventMsg {
    transaction: Option<String>,
    session_id: Option<u64>,
    plugindata: Option<PluginData>,
    jsep: Option<Jsep>,
}

// IncomingMessage
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "janus", rename_all = "lowercase")]
enum JsonReply {
    Ack,
    Success(SuccessMsg),
    Event(EventMsg),
    WebRTCUp,
    Media,
    Error(InnerError),
    HangUp(InnerHangup),
    SlowLink(InnerSlowLink),
}

#[derive(Default)]
struct State {
    ws_sender: Option<mpsc::Sender<OutgoingMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    recv_task_handle: Option<task::JoinHandle<()>>,
    session_id: Option<u64>,
    handle_id: Option<u64>,
    room_id: Option<JanusId>,
    feed_id: Option<JanusId>,
    leave_room_rx: Option<tokio::sync::oneshot::Receiver<()>>,
}

// Mutex order:
// - self.state
// - self.settings

#[derive(Clone)]
struct Settings {
    janus_endpoint: String,
    room_id: Option<JanusId>,
    feed_id: Option<JanusId>,
    display_name: Option<String>,
    secret_key: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            janus_endpoint: "ws://127.0.0.1:8188".to_string(),
            display_name: None,
            room_id: None,
            feed_id: None,
            secret_key: None,
        }
    }
}

#[derive(Default, Properties)]
#[properties(wrapper_type = super::JanusVRSignaller)]
pub struct Signaller {
    state: Mutex<State>,
    #[property(name="manual-sdp-munging", default = false, get = |_| false, type = bool, blurb = "Whether the signaller manages SDP munging itself")]
    #[property(name="janus-endpoint", get, set, type = String, member = janus_endpoint, blurb = "The Janus server endpoint to POST SDP offer to")]
    #[property(name="display-name", get, set, type = String, member = display_name, blurb = "The name of the publisher in the Janus Video Room")]
    #[property(name="secret-key", get, set, type = String, member = secret_key, blurb = "The secret API key to communicate with Janus server")]
    // Properties whose type depends of the Janus ID format (u64 or string) are implemented in Signaller subclasses
    settings: Mutex<Settings>,
}

impl Signaller {
    fn raise_error(&self, msg: String) {
        self.obj()
            .emit_by_name::<()>("error", &[&format!("Error: {msg}")]);
    }

    async fn connect(&self) -> Result<(), Error> {
        let settings = self.settings.lock().unwrap().clone();
        use tungstenite::client::IntoClientRequest;
        let mut request = settings
            .janus_endpoint
            .parse::<Uri>()?
            .into_client_request()?;
        request.headers_mut().append(
            "Sec-WebSocket-Protocol",
            http::HeaderValue::from_static("janus-protocol"),
        );

        let (ws, _) = timeout(
            // FIXME: Make the timeout configurable
            Duration::from_secs(20),
            async_tungstenite::tokio::connect_async(request),
        )
        .await??;

        // Channel for asynchronously sending out websocket message
        let (mut ws_sink, mut ws_stream) = ws.split();

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (ws_sender, mut ws_receiver) = mpsc::channel::<OutgoingMessage>(1000);
        let send_task_handle = RUNTIME.spawn(glib::clone!(
            #[to_owned(rename_to = this)]
            self,
            async move {
                let mut res = Ok(());
                loop {
                    tokio::select! {
                        opt = ws_receiver.next() => match opt {
                            Some(msg) => {
                                gst::trace!(CAT, "Sending websocket message {:?}", msg);
                                res = ws_sink
                                    .send(WsMessage::text(serde_json::to_string(&msg).unwrap()))
                                    .await;
                            },
                            None => break,
                        },
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {
                                let (session_id, apisecret) = {
                                    let state = this.state.lock().unwrap();
                                    let settings = this.settings.lock().unwrap();
                                    (
                                        state.session_id.unwrap(),
                                        settings.secret_key.clone(),
                                    )
                                };
                                let msg = OutgoingMessage::KeepAlive{
                                    transaction: transaction_id(),
                                    session_id,
                                    apisecret,
                                };
                                res = ws_sink
                                    .send(WsMessage::text(serde_json::to_string(&msg).unwrap()))
                                    .await;
                        }
                    }

                    if let Err(ref err) = res {
                        gst::error!(CAT, imp = this, "Quitting send task: {err}");
                        break;
                    }
                }

                gst::debug!(CAT, imp = this, "Done sending");

                let _ = ws_sink.close().await;

                res.map_err(Into::into)
            }
        ));

        let recv_task_handle = RUNTIME.spawn(glib::clone!(
            #[to_owned(rename_to = this)]
            self,
            async move {
                while let Some(msg) = tokio_stream::StreamExt::next(&mut ws_stream).await {
                    if let ControlFlow::Break(_) = this.handle_msg(msg) {
                        break;
                    }
                }

                let msg = "Stopped websocket receiving";
                gst::info!(CAT, imp = this, "{msg}");
            }
        ));

        let mut state = self.state.lock().unwrap();
        state.ws_sender = Some(ws_sender);
        state.send_task_handle = Some(send_task_handle);
        state.recv_task_handle = Some(recv_task_handle);

        Ok(())
    }

    fn handle_msg(
        &self,
        msg: Result<WsMessage, async_tungstenite::tungstenite::Error>,
    ) -> ControlFlow<()> {
        match msg {
            Ok(WsMessage::Text(msg)) => {
                gst::trace!(CAT, imp = self, "Received message {}", msg);
                if let Ok(reply) = serde_json::from_str::<JsonReply>(&msg) {
                    self.handle_reply(reply);
                } else {
                    gst::error!(CAT, imp = self, "Unknown message from server: {}", msg);
                }
            }
            Ok(WsMessage::Close(reason)) => {
                gst::info!(CAT, imp = self, "websocket connection closed: {:?}", reason);
                return ControlFlow::Break(());
            }
            Ok(_) => (),
            Err(err) => {
                self.raise_error(err.to_string());
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn handle_reply(&self, reply: JsonReply) {
        match reply {
            JsonReply::WebRTCUp => {
                gst::trace!(CAT, imp = self, "WebRTC streaming is working!");

                self.obj()
                    .emit_by_name::<()>("state-updated", &[&JanusVRSignallerState::WebrtcUp]);
            }
            JsonReply::Success(success) => {
                if let Some(data) = success.data {
                    if success.session_id.is_none() {
                        gst::trace!(
                            CAT,
                            imp = self,
                            "Janus session {} was created successfully",
                            data.id
                        );
                        self.set_session_id(data.id);
                        self.attach_plugin();
                    } else {
                        gst::trace!(
                            CAT,
                            imp = self,
                            "Attached to Janus Video Room plugin successfully, handle: {}",
                            data.id
                        );
                        self.set_handle_id(data.id);
                        self.join_room();
                    }
                }
            }
            JsonReply::Event(event) => {
                if let Some(PluginData::VideoRoom { data: plugindata }) = event.plugindata {
                    match plugindata {
                        VideoRoomData::Joined(joined) => {
                            let feed_id_changed = {
                                let mut feed_id_changed = false;
                                let mut state = self.state.lock().unwrap();
                                {
                                    let mut settings = self.settings.lock().unwrap();
                                    if settings.feed_id.as_ref() != Some(&joined.id) {
                                        settings.feed_id = Some(joined.id.clone());
                                        feed_id_changed = true;
                                    }
                                }

                                state.feed_id = Some(joined.id);

                                feed_id_changed
                            };

                            if feed_id_changed {
                                self.obj().notify("feed-id");
                            }

                            gst::trace!(
                                CAT,
                                imp = self,
                                "Joined room {:?} successfully",
                                joined.room
                            );

                            self.obj().emit_by_name::<()>(
                                "state-updated",
                                &[&JanusVRSignallerState::RoomJoined],
                            );

                            self.session_requested();
                        }
                        VideoRoomData::Event(room_event) => {
                            if room_event.error_code.is_some() && room_event.error.is_some() {
                                self.raise_error(format!(
                                    "code: {}, reason: {}",
                                    room_event.error_code.unwrap(),
                                    room_event.error.unwrap(),
                                ));
                                return;
                            }

                            if let Some(Jsep::Answer { sdp, .. }) = event.jsep {
                                gst::trace!(CAT, imp = self, "Session requested successfully");
                                self.handle_answer(sdp);
                            }
                        }
                        VideoRoomData::Destroyed(room_destroyed) => {
                            gst::trace!(
                                CAT,
                                imp = self,
                                "Room {} has been destroyed",
                                room_destroyed.room
                            );

                            self.raise_error(format!(
                                "room {} has been destroyed",
                                room_destroyed.room
                            ));
                        }
                        VideoRoomData::Talking(talking) => {
                            self.emit_talking(true, talking.id, talking.audio_level);
                        }
                        VideoRoomData::StoppedTalking(talking) => {
                            self.emit_talking(false, talking.id, talking.audio_level);
                        }
                        VideoRoomData::SlowLink(_slow_link) => {
                            // TODO: use to reduce the bitrate?
                        }
                    }
                }
            }
            JsonReply::Error(error) => {
                self.raise_error(format!("code: {}, reason: {}", error.code, error.reason))
            }
            JsonReply::HangUp(hangup) => self.raise_error(format!("hangup: {}", hangup.reason)),
            // ignore for now
            JsonReply::Ack | JsonReply::Media | JsonReply::SlowLink(_) => {}
        }
    }

    fn send(&self, msg: OutgoingMessage) {
        let state = self.state.lock().unwrap();
        if let Some(mut sender) = state.ws_sender.clone() {
            RUNTIME.spawn(glib::clone!(
                #[to_owned(rename_to = this)]
                self,
                async move {
                    if let Err(err) = sender.send(msg).await {
                        this.raise_error(err.to_string());
                    }
                }
            ));
        }
    }

    fn create_session(&self) {
        let transaction = transaction_id();
        let apisecret = {
            let settings = self.settings.lock().unwrap();
            settings.secret_key.clone()
        };
        self.send(OutgoingMessage::Create {
            transaction,
            apisecret,
        });
    }

    fn set_session_id(&self, session_id: u64) {
        {
            let mut state = self.state.lock().unwrap();
            state.session_id = Some(session_id);
        }
        self.obj()
            .emit_by_name::<()>("state-updated", &[&JanusVRSignallerState::SessionCreated]);
    }

    fn set_handle_id(&self, handle_id: u64) {
        {
            let mut state = self.state.lock().unwrap();
            state.handle_id = Some(handle_id);
        }
        self.obj().emit_by_name::<()>(
            "state-updated",
            &[&JanusVRSignallerState::VideoroomAttached],
        );
    }

    fn attach_plugin(&self) {
        let (session_id, apisecret) = {
            let state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            (state.session_id.unwrap(), settings.secret_key.clone())
        };
        self.send(OutgoingMessage::Attach {
            transaction: transaction_id(),
            plugin: "janus.plugin.videoroom".to_string(),
            session_id,
            apisecret,
        });
    }

    fn join_room(&self) {
        let (session_id, handle_id, room, feed_id, display, apisecret) = {
            let mut state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            state.room_id.clone_from(&settings.room_id);

            (
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
                state.room_id.clone().unwrap(),
                settings.feed_id.clone(),
                settings.display_name.clone(),
                settings.secret_key.clone(),
            )
        };
        self.send(OutgoingMessage::Message {
            transaction: transaction_id(),
            session_id,
            handle_id,
            apisecret,
            body: MessageBody::Join(Join::Publisher {
                room,
                id: feed_id,
                display,
            }),
            jsep: None,
        });
    }

    fn leave_room(&self) {
        let mut state = self.state.lock().unwrap();
        let (session_id, handle_id, apisecret) = {
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            (
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
                settings.secret_key.clone(),
            )
        };
        if let Some(mut sender) = state.ws_sender.clone() {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            state.leave_room_rx = Some(rx);
            let msg = OutgoingMessage::Message {
                transaction: transaction_id(),
                session_id,
                handle_id,
                apisecret,
                jsep: None,
                body: MessageBody::Leave,
            };
            RUNTIME.spawn(glib::clone!(
                #[to_owned(rename_to = this)]
                self,
                async move {
                    if let Err(err) = sender.send(msg).await {
                        this.raise_error(err.to_string());
                    }
                    let _ = tx.send(());
                }
            ));
        }
    }

    fn publish(&self, offer: &gst_webrtc::WebRTCSessionDescription) {
        let (session_id, handle_id, apisecret) = {
            let state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            self.obj()
                .emit_by_name::<()>("state-updated", &[&JanusVRSignallerState::Negotiating]);

            (
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
                settings.secret_key.clone(),
            )
        };
        let sdp = offer.sdp().as_text().unwrap();
        self.send(OutgoingMessage::Message {
            transaction: transaction_id(),
            session_id,
            handle_id,
            apisecret,
            body: MessageBody::Publish,
            jsep: Some(Jsep::Offer {
                sdp,
                trickle: Some(true),
            }),
        });
    }

    fn trickle(&self, candidate: &str, sdp_m_line_index: u32) {
        let (session_id, handle_id, apisecret) = {
            let state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            (
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
                settings.secret_key.clone(),
            )
        };
        self.send(OutgoingMessage::Trickle {
            transaction: transaction_id(),
            session_id,
            handle_id,
            apisecret,
            candidate: Candidate {
                candidate: candidate.to_string(),
                sdp_m_line_index,
            },
        });
    }

    fn session_requested(&self) {
        self.obj().emit_by_name::<()>(
            "session-requested",
            &[
                &"unique",
                &"unique",
                &None::<gst_webrtc::WebRTCSessionDescription>,
            ],
        );
    }

    fn handle_answer(&self, sdp: String) {
        match gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()) {
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
        }
    }

    fn emit_talking(&self, talking: bool, id: JanusId, audio_level: f32) {
        let obj = self.obj();
        (obj.class().as_ref().emit_talking)(&obj, talking, id, audio_level)
    }
}

impl SignallableImpl for Signaller {
    fn start(&self) {
        let this = self.obj().clone();
        let imp = self.downgrade();
        RUNTIME.spawn(async move {
            if let Some(imp) = imp.upgrade() {
                if let Err(err) = imp.connect().await {
                    this.emit_by_name::<()>("error", &[&format!("{:?}", anyhow!(err))]);
                } else {
                    imp.create_session();
                }
            }
        });
    }

    fn send_sdp(&self, _session_id: &str, offer: &gst_webrtc::WebRTCSessionDescription) {
        gst::info!(
            CAT,
            imp = self,
            "sending SDP offer to peer: {:?}",
            offer.sdp().as_text()
        );

        self.publish(offer);
    }

    fn add_ice(
        &self,
        _session_id: &str,
        candidate: &str,
        sdp_m_line_index: u32,
        _sdp_mid: Option<String>,
    ) {
        self.trickle(candidate, sdp_m_line_index);
    }

    fn stop(&self) {
        gst::info!(CAT, imp = self, "Stopping now");
        let mut state = self.state.lock().unwrap();

        let send_task_handle = state.send_task_handle.take();
        let recv_task_handle = state.recv_task_handle.take();

        if let Some(mut sender) = state.ws_sender.take() {
            RUNTIME.block_on(async move {
                sender.close_channel();

                if let Some(handle) = send_task_handle {
                    if let Err(err) = handle.await {
                        gst::warning!(CAT, imp = self, "Error while joining send task: {}", err);
                    }
                }

                if let Some(handle) = recv_task_handle {
                    // if awaited instead, it hangs the plugin
                    handle.abort();
                }
            });
        }

        if let Some(rx) = state.leave_room_rx.take() {
            RUNTIME.block_on(async move {
                let _ = rx.await;
            });
        }

        state.session_id = None;
        state.handle_id = None;
    }

    fn end_session(&self, _session_id: &str) {
        self.leave_room();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "GstJanusVRWebRTCSignaller";
    type Type = super::JanusVRSignaller;
    type Class = super::JanusVRSignallerClass;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
    const ABSTRACT: bool = true;
}

#[glib::derived_properties]
impl ObjectImpl for Signaller {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                /**
                 * GstJanusVRWebRTCSignaller::state-updated:
                 * @state: the new state.
                 *
                 * This signal is emitted when the Janus state of the signaller is updated.
                 */
                glib::subclass::Signal::builder("state-updated")
                    .param_types([JanusVRSignallerState::static_type()])
                    .build(),

            ]
        });

        SIGNALS.as_ref()
    }
}

// below are Signaller subclasses implementing properties whose type depends of the Janus ID format (u64 or string).
// User can control which signaller is used by setting the `use-string-ids` construct property on `janusvrwebrtcsink`.

// each object needs to live in its own module as the code generated by the Properties macro is not namespaced
pub mod signaller_u64 {
    use super::*;

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::super::JanusVRSignallerU64)]
    pub struct SignallerU64 {
        #[property(name="room-id", get, set, type = u64, get = Self::get_room_id, set = Self::set_room_id, blurb = "The Janus Room ID that will be joined to")]
        #[property(name="feed-id", get, set, type = u64, get = Self::get_feed_id, set = Self::set_feed_id, blurb = "The Janus Feed ID to identify where the track is coming from")]
        /// Properties macro does not work with empty struct: https://github.com/gtk-rs/gtk-rs-core/issues/1110
        _unused: bool,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SignallerU64 {
        const NAME: &'static str = "GstJanusVRWebRTCSignallerU64";
        type Type = super::super::JanusVRSignallerU64;
        type ParentType = super::super::JanusVRSignaller;
    }

    #[glib::derived_properties]
    impl ObjectImpl for SignallerU64 {
        fn signals() -> &'static [Signal] {
            static SIGNALS: LazyLock<Vec<Signal>> = LazyLock::new(|| {
                vec![
                    /**
                     * GstJanusVRWebRTCSignallerU64::talking:
                     * @self: A #GstJanusVRWebRTCSignallerStr
                     * @talking: if the publisher is talking, or not
                     * @id: unique numeric ID of the publisher
                     * @audio-level: average value of audio level, 127=muted, 0='too loud'
                     */
                    Signal::builder("talking")
                        .param_types([bool::static_type(), u64::static_type(), f32::static_type()])
                        .build(),
                ]
            });

            SIGNALS.as_ref()
        }
    }

    impl super::super::JanusVRSignallerImpl for SignallerU64 {
        fn emit_talking(&self, talking: bool, id: super::JanusId, audio_level: f32) {
            self.obj()
                .emit_by_name::<()>("talking", &[&talking, &id.as_num(), &audio_level]);
        }
    }

    impl SignallerU64 {
        fn get_room_id(&self) -> u64 {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let settings = signaller.settings.lock().unwrap();

            settings
                .room_id
                .as_ref()
                .map(|id| id.as_num())
                .unwrap_or_default()
        }

        fn set_room_id(&self, id: u64) {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let mut settings = signaller.settings.lock().unwrap();

            settings.room_id = Some(JanusId::Num(id));
        }

        fn get_feed_id(&self) -> u64 {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let settings = signaller.settings.lock().unwrap();

            settings
                .feed_id
                .as_ref()
                .map(|id| id.as_num())
                .unwrap_or_default()
        }

        fn set_feed_id(&self, id: u64) {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let mut settings = signaller.settings.lock().unwrap();

            settings.feed_id = Some(JanusId::Num(id));
        }
    }
}

pub mod signaller_str {
    use super::*;

    #[derive(Default, Properties)]
    #[properties(wrapper_type = super::super::JanusVRSignallerStr)]
    pub struct SignallerStr {
        #[property(name="room-id", get, set, type = String, get = Self::get_room_id, set = Self::set_room_id, blurb = "The Janus Room ID that will be joined to")]
        #[property(name="feed-id", get, set, type = String, get = Self::get_feed_id, set = Self::set_feed_id, blurb = "The Janus Feed ID to identify where the track is coming from")]
        /// Properties macro does not work with empty struct: https://github.com/gtk-rs/gtk-rs-core/issues/1110
        _unused: bool,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SignallerStr {
        const NAME: &'static str = "GstJanusVRWebRTCSignallerStr";
        type Type = super::super::JanusVRSignallerStr;
        type ParentType = super::super::JanusVRSignaller;
    }

    #[glib::derived_properties]
    impl ObjectImpl for SignallerStr {
        fn signals() -> &'static [Signal] {
            static SIGNALS: LazyLock<Vec<Signal>> = LazyLock::new(|| {
                vec![
                    /**
                     * GstJanusVRWebRTCSignallerStr::talking:
                     * @self: A #GstJanusVRWebRTCSignallerStr
                     * @talking: if the publisher is talking, or not
                     * @id: unique string ID of the publisher
                     * @audio-level: average value of audio level, 127=muted, 0='too loud'
                     */
                    Signal::builder("talking")
                        .param_types([bool::static_type(), str::static_type(), f32::static_type()])
                        .build(),
                ]
            });

            SIGNALS.as_ref()
        }
    }

    impl super::super::JanusVRSignallerImpl for SignallerStr {
        fn emit_talking(&self, talking: bool, id: super::JanusId, audio_level: f32) {
            self.obj()
                .emit_by_name::<()>("talking", &[&talking, &id.as_string(), &audio_level]);
        }
    }

    impl SignallerStr {
        fn get_room_id(&self) -> String {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let settings = signaller.settings.lock().unwrap();

            settings
                .room_id
                .as_ref()
                .map(|id| id.as_string())
                .unwrap_or_default()
        }

        fn set_room_id(&self, id: String) {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let mut settings = signaller.settings.lock().unwrap();

            settings.room_id = Some(JanusId::Str(id));
        }

        fn get_feed_id(&self) -> String {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let settings = signaller.settings.lock().unwrap();

            settings
                .feed_id
                .as_ref()
                .map(|id| id.as_string())
                .unwrap_or_default()
        }

        fn set_feed_id(&self, id: String) {
            let obj = self.obj();
            let signaller = obj.upcast_ref::<super::super::JanusVRSignaller>().imp();
            let mut settings = signaller.settings.lock().unwrap();

            settings.feed_id = Some(JanusId::Str(id));
        }
    }
}
