use crate::webrtcsink::WebRTCSink;
use anyhow::{anyhow, Error};
use async_std::task;
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::channel::mpsc;
use futures::prelude::*;
use gst::glib;
use gst::glib::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_trace, gst_warning};
use once_cell::sync::Lazy;
use serde_derive::{Deserialize, Serialize};
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsink-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC sink signaller"),
    )
});

#[derive(Default)]
struct State {
    /// Sender for the websocket messages
    websocket_sender: Option<mpsc::Sender<WsMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    receive_task_handle: Option<task::JoinHandle<()>>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
enum SdpMessage {
    Offer { sdp: String },
    Answer { sdp: String },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsgInner {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    Sdp(SdpMessage),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
struct JsonMsg {
    #[serde(rename = "peer-id")]
    peer_id: String,
    #[serde(flatten)]
    inner: JsonMsgInner,
}

struct Settings {
    address: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            address: Some("ws://127.0.0.1:8443".to_string()),
        }
    }
}

#[derive(Default)]
pub struct Signaller {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Signaller {
    async fn connect(&self, element: &WebRTCSink) -> Result<(), Error> {
        let address = self
            .settings
            .lock()
            .unwrap()
            .address
            .as_ref()
            .unwrap()
            .clone();

        let (ws, _) = async_tungstenite::async_std::connect_async(address).await?;

        gst_info!(CAT, obj: element, "connected");

        // Channel for asynchronously sending out websocket message
        let (mut ws_sink, mut ws_stream) = ws.split();

        ws_sink
            .send(WsMessage::Text("REGISTER PRODUCER".to_string()))
            .await?;

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (websocket_sender, mut websocket_receiver) = mpsc::channel::<WsMessage>(1000);
        let element_clone = element.downgrade();
        let send_task_handle = task::spawn(async move {
            while let Some(msg) = websocket_receiver.next().await {
                if let Some(element) = element_clone.upgrade() {
                    gst_trace!(CAT, obj: &element, "Sending websocket message {:?}", msg);
                }
                ws_sink.send(msg).await?;
            }

            if let Some(element) = element_clone.upgrade() {
                gst_info!(CAT, obj: &element, "Done sending");
            }

            ws_sink.send(WsMessage::Close(None)).await?;
            ws_sink.close().await?;

            Ok::<(), Error>(())
        });

        let element_clone = element.downgrade();
        let receive_task_handle = task::spawn(async move {
            while let Some(msg) = async_std::stream::StreamExt::next(&mut ws_stream).await {
                if let Some(element) = element_clone.upgrade() {
                    match msg {
                        Ok(WsMessage::Text(msg)) => {
                            gst_trace!(CAT, obj: &element, "Received message {}", msg);

                            if msg.starts_with("REGISTERED ") {
                                gst_info!(CAT, obj: &element, "We are registered with the server");
                            } else if let Some(peer_id) = msg.strip_prefix("START_SESSION ") {
                                if let Err(err) = element.add_consumer(peer_id) {
                                    gst_warning!(CAT, obj: &element, "{}", err);
                                }
                            } else if let Some(peer_id) = msg.strip_prefix("END_SESSION ") {
                                if let Err(err) = element.remove_consumer(peer_id) {
                                    gst_warning!(CAT, obj: &element, "{}", err);
                                }
                            } else if let Ok(msg) = serde_json::from_str::<JsonMsg>(&msg) {
                                match msg.inner {
                                    JsonMsgInner::Sdp(SdpMessage::Answer { sdp }) => {
                                        if let Err(err) = element.handle_sdp(
                                            &msg.peer_id,
                                            &gst_webrtc::WebRTCSessionDescription::new(
                                                gst_webrtc::WebRTCSDPType::Answer,
                                                gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
                                                    .unwrap(),
                                            ),
                                        ) {
                                            gst_warning!(CAT, obj: &element, "{}", err);
                                        }
                                    }
                                    JsonMsgInner::Sdp(SdpMessage::Offer { .. }) => {
                                        gst_warning!(
                                            CAT,
                                            obj: &element,
                                            "Ignoring offer from peer"
                                        );
                                    }
                                    JsonMsgInner::Ice {
                                        candidate,
                                        sdp_mline_index,
                                    } => {
                                        if let Err(err) = element.handle_ice(
                                            &msg.peer_id,
                                            Some(sdp_mline_index),
                                            None,
                                            &candidate,
                                        ) {
                                            gst_warning!(CAT, obj: &element, "{}", err);
                                        }
                                    }
                                }
                            } else {
                                gst_error!(
                                    CAT,
                                    obj: &element,
                                    "Unknown message from server: {}",
                                    msg
                                );
                                element.handle_signalling_error(anyhow!(
                                    "Unknown message from server: {}",
                                    msg
                                ));
                            }
                        }
                        Ok(WsMessage::Close(reason)) => {
                            gst_info!(
                                CAT,
                                obj: &element,
                                "websocket connection closed: {:?}",
                                reason
                            );
                            break;
                        }
                        Ok(_) => (),
                        Err(err) => {
                            element.handle_signalling_error(anyhow!("Error receiving: {}", err));
                            break;
                        }
                    }
                } else {
                    break;
                }
            }

            if let Some(element) = element_clone.upgrade() {
                gst_info!(CAT, obj: &element, "Stopped websocket receiving");
            }
        });

        let mut state = self.state.lock().unwrap();
        state.websocket_sender = Some(websocket_sender);
        state.send_task_handle = Some(send_task_handle);
        state.receive_task_handle = Some(receive_task_handle);

        Ok(())
    }

