// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst::glib;
use serde::{Deserialize, Serialize};

#[derive(
    Serialize, Deserialize, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::GEnum,
)]
#[repr(u32)]
#[genum(type_name = "GstTtToCea608Mode")]
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
#[derive(Serialize, Deserialize, Debug)]
pub struct Chunk {
    pub style: TextStyle,
    pub underline: bool,
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Line {
    pub column: Option<u32>,
    pub row: Option<u32>,
    pub chunks: Vec<Chunk>,
    /* In roll-up modes, new lines don't introduce a carriage return by
     * default, but that can be overridden */
    pub carriage_return: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
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
