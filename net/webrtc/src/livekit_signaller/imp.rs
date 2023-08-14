// SPDX-License-Identifier: MPL-2.0

use crate::signaller::{Signallable, SignallableImpl};

use crate::utils::{wait_async, WaitError};
use crate::RUNTIME;

use anyhow::anyhow;
use futures::executor::block_on;
use gst::glib;
use gst::glib::once_cell::sync::Lazy;
use gst::prelude::*;
use gst::subclass::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use livekit_api::access_token::{AccessToken, VideoGrants};
use livekit_api::signal_client;
use livekit_protocol as proto;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtc-livekit-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC LiveKit signaller"),
    )
});

const DEFAULT_TRACK_PUBLISH_TIMEOUT: u32 = 10;

#[derive(Clone)]
struct Settings {
    wsurl: Option<String>,
    api_key: Option<String>,
    secret_key: Option<String>,
    participant_name: Option<String>,
    identity: Option<String>,
    room_name: Option<String>,
    auth_token: Option<String>,
    timeout: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wsurl: Some("ws://127.0.0.1:7880".to_string()),
            api_key: None,
            secret_key: None,
            participant_name: Some("GStreamer".to_string()),
            identity: Some("gstreamer".to_string()),
            room_name: None,
            auth_token: None,
            timeout: DEFAULT_TRACK_PUBLISH_TIMEOUT,
        }
    }
}

#[derive(Default)]
pub struct Signaller {
    settings: Mutex<Settings>,
    connection: Mutex<Option<Connection>>,
    join_canceller: Mutex<Option<futures::future::AbortHandle>>,
    signal_task_canceller: Mutex<Option<futures::future::AbortHandle>>,
}

struct Channels {
    reliable_channel: gst_webrtc::WebRTCDataChannel,
    lossy_channel: gst_webrtc::WebRTCDataChannel,
}

struct Connection {
    signal_client: Arc<signal_client::SignalClient>,
    pending_tracks: HashMap<String, oneshot::Sender<proto::TrackInfo>>,
    signal_task: JoinHandle<()>,
    early_candidates: Option<Vec<String>>,
    channels: Option<Channels>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IceCandidateJson {
    pub sdp_mid: String,
    pub sdp_m_line_index: i32,
    pub candidate: String,
}

impl Signaller {
    fn raise_error(&self, msg: String) {
        self.obj()
            .emit_by_name::<()>("error", &[&format!("Error: {msg}")]);
    }

    async fn signal_task(&self, mut signal_events: signal_client::SignalEvents) {
        loop {
            match wait_async(&self.signal_task_canceller, signal_events.recv(), 0).await {
                Ok(Some(signal)) => match signal {
                    signal_client::SignalEvent::Message(signal) => {
                        self.on_signal_event(*signal).await;
                    }
                    signal_client::SignalEvent::Close => {
                        gst::debug!(CAT, imp: self, "Close");
                        self.raise_error("Server disconnected".to_string());
                        break;
                    }
                },
                Ok(None) => {}
                Err(err) => match err {
                    WaitError::FutureAborted => {
                        gst::debug!(CAT, imp: self, "Closing signal_task");
                        break;
                    }
                    WaitError::FutureError(err) => self.raise_error(err.to_string()),
                },
            }
        }
    }

