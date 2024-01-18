// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct JanusVRSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

impl Default for JanusVRSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}
