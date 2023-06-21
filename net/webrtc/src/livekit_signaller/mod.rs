// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct LiveKitSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

unsafe impl Send for LiveKitSignaller {}
unsafe impl Sync for LiveKitSignaller {}

impl Default for LiveKitSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}