    async fn on_signal_event(&self, event: proto::signal_response::Message) {
        match event {
            proto::signal_response::Message::Answer(answer) => {
                gst::debug!(CAT, imp: self, "Received publisher answer: {:?}", answer);
                let sdp = match gst_sdp::SDPMessage::parse_buffer(answer.sdp.as_bytes()) {
                    Ok(sdp) => sdp,
                    Err(_) => {
                        self.raise_error("Couldn't parse Answer SDP".to_string());
                        return;
                    }
                };
                let answer = gst_webrtc::WebRTCSessionDescription::new(
                    gst_webrtc::WebRTCSDPType::Answer,
                    sdp,
                );
                self.obj()
                    .emit_by_name::<()>("session-description", &[&"unique", &answer]);
            }
            proto::signal_response::Message::Trickle(trickle) => {
                gst::debug!(CAT, imp: self, "Received ice_candidate {:?}", trickle);

                if trickle.target() == proto::SignalTarget::Publisher {
                    if let Ok(json) =
                        serde_json::from_str::<IceCandidateJson>(&trickle.candidate_init)
                    {
                        let mline = json.sdp_m_line_index as u32;
                        self.obj().emit_by_name::<()>(
                            "handle-ice",
                            &[&"unique", &mline, &Some(json.sdp_mid), &json.candidate],
                        );
                    }
                }
            }

            proto::signal_response::Message::ConnectionQuality(quality) => {
                gst::debug!(CAT, imp: self, "Connection quality: {:?}", quality);
            }

            proto::signal_response::Message::TrackPublished(publish_res) => {
                gst::debug!(CAT, imp: self, "Track published: {:?}", publish_res);
                if let Some(connection) = &mut *self.connection.lock().unwrap() {
                    if let Some(tx) = connection.pending_tracks.remove(&publish_res.cid) {
                        let _ = tx.send(publish_res.track.unwrap());
                    }
                }
            }

            proto::signal_response::Message::Leave(leave) => {
                gst::debug!(CAT, imp: self, "Leave: {:?}", leave);
            }

            _ => {}
        }
    }
}

impl SignallableImpl for Signaller {
    fn start(&self) {
        gst::debug!(CAT, imp: self, "Connecting");

        let wsurl = if let Some(wsurl) = &self.settings.lock().unwrap().wsurl {
            wsurl.clone()
        } else {
            self.raise_error("WebSocket URL must be set".to_string());
            return;
        };

        let auth_token = {
            let settings = self.settings.lock().unwrap();

            if let Some(auth_token) = &settings.auth_token {
                auth_token.clone()
            } else if let (
                Some(api_key),
                Some(secret_key),
                Some(identity),
                Some(participant_name),
                Some(room_name),
            ) = (
                &settings.api_key,
                &settings.secret_key,
                &settings.identity,
                &settings.participant_name,
                &settings.room_name,
            ) {
                let grants = VideoGrants {
                    room_join: true,
                    can_subscribe: false,
                    room: room_name.clone(),
                    ..Default::default()
                };
                let access_token = AccessToken::with_api_key(api_key, secret_key)
                    .with_name(participant_name)
                    .with_identity(identity)
                    .with_grants(grants);
                match access_token.to_jwt() {
                    Ok(token) => token,
                    Err(err) => {
                        self.raise_error(format!(
                            "{:?}",
                            anyhow!("Could not create auth token {err}")
                        ));
                        return;
                    }
                }
            } else {
                self.raise_error("Either auth-token or (api-key and secret-key and identity and room-name) must be set".to_string());
                return;
            }
        };

        gst::debug!(CAT, imp: self, "We have an authentication token");

        let weak_imp = self.downgrade();
        RUNTIME.spawn(async move {
            let imp = if let Some(imp) = weak_imp.upgrade() {
                imp
            } else {
                return;
            };

            let options = signal_client::SignalOptions::default();
            gst::debug!(CAT, imp: imp, "Connecting to {}", wsurl);

            let res = signal_client::SignalClient::connect(&wsurl, &auth_token, options).await;
            let (signal_client, join_response, signal_events) = match res {
                Err(err) => {
                    imp.obj()
                        .emit_by_name::<()>("error", &[&format!("{:?}", anyhow!("Error: {err}"))]);
                    return;
                }
                Ok(ok) => ok,
            };
            let signal_client = Arc::new(signal_client);

            gst::debug!(
                CAT,
                imp: imp,
                "Connected with JoinResponse: {:?}",
                join_response
            );

            let weak_imp = imp.downgrade();
            let signal_task = RUNTIME.spawn(async move {
                if let Some(imp) = weak_imp.upgrade() {
                    imp.signal_task(signal_events).await;
                }
            });

            let weak_imp = imp.downgrade();
            imp.obj().connect_closure(
                "webrtcbin-ready",
                false,
                glib::closure!(|_signaler: &super::LiveKitSignaller,
                                _consumer_identifier: &str,
                                webrtcbin: &gst::Element| {
                    gst::info!(CAT, "Adding data channels");
                    let reliable_channel = webrtcbin.emit_by_name::<gst_webrtc::WebRTCDataChannel>(
                        "create-data-channel",
                        &[
                            &"_reliable",
                            &gst::Structure::builder("config")
                                .field("ordered", true)
                                .build(),
                        ],
                    );
                    let lossy_channel = webrtcbin.emit_by_name::<gst_webrtc::WebRTCDataChannel>(
                        "create-data-channel",
                        &[
                            &"_lossy",
                            &gst::Structure::builder("config")
                                .field("ordered", true)
                                .field("max-retransmits", 0)
                                .build(),
                        ],
                    );

                    if let Some(imp) = weak_imp.upgrade() {
                        let mut connection = imp.connection.lock().unwrap();
                        if let Some(connection) = connection.as_mut() {
                            connection.channels = Some(Channels {
                                reliable_channel,
                                lossy_channel,
                            });
                        }
                    }
                }),
            );

            let connection = Connection {
                signal_client,
                signal_task,
                pending_tracks: Default::default(),
                early_candidates: Some(Vec::new()),
                channels: None,
            };

            if let Ok(mut sc) = imp.connection.lock() {
                *sc = Some(connection);
            }

            imp.obj().emit_by_name::<()>(
                "session-requested",
                &[
                    &"unique",
                    &"unique",
                    &None::<gst_webrtc::WebRTCSessionDescription>,
                ],
            );
        });
    }

