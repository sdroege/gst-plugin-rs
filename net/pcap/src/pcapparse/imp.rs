// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::glib::value::ToValueOptional;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::net::IpAddr;
use std::sync::LazyLock;
use std::sync::Mutex;

use pcap_parser::pcap::parse_pcap_header;
use pcap_parser::pcapng;
use pcap_parser::traits::PcapNGPacketBlock;
use pcap_parser::{Linktype, nom};

const MAGIC_HEADER_SIZE_IN_BYTES: usize = 4;

// PCAP (legacy) file header
// magic(4) + version(4) + thiszone(4) + sigfigs(4) + snaplen(4) + network(4)
const PCAP_GLOBAL_HEADER_SIZE_IN_BYTES: usize = 24;
// PCAP (legacy) per-packet record header
// ts_sec(4) + ts_usec(4) + incl_len(4) + orig_len(4)
const PCAP_RECORD_HEADER_SIZE_IN_BYTES: usize = 16;

// Default PCAPNG timestamp resolution in units per second. `if_tsresol` is a
// power of 10 (specification default of 6), so 10^6 = microsecond resolution.
const DEFAULT_PCAPNG_TS_RESOLUTION: u64 = 1_000_000;
const PCAPNG_MAGIC_HEADER: u32 = 0x0A0D0D0A;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "pcapparse2",
        gst::DebugColorFlags::empty(),
        Some("PCAP parser Element"),
    )
});

fn parse_ip_address(value: &str) -> Option<IpAddr> {
    match value.parse() {
        Ok(addr) => Some(addr),
        Err(_) => {
            gst::warning!(CAT, "Invalid IP address '{value}'");
            None
        }
    }
}

fn pcapng_timestamp_to_clocktime(
    ts_high: u32,
    ts_low: u32,
    ticks_per_second: u64,
    ts_offset_secs: i64,
) -> gst::ClockTime {
    const NS_PER_SECOND: u128 = 1_000_000_000; // gst::ClockTime::SECOND
    const MAX_FINITE_NS: i128 = (u64::MAX - 1) as i128;

    // We make sure this does not happen with DEFAULT_PCAPNG_TS_RESOLUTION.
    assert!(
        ticks_per_second != 0,
        "PCAPNG timestamp resolution must be non-zero"
    );

    let raw_ticks = (u128::from(ts_high) << 32) | u128::from(ts_low);

    // Integer division intentionally truncates toward zero.
    let timestamp_ns = (raw_ticks * NS_PER_SECOND / u128::from(ticks_per_second)) as i128;

    let offset_ns = i128::from(ts_offset_secs) * NS_PER_SECOND as i128;
    let clamped_ns = timestamp_ns
        .saturating_add(offset_ns)
        .clamp(0, MAX_FINITE_NS) as u64;

    gst::ClockTime::from_nseconds(clamped_ns)
}

#[derive(Debug)]
struct Settings {
    src_ip: Option<IpAddr>,
    dst_ip: Option<IpAddr>,
    src_port: Option<u32>,
    dst_port: Option<u32>,
    caps: Option<gst::Caps>,
    ts_offset: Option<gst::ClockTime>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            src_ip: None,
            dst_ip: None,
            src_port: None,
            dst_port: None,
            caps: None,
            ts_offset: Some(gst::ClockTime::ZERO),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcapFormat {
    Legacy,
    Ng,
}

#[derive(Default)]
struct InterfaceInfo {
    linktype: Linktype,
    ts_resolution: u64,
    ts_offset: i64,
}

struct State {
    buffer: Vec<u8>,

    needs_endianness: bool,
    swap_endian: bool,
    nanosecond_timestamp: bool,

    format: Option<PcapFormat>,
    linktype: Linktype,
    if_infos: Vec<InterfaceInfo>,

    newsegment_sent: bool,
    first_packet: bool,
    base_ts: Option<gst::ClockTime>,

    packets: Vec<(Vec<u8>, Option<gst::ClockTime>)>,
}

impl Default for State {
    fn default() -> Self {
        State {
            buffer: Vec::new(),
            needs_endianness: true,
            swap_endian: false,
            nanosecond_timestamp: false,
            format: None,
            linktype: Linktype::default(),
            if_infos: Vec::new(),
            newsegment_sent: false,
            first_packet: true,
            base_ts: None,
            packets: Vec::new(),
        }
    }
}

impl State {
    fn read_u32_from_buffer(&self, offset: usize) -> u32 {
        let buffer = self.buffer.as_slice();
        let bytes = [
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ];

        if self.swap_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    }
}

pub struct PcapParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl PcapParse {
    fn reset(&self) {
        let mut state = self.state.lock().unwrap();

        state.needs_endianness = true;
        state.swap_endian = false;
        state.nanosecond_timestamp = false;
        state.newsegment_sent = false;
        state.first_packet = true;

        state.linktype = Linktype(0);
        state.format = None;
        state.base_ts = None;

        state.if_infos.clear();
        state.buffer.clear();
        state.packets.clear();
    }

