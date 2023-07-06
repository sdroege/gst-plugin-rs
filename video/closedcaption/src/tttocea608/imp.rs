// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst::glib::once_cell::sync::Lazy;

use crate::ffi;
use std::sync::Mutex;

use crate::cea608utils::{Cea608Mode, TextStyle};
use crate::ttutils::{Chunk, Line, Lines};

fn is_punctuation(word: &str) -> bool {
    word == "." || word == "," || word == "?" || word == "!" || word == ";" || word == ":"
}

fn is_basicna(cc_data: u16) -> bool {
    0x0000 != (0x6000 & cc_data)
}

fn is_westeu(cc_data: u16) -> bool {
    0x1220 == (0x7660 & cc_data)
}

fn is_specialna(cc_data: u16) -> bool {
    0x1130 == (0x7770 & cc_data)
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn eia608_from_utf8_1(c: &[u8; 5]) -> u16 {
    assert!(c[4] == 0);
    unsafe { ffi::eia608_from_utf8_1(c.as_ptr() as *const _, 0) }
}

fn eia608_to_text(cc_data: u16) -> String {
    unsafe {
        let bufsz = ffi::eia608_to_text(std::ptr::null_mut(), 0, cc_data);
        let mut data = Vec::with_capacity((bufsz + 1) as usize);
        ffi::eia608_to_text(data.as_ptr() as *mut _, (bufsz + 1) as usize, cc_data);
        data.set_len(bufsz as usize);
        String::from_utf8_unchecked(data)
    }
}

fn eia608_row_column_preamble(row: i32, col: i32, underline: bool) -> u16 {
    unsafe {
        /* Hardcoded chan */
        ffi::eia608_row_column_pramble(row, col, 0, underline as i32)
    }
}

fn eia608_row_style_preamble(row: i32, style: u32, underline: bool) -> u16 {
    unsafe {
        /* Hardcoded chan */
        ffi::eia608_row_style_pramble(row, 0, style, underline as i32)
    }
}

fn eia608_midrow_change(style: u32, underline: bool) -> u16 {
    unsafe {
        /* Hardcoded chan and underline */
        ffi::eia608_midrow_change(0, style, underline as i32)
    }
}

fn eia608_control_command(cmd: ffi::eia608_control_t) -> u16 {
    unsafe { ffi::eia608_control_command(cmd, 0) }
}

fn eia608_from_basicna(bna1: u16, bna2: u16) -> u16 {
    unsafe { ffi::eia608_from_basicna(bna1, bna2) }
}

fn erase_non_displayed_memory() -> u16 {
    eia608_control_command(ffi::eia608_control_t_eia608_control_erase_non_displayed_memory)
}

const DEFAULT_FPS_N: i32 = 30;
const DEFAULT_FPS_D: i32 = 1;

const DEFAULT_MODE: Cea608Mode = Cea608Mode::RollUp2;
const DEFAULT_ORIGIN_ROW: i32 = -1;
const DEFAULT_ORIGIN_COLUMN: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    mode: Cea608Mode,
    origin_row: i32,
    origin_column: u32,
    roll_up_timeout: Option<gst::ClockTime>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: DEFAULT_MODE,
            origin_row: DEFAULT_ORIGIN_ROW,
            origin_column: DEFAULT_ORIGIN_COLUMN,
            roll_up_timeout: gst::ClockTime::NONE,
        }
    }
}

struct State {
    framerate: gst::Fraction,
    erase_display_frame_no: Option<u64>,
    last_frame_no: u64,
    max_frame_no: u64,
    send_roll_up_preamble: bool,
    json_input: bool,
    style: TextStyle,
    underline: bool,
    column: u32,
    mode: Cea608Mode,
    force_clear: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            framerate: gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
            erase_display_frame_no: None,
            last_frame_no: 0,
            max_frame_no: 0,
            column: 0,
            send_roll_up_preamble: false,
            json_input: false,
            style: TextStyle::White,
            underline: false,
            mode: Cea608Mode::PopOn,
            force_clear: false,
        }
    }
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "tttocea608",
        gst::DebugColorFlags::empty(),
        Some("TT CEA 608 Element"),
    )
});

