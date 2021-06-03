// Copyright (C) 2020 Philippe Normand <philn@igalia.com>
// Copyright (C) 2020 Natanael Mojica <neithanmo@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct AudioRNNoise(ObjectSubclass<imp::AudioRNNoise>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

unsafe impl Send for AudioRNNoise {}
unsafe impl Sync for AudioRNNoise {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "audiornnoise",
        gst::Rank::None,
        AudioRNNoise::static_type(),
    )
}
