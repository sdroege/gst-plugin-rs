use gst::glib::{self, object::ObjectExt};
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
        let id = self.property::<String>("id");
        let peer = self.property::<DirectSignaller>("peer");
        let peer_id = peer.property::<String>("id");

        gst::debug!(CAT, obj = self, "{id}: Requesting session");

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
