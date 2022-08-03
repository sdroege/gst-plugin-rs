// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;
mod packet;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstAwsTranscriberResultStability")]
#[non_exhaustive]
pub enum AwsTranscriberResultStability {
    #[enum_value(name = "High: stabilize results as fast as possible", nick = "high")]
    High = 0,
    #[enum_value(
        name = "Medium: balance between stability and accuracy",
        nick = "medium"
    )]
    Medium = 1,
    #[enum_value(
        name = "Low: relatively less stable partial transcription results with higher accuracy",
        nick = "low"
    )]
    Low = 2,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstAwsTranscriberVocabularyFilterMethod")]
#[non_exhaustive]
pub enum AwsTranscriberVocabularyFilterMethod {
    #[enum_value(name = "Mask: replace words with ***", nick = "mask")]
    Mask = 0,
    #[enum_value(name = "Remove: delete words", nick = "remove")]
    Remove = 1,
    #[enum_value(name = "Tag: flag words without changing them", nick = "tag")]
    Tag = 2,
}

glib::wrapper! {
    pub struct Transcriber(ObjectSubclass<imp::Transcriber>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "awstranscriber",
        gst::Rank::None,
        Transcriber::static_type(),
    )
}
