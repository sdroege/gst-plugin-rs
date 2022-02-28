// Copyright (C) 2019 Amazon.com, Inc. or its affiliates <mkolny@amazon.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstS3SinkOnError")]
pub(crate) enum OnError {
    #[enum_value(name = "Abort: Abort multipart upload on error.", nick = "abort")]
    Abort,
    #[enum_value(
        name = "Complete: Complete multipart upload on error.",
        nick = "complete"
    )]
    Complete,
    #[enum_value(name = "DoNothing: Do nothing on error.", nick = "nothing")]
    DoNothing,
}

glib::wrapper! {
    pub struct S3Sink(ObjectSubclass<imp::S3Sink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rusotos3sink",
        gst::Rank::Primary,
        S3Sink::static_type(),
    )
}
