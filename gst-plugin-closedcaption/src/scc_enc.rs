// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019 Jordan Petridis <jordan@centricular.com>
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
use gst_video::{self, ValidVideoTimeCode};

use std::io::Write;
use std::sync::Mutex;

lazy_static! {
    static ref CAT: gst::DebugCategory = {
        gst::DebugCategory::new(
            "sccenc",
            gst::DebugColorFlags::empty(),
            Some("Scc Encoder Element"),
        )
    };
}

#[derive(Debug)]
struct State {
    need_headers: bool,
    expected_timecode: Option<ValidVideoTimeCode>,
    internal_buffer: Vec<gst::Buffer>,
    framerate: Option<gst::Fraction>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            need_headers: true,
            expected_timecode: None,
            internal_buffer: Vec::with_capacity(64),
            framerate: None,
        }
    }
}

impl State {
    // Write the header to the buffer and set need_headers to false
    fn generate_headers(&mut self, buffer: &mut Vec<u8>) {
        assert!(buffer.is_empty());
        self.need_headers = false;
        buffer.extend_from_slice(b"Scenarist_SCC V1.0\r\n\r\n");
    }

    fn encode_payload(outbuf: &mut Vec<u8>, slice: &[u8]) {
        write!(outbuf, "{:02x}{:02x}", slice[0], slice[1]).unwrap();
    }

    fn generate_caption(
        &mut self,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<Option<gst::Buffer>, gst::FlowError> {
        // Arbitrary number that was chosen to keep in order
        // to batch pushes of smaller buffers
        const MAXIMUM_PACKETES_PER_LINE: usize = 16;

        assert!(self.internal_buffer.len() < MAXIMUM_PACKETES_PER_LINE);

        if buffer.get_size() != 2 {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["Wrongly sized CEA608 packet: {}", buffer.get_size()]
            );

            return Err(gst::FlowError::Error);
        };

        let mut timecode = buffer
            .get_meta::<gst_video::VideoTimeCodeMeta>()
            .ok_or_else(|| {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Stream with timecodes on each buffer required"]
                );

                // If we neeed to skip a buffer, increment the frame if it exists
                // to avoid getting out of sync
                if let Some(ref mut timecode) = self.expected_timecode {
                    timecode.increment_frame();
                }

                gst::FlowError::Error
            })?
            .get_tc();

        if self.expected_timecode.is_none() {
            self.expected_timecode = Some(timecode.clone());
        }

        // if the timecode is different from the expected one,
        // flush the previous line into the buffer, and push
        // the new packet to the, now empty, internal buffer
        if Some(&timecode) != self.expected_timecode.as_ref() {
            let outbuf = self.write_line(element)?;

            assert!(self.internal_buffer.is_empty());
            self.internal_buffer.push(buffer);

            timecode.increment_frame();
            self.expected_timecode = Some(timecode);

            return Ok(outbuf);
        } else if let Some(ref mut timecode) = self.expected_timecode {
            timecode.increment_frame();
        }

        self.internal_buffer.push(buffer);

        if self.internal_buffer.len() == MAXIMUM_PACKETES_PER_LINE {
            return self.write_line(element);
        }

        Ok(None)
    }

    // Flush the internal buffers into a line
    fn write_line(
        &mut self,
        element: &gst::Element,
    ) -> Result<Option<gst::Buffer>, gst::FlowError> {
        let mut outbuf = Vec::new();
        let mut line_start = true;

        if self.internal_buffer.is_empty() {
            return Ok(None);
        }

        if self.need_headers {
            self.generate_headers(&mut outbuf);
        }

        let first_buf = self.internal_buffer.first().unwrap();
        for buffer in self.internal_buffer.iter() {
            let map = buffer.map_readable().ok_or_else(|| {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Failed to map buffer readable"]
                );

                gst::FlowError::Error
            })?;

            // If its the first packet in the line, write the timecode first
            // else, separate the packets with a space
            if line_start {
                let timecode = buffer
                    .get_meta::<gst_video::VideoTimeCodeMeta>()
                    // Checked already before the buffer has been pushed to the
                    // internal_buffer
                    .expect("Buffer without timecode")
                    .get_tc();

                let _ = write!(outbuf, "{}\t", timecode);
                line_start = false;
            } else {
                outbuf.push(b' ');
            }

            Self::encode_payload(&mut outbuf, &*map);
        }

        outbuf.extend_from_slice(b"\r\n\r\n".as_ref());

        let buffer = {
            let mut buffer = gst::Buffer::from_mut_slice(outbuf);
            let buf_mut = buffer.get_mut().unwrap();

            // Something is seriously wrong else
            assert!(self.framerate.is_some());
            let framerate = self.framerate.unwrap();

            let dur = gst::SECOND
                .mul_div_floor(
                    self.internal_buffer.len() as u64 * *framerate.denom() as u64,
                    *framerate.numer() as u64,
                )
                .unwrap_or(gst::CLOCK_TIME_NONE);
            buf_mut.set_duration(dur);

            // Copy the metadata of the first buffer
            first_buf
                .copy_into(buf_mut, *gst::BUFFER_COPY_METADATA, 0, None)
                .expect("Failed to copy buffer metadata");
            buf_mut.set_pts(first_buf.get_pts());
            buffer
        };

        // Clear the internal buffer
        self.internal_buffer.clear();

        Ok(Some(buffer))
    }
}

