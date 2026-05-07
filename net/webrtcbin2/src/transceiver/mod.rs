// SPDX-License-Identifier: MPL-2.0

use gst::glib;

pub mod imp;

glib::wrapper! {
    pub struct Transceiver(ObjectSubclass<imp::Transceiver>) @extends gst::Object;
}
