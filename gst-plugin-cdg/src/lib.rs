// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "cdylib"]

#[macro_use]
extern crate glib;
#[macro_use]
extern crate gstreamer as gst;

mod cdgdec;
mod cdgparse;
mod constants;

use constants::{CDG_COMMAND, CDG_MASK, CDG_PACKET_SIZE};

const TYPEFIND_SEARCH_WINDOW: i64 = 100000; /* in bytes */

/* Return the percentage of CDG packets in the first @len bytes of @typefind */
fn cdg_packets_ratio(typefind: &mut gst::TypeFind, len: i64) -> i64 {
    let mut count = 0;
    let total = len / CDG_PACKET_SIZE as i64;

    for offset in (0..len).step_by(CDG_PACKET_SIZE as usize) {
        match typefind.peek(offset, CDG_PACKET_SIZE as u32) {
            Some(data) => {
                if data[0] & CDG_MASK == CDG_COMMAND {
                    count += 1;
                }
            }
            None => break,
        }
    }
    (count * 100) / total
}

fn typefind_register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    use gst::{Caps, TypeFind, TypeFindProbability};

    TypeFind::register(
        Some(plugin),
        "cdg_typefind",
        gst::Rank::None,
        Some("cdg"),
        Some(&Caps::new_simple("video/x-cdg", &[])),
        |mut typefind| {
            let proba = match cdg_packets_ratio(&mut typefind, TYPEFIND_SEARCH_WINDOW) {
                0...5 => TypeFindProbability::None,
                6...10 => TypeFindProbability::Possible,
                _ => TypeFindProbability::Likely,
            };

            if proba != gst::TypeFindProbability::None {
                typefind.suggest(proba, &Caps::new_simple("video/x-cdg", &[]));
            }
        },
    )
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    cdgdec::register(plugin)?;
    cdgparse::register(plugin)?;
    typefind_register(plugin)?;
    Ok(())
}

gst_plugin_define!(
    cdg,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
