// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
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

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use super::cea608tott_ffi as ffi;
use atomic_refcell::AtomicRefCell;

fn is_basicna(cc_data: u16) -> bool {
    0x0000 != (0x6000 & cc_data)
}

fn is_westeu(cc_data: u16) -> bool {
    0x1220 == (0x7660 & cc_data)
}

fn is_specialna(cc_data: u16) -> bool {
    0x1130 == (0x7770 & cc_data)
}

fn eia608_from_utf8_1(c: &[u8; 5]) -> u16 {
    assert!(c[4] == 0);
    unsafe { ffi::eia608_from_utf8_1(c.as_ptr() as *const _, 0) }
}

fn eia608_row_column_preamble(row: i32, col: i32) -> u16 {
    unsafe {
        /* Hardcoded chan and underline */
        ffi::eia608_row_column_pramble(row, col, 0, 0)
    }
}

fn eia608_control_command(cmd: ffi::eia608_control_t) -> u16 {
    unsafe { ffi::eia608_control_command(cmd, 0) }
}

fn eia608_from_basicna(bna1: u16, bna2: u16) -> u16 {
    unsafe { ffi::eia608_from_basicna(bna1, bna2) }
}

fn buffer_from_cc_data(cc_data: u16) -> gst::buffer::Buffer {
    let mut ret = gst::Buffer::with_size(2).unwrap();
    {
        let buf_mut = ret.get_mut().unwrap();

        let cc_data = cc_data.to_be_bytes();

        gst_trace!(CAT, "CC data: {:x} {:x}", cc_data[0], cc_data[1]);

        buf_mut.copy_from_slice(0, &cc_data).unwrap();
    }

    ret
}

fn control_command_buffer(buffers: &mut Vec<gst::Buffer>, cmd: ffi::eia608_control_t) {
    let cc_data = eia608_control_command(cmd);
    buffers.push(buffer_from_cc_data(cc_data));
    buffers.push(buffer_from_cc_data(cc_data));
}

fn erase_non_displayed_memory(buffers: &mut Vec<gst::Buffer>) {
    control_command_buffer(
        buffers,
        ffi::eia608_control_t_eia608_control_erase_non_displayed_memory,
    );
}

fn erase_display_memory(
    bufferlist: &mut gst::BufferListRef,
    pts: gst::ClockTime,
    duration: gst::ClockTime,
) {
    let cc_data = eia608_control_command(ffi::eia608_control_t_eia608_control_erase_display_memory);

    let mut buffer = buffer_from_cc_data(cc_data);
    {
        let buf_mut = buffer.get_mut().unwrap();
        buf_mut.set_pts(pts);
        buf_mut.set_duration(duration);
    }
    bufferlist.insert(0, buffer);

    let mut buffer = buffer_from_cc_data(cc_data);
    {
        let buf_mut = buffer.get_mut().unwrap();
        buf_mut.set_pts(pts + duration);
        buf_mut.set_duration(duration);
    }
    bufferlist.insert(1, buffer);
}

fn resume_caption_loading(buffers: &mut Vec<gst::Buffer>) {
    control_command_buffer(
        buffers,
        ffi::eia608_control_t_eia608_control_resume_caption_loading,
    );
}

fn end_of_caption(buffers: &mut Vec<gst::Buffer>) {
    control_command_buffer(buffers, ffi::eia608_control_t_eia608_control_end_of_caption);
}

fn preamble_buffer(buffers: &mut Vec<gst::Buffer>, row: i32, col: i32) {
    let cc_data = eia608_row_column_preamble(row, col);
    buffers.push(buffer_from_cc_data(cc_data));
    buffers.push(buffer_from_cc_data(cc_data));
}

fn bna_buffer(buffers: &mut Vec<gst::Buffer>, bna1: u16, bna2: u16) {
    let cc_data = eia608_from_basicna(bna1, bna2);

    buffers.push(buffer_from_cc_data(cc_data));
}

