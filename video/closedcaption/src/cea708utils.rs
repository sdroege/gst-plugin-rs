// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use cea708_types::{tables::*, Service};

use gst::glib;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::cea608utils::TextStyle;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cea708utils",
        gst::DebugColorFlags::empty(),
        Some("CEA-708 Utilities"),
    )
});

#[derive(
    Serialize, Deserialize, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum,
)]
#[repr(u32)]
#[enum_type(name = "GstTtToCea708Mode")]
pub enum Cea708Mode {
    PopOn,
    PaintOn,
    RollUp,
}

pub fn textstyle_foreground_color(style: TextStyle) -> cea708_types::tables::Color {
    let color = match style {
        TextStyle::Red => cea608_types::tables::Color::Red,
        TextStyle::Blue => cea608_types::tables::Color::Blue,
        TextStyle::Cyan => cea608_types::tables::Color::Cyan,
        TextStyle::White => cea608_types::tables::Color::White,
        TextStyle::Green => cea608_types::tables::Color::Green,
        TextStyle::Yellow => cea608_types::tables::Color::Yellow,
        TextStyle::Magenta => cea608_types::tables::Color::Magenta,
        TextStyle::ItalicWhite => cea608_types::tables::Color::White,
    };
    cea608_color_to_foreground_color(color)
}

pub fn cea608_color_to_foreground_color(
    color: cea608_types::tables::Color,
) -> cea708_types::tables::Color {
    match color {
        cea608_types::tables::Color::Red => cea708_types::tables::Color {
            r: ColorValue::Full,
            g: ColorValue::None,
            b: ColorValue::None,
        },
        cea608_types::tables::Color::Green => cea708_types::tables::Color {
            r: ColorValue::None,
            g: ColorValue::Full,
            b: ColorValue::None,
        },
        cea608_types::tables::Color::Blue => cea708_types::tables::Color {
            r: ColorValue::None,
            g: ColorValue::None,
            b: ColorValue::Full,
        },
        cea608_types::tables::Color::Cyan => cea708_types::tables::Color {
            r: ColorValue::None,
            g: ColorValue::Full,
            b: ColorValue::Full,
        },
        cea608_types::tables::Color::Yellow => cea708_types::tables::Color {
            r: ColorValue::Full,
            g: ColorValue::Full,
            b: ColorValue::None,
        },
        cea608_types::tables::Color::Magenta => cea708_types::tables::Color {
            r: ColorValue::Full,
            g: ColorValue::None,
            b: ColorValue::Full,
        },
        cea608_types::tables::Color::White => cea708_types::tables::Color {
            r: ColorValue::Full,
            g: ColorValue::Full,
            b: ColorValue::Full,
        },
    }
}

pub fn textstyle_to_pen_color(style: TextStyle) -> SetPenColorArgs {
    let black = Color {
        r: ColorValue::None,
        g: ColorValue::None,
        b: ColorValue::None,
    };
    SetPenColorArgs {
        foreground_color: textstyle_foreground_color(style),
        foreground_opacity: Opacity::Solid,
        background_color: black,
        background_opacity: Opacity::Solid,
        edge_color: black,
    }
}

#[derive(Debug)]
pub(crate) struct Cea708ServiceWriter {
    codes: Vec<Code>,
    service_no: u8,
    active_window: WindowBits,
    hidden_window: WindowBits,
}

impl Cea708ServiceWriter {
    pub fn new(service_no: u8) -> Self {
        Self {
            codes: vec![],
            service_no,
            active_window: WindowBits::ZERO,
            hidden_window: WindowBits::ONE,
        }
    }

    pub fn take_service(&mut self, available_bytes: usize) -> Option<Service> {
        if self.codes.is_empty() {
            return None;
        }

        gst::trace!(CAT, "New service block {}", self.service_no);
        let mut service = Service::new(self.service_no);
        let mut i = 0;
        for code in self.codes.iter() {
            if code.byte_len() > service.free_space() {
                gst::trace!(CAT, "service is full");
                break;
            }
            if service.len() + code.byte_len() > available_bytes {
                gst::trace!(CAT, "packet is full");
                break;
            }
            gst::trace!(CAT, "Adding code {code:?} to service");
            match service.push_code(code) {
                Ok(_) => i += 1,
                Err(_) => break,
            }
        }
        if i == 0 {
            return None;
        }
        self.codes = self.codes.split_off(i);
        Some(service)
    }

