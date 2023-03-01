// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::ffi;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cea608utils",
        gst::DebugColorFlags::empty(),
        Some("CEA-608 utilities"),
    )
});

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

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
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

pub(crate) fn is_basicna(cc_data: u16) -> bool {
    0x0000 != (0x6000 & cc_data)
}

pub(crate) fn is_preamble(cc_data: u16) -> bool {
    0x1040 == (0x7040 & cc_data)
}

pub(crate) fn is_midrowchange(cc_data: u16) -> bool {
    0x1120 == (0x7770 & cc_data)
}

pub(crate) fn is_specialna(cc_data: u16) -> bool {
    0x1130 == (0x7770 & cc_data)
}

pub(crate) fn is_xds(cc_data: u16) -> bool {
    0x0000 == (0x7070 & cc_data) && 0x0000 != (0x0F0F & cc_data)
}

pub(crate) fn is_westeu(cc_data: u16) -> bool {
    0x1220 == (0x7660 & cc_data)
}

pub(crate) fn is_control(cc_data: u16) -> bool {
    0x1420 == (0x7670 & cc_data) || 0x1720 == (0x7770 & cc_data)
}

pub(crate) fn parse_control(cc_data: u16) -> (ffi::eia608_control_t, i32) {
    unsafe {
        let mut chan = 0;
        let cmd = ffi::eia608_parse_control(cc_data, &mut chan);

        (cmd, chan)
    }
}

#[derive(Debug)]
pub(crate) struct Preamble {
    pub row: i32,
    pub col: i32,
    pub style: TextStyle,
    pub chan: i32,
    pub underline: i32,
}

pub(crate) fn parse_preamble(cc_data: u16) -> Preamble {
    unsafe {
        let mut row = 0;
        let mut col = 0;
        let mut style = 0;
        let mut chan = 0;
        let mut underline = 0;

        ffi::eia608_parse_preamble(
            cc_data,
            &mut row,
            &mut col,
            &mut style,
            &mut chan,
            &mut underline,
        );

        Preamble {
            row,
            col,
            style: style.into(),
            chan,
            underline,
        }
    }
}

#[derive(Debug)]
pub(crate) struct MidRowChange {
    pub chan: i32,
    pub style: TextStyle,
    pub underline: bool,
}

pub(crate) fn parse_midrowchange(cc_data: u16) -> MidRowChange {
    unsafe {
        let mut chan = 0;
        let mut style = 0;
        let mut underline = 0;

        ffi::eia608_parse_midrowchange(cc_data, &mut chan, &mut style, &mut underline);

        MidRowChange {
            chan,
            style: style.into(),
            underline: underline > 0,
        }
    }
}

pub(crate) fn eia608_to_utf8(cc_data: u16) -> (Option<char>, Option<char>, i32) {
    unsafe {
        let mut chan = 0;
        let mut char1 = [0u8; 5usize];
        let mut char2 = [0u8; 5usize];

        let n_chars = ffi::eia608_to_utf8(
            cc_data,
            &mut chan,
            char1.as_mut_ptr() as *mut _,
            char2.as_mut_ptr() as *mut _,
        );

        let char1 = if n_chars > 0 {
            Some(
                std::ffi::CStr::from_bytes_with_nul_unchecked(&char1)
                    .to_string_lossy()
                    .chars()
                    .next()
                    .unwrap(),
            )
        } else {
            None
        };

        let char2 = if n_chars > 1 {
            Some(
                std::ffi::CStr::from_bytes_with_nul_unchecked(&char2)
                    .to_string_lossy()
                    .chars()
                    .next()
                    .unwrap(),
            )
        } else {
            None
        };

        (char1, char2, chan)
    }
}