static SPACE: Lazy<u16> = Lazy::new(|| eia608_from_utf8_1(&[0x20, 0, 0, 0, 0]));

fn cc_data_buffer(
    imp: &TtToCea608,
    cc_data: u16,
    pts: gst::ClockTime,
    duration: gst::ClockTime,
) -> gst::Buffer {
    let mut ret = gst::Buffer::with_size(2).unwrap();
    let buf_mut = ret.get_mut().unwrap();
    let data = cc_data.to_be_bytes();

    if cc_data != 0x8080 {
        gst::log!(
            CAT,
            imp: imp,
            "{} -> {}: {}",
            pts,
            pts + duration,
            eia608_to_text(cc_data)
        );
    } else {
        gst::trace!(CAT, imp: imp, "{} -> {}: padding", pts, pts + duration);
    }

    buf_mut.copy_from_slice(0, &data).unwrap();
    buf_mut.set_pts(pts);
    buf_mut.set_duration(duration);

    ret
}

impl State {
    fn check_erase_display(
        &mut self,
        imp: &TtToCea608,
        bufferlist: &mut gst::BufferListRef,
    ) -> bool {
        if let Some(erase_display_frame_no) = self.erase_display_frame_no {
            if self.last_frame_no == erase_display_frame_no - 1 {
                self.erase_display_frame_no = None;
                self.column = 0;
                self.send_roll_up_preamble = true;
                self.erase_display_memory(imp, bufferlist);
                return true;
            }
        }

        false
    }

    fn cc_data(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef, cc_data: u16) {
        self.check_erase_display(imp, bufferlist);

        let (fps_n, fps_d) = (self.framerate.numer() as u64, self.framerate.denom() as u64);

        let pts = self
            .last_frame_no
            .seconds()
            .mul_div_round(fps_d, fps_n)
            .unwrap();

        if self.last_frame_no < self.max_frame_no {
            self.last_frame_no += 1;
        } else {
            gst::debug!(CAT, imp: imp, "More text than bandwidth!");
        }

        let next_pts = self
            .last_frame_no
            .seconds()
            .mul_div_round(fps_d, fps_n)
            .unwrap();

        let duration = next_pts - pts;

        bufferlist.insert(-1, cc_data_buffer(imp, cc_data, pts, duration));
    }

    fn pad(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef, frame_no: u64) {
        while self.last_frame_no < frame_no {
            if !self.check_erase_display(imp, bufferlist) {
                self.cc_data(imp, bufferlist, 0x8080);
            }
        }
    }

    fn resume_caption_loading(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_resume_caption_loading),
        )
    }

    fn resume_direct_captioning(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_resume_direct_captioning),
        )
    }

    fn delete_to_end_of_row(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_delete_to_end_of_row),
        )
    }

    fn roll_up_2(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_roll_up_2),
        )
    }

    fn roll_up_3(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_roll_up_3),
        )
    }

    fn roll_up_4(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_roll_up_4),
        )
    }

    fn carriage_return(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_carriage_return),
        )
    }

    fn end_of_caption(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_end_of_caption),
        )
    }

    fn tab_offset(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef, offset: u32) {
        match offset {
            0 => (),
            1 => self.cc_data(
                imp,
                bufferlist,
                eia608_control_command(ffi::eia608_control_t_eia608_tab_offset_1),
            ),
            2 => self.cc_data(
                imp,
                bufferlist,
                eia608_control_command(ffi::eia608_control_t_eia608_tab_offset_2),
            ),
            3 => self.cc_data(
                imp,
                bufferlist,
                eia608_control_command(ffi::eia608_control_t_eia608_tab_offset_3),
            ),
            _ => unreachable!(),
        }
    }

    fn preamble_indent(
        &mut self,
        imp: &TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        row: i32,
        col: i32,
        underline: bool,
    ) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_row_column_preamble(row, col, underline),
        )
    }

    fn preamble_style(
        &mut self,
        imp: &TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        row: i32,
        style: u32,
        underline: bool,
    ) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_row_style_preamble(row, style, underline),
        )
    }

    fn midrow_change(
        &mut self,
        imp: &TtToCea608,
        bufferlist: &mut gst::BufferListRef,
        style: u32,
        underline: bool,
    ) {
        self.cc_data(imp, bufferlist, eia608_midrow_change(style, underline))
    }

    fn bna(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef, bna1: u16, bna2: u16) {
        self.cc_data(imp, bufferlist, eia608_from_basicna(bna1, bna2))
    }

    fn erase_display_memory(&mut self, imp: &TtToCea608, bufferlist: &mut gst::BufferListRef) {
        self.cc_data(
            imp,
            bufferlist,
            eia608_control_command(ffi::eia608_control_t_eia608_control_erase_display_memory),
        )
    }
}