    pub fn popon_preamble(&mut self) {
        gst::trace!(CAT, "popon_preamble");
        let window = match self.hidden_window {
            // switch up the newly defined window
            WindowBits::ZERO => 0,
            WindowBits::ONE => 1,
            _ => unreachable!(),
        };
        let args = DefineWindowArgs::new(
            window,
            0,
            Anchor::BottomMiddle,
            false,
            70,
            105,
            14,
            31,
            true,
            true,
            false,
            1,
            1,
        );
        gst::trace!(CAT, "active window {:?}", self.active_window);
        let codes = [
            Code::DeleteWindows(!self.active_window),
            Code::DefineWindow(args),
        ];
        self.push_codes(&codes)
    }

    pub fn clear_current_window(&mut self) {
        gst::trace!(CAT, "clear_current_window {:?}", self.active_window);
        self.push_codes(&[Code::ClearWindows(self.active_window)])
    }

    pub fn clear_hidden_window(&mut self) {
        gst::trace!(CAT, "clear_hidden_window");
        self.push_codes(&[Code::ClearWindows(self.hidden_window)])
    }

    pub fn end_of_caption(&mut self) {
        gst::trace!(CAT, "end_of_caption");
        self.push_codes(&[Code::ToggleWindows(self.active_window | self.hidden_window)]);
        std::mem::swap(&mut self.active_window, &mut self.hidden_window);
        gst::trace!(CAT, "active window {:?}", self.active_window);
    }

    pub fn paint_on_preamble(&mut self) {
        gst::trace!(CAT, "paint_on_preamble");
        let window = match self.active_window {
            WindowBits::ZERO => 0,
            WindowBits::ONE => 1,
            _ => unreachable!(),
        };
        self.push_codes(&[
            // FIXME: assumes positioning in a 16:9 ratio
            Code::DefineWindow(DefineWindowArgs::new(
                window,
                0,
                Anchor::BottomMiddle,
                false,
                70,
                105,
                14,
                31,
                true,
                true,
                true,
                1,
                1,
            )),
        ])
    }

    pub fn rollup_preamble(&mut self, rollup_count: u8, base_row: u8) {
        let base_row = std::cmp::max(rollup_count, base_row);
        let anchor_vertical = (base_row as u32 * 100 / 14) as u8;
        gst::trace!(
            CAT,
            "rollup_preamble base {base_row} count {rollup_count}, anchor-v {anchor_vertical}"
        );
        let codes = [
            Code::DeleteWindows(!WindowBits::ZERO),
            Code::DefineWindow(DefineWindowArgs::new(
                0,
                0,
                Anchor::BottomMiddle,
                true,
                anchor_vertical,
                50,
                rollup_count - 1,
                31,
                true,
                true,
                true,
                1,
                1,
            )),
            Code::SetPenLocation(SetPenLocationArgs::new(rollup_count - 1, 0)),
        ];
        self.active_window = WindowBits::ZERO;
        self.hidden_window = WindowBits::ONE;
        self.push_codes(&codes)
    }

    pub fn write_char(&mut self, c: char) {
        if let Some(code) = Code::from_char(c) {
            self.push_codes(&[code])
        }
    }

    pub fn push_codes(&mut self, codes: &[Code]) {
        gst::log!(CAT, "pushing codes: {codes:?}");
        self.codes.extend(codes.iter().cloned());
    }

    pub fn etx(&mut self) {
        self.push_codes(&[Code::ETX])
    }

    pub fn carriage_return(&mut self) {
        self.push_codes(&[Code::CR])
    }

    pub fn set_pen_attributes(&mut self, args: SetPenAttributesArgs) {
        self.push_codes(&[Code::SetPenAttributes(args)])
    }

    pub fn set_pen_location(&mut self, args: SetPenLocationArgs) {
        self.push_codes(&[Code::SetPenLocation(args)])
    }

    pub fn set_pen_color(&mut self, args: SetPenColorArgs) {
        self.push_codes(&[Code::SetPenColor(args)])
    }
}
