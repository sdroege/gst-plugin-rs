use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::transceiver::Transceiver;
use crate::utils::parse_request_name_single_index;
use crate::webrtcsend::WebRTCSendSinkPad;
use crate::webrtcsession::SharedWebRTCState;
use crate::webrtcsession::WebRTCSession;
use crate::webrtcsession::dtls::KeyMaterial;
use crate::webrtcsession::srtp::SrtpKeyMaterial;

use super::WebRTCSendEarlyDataMode;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc2send",
        gst::DebugColorFlags::empty(),
        Some("WebRTC2Send"),
    )
});

/*
 * Locking order:
 *
 * - state lock
 * - session lock
 * - transceiver lock
 * - pad lock
 */

#[derive(Default, Debug)]
pub struct WebRTCSend {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct Settings {
    early_data_mode: WebRTCSendEarlyDataMode,
    session_id: String,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    session: Option<SharedWebRTCState>,
    max_sinkpad_id: usize,
    transport: Option<TransportSendElements>,
    pending_transceivers: Vec<Transceiver>,
    pending_pads: Vec<WebRTCSendSinkPad>,
    pads: Vec<WebRTCSendSinkPad>,
}

impl State {
    pub fn unblock_sink_pad(&mut self, sink_pad: &WebRTCSendSinkPad) {
        if let Some(idx) = self
            .pending_pads
            .iter()
            .position(|pending| pending == sink_pad)
        {
            let _pending = self.pending_pads.swap_remove(idx);
        }
        if sink_pad.target().is_none() {
            let funnel_sink_pad = self
                .transport
                .as_ref()
                .unwrap()
                .rtpfunnel
                .request_pad_simple("sink_%u");
            sink_pad.set_target(funnel_sink_pad.as_ref()).unwrap();
        }
    }

    pub(crate) fn key_material(&self, material: &KeyMaterial, is_client: bool) {
        let Some(transport) = self.transport.as_ref() else {
            return;
        };
        let auth = material.srtp_auth();
        let cipher = material.srtp_cipher();
        transport
            .srtpenc
            .set_property_from_str("rtp-auth", auth.gst_enum_name());
        transport
            .srtpenc
            .set_property_from_str("rtcp-auth", auth.gst_enum_name());
        transport
            .srtpenc
            .set_property_from_str("rtp-cipher", cipher.gst_enum_name());
        transport
            .srtpenc
            .set_property_from_str("rtcp-cipher", cipher.gst_enum_name());
        let srtp_material = SrtpKeyMaterial::from_dtls(material, is_client);

        let encode_key = gst::Buffer::from_mut_slice(srtp_material.encode_key().to_vec());
        transport.srtpenc.set_property("key", encode_key);
    }
}

#[derive(Debug)]
struct TransportSendElements {
    rtpfunnel: gst::Element,
    _rtpsend: gst::Element,
    srtpenc: gst::Element,
    // funnel for rtcp-mux handling
    _srtpfunnel: gst::Element,
    _appsink: gst_app::AppSink,
}

impl WebRTCSend {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSend {
    const NAME: &'static str = "GstWebRTCSend";
    type Type = super::WebRTCSend;
    type ParentType = gst::Bin;
}

impl ObjectImpl for WebRTCSend {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder::<WebRTCSendEarlyDataMode>("early-data-mode")
                    .nick("Early data mode")
                    .blurb(
                        "Controls how this sink pad deals with buffers while connection is pending",
                    )
                    .build(),
                glib::ParamSpecString::builder("id")
                    .nick("ID")
                    .blurb("The identifier to connect with another webrtcrecv element")
                    .build(),
                glib::ParamSpecObject::builder::<WebRTCSession>("session")
                    .nick("Session")
                    .blurb("The internal session object. Only valid after reaching READY state")
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "early-data-mode" => self.settings.lock().unwrap().early_data_mode.to_value(),
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
            "early-data-mode" => {
                self.settings.lock().unwrap().early_data_mode =
                    value.get::<WebRTCSendEarlyDataMode>().unwrap();
            }
            "id" => {
                self.settings.lock().unwrap().session_id =
                    value.get::<String>().expect("type checked upstream")
            }
            _ => unreachable!(),
        }
    }
}

impl GstObjectImpl for WebRTCSend {}