    // Adapted from `gst_pcap_parse_scan_frame` in `gstpcapparse.c`.
    fn extract_payload<'a>(
        &self,
        packet_data: &'a [u8],
        linktype: Linktype,
        settings: &Settings,
    ) -> Option<&'a [u8]> {
        use etherparse::{NetSlice, SlicedPacket};

        let sliced = match linktype {
            pcap_parser::Linktype(1) => SlicedPacket::from_ethernet(packet_data).ok()?,
            pcap_parser::Linktype(113) => SlicedPacket::from_linux_sll(packet_data).ok()?,
            pcap_parser::Linktype(276) => {
                // Linux SLL2: first 2 bytes are ethertype, then 18 bytes header
                if packet_data.len() < 20 {
                    return None;
                }
                let ether_type = u16::from_be_bytes([packet_data[0], packet_data[1]]);
                // Accept both IPv4 and IPv6
                if ether_type != 0x0800 && ether_type != 0x86DD {
                    return None;
                }
                SlicedPacket::from_ether_type(etherparse::EtherType(ether_type), &packet_data[20..])
                    .ok()?
            }
            pcap_parser::Linktype(101) => {
                // IPv4 or IPv6 packets with no link-layer header.
                SlicedPacket::from_ip(packet_data).ok()?
            }
            pcap_parser::Linktype(228) => {
                // IPv4 packets with no link-layer header.
                SlicedPacket::from_ether_type(etherparse::ether_type::IPV4, packet_data).ok()?
            }
            pcap_parser::Linktype(229) => {
                // IPv6 packets with no link-layer header.
                SlicedPacket::from_ether_type(etherparse::ether_type::IPV6, packet_data).ok()?
            }
            _ => return None,
        };

        match sliced.net.as_ref()? {
            NetSlice::Ipv4(ipv4) => {
                // TODO: Support fragmented IPv4 packets
                if ipv4.header().more_fragments()
                    || ipv4.header().fragments_offset() != etherparse::IpFragOffset::ZERO
                {
                    return None;
                }

                if settings
                    .src_ip
                    .is_some_and(|src_ip| src_ip != IpAddr::V4(ipv4.header().source_addr()))
                {
                    return None;
                }

                if settings
                    .dst_ip
                    .is_some_and(|dst_ip| dst_ip != IpAddr::V4(ipv4.header().destination_addr()))
                {
                    return None;
                }
            }
            NetSlice::Ipv6(ipv6) => {
                // TODO: Support fragmented IPv6 packets
                if ipv6.is_payload_fragmented() {
                    return None;
                }

                if settings
                    .src_ip
                    .is_some_and(|src_ip| src_ip != IpAddr::V6(ipv6.header().source_addr()))
                {
                    return None;
                }

                if settings
                    .dst_ip
                    .is_some_and(|dst_ip| dst_ip != IpAddr::V6(ipv6.header().destination_addr()))
                {
                    return None;
                }
            }
            _ => {
                gst::log!(CAT, "Ignoring non-IPv4/IPv6 network layer");
            }
        }

