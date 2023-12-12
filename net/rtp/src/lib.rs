//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
// Copyright (C) 2022-24 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * plugin-rsrtp:
 *
 * Since: plugins-rs-0.9.0
 */
use gst::glib;

mod gcc;

mod audio_discont;
mod baseaudiopay;
mod basedepay;
mod basepay;

mod av1;
mod mp2t;
mod pcmau;
mod vp8;
mod vp9;

#[cfg(test)]
mod tests;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gcc::register(plugin)?;

    #[cfg(feature = "doc")]
    {
        use gst::prelude::*;

        crate::basepay::RtpBasePay2::static_type() // make base classes available in docs
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::basedepay::RtpBaseDepay2::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::baseaudiopay::RtpBaseAudioPay2::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    av1::depay::register(plugin)?;
    av1::pay::register(plugin)?;

    mp2t::depay::register(plugin)?;
    mp2t::pay::register(plugin)?;

    pcmau::depay::register(plugin)?;
    pcmau::pay::register(plugin)?;

    vp8::depay::register(plugin)?;
    vp8::pay::register(plugin)?;

    vp9::depay::register(plugin)?;
    vp9::pay::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    rsrtp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
