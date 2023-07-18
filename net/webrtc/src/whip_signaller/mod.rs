// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct WhipClientSignaller(ObjectSubclass<imp::WhipClient>) @implements Signallable;
}

unsafe impl Send for WhipClientSignaller {}
unsafe impl Sync for WhipClientSignaller {}

impl Default for WhipClientSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}
