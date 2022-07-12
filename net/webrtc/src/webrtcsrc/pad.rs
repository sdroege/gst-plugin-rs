// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

#[derive(Default)]
pub struct WebRTCSrcPad {
    needs_raw: AtomicBool,
    stream_id: Mutex<Option<String>>,
}

impl WebRTCSrcPad {
    pub fn set_needs_decoding(&self, raw_wanted: bool) {
        self.needs_raw.store(raw_wanted, Ordering::SeqCst);
    }

    pub fn needs_decoding(&self) -> bool {
        self.needs_raw.load(Ordering::SeqCst)
    }

    pub fn set_stream_id(&self, stream_id: &str) {
        *self.stream_id.lock().unwrap() = Some(stream_id.to_string());
    }

    pub fn stream_id(&self) -> String {
        let stream_id = self.stream_id.lock().unwrap();
        stream_id.as_ref().unwrap().clone()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSrcPad {
    const NAME: &'static str = "GstWebRTCSrcPad";
    type Type = super::WebRTCSrcPad;
    type ParentType = gst::GhostPad;
}

impl ObjectImpl for WebRTCSrcPad {}
impl GstObjectImpl for WebRTCSrcPad {}
impl PadImpl for WebRTCSrcPad {}
impl ProxyPadImpl for WebRTCSrcPad {}
impl GhostPadImpl for WebRTCSrcPad {}