    fn send_sdp(&self, _session_id: &str, sessdesc: &gst_webrtc::WebRTCSessionDescription) {
        gst::debug!(CAT, imp: self, "Created offer SDP {:#?}", sessdesc.sdp());

        assert!(sessdesc.type_() == gst_webrtc::WebRTCSDPType::Offer);

        let weak_imp = self.downgrade();
        let sessdesc = sessdesc.clone();
        RUNTIME.spawn(async move {
            if let Some(imp) = weak_imp.upgrade() {
                let sdp = sessdesc.sdp();
                let signal_client = imp
                    .connection
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .signal_client
                    .clone();
                let timeout = imp.settings.lock().unwrap().timeout;

                for media in sdp.medias() {
                    if let Some(mediatype) = media.media() {
                        let (mtype, msource) = if mediatype == "audio" {
                            (
                                proto::TrackType::Audio,
                                proto::TrackSource::Microphone as i32,
                            )
                        } else if mediatype == "video" {
                            (proto::TrackType::Video, proto::TrackSource::Camera as i32)
                        } else {
                            continue;
                        };

                        let mut disable_red = true;

                        if mtype == proto::TrackType::Audio {
                            for format in media.formats() {
                                if let Ok(pt) = format.parse::<i32>() {
                                    if let Some(caps) = media.caps_from_media(pt) {
                                        let s = caps.structure(0).unwrap();
                                        let encoding_name = s.get::<&str>("encoding-name").unwrap();
                                        if encoding_name == "RED" {
                                            disable_red = false;
                                        }
                                    }
                                }
                            }
                        }

                        // Our SDP should always have a mid
                        let mid = media.attribute_val("mid").unwrap().to_string();

                        let mut trackid = "";
                        for attr in media.attributes() {
                            if attr.key() == "ssrc" {
                                if let Some(val) = attr.value() {
                                    let split: Vec<&str> = val.split_whitespace().collect();
                                    if split.len() == 3 && split[1].starts_with("msid:") {
                                        trackid = split[2];
                                        break;
                                    }
                                }
                            }
                        }

                        // let layers = if mtype == proto::TrackType::Video {
                        //     let ssrc = if let Some(attr) = media.attribute_val("ssrc") {
                        //         let mut split = attr.split_whitespace();
                        //         if let Some(ssrc_str) = split.next() {
                        //             ssrc_str.parse().unwrap_or(0)
                        //         } else {
                        //             0
                        //         }
                        //     } else {
                        //         0 as u32
                        //     };

                        //     gst::debug!(CAT, imp: imp, "Adding video track {mid} with ssrc {ssrc}");
                        //     vec![ proto::VideoLayer {
                        //         quality: proto::VideoQuality::High as i32,
                        //         width: 1280,
                        //         height: 720,
                        //         bitrate: 5000,
                        //         ssrc
                        //     }]
                        // } else {
                        //     gst::debug!(CAT, imp: imp, "Adding audio track {mid}");
                        //     Vec::new()
                        // };

                        let req = proto::AddTrackRequest {
                            cid: trackid.to_string(),
                            name: mid.clone(),
                            r#type: mtype as i32,
                            muted: false,
                            source: msource,
                            disable_dtx: true,
                            disable_red,
                            //    layers: layers,
                            ..Default::default()
                        };

                        let (tx, rx) = oneshot::channel();
                        if let Some(connection) = &mut *imp.connection.lock().unwrap() {
                            let pendings_tracks = &mut connection.pending_tracks;
                            if pendings_tracks.contains_key(&req.cid) {
                                panic!("track already published");
                            }
                            pendings_tracks.insert(req.cid.clone(), tx);
                        }

                        let cid = req.cid.clone();

                        signal_client
                            .send(proto::signal_request::Message::AddTrack(req))
                            .await;

                        if let Err(err) = wait_async(&imp.join_canceller, rx, timeout).await {
                            if let Some(connection) = &mut *imp.connection.lock().unwrap() {
                                connection.pending_tracks.remove(&cid);
                            }

                            match err {
                                WaitError::FutureAborted => {
                                    gst::warning!(CAT, imp: imp, "Future aborted")
                                }
                                WaitError::FutureError(err) => imp.raise_error(err.to_string()),
                            };
                        }
                    }
                }

                gst::debug!(CAT, imp: imp, "Sending SDP now");
                signal_client
                    .send(proto::signal_request::Message::Offer(
                        proto::SessionDescription {
                            r#type: "offer".to_string(),
                            sdp: sessdesc.sdp().to_string(),
                        },
                    ))
                    .await;

                if let Some(imp) = weak_imp.upgrade() {
                    let early_candidates =
                        if let Some(connection) = &mut *imp.connection.lock().unwrap() {
                            connection.early_candidates.take()
                        } else {
                            None
                        };

                    if let Some(mut early_candidates) = early_candidates {
                        while let Some(candidate_str) = early_candidates.pop() {
                            gst::debug!(
                                CAT,
                                imp: imp,
                                "Sending delayed ice candidate {candidate_str:?}"
                            );
                            signal_client
                                .send(proto::signal_request::Message::Trickle(
                                    proto::TrickleRequest {
                                        candidate_init: candidate_str,
                                        target: proto::SignalTarget::Publisher as i32,
                                    },
                                ))
                                .await;
                        }
                    }
                }
            }
        });
    }

