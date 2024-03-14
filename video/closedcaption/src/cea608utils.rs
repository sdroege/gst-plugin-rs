// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use serde::{Deserialize, Serialize};

#[derive(
    Serialize, Deserialize, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum,
)]
#[repr(u32)]
#[enum_type(name = "GstTtToCea608Mode")]
pub enum Cea608Mode {
    PopOn,
    PaintOn,
    RollUp2,
    RollUp3,
    RollUp4,
}

impl From<cea608_types::Mode> for Cea608Mode {
    fn from(value: cea608_types::Mode) -> Self {
        match value {
            cea608_types::Mode::PopOn => Self::PopOn,
            cea608_types::Mode::PaintOn => Self::PaintOn,
            cea608_types::Mode::RollUp2 => Self::RollUp2,
            cea608_types::Mode::RollUp3 => Self::RollUp3,
            cea608_types::Mode::RollUp4 => Self::RollUp4,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
pub enum TextStyle {
    #[default]
    White,
    Green,
    Blue,
    Cyan,
    Red,
    Yellow,
    Magenta,
    ItalicWhite,
}

impl TextStyle {
    pub fn is_italics(&self) -> bool {
        *self == TextStyle::ItalicWhite
    }

    pub fn to_cea608_color(self) -> Option<cea608_types::tables::Color> {
        match self {
            Self::White => Some(cea608_types::tables::Color::White),
            Self::Green => Some(cea608_types::tables::Color::Green),
            Self::Blue => Some(cea608_types::tables::Color::Blue),
            Self::Cyan => Some(cea608_types::tables::Color::Cyan),
            Self::Red => Some(cea608_types::tables::Color::Red),
            Self::Yellow => Some(cea608_types::tables::Color::Yellow),
            Self::Magenta => Some(cea608_types::tables::Color::Magenta),
            Self::ItalicWhite => None,
        }
    }
}

impl From<cea608_types::tables::PreambleAddressCode> for TextStyle {
    fn from(value: cea608_types::tables::PreambleAddressCode) -> Self {
        value.color().into()
    }
}

impl From<cea608_types::tables::MidRow> for TextStyle {
    fn from(value: cea608_types::tables::MidRow) -> Self {
        if value.italics() {
            Self::ItalicWhite
        } else {
            // XXX: MidRow may not change color
            value
                .color()
                .unwrap_or(cea608_types::tables::Color::White)
                .into()
        }
    }
}

impl From<cea608_types::tables::Color> for TextStyle {
    fn from(value: cea608_types::tables::Color) -> Self {
        match value {
            cea608_types::tables::Color::White => Self::White,
            cea608_types::tables::Color::Green => Self::Green,
            cea608_types::tables::Color::Blue => Self::Blue,
            cea608_types::tables::Color::Cyan => Self::Cyan,
            cea608_types::tables::Color::Red => Self::Red,
            cea608_types::tables::Color::Yellow => Self::Yellow,
            cea608_types::tables::Color::Magenta => Self::Magenta,
        }
    }
}

impl From<u32> for TextStyle {
    fn from(val: u32) -> Self {
        match val {
            0 => TextStyle::White,
            1 => TextStyle::Green,
            2 => TextStyle::Blue,
            3 => TextStyle::Cyan,
            4 => TextStyle::Red,
            5 => TextStyle::Yellow,
            6 => TextStyle::Magenta,
            7 => TextStyle::ItalicWhite,
            _ => TextStyle::White,
        }
    }
}
