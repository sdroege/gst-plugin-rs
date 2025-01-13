// GStreamer RTP Vorbis Depayloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpvorbisdepay2
 * @see_also: rtpvorbispay2, rtpvorbispay, rtpvorbisdepay, vorbisdec, vorbisenc
 *
 * Extracts an Vorbis audio stream from RTP packets as per [RFC 5215][rfc-5215].
 *
 * [rfc-5215]: https://www.rfc-editor.org/rfc/rfc5215.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch udpsrc caps='application/x-rtp, media=audio, clock-rate=48000, encoding-name=VORBIS, payload=96' ! rtpvorbisdepay2 ! vorbisdec ! audioconvert ! audioresample ! autoaudiosink
 * ]| This will depayload an incoming RTP Vorbis audio stream. You can use the #vorbisenc and
 * #rtpvorbispay2 elements to create such an RTP stream. Note that if you don't have external
 * signalling (RTSP, SDP, WebRTC, RTP caps with a `configuration` field) you may need to set the
 * `config-interval` property on the payloader.
 *
 * Since: plugins-rs-0.16
 */
use atomic_refcell::AtomicRefCell;

use bitstream_io::{BigEndian, BitRead, BitReader, ByteRead, ByteReader};

use data_encoding::BASE64;

use gst::{glib, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basedepay::{PacketToBufferRelation, RtpBaseDepay2Ext, RtpBaseDepay2Impl};

use crate::vorbis::config::*;

use std::io::Cursor;
use std::ops::RangeInclusive;

// Limit memory usage for configs
const MAX_CONFIGS: usize = 64;

#[derive(Debug, PartialEq)]
enum FragType {
    NotFragmented,
    Start,
    Continuation,
    End,
}

#[derive(Debug, PartialEq)]
enum VorbisDataType {
    RawPacket,
    PackedConfig,
    LegacyComment,
    Reserved,
}

#[derive(Debug)]
struct Accumulator {
    data: Vec<u8>,
    ext_seqnum: u64,
    ext_timestamp: u64,
    ident: u32,
    vdt: VorbisDataType,
}

#[derive(Default)]
struct State {
    // Accumulator for fragmented payloads
    acc: Option<Accumulator>,

    // Available configs
    configs: ConfigPool,

    // Active config
    active_ident: Option<u32>,

    // Throttle warning messages we send
    last_warning_message: Option<std::time::Instant>,
}

#[derive(Default)]
pub struct RtpVorbisDepay {
    state: AtomicRefCell<State>,
}

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpvorbisdepay2",
        gst::DebugColorFlags::empty(),
        Some("RTP Vorbis Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpVorbisDepay {
    const NAME: &'static str = "GstRtpVorbisDepay2";
    type Type = super::RtpVorbisDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpVorbisDepay {}

impl GstObjectImpl for RtpVorbisDepay {}

impl ElementImpl for RtpVorbisDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP Vorbis Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload a Vorbis audio stream from RTP packets (RFC 5215)",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("encoding-name", "VORBIS")
                            .field("clock-rate", gst::IntRange::new(1i32, i32::MAX))
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("audio/x-vorbis").build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RtpBaseDepay2Impl for RtpVorbisDepay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State {
            acc: None,
            configs: ConfigPool::new(MAX_CONFIGS),
            active_ident: None,
            last_warning_message: None,
        };

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        Ok(())
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        // We'll use the sample rate extracted from the headers later for the caps
        let _clock_rate = s.get::<i32>("clock-rate").unwrap();

        let Some(packed_config) = s.get::<&str>("configuration").ok() else {
            gst::debug!(
                CAT,
                imp = self,
                "Got caps without vorbis configuration -- expecting in-band configuration"
            );
            return true;
        };

        let Some(packed_config) = BASE64
            .decode(packed_config.trim().as_bytes())
            .ok()
            .filter(|v| !v.is_empty())
        else {
            gst::warning!(
                CAT,
                imp = self,
                "Failed to parse configuration from Vorbis RTP input caps {s}"
            );
            return false;
        };

        let mut r = ByteReader::endian(Cursor::new(&packed_config), BigEndian);

        let Ok(n_configs) = r.read::<u32>() else {
            gst::error!(
                CAT,
                imp = self,
                "Failed to parse vorbis configuration: not enough data"
            );
            return false;
        };

        let mut state = self.state.borrow_mut();

        gst::info!(
            CAT,
            imp = self,
            "{n_configs} vorbis configurations, {} bytes total",
            packed_config.len()
        );

        for i in 0..n_configs {
            gst::info!(CAT, imp = self, "Parsing vorbis configuration {i}..");

            let config = match r.parse::<VorbisConfig>() {
                Err(e) => {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Decode,
                        ["Failed to parse vorbis configuration {i}: {e:?}"]
                    );
                    return false;
                }
                Ok(config) => config,
            };

            gst::info!(
                CAT,
                imp = self,
                "Got out-of-band vorbis configuration {i} with ident {:04x}, {}ch @ {}Hz",
                config.ident(),
                config.channels(),
                config.rate()
            );

            state.configs.add_config(config, ConfigSource::OutOfBand);
        }

        // If we have exactly one configuration we optimistically activate it now.
        // Otherwise we'll handle it later when we get the first packet with the ident that applies.
        if n_configs == 1 {
            let config = state.configs.front().unwrap().clone();

            let ident = config.ident();

            if self.activate_config(config).is_err() {
                return false;
            }

            gst::debug!(CAT, imp = self, "Activated new config {ident:x?}");

            state.active_ident = Some(ident);
        }

        true
    }

    // https://www.rfc-editor.org/rfc/rfc5215.html#section-5
    // https://www.rfc-editor.org/errata/rfc5215
    //
    // We either get 1-N whole Vorbis audio frames in an RTP packet,
    // or a single Vorbis audio frame (or config) split over multiple RTP packets.
    //
    // The marker flag is unused in Vorbis according to the spec and should always
    // be zero, so we don't do anything with it.
    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        if packet.discont() {
            state.acc = None;
        }

        let payload = packet.payload();

        // Payload Header - https://www.rfc-editor.org/rfc/rfc5215.html#section-2.2

        if payload.len() < (4 + 2) {
            gst::warning!(
                CAT,
                imp = self,
                "Payload too small: {} bytes, but need at least 6 bytes",
                payload.len()
            );
            state.acc = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        gst::trace!(CAT, imp = self, "Payload header: {:02x?}", &payload[0..4]);

        let mut hdr = BitReader::endian(&payload[0..6], BigEndian);

        let ident = hdr.read::<24, u32>().unwrap();

        let frag_type = match hdr.read::<2, u8>().unwrap() {
            0 => FragType::NotFragmented,
            1 => FragType::Start,
            2 => FragType::Continuation,
            3 => FragType::End,
            _ => unreachable!(),
        };

        let vdt = match hdr.read::<2, u8>().unwrap() {
            0 => VorbisDataType::RawPacket,
            1 => VorbisDataType::PackedConfig,
            2 => VorbisDataType::LegacyComment,
            3 => VorbisDataType::Reserved,
            _ => unreachable!(),
        };

        if vdt == VorbisDataType::Reserved {
            gst::warning!(
                CAT,
                imp = self,
                "Ignoring packet of unexpected/reserved Vorbis data type"
            );
            state.acc = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        // Number of packets is informational only, we don't use it
        let n_packets = hdr.read::<4, u8>().unwrap();

        // All packet types have a two-byte length indicator at the start
        let packet_len = hdr.read::<16, u16>().unwrap() as usize;

        gst::log!(
            CAT,
            imp = self,
            "{vdt:?}, {frag_type:?}, \
             {n_packets} packets, \
             (first) length {packet_len}"
        );

        let (_, payload) = payload.split_at(6);

        // Short packet?
        if packet_len > payload.len() {
            gst::warning!(
                CAT,
                imp = self,
                "Short packet, expected {packet_len} bytes but only have {} byte payload",
                payload.len()
            );

            state.acc = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        match frag_type {
            FragType::Start => {
                if let Some(acc) = state.acc.as_ref() {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Dropping unfinished partial frame {:?}",
                        acc
                    );
                    self.obj()
                        .drop_packets(acc.ext_seqnum..=packet.ext_seqnum() - 1);
                    state.acc = None;
                }

                let mut data = Vec::with_capacity(3 * packet_len);
                data.extend_from_slice(payload);

                state.acc = Some(Accumulator {
                    data,
                    ext_seqnum: packet.ext_seqnum(),
                    ext_timestamp: packet.ext_timestamp(),
                    ident,
                    vdt,
                });

                gst::trace!(CAT, imp = self, "Partial frame {:?}", state.acc);

                return Ok(gst::FlowSuccess::Ok);
            }

            FragType::Continuation | FragType::End => {
                let Some(acc) = state.acc.as_mut() else {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "{frag_type:?} packet but no partial frame (most likely indicates packet loss)"
                    );
                    self.obj().drop_packet(packet);
                    state.acc = None;
                    return Ok(gst::FlowSuccess::Ok);
                };

                if acc.ext_timestamp != packet.ext_timestamp() {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "{frag_type:?} packet timestamp {} doesn't match existing partial fragment timestamp {}",
                        packet.ext_timestamp(),
                        acc.ext_timestamp
                    );
                    state.acc = None;
                    self.obj().drop_packet(packet);
                    return Ok(gst::FlowSuccess::Ok);
                }

                if acc.ident != ident || acc.vdt != vdt {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Data type {vdt:?} or ident {ident:?} don't match existing partial fragment ({:?}, {:?})",
                        acc.vdt,
                        acc.ident
                    );
                    state.acc = None;
                    self.obj().drop_packet(packet);
                    return Ok(gst::FlowSuccess::Ok);
                }

                acc.data.extend_from_slice(payload);

                gst::debug!(
                    CAT,
                    imp = self,
                    "Added {frag_type:?} packet payload, assembled {} bytes now",
                    acc.data.len()
                );

                if frag_type == FragType::End {
                    let acc = state.acc.take().unwrap();
                    return self.handle_complete_packet(
                        state,
                        &acc.data,
                        acc.data.len(), // first packet length = whole data length here
                        ident,
                        vdt,
                        acc.ext_seqnum..=packet.ext_seqnum(),
                    );
                }
            }

            FragType::NotFragmented => {
                return self.handle_complete_packet(
                    state,
                    payload,
                    packet_len,
                    ident,
                    vdt,
                    packet.ext_seqnum()..=packet.ext_seqnum(),
                );
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

impl RtpVorbisDepay {
    fn activate_config(&self, config: VorbisConfig) -> Result<(), gst::FlowError> {
        let ident = config.ident();

        let channels = config.channels() as i32;
        let rate = config.rate();

        gst::debug!(
            CAT,
            imp = self,
            "New config {ident:x?} has {channels}ch @ {rate}Hz"
        );

        let headers = config.into_headers().map(|hdr| {
            let mut hdr_buf = gst::Buffer::from_mut_slice(hdr);
            hdr_buf
                .get_mut()
                .unwrap()
                .set_flags(gst::BufferFlags::HEADER);
            hdr_buf
        });

        gst::debug!(CAT, imp = self, "Setting new output caps..");

        let src_caps = gst::Caps::builder("audio/x-vorbis")
            .field("streamheader", gst::Array::new(&headers))
            .field("channels", channels)
            .field("rate", rate)
            .build();

        // Base class will make sure we don't send out the same caps again if nothing changed
        self.obj().set_src_caps(&src_caps);

        gst::debug!(CAT, imp = self, "Sending headers for new config {ident:x?}");

        for hdr_buf in headers {
            self.obj()
                .queue_buffer(PacketToBufferRelation::OutOfBand, hdr_buf)
                .inspect_err(|&err| {
                    gst::warning!(CAT, imp = self, "Failed to push header buffer: {err:?}");
                })?;
        }
        Ok(())
    }

    // IMPROVE: Might be better to pass an input buffer as well so we can make sub-buffers, also for
    // the case where we assembled a RawPacket from multiple RTP packets and have a Vec with the
    // bytes already, currently we would copy that data again instead of using that existing Vec.
    fn handle_complete_packet(
        &self,
        mut state: atomic_refcell::AtomicRefMut<'_, State>,
        data: &[u8],
        packet_len: usize,
        ident: u32,
        vdt: VorbisDataType,
        seqnums: RangeInclusive<u64>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        match vdt {
            VorbisDataType::RawPacket => {
                let mut data = data;
                let mut packet_len = packet_len;

                if Some(ident) != state.active_ident {
                    gst::info!(
                        CAT,
                        imp = self,
                        "Need to switch codebooks from config {:x?} to new config {:x?}",
                        state.active_ident,
                        ident,
                    );

                    let Ok(new_config) = state.configs.activate_config(ident) else {
                        // Make sure we don't flood the application with warning messages
                        match state.last_warning_message {
                            Some(t) if t.elapsed() < std::time::Duration::from_secs(1) => {}
                            _ => {
                                gst::element_imp_warning!(
                                    self,
                                    gst::StreamError::Decode,
                                    [
                                        "Could not switch codebooks, Vorbis config {ident:x?} not available yet"
                                    ]
                                );
                                state.last_warning_message = Some(std::time::Instant::now());
                            }
                        }

                        self.obj().drop_packets(seqnums);
                        return Ok(gst::FlowSuccess::Ok);
                    };

                    self.activate_config(new_config)?;

                    gst::debug!(CAT, imp = self, "Activated new config {ident:x?}");

                    state.active_ident = Some(ident);
                }

                loop {
                    gst::log!(CAT, imp = self, "Packet length: {packet_len} bytes");

                    if data.len() < packet_len {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Short packet, expected {packet_len} bytes but only have {} byte payload left",
                            data.len()
                        );
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    // TODO: would be good to add durations to outgoing packets, but it's non-trivial
                    let outbuf = gst::Buffer::from_mut_slice(data[..packet_len].to_vec());

                    self.obj()
                        .queue_buffer(PacketToBufferRelation::Seqnums(seqnums.clone()), outbuf)?;

                    data = &data[packet_len..];

                    // Read next packet's length, if any data is left
                    if data.len() < 2 {
                        break;
                    }
                    packet_len = u16::from_be_bytes([data[0], data[1]]) as usize;
                    data = &data[2..];
                }
                Ok(gst::FlowSuccess::Ok)
            }
            VorbisDataType::PackedConfig => {
                gst::debug!(CAT, imp = self, "Processing PackedConfig payload..");

                if data.len() < packet_len || packet_len < 3 {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Decode,
                        [
                            "Short in-band Vorbis configuration ({}, {packet_len})",
                            data.len()
                        ]
                    );
                    return Err(gst::FlowError::Error);
                }

                let mut br = ByteReader::endian(Cursor::new(&data[..packet_len]), BigEndian);
                let headers = match br.parse::<VorbisHeaders>() {
                    Err(e) => {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Decode,
                            ["Failed to parse in-band Vorbis configuration: {e:?}"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                    Ok(headers) => headers,
                };

                let config = VorbisConfig::new(ident, headers);

                gst::debug!(
                    CAT,
                    imp = self,
                    "Got in-band vorbis configuration with ident {:04x}, {}ch @ {}Hz",
                    config.ident(),
                    config.channels(),
                    config.rate()
                );

                state.configs.add_config(config, ConfigSource::InBand);

                Ok(gst::FlowSuccess::Ok)
            }
            VorbisDataType::LegacyComment => {
                gst::debug!(CAT, imp = self, "Ignoring LegacyComment payload");
                Ok(gst::FlowSuccess::Ok)
            }
            _ => unreachable!(),
        }
    }
}
