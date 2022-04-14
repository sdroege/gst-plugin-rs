use crate::webrtcsink::WebRTCSink;
use anyhow::{anyhow, Error};
use async_std::task;
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::channel::mpsc;
use futures::prelude::*;
use gst::glib;
use gst::glib::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::Mutex;
use webrtcsink_protocol as p;

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
    websocket_sender: Option<mpsc::Sender<p::IncomingMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    receive_task_handle: Option<task::JoinHandle<()>>,
}

#[derive(Clone)]
struct Settings {
    address: Option<String>,
    cafile: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            address: Some("ws://127.0.0.1:8443".to_string()),
            cafile: None,
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
        let settings = self.settings.lock().unwrap().clone();

        let connector = if let Some(path) = settings.cafile {
            let cert = async_std::fs::read_to_string(&path).await?;
            let cert = async_native_tls::Certificate::from_pem(cert.as_bytes())?;
            let connector = async_native_tls::TlsConnector::new();
            Some(connector.add_root_certificate(cert))
        } else {
            None
        };

        let (ws, _) = async_tungstenite::async_std::connect_async_with_tls_connector(
            settings.address.unwrap(),
            connector,
        )
        .await?;

        gst::info!(CAT, obj: element, "connected");

        // Channel for asynchronously sending out websocket message
        let (mut ws_sink, mut ws_stream) = ws.split();

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (mut websocket_sender, mut websocket_receiver) =
            mpsc::channel::<p::IncomingMessage>(1000);
        let element_clone = element.downgrade();
        let send_task_handle = task::spawn(async move {
            while let Some(msg) = websocket_receiver.next().await {
                if let Some(element) = element_clone.upgrade() {
                    gst::trace!(CAT, obj: &element, "Sending websocket message {:?}", msg);
                }
                ws_sink
                    .send(WsMessage::Text(serde_json::to_string(&msg).unwrap()))
                    .await?;
            }

            if let Some(element) = element_clone.upgrade() {
                gst::info!(CAT, obj: &element, "Done sending");
            }

            ws_sink.send(WsMessage::Close(None)).await?;
            ws_sink.close().await?;

            Ok::<(), Error>(())
        });

        websocket_sender
            .send(p::IncomingMessage::Register(p::RegisterMessage::Producer {
                display_name: element.property("display-name"),
            }))
            .await?;

        let element_clone = element.downgrade();
        let receive_task_handle = task::spawn(async move {
            while let Some(msg) = async_std::stream::StreamExt::next(&mut ws_stream).await {
                if let Some(element) = element_clone.upgrade() {
                    match msg {
                        Ok(WsMessage::Text(msg)) => {
                            gst::trace!(CAT, obj: &element, "Received message {}", msg);

                            if let Ok(msg) = serde_json::from_str::<p::OutgoingMessage>(&msg) {
                                match msg {
                                    p::OutgoingMessage::Registered(
                                        p::RegisteredMessage::Producer { peer_id, .. },
                                    ) => {
                                        gst::info!(
                                            CAT,
                                            obj: &element,
                                            "We are registered with the server, our peer id is {}",
                                            peer_id
                                        );
                                    }
                                    p::OutgoingMessage::Registered(_) => unreachable!(),
                                    p::OutgoingMessage::StartSession { peer_id } => {
                                        if let Err(err) = element.add_consumer(&peer_id) {
                                            gst::warning!(CAT, obj: &element, "{}", err);
                                        }
                                    }
                                    p::OutgoingMessage::EndSession { peer_id } => {
                                        if let Err(err) = element.remove_consumer(&peer_id) {
                                            gst::warning!(CAT, obj: &element, "{}", err);
                                        }
                                    }
                                    p::OutgoingMessage::Peer(p::PeerMessage {
                                        peer_id,
                                        peer_message,
                                    }) => match peer_message {
                                        p::PeerMessageInner::Sdp(p::SdpMessage::Answer { sdp }) => {
                                            if let Err(err) = element.handle_sdp(
                                                &peer_id,
                                                &gst_webrtc::WebRTCSessionDescription::new(
                                                    gst_webrtc::WebRTCSDPType::Answer,
                                                    gst_sdp::SDPMessage::parse_buffer(
                                                        sdp.as_bytes(),
                                                    )
                                                    .unwrap(),
                                                ),
                                            ) {
                                                gst::warning!(CAT, obj: &element, "{}", err);
                                            }
                                        }
                                        p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                                            ..
                                        }) => {
                                            gst::warning!(
                                                CAT,
                                                obj: &element,
                                                "Ignoring offer from peer"
                                            );
                                        }
                                        p::PeerMessageInner::Ice {
                                            candidate,
                                            sdp_m_line_index,
                                        } => {
                                            if let Err(err) = element.handle_ice(
                                                &peer_id,
                                                Some(sdp_m_line_index),
                                                None,
                                                &candidate,
                                            ) {
                                                gst::warning!(CAT, obj: &element, "{}", err);
                                            }
                                        }
                                    },
                                    _ => {
                                        gst::warning!(
                                            CAT,
                                            obj: &element,
                                            "Ignoring unsupported message {:?}",
                                            msg
                                        );
                                    }
                                }
                            } else {
                                gst::error!(
                                    CAT,
                                    obj: &element,
                                    "Unknown message from server: {}",
                                    msg
                                );
                                element.handle_signalling_error(
                                    anyhow!("Unknown message from server: {}", msg).into(),
                                );
                            }
                        }
                        Ok(WsMessage::Close(reason)) => {
                            gst::info!(
                                CAT,
                                obj: &element,
                                "websocket connection closed: {:?}",
                                reason
                            );
                            break;
                        }
                        Ok(_) => (),
                        Err(err) => {
                            element.handle_signalling_error(
                                anyhow!("Error receiving: {}", err).into(),
                            );
                            break;
                        }
                    }
                } else {
                    break;
                }
            }

