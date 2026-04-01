// SPDX-License-Identifier: MPL-2.0

use std::sync::LazyLock;
use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::webrtcrecv::WebRTCRecvSrcPad;
use crate::webrtcsession::dtls::KeyMaterial;
use crate::webrtcsession::srtp::SrtpKeyMaterial;
use crate::webrtcsession::{SharedWebRTCState, WebRTCSession};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc2recv",
        gst::DebugColorFlags::empty(),
        Some("WebRTC2Recv"),
    )
});

#[derive(Default, Debug)]
pub struct WebRTCRecv {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct Settings {
    session_id: String,
}

#[derive(Debug, Default)]
struct State {
    session: Option<SharedWebRTCState>,
    transport: Option<TransportReceiveElements>,
    src_pad_serial: usize,
}

#[derive(Debug)]
struct TransportReceiveElements {
    appsrc: gst_app::AppSrc,
    _rtprecv: gst::Element,
    _srtpdec: gst::Element,
    material: Option<SrtpKeyMaterial>,
}

impl WebRTCRecv {
    pub(crate) fn recv_srtp(&self, data: Vec<u8>) {
        debug_assert!(!data.is_empty());
        let state = self.state.lock().unwrap();
        let Some(transport) = state.transport.as_ref() else {
            return;
        };
        let buffer = gst::Buffer::from_mut_slice(data);
        if let Err(e) = transport.appsrc.push_buffer(buffer) {
            gst::warning!(CAT, "receiving data failed with: {e:?}");
        }
    }

