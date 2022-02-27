// Copyright (C) 2022 Tomasz Andrzejak <andreiltd@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use gst::{element_error, error_msg, glib, loggable_error};

use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_rtp::rtp_buffer::*;
use gst_rtp::RTPBuffer;

use once_cell::sync::Lazy;

use std::collections::HashSet;
use std::sync::{mpsc, Mutex};

use raptorq::{
    extended_source_block_symbols, ObjectTransmissionInformation, SourceBlockEncoder,
    SourceBlockEncodingPlan,
};

use crate::fecscheme::{self, DataUnitHeader, RepairPayloadId};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "raptorqenc",
        gst::DebugColorFlags::empty(),
        Some("RTP RaptorQ Encoder"),
    )
});

const DEFAULT_PROTECTED_PACKETS: u32 = 25;
const DEFAULT_REPAIR_PACKETS: u32 = 5;
const DEFAULT_REPAIR_WINDOW: u32 = 50;
const DEFAULT_SYMBOL_SIZE: u32 = 1408;
const DEFAULT_MTU: u32 = 1400;
const DEFAULT_PT: u32 = 97;

const SYMBOL_ALIGNMENT: usize = 8;

#[derive(Debug, Clone, Copy)]
struct Settings {
    protected_packets: u32,
    repair_packets: u32,
    repair_window: u32,
    symbol_size: u32,
    mtu: u32,
    pt: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            protected_packets: DEFAULT_PROTECTED_PACKETS,
            repair_packets: DEFAULT_REPAIR_PACKETS,
            repair_window: DEFAULT_REPAIR_WINDOW,
            symbol_size: DEFAULT_SYMBOL_SIZE,
            mtu: DEFAULT_MTU,
            pt: DEFAULT_PT,
        }
    }
}

type BufferTarget = (Option<gst::ClockTime>, gst::Buffer);
type BufferTrigger = (gst::ClockId, gst::Buffer);

enum SrcTaskMsg {
    Schedule(BufferTarget),
    Timeout(BufferTrigger),
    Eos,
}

#[derive(Debug, Clone)]
struct State {
    packets: Vec<gst::Buffer>,
    seqnums: Vec<u16>,
    sender: Option<mpsc::Sender<SrcTaskMsg>>,
    segment: gst::FormattedSegment<gst::ClockTime>,
    repair_packets_num: usize,
    protected_packets_num: usize,
    repair_window: usize,
    symbol_size: usize,
    symbols_per_packet: usize,
    symbols_per_block: usize,
    mtu: usize,
    pt: u8,
    seq: u16,
    ssrc: u32,
    clock_rate: Option<u32>,
    info: ObjectTransmissionInformation,
    plan: SourceBlockEncodingPlan,
}

pub struct RaptorqEnc {
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    srcpad_fec: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
    pending_timers: Mutex<HashSet<gst::ClockId>>,
}

