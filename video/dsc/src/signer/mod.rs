// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * element-dscsigner:
 * @short-description: Based on Digitally Signed Content (DSC), signs video buffers
 * using provided private keys. Signatures are added as metadata to the buffers.
 *
 * ## Example signing a stream
 * ```bash
 * gst-launch-1.0 videotestsrc pattern=ball num-buffers=30 ! "video/x-raw,framerate=30/1" !
 * videoconvert ! x265enc key-int-max=5 ! dscsigner private-key-path=./example_content.key
 * public-key-uri=./example_content.crt substream-length=5 ! dscverifier key-store-path="$(pwd)"
 * ! h265parse ! avdec_h265 ! videoconvert ! autovideosink
 * ```
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;
use gst_base::BaseTransform;

mod imp;

glib::wrapper! {
    pub struct DscSigner(ObjectSubclass<imp::DscSigner>) @extends BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "dscsigner",
        gst::Rank::NONE,
        DscSigner::static_type(),
    )
}
