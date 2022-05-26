// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty)]

use gst::glib;

mod aws_transcribe_parse;
mod aws_transcriber;
mod s3sink;
mod s3src;
mod s3url;
mod s3utils;

pub use aws_transcriber::AwsTranscriberResultStability;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    s3sink::register(plugin)?;
    s3src::register(plugin)?;
    aws_transcribe_parse::register(plugin)?;
    aws_transcriber::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    aws,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    // FIXME: MPL-2.0 is only allowed since 1.18.3 (as unknown) and 1.20 (as known)
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