    fn add_ice(
        &self,
        _session_id: &str,
        candidate: &str,
        sdp_m_line_index: u32,
        sdp_mid: Option<String>,
    ) {
        let candidate_str = serde_json::to_string(&IceCandidateJson {
            sdp_mid: sdp_mid.unwrap_or("".to_string()),
            sdp_m_line_index: sdp_m_line_index as i32,
            candidate: candidate.to_string(),
        })
        .unwrap();

        if let Some(connection) = &mut *self.connection.lock().unwrap() {
            if let Some(early_candidates) = connection.early_candidates.as_mut() {
                gst::debug!(CAT, imp: self, "Delaying ice candidate {candidate_str:?}");

                early_candidates.push(candidate_str);
                return;
            }
        };

        gst::debug!(CAT, imp: self, "Sending ice candidate {candidate_str:?}");

        let imp = self.downgrade();
        RUNTIME.spawn(async move {
            if let Some(imp) = imp.upgrade() {
                let signal_client = if let Some(connection) = &mut *imp.connection.lock().unwrap() {
                    connection.signal_client.clone()
                } else {
                    return;
                };

                signal_client
                    .send(proto::signal_request::Message::Trickle(
                        proto::TrickleRequest {
                            candidate_init: candidate_str,
                            target: proto::SignalTarget::Publisher as i32,
                        },
                    ))
                    .await;
            };
        });
    }

