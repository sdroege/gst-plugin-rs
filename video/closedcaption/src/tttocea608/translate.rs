// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use once_cell::sync::Lazy;
use std::collections::VecDeque;

use crate::ffi;

use crate::cea608utils::{is_basicna, is_specialna, is_westeu, Cea608Mode, TextStyle};
use crate::ttutils::{Chunk, Lines};

pub const DEFAULT_FPS_N: i32 = 30;
pub const DEFAULT_FPS_D: i32 = 1;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "tttocea608translator",
        gst::DebugColorFlags::empty(),
        Some("TT CEA 608 translator"),
    )
});

static SPACE: Lazy<u16> = Lazy::new(|| eia608_from_utf8_1(&[0x20, 0, 0, 0, 0]));

fn is_punctuation(word: &str) -> bool {
    word == "." || word == "," || word == "?" || word == "!" || word == ";" || word == ":"
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn eia608_from_utf8_1(c: &[u8; 5]) -> u16 {
    assert!(c[4] == 0);
    unsafe { ffi::eia608_from_utf8_1(c.as_ptr() as *const _, 0) }
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

fn peek_word_length(chars: std::iter::Peekable<std::str::Chars>) -> u32 {
    chars.take_while(|c| !c.is_ascii_whitespace()).count() as u32
}

#[derive(Debug)]
pub struct TimedCea608 {
    pub cea608: u16,
    pub frame_no: u64,
}

#[derive(Debug)]
pub struct TextToCea608 {
    // settings
    origin_column: u32,
    framerate: gst::Fraction,
    roll_up_timeout: Option<gst::ClockTime>,
    // state
    output_frames: VecDeque<TimedCea608>,
    erase_display_frame_no: Option<u64>,
    last_frame_no: u64,
    send_roll_up_preamble: bool,
    style: TextStyle,
    underline: bool,
    column: u8,
    mode: Cea608Mode,
}

impl Default for TextToCea608 {
    fn default() -> Self {
        Self {
            origin_column: 0,
            framerate: gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
            roll_up_timeout: None,
            output_frames: VecDeque::new(),
            erase_display_frame_no: None,
            last_frame_no: 0,
            column: 0,
            send_roll_up_preamble: false,
            style: TextStyle::White,
            underline: false,
            mode: Cea608Mode::PopOn,
        }
    }
}

impl TextToCea608 {
    pub fn set_mode(&mut self, mode: Cea608Mode) {
        self.mode = mode;
        if self.mode != Cea608Mode::PopOn {
            self.send_roll_up_preamble = true;
            self.column = self.origin_column as u8;
        }
    }

    pub fn set_framerate(&mut self, framerate: gst::Fraction) {
        self.framerate = framerate;
    }

    pub fn framerate(&self) -> gst::Fraction {
        self.framerate
    }

    pub fn set_roll_up_timeout(&mut self, timeout: Option<gst::ClockTime>) {
        self.roll_up_timeout = timeout;
    }

    pub fn set_origin_column(&mut self, origin_column: u32) {
        self.origin_column = origin_column
    }

    pub fn pop_output(&mut self) -> Option<TimedCea608> {
        self.output_frames.pop_front()
    }

    pub fn last_frame_no(&self) -> u64 {
        self.last_frame_no
    }

    pub fn erase_display_frame_no(&self) -> Option<u64> {
        self.erase_display_frame_no
    }

    pub fn set_column(&mut self, column: u8) {
        self.column = column
    }

    pub fn flush(&mut self) {
        self.erase_display_frame_no = None;
        self.output_frames.clear();
        self.send_roll_up_preamble = true;
    }

    fn check_erase_display(&mut self) -> bool {
        if let Some(erase_display_frame_no) = self.erase_display_frame_no {
            if self.last_frame_no == erase_display_frame_no - 1 {
                self.column = 0;
                self.send_roll_up_preamble = true;
                self.erase_display_memory();
                return true;
            }
        }

        false
    }

    fn cc_data(&mut self, cc_data: u16) {
        self.check_erase_display();

        self.output_frames.push_back(TimedCea608 {
            cea608: cc_data,
            frame_no: self.last_frame_no,
        });

        self.last_frame_no += 1;
    }

    fn pad(&mut self, frame_no: u64) {
        while self.last_frame_no < frame_no {
            if !self.check_erase_display() {
                self.cc_data(0x8080);
            }
        }
    }

    fn resume_caption_loading(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_resume_caption_loading,
        ))
    }

    fn resume_direct_captioning(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_resume_direct_captioning,
        ))
    }

    fn delete_to_end_of_row(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_delete_to_end_of_row,
        ))
    }

    fn roll_up_2(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_roll_up_2,
        ))
    }

    fn roll_up_3(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_roll_up_3,
        ))
    }

    fn roll_up_4(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_roll_up_4,
        ))
    }

    fn carriage_return(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_carriage_return,
        ))
    }

    fn end_of_caption(&mut self) {
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_end_of_caption,
        ))
    }

    fn tab_offset(&mut self, offset: u32) {
        match offset {
            0 => (),
            1 => self.cc_data(eia608_control_command(
                ffi::eia608_control_t_eia608_tab_offset_1,
            )),
            2 => self.cc_data(eia608_control_command(
                ffi::eia608_control_t_eia608_tab_offset_2,
            )),
            3 => self.cc_data(eia608_control_command(
                ffi::eia608_control_t_eia608_tab_offset_3,
            )),
            _ => unreachable!(),
        }
    }

    fn preamble_indent(&mut self, row: i32, col: i32, underline: bool) {
        self.cc_data(eia608_row_column_preamble(row, col, underline))
    }

    fn preamble_style(&mut self, row: i32, style: u32, underline: bool) {
        self.cc_data(eia608_row_style_preamble(row, style, underline))
    }

    fn midrow_change(&mut self, style: u32, underline: bool) {
        self.cc_data(eia608_midrow_change(style, underline))
    }

    fn bna(&mut self, bna1: u16, bna2: u16) {
        self.cc_data(eia608_from_basicna(bna1, bna2))
    }

    fn erase_display_memory(&mut self) {
        self.erase_display_frame_no = None;
        self.cc_data(eia608_control_command(
            ffi::eia608_control_t_eia608_control_erase_display_memory,
        ))
    }

    fn open_chunk(&mut self, chunk: &Chunk, col: u32) -> bool {
        if (chunk.style != self.style || chunk.underline != self.underline) && col < 31 {
            self.midrow_change(chunk.style as u32, chunk.underline);
            self.style = chunk.style;
            self.underline = chunk.underline;
            true
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_line(
        &mut self,
        chunk: &Chunk,
        col: &mut u32,
        row: i32,
        carriage_return: Option<bool>,
    ) -> bool {
        let mut ret = true;

        let do_preamble = match self.mode {
            Cea608Mode::PopOn | Cea608Mode::PaintOn => true,
            Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                if let Some(carriage_return) = carriage_return {
                    if carriage_return {
                        *col = self.origin_column;
                        self.carriage_return();
                        true
                    } else {
                        self.send_roll_up_preamble
                    }
                } else {
                    self.send_roll_up_preamble
                }
            }
        };

        let mut indent = *col / 4;
        let mut offset = *col % 4;

        if do_preamble {
            match self.mode {
                Cea608Mode::RollUp2 => self.roll_up_2(),
                Cea608Mode::RollUp3 => self.roll_up_3(),
                Cea608Mode::RollUp4 => self.roll_up_4(),
                _ => (),
            }

            if chunk.style != TextStyle::White && indent == 0 {
                self.preamble_style(row, chunk.style as u32, chunk.underline);
                self.style = chunk.style;
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

                self.style = TextStyle::White;
                self.preamble_indent(row, (indent * 4) as i32, chunk.underline);
            }

            if self.mode == Cea608Mode::PaintOn {
                self.delete_to_end_of_row();
            }

            self.tab_offset(offset);

            self.underline = chunk.underline;
            self.send_roll_up_preamble = false;
            ret = false;
        } else if *col == self.origin_column {
            ret = false;
        }

        if self.open_chunk(chunk, *col) {
            *col += 1;
            ret = false
        }

        ret
    }

    // XXX: use a range for frame_no?
    pub fn generate(&mut self, frame_no: u64, end_frame_no: u64, lines: Lines) {
        let origin_column = self.origin_column;
        let mut row = 13;
        let (fps_n, fps_d) = (self.framerate.numer() as u64, self.framerate.denom() as u64);

        if self.last_frame_no == 0 {
            gst::debug!(CAT, "Initial skip to frame no {}", frame_no);
            self.last_frame_no = frame_no;
        }

        gst::trace!(
            CAT,
            "generate from frame {frame_no} to {end_frame_no}, erase frame no: {:?}",
            self.erase_display_frame_no
        );

        let frame_no = frame_no.max(self.last_frame_no);
        let end_frame_no = end_frame_no.max(frame_no);

        let mut col = if self.mode == Cea608Mode::PopOn || self.mode == Cea608Mode::PaintOn {
            0
        } else {
            self.column as u32
        };

        self.pad(frame_no);

        let mut cleared = false;
        if let Some(mode) = lines.mode {
            if mode != self.mode {
                /* Always erase the display when going to or from pop-on */
                if self.mode == Cea608Mode::PopOn || mode == Cea608Mode::PopOn {
                    self.erase_display_memory();
                    cleared = true;
                }

                self.mode = mode;
                match self.mode {
                    Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                        self.send_roll_up_preamble = true;
                    }
                    _ => col = origin_column,
                }
            }
        }

        if let Some(clear) = lines.clear {
            if clear && !cleared {
                self.erase_display_memory();
                if self.mode != Cea608Mode::PopOn && self.mode != Cea608Mode::PaintOn {
                    self.send_roll_up_preamble = true;
                }
                col = origin_column;
            }
        }

        if !lines.lines.is_empty() {
            if self.mode == Cea608Mode::PopOn {
                self.resume_caption_loading();
                self.cc_data(erase_non_displayed_memory());
            } else if self.mode == Cea608Mode::PaintOn {
                self.resume_direct_captioning();
            }
        }

        let mut prev_char = 0;

        for line in &lines.lines {
            gst::log!(CAT, "Processing {:?}", line);

            if let Some(line_row) = line.row {
                row = line_row;
            }

            if row > 14 {
                gst::warning!(CAT, "Dropping line after 15th row: {:?}", line);
                continue;
            }

            if let Some(line_column) = line.column {
                if self.mode != Cea608Mode::PopOn && self.mode != Cea608Mode::PaintOn {
                    self.send_roll_up_preamble = true;
                }
                col = line_column;
            } else if self.mode == Cea608Mode::PopOn || self.mode == Cea608Mode::PaintOn {
                col = origin_column;
            }

            for (j, chunk) in line.chunks.iter().enumerate() {
                let mut prepend_space = true;
                if prev_char != 0 {
                    self.cc_data(prev_char);
                    prev_char = 0;
                }

                if j == 0 {
                    prepend_space =
                        self.open_line(chunk, &mut col, row as i32, line.carriage_return);
                } else if self.open_chunk(chunk, col) {
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
                        gst::warning!(CAT, "Not translating UTF8: {}", c);
                        cc_data = *SPACE;
                    }

                    if is_basicna(prev_char) {
                        if is_basicna(cc_data) {
                            self.bna(prev_char, cc_data);
                        } else if is_westeu(cc_data) {
                            // extended characters overwrite the previous character,
                            // so insert a dummy char then write the extended char
                            self.bna(prev_char, *SPACE);
                            self.cc_data(cc_data);
                        } else {
                            self.cc_data(prev_char);
                            self.cc_data(cc_data);
                        }
                        prev_char = 0;
                    } else if is_westeu(cc_data) {
                        // extended characters overwrite the previous character,
                        // so insert a dummy char then write the extended char
                        self.cc_data(*SPACE);
                        self.cc_data(cc_data);
                    } else if is_basicna(cc_data) {
                        prev_char = cc_data;
                    } else {
                        self.cc_data(cc_data);
                    }

                    if is_specialna(cc_data) {
                        // adapted from libcaption's generation code:
                        // specialna are treated as control characters. Duplicated control characters are discarded
                        // So we write a resume after a specialna as a noop control command to break repetition detection
                        match self.mode {
                            Cea608Mode::RollUp2 => self.roll_up_2(),
                            Cea608Mode::RollUp3 => self.roll_up_3(),
                            Cea608Mode::RollUp4 => self.roll_up_4(),
                            Cea608Mode::PopOn => self.resume_caption_loading(),
                            Cea608Mode::PaintOn => self.resume_direct_captioning(),
                        }
                    }

                    col += 1;

                    if self.mode.is_rollup() {
                        /* In roll-up mode, we introduce carriage returns automatically.
                         * Instead of always wrapping once the last column is reached, we
                         * want to look ahead and check whether the following word will fit
                         * on the current row. If it won't, we insert a carriage return,
                         * unless it won't fit on a full row either, in which case it will need
                         * to be broken up.
                         */
                        let next_word_length = if c.is_ascii_whitespace() {
                            peek_word_length(chars.clone())
                        } else {
                            0
                        };

                        if (next_word_length <= 32 - origin_column && col + next_word_length > 31)
                            || col > 31
                        {
                            if prev_char != 0 {
                                self.cc_data(prev_char);
                                prev_char = 0;
                            }

                            self.open_line(chunk, &mut col, row as i32, Some(true));
                        }
                    } else if col > 31 {
                        if chars.peek().is_some() {
                            gst::warning!(CAT, "Dropping characters after 32nd column: {}", c);
                        }
                        break;
                    }
                }
            }

            if self.mode == Cea608Mode::PopOn || self.mode == Cea608Mode::PaintOn {
                if prev_char != 0 {
                    self.cc_data(prev_char);
                    prev_char = 0;
                }
                row += 1;
            }
        }

        if !lines.lines.is_empty() {
            if prev_char != 0 {
                self.cc_data(prev_char);
            }

            if self.mode == Cea608Mode::PopOn {
                /* No need to erase the display at this point, end_of_caption will be equivalent */
                self.erase_display_frame_no = None;
                self.end_of_caption();
            }
        }

        self.column = col as u8;

        if self.mode == Cea608Mode::PopOn {
            self.erase_display_frame_no =
                Some(self.last_frame_no + end_frame_no.saturating_sub(frame_no));
        } else if let Some(timeout) = self.roll_up_timeout {
            self.erase_display_frame_no =
                Some(self.last_frame_no + timeout.mul_div_round(fps_n, fps_d).unwrap().seconds());
        }

        self.pad(end_frame_no);
    }
}
