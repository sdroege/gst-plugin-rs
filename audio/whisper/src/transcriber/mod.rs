// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use whisper_rs::DtwModelPreset;

mod imp;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstWhisperTranscriberModelPreset")]
#[non_exhaustive]
pub enum WhisperTranscriberModelPreset {
    #[enum_value(name = "TinyEn", nick = "tiny-en")]
    TinyEn = 0,
    #[enum_value(name = "Tiny", nick = "tiny")]
    Tiny = 1,
    #[enum_value(name = "BaseEn", nick = "base-en")]
    BaseEn = 2,
    #[enum_value(name = "Base", nick = "base")]
    Base = 3,
    #[enum_value(name = "SmallEn", nick = "small-en")]
    SmallEn = 4,
    #[enum_value(name = "Small", nick = "small")]
    Small = 5,
    #[enum_value(name = "MediumEn", nick = "medium-en")]
    MediumEn = 6,
    #[enum_value(name = "Medium", nick = "medium")]
    Medium = 7,
    #[enum_value(name = "LargeV1", nick = "large-v1")]
    LargeV1 = 8,
    #[enum_value(name = "LargeV2", nick = "large-v2")]
    LargeV2 = 9,
    #[enum_value(name = "LargeV3", nick = "large-v3")]
    LargeV3 = 10,
    #[enum_value(name = "LargeV3Turbo", nick = "large-v3-turbo")]
    LargeV3Turbo = 11,
}

impl From<WhisperTranscriberModelPreset> for DtwModelPreset {
    fn from(val: WhisperTranscriberModelPreset) -> Self {
        use WhisperTranscriberModelPreset::*;
        match val {
            TinyEn => DtwModelPreset::TinyEn,
            Tiny => DtwModelPreset::Tiny,
            BaseEn => DtwModelPreset::BaseEn,
            Base => DtwModelPreset::Base,
            SmallEn => DtwModelPreset::SmallEn,
            Small => DtwModelPreset::Small,
            MediumEn => DtwModelPreset::MediumEn,
            Medium => DtwModelPreset::Medium,
            LargeV1 => DtwModelPreset::LargeV1,
            LargeV2 => DtwModelPreset::LargeV2,
            LargeV3 => DtwModelPreset::LargeV3,
            LargeV3Turbo => DtwModelPreset::LargeV3Turbo,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstWhisperTranscriberSamplingStrategy")]
#[non_exhaustive]
pub enum WhisperTranscriberSamplingStrategy {
    #[enum_value(name = "Greedy", nick = "greedy")]
    Greedy = 0,
    #[enum_value(name = "BeamSearch", nick = "beam-search")]
    BeamSearch = 1,
}

glib::wrapper! {
    pub struct Transcriber(ObjectSubclass<imp::Transcriber>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        WhisperTranscriberModelPreset::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        WhisperTranscriberSamplingStrategy::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    gst::Element::register(
        Some(plugin),
        "whispertranscriber",
        gst::Rank::NONE,
        Transcriber::static_type(),
    )
}
