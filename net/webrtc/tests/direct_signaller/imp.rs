use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_webrtc::WebRTCSessionDescription;

use gstrswebrtc::signaller::{Signallable, SignallableImpl};

use std::sync::{LazyLock, Mutex};

use super::CAT;

#[derive(Debug, Default)]
struct Settings {
    id: String,
    peer: Option<super::DirectSignaller>,
}

#[derive(Debug, Default)]
pub struct DirectSignaller {
    settings: Mutex<Settings>,
}

impl SignallableImpl for DirectSignaller {
    fn start(&self) {
        let settings = self.settings.lock().unwrap();
        let id = settings.id.clone();
        gst::debug!(CAT, imp = self, "{id}: Starting");
        drop(settings);

        self.obj().emit_by_name::<()>("started", &[]);
        gst::debug!(CAT, imp = self, "{id}: Started");
    }

    fn stop(&self) {
        let settings = self.settings.lock().unwrap();
        let id = settings.id.clone();
        gst::debug!(CAT, imp = self, "{id}: Stopping");
        drop(settings);

        self.obj().emit_by_name::<()>("shutdown", &[]);

        gst::debug!(CAT, imp = self, "{id}: Stopped");
    }

    fn send_sdp(&self, session_id: &str, sdp: &WebRTCSessionDescription) {
        let settings = self.settings.lock().unwrap();
        gst::debug!(
            CAT,
            imp = self,
            "{}: Sending sdp {session_id}: {sdp:?}",
            settings.id
        );

        let peer = settings.peer.clone().unwrap();
        drop(settings);

        peer.emit_by_name::<()>("session-description", &[&session_id, &sdp]);
    }

    fn add_ice(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: u32,
        sdp_mid: Option<String>,
    ) {
        let settings = self.settings.lock().unwrap();
        gst::debug!(
            CAT,
            imp = self,
            "{}: Sending ICE candidate {session_id}: {candidate}, {sdp_m_line_index}, {sdp_mid:?}",
            settings.id,
        );

        let peer = settings.peer.clone().unwrap();
        drop(settings);

        peer.emit_by_name::<()>(
            "handle-ice",
            &[&session_id, &sdp_m_line_index, &sdp_mid, &candidate],
        );
    }

    fn end_session(&self, session_id: &str) {
        gst::debug!(
            CAT,
            imp = self,
            "{}: Session ended {session_id}",
            self.settings.lock().unwrap().id
        );
    }

    fn end_session_with_error(&self, session_id: &str, error: Option<String>) {
        gst::error!(
            CAT,
            imp = self,
            "{}: Session ended {session_id}, error: {error:?}",
            self.settings.lock().unwrap().id
        );
    }
}

#[glib::object_subclass]
impl ObjectSubclass for DirectSignaller {
    const NAME: &'static str = "DirectWebRTCSinkSignaller";
    type Type = super::DirectSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for DirectSignaller {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("manual-sdp-munging")
                    .nick("Manual SDP munging")
                    .blurb("Whether the signaller manages SDP munging itself")
                    .default_value(false)
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("id")
                    .nick("Identifier")
                    .blurb("The identifier of this signaller interface")
                    .default_value("undefined")
                    .readwrite()
                    .build(),
                glib::ParamSpecObject::builder::<super::DirectSignaller>("peer")
                    .nick("Peer")
                    .blurb("The peer interface this signaller interface communicates with")
                    .build(),
            ]
        });

        PROPS.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "manual-sdp-munging" => false.to_value(),
            "id" => self.settings.lock().unwrap().id.to_value(),
            "peer" => self.settings.lock().unwrap().peer.as_ref().to_value(),
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "id" => {
                self.settings.lock().unwrap().id = value.get().unwrap();
            }
            "peer" => {
                self.settings.lock().unwrap().peer = Some(value.get().unwrap());
            }
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> =
            LazyLock::new(|| vec![glib::subclass::Signal::builder("started").build()]);

        SIGNALS.as_ref()
    }
}