const DEFAULT_FPS_N: i32 = 30;
const DEFAULT_FPS_D: i32 = 1;

struct State {
    framerate: gst::Fraction,
    last_ts: gst::ClockTime,
}

impl Default for State {
    fn default() -> Self {
        Self {
            framerate: gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
            last_ts: gst::CLOCK_TIME_NONE,
        }
    }
}

struct TtToCea608 {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    state: AtomicRefCell<State>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "tttocea608",
        gst::DebugColorFlags::empty(),
        Some("TT CEA 608 Element"),
    );
    static ref SPACE: u16 = eia608_from_utf8_1(&[0x20, 0, 0, 0, 0]);
}

impl TtToCea608 {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(CAT, obj: pad, "Handling buffer {:?}", buffer);
        let mut row = 13;
        let mut col = 0;

        let mut pts = match buffer.get_pts() {
            gst::CLOCK_TIME_NONE => {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Stream with timestamped buffers required"]
                );
                Err(gst::FlowError::Error)
            }
            pts => Ok(pts),
        }?;

        let duration = match buffer.get_duration() {
            gst::CLOCK_TIME_NONE => {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Buffers of stream need to have a duration"]
                );
                Err(gst::FlowError::Error)
            }
            duration => Ok(duration),
        }?;

        let mut state = self.state.borrow_mut();
        let mut buffers = vec![];

        let frame_duration = gst::SECOND
            .mul_div_floor(
                *state.framerate.denom() as u64,
                *state.framerate.numer() as u64,
            )
            .unwrap();

        {
            resume_caption_loading(&mut buffers);
            erase_non_displayed_memory(&mut buffers);
            preamble_buffer(&mut buffers, row, 0);

            let data = buffer.map_readable().map_err(|_| {
                gst_error!(CAT, obj: pad, "Can't map buffer readable");

                gst::FlowError::Error
            })?;

            let data = std::str::from_utf8(&data).map_err(|err| {
                gst_error!(CAT, obj: pad, "Can't decode utf8: {}", err);

                gst::FlowError::Error
            })?;

            let mut prev_char: u16 = 0;
            for c in data.chars() {
                if c == '\n' {
                    if prev_char != 0 {
                        buffers.push(buffer_from_cc_data(prev_char));
                        prev_char = 0;
                    }

                    row += 1;

                    if row > 14 {
                        break;
                    }

                    preamble_buffer(&mut buffers, row, 0);

                    col = 0;
                    continue;
                } else if c == '\r' {
                    continue;
                }

                let mut encoded = [0; 5];
                c.encode_utf8(&mut encoded);
                let mut cc_data = eia608_from_utf8_1(&encoded);

                if cc_data == 0 {
                    gst_warning!(CAT, obj: element, "Not translating UTF8: {}", c);
                    cc_data = *SPACE;
                }

                if is_basicna(prev_char) {
                    if is_basicna(cc_data) {
                        bna_buffer(&mut buffers, prev_char, cc_data);
                    } else if is_westeu(cc_data) {
                        // extended characters overwrite the previous character,
                        // so insert a dummy char then write the extended char
                        bna_buffer(&mut buffers, prev_char, *SPACE);
                        buffers.push(buffer_from_cc_data(cc_data));
                    } else {
                        buffers.push(buffer_from_cc_data(prev_char));
                        buffers.push(buffer_from_cc_data(cc_data));
                    }
                    prev_char = 0;
                } else if is_westeu(cc_data) {
                    // extended characters overwrite the previous character,
                    // so insert a dummy char then write the extended char
                    buffers.push(buffer_from_cc_data(*SPACE));
                    buffers.push(buffer_from_cc_data(cc_data));
                } else if is_basicna(cc_data) {
                    prev_char = cc_data;
                } else {
                    buffers.push(buffer_from_cc_data(cc_data));
                }

                if is_specialna(cc_data) {
                    resume_caption_loading(&mut buffers);
                }

                col += 1;

                if col > 32 {
                    gst_warning!(
                        CAT,
                        obj: element,
                        "Dropping character after 32nd column: {}",
                        c
                    );
                    continue;
                }
            }

            if prev_char != 0 {
                buffers.push(buffer_from_cc_data(prev_char));
            }

            end_of_caption(&mut buffers);
        }

        let mut bufferlist = gst::BufferList::new();

        let erase_display_pts = {
            if state.last_ts.is_some() && state.last_ts < pts {
                state.last_ts
            } else {
                gst::CLOCK_TIME_NONE
            }
        };

        state.last_ts = pts + duration;

        // FIXME: the following code may result in overlapping timestamps
        // when too many characters need encoding for a given interval

        /* Account for doubled end_of_caption control */
        pts += frame_duration;

        for mut buffer in buffers.drain(..).rev() {
            let buf_mut = buffer.get_mut().unwrap();
            let prev_pts = pts;

            buf_mut.set_pts(pts);

            if pts > frame_duration {
                pts -= frame_duration;
            } else {
                pts = 0.into();
            }

            buf_mut.set_duration(prev_pts - pts);
            bufferlist.get_mut().unwrap().insert(0, buffer);
        }

        if erase_display_pts.is_some() {
            erase_display_memory(
                bufferlist.get_mut().unwrap(),
                erase_display_pts,
                frame_duration,
            );
        }

        self.srcpad.push_list(bufferlist).map_err(|err| {
            gst_error!(CAT, obj: &self.srcpad, "Pushing buffer returned {:?}", err);
            err
        })
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        use gst::EventView;

        match event.view() {
            EventView::Caps(..) => {
                let mut downstream_caps = match self.srcpad.get_allowed_caps() {
                    None => self.srcpad.get_pad_template_caps().unwrap(),
                    Some(caps) => caps,
                };

                if downstream_caps.is_empty() {
                    gst_error!(CAT, obj: pad, "Empty downstream caps");
                    return false;
                }

                let caps = downstream_caps.make_mut();
                let s = caps.get_mut_structure(0).unwrap();

                s.fixate_field_nearest_fraction(
                    "framerate",
                    gst::Fraction::new(DEFAULT_FPS_N, DEFAULT_FPS_D),
                );
                s.fixate();

                let mut state = self.state.borrow_mut();
                state.framerate = s.get_some::<gst::Fraction>("framerate").unwrap();

                gst_debug!(CAT, obj: pad, "Pushing caps {}", caps);

                let new_event = gst::Event::new_caps(&downstream_caps).build();

                return self.srcpad.push_event(new_event);
            }
            _ => (),
        }

        pad.event_default(Some(element), event)
    }
}

impl ObjectSubclass for TtToCea608 {
    const NAME: &'static str = "TtToCea608";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

        sinkpad.set_chain_function(|pad, parent, buffer| {
            TtToCea608::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |this, element| this.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            TtToCea608::catch_panic_pad_function(
                parent,
                || false,
                |this, element| this.sink_event(pad, element, event),
            )
        });

        sinkpad.use_fixed_caps();
        srcpad.use_fixed_caps();

        Self {
            srcpad,
            sinkpad,
            state: AtomicRefCell::new(State::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "TT to CEA-608",
            "Generic",
            "Converts timed text to CEA-608 Closed Captions",
            "Mathieu Duponchelle <mathieu@centricular.com>",
        );

        let caps = gst::Caps::builder("text/x-raw").build();

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let framerate = gst::FractionRange::new(
            gst::Fraction::new(1, std::i32::MAX),
            gst::Fraction::new(std::i32::MAX, 1),
        );

        let caps = gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .field("framerate", &framerate)
            .build();

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);
    }
}

impl ObjectImpl for TtToCea608 {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for TtToCea608 {
    fn change_state(
        &self,
        element: &gst::Element,
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

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "tttocea608",
        gst::Rank::None,
        TtToCea608::get_type(),
    )
}