impl RaptorqEnc {
    fn process_source_block(
        element: &super::RaptorqEnc,
        state: &mut State,
        now_pts: Option<gst::ClockTime>,
        now_dts: Option<gst::ClockTime>,
        now_rtpts: u32,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let sender = match &state.sender {
            Some(sender) => sender,
            None => return Ok(gst::FlowSuccess::Ok),
        };

        // Build Source Block, RFC6881, section 8.
        let mut source_block = Vec::with_capacity(
            state
                .symbol_size
                .checked_mul(state.symbols_per_block)
                .ok_or(gst::FlowError::NotSupported)?,
        );

        source_block.extend(state.packets.iter().flat_map(|packet| {
            // As defined in RFC6881, section 8.2.4, length indication
            // should be equal to UDP packet length without RTP header.
            let li = packet.size() - 12;
            // Value of s[i] should be equal to number of repair symbols
            // placed in each repair packet.
            let si = state.symbols_per_packet;

            gst::trace!(
                CAT,
                obj: element,
                "Source Block add ADU: si {}, li {}",
                si,
                li
            );

            let mut data = vec![0; si * state.symbol_size];

            data[0..3].copy_from_slice(
                &DataUnitHeader {
                    flow_indication: 0,
                    len_indication: li as u16,
                }
                .encode(),
            );

            let packet_map = packet.map_readable().unwrap();
            let packet_data = packet_map.as_slice();

            data[3..3 + packet.size()].copy_from_slice(packet_data);
            data
        }));

        assert_eq!(
            state.symbol_size * state.symbols_per_block,
            source_block.len()
        );

        let encoder =
            SourceBlockEncoder::with_encoding_plan2(0, &state.info, &source_block, &state.plan);

        let sbl = state.symbols_per_block;

        // Initial sequnce number in Repair Payload ID is a sequence number of
        // the first packet in the Source Block.
        let seq = state.seqnums.first().cloned().unwrap();

        // Build FEC packets as defined in RFC6881, section 8.1.3
        let repair_symbols = state.repair_packets_num * state.symbols_per_packet;

        // Delay step is used to create linearly spaced vector of delays for
        // repair packets. All the repair packets are send within repair_window
        // span from the fec srcpad thread.
        let delay_step = state
            .repair_window
            .checked_div(state.repair_packets_num)
            .unwrap_or(0);

        let delays = (1..=state.repair_packets_num)
            .map(|n| gst::ClockTime::from_mseconds((n * delay_step) as u64))
            .collect::<Vec<_>>();

        let base_time = element.base_time();
        let running_time = state.segment.to_running_time(now_pts);

        for (target_time, repair_packet) in Iterator::zip(
            delays
                .iter()
                .map(|delay| base_time.opt_add(running_time).opt_add(delay)),
            encoder
                .repair_packets(0, repair_symbols as u32)
                .chunks_exact(state.symbols_per_packet)
                .enumerate()
                .zip(&delays)
                .map(|((n, packets), delay)| {
                    let esi = packets[0].payload_id().encoding_symbol_id();

                    let payload_id = RepairPayloadId {
                        initial_sequence_num: seq,
                        source_block_len: sbl as u16,
                        encoding_symbol_id: esi,
                    }
                    .encode();

                    let fecsz = payload_id.len() + state.symbol_size * state.symbols_per_packet;
                    let mut buf = gst::Buffer::new_rtp_with_sizes(fecsz as u32, 0, 0).unwrap();

                    {
                        let buf_mut = buf.get_mut().unwrap();
                        buf_mut.set_pts(now_pts.opt_add(delay));
                        buf_mut.set_dts(now_dts.opt_add(delay));

                        let mut rtpbuf = RTPBuffer::from_buffer_writable(buf_mut).unwrap();

                        rtpbuf.set_payload_type(state.pt);
                        rtpbuf.set_seq(state.seq);
                        rtpbuf.set_marker(n == state.repair_packets_num - 1);

                        if let Some(clock_rate) = state.clock_rate {
                            let rtpdelay = delay
                                .mul_div_round(*gst::ClockTime::SECOND, clock_rate as u64)
                                .unwrap()
                                .nseconds() as u32;

                            rtpbuf.set_timestamp(now_rtpts.overflowing_add(rtpdelay).0);
                        }

                        state.seq = state.seq.overflowing_add(1).0;

                        let payload = rtpbuf.payload_mut().unwrap();
                        payload[0..payload_id.len()].copy_from_slice(&payload_id);

                        for (n, packet) in packets.iter().enumerate() {
                            let data = packet.data();
                            let start = payload_id.len() + n * data.len();

                            payload[start..start + data.len()].copy_from_slice(data);
                        }
                    }

                    buf
                }),
        ) {
            if sender
                .send(SrcTaskMsg::Schedule((target_time, repair_packet)))
                .is_err()
            {
                break;
            }
        }

        state.packets.clear();
        state.seqnums.clear();

        Ok(gst::FlowSuccess::Ok)
    }

