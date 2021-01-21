// Copyright (C) 2020 Matthew Waters <matthew@centricular.com>
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

use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_trace, gst_warning};
use gst_base::subclass::prelude::*;

use byteorder::{BigEndian, ByteOrder};

use once_cell::sync::Lazy;

use std::fmt;
use std::sync::Mutex;
use std::u64;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ccdetect",
        gst::DebugColorFlags::empty(),
        Some("Closed Caption Detection"),
    )
});

const DEFAULT_WINDOW: u64 = 10 * gst::SECOND_VAL;
const DEFAULT_CC608: bool = false;
const DEFAULT_CC708: bool = false;

#[derive(Debug, Clone, Copy)]
struct Settings {
    pub window: u64,
    pub cc608: bool,
    pub cc708: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            window: DEFAULT_WINDOW,
            cc608: DEFAULT_CC608,
            cc708: DEFAULT_CC708,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CCFormat {
    Cc708Cdp,
    Cc708CcData,
}

#[derive(Debug, Clone, Copy)]
struct State {
    format: CCFormat,
    last_cc608_change: gst::ClockTime,
    last_cc708_change: gst::ClockTime,
}

pub struct CCDetect {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

#[derive(Debug, Clone, Copy)]
struct CCPacketContents {
    cc608: bool,
    cc708: bool,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy)]
enum ParseErrorCode {
    WrongLength,
    WrongMagicSequence,
    WrongLayout,
}

#[derive(Debug, Clone)]
struct ParseError {
    code: ParseErrorCode,
    byte: usize,
    msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?} at byte {}: {}", self.code, self.byte, self.msg)
    }
}

impl std::error::Error for ParseError {}

impl CCDetect {
    fn detect_cc_data(data: &[u8]) -> Result<CCPacketContents, ParseError> {
        if data.len() % 3 != 0 {
            gst_warning!(CAT, "cc_data length is not a multiple of 3, truncating");
        }

        /* logic from ccconverter */
        let mut started_ccp = false;
        let mut have_cc608 = false;
        let mut have_cc708 = false;
        for (i, triple) in data.chunks_exact(3).enumerate() {
            let cc_valid = (triple[0] & 0x04) == 0x04;
            let cc_type = triple[0] & 0x03;
            gst_trace!(
                CAT,
                "triple:{} have ccp:{} 608:{} 708:{} data:{:02x},{:02x},{:02x} cc_valid:{} cc_type:{:02b}",
                i * 3,
                started_ccp,
                have_cc608,
                have_cc708,
                triple[0],
                triple[1],
                triple[2],
                cc_valid,
                cc_type
            );

            if !started_ccp && cc_valid {
                if cc_type == 0x00 {
                    if triple[1] != 0x80 || triple[2] != 0x80 {
                        have_cc608 = true;
                    }
                    continue;
                } else if cc_type == 0x01 {
                    if triple[1] != 0x80 || triple[2] != 0x80 {
                        have_cc708 = true;
                    }
                    continue;
                }
            }

            if cc_type & 0b10 == 0b10 {
                started_ccp = true;
            }

            if !cc_valid {
                continue;
            }

            if cc_type == 0x00 || cc_type == 0x01 {
                return Err(ParseError {
                    code: ParseErrorCode::WrongLayout,
                    byte: data.len() - i * 3,
                    msg: String::from("Invalid cc_data. cea608 bytes after cea708"),
                });
            }

            have_cc708 = true;
        }

        Ok(CCPacketContents {
            cc608: have_cc608,
            cc708: have_cc708,
        })
    }

