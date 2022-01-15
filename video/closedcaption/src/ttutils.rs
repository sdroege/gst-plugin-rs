// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
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

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq)]
pub enum TextStyle {
    White,
    Green,
    Blue,
    Cyan,
    Red,
    Yellow,
    Magenta,
    ItalicWhite,
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

// TODO allow indenting chunks
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Chunk {
    pub style: TextStyle,
    pub underline: bool,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Line {
    pub column: Option<u32>,
    pub row: Option<u32>,
    pub chunks: Vec<Chunk>,
    /* In roll-up modes, new lines don't introduce a carriage return by
     * default, but that can be overridden */
    pub carriage_return: Option<bool>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Lines {
    pub lines: Vec<Line>,
    pub mode: Option<Cea608Mode>,
    pub clear: Option<bool>,
}

impl Cea608Mode {
    pub fn is_rollup(&self) -> bool {
        *self == Cea608Mode::RollUp2 || *self == Cea608Mode::RollUp3 || *self == Cea608Mode::RollUp4
    }
}