            if let Some(element) = element_clone.upgrade() {
                gst::info!(CAT, obj: &element, "Stopped websocket receiving");
            }
        });

        let mut state = self.state.lock().unwrap();
        state.websocket_sender = Some(websocket_sender);
        state.send_task_handle = Some(send_task_handle);
        state.receive_task_handle = Some(receive_task_handle);

        Ok(())
    }

    pub fn start(&self, element: &WebRTCSink) {
        let this = self.instance();
        let element_clone = element.clone();
        task::spawn(async move {
            let this = Self::from_instance(&this);
            if let Err(err) = this.connect(&element_clone).await {
                element_clone.handle_signalling_error(err.into());
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

        let msg = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: peer_id.to_string(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: sdp.sdp().as_text().unwrap(),
            }),
        });

        if let Some(mut sender) = state.websocket_sender.clone() {
            let element = element.downgrade();
            task::spawn(async move {
                if let Err(err) = sender.send(msg).await {
                    if let Some(element) = element.upgrade() {
                        element.handle_signalling_error(anyhow!("Error: {}", err).into());
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
        sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
    ) {
        let state = self.state.lock().unwrap();

        let msg = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: peer_id.to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: candidate.to_string(),
                sdp_m_line_index: sdp_m_line_index.unwrap(),
            },
        });

        if let Some(mut sender) = state.websocket_sender.clone() {
            let element = element.downgrade();
            task::spawn(async move {
                if let Err(err) = sender.send(msg).await {
                    if let Some(element) = element.upgrade() {
                        element.handle_signalling_error(anyhow!("Error: {}", err).into());
                    }
                }
            });
        }
    }

    pub fn stop(&self, element: &WebRTCSink) {
        gst::info!(CAT, obj: element, "Stopping now");

        let mut state = self.state.lock().unwrap();
        let send_task_handle = state.send_task_handle.take();
        let receive_task_handle = state.receive_task_handle.take();
        if let Some(mut sender) = state.websocket_sender.take() {
            task::block_on(async move {
                sender.close_channel();

                if let Some(handle) = send_task_handle {
                    if let Err(err) = handle.await {
                        gst::warning!(CAT, obj: element, "Error while joining send task: {}", err);
                    }
                }

                if let Some(handle) = receive_task_handle {
                    handle.await;
                }
            });
        }
    }

    pub fn consumer_removed(&self, element: &WebRTCSink, peer_id: &str) {
        gst::debug!(CAT, obj: element, "Signalling consumer {} removed", peer_id);

        let state = self.state.lock().unwrap();
        let peer_id = peer_id.to_string();
        let element = element.downgrade();
        if let Some(mut sender) = state.websocket_sender.clone() {
            task::spawn(async move {
                if let Err(err) = sender
                    .send(p::IncomingMessage::EndSession(p::EndSessionMessage {
                        peer_id: peer_id.to_string(),
                    }))
                    .await
                {
                    if let Some(element) = element.upgrade() {
                        element.handle_signalling_error(anyhow!("Error: {}", err).into());
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
            vec![
                glib::ParamSpecString::new(
                    "address",
                    "Address",
                    "Address of the signalling server",
                    Some("ws://127.0.0.1:8443"),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecString::new(
                    "cafile",
                    "CA file",
                    "Path to a Certificate file to add to the set of roots the TLS connector will trust",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
            ]
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
                    gst::info!(CAT, "Signaller address set to {}", address);

                    let mut settings = self.settings.lock().unwrap();
                    settings.address = Some(address);
                } else {
                    gst::error!(CAT, "address can't be None");
                }
            }
            "cafile" => {
                let value: String = value.get().unwrap();
                let mut settings = self.settings.lock().unwrap();
                settings.cafile = Some(value.into());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "address" => self.settings.lock().unwrap().address.to_value(),
            "cafile" => {
                let settings = self.settings.lock().unwrap();
                let cafile = settings.cafile.as_ref();
                cafile.and_then(|file| file.to_str()).to_value()
            }
            _ => unimplemented!(),
        }
    }
}
