use gst::glib::{self, object::ObjectExt, subclass::types::ObjectSubclassIsExt};
use gstrswebrtc::signaller::Signallable;

mod imp;

pub static CAT: std::sync::LazyLock<gst::DebugCategory> = std::sync::LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc-direct-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC direct signaller"),
    )
});

pub const SESSION_ID: &str = "1234";

glib::wrapper! {
    pub struct DirectSignaller(ObjectSubclass<imp::DirectSignaller>) @implements Signallable;
}

impl DirectSignaller {
    pub fn new(id: &str) -> Self {
        let this = glib::Object::new::<Self>();
        this.set_property("id", id);

        this
    }

    pub fn request_session(&self) {
        let (peer, id) = {
            let settings = self.imp().settings.lock().unwrap();
            let id = settings.id.to_string();
            gst::debug!(CAT, obj = self, "{id}: Requesting session");

            let Some(ref peer) = settings.peer else {
                panic!("{id}: unknown peer (was about to request session)");
            };
            (
                peer.upgrade().unwrap_or_else(|| {
                    panic!("{id}: peer disappeared (was about to request session)");
                }),
                id,
            )
        };

        let peer_id = peer.property::<String>("id");

        self.emit_by_name::<()>("session-started", &[&SESSION_ID, &peer_id.as_str()]);

        peer.emit_by_name::<()>(
            "session-requested",
            &[
                &SESSION_ID,
                &id,
                &None::<gst_webrtc::WebRTCSessionDescription>,
            ],
        );
    }

    pub fn associate(peer0: &DirectSignaller, peer1: &DirectSignaller) {
        peer0.set_property("peer", peer1.clone());
        peer1.set_property("peer", peer0.clone());
    }
}

impl Default for DirectSignaller {
    fn default() -> Self {
        DirectSignaller::new("undefined")
    }
}
