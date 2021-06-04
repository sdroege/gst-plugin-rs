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

use gst::glib;
use gst::prelude::*;
use gst::structure;
use gst::subclass::prelude::*;
use gst::{gst_error, gst_log, gst_trace};
use gst_video::{self, ValidVideoTimeCode};

use once_cell::sync::Lazy;

use std::io::Write;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "sccenc",
        gst::DebugColorFlags::empty(),
        Some("Scc Encoder Element"),
    )
});

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
        element: &super::SccEnc,
        buffer: gst::Buffer,
    ) -> Result<Option<gst::Buffer>, gst::FlowError> {
        // Arbitrary number that was chosen to keep in order
        // to batch pushes of smaller buffers
        const MAXIMUM_PACKETES_PER_LINE: usize = 16;

        assert!(self.internal_buffer.len() < MAXIMUM_PACKETES_PER_LINE);

        if buffer.size() != 2 {
            gst::element_error!(
                element,
                gst::StreamError::Format,
                ["Wrongly sized CEA608 packet: {}", buffer.size()]
            );

            return Err(gst::FlowError::Error);
        };

        let mut timecode = buffer
            .meta::<gst_video::VideoTimeCodeMeta>()
            .ok_or_else(|| {
                gst::element_error!(
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
            .tc();

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
        element: &super::SccEnc,
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
            let map = buffer.map_readable().map_err(|_| {
                gst::element_error!(
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
                    .meta::<gst_video::VideoTimeCodeMeta>()
                    // Checked already before the buffer has been pushed to the
                    // internal_buffer
                    .expect("Buffer without timecode")
                    .tc();

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

            let dur = gst::ClockTime::SECOND.mul_div_floor(
                self.internal_buffer.len() as u64 * *framerate.denom() as u64,
                *framerate.numer() as u64,
            );
            buf_mut.set_duration(dur);

            // Copy the metadata of the first buffer
            first_buf
                .copy_into(buf_mut, gst::BUFFER_COPY_METADATA, 0, None)
                .expect("Failed to copy buffer metadata");
            buf_mut.set_pts(first_buf.pts());
            buffer
        };

        // Clear the internal buffer
        self.internal_buffer.clear();

        Ok(Some(buffer))
    }
}

pub struct SccEnc {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl SccEnc {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::SccEnc,
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

    fn sink_event(&self, pad: &gst::Pad, element: &super::SccEnc, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(ev) => {
                let caps = ev.caps();
                let s = caps.structure(0).unwrap();
                let framerate = match s.get::<gst::Fraction>("framerate") {
                    Ok(framerate) => Some(framerate),
                    Err(structure::GetError::FieldNotFound { .. }) => {
                        gst_error!(CAT, obj: pad, "Caps without framerate");
                        return false;
                    }
                    err => panic!("SccEnc::sink_event caps: {:?}", err),
                };

                let mut state = self.state.lock().unwrap();
                state.framerate = framerate;

                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-scc").build();
                self.srcpad.push_event(gst::event::Caps::new(&caps))
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

    fn src_event(&self, pad: &gst::Pad, element: &super::SccEnc, event: gst::Event) -> bool {
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

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::SccEnc,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Seeking(mut q) => {
                // We don't support any seeking at all
                let fmt = q.format();
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

#[glib::object_subclass]
impl ObjectSubclass for SccEnc {
    const NAME: &'static str = "RsSccEnc";
    type Type = super::SccEnc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                SccEnc::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |enc, element| enc.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                SccEnc::catch_panic_pad_function(
                    parent,
                    || false,
                    |enc, element| enc.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                SccEnc::catch_panic_pad_function(
                    parent,
                    || false,
                    |enc, element| enc.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                SccEnc::catch_panic_pad_function(
                    parent,
                    || false,
                    |enc, element| enc.src_query(pad, element, query),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for SccEnc {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for SccEnc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
            "Scc Encoder",
            "Encoder/ClosedCaption",
            "Encodes SCC Closed Caption Files",
            "Sebastian Dröge <sebastian@centricular.com>, Jordan Petridis <jordan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
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

            let caps = gst::Caps::builder("application/x-scc").build();
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

    fn change_state(
        &self,
        element: &Self::Type,
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