    fn detect_cdp(mut data: &[u8]) -> Result<CCPacketContents, ParseError> {
        /* logic from ccconverter */
        let data_len = data.len();

        if data.len() < 11 {
            return Err(ParseError {
                code: ParseErrorCode::WrongLength,
                byte: data_len - data.len(),
                msg: format!(
                    "cdp packet too short {}. expected at least {}",
                    data.len(),
                    11
                ),
            });
        }

        if 0x9669 != BigEndian::read_u16(&data[..2]) {
            return Err(ParseError {
                code: ParseErrorCode::WrongMagicSequence,
                byte: data_len - data.len(),
                msg: String::from("cdp packet does not have initial magic bytes of 0x9669"),
            });
        }
        data = &data[2..];

        if (data[0] as usize) != data_len {
            return Err(ParseError {
                code: ParseErrorCode::WrongLength,
                byte: data_len - data.len(),
                msg: format!(
                    "advertised cdp packet length {} does not match length of data {}",
                    data[0], data_len
                ),
            });
        }
        data = &data[1..];

        /* skip framerate value */
        data = &data[1..];

        let flags = data[0];
        data = &data[1..];

        if flags & 0x40 == 0 {
            /* no cc_data */
            return Ok(CCPacketContents {
                cc608: false,
                cc708: false,
            });
        }

        /* skip sequence counter */
        data = &data[2..];

        /* timecode present? */
        if flags & 0x80 == 0x80 {
            if data.len() < 5 {
                return Err(ParseError {
                    code: ParseErrorCode::WrongLength,
                    byte: data_len - data.len(),
                    msg: String::from("cdp packet signals a timecode but is not large enough to contain a timecode")
                });
            }
            data = &data[5..];
        }

        /* cc_data */
        if data.len() < 2 {
            return Err(ParseError {
                code: ParseErrorCode::WrongLength,
                byte: data_len - data.len(),
                msg: String::from(
                    "cdp packet signals cc_data but is not large enough to contain cc_data",
                ),
            });
        }

        if data[0] != 0x72 {
            return Err(ParseError {
                code: ParseErrorCode::WrongMagicSequence,
                byte: data_len - data.len(),
                msg: String::from("ccp is missing start code 0x72"),
            });
        }
        data = &data[1..];

        let cc_count = data[0];
        data = &data[1..];
        if cc_count & 0xe0 != 0xe0 {
            return Err(ParseError {
                code: ParseErrorCode::WrongMagicSequence,
                byte: data_len - data.len(),
                msg: format!("reserved bits are not 0xe0, found {:02x}", cc_count & 0xe0),
            });
        }
        let cc_count = cc_count & 0x1f;
        let len = 3 * cc_count as usize;

        if len > data.len() {
            return Err(ParseError {
                code: ParseErrorCode::WrongLength,
                byte: data_len - data.len(),
                msg: String::from("cc_data length extends past the end of the cdp packet"),
            });
        }

        /* TODO: validate checksum */

        Self::detect_cc_data(&data[..len])
    }

    fn detect(format: CCFormat, data: &[u8]) -> Result<CCPacketContents, ParseError> {
        match format {
            CCFormat::Cc708CcData => Self::detect_cc_data(data),
            CCFormat::Cc708Cdp => Self::detect_cdp(data),
        }
    }

    fn maybe_update_properties(
        &self,
        element: &super::CCDetect,
        ts: gst::ClockTime,
        cc_packet: CCPacketContents,
    ) -> Result<(), gst::FlowError> {
        let mut notify_cc608 = false;
        let mut notify_cc708 = false;

        {
            let mut settings = self.settings.lock().unwrap();

            let mut state_guard = self.state.lock().unwrap();
            let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

            gst_trace!(
                CAT,
                "packet contains {:?} current settings {:?} and state {:?}",
                cc_packet,
                settings,
                state
            );

            if cc_packet.cc608 != settings.cc608 {
                if state.last_cc608_change.is_none()
                    || ts - state.last_cc608_change > settings.window.into()
                {
                    settings.cc608 = cc_packet.cc608;
                    state.last_cc608_change = ts;
                    notify_cc608 = true;
                }
            } else {
                state.last_cc608_change = ts;
            }

            if cc_packet.cc708 != settings.cc708 {
                if state.last_cc708_change.is_none()
                    || ts - state.last_cc708_change > settings.window.into()
                {
                    settings.cc708 = cc_packet.cc708;
                    state.last_cc708_change = ts;
                    notify_cc708 = true;
                }
            } else {
                state.last_cc708_change = ts;
            }

            gst_trace!(CAT, "changed to settings {:?} state {:?}", settings, state);
        }

        if notify_cc608 {
            element.notify("cc608");
        }
        if notify_cc708 {
            element.notify("cc708");
        }

        Ok(())
    }
}

