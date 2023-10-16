// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct JanusVRSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

unsafe impl Send for JanusVRSignaller {}
unsafe impl Sync for JanusVRSignaller {}

impl Default for JanusVRSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}