    fn start_task(&self, element: &super::RaptorqEnc) -> Result<(), gst::LoggableError> {
        let (sender, receiver) = mpsc::channel();

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().unwrap();

        state.sender = Some(sender);
        drop(state_guard);

        let element_weak = element.downgrade();
        let pad_weak = self.srcpad_fec.downgrade();

        let mut eos = false;

        self.srcpad_fec
            .start_task(move || {
                while let Ok(msg) = receiver.recv() {
                    let pad = match pad_weak.upgrade() {
                        Some(pad) => pad,
                        None => break,
                    };

                    let element = match element_weak.upgrade() {
                        Some(element) => element,
                        None => break,
                    };

                    match msg {
                        SrcTaskMsg::Timeout((id, buf)) => {
                            let mut timers = element.imp().pending_timers.lock().unwrap();
                            let _ = timers.remove(&id);

                            let push_eos = eos && timers.is_empty();

                            drop(timers);

                            if let Err(err) = pad.push(buf) {
                                gst::element_error!(
                                    element,
                                    gst::CoreError::Pad,
                                    ["Failed to push on src FEC pad {:?}", err]
                                );

                                break;
                            }

                            if push_eos {
                                pad.push_event(gst::event::Eos::new());
                                break;
                            }
                        }
                        SrcTaskMsg::Schedule((target, buf)) => {
                            let target = match target {
                                Some(target) => target,
                                None => {
                                    // No target, push buffer immediately
                                    if let Err(err) = pad.push(buf) {
                                        gst::element_error!(
                                            element,
                                            gst::CoreError::Pad,
                                            ["Failed to push on src FEC pad {:?}", err]
                                        );
                                        break;
                                    }

                                    continue;
                                }
                            };

                            let clock = match element.clock() {
                                Some(clock) => clock,
                                None => {
                                    // No clock provided, push buffer immediately
                                    if let Err(err) = pad.push(buf) {
                                        gst::element_error!(
                                            element,
                                            gst::CoreError::Pad,
                                            ["Failed to push on src FEC pad {:?}", err]
                                        );
                                        break;
                                    }

                                    continue;
                                }
                            };

                            let timeout_sender = {
                                let state_guard = element.imp().state.lock().unwrap();
                                let state = match state_guard.as_ref() {
                                    Some(state) => state,
                                    None => break,
                                };

                                state.sender.as_ref().unwrap().clone()
                            };

                            let timeout = clock.new_single_shot_id(target);

                            let mut timers = element.imp().pending_timers.lock().unwrap();
                            timers.insert(timeout.clone().into());

                            timeout
                                .wait_async(move |_clock, _time, id| {
                                    let id = id.clone();
                                    let _ = timeout_sender.send(SrcTaskMsg::Timeout((id, buf)));
                                })
                                .expect("Failed to wait async");
                        }
                        SrcTaskMsg::Eos => {
                            if element.imp().pending_timers.lock().unwrap().is_empty() {
                                pad.push_event(gst::event::Eos::new());
                                break;
                            }

                            eos = true;
                        }
                    }
                }

                // All senders dropped or error
                let pad = match pad_weak.upgrade() {
                    Some(pad) => pad,
                    None => return,
                };

                let _ = pad.pause_task();
            })
            .map_err(|_| loggable_error!(CAT, "Failed to start pad task"))?;

        Ok(())
    }

