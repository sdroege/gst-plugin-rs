// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::{glib, prelude::ObjectExt, subclass::prelude::ObjectSubclassIsExt};

mod client;
mod server;

glib::wrapper! {
    pub struct WhepClientSignaller(ObjectSubclass<client::WhepClient>) @implements Signallable;
}

glib::wrapper! {
    pub struct WhepServerSignaller(ObjectSubclass<server::WhepServer>) @implements Signallable;
}

unsafe impl Send for WhepClientSignaller {}
unsafe impl Sync for WhepClientSignaller {}

impl Default for WhepClientSignaller {
    fn default() -> Self {
        let sig: WhepClientSignaller = glib::Object::new();
        sig.connect_closure("webrtcbin-ready", false, sig.imp().on_webrtcbin_ready());
        sig
    }
}

unsafe impl Send for WhepServerSignaller {}
unsafe impl Sync for WhepServerSignaller {}

impl Default for WhepServerSignaller {
    fn default() -> Self {
        let sig: WhepServerSignaller = glib::Object::new();
        sig.connect_closure("webrtcbin-ready", false, sig.imp().on_webrtcbin_ready());
        sig
    }
}
