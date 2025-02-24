// Copyright (C) 2023 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use cea708_types::{CCDataParser, Service};
use cea708_types::{CCDataWriter, DTVCCPacket, Framerate};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use once_cell::sync::Lazy;

#[derive(Default, Copy, Clone, PartialEq, Eq)]
enum CeaFormat {
    S334_1a,
    Cea608Field0,
    Cea608Field1,
    CcData,
    #[default]
    Cdp,
}

impl CeaFormat {
    fn from_caps(caps: &gst::CapsRef) -> Result<Self, gst::LoggableError> {
        let structure = caps.structure(0).expect("Caps has no structure");
        match structure.name().as_str() {
            "closedcaption/x-cea-608" => match structure.get::<&str>("format") {
                Ok("raw") => {
                    if structure.has_field("field") {
                        match structure.get::<i32>("field") {
                            Ok(0) => Ok(CeaFormat::Cea608Field0),
                            Ok(1) => Ok(CeaFormat::Cea608Field1),
                            _ => Err(gst::loggable_error!(
                                CAT,
                                "unknown \'field\' value in caps, {caps:?}"
                            )),
                        }
                    } else {
                        Ok(CeaFormat::Cea608Field0)
                    }
                }
                Ok("s334-1a") => Ok(CeaFormat::S334_1a),
                v => Err(gst::loggable_error!(
                    CAT,
                    "unknown or missing \'format\' value {v:?} in caps, {caps:?}"
                )),
            },
            "closedcaption/x-cea-708" => match structure.get::<&str>("format") {
                Ok("cdp") => Ok(CeaFormat::Cdp),
                Ok("cc_data") => Ok(CeaFormat::CcData),
                v => Err(gst::loggable_error!(
                    CAT,
                    "unknown or missing \'format\' value {v:?} in caps, {caps:?}"
                )),
            },
            name => Err(gst::loggable_error!(
                CAT,
                "Unknown caps name: {name} in caps"
            )),
        }
    }
}

fn fps_from_caps(caps: &gst::CapsRef) -> Result<Framerate, gst::LoggableError> {
    let structure = caps.structure(0).expect("Caps has no structure");
    let framerate = structure
        .get::<gst::Fraction>("framerate")
        .map_err(|_| gst::loggable_error!(CAT, "Caps do not contain framerate"))?;
    Ok(Framerate::new(
        framerate.numer() as u32,
        framerate.denom() as u32,
    ))
}

struct State {
    out_format: CeaFormat,
    fps: Option<Framerate>,
    dtvcc_seq_no: u8,
    writer: CCDataWriter,
    n_frames: u64,
}

impl Default for State {
    fn default() -> Self {
        let mut writer = CCDataWriter::default();
        writer.set_output_padding(true);
        writer.set_output_cea608_padding(true);
        Self {
            out_format: CeaFormat::default(),
            fps: None,
            dtvcc_seq_no: 0,
            writer,
            n_frames: 0,
        }
    }
}

#[derive(Default)]
pub struct Cea708Mux {
    state: Mutex<State>,
}

pub(crate) static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cea708mux",
        gst::DebugColorFlags::empty(),
        Some("CEA-708 Mux Element"),
    )
});