impl ObjectSubclass for CCDetect {
    const NAME: &'static str = "CCDetect";
    type Type = super::CCDetect;
    type ParentType = gst_base::BaseTransform;
    type Interfaces = ();
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::object_subclass!();

    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(None),
        }
    }
}

impl ObjectImpl for CCDetect {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::uint64(
                    "window",
                    "Window",
                    "Window of time (in ns) to determine if captions exist in the stream",
                    0,
                    u64::MAX,
                    DEFAULT_WINDOW,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::boolean(
                    "cc608",
                    "cc608",
                    "Whether CEA608 captions (CC1/CC3) have been detected",
                    DEFAULT_CC608,
                    glib::ParamFlags::READABLE,
                ),
                glib::ParamSpec::boolean(
                    "cc708",
                    "cc608",
                    "Whether CEA708 captions (cc_data) have been detected",
                    DEFAULT_CC708,
                    glib::ParamFlags::READABLE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.get_name() {
            "window" => {
                let mut settings = self.settings.lock().unwrap();
                settings.window = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.get_name() {
            "window" => {
                let settings = self.settings.lock().unwrap();
                settings.window.to_value()
            }
            "cc608" => {
                let settings = self.settings.lock().unwrap();
                settings.cc608.to_value()
            }
            "cc708" => {
                let settings = self.settings.lock().unwrap();
                settings.cc708.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for CCDetect {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Closed Caption Detect",
                "Filter/Video/ClosedCaption/Detect",
                "Detect if valid closed captions are present in a stream",
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
                let s = gst::Structure::builder("closedcaption/x-cea-708")
                    .field("format", &gst::List::new(&[&"cc_data", &"cdp"]))
                    .build();
                caps.append_structure(s);
            }

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

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
}

impl BaseTransformImpl for CCDetect {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;

    fn transform_ip_passthrough(
        &self,
        element: &Self::Type,
        buf: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;

        if buf.get_pts().is_none() {
            gst::element_error!(
                element,
                gst::ResourceError::Read,
                ["Input buffers must have valid timestamps"]
            );
            return Err(gst::FlowError::Error);
        }

        let format = {
            let mut state_guard = self.state.lock().unwrap();
            let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;
            state.format
        };

        let cc_packet = match Self::detect(format, map.as_slice()) {
            Ok(v) => v,
            Err(e) => {
                gst_warning!(CAT, "{}", &e.to_string());
                gst::element_warning!(element, gst::StreamError::Decode, [&e.to_string()]);
                CCPacketContents {
                    cc608: false,
                    cc708: false,
                }
            }
        };

        self.maybe_update_properties(element, buf.get_pts(), cc_packet)
            .map_err(|_| gst::FlowError::Error)?;

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, element: &Self::Type, event: gst::Event) -> bool {
        match event.view() {
            gst::event::EventView::Gap(gap) => {
                let _ = self.maybe_update_properties(
                    element,
                    gap.get().0,
                    CCPacketContents {
                        cc608: false,
                        cc708: false,
                    },
                );
                self.parent_sink_event(element, event)
            }
            _ => self.parent_sink_event(element, event),
        }
    }

    fn set_caps(
        &self,
        _element: &Self::Type,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        if incaps != outcaps {
            return Err(gst::loggable_error!(
                CAT,
                "Input and output caps are not the same"
            ));
        }

        let s = incaps
            .get_structure(0)
            .ok_or_else(|| gst::loggable_error!(CAT, "Failed to parse input caps"))?;
        let format_str = s
            .get::<&str>("format")
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse input caps"))?
            .ok_or_else(|| gst::loggable_error!(CAT, "Failed to parse input caps"))?;
        let cc_format = match format_str {
            "cdp" => CCFormat::Cc708Cdp,
            "cc_data" => CCFormat::Cc708CcData,
            _ => return Err(gst::loggable_error!(CAT, "Failed to parse input caps")),
        };

        *self.state.lock().unwrap() = Some(State {
            format: cc_format,
            last_cc608_change: gst::ClockTime::none(),
            last_cc708_change: gst::ClockTime::none(),
        });

        Ok(())
    }

    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        // Drop state
        let _ = self.state.lock().unwrap().take();

        Ok(())
    }
}
