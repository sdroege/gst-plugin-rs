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

#![crate_type = "cdylib"]
#![recursion_limit = "128"]

// These macros are in weird paths currently,
// and extern crate is used to avoid the explicit imports
// should not be needed ideally in the upcoming releases.
// https://github.com/gtk-rs/glib/issues/420
// https://gitlab.freedesktop.org/gstreamer/gstreamer-rs/issues/170
#[macro_use]
extern crate glib;
#[macro_use]
extern crate gst;
#[macro_use]
extern crate lazy_static;

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

mod line_reader;
mod mcc_enc;
mod mcc_parse;
mod mcc_parser;
mod scc_enc;
mod scc_parse;
mod scc_parser;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    mcc_parse::register(plugin)?;
    mcc_enc::register(plugin)?;
    scc_parse::register(plugin)?;
    scc_enc::register(plugin)?;
    Ok(())
}

gst_plugin_define!(
    rsclosedcaption,
    "Rust Closed Caption Plugin",
    plugin_init,
    "0.1.0",
    "LGPL",
    "rsclosedcaption",
    "rsclosedcaption",
    "https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs",
    "2018-12-17"
);