    fn src_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &super::RaptorqEnc,
        _mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            self.start_task(element)?;
        } else {
            // element stop should be called at this point so that all mpsc
            // senders used in task are dropped, otherwise channel can deadlock
            self.srcpad_fec.stop_task()?;
        }

        Ok(())
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::RaptorqEnc,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        if buffer.size() > state.mtu {
            gst::error!(CAT, obj: element, "Packet length exceeds configured MTU");
            return Err(gst::FlowError::NotSupported);
        }

        let (curr_seq, now_rtpts) = match RTPBuffer::from_buffer_readable(&buffer) {
            Ok(rtpbuf) => (rtpbuf.seq(), rtpbuf.timestamp()),
            Err(_) => {
                gst::error!(CAT, obj: element, "Mapping to RTP packet failed");
                return Err(gst::FlowError::NotSupported);
            }
        };

        if let Some(last_seq) = state.seqnums.last() {
            if last_seq.overflowing_add(1).0 != curr_seq {
                gst::error!(CAT, obj: element, "Got out of sequence packets");
                return Err(gst::FlowError::NotSupported);
            }
        }

        state.packets.push(buffer.clone());
        state.seqnums.push(curr_seq);

        assert_eq!(state.packets.len(), state.seqnums.len());

        if state.packets.len() == state.protected_packets_num as usize {
            // We use current buffer timing as a base for repair packets timestamps
            let now_pts = buffer.pts();
            let now_dts = buffer.dts_or_pts();

            Self::process_source_block(element, state, now_pts, now_dts, now_rtpts)?;
        }

        drop(state_guard);
        self.srcpad.push(buffer)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::RaptorqEnc, event: gst::Event) -> bool {
        gst::debug!(CAT, "Handling event {:?}", event);
        use gst::EventView;

        match event.view() {
            EventView::FlushStart(_) => {
                if let Err(err) = self.stop(element) {
                    element_error!(
                        element,
                        gst::CoreError::Event,
                        ["Failed to stop encoder after flush start {:?}", err]
                    );
                    return false;
                }

                let _ = self.srcpad_fec.set_active(false);
            }
            EventView::FlushStop(_) => {
                if let Err(err) = self.start(element) {
                    element_error!(
                        element,
                        gst::CoreError::Event,
                        ["Failed to start encoder after flush stop {:?}", err]
                    );
                    return false;
                }

                let _ = self.srcpad_fec.set_active(true);
            }
            EventView::Caps(ev) => {
                let caps = ev.caps();
                gst::info!(CAT, obj: pad, "Got caps {:?}", caps);

                let mut state_guard = self.state.lock().unwrap();

                if let Some(state) = state_guard.as_mut() {
                    let s = caps.structure(0).unwrap();

                    // We need clock rate to calculate RTP timestamps of
                    // delayed repair packets.
                    if let Ok(clock_rate) = s.get::<i32>("clock-rate") {
                        if clock_rate <= 0 {
                            element_error!(element, gst::CoreError::Event, ["Invalid clock rate"]);
                            return false;
                        }

                        state.clock_rate = Some(clock_rate as u32);
                    }
                }
            }
            EventView::Segment(ev) => {
                let mut state_guard = self.state.lock().unwrap();

                if let Some(state) = state_guard.as_mut() {
                    let segment = ev.segment().clone();
                    let segment = match segment.downcast::<gst::ClockTime>() {
                        Ok(segment) => segment,
                        Err(_) => {
                            element_error!(
                                element,
                                gst::CoreError::Event,
                                ["Only time segments are supported"]
                            );
                            return false;
                        }
                    };

                    state.segment = segment.clone();

                    // Push stream events on FEC srcpad as well
                    let pad = &self.srcpad_fec;
                    let stream_id = pad.create_stream_id(element, Some("fec")).to_string();

                    let kmax = extended_source_block_symbols(state.symbols_per_block as u32);
                    let scheme_id = fecscheme::FEC_SCHEME_ID;

                    // RFC 6682, section 6.1.1
                    let caps = gst::Caps::builder("application/x-rtp")
                        .field("payload", state.pt as i32)
                        .field("ssrc", state.ssrc as i32)
                        .field("clock-rate", state.clock_rate.unwrap_or(0) as i32)
                        .field("encoding-name", "RAPTORFEC")
                        .field("raptor-scheme-id", scheme_id.to_string())
                        .field("kmax", kmax.to_string())
                        .field("repair-window", (state.repair_window * 1000).to_string()) // ms -> us
                        .field("t", state.symbol_size.to_string())
                        .field("p", "B")
                        .build();

                    drop(state_guard);

                    pad.push_event(gst::event::StreamStart::new(&stream_id));
                    pad.push_event(gst::event::Caps::new(&caps));
                    pad.push_event(gst::event::Segment::new(&segment));
                }
            }
            EventView::Eos(_) => {
                let mut state_guard = self.state.lock().unwrap();

                if let Some(state) = state_guard.as_mut() {
                    let _ = state.sender.as_ref().unwrap().send(SrcTaskMsg::Eos);
                }
            }
            _ => (),
        }

        pad.event_default(Some(element), event)
    }

    fn iterate_internal_links(
        &self,
        pad: &gst::Pad,
        _element: &super::RaptorqEnc,
    ) -> gst::Iterator<gst::Pad> {
        if pad == &self.sinkpad {
            gst::Iterator::from_vec(vec![self.srcpad.clone()])
        } else if pad == &self.srcpad {
            gst::Iterator::from_vec(vec![self.sinkpad.clone()])
        } else {
            gst::Iterator::from_vec(vec![])
        }
    }

    fn start(&self, element: &super::RaptorqEnc) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        let protected_packets_num = settings.protected_packets as usize;
        let repair_packets_num = settings.repair_packets as usize;
        let repair_window = settings.repair_window as usize;
        let symbol_size = settings.symbol_size as usize;
        let mtu = settings.mtu as usize;
        let pt = settings.pt as u8;

        // this is the number of repair symbols placed in each repair packet,
        // it SHALL be the same for all repair packets in a block. This include
        // 1 byte of flow indication and 2 bytes of lenght indication as defined
        // in RFC6881, section 8.2.4.
        let symbols_per_packet = (mtu + 3 + symbol_size - 1) / symbol_size;
        let symbols_per_block = symbols_per_packet * protected_packets_num;

        if symbol_size.rem_euclid(SYMBOL_ALIGNMENT) != 0 {
            let details = format!(
                "Symbol size is not multiple of Symbol Alignment {}",
                SYMBOL_ALIGNMENT
            );

            gst::element_error!(element, gst::CoreError::Failed, [&details]);
            return Err(error_msg!(gst::CoreError::Failed, [&details]));
        }

        if symbol_size > fecscheme::MAX_ENCODING_SYMBOL_SIZE {
            let details = format!(
                "Symbol size exceeds Maximum Encoding Symbol Size: {}",
                fecscheme::MAX_ENCODING_SYMBOL_SIZE
            );

            gst::element_error!(element, gst::CoreError::Failed, [&details]);
            return Err(error_msg!(gst::CoreError::Failed, [&details]));
        }

        if symbols_per_block > fecscheme::MAX_SOURCE_BLOCK_LEN {
            let details = format!(
                "Source block length exceeds Maximum Source Block Length: {}",
                fecscheme::MAX_SOURCE_BLOCK_LEN
            );

            gst::element_error!(element, gst::CoreError::Failed, [&details]);
            return Err(error_msg!(gst::CoreError::Failed, [&details]));
        }

        gst::info!(
            CAT,
            obj: element,
            "Starting RaptorQ Encoder, Symbols per Block: {}, Symbol Size: {}",
            symbols_per_block,
            symbol_size
        );

        let plan = SourceBlockEncodingPlan::generate(symbols_per_block as u16);
        let info = ObjectTransmissionInformation::new(0, symbol_size as u16, 1, 1, 8);

        let segment = gst::FormattedSegment::<gst::ClockTime>::default();

        *self.state.lock().unwrap() = Some(State {
            info,
            plan,
            repair_packets_num,
            protected_packets_num,
            repair_window,
            symbol_size,
            symbols_per_packet,
            symbols_per_block,
            mtu,
            pt,
            segment,
            seq: 0,
            ssrc: 0,
            packets: Vec::new(),
            seqnums: Vec::new(),
            clock_rate: None,
            sender: None,
        });

        Ok(())
    }

    fn stop(&self, _element: &super::RaptorqEnc) -> Result<(), gst::ErrorMessage> {
        let mut timers = self.pending_timers.lock().unwrap();
        for timer in timers.drain() {
            timer.unschedule();
        }

        // Drop state
        let _ = self.state.lock().unwrap().take();
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RaptorqEnc {
    const NAME: &'static str = "GstRaptorqEnc";
    type Type = super::RaptorqEnc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this, element| this.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Self::catch_panic_pad_function(
                    parent,
                    || false,
                    |this, element| this.sink_event(pad, element, event),
                )
            })
            .iterate_internal_links_function(|pad, parent| {
                Self::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |this, element| this.iterate_internal_links(pad, element),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .iterate_internal_links_function(|pad, parent| {
                Self::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |this, element| this.iterate_internal_links(pad, element),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS)
            .build();

        let templ = klass.pad_template("fec_0").unwrap();
        let srcpad_fec = gst::Pad::builder_with_template(&templ, Some("fec_0"))
            .activatemode_function(move |pad, parent, mode, active| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(loggable_error!(CAT, "Panic activating src pad with mode")),
                    |this, element| this.src_activatemode(pad, element, mode, active),
                )
            })
            .iterate_internal_links_function(|pad, parent| {
                Self::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |this, element| this.iterate_internal_links(pad, element),
                )
            })
            .build();

        Self {
            sinkpad,
            srcpad,
            srcpad_fec,
            settings: Mutex::new(Default::default()),
            state: Mutex::new(None),
            pending_timers: Mutex::new(HashSet::new()),
        }
    }
}