impl AggregatorImpl for Cea708Mux {
    fn aggregate(&self, timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let fps = state.fps.unwrap();
        let src_segment = self
            .obj()
            .src_pad()
            .segment()
            .downcast::<gst::ClockTime>()
            .expect("Non-TIME segment");

        let start_running_time =
            if src_segment.position().is_none() || src_segment.position() < src_segment.start() {
                src_segment.start().unwrap()
            } else {
                src_segment.position().unwrap()
            };

        let duration = 1_000_000_000
            .mul_div_round(fps.denom() as u64, fps.numer() as u64)
            .unwrap()
            .nseconds();
        let end_running_time = start_running_time + duration;
        let mut need_data = false;
        let mut all_eos = true;
        gst::debug!(
            CAT,
            imp = self,
            "Aggregating for start time {} end {} timeout {}",
            start_running_time.display(),
            end_running_time.display(),
            timeout
        );

        let sinkpads = self.obj().sink_pads();

        // phase 1, ensure all pads have the relevant data (or a timeout)
        for pad in sinkpads.iter().map(|pad| {
            pad.downcast_ref::<super::Cea708MuxSinkPad>()
                .expect("Not a Cea708MuxSinkPad?!")
        }) {
            let mut pad_state = pad.imp().pad_state.lock().unwrap();

            if pad.is_eos() {
                if pad_state.pending_buffer.is_some() {
                    all_eos = false;
                }
                continue;
            }
            all_eos = false;

            let buffer = if let Some(buffer) = pad.peek_buffer() {
                buffer
            } else {
                need_data = true;
                continue;
            };

            let Ok(segment) = pad.segment().downcast::<gst::ClockTime>() else {
                drop(pad_state);
                drop(state);
                self.post_error_message(gst::error_msg!(
                    gst::CoreError::Clock,
                    ["Incoming segment not in TIME format"]
                ));
                return Err(gst::FlowError::Error);
            };
            let Some(buffer_start_ts) = segment.to_running_time(buffer.pts()) else {
                drop(pad_state);
                drop(state);
                self.post_error_message(gst::error_msg!(
                    gst::CoreError::Clock,
                    ["Incoming buffer does not contain valid PTS"]
                ));
                return Err(gst::FlowError::Error);
            };
            if buffer_start_ts >= end_running_time {
                // buffer is not for this output time, skip
                continue;
            }
            let duration = buffer.duration().unwrap_or(gst::ClockTime::ZERO);
            let buffer_end_ts = buffer_start_ts + duration;
            if start_running_time.saturating_sub(buffer_end_ts) > gst::ClockTime::ZERO {
                // need to wait for the next input buffer which might need to be part of this
                // output buffer.
                need_data = true;
            }

            let Ok(mapped) = buffer.map_readable() else {
                drop(pad_state);
                drop(state);
                self.post_error_message(gst::error_msg!(
                    gst::CoreError::Clock,
                    ["Failed to map input buffer"]
                ));
                return Err(gst::FlowError::Error);
            };

            gst::debug!(CAT, obj = pad, "Parsing input buffer {buffer:?}");
            let in_format = pad_state.format;
            match in_format {
                CeaFormat::CcData => {
                    // gst's cc_data does not contain the 2 byte header contained in the CEA-708
                    // specification
                    let mut cc_data = vec![0; 2];
                    // reserved | process_cc_data | length
                    cc_data[0] = 0x80 | 0x40 | ((mapped.len() / 3) & 0x1f) as u8;
                    cc_data[1] = 0xFF;
                    cc_data.extend(mapped.iter());
                    pad_state.ccp_parser.push(&cc_data).unwrap();

                    if let Some(cea608) = pad_state.ccp_parser.cea608() {
                        for pair in cea608 {
                            state.writer.push_cea608(*pair);
                        }
                    }
                }
                _ => unreachable!(),
            }
            pad_state.pending_buffer = Some(buffer.clone());
            pad.drop_buffer();
        }

        if need_data && !timeout {
            return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
        }
        if all_eos {
            return Err(gst::FlowError::Eos);
        }

        self.obj()
            .selected_samples(start_running_time, None, duration, None);

        // phase 2: write stored data into output packet
        let mut services = HashMap::new();

        for pad in sinkpads.iter().map(|pad| {
            pad.downcast_ref::<super::Cea708MuxSinkPad>()
                .expect("Not a Cea708MuxSinkPad?!")
        }) {
            let mut pad_state = pad.imp().pad_state.lock().unwrap();
            pad_state.pending_buffer = None;
            let in_format = pad_state.format;
            #[allow(clippy::single_match)]
            match in_format {
                CeaFormat::CcData => {
                    while let Some(packet) = pad_state.ccp_parser.pop_packet() {
                        for service in packet.services() {
                            let service_no = service.number();
                            if service.number() == 0 {
                                // skip null service
                                continue;
                            }
                            let new_service = services
                                .entry(service_no)
                                .or_insert_with_key(|&n| Service::new(n));

                            let mut overflowed = false;
                            if let Some(pending_codes) =
                                pad_state.pending_services.get_mut(&service.number())
                            {
                                while let Some(code) = pending_codes.pop_front() {
                                    match new_service.push_code(&code) {
                                        Ok(_) => (),
                                        Err(cea708_types::WriterError::WouldOverflow(_)) => {
                                            overflowed = true;
                                            pending_codes.push_front(code);
                                            break;
                                        }
                                        Err(cea708_types::WriterError::ReadOnly) => unreachable!(),
                                    }
                                }
                            }

                            for code in service.codes() {
                                gst::trace!(
                                    CAT,
                                    obj = pad,
                                    "Handling service {} code {code:?}",
                                    service.number()
                                );
                                if overflowed {
                                    pad_state
                                        .pending_services
                                        .entry(service.number())
                                        .or_default()
                                        .push_back(code.clone());
                                } else {
                                    match new_service.push_code(code) {
                                        Ok(_) => (),
                                        Err(cea708_types::WriterError::WouldOverflow(_)) => {
                                            overflowed = true;
                                            pad_state
                                                .pending_services
                                                .entry(service.number())
                                                .or_default()
                                                .push_back(code.clone());
                                        }
                                        Err(cea708_types::WriterError::ReadOnly) => unreachable!(),
                                    }
                                }
                            }
                        }
                    }
                    for (service_no, pending_codes) in pad_state.pending_services.iter_mut() {
                        let new_service = services
                            .entry(*service_no)
                            .or_insert_with_key(|&n| Service::new(n));

                        while let Some(code) = pending_codes.pop_front() {
                            match new_service.push_code(&code) {
                                Ok(_) => (),
                                Err(cea708_types::WriterError::WouldOverflow(_)) => {
                                    pending_codes.push_front(code);
                                    break;
                                }
                                Err(cea708_types::WriterError::ReadOnly) => unreachable!(),
                            }
                        }
                    }
                }
                _ => (),
            }
        }

        let mut packet = DTVCCPacket::new(state.dtvcc_seq_no & 0x3);

        for (_service_no, service) in services.into_iter().filter(|(_, s)| !s.codes().is_empty()) {
            // FIXME: handle needing to split services
            gst::trace!(
                CAT,
                imp = self,
                "Adding service {} to packet",
                service.number()
            );
            packet.push_service(service).unwrap();
            if packet.sequence_no() == state.dtvcc_seq_no & 0x3 {
                state.dtvcc_seq_no = state.dtvcc_seq_no.wrapping_add(1);
            }
        }

        let mut data = vec![];
        state.writer.push_packet(packet);
        let _ = state.writer.write(fps, &mut data);
        state.n_frames += 1;
        drop(state);

        // remove 2 byte header that our cc_data format does not use
        let ret = if data.len() > 2 {
            let data = data.split_off(2);
            gst::trace!(CAT, "generated data {data:x?}");
            let mut buf = gst::Buffer::from_mut_slice(data);
            {
                let buf = buf.get_mut().unwrap();
                buf.set_pts(Some(start_running_time));
                if start_running_time < end_running_time {
                    buf.set_duration(Some(end_running_time - start_running_time));
                }
            }

            self.finish_buffer(buf)
        } else {
            self.obj().src_pad().push_event(
                gst::event::Gap::builder(start_running_time)
                    .duration(duration)
                    .build(),
            );
            Ok(gst::FlowSuccess::Ok)
        };

        self.obj().set_position(end_running_time);

        ret
    }

