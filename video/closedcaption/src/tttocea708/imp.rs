// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use cea708_types::CCDataWriter;
use cea708_types::DTVCCPacket;
use cea708_types::Framerate;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use once_cell::sync::Lazy;

use crate::cea608utils::Cea608Mode;
use std::sync::Mutex;

use cea708_types::tables::*;

use crate::cea608utils::TextStyle;
use crate::cea708utils::{
    textstyle_foreground_color, textstyle_to_pen_color, Cea708Mode, Cea708ServiceWriter,
};
use crate::ttutils::{Chunk, Line, Lines};

const DEFAULT_FPS_N: i32 = 30;
const DEFAULT_FPS_D: i32 = 1;

const DEFAULT_MODE: Cea708Mode = Cea708Mode::RollUp;
const DEFAULT_ORIGIN_ROW: i32 = -1;
const DEFAULT_ORIGIN_COLUMN: u32 = 0;
const DEFAULT_ROLL_UP_ROWS: u8 = 2;
const DEFAULT_SERVICE_NO: u8 = 1;

#[derive(Debug, Clone)]
struct Settings {
    mode: Cea708Mode,
    service_no: u8,
    roll_up_rows: u8,
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
            roll_up_rows: DEFAULT_ROLL_UP_ROWS,
            roll_up_timeout: gst::ClockTime::NONE,
            service_no: DEFAULT_SERVICE_NO,
        }
    }
}

struct State {
    sequence_no: u8,
    cc_data_writer: CCDataWriter,
    framerate: gst::Fraction,
    service_writer: Cea708ServiceWriter,
    pen_location: SetPenLocationArgs,
    pen_color: SetPenColorArgs,
    pen_attributes: SetPenAttributesArgs,
    mode: Cea708Mode,
    erase_display_frame_no: Option<u64>,
    last_frame_no: u64,
    max_frame_no: u64,
    send_roll_up_preamble: bool,
    force_clear: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sequence_no: 0,
            cc_data_writer: CCDataWriter::default(),
            framerate: gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
            pen_color: textstyle_to_pen_color(TextStyle::White),
            pen_attributes: SetPenAttributesArgs::new(
                PenSize::Standard,
                FontStyle::Default,
                TextTag::Dialog,
                TextOffset::Normal,
                false,
                false,
                EdgeType::None,
            ),
            pen_location: SetPenLocationArgs::new(0, 0),
            service_writer: Cea708ServiceWriter::new(0),
            erase_display_frame_no: None,
            last_frame_no: 0,
            max_frame_no: 0,
            send_roll_up_preamble: false,
            mode: Cea708Mode::PopOn,
            force_clear: false,
        }
    }
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "tttocea708",
        gst::DebugColorFlags::empty(),
        Some("TT CEA 708 Element"),
    )
});

fn cc_data_buffer(data: &[u8], pts: gst::ClockTime, duration: gst::ClockTime) -> gst::Buffer {
    let mut ret = gst::Buffer::with_size(data.len()).unwrap();
    let buf_mut = ret.get_mut().unwrap();

    buf_mut.copy_from_slice(0, data).unwrap();
    buf_mut.set_pts(pts);
    buf_mut.set_duration(duration);

    ret
}

fn fraction_to_framerate(fraction: gst::Fraction) -> Framerate {
    Framerate::new(fraction.numer() as u32, fraction.denom() as u32)
}

impl State {
    fn check_erase_display(&mut self) -> bool {
        if let Some(erase_display_frame_no) = self.erase_display_frame_no {
            if self.last_frame_no == erase_display_frame_no - 1 {
                self.erase_display_frame_no = None;
                self.send_roll_up_preamble = true;
                self.service_writer.clear_current_window();
                return true;
            }
        }

        false
    }

    fn cc_data(&mut self, imp: &TtToCea708, bufferlist: &mut gst::BufferListRef) {
        self.check_erase_display();

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

        let seq_no = self.sequence_no;
        self.sequence_no = (self.sequence_no + 1) & 0x3;

        let mut packet = DTVCCPacket::new(seq_no);
        gst::trace!(CAT, "New packet {}", packet.sequence_no());
        while let Some(service) = self.service_writer.take_service(packet.free_space()) {
            gst::trace!(CAT, "adding service {service:?} to packet");
            packet.push_service(service).unwrap();
        }
        gst::trace!(CAT, "push packet to writer");
        self.cc_data_writer.push_packet(packet);

        let mut cc_data = vec![];
        gst::trace!(CAT, "write packet to data");
        self.cc_data_writer
            .write(fraction_to_framerate(self.framerate), &mut cc_data)
            .unwrap();

        gst::trace!(CAT, "add data to buffer list");
        bufferlist.insert(-1, cc_data_buffer(&cc_data[2..], pts, duration));
    }

    fn pad(&mut self, imp: &TtToCea708, bufferlist: &mut gst::BufferListRef, frame_no: u64) {
        while self.last_frame_no < frame_no {
            if !self.check_erase_display() {
                self.cc_data(imp, bufferlist);
            }
        }
    }
}