struct SccEnc {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl SccEnc {
    fn set_pad_functions(sinkpad: &gst::Pad, srcpad: &gst::Pad) {
        sinkpad.set_chain_function(|pad, parent, buffer| {
            SccEnc::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |enc, element| enc.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            SccEnc::catch_panic_pad_function(
                parent,
                || false,
                |enc, element| enc.sink_event(pad, element, event),
            )
        });

        srcpad.set_event_function(|pad, parent, event| {
            SccEnc::catch_panic_pad_function(
                parent,
                || false,
                |enc, element| enc.src_event(pad, element, event),
            )
        });
        srcpad.set_query_function(|pad, parent, query| {
            SccEnc::catch_panic_pad_function(
                parent,
                || false,
                |enc, element| enc.src_query(pad, element, query),
            )
        });
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();
        let res = state.generate_caption(element, buffer)?;

        if let Some(outbuf) = res {
            gst_trace!(CAT, obj: pad, "Pushing buffer {:?} to the pad", &outbuf);

            drop(state);
            self.srcpad.push(outbuf)?;
        };

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(ev) => {
                let caps = ev.get_caps();
                let s = caps.get_structure(0).unwrap();
                let framerate = s.get::<gst::Fraction>("framerate");
                if framerate.is_none() {
                    gst_error!(CAT, obj: pad, "Caps without framerate");
                    return false;
                };

                let mut state = self.state.lock().unwrap();
                state.framerate = framerate;

                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-scc").build();
                self.srcpad.push_event(gst::Event::new_caps(&caps).build())
            }
            EventView::Eos(_) => {
                let mut state = self.state.lock().unwrap();

                let outbuf = state.write_line(element);

                if let Ok(Some(buffer)) = outbuf {
                    gst_trace!(CAT, obj: pad, "Pushing buffer {:?} to the pad", &buffer);

                    drop(state);
                    if self.srcpad.push(buffer).is_err() {
                        gst_error!(CAT, obj: pad, "Failed to push buffer to the pad");
                        return false;
                    }
                } else if let Err(err) = outbuf {
                    gst_error!(CAT, obj: pad, "Failed to write a line after EOS: {:?}", err);
                    return false;
                }
                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(_) => {
                gst_log!(CAT, obj: pad, "Dropping seek event");
                false
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Seeking(mut q) => {
                // We don't support any seeking at all
                let fmt = q.get_format();
                q.set(
                    false,
                    gst::GenericFormattedValue::Other(fmt, -1),
                    gst::GenericFormattedValue::Other(fmt, -1),
                );
                true
            }
            _ => pad.query_default(Some(element), query),
        }
    }
}

impl ObjectSubclass for SccEnc {
    const NAME: &'static str = "RsSccEnc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

        SccEnc::set_pad_functions(&sinkpad, &srcpad);

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Scc Encoder",
            "Encoder/ClosedCaption",
            "Encodes SCC Closed Caption Files",
            "Sebastian Dröge <sebastian@centricular.com>, Jordan Petridis <jordan@centricular.com>",
        );

        let framerates =
            gst::List::new(&[&gst::Fraction::new(30000, 1001), &gst::Fraction::new(30, 1)]);
        let caps = gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .field("framerate", &framerates)
            .build();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let caps = gst::Caps::builder("application/x-scc").build();
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

impl ObjectImpl for SccEnc {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for SccEnc {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady => {
                // Reset the whole state
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(Some(plugin), "sccenc", gst::Rank::None, SccEnc::get_type())
}
