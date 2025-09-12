// SPDX-License-Identifier: MPL-2.0

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

/**
 * SECTION:object-webrtcsession
 */
use gst::glib;

pub mod dtls;
mod imp;
pub mod sdp;
pub mod srtp;

glib::wrapper! {
    pub struct WebRTCSession(ObjectSubclass<imp::WebRTCSession>) @extends gst::Object;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GstWebRTCBin2SignalingStatus")]
pub enum SignalingStatus {
    #[default]
    Stable,
    HaveLocalOffer,
    HaveLocalPrAnswer,
    HaveRemoteOffer,
    HaveRemotePrAnswer,
    Closed,
}

static SHARED_WEBRTC_STATE: OnceLock<Mutex<HashMap<String, SharedWebRTCState>>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct SharedWebRTCState {
    name: String,
    inner: Arc<Mutex<SharedWebRTCStateInner>>,
}

#[derive(Debug)]
struct SharedWebRTCStateInner {
    session: WebRTCSession,
    send_outstanding: bool,
    recv_outstanding: bool,
}

impl SharedWebRTCState {
    pub fn recv_get_or_init(name: String) -> Self {
        SHARED_WEBRTC_STATE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .entry(name)
            .and_modify(|v| {
                v.inner.lock().unwrap().recv_outstanding = true;
            })
            .or_insert_with_key(|name| SharedWebRTCState {
                name: name.to_owned(),
                inner: Arc::new(Mutex::new(SharedWebRTCStateInner {
                    session: glib::Object::new(),
                    send_outstanding: false,
                    recv_outstanding: true,
                })),
            })
            .clone()
    }

    pub fn send_get_or_init(name: String) -> Self {
        SHARED_WEBRTC_STATE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .entry(name)
            .and_modify(|v| {
                v.inner.lock().unwrap().send_outstanding = true;
            })
            .or_insert_with_key(|name| SharedWebRTCState {
                name: name.to_owned(),
                inner: Arc::new(Mutex::new(SharedWebRTCStateInner {
                    session: glib::Object::new(),
                    send_outstanding: true,
                    recv_outstanding: false,
                })),
            })
            .clone()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn unmark_send_outstanding(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.send_outstanding = false;
        if !inner.recv_outstanding {
            Self::remove_from_global(&self.name);
        }
    }

    pub fn unmark_recv_outstanding(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.recv_outstanding = false;
        if !inner.send_outstanding {
            Self::remove_from_global(&self.name);
        }
    }

    fn remove_from_global(name: &str) {
        let _shared = SHARED_WEBRTC_STATE
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(name);
    }

    pub fn session(&self) -> WebRTCSession {
        self.inner.lock().unwrap().session.clone()
    }
}