    pub(crate) fn key_material(&self, material: &KeyMaterial, is_client: bool) {
        let mut state = self.state.lock().unwrap();
        let Some(transport) = state.transport.as_mut() else {
            return;
        };
        transport.material = Some(SrtpKeyMaterial::from_dtls(material, is_client));
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCRecv {
    const NAME: &'static str = "GstWebRTCRecv";
    type Type = super::WebRTCRecv;
    type ParentType = gst::Bin;
}

impl ObjectImpl for WebRTCRecv {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("id")
                    .nick("ID")
                    .blurb("The identifier to connect with another webrtcsend element")
                    .build(),
                glib::ParamSpecObject::builder::<WebRTCSession>("session")
                    .nick("session")
                    .blurb("The internal session object. Only valid after reaching READY state")
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "id" => self.settings.lock().unwrap().session_id.to_value(),
            "session" => self
                .state
                .lock()
                .unwrap()
                .session
                .as_ref()
                .map(|s| s.session())
                .to_value(),
            _ => unreachable!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "id" => {
                self.settings.lock().unwrap().session_id =
                    value.get::<String>().expect("type checked upstream")
            }
            _ => unreachable!(),
        }
    }
}

impl GstObjectImpl for WebRTCRecv {}

impl ElementImpl for WebRTCRecv {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "WebRTC Receiver",
                "Network/Bin/WebRTC",
                "Receive streams using WebRTC",
                "Matthew Waters <matthew@centricular.com>",
            )
        });
        Some(&*METADATA)
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let session_id = self.settings.lock().unwrap().session_id.clone();
                let rtp_id = format!("webrtcbin2-{session_id}");
                let rtprecv = gst::ElementFactory::make("rtprecv")
                    .property("rtp-id", rtp_id)
                    .build()
                    .map_err(|_| {
                        self.post_error_message(gst::error_msg!(
                            gst::CoreError::MissingPlugin,
                            ["\'rtprecv\' element not found"]
                        ));
                        gst::StateChangeError
                    })?;
                let weak_self = self.downgrade();
                rtprecv.connect_pad_added(move |_rtprecv, pad| {
                    gst::info!(CAT, "have new rtprecv pad with name {}", pad.name());
                    if pad.name().starts_with("rtp_src_") {
                        let Some(this) = weak_self.upgrade() else {
                            return;
                        };
                        let mut state = this.state.lock().unwrap();
                        let src_pad_id = state.src_pad_serial;
                        state.src_pad_serial += 1;
                        drop(state);
                        let expose_pad = gst::GhostPad::builder_with_target(pad)
                            .unwrap()
                            .name(format!("src_{src_pad_id}"))
                            .build();
                        expose_pad.set_active(true).unwrap();
                        this.obj().add_pad(&expose_pad).unwrap();
                    }
                });
                let srtpdec = gst::ElementFactory::make("srtpdec").build().map_err(|_| {
                    self.post_error_message(gst::error_msg!(
                        gst::CoreError::MissingPlugin,
                        ["\'srtpdec\' element not found"]
                    ));
                    gst::StateChangeError
                })?;
                let weak_self = self.downgrade();
                srtpdec.connect_closure(
                    "request-key",
                    false,
                    glib::closure!(move |srtpdec: &gst::Element, _ssrc: u32| {
                        let this = weak_self.upgrade()?;
                        let state = this.state.lock().unwrap();
                        gst::trace!(CAT, "{srtpdec:?} request-key called");
                        let transport = state.transport.as_ref()?;
                        let material = transport.material.as_ref()?;
                        let key = gst::Buffer::from_mut_slice(material.decode_key().to_vec());
                        let ret = gst::Caps::builder("application/x-srtp")
                            .field("srtp-cipher", material.cipher().gst_enum_name())
                            .field("srtcp-cipher", material.cipher().gst_enum_name())
                            .field("srtp-auth", material.auth().gst_enum_name())
                            .field("srtcp-auth", material.auth().gst_enum_name())
                            .field("srtp-key", key)
                            .build();
                        gst::trace!(CAT, "{srtpdec:?} request-key returning {ret}");
                        Some(ret)
                    }),
                );
                let appsrc = gst_app::AppSrc::builder().format(gst::Format::Time).build();
                if self
                    .obj()
                    .add_many([appsrc.upcast_ref(), &srtpdec, &rtprecv])
                    .is_err()
                {
                    return Err(gst::StateChangeError);
                }
                rtprecv.set_state(gst::State::Ready)?;
                appsrc
                    .link_pads(Some("src"), &srtpdec, Some("rtp_sink"))
                    .unwrap();
                srtpdec
                    .link_pads(Some("rtp_src"), &rtprecv, Some("rtp_sink_0"))
                    .unwrap();
                srtpdec
                    .link_pads(Some("rtcp_src"), &rtprecv, Some("rtcp_sink_0"))
                    .unwrap();
                let rtp_session = rtprecv.emit_by_name::<gst::Object>("get-session", &[&0u32]);
                let mut state = self.state.lock().unwrap();
                state.transport = Some(TransportReceiveElements {
                    appsrc,
                    _rtprecv: rtprecv,
                    _srtpdec: srtpdec,
                    material: None,
                });
                let shared_session = SharedWebRTCState::recv_get_or_init(session_id.clone());
                let session = shared_session.session();
                let mut session_state = session.imp().state();
                session_state.set_rtp_session(rtp_session);
                session_state.set_webrtc_recv(self.obj().downgrade());
                state.session = Some(shared_session);
                drop(state);
                self.obj().notify("session");
            }
            _ => (),
        }
        self.parent_change_state(transition)?;
        match transition {
            gst::StateChange::ReadyToNull => {
                let mut state = self.state.lock().unwrap();
                if let Some(session) = state.session.take() {
                    session.unmark_recv_outstanding();
                }
            }
            _ => (),
        }
        Ok(gst::StateChangeSuccess::Success)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let rtp_caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtp").build())
                .build();

            vec![
                gst::PadTemplate::builder(
                    "src_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &rtp_caps,
                )
                .gtype(WebRTCRecvSrcPad::static_type())
                .build()
                .unwrap(),
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &rtp_caps,
                )
                .unwrap(),
            ]
        });

        &PAD_TEMPLATES
    }
}

impl BinImpl for WebRTCRecv {}
