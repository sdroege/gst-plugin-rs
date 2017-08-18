// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "cdylib"]

extern crate url;
#[macro_use]
extern crate gst_plugin;
#[macro_use]
extern crate slog;
#[macro_use]
extern crate nom;
extern crate flavors;
extern crate muldiv;

use gst_plugin::plugin::*;
use gst_plugin::demuxer::*;
use gst_plugin::caps::*;

mod flvdemux;

use flvdemux::FlvDemux;

fn plugin_init(plugin: &Plugin) -> bool {
    demuxer_register(
        plugin,
        &DemuxerInfo {
            name: "rsflvdemux",
            long_name: "FLV Demuxer",
            description: "Demuxes FLV Streams",
            classification: "Codec/Demuxer",
            author: "Sebastian Dröge <sebastian@centricular.com>",
            rank: 256 + 100,
            create_instance: FlvDemux::new_boxed,
            input_caps: &Caps::new_simple("video/x-flv", &[]),
            output_caps: &Caps::new_any(),
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
