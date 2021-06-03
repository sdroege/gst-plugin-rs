// Copyright (C) 2019-2020 Sebastian Dröge <sebastian@centricular.com>
//
// Audio processing part of this file ported from ffmpeg/libavfilter/af_loudnorm.c
//
// Copyright (c) 2016 Kyle Swanson <k@ylo.ph>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Lesser General Public
// License as published by the Free Software Foundation; either
// version 2.1 of the License, or (at your option) any later version.
//
// FFmpeg is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public
// License along with FFmpeg; if not, write to the Free Software
// Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct AudioLoudNorm(ObjectSubclass<imp::AudioLoudNorm>) @extends gst::Element, gst::Object;
}

unsafe impl Send for AudioLoudNorm {}
unsafe impl Sync for AudioLoudNorm {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsaudioloudnorm",
        gst::Rank::None,
        AudioLoudNorm::static_type(),
    )
}
