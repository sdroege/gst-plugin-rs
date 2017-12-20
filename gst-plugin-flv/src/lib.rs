// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "cdylib"]

extern crate flavors;
#[macro_use]
extern crate gst_plugin;
extern crate gst_plugin_simple;
#[macro_use]
extern crate gstreamer as gst;
extern crate muldiv;
extern crate nom;
extern crate num_rational;
extern crate url;

use gst_plugin_simple::demuxer::*;

mod flvdemux;

use flvdemux::FlvDemux;

fn plugin_init(plugin: &gst::Plugin) -> bool {
    demuxer_register(
        plugin,
        DemuxerInfo {
            name: "rsflvdemux".into(),
            long_name: "FLV Demuxer".into(),
            description: "Demuxes FLV Streams".into(),
            classification: "Codec/Demuxer".into(),
            author: "Sebastian Dröge <sebastian@centricular.com>".into(),
            rank: 256 + 100,
            create_instance: FlvDemux::new_boxed,
            input_caps: gst::Caps::new_simple("video/x-flv", &[]),
            output_caps: gst::Caps::new_any(),
        },
    );

    true
}

plugin_define!(
    b"rsflv\0",
    b"Rust FLV Plugin\0",
    plugin_init,
    b"1.0\0",
    b"MIT/X11\0",
    b"rsflv\0",
    b"rsflv\0",
    b"https://github.com/sdroege/rsplugin\0",
    b"2016-12-08\0"
);