    fn peek_next_sample(&self, pad: &gst_base::AggregatorPad) -> Option<gst::Sample> {
        let cea_pad = pad
            .downcast_ref::<super::Cea708MuxSinkPad>()
            .expect("Not a Cea708MuxSinkPad?!");
        let pad_state = cea_pad.imp().pad_state.lock().unwrap();
        pad_state
            .pending_buffer
            .as_ref()
            .zip(cea_pad.current_caps())
            .map(|(buffer, caps)| {
                gst::Sample::builder()
                    .buffer(buffer)
                    .segment(&cea_pad.segment())
                    .caps(&caps)
                    .build()
            })
    }

    fn next_time(&self) -> Option<gst::ClockTime> {
        self.obj().simple_get_next_time()
    }

    fn flush(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let format = state.out_format;
        let fps = state.fps;
        *state = State::default();
        state.writer.set_output_padding(true);
        state.writer.set_output_cea608_padding(true);
        state.out_format = format;
        state.fps = fps;
        state.n_frames = 0;

        self.obj()
            .src_pad()
            .segment()
            .set_position(None::<gst::ClockTime>);

        Ok(gst::FlowSuccess::Ok)
    }

    fn negotiated_src_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let mut state = self.state.lock().unwrap();
        state.out_format = CeaFormat::from_caps(caps.as_ref())?;
        state.fps = Some(fps_from_caps(caps.as_ref())?);
        Ok(())
    }

    fn sink_event(&self, pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
        let mux_pad = pad
            .downcast_ref::<super::Cea708MuxSinkPad>()
            .expect("Not a Cea708MuxSinkPad");
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);
        #[allow(clippy::single_match)]
        match event.view() {
            EventView::Caps(event) => {
                let mut state = mux_pad.imp().pad_state.lock().unwrap();
                state.format = match CeaFormat::from_caps(event.caps()) {
                    Ok(format) => format,
                    Err(err) => {
                        err.log_with_imp(self);
                        return false;
                    }
                };
            }
            _ => (),
        }

        self.parent_sink_event(pad, event)
    }

    fn clip(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        let Some(pts) = buffer.pts() else {
            return Some(buffer);
        };
        let segment = aggregator_pad.segment();
        segment
            .downcast_ref::<gst::ClockTime>()
            .map(|segment| segment.clip(pts, pts))
            .map(|_| buffer)
    }
}