pub struct TtToCea608 {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    // Ordered by locking order
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl TtToCea608 {
    fn open_chunk(
        &self,
        state: &mut State,
        chunk: &Chunk,
        bufferlist: &mut gst::BufferListRef,
        col: u32,
    ) -> bool {
        if (chunk.style != state.style || chunk.underline != state.underline) && col < 31 {
            state.midrow_change(self, bufferlist, chunk.style as u32, chunk.underline);
            state.style = chunk.style;
            state.underline = chunk.underline;
            true
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_line(
        &self,
        state: &mut State,
        settings: &Settings,
        chunk: &Chunk,
        bufferlist: &mut gst::BufferListRef,
        col: &mut u32,
        row: i32,
        carriage_return: Option<bool>,
    ) -> bool {
        let mut ret = true;

        let do_preamble = match state.mode {
            Cea608Mode::PopOn | Cea608Mode::PaintOn => true,
            Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                if let Some(carriage_return) = carriage_return {
                    if carriage_return {
                        *col = settings.origin_column;
                        state.carriage_return(self, bufferlist);
                        true
                    } else {
                        state.send_roll_up_preamble
                    }
                } else {
                    state.send_roll_up_preamble
                }
            }
        };

        let mut indent = *col / 4;
        let mut offset = *col % 4;

        if do_preamble {
            match state.mode {
                Cea608Mode::RollUp2 => state.roll_up_2(self, bufferlist),
                Cea608Mode::RollUp3 => state.roll_up_3(self, bufferlist),
                Cea608Mode::RollUp4 => state.roll_up_4(self, bufferlist),
                _ => (),
            }

            if chunk.style != TextStyle::White && indent == 0 {
                state.preamble_style(self, bufferlist, row, chunk.style as u32, chunk.underline);
                state.style = chunk.style;
            } else {
                if chunk.style != TextStyle::White {
                    if offset > 0 {
                        offset -= 1;
                    } else {
                        indent -= 1;
                        offset = 3;
                    }
                    *col -= 1;
                }

                state.style = TextStyle::White;
                state.preamble_indent(self, bufferlist, row, (indent * 4) as i32, chunk.underline);
            }

            if state.mode == Cea608Mode::PaintOn {
                state.delete_to_end_of_row(self, bufferlist);
            }

            state.tab_offset(self, bufferlist, offset);

            state.underline = chunk.underline;
            state.send_roll_up_preamble = false;
            ret = false;
        } else if *col == settings.origin_column {
            ret = false;
        }

        if self.open_chunk(state, chunk, bufferlist, *col) {
            *col += 1;
            ret = false
        }

        ret
    }

    fn peek_word_length(&self, chars: std::iter::Peekable<std::str::Chars>) -> u32 {
        chars.take_while(|c| !c.is_ascii_whitespace()).count() as u32
    }

    fn generate(
        &self,
        mut state: &mut State,
        settings: &Settings,
        pts: gst::ClockTime,
        duration: gst::ClockTime,
        lines: Lines,
    ) -> Result<gst::BufferList, gst::FlowError> {
        let origin_column = settings.origin_column;
        let mut row = 13;
        let mut bufferlist = gst::BufferList::new();
        let mut_list = bufferlist.get_mut().unwrap();

        let mut col = if state.mode == Cea608Mode::PopOn || state.mode == Cea608Mode::PaintOn {
            0
        } else {
            state.column
        };

        let (fps_n, fps_d) = (
            state.framerate.numer() as u64,
            state.framerate.denom() as u64,
        );

        let frame_no = pts.mul_div_round(fps_n, fps_d).unwrap().seconds();

        if state.last_frame_no == 0 {
            gst::debug!(CAT, imp: self, "Initial skip to frame no {}", frame_no);
            state.last_frame_no = pts.mul_div_floor(fps_n, fps_d).unwrap().seconds();
        }

        state.max_frame_no = (pts + duration)
            .mul_div_round(fps_n, fps_d)
            .unwrap()
            .seconds();

        state.pad(self, mut_list, frame_no);

        let mut cleared = false;
        if let Some(mode) = lines.mode {
            if mode != state.mode {
                /* Always erase the display when going to or from pop-on */
                if state.mode == Cea608Mode::PopOn || mode == Cea608Mode::PopOn {
                    state.erase_display_frame_no = None;
                    state.erase_display_memory(self, mut_list);
                    cleared = true;
                }

                state.mode = mode;
                match state.mode {
                    Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                        state.send_roll_up_preamble = true;
                    }
                    _ => col = origin_column,
                }
            }
        }

        if let Some(clear) = lines.clear {
            if clear && !cleared {
                state.erase_display_frame_no = None;
                state.erase_display_memory(self, mut_list);
                if state.mode != Cea608Mode::PopOn && state.mode != Cea608Mode::PaintOn {
                    state.send_roll_up_preamble = true;
                }
                col = origin_column;
            }
        }

        if state.mode == Cea608Mode::PopOn {
            state.resume_caption_loading(self, mut_list);
            state.cc_data(self, mut_list, erase_non_displayed_memory());
        } else if state.mode == Cea608Mode::PaintOn {
            state.resume_direct_captioning(self, mut_list);
        }

        let mut prev_char = 0;

        for line in &lines.lines {
            gst::log!(CAT, imp: self, "Processing {:?}", line);

            if let Some(line_row) = line.row {
                row = line_row;
            }

            if row > 14 {
                gst::warning!(CAT, imp: self, "Dropping line after 15th row: {:?}", line);
                continue;
            }

            if let Some(line_column) = line.column {
                if state.mode != Cea608Mode::PopOn && state.mode != Cea608Mode::PaintOn {
                    state.send_roll_up_preamble = true;
                }
                col = line_column;
            } else if state.mode == Cea608Mode::PopOn || state.mode == Cea608Mode::PaintOn {
                col = origin_column;
            }

            for (j, chunk) in line.chunks.iter().enumerate() {
                let mut prepend_space = true;
                if prev_char != 0 {
                    state.cc_data(self, mut_list, prev_char);
                    prev_char = 0;
                }

                if j == 0 {
                    prepend_space = self.open_line(
                        state,
                        settings,
                        chunk,
                        mut_list,
                        &mut col,
                        row as i32,
                        line.carriage_return,
                    );
                } else if self.open_chunk(state, chunk, mut_list, col) {
                    prepend_space = false;
                    col += 1;
                }

                if is_punctuation(&chunk.text) {
                    prepend_space = false;
                }

                let text = {
                    if prepend_space {
                        let mut text = " ".to_string();
                        text.push_str(&chunk.text);
                        text
                    } else {
                        chunk.text.clone()
                    }
                };

                let mut chars = text.chars().peekable();

                while let Some(c) = chars.next() {
                    if c == '\r' {
                        continue;
                    }

                    let mut encoded = [0; 5];
                    c.encode_utf8(&mut encoded);
                    let mut cc_data = eia608_from_utf8_1(&encoded);

                    if cc_data == 0 {
                        gst::warning!(CAT, imp: self, "Not translating UTF8: {}", c);
                        cc_data = *SPACE;
                    }

                    if is_basicna(prev_char) {
                        if is_basicna(cc_data) {
                            state.bna(self, mut_list, prev_char, cc_data);
                        } else if is_westeu(cc_data) {
                            // extended characters overwrite the previous character,
                            // so insert a dummy char then write the extended char
                            state.bna(self, mut_list, prev_char, *SPACE);
                            state.cc_data(self, mut_list, cc_data);
                        } else {
                            state.cc_data(self, mut_list, prev_char);
                            state.cc_data(self, mut_list, cc_data);
                        }
                        prev_char = 0;
                    } else if is_westeu(cc_data) {
                        // extended characters overwrite the previous character,
                        // so insert a dummy char then write the extended char
                        state.cc_data(self, mut_list, *SPACE);
                        state.cc_data(self, mut_list, cc_data);
                    } else if is_basicna(cc_data) {
                        prev_char = cc_data;
                    } else {
                        state.cc_data(self, mut_list, cc_data);
                    }

                    if is_specialna(cc_data) {
                        // adapted from libcaption's generation code:
                        // specialna are treated as control characters. Duplicated control characters are discarded
                        // So we write a resume after a specialna as a noop control command to break repetition detection
                        match state.mode {
                            Cea608Mode::RollUp2 => state.roll_up_2(self, mut_list),
                            Cea608Mode::RollUp3 => state.roll_up_3(self, mut_list),
                            Cea608Mode::RollUp4 => state.roll_up_4(self, mut_list),
                            Cea608Mode::PopOn => state.resume_caption_loading(self, mut_list),
                            Cea608Mode::PaintOn => state.resume_direct_captioning(self, mut_list),
                        }
                    }

                    col += 1;

                    if state.mode.is_rollup() {
                        /* In roll-up mode, we introduce carriage returns automatically.
                         * Instead of always wrapping once the last column is reached, we
                         * want to look ahead and check whether the following word will fit
                         * on the current row. If it won't, we insert a carriage return,
                         * unless it won't fit on a full row either, in which case it will need
                         * to be broken up.
                         */
                        let next_word_length = if c.is_ascii_whitespace() {
                            self.peek_word_length(chars.clone())
                        } else {
                            0
                        };

                        if (next_word_length <= 32 - origin_column && col + next_word_length > 31)
                            || col > 31
                        {
                            if prev_char != 0 {
                                state.cc_data(self, mut_list, prev_char);
                                prev_char = 0;
                            }

                            self.open_line(
                                state,
                                settings,
                                chunk,
                                mut_list,
                                &mut col,
                                row as i32,
                                Some(true),
                            );
                        }
                    } else if col > 31 {
                        if chars.peek().is_some() {
                            gst::warning!(
                                CAT,
                                imp: self,
                                "Dropping characters after 32nd column: {}",
                                c
                            );
                        }
                        break;
                    }
                }
            }

            if state.mode == Cea608Mode::PopOn || state.mode == Cea608Mode::PaintOn {
                if prev_char != 0 {
                    state.cc_data(self, mut_list, prev_char);
                    prev_char = 0;
                }
                row += 1;
            }
        }

        if prev_char != 0 {
            state.cc_data(self, mut_list, prev_char);
        }

        if state.mode == Cea608Mode::PopOn {
            /* No need to erase the display at this point, end_of_caption will be equivalent */
            state.erase_display_frame_no = None;
            state.end_of_caption(self, mut_list);
        }

        state.column = col;

        if state.mode == Cea608Mode::PopOn {
            state.erase_display_frame_no =
                Some(state.last_frame_no + duration.mul_div_round(fps_n, fps_d).unwrap().seconds());
        } else if let Some(timeout) = settings.roll_up_timeout {
            state.erase_display_frame_no =
                Some(state.last_frame_no + timeout.mul_div_round(fps_n, fps_d).unwrap().seconds());
        }

        state.pad(self, mut_list, state.max_frame_no);

        Ok(bufferlist)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, imp: self, "Handling {:?}", buffer);

        let pts = buffer.pts().ok_or_else(|| {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["Stream with timestamped buffers required"]
            );
            gst::FlowError::Error
        })?;

