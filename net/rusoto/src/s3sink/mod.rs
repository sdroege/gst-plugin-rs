// Copyright (C) 2019 Amazon.com, Inc. or its affiliates <mkolny@amazon.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

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

unsafe impl Send for S3Sink {}
unsafe impl Sync for S3Sink {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rusotos3sink",
        gst::Rank::Primary,
        S3Sink::static_type(),
    )
}