    fn stop(&self) {
        if let Some(canceller) = &*self.join_canceller.lock().unwrap() {
            canceller.abort();
        }
        if let Some(canceller) = &*self.signal_task_canceller.lock().unwrap() {
            canceller.abort();
        }

        if let Some(connection) = self.connection.lock().unwrap().take() {
            block_on(connection.signal_task).unwrap();
            block_on(connection.signal_client.close());
        }
    }

    fn end_session(&self, session_id: &str) {
        assert_eq!(session_id, "unique");
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "GstLiveKitWebRTCSinkSignaller";
    type Type = super::LiveKitSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for Signaller {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("ws-url")
                    .nick("WebSocket URL")
                    .blurb("The URL of the websocket of the LiveKit server")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("api-key")
                    .nick("API key")
                    .blurb("API key (combined into auth-token)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("secret-key")
                    .nick("Secret Key")
                    .blurb("Secret key (combined into auth-token)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("participant-name")
                    .nick("Participant name")
                    .blurb("Human readable name of the participant (combined into auth-token)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("identity")
                    .nick("Participant Identity")
                    .blurb("Identity of the participant (combined into auth-token)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("auth-token")
                    .nick("Authorization Token")
                    .blurb("Authentication token to use (contains api_key/secret/name/identity)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("room-name")
                    .nick("Room Name")
                    .blurb("Name of the room to join (mandatory)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout join requests.")
                    .maximum(3600)
                    .minimum(1)
                    .default_value(DEFAULT_TRACK_PUBLISH_TIMEOUT)
                    .build(),
                glib::ParamSpecObject::builder::<gst_webrtc::WebRTCDataChannel>("reliable-channel")
                    .nick("Reliable Channel")
                    .blurb("Reliable Data Channel object.")
                    .flags(glib::ParamFlags::READABLE)
                    .build(),
                glib::ParamSpecObject::builder::<gst_webrtc::WebRTCDataChannel>("lossy-channel")
                    .nick("Lossy Channel")
                    .blurb("Lossy Data Channel object.")
                    .flags(glib::ParamFlags::READABLE)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "ws-url" => {
                settings.wsurl = value.get().unwrap();
            }
            "api-key" => {
                settings.api_key = value.get().unwrap();
            }
            "secret-key" => {
                settings.secret_key = value.get().unwrap();
            }
            "participant-name" => {
                settings.participant_name = value.get().unwrap();
            }
            "identity" => {
                settings.identity = value.get().unwrap();
            }
            "room-name" => {
                settings.room_name = value.get().unwrap();
            }
            "auth-token" => {
                settings.auth_token = value.get().unwrap();
            }
            "timeout" => {
                settings.timeout = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "ws-url" => settings.wsurl.to_value(),
            "api-key" => settings.api_key.to_value(),
            "secret-key" => settings.secret_key.to_value(),
            "participant-name" => settings.participant_name.to_value(),
            "identity" => settings.identity.to_value(),
            "room-name" => settings.room_name.to_value(),
            "auth-token" => settings.auth_token.to_value(),
            "timeout" => settings.timeout.to_value(),
            channel @ ("reliable-channel" | "lossy-channel") => {
                let channel = if let Some(connection) = &*self.connection.lock().unwrap() {
                    if let Some(channels) = &connection.channels {
                        if channel == "reliable-channel" {
                            Some(channels.reliable_channel.clone())
                        } else {
                            Some(channels.lossy_channel.clone())
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                channel.to_value()
            }
            _ => unimplemented!(),
        }
    }
}
