// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(unused_doc_comments)]

/**
 * plugin-dsc:
 *
 * Plugin implementing H.274 Digital Signed Content (DSC) metadata, which allows
 * signing and verifying H.265 and H.266 video streams.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;

#[cfg(feature = "doc")]
use gst::prelude::*;

mod common;
mod dsc_substream;
mod nal_parser;
mod signer;
mod verifier;

pub use common::HashMethod;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc-rs")))]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    #[cfg(feature = "rustls-aws-lc-rs")]
    {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
    #[cfg(all(not(feature = "rustls-ring"), not(feature = "rustls-aws-lc-rs")))]
    {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            panic!(
                "No rustls crypto provider is configured. \
                Application must install a crypto provider. For example: \
                `rustls::crypto::ring::default_provider().install_default().unwrap();` \
                See https://docs.rs/rustls/latest/rustls/#cryptography-providers for details."
            );
        }
    }

    #[cfg(feature = "doc")]
    HashMethod::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    signer::register(plugin)?;
    verifier::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    dsc,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
