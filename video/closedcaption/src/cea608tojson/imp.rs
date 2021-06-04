// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// TODO:
//
//  * For now the element completely discards data for channel 1,
//    as it has never really been used. Ideally it should handle it,
//    not critical.
//
//  * A few control commands aren't supported, see TODO in
//    decode_control. The only notable command is delete_to_end_of_row,
//    probably hasn't seen wide usage though :)
//
//  * By design the element outputs text only once, leaving one corner case
//    not covered: fill both the display and the non-displayed memory in
//    pop-on mode, then send multiple End Of Caption commands: the expected
//    result is that the text flips back and forth. This is probably never
//    used in practice, and difficult to represent with our output format.
//
//  * The Chunk object could have an "indent" field, that would get translated
//    to tab offsets for small bandwidth savings

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace, gst_warning};

use crate::ffi;
use crate::ttutils::{Cea608Mode, Chunk, Line, Lines, TextStyle};

use atomic_refcell::AtomicRefCell;

use once_cell::sync::Lazy;

use std::collections::BTreeMap;

#[derive(Debug)]
struct TimestampedLines {
    lines: Lines,
    pts: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
}

struct Cursor {
    row: u32,
    col: usize,
    style: TextStyle,
    underline: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Cell {
    Char {
        c: char,
        style: TextStyle,
        underline: bool,
    },
    Empty,
}

#[derive(Clone)]
struct Row {
    cells: Vec<Cell>,
    row: u32,
}

impl Row {
    fn new(row: u32) -> Self {
        Self {
            cells: vec![Cell::Empty; 32],
            row,
        }
    }

    fn push(&mut self, cursor: &mut Cursor, c: char) {
        self.cells[cursor.col] = Cell::Char {
            c,
            style: cursor.style,
            underline: cursor.underline,
        };

        // https://www.law.cornell.edu/cfr/text/47/79.101 (f)(1)(v)
        if cursor.col < 31 {
            cursor.col += 1;
        }
    }

    fn push_midrow(&mut self, cursor: &mut Cursor, style: TextStyle, underline: bool) {
        self.cells[cursor.col] = Cell::Empty;
        cursor.style = style;
        cursor.underline = underline;

        // https://www.law.cornell.edu/cfr/text/47/79.101 (f)(1)(v)
        if cursor.col < 31 {
            cursor.col += 1;
        }
    }

    fn pop(&mut self, cursor: &mut Cursor) {
        if cursor.col > 0 {
            self.cells[cursor.col] = Cell::Empty;
            cursor.col -= 1;
        }
    }

    fn is_empty(&self) -> bool {
        self.cells.iter().all(|c| *c == Cell::Empty)
    }
}

impl From<Row> for Line {
    fn from(row: Row) -> Self {
        let mut chunks = vec![];
        let mut current_chunk: Option<Chunk> = None;
        let mut indent = 0;

        let mut trailing = 0;

        for sc in row.cells {
            match sc {
                Cell::Char {
                    c,
                    style,
                    underline,
                } => {
                    if let Some(mut chunk) = current_chunk.take() {
                        current_chunk = {
                            if style != chunk.style || underline != chunk.underline || trailing > 0
                            {
                                let mut text = " ".repeat(trailing);
                                trailing = 0;
                                text.push(c);

                                chunks.push(chunk);
                                Some(Chunk {
                                    style,
                                    underline,
                                    text,
                                })
                            } else {
                                chunk.text.push(c);
                                Some(chunk)
                            }
                        }
                    } else {
                        current_chunk = Some(Chunk {
                            style,
                            underline,
                            text: c.into(),
                        });
                    }
                }
                Cell::Empty => {
                    if current_chunk.is_none() {
                        indent += 1;
                    } else {
                        trailing += 1;
                    }
                }
            }
        }

        if let Some(chunk) = current_chunk.take() {
            chunks.push(chunk);
        }

        Line {
            column: Some(indent),
            row: Some(row.row),
            chunks,
            carriage_return: None,
        }
    }
}

struct State {
    mode: Option<Cea608Mode>,
    last_cc_data: Option<u16>,
    rows: BTreeMap<u32, Row>,
    first_pts: Option<gst::ClockTime>,
    current_pts: Option<gst::ClockTime>,
    current_duration: Option<gst::ClockTime>,
    carriage_return: Option<bool>,
    clear: Option<bool>,
    cursor: Cursor,
    pending_lines: Option<TimestampedLines>,
}

impl Default for State {
    fn default() -> Self {
        State {
            mode: None,
            last_cc_data: None,
            rows: BTreeMap::new(),
            first_pts: gst::ClockTime::NONE,
            current_pts: gst::ClockTime::NONE,
            current_duration: gst::ClockTime::NONE,
            carriage_return: None,
            clear: None,
            cursor: Cursor {
                row: 14,
                col: 0,
                style: TextStyle::White,
                underline: false,
            },
            pending_lines: None,
        }
    }
}

pub struct Cea608ToJson {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    state: AtomicRefCell<State>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cea608tojson",
        gst::DebugColorFlags::empty(),
        Some("CEA-608 to JSON Element"),
    )
});

