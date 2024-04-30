// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
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
#[enum_type(name = "GstQuicPrivateKeyType")]
pub enum QuicPrivateKeyType {
    #[enum_value(name = "PKCS8: PKCS #8 Private Key.", nick = "pkcs8")]
    Pkcs8,
    #[enum_value(name = "RSA: RSA Private Key.", nick = "rsa")]
    Rsa,
}

glib::wrapper! {
    pub struct QuinnQuicSrc(ObjectSubclass<imp::QuinnQuicSrc>) @extends gst_base::BaseSrc, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "quinnquicsrc",
        gst::Rank::MARGINAL,
        QuinnQuicSrc::static_type(),
    )
}
