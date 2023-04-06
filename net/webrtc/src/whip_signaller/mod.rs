// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct WhipSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

unsafe impl Send for WhipSignaller {}
unsafe impl Sync for WhipSignaller {}

impl Default for WhipSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}