fn is_basicna(cc_data: u16) -> bool {
    0x0000 != (0x6000 & cc_data)
}

fn is_preamble(cc_data: u16) -> bool {
    0x1040 == (0x7040 & cc_data)
}

fn is_midrowchange(cc_data: u16) -> bool {
    0x1120 == (0x7770 & cc_data)
}

fn is_specialna(cc_data: u16) -> bool {
    0x1130 == (0x7770 & cc_data)
}

fn is_xds(cc_data: u16) -> bool {
    0x0000 == (0x7070 & cc_data) && 0x0000 != (0x0F0F & cc_data)
}

fn is_westeu(cc_data: u16) -> bool {
    0x1220 == (0x7660 & cc_data)
}

fn is_control(cc_data: u16) -> bool {
    0x1420 == (0x7670 & cc_data) || 0x1720 == (0x7770 & cc_data)
}

fn parse_control(cc_data: u16) -> (ffi::eia608_control_t, i32) {
    unsafe {
        let mut chan = 0;
        let cmd = ffi::eia608_parse_control(cc_data, &mut chan);

        (cmd, chan)
    }
}

#[derive(Debug)]
struct Preamble {
    row: i32,
    col: i32,
    style: TextStyle,
    chan: i32,
    underline: i32,
}

fn parse_preamble(cc_data: u16) -> Preamble {
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

struct MidrowChange {
    chan: i32,
    style: TextStyle,
    underline: bool,
}

fn parse_midrowchange(cc_data: u16) -> MidrowChange {
    unsafe {
        let mut chan = 0;
        let mut style = 0;
        let mut underline = 0;

        ffi::eia608_parse_midrowchange(cc_data, &mut chan, &mut style, &mut underline);

        MidrowChange {
            chan,
            style: style.into(),
            underline: underline > 0,
        }
    }
}

fn eia608_to_utf8(cc_data: u16) -> (Option<char>, Option<char>, i32) {
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

fn eia608_to_text(cc_data: u16) -> String {
    unsafe {
        let bufsz = ffi::eia608_to_text(std::ptr::null_mut(), 0, cc_data);
        let mut data = Vec::with_capacity((bufsz + 1) as usize);
        data.set_len(bufsz as usize);
        ffi::eia608_to_text(data.as_ptr() as *mut _, (bufsz + 1) as usize, cc_data);
        String::from_utf8_unchecked(data)
    }
}

fn dump(
    element: &super::Cea608ToJson,
    cc_data: u16,
    pts: impl Into<Option<gst::ClockTime>>,
    duration: impl Into<Option<gst::ClockTime>>,
) {
    let pts = pts.into();
    let end = pts
        .zip(duration.into())
        .map(|(pts, duration)| pts + duration);

    if cc_data != 0x8080 {
        gst_debug!(
            CAT,
            obj: element,
            "{} -> {}: {}",
            pts.display(),
            end.display(),
            eia608_to_text(cc_data)
        );
    } else {
        gst_trace!(
            CAT,
            obj: element,
            "{} -> {}: padding",
            pts.display(),
            end.display()
        );
    }
}

impl State {
    fn update_mode(
        &mut self,
        element: &super::Cea608ToJson,
        mode: Cea608Mode,
    ) -> Option<TimestampedLines> {
        if mode.is_rollup() && self.mode == Some(Cea608Mode::PopOn) {
            // https://www.law.cornell.edu/cfr/text/47/79.101 (f)(2)(v)
            let _ = self.drain(element);
        }

        let ret = if Some(mode) != self.mode {
            if self.mode == Some(Cea608Mode::PopOn) {
                self.drain_pending(element)
            } else {
                self.drain(element)
            }
        } else {
            None
        };

        if mode.is_rollup()
            && (self.mode == Some(Cea608Mode::PopOn)
                || self.mode == Some(Cea608Mode::PaintOn)
                || self.mode.is_none())
        {
            // https://www.law.cornell.edu/cfr/text/47/79.101 (f)(1)(ii)
            self.cursor.row = 14;
            self.cursor.col = 0;

            self.rows.insert(self.cursor.row, Row::new(self.cursor.row));
        }

        self.mode = Some(mode);

        ret
    }

    fn drain(&mut self, element: &super::Cea608ToJson) -> Option<TimestampedLines> {
        gst_log!(CAT, obj: element, "Draining");

        let pts = self.first_pts;

        let duration = match self.mode {
            Some(Cea608Mode::PopOn) => gst::ClockTime::NONE,
            _ => self
                .current_pts
                .zip(self.current_duration)
                .map(|(cur_pts, cur_duration)| cur_pts + cur_duration)
                .zip(self.first_pts)
                .and_then(|(cur_end, first_pts)| cur_end.checked_sub(first_pts)),
        };

        self.first_pts = gst::ClockTime::NONE;

        let mut lines: Vec<Line> = vec![];

        // Wish BTreeMap had a drain() method
        for (_idx, row) in std::mem::take(&mut self.rows).into_iter() {
            if !row.is_empty() {
                let mut line: Line = row.into();
                line.carriage_return = self.carriage_return.take();
                lines.push(line);
            }
        }

        self.rows.clear();

        let clear = self.clear.take();

        if !lines.is_empty() {
            Some(TimestampedLines {
                lines: Lines {
                    lines,
                    mode: self.mode,
                    clear,
                },
                pts,
                duration,
            })
        } else if clear == Some(true) {
            Some(TimestampedLines {
                lines: Lines {
                    lines,
                    mode: self.mode,
                    clear,
                },
                pts: self.current_pts,
                duration: Some(gst::ClockTime::ZERO),
            })
        } else {
            None
        }
    }

    fn drain_pending(&mut self, element: &super::Cea608ToJson) -> Option<TimestampedLines> {
        if let Some(mut pending) = self.pending_lines.take() {
            gst_log!(CAT, obj: element, "Draining pending");
            pending.duration = self
                .current_pts
                .zip(self.current_duration)
                .map(|(cur_pts, cur_dur)| cur_pts + cur_dur)
                .zip(pending.pts)
                .and_then(|(cur_end, pending_pts)| cur_end.checked_sub(pending_pts));
            Some(pending)
        } else {
            None
        }
    }

    fn decode_preamble(
        &mut self,
        element: &super::Cea608ToJson,
        cc_data: u16,
    ) -> Option<TimestampedLines> {
        let preamble = parse_preamble(cc_data);

        if preamble.chan != 0 {
            return None;
        }

        gst_log!(CAT, obj: element, "preamble: {:?}", preamble);

        self.cursor.row = preamble.row as u32;
        self.cursor.col = preamble.col as usize;
        self.cursor.style = preamble.style;
        self.cursor.underline = preamble.underline != 0;

        if let Some(mode) = self.mode {
            match mode {
                // The relocation is potentially destructive, let us drain
                Cea608Mode::RollUp2
                | Cea608Mode::RollUp3
                | Cea608Mode::RollUp4
                | Cea608Mode::PaintOn => {
                    let ret = self.drain(element);

                    self.rows.insert(self.cursor.row, Row::new(self.cursor.row));

                    ret
                }
                Cea608Mode::PopOn => {
                    #[allow(clippy::map_entry)]
                    if !self.rows.contains_key(&self.cursor.row) {
                        self.rows.insert(self.cursor.row, Row::new(self.cursor.row));
                    }
                    None
                }
            }
        } else {
            None
        }
    }

    fn decode_control(
        &mut self,
        element: &super::Cea608ToJson,
        cc_data: u16,
    ) -> Option<TimestampedLines> {
        let (cmd, chan) = parse_control(cc_data);

        gst_log!(CAT, obj: element, "Command for CC {}", chan);

        if chan != 0 {
            return None;
        }

        match cmd {
            ffi::eia608_control_t_eia608_control_resume_direct_captioning => {
                return self.update_mode(element, Cea608Mode::PaintOn);
            }
            ffi::eia608_control_t_eia608_control_erase_display_memory => {
                return match self.mode {
                    Some(Cea608Mode::PopOn) => {
                        self.clear = Some(true);
                        self.drain_pending(element)
                    }
                    _ => {
                        let ret = self.drain(element);
                        self.clear = Some(true);
                        ret
                    }
                };
            }
            ffi::eia608_control_t_eia608_control_roll_up_2 => {
                return self.update_mode(element, Cea608Mode::RollUp2);
            }
            ffi::eia608_control_t_eia608_control_roll_up_3 => {
                return self.update_mode(element, Cea608Mode::RollUp3);
            }
            ffi::eia608_control_t_eia608_control_roll_up_4 => {
                return self.update_mode(element, Cea608Mode::RollUp4);
            }
            ffi::eia608_control_t_eia608_control_carriage_return => {
                gst_log!(CAT, obj: element, "carriage return");

                if let Some(mode) = self.mode {
                    // https://www.law.cornell.edu/cfr/text/47/79.101 (f)(2)(i) (f)(3)(i)
                    if mode.is_rollup() {
                        let ret = self.drain(element);
                        self.carriage_return = Some(true);
                        return ret;
                    }
                }
            }
            ffi::eia608_control_t_eia608_control_backspace => {
                if let Some(row) = self.rows.get_mut(&self.cursor.row) {
                    row.pop(&mut self.cursor);
                }
            }
            ffi::eia608_control_t_eia608_control_resume_caption_loading => {
                return self.update_mode(element, Cea608Mode::PopOn);
            }
            ffi::eia608_control_t_eia608_control_erase_non_displayed_memory => {
                if self.mode == Some(Cea608Mode::PopOn) {
                    self.rows.clear();
                }
            }
            ffi::eia608_control_t_eia608_control_end_of_caption => {
                // https://www.law.cornell.edu/cfr/text/47/79.101 (f)(2)
                self.update_mode(element, Cea608Mode::PopOn);
                self.first_pts = self.current_pts;
                let ret = self.drain_pending(element);
                self.pending_lines = self.drain(element);
                return ret;
            }
            ffi::eia608_control_t_eia608_tab_offset_0
            | ffi::eia608_control_t_eia608_tab_offset_1
            | ffi::eia608_control_t_eia608_tab_offset_2
            | ffi::eia608_control_t_eia608_tab_offset_3 => {
                self.cursor.col += (cmd - ffi::eia608_control_t_eia608_tab_offset_0) as usize;
            }
            // TODO
            ffi::eia608_control_t_eia608_control_alarm_off
            | ffi::eia608_control_t_eia608_control_delete_to_end_of_row => {}
            ffi::eia608_control_t_eia608_control_alarm_on
            | ffi::eia608_control_t_eia608_control_text_restart
            | ffi::eia608_control_t_eia608_control_text_resume_text_display => {}
            _ => {
                gst_warning!(CAT, obj: element, "Unknown command {}!", cmd);
            }
        }

        None
    }

    fn decode_text(&mut self, element: &super::Cea608ToJson, cc_data: u16) {
        let (char1, char2, chan) = eia608_to_utf8(cc_data);

        if chan != 0 {
            return;
        }

        if let Some(row) = self.rows.get_mut(&self.cursor.row) {
            if is_westeu(cc_data) {
                row.pop(&mut self.cursor);
            }

            if (char1.is_some() || char2.is_some()) && self.first_pts.is_none() {
                if let Some(mode) = self.mode {
                    if mode.is_rollup() || mode == Cea608Mode::PaintOn {
                        self.first_pts = self.current_pts;
                    }
                }
            }

            if let Some(c) = char1 {
                row.push(&mut self.cursor, c);
            }

            if let Some(c) = char2 {
                row.push(&mut self.cursor, c);
            }
        } else {
            gst_warning!(CAT, obj: element, "No row to append decoded text to!");
        }
    }

    fn decode_midrowchange(&mut self, cc_data: u16) {
        if let Some(row) = self.rows.get_mut(&self.cursor.row) {
            let midrowchange = parse_midrowchange(cc_data);

            if midrowchange.chan == 0 {
                row.push_midrow(&mut self.cursor, midrowchange.style, midrowchange.underline);
            }
        }
    }

    fn handle_cc_data(
        &mut self,
        element: &super::Cea608ToJson,
        pts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
        cc_data: u16,
    ) -> Option<TimestampedLines> {
        if (is_specialna(cc_data) || is_control(cc_data)) && Some(cc_data) == self.last_cc_data {
            gst_log!(CAT, obj: element, "Skipping duplicate");
            return None;
        }

        self.last_cc_data = Some(cc_data);
        self.current_pts = pts;
        self.current_duration = duration;

        if is_xds(cc_data) {
            gst_log!(CAT, obj: element, "XDS, ignoring");
        } else if is_control(cc_data) {
            gst_log!(CAT, obj: element, "control!");
            return self.decode_control(element, cc_data);
        } else if is_basicna(cc_data) || is_specialna(cc_data) || is_westeu(cc_data) {
            self.mode?;
            gst_log!(CAT, obj: element, "text");
            self.decode_text(element, cc_data);
        } else if is_preamble(cc_data) {
            gst_log!(CAT, obj: element, "preamble");
            return self.decode_preamble(element, cc_data);
        } else if is_midrowchange(cc_data) {
            gst_log!(CAT, obj: element, "midrowchange");
            self.decode_midrowchange(cc_data);
        }
        None
    }
}

impl Cea608ToJson {
    fn output(
        &self,
        element: &super::Cea608ToJson,
        lines: TimestampedLines,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(CAT, obj: element, "outputting: {:?}", lines);

        let json = serde_json::to_string(&lines.lines).map_err(|err| {
            gst::element_error!(
                element,
                gst::ResourceError::Write,
                ["Failed to serialize as json {}", err]
            );

            gst::FlowError::Error
        })?;

        let mut buf = gst::Buffer::from_mut_slice(json.into_bytes());
        {
            let buf_mut = buf.get_mut().unwrap();
            buf_mut.set_pts(lines.pts);
            buf_mut.set_duration(lines.duration);
        }

        gst_log!(CAT, obj: element, "Pushing {:?}", buf);

        self.srcpad.push(buf)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::Cea608ToJson,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_trace!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.borrow_mut();

        let pts = buffer.pts();
        if pts.is_none() {
            gst_error!(CAT, obj: pad, "Require timestamped buffers");
            return Err(gst::FlowError::Error);
        }

        let duration = buffer.duration();
        if duration.is_none() {
            gst_error!(CAT, obj: pad, "Require buffers with duration");
            return Err(gst::FlowError::Error);
        }

        let data = buffer.map_readable().map_err(|_| {
            gst_error!(CAT, obj: pad, "Can't map buffer readable");

            gst::FlowError::Error
        })?;

        if data.len() < 2 {
            gst_error!(CAT, obj: pad, "Invalid closed caption packet size");

            return Ok(gst::FlowSuccess::Ok);
        }

        let cc_data = (data[0] as u16) << 8 | data[1] as u16;

        dump(element, cc_data, pts, duration);

        if let Some(lines) = state.handle_cc_data(element, pts, duration, cc_data) {
            drop(state);
            self.output(element, lines)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::Cea608ToJson, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(..) => {
                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-json")
                    .field("format", &"cea608")
                    .build();
                self.srcpad.push_event(gst::event::Caps::new(&caps))
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
                drop(state);
                pad.event_default(Some(element), event)
            }
            EventView::Eos(..) => {
                if let Some(lines) = self.state.borrow_mut().drain_pending(element) {
                    let _ = self.output(element, lines);
                }
                if let Some(lines) = self.state.borrow_mut().drain(element) {
                    let _ = self.output(element, lines);
                }

                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Cea608ToJson {
    const NAME: &'static str = "Cea608ToJson";
    type Type = super::Cea608ToJson;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                Cea608ToJson::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this, element| this.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Cea608ToJson::catch_panic_pad_function(
                    parent,
                    || false,
                    |this, element| this.sink_event(pad, element, event),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: AtomicRefCell::new(State::default()),
        }
    }
}

impl ObjectImpl for Cea608ToJson {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for Cea608ToJson {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "CEA-608 to TT",
                "Generic",
                "Converts CEA-608 Closed Captions to JSON",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/x-json").build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("closedcaption/x-cea-608")
                .field("format", &"raw")
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        let ret = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        Ok(ret)
    }
}
