// SPDX-License-Identifier: MPL-2.0

use gst::glib;

mod imp;

glib::wrapper! {
    pub struct Transceiver(ObjectSubclass<imp::Transceiver>) @extends gst::Object;
}