        let duration = buffer.duration().ok_or_else(|| {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["Buffers of stream need to have a duration"]
            );
            gst::FlowError::Error
        })?;

        let data = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, obj: pad, "Can't map buffer readable");

            gst::FlowError::Error
        })?;

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let mut lines = Lines {
            lines: Vec::new(),
            mode: Some(settings.mode),
            clear: Some(state.force_clear),
        };
        state.force_clear = false;
        match state.json_input {
            false => {
                let data = std::str::from_utf8(&data).map_err(|err| {
                    gst::error!(CAT, obj: pad, "Can't decode utf8: {}", err);

                    gst::FlowError::Error
                })?;

                let phrases: Vec<&str> = data.split('\n').collect();
                let mut row = match settings.origin_row {
                    -1 => match settings.mode {
                        Cea608Mode::PopOn | Cea608Mode::PaintOn => {
                            15u32.saturating_sub(phrases.len() as u32)
                        }
                        Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => 14,
                    },
                    _ => settings.origin_row as u32,
                };

                for phrase in &phrases {
                    lines.lines.push(Line {
                        carriage_return: None,
                        column: None,
                        row: Some(row),
                        chunks: vec![Chunk {
                            style: TextStyle::White,
                            underline: false,
                            text: phrase.to_string(),
                        }],
                    });
                    if settings.mode == Cea608Mode::PopOn || settings.mode == Cea608Mode::PaintOn {
                        row += 1;
                    }
                }
            }
            true => {
                lines = serde_json::from_slice(&data).map_err(|err| {
                    gst::error!(CAT, obj: pad, "Failed to parse input as json: {}", err);

                    gst::FlowError::Error
                })?;
            }
        }

        let bufferlist = self.generate(&mut state, &settings, pts, duration, lines)?;

        drop(settings);
        drop(state);

        self.srcpad.push_list(bufferlist)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {:?}", event);

        use gst::EventView;

        match event.view() {
            EventView::Caps(e) => {
                let mut downstream_caps = match self.srcpad.allowed_caps() {
                    None => self.srcpad.pad_template_caps(),
                    Some(caps) => caps,
                };

                if downstream_caps.is_empty() {
                    gst::error!(CAT, obj: pad, "Empty downstream caps");
                    return false;
                }

                let caps = downstream_caps.make_mut();
                let s = caps.structure_mut(0).unwrap();

                s.fixate_field_nearest_fraction(
                    "framerate",
                    gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
                );
                s.fixate();

                let caps = gst::Caps::builder_full().structure(s.to_owned()).build();

                let mut state = self.state.lock().unwrap();
                state.framerate = s.get::<gst::Fraction>("framerate").unwrap();

                let upstream_caps = e.caps();
                let s = upstream_caps.structure(0).unwrap();
                state.json_input = s.name() == "application/x-json";

                gst::debug!(CAT, obj: pad, "Pushing caps {}", caps);

                let new_event = gst::event::Caps::new(&caps);

                drop(state);

                self.srcpad.push_event(new_event)
            }
            EventView::Gap(e) => {
                let mut state = self.state.lock().unwrap();

                let (fps_n, fps_d) = (
                    state.framerate.numer() as u64,
                    state.framerate.denom() as u64,
                );

                let (timestamp, duration) = e.get();

                if state.last_frame_no == 0 {
                    state.last_frame_no = timestamp.mul_div_floor(fps_n, fps_d).unwrap().seconds();

                    gst::debug!(
                        CAT,
                        imp: self,
                        "Initial skip to frame no {}",
                        state.last_frame_no
                    );
                }

                let frame_no = (timestamp + duration.unwrap_or(gst::ClockTime::ZERO))
                    .mul_div_round(fps_n, fps_d)
                    .unwrap()
                    .seconds();
                state.max_frame_no = frame_no;

                let mut bufferlist = gst::BufferList::new();
                let mut_list = bufferlist.get_mut().unwrap();

                state.pad(self, mut_list, frame_no);

                drop(state);

                let _ = self.srcpad.push_list(bufferlist);

                true
            }
            EventView::Eos(_) => {
                let mut state = self.state.lock().unwrap();
                if let Some(erase_display_frame_no) = state.erase_display_frame_no {
                    let mut bufferlist = gst::BufferList::new();
                    let mut_list = bufferlist.get_mut().unwrap();

                    state.max_frame_no = erase_display_frame_no;
                    state.pad(self, mut_list, erase_display_frame_no);

                    drop(state);

                    let _ = self.srcpad.push_list(bufferlist);
                } else {
                    drop(state);
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                let settings = self.settings.lock().unwrap();

                *state = State::default();

                state.mode = settings.mode;

                if state.mode != Cea608Mode::PopOn {
                    state.send_roll_up_preamble = true;
                }

                drop(settings);
                drop(state);

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TtToCea608 {
    const NAME: &'static str = "GstTtToCea608";
    type Type = super::TtToCea608;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                TtToCea608::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this| this.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TtToCea608::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| this.sink_event(pad, event),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for TtToCea608 {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecEnum::builder_with_default("mode", DEFAULT_MODE)
                    .nick("Mode")
                    .blurb("Which mode to operate in")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecInt::builder("origin-row")
                    .nick("Origin row")
                    .blurb("Origin row, (-1=automatic)")
                    .minimum(-1)
                    .maximum(14)
                    .default_value(DEFAULT_ORIGIN_ROW)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("origin-column")
                    .nick("Origin column")
                    .blurb("Origin column")
                    .maximum(31)
                    .default_value(DEFAULT_ORIGIN_COLUMN)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder("roll-up-timeout")
                    .nick("Roll-Up Timeout")
                    .blurb("Duration after which to erase display memory in roll-up mode")
                    .default_value(u64::MAX)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "mode" => {
                let mut state = self.state.lock().unwrap();
                let mut settings = self.settings.lock().unwrap();
                settings.mode = value.get::<Cea608Mode>().expect("type checked upstream");
                state.force_clear = true;
            }
            "origin-row" => {
                let mut state = self.state.lock().unwrap();
                let mut settings = self.settings.lock().unwrap();
                settings.origin_row = value.get().expect("type checked upstream");
                state.force_clear = true;
            }
            "origin-column" => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                settings.origin_column = value.get().expect("type checked upstream");
                state.force_clear = true;
                state.column = settings.origin_column;
            }
            "roll-up-timeout" => {
                let mut settings = self.settings.lock().unwrap();

                let timeout = value.get().expect("type checked upstream");

                settings.roll_up_timeout = match timeout {
                    u64::MAX => gst::ClockTime::NONE,
                    _ => Some(timeout.nseconds()),
                };
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "mode" => {
                let settings = self.settings.lock().unwrap();
                settings.mode.to_value()
            }
            "origin-row" => {
                let settings = self.settings.lock().unwrap();
                settings.origin_row.to_value()
            }
            "origin-column" => {
                let settings = self.settings.lock().unwrap();
                settings.origin_column.to_value()
            }
            "roll-up-timeout" => {
                let settings = self.settings.lock().unwrap();

                if let Some(timeout) = settings.roll_up_timeout {
                    timeout.nseconds().to_value()
                } else {
                    u64::MAX.to_value()
                }
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for TtToCea608 {}

impl ElementImpl for TtToCea608 {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "TT to CEA-608",
                "Generic",
                "Converts timed text to CEA-608 Closed Captions",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                let s = gst::Structure::builder("text/x-raw").build();
                caps.append_structure(s);

                let s = gst::Structure::builder("application/x-json")
                    .field("format", "cea608")
                    .build();
                caps.append_structure(s);
            }

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let framerate = gst::FractionRange::new(
                gst::Fraction::new(1, std::i32::MAX),
                gst::Fraction::new(std::i32::MAX, 1),
            );

            let caps = gst::Caps::builder("closedcaption/x-cea-608")
                .field("format", "raw")
                .field("framerate", framerate)
                .field("field", 0)
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp: self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();
                let settings = self.settings.lock().unwrap();
                *state = State::default();
                state.force_clear = false;
                state.mode = settings.mode;
                if state.mode != Cea608Mode::PopOn {
                    state.send_roll_up_preamble = true;
                    state.column = settings.origin_column;
                }
            }
            _ => (),
        }

        let ret = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        Ok(ret)
    }
}