        match sliced.transport.as_ref()? {
            etherparse::TransportSlice::Udp(udp) => {
                if let Some(src_port) = settings.src_port
                    && udp.source_port() != src_port as u16
                {
                    return None;
                }

                if let Some(dst_port) = settings.dst_port
                    && udp.destination_port() != dst_port as u16
                {
                    return None;
                }

                Some(udp.payload())
            }
            etherparse::TransportSlice::Tcp(tcp) => {
                if let Some(src_port) = settings.src_port
                    && tcp.source_port() != src_port as u16
                {
                    return None;
                }

                if let Some(dst_port) = settings.dst_port
                    && tcp.destination_port() != dst_port as u16
                {
                    return None;
                }

                Some(tcp.payload())
            }
            _ => {
                gst::log!(CAT, "Ignoring non-UDP/TCP transport layer");
                None
            }
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Ok(data) = buffer.map_readable() else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Failed to map buffer readable"]
            );
            return Err(gst::FlowError::Error);
        };

        gst::trace!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        state.buffer.extend_from_slice(data.as_slice());
        drop(data);

        loop {
            if state.format.is_none() {
                if state.buffer.len() < MAGIC_HEADER_SIZE_IN_BYTES {
                    break;
                }

                // PCAP-NG magic 0x0A0D0D0A is a byte-wise palindrome, so
                // read_u32_from_buffer returns the same value regardless
                // of whether state.swap_endian is set. This is safe: SHB
                // parser below determines the actual endianness from the
                // BOM field inside the block.
                if PCAPNG_MAGIC_HEADER == state.read_u32_from_buffer(0) {
                    state.format = Some(PcapFormat::Ng);
                    gst::info!(CAT, obj = pad, "Detected PCAPNG format");
                } else {
                    match parse_pcap_header(&state.buffer) {
                        Ok((_, header)) => {
                            let version_major = header.version_major;
                            if version_major != 2 {
                                gst::element_imp_error!(
                                    self,
                                    gst::StreamError::WrongType,
                                    [
                                        "Invalid PCAP header, unsupported major version {version_major}"
                                    ]
                                );

                                return Err(gst::FlowError::Error);
                            }

                            state.format = Some(PcapFormat::Legacy);

                            state.swap_endian = header.is_bigendian();
                            state.nanosecond_timestamp = header.is_nanosecond_precision();
                            state.linktype = header.network;

                            gst::info!(
                                CAT,
                                obj = pad,
                                "Detected legacy PCAP format, Interface: {}",
                                state.linktype.to_string()
                            );

                            state.buffer.drain(..header.size());
                        }
                        Err(nom::Err::Incomplete(_)) => {
                            // Need more data
                            if state.buffer.len() < PCAP_GLOBAL_HEADER_SIZE_IN_BYTES {
                                break;
                            }

                            gst::element_imp_error!(
                                self,
                                gst::StreamError::WrongType,
                                ["Invalid PCAP header"]
                            );

                            return Err(gst::FlowError::Error);
                        }
                        Err(err) => {
                            gst::error!(CAT, obj = pad, "Invalid PCAP header {err:?}");

                            gst::element_imp_error!(
                                self,
                                gst::StreamError::WrongType,
                                ["Invalid PCAP header"]
                            );

                            return Err(gst::FlowError::Error);
                        }
                    }
                }
            }

            match state.format {
                Some(PcapFormat::Ng) => {
                    if state.needs_endianness {
                        match pcapng::parse_sectionheaderblock(&state.buffer) {
                            Ok((remaining, shb)) => {
                                let big_endian = shb.big_endian();
                                let block_size = state.buffer.len() - remaining.len();

                                state.swap_endian = big_endian;
                                state.buffer.drain(..block_size);

                                state.needs_endianness = false;

                                // Starting a new section, clear known interfaces
                                state.if_infos.clear();

                                gst::debug!(
                                    CAT,
                                    obj = pad,
                                    "New PCAPNG section, big_endian: {}",
                                    big_endian
                                );
                            }
                            Err(nom::Err::Incomplete(_)) => break,
                            Err(err) => {
                                gst::error!(CAT, obj = pad, "PCAPNG SHB parse error: {err:?}");

                                // Skip invalid data
                                if !state.buffer.is_empty() {
                                    state.buffer.drain(..1);
                                }
                                continue;
                            }
                        }
                    }

                    if state.buffer.is_empty() {
                        break;
                    }

                    let parse_result = if state.swap_endian {
                        pcapng::parse_block_be(&state.buffer)
                    } else {
                        pcapng::parse_block_le(&state.buffer)
                    };

                    match parse_result {
                        Ok((remaining, block)) => {
                            let consumed = state.buffer.len() - remaining.len();

                            match block {
                                pcapng::Block::SectionHeader(shb) => {
                                    let big_endian = shb.big_endian();

                                    state.swap_endian = big_endian;
                                    state.if_infos.clear();

                                    gst::debug!(
                                        CAT,
                                        obj = pad,
                                        "New PCAPNG section, big_endian: {}",
                                        big_endian
                                    );
                                }
                                pcapng::Block::InterfaceDescription(idb) => {
                                    let linktype = idb.linktype;
                                    // `ts_resolution()` returns the number of
                                    // time units per second (e.g. 1000 for
                                    // ms, 10^6 for µs resolution).
                                    //
                                    // This fallback only matters for invalid
                                    // `if_tsresol` values.
                                    let ts_resolution =
                                        idb.ts_resolution().unwrap_or(DEFAULT_PCAPNG_TS_RESOLUTION);

                                    // TODO:
                                    //
                                    // pcap_parser 0.17 decodes if_tsoffset as
                                    // little-endian regardless of the section
                                    // byte order. Decode it here using section's
                                    // actual byte order.
                                    // https://github.com/rusticata/pcap-parser/issues/62
                                    let ts_offset = idb
                                        .options
                                        .iter()
                                        .find(|opt| {
                                            opt.code == pcap_parser::pcapng::OptionCode::IfTsoffset
                                        })
                                        .and_then(|opt| opt.as_bytes().ok())
                                        .and_then(|bytes| bytes.try_into().ok())
                                        .map(|bytes: [u8; 8]| {
                                            if state.swap_endian {
                                                i64::from_be_bytes(bytes)
                                            } else {
                                                i64::from_le_bytes(bytes)
                                            }
                                        })
                                        .unwrap_or(0);

                                    state.if_infos.push(InterfaceInfo {
                                        linktype,
                                        ts_resolution,
                                        ts_offset,
                                    });

                                    gst::debug!(
                                        CAT,
                                        obj = pad,
                                        "PCAPNG Interface: {}, ts_resolution: {}, ts_offset: {}",
                                        linktype.to_string(),
                                        ts_resolution,
                                        ts_offset
                                    );
                                }
                                pcapng::Block::EnhancedPacket(epb) => {
                                    let if_id = epb.if_id as usize;

                                    if if_id < state.if_infos.len() {
                                        let if_info = &state.if_infos[if_id];

                                        let ts = pcapng_timestamp_to_clocktime(
                                            epb.ts_high,
                                            epb.ts_low,
                                            if_info.ts_resolution,
                                            if_info.ts_offset,
                                        );

                                        if let Some(payload) = self.extract_payload(
                                            epb.packet_data(),
                                            if_info.linktype,
                                            &settings,
                                        ) {
                                            let payload = payload.to_vec();
                                            state.packets.push((payload, Some(ts)));
                                        }
                                    }
                                }
                                pcapng::Block::SimplePacket(spb) if !state.if_infos.is_empty() => {
                                    let linktype = state.if_infos[0].linktype;

                                    if let Some(payload) =
                                        self.extract_payload(spb.packet_data(), linktype, &settings)
                                    {
                                        // Simple Packet Blocks carry no timestamp.
                                        let payload = payload.to_vec();
                                        state.packets.push((payload, None));
                                    }
                                }
                                _ => {
                                    // Ignore other block types
                                }
                            }

                            state.buffer.drain(..consumed);
                        }
                        Err(nom::Err::Incomplete(_)) => break,
                        Err(err) => {
                            gst::error!(CAT, obj = pad, "PCAPNG parse error: {err:?}");

                            // Skip invalid data
                            if !state.buffer.is_empty() {
                                state.buffer.drain(..1);
                            }
                        }
                    }

                    if !state.buffer.is_empty() {
                        continue;
                    }
                }
                Some(PcapFormat::Legacy) => {
                    if state.buffer.len() < PCAP_RECORD_HEADER_SIZE_IN_BYTES {
                        break;
                    }

                    let ts_sec = state.read_u32_from_buffer(0);
                    let ts_usec = state.read_u32_from_buffer(4);
                    let incl_len = state.read_u32_from_buffer(8);

                    if state.buffer.len() < PCAP_RECORD_HEADER_SIZE_IN_BYTES + incl_len as usize {
                        break;
                    }

                    let ts_frac = if state.nanosecond_timestamp {
                        ts_usec as u64
                    } else {
                        (ts_usec as u64) * u64::from(gst::ClockTime::USECOND)
                    };

                    let ts = Some(
                        gst::ClockTime::from_seconds(ts_sec as u64)
                            + gst::ClockTime::from_nseconds(ts_frac),
                    );

                    let packet_start = PCAP_RECORD_HEADER_SIZE_IN_BYTES;
                    let packet_end = packet_start + incl_len as usize;

                    let packet_data = &state.buffer[packet_start..packet_end];

                    if let Some(payload) =
                        self.extract_payload(packet_data, state.linktype, &settings)
                    {
                        let payload = payload.to_vec();
                        state.packets.push((payload, ts));
                    }

                    state.buffer.drain(..packet_end);
                }
                None => break,
            }
        }

        if state.packets.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let packets = std::mem::take(&mut state.packets);
        let mut buflist = gst::BufferList::new();
        let buflist_ref = buflist.get_mut().unwrap();

        for (payload, packet_ts) in packets.into_iter() {
            let mut buf = gst::Buffer::from_slice(payload);
            {
                let buf_ref = buf.get_mut().unwrap();

                if let Some(pkt_ts) = packet_ts {
                    if state.base_ts.is_none() {
                        state.base_ts = Some(pkt_ts);
                    }

                    let ts = match settings.ts_offset {
                        Some(offset) => {
                            let base_ts = state.base_ts.unwrap();
                            let delta = pkt_ts.nseconds().saturating_sub(base_ts.nseconds());

                            gst::ClockTime::from_nseconds(
                                delta.saturating_add(offset.nseconds()).min(u64::MAX - 1),
                            )
                        }
                        None => pkt_ts,
                    };

                    buf_ref.set_dts(Some(ts));
                }

                if state.first_packet {
                    buf_ref.set_flags(gst::BufferFlags::DISCONT);
                    state.first_packet = false;
                } else {
                    buf_ref.unset_flags(gst::BufferFlags::DISCONT);
                }
            }

            buflist_ref.add(buf);
        }

        let src_caps = settings.caps.clone();
        let segment_start = match settings.ts_offset {
            Some(_) => gst::ClockTime::ZERO,
            None => state.base_ts.unwrap_or(gst::ClockTime::ZERO),
        };
        let send_segment = !state.newsegment_sent && !buflist.is_empty();

        if send_segment {
            state.newsegment_sent = true;
        }

        drop(settings);
        drop(state);

        if send_segment {
            if let Some(caps) = &src_caps {
                self.srcpad.push_event(gst::event::Caps::new(caps));
            }

            let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
            segment.set_start(segment_start);

            self.srcpad.push_event(gst::event::Segment::new(&segment));
        }

        if !buflist.is_empty() {
            self.srcpad.push_list(buflist)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        match event.view() {
            EventView::FlushStop(_) => {
                self.reset();
                // Push event down the pipeline so that other elements
                // stop flushing fall through.
                self.srcpad.push_event(event)
            }
            EventView::Segment(_) => {
                // Drop it, we'll replace it with our own
                true
            }
            _ => self.srcpad.push_event(event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PcapParse {
    const NAME: &'static str = "GstPcapParse2";
    type Type = super::PcapParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                PcapParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |pcapparse| pcapparse.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                PcapParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |pcapparse| pcapparse.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ).build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for PcapParse {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj().add_pad(&self.sinkpad).unwrap();
        self.obj().add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("src-ip")
                    .nick("Source IP")
                    .blurb("Source IP to restrict to")
                    .build(),
                glib::ParamSpecString::builder("dst-ip")
                    .nick("Destination IP")
                    .blurb("Destination IP to restrict to")
                    .build(),
                glib::ParamSpecUInt::builder("src-port")
                    .nick("Source Port")
                    .blurb("Source port to restrict to")
                    .maximum(u16::MAX.into())
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt::builder("dst-port")
                    .nick("Destination Port")
                    .blurb("Destination port to restrict to")
                    .maximum(u16::MAX.into())
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt64::builder("ts-offset")
                    .nick("Timestamp Offset")
                    .blurb(
                        "Relative timestamp offset (ns) to apply (gst::ClockTime::None = Use absolute packet time)",
                    )
                    .default_value(0)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("Caps")
                    .blurb("The caps of the source pad")
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "src-ip" => {
                let ip = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                settings.src_ip = ip.as_deref().and_then(parse_ip_address);
            }
            "dst-ip" => {
                let ip = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                settings.dst_ip = ip.as_deref().and_then(parse_ip_address);
            }
            "src-port" => {
                settings.src_port = value.get::<u32>().expect("type checked upstream").into();
            }
            "dst-port" => {
                settings.dst_port = value.get::<u32>().expect("type checked upstream").into();
            }
            "ts-offset" => {
                settings.ts_offset = value
                    .get::<Option<gst::ClockTime>>()
                    .expect("type checked upstream");
            }
            "caps" => {
                settings.caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "src-ip" => settings.src_ip.map(|ip| ip.to_string()).to_value(),
            "dst-ip" => settings.dst_ip.map(|ip| ip.to_string()).to_value(),
            "src-port" => settings.src_port.unwrap_or(0u32).to_value(),
            "dst-port" => settings.dst_port.unwrap_or(0u32).to_value(),
            "ts-offset" => gst::ClockTime::to_value_optional(settings.ts_offset.as_ref()),
            "caps" => settings.caps.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for PcapParse {}

impl ElementImpl for PcapParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "PCAP Parser",
                "Generic",
                "Parses PCAP stream",
                "Sanchayan Maity <sanchayan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let caps = gst::Caps::builder("raw/x-pcap").build();
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
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        let ret = self.parent_change_state(transition);

        if transition == gst::StateChange::PausedToReady {
            self.reset();
        }

        ret
    }
}