impl ElementImpl for WebRTCSend {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "WebRTC Sender",
                "Network/Bin/WebRTC",
                "Send streams using WebRTC",
                "Matthew Waters <matthew@centricular.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let rtp_caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtp").build())
                .build();

            vec![
                gst::PadTemplate::builder(
                    "sink_%u",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &rtp_caps,
                )
                .gtype(WebRTCSendSinkPad::static_type())
                .build()
                .unwrap(),
                gst::PadTemplate::new(
                    "src_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &rtp_caps,
                )
                .unwrap(),
            ]
        });

        &PAD_TEMPLATES
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let session_id = self.settings.lock().unwrap().session_id.clone();
                let shared_session = SharedWebRTCState::send_get_or_init(session_id.clone());

                let rtp_id = format!("webrtcbin2-{session_id}");
                let rtpsend = gst::ElementFactory::make("rtpsend")
                    .property("rtp-id", rtp_id)
                    .property_from_str("rtp-profile", "avpf")
                    .build()
                    .map_err(|_| {
                        self.post_error_message(gst::error_msg!(
                            gst::CoreError::MissingPlugin,
                            ["\'rtpsend\' element not found"]
                        ));
                        gst::StateChangeError
                    })?;
                let rtpfunnel = gst::ElementFactory::make("rtpfunnel")
                    .build()
                    .map_err(|_| {
                        self.post_error_message(gst::error_msg!(
                            gst::CoreError::MissingPlugin,
                            ["\'rtpfunnel\' element not found"]
                        ));
                        gst::StateChangeError
                    })?;
                let srtpenc = gst::ElementFactory::make("srtpenc")
                    .property("random-key", true)
                    .build()
                    .map_err(|_| {
                        self.post_error_message(gst::error_msg!(
                            gst::CoreError::MissingPlugin,
                            ["\'srtpenc\' element not found"]
                        ));
                        gst::StateChangeError
                    })?;
                let srtpfunnel = gst::ElementFactory::make("funnel").build().map_err(|_| {
                    self.post_error_message(gst::error_msg!(
                        gst::CoreError::MissingPlugin,
                        ["\'funnel\' element not found"]
                    ));
                    gst::StateChangeError
                })?;
                let weak_self = self.downgrade();
                let appsink = gst_app::AppSink::builder()
                    .qos(false)
                    .async_(false)
                    // TODO: sync should become 'false' when when performing congestion control upstream
                    .sync(true)
                    .callbacks(
                        gst_app::AppSinkCallbacks::builder()
                            .new_sample(move |appsink| {
                                let Ok(sample) = appsink.pull_sample() else {
                                    return Err(gst::FlowError::Error);
                                };
                                let Some(buffer) = sample.buffer() else {
                                    return Err(gst::FlowError::Error);
                                };
                                let Some(this) = weak_self.upgrade() else {
                                    return Err(gst::FlowError::Flushing);
                                };
                                let state = this.state.lock().unwrap();
                                let Some(session) = state.session.as_ref() else {
                                    return Err(gst::FlowError::Flushing);
                                };
                                let Ok(map) = buffer.map_readable() else {
                                    return Err(gst::FlowError::Flushing);
                                };
                                session.session().imp().send_data(&map);
                                Ok(gst::FlowSuccess::Ok)
                            })
                            .build(),
                    )
                    .build();

                if self
                    .obj()
                    .add_many([
                        &rtpfunnel,
                        &rtpsend,
                        &srtpenc,
                        &srtpfunnel,
                        appsink.upcast_ref(),
                    ])
                    .is_err()
                {
                    return Err(gst::StateChangeError);
                }
                rtpsend.set_state(gst::State::Ready)?;

                rtpfunnel
                    .link_pads(Some("src"), &rtpsend, Some("rtp_sink_0"))
                    .unwrap();
                let _sink_pad = srtpenc.request_pad_simple("rtp_sink_0");
                rtpsend
                    .link_pads(Some("rtp_src_0"), &srtpenc, Some("rtp_sink_0"))
                    .unwrap();
                let _sink_pad = srtpenc.request_pad_simple("rtcp_sink_0");
                let _src_pad = rtpsend.request_pad_simple("rtcp_src_0");
                rtpsend
                    .link_pads(Some("rtcp_src_0"), &srtpenc, Some("rtcp_sink_0"))
                    .unwrap();
                srtpenc
                    .link_pads(Some("rtp_src_0"), &srtpfunnel, Some("sink_0"))
                    .unwrap();
                srtpenc
                    .link_pads(Some("rtcp_src_0"), &srtpfunnel, Some("sink_1"))
                    .unwrap();
                srtpfunnel.link(&appsink).unwrap();

                let rtp_session = rtpsend.emit_by_name::<gst::Object>("get-session", &[&0u32]);

                let mut state = self.state.lock().unwrap();
                let session = shared_session.session();
                let mut session_state = session.imp().state();
                session_state.set_rtp_session(rtp_session);
                session_state.set_webrtc_send(self.obj().downgrade());
                for transceiver in state.pending_transceivers.drain(..) {
                    session_state.add_transceiver(transceiver);
                }
                let transport = TransportSendElements {
                    rtpfunnel,
                    _rtpsend: rtpsend,
                    srtpenc,
                    _srtpfunnel: srtpfunnel,
                    _appsink: appsink,
                };
                state.session = Some(shared_session.clone());
                state.transport = Some(transport);
                drop(session_state);
                drop(state);

                self.obj().notify("session");
                session.imp().check_negotiation_needed();
            }
            _ => (),
        }
        self.parent_change_state(transition)?;
        match transition {
            gst::StateChange::ReadyToNull => {
                let mut state = self.state.lock().unwrap();
                if let Some(session) = state.session.take() {
                    session.unmark_send_outstanding();
                }
            }
            _ => (),
        }
        Ok(gst::StateChangeSuccess::Success)
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut state = self.state.lock().unwrap();
        let max_sinkpad_id = state.max_sinkpad_id;

        gst::trace!(CAT, "request new pad with name: {name:?}");

        match templ.name_template() {
            "sink_%u" => parse_request_name_single_index(name, "sink_", max_sinkpad_id).map(|id| {
                gst::trace!(
                    CAT,
                    "request new pad id {id} from name: {name:?}, max sinkpad {max_sinkpad_id}"
                );
                state.max_sinkpad_id = (id + 1).max(state.max_sinkpad_id);
                let transceiver = if let Some(session) = state.session.as_mut() {
                    let session = session.session();
                    let mut session_state = session.imp().state();
                    let mut mline_transceiver = None;
                    let mut possible_transceiver = None;
                    for transceiver in session_state.transceivers_mut() {
                        let trans_state = transceiver.imp().state();

                        if trans_state.mline() == Some(id) {
                            mline_transceiver = Some(transceiver.clone());
                        }

                        if !trans_state.direction().has_send() {
                            // ignore transceivers that are configured only for receiving
                            continue;
                        }

                        if state
                            .pads
                            .iter()
                            .any(|pad| pad.imp().state().transceiver() == transceiver)
                        {
                            // ignore transceivers that already have a sink pad associated.
                            continue;
                        }

                        // TODO: check stopped, codec-preferences, kind
                        possible_transceiver = Some(transceiver.clone());
                    }
                    mline_transceiver
                        .or(possible_transceiver)
                        .unwrap_or_else(|| {
                            let transceiver = glib::Object::new::<Transceiver>();
                            session_state.add_transceiver(transceiver.clone());
                            transceiver
                        })
                } else {
                    let transceiver = glib::Object::new::<Transceiver>();
                    state.pending_transceivers.push(transceiver.clone());
                    transceiver
                };
                gst::trace!(CAT, "found transceiver {transceiver:?} for id {id}");

                let sinkpad = gst::Pad::builder_from_template(templ)
                    .flags(gst::PadFlags::PROXY_CAPS)
                    .name(format!("sink_{id}"))
                    .event_function(|pad, parent, event| {
                        #[allow(clippy::single_match)]
                        match event.view() {
                            gst::EventView::Caps(caps) => {
                                gst::debug!(CAT, "have caps event {}", caps.caps());
                                let pad = pad.downcast_ref::<WebRTCSendSinkPad>().unwrap();
                                pad.imp().state().set_received_caps(caps.caps_owned());
                                let Some(parent) = pad.parent() else {
                                    return false;
                                };
                                let state = parent
                                    .downcast_ref::<super::WebRTCSend>()
                                    .unwrap()
                                    .imp()
                                    .state();
                                let Some(session) = state.session.as_ref() else {
                                    return false;
                                };
                                session.session().imp().check_negotiation_needed();
                                // TODO: handle caps received after remote offer set
                            }
                            _ => (),
                        }
                        gst::Pad::event_default(pad, parent, event)
                    })
                    .build()
                    .downcast::<WebRTCSendSinkPad>()
                    .unwrap();

                let mut trans_state = transceiver.imp().state();
                let mut sink_state = sinkpad.imp().state();

                let block_id = if self.settings.lock().unwrap().early_data_mode.is_block() {
                    sinkpad
                        .add_probe(
                            gst::PadProbeType::BLOCK
                                | gst::PadProbeType::BUFFER
                                | gst::PadProbeType::BUFFER_LIST,
                            |_pad, _info| gst::PadProbeReturn::Ok,
                        )
                        .unwrap()
                } else {
                    sinkpad
                        .add_probe(
                            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                            |_pad, _info| gst::PadProbeReturn::Drop,
                        )
                        .unwrap()
                };

                trans_state.set_send_pad(sinkpad.upcast_ref());
                sink_state.set_mline(Some(id));
                sink_state.set_block_id(block_id);
                state.pads.push(sinkpad.clone());
                state.pending_pads.push(sinkpad.clone());
                drop(trans_state);
                sink_state.set_transceiver(transceiver);
                drop(sink_state);
                drop(state);

                self.obj().add_pad(&sinkpad).unwrap();
                sinkpad.upcast()
            }),
            _ => None,
        }
    }
}

impl BinImpl for WebRTCSend {}
