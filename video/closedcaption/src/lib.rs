// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

#![recursion_limit = "128"]

use gst::glib;

#[allow(non_camel_case_types, non_upper_case_globals, unused)]
#[allow(clippy::redundant_static_lifetimes, clippy::unreadable_literal)]
#[allow(clippy::useless_transmute, clippy::trivially_copy_pass_by_ref)]
mod ffi;

mod caption_frame;
mod ccdetect;
mod ccutils;
mod cea608overlay;
mod cea608tojson;
mod cea608tott;
mod line_reader;
mod mcc_enc;
mod mcc_parse;
mod parser_utils;
mod scc_enc;
mod scc_parse;
mod tttocea608;
mod tttojson;
mod ttutils;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    mcc_parse::register(plugin)?;
    mcc_enc::register(plugin)?;
    scc_parse::register(plugin)?;
    scc_enc::register(plugin)?;
    cea608tott::register(plugin)?;
    tttocea608::register(plugin)?;
    cea608overlay::register(plugin)?;
    ccdetect::register(plugin)?;
    tttojson::register(plugin)?;
    cea608tojson::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    rsclosedcaption,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