impl ObjectImpl for RaptorqEnc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::new(
                    "protected-packets",
                    "Protected Packets",
                    "Number of packets to protect together",
                    1,
                    u32::MAX - 1,
                    DEFAULT_PROTECTED_PACKETS,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "repair-packets",
                    "Repair Packets",
                    "Number of repair packets per block to send",
                    1,
                    u32::MAX - 1,
                    DEFAULT_REPAIR_PACKETS,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "repair-window",
                    "Repair Window",
                    "A time span in milliseconds in which repair packets are send",
                    0,
                    u32::MAX - 1,
                    DEFAULT_REPAIR_PACKETS,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "symbol-size",
                    "Symbol Size",
                    "Size of RaptorQ data unit",
                    1,
                    u32::MAX - 1,
                    DEFAULT_SYMBOL_SIZE,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    // TODO: maybe change this to max-rtp-packet-size or max-media-packet-size
                    "mtu",
                    "MTU",
                    "Maximum expected packet size",
                    0,
                    i32::MAX as u32,
                    DEFAULT_MTU,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "pt",
                    "Payload Type",
                    "The payload type of FEC packets",
                    96,
                    255,
                    DEFAULT_PT,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
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
        match pspec.name() {
            "protected-packets" => {
                let mut settings = self.settings.lock().unwrap();
                let protected_packets = value.get().expect("type checked upstream");
                settings.protected_packets = protected_packets;
            }
            "repair-packets" => {
                let mut settings = self.settings.lock().unwrap();
                let repair_packets = value.get().expect("type checked upstream");
                settings.repair_packets = repair_packets;
            }
            "repair-window" => {
                let mut settings = self.settings.lock().unwrap();
                let repair_window = value.get().expect("type checked upstream");
                settings.repair_window = repair_window;
            }
            "symbol-size" => {
                let mut settings = self.settings.lock().unwrap();
                let symbol_size = value.get().expect("type checked upstream");
                settings.symbol_size = symbol_size;
            }
            "mtu" => {
                let mut settings = self.settings.lock().unwrap();
                let mtu = value.get().expect("type checked upstream");
                settings.mtu = mtu;
            }
            "pt" => {
                let mut settings = self.settings.lock().unwrap();
                let pt = value.get().expect("type checked upstream");
                settings.pt = pt;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "protected-packets" => {
                let settings = self.settings.lock().unwrap();
                settings.protected_packets.to_value()
            }
            "repair-packets" => {
                let settings = self.settings.lock().unwrap();
                settings.repair_packets.to_value()
            }
            "repair-window" => {
                let settings = self.settings.lock().unwrap();
                settings.repair_window.to_value()
            }
            "symbol-size" => {
                let settings = self.settings.lock().unwrap();
                settings.symbol_size.to_value()
            }
            "mtu" => {
                let settings = self.settings.lock().unwrap();
                settings.mtu.to_value()
            }
            "pt" => {
                let settings = self.settings.lock().unwrap();
                settings.pt.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
        obj.add_pad(&self.srcpad_fec).unwrap();
    }
}

impl GstObjectImpl for RaptorqEnc {}

impl ElementImpl for RaptorqEnc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP RaptorQ FEC Encoder",
                "RTP RaptorQ FEC Encoding",
                "Performs FEC using RaptorQ (RFC6681, RFC6682)",
                "Tomasz Andrzejak <andreiltd@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/x-rtp")
                .field("clock-rate", gst::IntRange::new(0, std::i32::MAX))
                .build();

            let srcpad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sinkpad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let srcpad_fec_template = gst::PadTemplate::new(
                "fec_0",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![srcpad_template, sinkpad_template, srcpad_fec_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}
