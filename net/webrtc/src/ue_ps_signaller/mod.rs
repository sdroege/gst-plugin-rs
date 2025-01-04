// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::glib;

mod imp;
mod protocol;

glib::wrapper! {
    pub struct UePsSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

impl Default for UePsSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}