pub struct TtToCea708 {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    // Ordered by locking order
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl TtToCea708 {
    fn open_line(
        &self,
        state: &mut State,
        settings: &Settings,
        chunk: &Chunk,
        carriage_return: Option<bool>,
    ) {
        let do_preamble = match state.mode {
            Cea708Mode::PopOn | Cea708Mode::PaintOn => true,
            Cea708Mode::RollUp => {
                if let Some(carriage_return) = carriage_return {
                    if carriage_return {
                        state.service_writer.push_codes(&[Code::CR]);
                        state.pen_location.column = settings.origin_column as u8;
                        true
                    } else {
                        state.send_roll_up_preamble
                    }
                } else {
                    state.send_roll_up_preamble
                }
            }
        };

        if do_preamble {
            if state.mode == Cea708Mode::RollUp {
                state
                    .service_writer
                    .rollup_preamble(settings.roll_up_rows, 15);
            }

            state.send_roll_up_preamble = false;
        }

        let mut need_pen_attributes = false;
        if state.pen_attributes.italics != chunk.style.is_italics() {
            need_pen_attributes = true;
            state.pen_attributes.italics = chunk.style.is_italics();
        }

        if state.pen_attributes.underline != (chunk.underline) {
            need_pen_attributes = true;
            state.pen_attributes.underline = chunk.underline;
        }

        if need_pen_attributes {
            state
                .service_writer
                .set_pen_attributes(state.pen_attributes);
        }

        if state.pen_color.foreground_color != textstyle_foreground_color(chunk.style) {
            state.pen_color.foreground_color = textstyle_foreground_color(chunk.style);
            state.service_writer.set_pen_color(state.pen_color);
        }
    }

    fn peek_word_length(&self, chars: std::iter::Peekable<std::str::Chars>) -> u32 {
        chars.take_while(|c| !c.is_ascii_whitespace()).count() as u32
    }

    fn generate(
        &self,
        state: &mut State,
        settings: &Settings,
        pts: gst::ClockTime,
        duration: gst::ClockTime,
        lines: Lines,
    ) -> Result<gst::BufferList, gst::FlowError> {
        let origin_column = settings.origin_column;
        let mut row = 13;
        let mut bufferlist = gst::BufferList::new();
        let mut_list = bufferlist.get_mut().unwrap();

        state.service_writer = Cea708ServiceWriter::new(settings.service_no);

        if state.mode == Cea708Mode::PopOn || state.mode == Cea708Mode::PaintOn {
            state.pen_location.column = 0;
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
        let mut need_pen_location = false;
        if let Some(mode) = lines.mode {
            if (mode.is_rollup() && state.mode != Cea708Mode::RollUp)
                || (mode == Cea608Mode::PaintOn && state.mode != Cea708Mode::PaintOn)
                || (mode == Cea608Mode::PopOn && state.mode == Cea708Mode::PopOn)
            {
                /* Always erase the display when going to or from pop-on */
                if state.mode == Cea708Mode::PopOn || mode == Cea608Mode::PopOn {
                    state.erase_display_frame_no = None;
                    state.service_writer.clear_current_window();
                    cleared = true;
                }

                state.mode = match mode {
                    Cea608Mode::PopOn => Cea708Mode::PopOn,
                    Cea608Mode::PaintOn => Cea708Mode::PaintOn,
                    Cea608Mode::RollUp2 | Cea608Mode::RollUp3 | Cea608Mode::RollUp4 => {
                        Cea708Mode::RollUp
                    }
                };
                match state.mode {
                    Cea708Mode::RollUp => {
                        state.send_roll_up_preamble = true;
                    }
                    _ => {
                        state.pen_location.column = origin_column as u8;
                        need_pen_location = true;
                    }
                }
            }
        }

        if let Some(clear) = lines.clear {
            if clear && !cleared {
                state.erase_display_frame_no = None;
                state.service_writer.clear_current_window();
                if state.mode != Cea708Mode::PopOn && state.mode != Cea708Mode::PaintOn {
                    state.send_roll_up_preamble = true;
                }
                state.pen_location.column = origin_column as u8;
                need_pen_location = true;
            }
        }

        if state.mode == Cea708Mode::PopOn {
            state.service_writer.popon_preamble();
        } else if state.mode == Cea708Mode::PaintOn {
            state.service_writer.paint_on_preamble();
        }

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
                if state.mode != Cea708Mode::PopOn && state.mode != Cea708Mode::PaintOn {
                    state.send_roll_up_preamble = true;
                }
                state.pen_location.column = line_column as u8;
                need_pen_location = true;
            } else if state.mode == Cea708Mode::PopOn || state.mode == Cea708Mode::PaintOn {
                state.pen_location.column = origin_column as u8;
                need_pen_location = true;
            }

            if state.pen_location.row != row as u8 {
                need_pen_location = true;
                state.pen_location.row = row as u8;
            }

            if need_pen_location {
                state.service_writer.set_pen_location(state.pen_location);
            }

            for (i, chunk) in line.chunks.iter().enumerate() {
                let cr = if i == 0 { Some(true) } else { Some(false) };
                self.open_line(state, settings, chunk, cr);

                let mut chars = chunk.text.chars().peekable();

                while let Some(c) = chars.next() {
                    if c == '\r' {
                        continue;
                    }

                    let code = Code::from_char(c).unwrap_or(Code::Space);
                    state.service_writer.push_codes(&[code]);
                    state.pen_location.column += 1;

                    if state.mode == Cea708Mode::RollUp {
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

                        if (next_word_length <= 32 - origin_column
                            && state.pen_location.column as u32 + next_word_length > 31)
                            || state.pen_location.column > 31
                        {
                            state.pen_location.column = settings.origin_column as u8;
                            state.service_writer.push_codes(&[Code::CR]);
                        }
                    } else if state.pen_location.column > 31 {
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

            if state.mode == Cea708Mode::PopOn || state.mode == Cea708Mode::PaintOn {
                row += 1;
            }
            need_pen_location = false;
        }

        if state.mode == Cea708Mode::PopOn {
            /* No need to erase the display at this point, end_of_caption will be equivalent */
            state.erase_display_frame_no = None;
            state.service_writer.end_of_caption();
        }

        if state.mode == Cea708Mode::PopOn {
            state.erase_display_frame_no =
                Some(state.last_frame_no + duration.mul_div_round(fps_n, fps_d).unwrap().seconds());
        } else if let Some(timeout) = settings.roll_up_timeout {
            state.erase_display_frame_no =
                Some(state.last_frame_no + timeout.mul_div_round(fps_n, fps_d).unwrap().seconds());
        }
        state.service_writer.push_codes(&[Code::ETX]);

        state.cc_data(self, mut_list);
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

        let cea608_mode = match settings.mode {
            Cea708Mode::PaintOn => Cea608Mode::PaintOn,
            Cea708Mode::PopOn => Cea608Mode::PopOn,
            Cea708Mode::RollUp => match settings.roll_up_rows {
                0..=2 => Cea608Mode::RollUp2,
                3 => Cea608Mode::RollUp3,
                _ => Cea608Mode::RollUp4,
            },
        };

        let mut lines = Lines {
            lines: Vec::new(),
            mode: Some(cea608_mode),
            clear: Some(state.force_clear),
        };
        state.force_clear = false;
        let data = std::str::from_utf8(&data).map_err(|err| {
            gst::error!(CAT, obj: pad, "Can't decode utf8: {}", err);

            gst::FlowError::Error
        })?;

        let phrases: Vec<&str> = data.split('\n').collect();
        let mut row = match settings.origin_row {
            -1 => match settings.mode {
                Cea708Mode::PopOn | Cea708Mode::PaintOn => {
                    15u32.saturating_sub(phrases.len() as u32)
                }
                Cea708Mode::RollUp => 14,
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
            if settings.mode == Cea708Mode::PopOn || settings.mode == Cea708Mode::PaintOn {
                row += 1;
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
            EventView::Caps(_e) => {
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

                if state.mode != Cea708Mode::PopOn {
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
impl ObjectSubclass for TtToCea708 {
    const NAME: &'static str = "GstTtToCea708";
    type Type = super::TtToCea708;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                TtToCea708::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this| this.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TtToCea708::catch_panic_pad_function(
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

impl ObjectImpl for TtToCea708 {
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
                glib::ParamSpecUInt::builder("service-number")
                    .nick("Service Number")
                    .blurb("Write DTVCC packets using this service")
                    .default_value(DEFAULT_SERVICE_NO as u32)
                    .minimum(1)
                    .maximum(63)
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
                settings.mode = value.get::<Cea708Mode>().expect("type checked upstream");
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
                state.pen_location.column = settings.origin_column as u8;
            }
            "roll-up-timeout" => {
                let mut settings = self.settings.lock().unwrap();

                let timeout = value.get().expect("type checked upstream");

                settings.roll_up_timeout = match timeout {
                    u64::MAX => gst::ClockTime::NONE,
                    _ => Some(timeout.nseconds()),
                };
            }
            "service-number" => {
                let mut settings = self.settings.lock().unwrap();
                settings.service_no = value.get::<u32>().expect("type checked upstream") as u8;
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
            "service-number" => {
                let settings = self.settings.lock().unwrap();
                (settings.service_no as u32).to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for TtToCea708 {}

impl ElementImpl for TtToCea708 {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "TT to CEA-708",
                "Generic",
                "Converts timed text to CEA-708 Closed Captions",
                "Matthew Waters <matthew@centricular.com>",
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
                /*
                let s = gst::Structure::builder("application/x-json")
                    .field("format", "cea608")
                    .build();
                caps.append_structure(s);*/
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

            let caps = gst::Caps::builder("closedcaption/x-cea-708")
                .field("format", "cc_data")
                .field("framerate", framerate)
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
                if state.mode != Cea708Mode::PopOn {
                    state.send_roll_up_preamble = true;
                    state.pen_location.column = settings.origin_column as u8;
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
