// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::structure;
use gst::subclass::prelude::*;

use chrono::prelude::*;
use uuid::Uuid;

use std::io::Write;
use std::sync::Mutex;

#[path = "mcc_enc_headers.rs"]
mod mcc_enc_headers;
use self::mcc_enc_headers::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Cea708Cdp,
    Cea608,
}

#[derive(Debug)]
struct State {
    format: Option<Format>,
    need_headers: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            format: None,
            need_headers: true,
        }
    }
}

#[derive(Debug, Clone)]
struct Settings {
    uuid: Option<String>,
    creation_date: Option<glib::DateTime>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            uuid: None,
            creation_date: None,
        }
    }
}

static PROPERTIES: [subclass::Property; 2] = [
    subclass::Property("uuid", |name| {
        glib::ParamSpec::string(
            name,
            "UUID",
            "UUID for the output file",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("creation-date", |name| {
        glib::ParamSpec::boxed(
            name,
            "Creation Date",
            "Creation date for the output file",
            glib::DateTime::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
];

struct MccEnc {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "mccenc",
        gst::DebugColorFlags::empty(),
        Some("Mcc Encoder Element"),
    );
}

impl MccEnc {
    fn set_pad_functions(sinkpad: &gst::Pad, srcpad: &gst::Pad) {
        sinkpad.set_chain_function(|pad, parent, buffer| {
            MccEnc::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |enc, element| enc.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            MccEnc::catch_panic_pad_function(
                parent,
                || false,
                |enc, element| enc.sink_event(pad, element, event),
            )
        });

        srcpad.set_event_function(|pad, parent, event| {
            MccEnc::catch_panic_pad_function(
                parent,
                || false,
                |enc, element| enc.src_event(pad, element, event),
            )
        });
        srcpad.set_query_function(|pad, parent, query| {
            MccEnc::catch_panic_pad_function(
                parent,
                || false,
                |enc, element| enc.src_query(pad, element, query),
            )
        });
    }

    #[allow(clippy::write_with_newline)]
    fn generate_headers(&self, _state: &State, buffer: &mut Vec<u8>) -> Result<(), gst::FlowError> {
        let settings = self.settings.lock().unwrap();

        let caps = self
            .sinkpad
            .get_current_caps()
            .ok_or(gst::FlowError::NotNegotiated)?;
        let framerate = match caps
            .get_structure(0)
            .unwrap()
            .get_some::<gst::Fraction>("framerate")
        {
            Ok(framerate) => framerate,
            Err(structure::GetError::FieldNotFound { .. }) => {
                return Err(gst::FlowError::NotNegotiated);
            }
            err => panic!("MccEnc::generate_headers caps: {:?}", err),
        };

        if framerate == gst::Fraction::new(60000, 1001) {
            buffer.extend_from_slice(PREAMBLE_V2);
        } else {
            buffer.extend_from_slice(PREAMBLE_V1);
        }

        if let Some(ref uuid) = settings.uuid {
            let _ = write!(buffer, "UUID={}\r\n", uuid);
        } else {
            let _ = write!(buffer, "UUID={:X}\r\n", Uuid::new_v4().to_hyphenated());
        }

        let _ = write!(
            buffer,
            "Creation Program=GStreamer MCC Encoder {}\r\n",
            env!("CARGO_PKG_VERSION")
        );

        if let Some(ref creation_date) = settings.creation_date {
            let creation_date = Utc
                .ymd(
                    creation_date.get_year() as i32,
                    creation_date.get_month() as u32,
                    creation_date.get_day_of_month() as u32,
                )
                .and_hms(
                    creation_date.get_hour() as u32,
                    creation_date.get_minute() as u32,
                    creation_date.get_seconds() as u32,
                )
                .with_timezone(&FixedOffset::east(
                    (creation_date.get_utc_offset() / 1_000_000) as i32,
                ));

            let _ = write!(
                buffer,
                "Creation Date={}\r\n",
                creation_date.format("%A, %B %d, %Y")
            );
            let _ = write!(
                buffer,
                "Creation Time={}\r\n",
                creation_date.format("%H:%M:%S")
            );
        } else {
            let creation_date = Local::now();
            let _ = write!(
                buffer,
                "Creation Date={}\r\n",
                creation_date.format("%A, %B %d, %Y")
            );
            let _ = write!(
                buffer,
                "Creation Time={}\r\n",
                creation_date.format("%H:%M:%S")
            );
        }

        if *framerate.denom() == 1 {
            let _ = write!(buffer, "Time Code Rate={}\r\n", *framerate.numer());
        } else {
            assert_eq!(*framerate.denom(), 1001);
            let _ = write!(buffer, "Time Code Rate={}DF\r\n", *framerate.numer() / 1000);
        }
        let _ = write!(buffer, "\r\n");

        Ok(())
    }

    fn encode_payload(outbuf: &mut Vec<u8>, mut slice: &[u8]) {
        while !slice.is_empty() {
            if slice.starts_with(
                [
                    0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA,
                    0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00,
                    0x00,
                ]
                .as_ref(),
            ) {
                outbuf.push(b'O');
                slice = &slice[27..];
            } else if slice.starts_with(
                [
                    0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA,
                    0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                ]
                .as_ref(),
            ) {
                outbuf.push(b'N');
                slice = &slice[24..];
            } else if slice.starts_with(
                [
                    0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA,
                    0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                ]
                .as_ref(),
            ) {
                outbuf.push(b'M');
                slice = &slice[21..];
            } else if slice.starts_with(
                [
                    0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA,
                    0x00, 0x00, 0xFA, 0x00, 0x00,
                ]
                .as_ref(),
            ) {
                outbuf.push(b'L');
                slice = &slice[18..];
            } else if slice.starts_with(
                [
                    0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA,
                    0x00, 0x00,
                ]
                .as_ref(),
            ) {
                outbuf.push(b'K');
                slice = &slice[15..];
            } else if slice.starts_with(
                [
                    0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                ]
                .as_ref(),
            ) {
                outbuf.push(b'J');
                slice = &slice[12..];
            } else if slice
                .starts_with([0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00].as_ref())
            {
                outbuf.push(b'I');
                slice = &slice[9..];
            } else if slice.starts_with([0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00].as_ref()) {
                outbuf.push(b'H');
                slice = &slice[6..];
            } else if slice.starts_with([0xFA, 0x00, 0x00].as_ref()) {
                outbuf.push(b'G');
                slice = &slice[3..];
            } else if slice.starts_with([0xFB, 0x80, 0x80].as_ref()) {
                outbuf.push(b'P');
                slice = &slice[3..];
            } else if slice.starts_with([0xFC, 0x80, 0x80].as_ref()) {
                outbuf.push(b'Q');
                slice = &slice[3..];
            } else if slice.starts_with([0xFD, 0x80, 0x80].as_ref()) {
                outbuf.push(b'R');
                slice = &slice[3..];
            } else if slice.starts_with([0x96, 0x69].as_ref()) {
                outbuf.push(b'S');
                slice = &slice[2..];
            } else if slice.starts_with([0x61, 0x01].as_ref()) {
                outbuf.push(b'T');
                slice = &slice[2..];
            } else if slice.starts_with([0xE1, 0x00, 0x00, 0x00].as_ref()) {
                outbuf.push(b'U');
                slice = &slice[4..];
            } else if slice[0] == 0x00 {
                outbuf.push(b'Z');
                slice = &slice[1..];
            } else {
                let _ = write!(outbuf, "{:02X}", slice[0]);
                slice = &slice[1..];
            }
        }
    }

    fn generate_caption(
        &self,
        element: &gst::Element,
        state: &State,
        buffer: &gst::Buffer,
        outbuf: &mut Vec<u8>,
    ) -> Result<(), gst::FlowError> {
        let meta = buffer
            .get_meta::<gst_video::VideoTimeCodeMeta>()
            .ok_or_else(|| {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Stream with timecodes on each buffer required"]
                );

                gst::FlowError::Error
            })?;

        let _ = write!(outbuf, "{}\t", meta.get_tc());

        let map = buffer.map_readable().map_err(|_| {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;

        let len = map.len();
        if len >= 256 {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["Too big buffer: {}", map.len()]
            );

            return Err(gst::FlowError::Error);
        }
        let len = len as u8;

        match state.format {
            Some(Format::Cea608) => {
                let _ = write!(outbuf, "6102{:02X}", len);
            }
            Some(Format::Cea708Cdp) => {
                let _ = write!(outbuf, "T{:02X}", len);
            }
            _ => return Err(gst::FlowError::NotNegotiated),
        };

        let checksum = map.iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
        Self::encode_payload(outbuf, &*map);

        if checksum == 0 {
            outbuf.push(b'Z');
        } else {
            let _ = write!(outbuf, "{:02X}", checksum);
        }

        outbuf.extend_from_slice(b"\r\n".as_ref());

        Ok(())
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();

        let mut outbuf = Vec::new();
        if state.need_headers {
            state.need_headers = false;
            self.generate_headers(&*state, &mut outbuf)?;
        }

        self.generate_caption(element, &*state, &buffer, &mut outbuf)?;

        let mut buf = gst::Buffer::from_mut_slice(outbuf);
        buffer
            .copy_into(buf.get_mut().unwrap(), gst::BUFFER_COPY_METADATA, 0, None)
            .expect("Failed to copy buffer metadata");

        drop(state);
        self.srcpad.push(buf)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(ev) => {
                let caps = ev.get_caps();
                let s = caps.get_structure(0).unwrap();
                let framerate = match s.get_some::<gst::Fraction>("framerate") {
                    Ok(framerate) => framerate,
                    Err(structure::GetError::FieldNotFound { .. }) => {
                        gst_error!(CAT, obj: pad, "Caps without framerate");
                        return false;
                    }
                    err => panic!("MccEnc::sink_event caps: {:?}", err),
                };

                let mut state = self.state.lock().unwrap();
                if s.get_name() == "closedcaption/x-cea-608" {
                    state.format = Some(Format::Cea608);
                } else {
                    state.format = Some(Format::Cea708Cdp);
                }
                drop(state);

                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-mcc")
                    .field(
                        "version",
                        if framerate == gst::Fraction::new(60000, 1001) {
                            &2i32
                        } else {
                            &1i32
                        },
                    )
                    .build();
                self.srcpad.push_event(gst::Event::new_caps(&caps).build())
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

impl ObjectSubclass for MccEnc {
    const NAME: &'static str = "RsMccEnc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

        MccEnc::set_pad_functions(&sinkpad, &srcpad);

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Mcc Encoder",
            "Encoder/ClosedCaption",
            "Encodes MCC Closed Caption Files",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();

            let framerates = gst::List::new(&[
                &gst::Fraction::new(24, 1),
                &gst::Fraction::new(25, 1),
                &gst::Fraction::new(30000, 1001),
                &gst::Fraction::new(30, 1),
                &gst::Fraction::new(50, 1),
                &gst::Fraction::new(60000, 1001),
                &gst::Fraction::new(60, 1),
            ]);

            let s = gst::Structure::builder("closedcaption/x-cea-708")
                .field("format", &"cdp")
                .field("framerate", &framerates)
                .build();
            caps.append_structure(s);

            let s = gst::Structure::builder("closedcaption/x-cea-608")
                .field("format", &"s334-1a")
                .field("framerate", &framerates)
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
        klass.add_pad_template(sink_pad_template);

        let caps = gst::Caps::builder("application/x-mcc").build();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for MccEnc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("uuid", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.uuid = value.get().expect("type checked upstream");
            }
            subclass::Property("creation-date", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.creation_date = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("uuid", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.uuid.to_value())
            }
            subclass::Property("creation-date", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.creation_date.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for MccEnc {
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
    gst::Element::register(
        Some(plugin),
        "mccenc",
        gst::Rank::Primary,
        MccEnc::get_type(),
    )
}
