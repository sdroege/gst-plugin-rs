// SPDX-License-Identifier: MPL-2.0

use crate::signaller::{Signallable, SignallableImpl};
use crate::RUNTIME;

use anyhow::{anyhow, Error};
use async_tungstenite::tungstenite;
use futures::channel::mpsc;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use gst::glib;
use gst::glib::once_cell::sync::Lazy;
use gst::glib::Properties;
use gst::prelude::*;
use gst::subclass::prelude::*;
use http::Uri;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::ops::ControlFlow;
use std::sync::Mutex;
use std::time::Duration;
use tokio::{task, time::timeout};
use tungstenite::Message as WsMessage;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtc-janusvr-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC Janus Video Room signaller"),
    )
});

fn transaction_id() -> String {
    thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .map(char::from)
        .take(30)
        .collect()
}

fn feed_id() -> u32 {
    thread_rng().gen()
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct KeepAliveMsg {
    janus: String,
    transaction: String,
    session_id: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct CreateSessionMsg {
    janus: String,
    transaction: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct AttachPluginMsg {
    janus: String,
    transaction: String,
    plugin: String,
    session_id: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct RoomRequestBody {
    request: String,
    ptype: String,
    room: u64,
    id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct RoomRequestMsg {
    janus: String,
    transaction: String,
    session_id: u64,
    handle_id: u64,
    body: RoomRequestBody,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct PublishBody {
    request: String,
    audio: bool,
    video: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Jsep {
    sdp: String,
    trickle: Option<bool>,
    r#type: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct PublishMsg {
    janus: String,
    transaction: String,
    session_id: u64,
    handle_id: u64,
    body: PublishBody,
    jsep: Jsep,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Candidate {
    candidate: String,
    #[serde(rename = "sdpMLineIndex")]
    sdp_m_line_index: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct TrickleMsg {
    janus: String,
    transaction: String,
    session_id: u64,
    handle_id: u64,
    candidate: Candidate,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum OutgoingMessage {
    KeepAlive(KeepAliveMsg),
    CreateSession(CreateSessionMsg),
    AttachPlugin(AttachPluginMsg),
    RoomRequest(RoomRequestMsg),
    Publish(PublishMsg),
    Trickle(TrickleMsg),
}

#[derive(Serialize, Deserialize, Debug)]
struct InnerError {
    code: i32,
    reason: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct RoomJoined {
    room: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug)]
struct RoomEvent {
    room: Option<u64>,
    error_code: Option<i32>,
    error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "videoroom")]
enum VideoRoomData {
    #[serde(rename = "joined")]
    Joined(RoomJoined),
    #[serde(rename = "event")]
    Event(RoomEvent),
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
#[serde(tag = "janus")]
enum JsonReply {
    #[serde(rename = "ack")]
    Ack,
    #[serde(rename = "success")]
    Success(SuccessMsg),
    #[serde(rename = "event")]
    Event(EventMsg),
    #[serde(rename = "webrtcup")]
    WebRTCUp,
    #[serde(rename = "media")]
    Media,
    #[serde(rename = "error")]
    Error(InnerError),
}

#[derive(Default)]
struct State {
    ws_sender: Option<mpsc::Sender<OutgoingMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    recv_task_handle: Option<task::JoinHandle<()>>,
    session_id: Option<u64>,
    handle_id: Option<u64>,
    transaction_id: Option<String>,
}

#[derive(Clone)]
struct Settings {
    janus_endpoint: String,
    room_id: Option<String>,
    feed_id: u32,
    display_name: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            janus_endpoint: "ws://127.0.0.1:8188".to_string(),
            room_id: None,
            feed_id: feed_id(),
            display_name: None,
        }
    }
}

#[derive(Default, Properties)]
#[properties(wrapper_type = super::JanusVRSignaller)]
pub struct Signaller {
    state: Mutex<State>,
    #[property(name="janus-endpoint", get, set, type = String, member = janus_endpoint, blurb = "The Janus server endpoint to POST SDP offer to")]
    #[property(name="room-id", get, set, type = String, member = room_id, blurb = "The Janus Room ID that will be joined to")]
    #[property(name="feed-id", get, set, type = u32, member = feed_id, blurb = "The Janus Feed ID to identify where the track is coming from")]
    #[property(name="display-name", get, set, type = String, member = display_name, blurb = "The name of the publisher in the Janus Video Room")]
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
        let send_task_handle =
            RUNTIME.spawn(glib::clone!(@weak-allow-none self as this => async move {
                loop {
                    tokio::select! {
                        opt = ws_receiver.next() => match opt {
                            Some(msg) => {
                                gst::log!(CAT, "Sending websocket message {:?}", msg);
                                ws_sink
                                    .send(WsMessage::Text(serde_json::to_string(&msg).unwrap()))
                                    .await?;
                            },
                            None => break,
                        },
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {
                            if let Some(ref this) = this {
                                let (transaction, session_id) = {
                                    let state = this.state.lock().unwrap();
                                    (state.transaction_id.clone().unwrap(),
                                    state.session_id.unwrap())
                                };
                                let msg = OutgoingMessage::KeepAlive(KeepAliveMsg {
                                    janus: "keepalive".to_string(),
                                    transaction,
                                    session_id,
                                });
                                ws_sink
                                    .send(WsMessage::Text(serde_json::to_string(&msg).unwrap()))
                                    .await?;
                            }
                        }
                    }
                }

                let msg = "Done sending";
                this.map_or_else(|| gst::info!(CAT, "{msg}"),
                    |this| gst::info!(CAT, imp: this, "{msg}")
                );

                ws_sink.send(WsMessage::Close(None)).await?;
                ws_sink.close().await?;

                Ok::<(), Error>(())
            }));

        let recv_task_handle =
            RUNTIME.spawn(glib::clone!(@weak-allow-none self as this => async move {
                while let Some(msg) = tokio_stream::StreamExt::next(&mut ws_stream).await {
                    if let Some(ref this) = this {
                        if let ControlFlow::Break(_) = this.handle_msg(msg) {
                            break;
                        }
                    } else {
                        break;
                    }
                }

                let msg = "Stopped websocket receiving";
                this.map_or_else(|| gst::info!(CAT, "{msg}"),
                    |this| gst::info!(CAT, imp: this, "{msg}")
                );
            }));

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
                gst::trace!(CAT, imp: self, "Received message {}", msg);
                if let Ok(reply) = serde_json::from_str::<JsonReply>(&msg) {
                    self.handle_reply(reply);
                } else {
                    gst::error!(CAT, imp: self, "Unknown message from server: {}", msg);
                }
            }
            Ok(WsMessage::Close(reason)) => {
                gst::info!(CAT, imp: self, "websocket connection closed: {:?}", reason);
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
                gst::trace!(CAT, imp: self, "WebRTC streaming is working!");
            }
            JsonReply::Success(success) => {
                if let Some(data) = success.data {
                    if success.session_id.is_none() {
                        gst::trace!(CAT, imp: self, "Janus session {} was created successfully", data.id);
                        self.set_session_id(data.id);
                        self.attach_plugin();
                    } else {
                        gst::trace!(CAT, imp: self, "Attached to Janus Video Room plugin successfully, handle: {}", data.id);
                        self.set_handle_id(data.id);
                        self.join_room();
                    }
                }
            }
            JsonReply::Event(event) => {
                if let Some(PluginData::VideoRoom { data: plugindata }) = event.plugindata {
                    match plugindata {
                        VideoRoomData::Joined(joined) => {
                            if let Some(room) = joined.room {
                                gst::trace!(CAT, imp: self, "Joined room {} successfully", room);
                                self.session_requested();
                            }
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

                            if let Some(jsep) = event.jsep {
                                if jsep.r#type == "answer" {
                                    gst::trace!(CAT, imp: self, "Session requested successfully");
                                    self.handle_answer(jsep.sdp);
                                }
                            }
                        }
                    }
                }
            }
            JsonReply::Error(error) => {
                self.raise_error(format!("code: {}, reason: {}", error.code, error.reason))
            }
            // ignore for now
            JsonReply::Ack | JsonReply::Media => {}
        }
    }

    fn send(&self, msg: OutgoingMessage) {
        let state = self.state.lock().unwrap();
        if let Some(mut sender) = state.ws_sender.clone() {
            RUNTIME.spawn(glib::clone!(@weak self as this => async move {
                if let Err(err) = sender.send(msg).await {
                    this.raise_error(err.to_string());
                }
            }));
        }
    }

    // Only used at the end when cleaning up the resources.
    // So that `SignallableImpl::stop` waits the last message
    // to be sent properly.
    fn send_blocking(&self, msg: OutgoingMessage) {
        let state = self.state.lock().unwrap();
        if let Some(mut sender) = state.ws_sender.clone() {
            RUNTIME.block_on(glib::clone!(@weak self as this => async move {
                if let Err(err) = sender.send(msg).await {
                    this.raise_error(err.to_string());
                }
            }));
        }
    }

    fn set_transaction_id(&self, transaction: String) {
        self.state.lock().unwrap().transaction_id = Some(transaction);
    }

    fn create_session(&self) {
        let transaction = transaction_id();
        self.set_transaction_id(transaction.clone());
        self.send(OutgoingMessage::CreateSession(CreateSessionMsg {
            janus: "create".to_string(),
            transaction,
        }));
    }

    fn set_session_id(&self, session_id: u64) {
        self.state.lock().unwrap().session_id = Some(session_id);
    }

    fn set_handle_id(&self, handle_id: u64) {
        self.state.lock().unwrap().handle_id = Some(handle_id);
    }

    fn attach_plugin(&self) {
        let (transaction, session_id) = {
            let state = self.state.lock().unwrap();

            (
                state.transaction_id.clone().unwrap(),
                state.session_id.unwrap(),
            )
        };
        self.send(OutgoingMessage::AttachPlugin(AttachPluginMsg {
            janus: "attach".to_string(),
            transaction,
            plugin: "janus.plugin.videoroom".to_string(),
            session_id,
        }));
    }

    fn join_room(&self) {
        let (transaction, session_id, handle_id, room, feed_id, display) = {
            let state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            (
                state.transaction_id.clone().unwrap(),
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
                settings.room_id.as_ref().unwrap().parse().unwrap(),
                settings.feed_id,
                settings.display_name.clone(),
            )
        };
        self.send(OutgoingMessage::RoomRequest(RoomRequestMsg {
            janus: "message".to_string(),
            transaction,
            session_id,
            handle_id,
            body: RoomRequestBody {
                request: "join".to_string(),
                ptype: "publisher".to_string(),
                room,
                id: feed_id,
                display,
            },
        }));
    }

    fn leave_room(&self) {
        let (transaction, session_id, handle_id, room, feed_id, display) = {
            let state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            (
                state.transaction_id.clone().unwrap(),
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
                settings.room_id.as_ref().unwrap().parse().unwrap(),
                settings.feed_id,
                settings.display_name.clone(),
            )
        };
        self.send_blocking(OutgoingMessage::RoomRequest(RoomRequestMsg {
            janus: "message".to_string(),
            transaction,
            session_id,
            handle_id,
            body: RoomRequestBody {
                request: "leave".to_string(),
                ptype: "publisher".to_string(),
                room,
                id: feed_id,
                display,
            },
        }));
    }

    fn publish(&self, offer: &gst_webrtc::WebRTCSessionDescription) {
        let (transaction, session_id, handle_id) = {
            let state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            (
                state.transaction_id.clone().unwrap(),
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
            )
        };
        let sdp_data = offer.sdp().as_text().unwrap();
        self.send(OutgoingMessage::Publish(PublishMsg {
            janus: "message".to_string(),
            transaction,
            session_id,
            handle_id,
            body: PublishBody {
                request: "publish".to_string(),
                audio: true,
                video: true,
            },
            jsep: Jsep {
                sdp: sdp_data,
                trickle: Some(true),
                r#type: "offer".to_string(),
            },
        }));
    }

    fn trickle(&self, candidate: &str, sdp_m_line_index: u32) {
        let (transaction, session_id, handle_id) = {
            let state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();

            if settings.room_id.is_none() {
                self.raise_error("Janus Room ID must be set".to_string());
                return;
            }

            (
                state.transaction_id.clone().unwrap(),
                state.session_id.unwrap(),
                state.handle_id.unwrap(),
            )
        };
        self.send(OutgoingMessage::Trickle(TrickleMsg {
            janus: "trickle".to_string(),
            transaction,
            session_id,
            handle_id,
            candidate: Candidate {
                candidate: candidate.to_string(),
                sdp_m_line_index,
            },
        }));
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
        gst::info!(CAT, imp: self, "sending SDP offer to peer: {:?}", offer.sdp().as_text());

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
        gst::info!(CAT, imp: self, "Stopping now");
        let mut state = self.state.lock().unwrap();

        let send_task_handle = state.send_task_handle.take();
        let recv_task_handle = state.recv_task_handle.take();

        if let Some(mut sender) = state.ws_sender.take() {
            RUNTIME.block_on(async move {
                sender.close_channel();

                if let Some(handle) = send_task_handle {
                    if let Err(err) = handle.await {
                        gst::warning!(CAT, imp: self, "Error while joining send task: {}", err);
                    }
                }

                if let Some(handle) = recv_task_handle {
                    // if awaited instead, it hangs the plugin
                    handle.abort();
                }
            });
        }

        state.session_id = None;
        state.handle_id = None;
        state.transaction_id = None;
    }

    fn end_session(&self, _session_id: &str) {
        self.leave_room();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "GstJanusVRWebRTCSignaller";
    type Type = super::JanusVRSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

#[glib::derived_properties]
impl ObjectImpl for Signaller {}
