// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use cea708_types::{tables::*, Service};

use gst::glib::once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cea708utils",
        gst::DebugColorFlags::empty(),
        Some("CEA-708 Utilities"),
    )
});

#[derive(Debug)]
pub enum WriteError {
    // returns the number of characters/bytes written
    WouldOverflow(usize),
}

pub(crate) struct Cea708ServiceWriter {
    service: Option<Service>,
    service_no: u8,
    active_window: WindowBits,
    hidden_window: WindowBits,
}

impl Cea708ServiceWriter {
    pub fn new(service_no: u8) -> Self {
        Self {
            service: None,
            service_no,
            active_window: WindowBits::ZERO,
            hidden_window: WindowBits::ONE,
        }
    }

    fn ensure_service(&mut self) {
        if self.service.is_none() {
            self.service = Some(Service::new(self.service_no));
        }
    }

    pub fn take_service(&mut self) -> Option<Service> {
        self.service.take()
    }

    pub fn popon_preamble(&mut self) -> Result<usize, WriteError> {
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

    pub fn clear_current_window(&mut self) -> Result<usize, WriteError> {
        gst::trace!(CAT, "clear_current_window {:?}", self.active_window);
        self.push_codes(&[Code::ClearWindows(self.active_window)])
    }

    pub fn clear_hidden_window(&mut self) -> Result<usize, WriteError> {
        gst::trace!(CAT, "clear_hidden_window");
        self.push_codes(&[Code::ClearWindows(self.hidden_window)])
    }

    pub fn end_of_caption(&mut self) -> Result<usize, WriteError> {
        gst::trace!(CAT, "end_of_caption");
        let ret =
            self.push_codes(&[Code::ToggleWindows(self.active_window | self.hidden_window)])?;
        std::mem::swap(&mut self.active_window, &mut self.hidden_window);
        gst::trace!(CAT, "active window {:?}", self.active_window);
        Ok(ret)
    }

    pub fn paint_on_preamble(&mut self) -> Result<usize, WriteError> {
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

    pub fn rollup_preamble(&mut self, rollup_count: u8, base_row: u8) -> Result<usize, WriteError> {
        let base_row = std::cmp::max(rollup_count, base_row);
        let anchor_vertical = (base_row as u32 * 100 / 15) as u8;
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

    pub fn write_char(&mut self, c: char) -> Result<usize, WriteError> {
        if let Some(code) = Code::from_char(c) {
            self.push_codes(&[code])
        } else {
            Ok(0)
        }
    }

    pub fn push_codes(&mut self, codes: &[Code]) -> Result<usize, WriteError> {
        self.ensure_service();
        let service = self.service.as_mut().unwrap();
        let start_len = service.len();
        if service.free_space() < codes.iter().map(|c| c.byte_len()).sum::<usize>() {
            return Err(WriteError::WouldOverflow(0));
        }
        for code in codes.iter() {
            gst::trace!(
                CAT,
                "pushing for service:{} code: {code:?}",
                service.number()
            );
            service.push_code(code).unwrap();
        }
        Ok(service.len() - start_len)
    }

    pub fn etx(&mut self) -> Result<usize, WriteError> {
        self.push_codes(&[Code::ETX])
    }

    pub fn carriage_return(&mut self) -> Result<usize, WriteError> {
        self.push_codes(&[Code::CR])
    }

    pub fn set_pen_attributes(&mut self, args: SetPenAttributesArgs) -> Result<usize, WriteError> {
        self.push_codes(&[Code::SetPenAttributes(args)])
    }

    pub fn set_pen_location(&mut self, args: SetPenLocationArgs) -> Result<usize, WriteError> {
        self.push_codes(&[Code::SetPenLocation(args)])
    }

    pub fn set_pen_color(&mut self, args: SetPenColorArgs) -> Result<usize, WriteError> {
        self.push_codes(&[Code::SetPenColor(args)])
    }
}