    pub fn start(&self, element: &WebRTCSink) {
        let this = self.instance().clone();
        let element_clone = element.clone();
        task::spawn(async move {
            let this = Self::from_instance(&this);
            if let Err(err) = this.connect(&element_clone).await {
                element_clone.handle_signalling_error(err);
            }
        });
    }

    pub fn handle_sdp(
        &self,
        element: &WebRTCSink,
        peer_id: &str,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) {
        let state = self.state.lock().unwrap();

        let msg = JsonMsg {
            peer_id: peer_id.to_string(),
            inner: JsonMsgInner::Sdp(SdpMessage::Offer {
                sdp: sdp.sdp().as_text().unwrap(),
            }),
        };

        if let Some(mut sender) = state.websocket_sender.clone() {
            let element = element.downgrade();
            task::spawn(async move {
                if let Err(err) = sender
                    .send(WsMessage::Text(serde_json::to_string(&msg).unwrap()))
                    .await
                {
                    if let Some(element) = element.upgrade() {
                        element.handle_signalling_error(anyhow!("Error: {}", err));
                    }
                }
            });
        }
    }

    pub fn handle_ice(
        &self,
        element: &WebRTCSink,
        peer_id: &str,
        candidate: &str,
        sdp_mline_index: Option<u32>,
        _sdp_mid: Option<String>,
    ) {
        let state = self.state.lock().unwrap();

        let msg = JsonMsg {
            peer_id: peer_id.to_string(),
            inner: JsonMsgInner::Ice {
                candidate: candidate.to_string(),
                sdp_mline_index: sdp_mline_index.unwrap(),
            },
        };

        if let Some(mut sender) = state.websocket_sender.clone() {
            let element = element.downgrade();
            task::spawn(async move {
                if let Err(err) = sender
                    .send(WsMessage::Text(serde_json::to_string(&msg).unwrap()))
                    .await
                {
                    if let Some(element) = element.upgrade() {
                        element.handle_signalling_error(anyhow!("Error: {}", err));
                    }
                }
            });
        }
    }

    pub fn stop(&self, element: &WebRTCSink) {
        gst_info!(CAT, obj: element, "Stopping now");

        let mut state = self.state.lock().unwrap();
        let send_task_handle = state.send_task_handle.take();
        let receive_task_handle = state.receive_task_handle.take();
        if let Some(mut sender) = state.websocket_sender.take() {
            task::block_on(async move {
                sender.close_channel();

                if let Some(handle) = send_task_handle {
                    if let Err(err) = handle.await {
                        gst_warning!(CAT, obj: element, "Error while joining send task: {}", err);
                    }
                }

                if let Some(handle) = receive_task_handle {
                    handle.await;
                }
            });
        }
    }

    pub fn consumer_removed(&self, element: &WebRTCSink, peer_id: &str) {
        gst_debug!(CAT, obj: element, "Signalling consumer {} removed", peer_id);

        let state = self.state.lock().unwrap();
        let peer_id = peer_id.to_string();
        let element = element.downgrade();
        if let Some(mut sender) = state.websocket_sender.clone() {
            task::spawn(async move {
                if let Err(err) = sender
                    .send(WsMessage::Text(format!("END_SESSION {}", peer_id)))
                    .await
                {
                    if let Some(element) = element.upgrade() {
                        element.handle_signalling_error(anyhow!("Error: {}", err));
                    }
                }
            });
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "RsWebRTCSinkSignaller";
    type Type = super::Signaller;
    type ParentType = glib::Object;
}

impl ObjectImpl for Signaller {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecString::new(
                "address",
                "Address",
                "Address of the signalling server",
                Some("ws://127.0.0.1:8443"),
                glib::ParamFlags::READWRITE,
            )]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "address" => {
                let address: Option<_> = value.get().expect("type checked upstream");

                if let Some(address) = address {
                    gst_info!(CAT, "Signaller address set to {}", address);

                    let mut settings = self.settings.lock().unwrap();
                    settings.address = Some(address);
                } else {
                    gst_error!(CAT, "address can't be None");
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "address" => {
                let settings = self.settings.lock().unwrap();
                settings.address.to_value()
            }
            _ => unimplemented!(),
        }
    }
}