pub(crate) fn eia608_to_text(cc_data: u16) -> String {
    unsafe {
        let bufsz = ffi::eia608_to_text(std::ptr::null_mut(), 0, cc_data);
        let mut data = Vec::with_capacity((bufsz + 1) as usize);
        ffi::eia608_to_text(data.as_ptr() as *mut _, (bufsz + 1) as usize, cc_data);
        data.set_len(bufsz as usize);
        String::from_utf8_unchecked(data)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodeSpace {
    BasicNA,
    SpecialNA,
    WestEU,
}

impl CodeSpace {
    fn from_cc_data(cc_data: u16) -> Self {
        if is_basicna(cc_data) {
            Self::BasicNA
        } else if is_specialna(cc_data) {
            Self::SpecialNA
        } else if is_westeu(cc_data) {
            Self::WestEU
        } else {
            unreachable!()
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Cea608Text {
    pub char1: Option<char>,
    pub char2: Option<char>,
    pub code_space: CodeSpace,
    pub chan: i32,
}

#[derive(Debug)]
pub(crate) enum Cea608 {
    Duplicate,
    NewMode(i32, Cea608Mode),
    EraseDisplay(i32),
    EraseNonDisplay(i32),
    CarriageReturn(i32),
    Backspace(i32),
    EndOfCaption(i32),
    TabOffset(i32, u32),
    Text(Cea608Text),
    Preamble(Preamble),
    MidRowChange(MidRowChange),
}

fn decode_control(cc_data: u16) -> Option<Cea608> {
    let (cmd, chan) = parse_control(cc_data);

    gst::log!(CAT, "Command for CC {}", chan);

    match cmd {
        ffi::eia608_control_t_eia608_control_resume_direct_captioning => {
            return Some(Cea608::NewMode(chan, Cea608Mode::PaintOn));
        }
        ffi::eia608_control_t_eia608_control_erase_display_memory => {
            return Some(Cea608::EraseDisplay(chan));
        }
        ffi::eia608_control_t_eia608_control_roll_up_2 => {
            return Some(Cea608::NewMode(chan, Cea608Mode::RollUp2));
        }
        ffi::eia608_control_t_eia608_control_roll_up_3 => {
            return Some(Cea608::NewMode(chan, Cea608Mode::RollUp3));
        }
        ffi::eia608_control_t_eia608_control_roll_up_4 => {
            return Some(Cea608::NewMode(chan, Cea608Mode::RollUp4));
        }
        ffi::eia608_control_t_eia608_control_carriage_return => {
            return Some(Cea608::CarriageReturn(chan));
        }
        ffi::eia608_control_t_eia608_control_backspace => {
            return Some(Cea608::Backspace(chan));
        }
        ffi::eia608_control_t_eia608_control_resume_caption_loading => {
            return Some(Cea608::NewMode(chan, Cea608Mode::PopOn));
        }
        ffi::eia608_control_t_eia608_control_erase_non_displayed_memory => {
            return Some(Cea608::EraseNonDisplay(chan));
        }
        ffi::eia608_control_t_eia608_control_end_of_caption => {
            return Some(Cea608::EndOfCaption(chan));
        }
        ffi::eia608_control_t_eia608_tab_offset_0
        | ffi::eia608_control_t_eia608_tab_offset_1
        | ffi::eia608_control_t_eia608_tab_offset_2
        | ffi::eia608_control_t_eia608_tab_offset_3 => {
            return Some(Cea608::TabOffset(
                chan,
                cmd - ffi::eia608_control_t_eia608_tab_offset_0,
            ));
        }
        // TODO
        ffi::eia608_control_t_eia608_control_alarm_off
        | ffi::eia608_control_t_eia608_control_delete_to_end_of_row => {}
        ffi::eia608_control_t_eia608_control_alarm_on
        | ffi::eia608_control_t_eia608_control_text_restart
        | ffi::eia608_control_t_eia608_control_text_resume_text_display => {}
        _ => {
            return None;
        }
    }
    None
}

#[derive(Debug, Default)]
pub(crate) struct Cea608StateTracker {
    last_cc_data: Option<u16>,
    pending_output: Vec<Cea608>,
}

impl Cea608StateTracker {
    pub(crate) fn push_cc_data(&mut self, cc_data: u16) {
        if (is_specialna(cc_data) || is_control(cc_data)) && Some(cc_data) == self.last_cc_data {
            gst::log!(CAT, "Skipping duplicate");
            self.pending_output.push(Cea608::Duplicate);
            return;
        }

        if is_xds(cc_data) {
            gst::log!(CAT, "XDS, ignoring");
        } else if is_control(cc_data) {
            if let Some(d) = decode_control(cc_data) {
                self.pending_output.push(d);
            }
        } else if is_basicna(cc_data) || is_specialna(cc_data) || is_westeu(cc_data) {
            let (char1, char2, chan) = eia608_to_utf8(cc_data);
            if char1.is_some() || char2.is_some() {
                self.pending_output.push(Cea608::Text(Cea608Text {
                    char1,
                    char2,
                    code_space: CodeSpace::from_cc_data(cc_data),
                    chan,
                }));
            }
        } else if is_preamble(cc_data) {
            self.pending_output
                .push(Cea608::Preamble(parse_preamble(cc_data)));
        } else if is_midrowchange(cc_data) {
            self.pending_output
                .push(Cea608::MidRowChange(parse_midrowchange(cc_data)));
        }
    }

    pub(crate) fn pop(&mut self) -> Option<Cea608> {
        self.pending_output.pop()
    }

    pub(crate) fn flush(&mut self) {
        self.pending_output = vec![];
    }
}