impl ElementImpl for Cea708Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "CEA-708 Mux",
                "Muxer",
                "Combines multiple CEA-708 streams",
                "Matthew Waters <matthew@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let framerates = gst::List::new([
                gst::Fraction::new(60, 1),
                gst::Fraction::new(60000, 1001),
                gst::Fraction::new(50, 1),
                gst::Fraction::new(30, 1),
                gst::Fraction::new(30000, 1001),
                gst::Fraction::new(25, 1),
                gst::Fraction::new(24, 1),
                gst::Fraction::new(24000, 1001),
            ]);

            let src_pad_template = gst::PadTemplate::builder(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &[
                    // TODO: handle CDP and s334-1a output
                    /*gst::Structure::builder("closedcaption/x-cea-708")
                    .field("format", "cdp")
                    .field("framerate", framerates)
                    .build(),*/
                    gst::Structure::builder("closedcaption/x-cea-708")
                        .field("format", "cc_data")
                        .field("framerate", framerates.clone())
                        .build(),
                    /*gst::Structure::builder("closedcaption/x-cea-608")
                    .field("format", "s334-1a")
                    .field("framerate", framerates)
                    .build()*/
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .gtype(gst_base::AggregatorPad::static_type())
            .build()
            .unwrap();

            let sink_pad_template = gst::PadTemplate::builder(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &[
                    // TODO: handle 608-only or cdp input
                    /*
                    gst::Structure::builder("closedcaption/x-cea-608")
                        .field("format", "s334-1a")
                        .build(),
                    gst::Structure::builder("closedcaption/x-cea-608")
                        .field("format", "raw")
                        .field("field", gst::List::new([0, 1]))
                        .build(),*/
                    gst::Structure::builder("closedcaption/x-cea-708")
                        .field("format", "cc_data")
                        .field("framerate", framerates)
                        .build(),
                    /*gst::Structure::builder("closedcaption/x-cea-708")
                    .field("framerate", framerates)
                    .field("format", "cdp")
                    .build(),*/
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .gtype(super::Cea708MuxSinkPad::static_type())
            .build()
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
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                state.writer.set_output_padding(true);
                state.writer.set_output_cea608_padding(true);
            }
            _ => (),
        }

        let ret = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                state.writer.set_output_padding(true);
                state.writer.set_output_cea608_padding(true);
            }
            _ => (),
        }

        Ok(ret)
    }
}

impl GstObjectImpl for Cea708Mux {}

impl ObjectImpl for Cea708Mux {}

#[glib::object_subclass]
impl ObjectSubclass for Cea708Mux {
    const NAME: &'static str = "GstCea708Mux";
    type Type = super::Cea708Mux;
    type ParentType = gst_base::Aggregator;
}

struct PadState {
    format: CeaFormat,
    ccp_parser: CCDataParser,
    pending_services: HashMap<u8, VecDeque<cea708_types::tables::Code>>,
    pending_buffer: Option<gst::Buffer>,
}

impl Default for PadState {
    fn default() -> Self {
        let mut ccp_parser = CCDataParser::default();
        ccp_parser.handle_cea608();
        Self {
            format: CeaFormat::default(),
            ccp_parser,
            pending_services: HashMap::default(),
            pending_buffer: None,
        }
    }
}

#[derive(Default)]
pub struct Cea708MuxSinkPad {
    pad_state: Mutex<PadState>,
}

impl Cea708MuxSinkPad {}

impl AggregatorPadImpl for Cea708MuxSinkPad {
    fn flush(
        &self,
        _aggregator: &gst_base::Aggregator,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.pad_state.lock().unwrap();
        state.ccp_parser.flush();
        Ok(gst::FlowSuccess::Ok)
    }
}

impl PadImpl for Cea708MuxSinkPad {}

impl GstObjectImpl for Cea708MuxSinkPad {}

impl ObjectImpl for Cea708MuxSinkPad {}

#[glib::object_subclass]
impl ObjectSubclass for Cea708MuxSinkPad {
    const NAME: &'static str = "GstCea708MuxSinkPad";
    type Type = super::Cea708MuxSinkPad;
    type ParentType = gst_base::AggregatorPad;
}
