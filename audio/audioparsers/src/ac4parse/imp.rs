// Copyright (C) 2026 Fluendo S.A.
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use bitstream_io::{BigEndian, BitRead, BitReader};
use std::{
    io::Cursor,
    sync::{LazyLock, Mutex},
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ac4parse",
        gst::DebugColorFlags::empty(),
        Some("AC4 parser"),
    )
});

const MIN_AC4_FRAME_SIZE: usize = 8;
const AC4_SYNCWORD_NO_CRC: u16 = 0xAC40;
const AC4_SYNCWORD_WITH_CRC: u16 = 0xAC41;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alignment {
    None,
    Frame,
}

// Frame rate information for AC-4 streams - based on ETSI TS 103 190-1
#[derive(Debug, Clone, Copy)]
struct FrameRate {
    numerator: u32,
    denominator: u32,
}

// Table 83: frame_rate_index for 48 kHz, 96 kHz, and 192 kHz
const FRAME_RATE_TABLE_48KHZ: [FrameRate; 16] = [
    FrameRate {
        numerator: 24000,
        denominator: 1001,
    }, // 0: 23.976 fps
    FrameRate {
        numerator: 24,
        denominator: 1,
    }, // 1: 24 fps
    FrameRate {
        numerator: 25,
        denominator: 1,
    }, // 2: 25 fps
    FrameRate {
        numerator: 30000,
        denominator: 1001,
    }, // 3: 29.97 fps
    FrameRate {
        numerator: 30,
        denominator: 1,
    }, // 4: 30 fps
    FrameRate {
        numerator: 48000,
        denominator: 1001,
    }, // 5: 47.952 fps
    FrameRate {
        numerator: 48,
        denominator: 1,
    }, // 6: 48 fps
    FrameRate {
        numerator: 50,
        denominator: 1,
    }, // 7: 50 fps
    FrameRate {
        numerator: 60000,
        denominator: 1001,
    }, // 8: 59.94 fps
    FrameRate {
        numerator: 60,
        denominator: 1,
    }, // 9: 60 fps
    FrameRate {
        numerator: 100,
        denominator: 1,
    }, // 10: 100 fps
    FrameRate {
        numerator: 120000,
        denominator: 1001,
    }, // 11: 119.88 fps
    FrameRate {
        numerator: 120,
        denominator: 1,
    }, // 12: 120 fps
    FrameRate {
        numerator: 375,
        denominator: 16,
    }, // 13: 23.4375 fps (48000/2048)
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 14: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 15: reserved
];

// Table 84: frame_rate_index for 44.1 kHz
const FRAME_RATE_TABLE_44_1KHZ: [FrameRate; 16] = [
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 0: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 1: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 2: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 3: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 4: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 5: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 6: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 7: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 8: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 9: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 10: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 11: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 12: reserved
    FrameRate {
        numerator: 11025,
        denominator: 512,
    }, // 13: 44100/2048 fps
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 14: reserved
    FrameRate {
        numerator: 0,
        denominator: 1,
    }, // 15: reserved
];

// AC-4 frame information extracted from TOC
#[derive(Debug, Clone, Copy)]
struct FrameInfo {
    rate: u32,
    framerate_numerator: u32,
    framerate_denominator: u32,
    bitstream_version: u32,
    source_change: bool,
}

#[derive(Default)]
pub struct Ac4Parse {
    state: Mutex<State>,
}

struct State {
    sample_rate: Option<u32>,
    frame_rate_numerator: Option<u32>,
    frame_rate_denominator: Option<u32>,
    bitstream_version: Option<u32>,
    sequence_counter_prev: Option<u16>,
    alignment: Alignment,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sample_rate: None,
            frame_rate_numerator: None,
            frame_rate_denominator: None,
            bitstream_version: None,
            sequence_counter_prev: None,
            alignment: Alignment::None,
        }
    }
}

impl Ac4Parse {
    fn find_syncword(data: &[u8]) -> Option<(usize, bool)> {
        if data.len() < 2 {
            return None;
        }

        for (i, window) in data.windows(2).enumerate() {
            let syncword: u16 = u16::from_be_bytes(window.try_into().unwrap());

            if syncword == AC4_SYNCWORD_NO_CRC {
                return Some((i, false));
            } else if syncword == AC4_SYNCWORD_WITH_CRC {
                return Some((i, true));
            }
        }
        None
    }

    fn get_framesize(data: &[u8], has_crc: bool) -> Result<(usize, bool), ()> {
        if data.len() < 4 {
            return Err(());
        }

        let framesize_parser = u16::from_be_bytes([data[2], data[3]]);
        let (frame_size, extended_frame_size) = if framesize_parser != 0xFFFF {
            // 16-bit frame size
            (framesize_parser as u32, false)
        } else {
            // 24-bit frame size
            if data.len() < 7 {
                return Err(());
            }
            let frame_size = u32::from_be_bytes([0, data[4], data[5], data[6]]);
            (frame_size, true)
        };

        let header_size = if extended_frame_size { 7 } else { 4 };
        let total_frame_size = header_size + frame_size as usize + if has_crc { 2 } else { 0 };

        Ok((total_frame_size, extended_frame_size))
    }

    // Read variable-length bits field according to TS 103 190-1 Section 4.2.2
    fn read_variable_bits<R: std::io::Read>(
        reader: &mut BitReader<R, BigEndian>,
        n_bits: u32,
    ) -> std::io::Result<u32> {
        let mut value: u32 = 0;

        assert!(n_bits <= 16);

        loop {
            value += reader.read_var::<u32>(n_bits)?;
            let b_read_more: u8 = reader.read_var::<u8>(1)?;

            if b_read_more == 1 {
                value <<= n_bits;
                value += 1 << n_bits;
            } else {
                break;
            }
        }
        Ok(value)
    }

    /// Parse AC-4 Table of Contents to extract frame information
    /// Based on ETSI TS 103 190-1 - Annex G
    fn parse_toc(
        &self,
        data: &[u8],
        extended_frame_size: bool,
        state: &mut State,
    ) -> std::io::Result<FrameInfo> {
        let mut reader = BitReader::endian(Cursor::new(data), BigEndian);

        if extended_frame_size {
            reader.skip(56)?; // syncword (16) + frame_size_indicator (16) + frame_size (24)
        } else {
            reader.skip(32)?; // syncword (16) + frame_size (16)
        }

        let mut bitstream_version: u32 = reader.read::<2, u32>()?; // bitstream_version
        if bitstream_version == 3 {
            bitstream_version += Self::read_variable_bits(&mut reader, 2)?;
        }

        let sequence_counter: u16 = reader.read::<10, u16>()?; // sequence_counter

        // Detect source changes according to ETSI TS 103 190-1, section 4.3.3.2.2
        let mut source_change = false;
        if let Some(prev) = state.sequence_counter_prev {
            let counter_increase = sequence_counter == prev.wrapping_add(1);
            let counter_wrap = (sequence_counter == 1) && (prev == 1020);
            let splice_detected = (sequence_counter != 0) && (prev == 0);

            if !counter_increase && !counter_wrap && !splice_detected {
                source_change = true;
                gst::info!(
                    CAT,
                    imp = self,
                    "Source change detected: sequence_counter={}, prev={}",
                    sequence_counter,
                    prev
                );
            }
        }

        // Store current counter for next frame
        state.sequence_counter_prev = Some(sequence_counter);

        let b_wait_frames: u32 = reader.read::<1, u32>()?; // b_wait_frames
        if b_wait_frames == 1 {
            let wait_frames: u32 = reader.read::<3, u32>()?; // wait_frames
            if wait_frames > 0 {
                reader.skip(2)?; // reserved
            }
        }

        let fs_index: u32 = reader.read::<1, u32>()?; // fs_index
        let frame_rate_index: u32 = reader.read::<4, u32>()?; // frame_rate_index

        // The TOC continues, but we stop here for now as we have what we need

        // Note: AC-4 allows higher fs (96 kHz, 192 kHz) for substreams, but this requires heavy
        // parsing of the TOC (b_sf_multiplier, sf_multiplier). We're going to ignore
        // those cases for now and just use the Base sampling frequency.
        let rate = if fs_index == 0 { 44100 } else { 48000 };

        // Get frame rate from lookup table
        let frame_rate = if fs_index == 0 {
            &FRAME_RATE_TABLE_44_1KHZ[frame_rate_index as usize]
        } else {
            &FRAME_RATE_TABLE_48KHZ[frame_rate_index as usize]
        };

        gst::log!(CAT, imp = self, "Parsed sample rate: {rate}");
        gst::log!(
            CAT,
            imp = self,
            "Parsed frame rate: {}/{} ({:.3} FPS)",
            frame_rate.numerator,
            frame_rate.denominator,
            frame_rate.numerator as f64 / frame_rate.denominator as f64
        );

        Ok(FrameInfo {
            rate,
            framerate_numerator: frame_rate.numerator,
            framerate_denominator: frame_rate.denominator,
            bitstream_version,
            source_change,
        })
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Ac4Parse {
    const NAME: &'static str = "GstAc4Parse";
    type Type = super::Ac4Parse;
    type ParentType = gst_base::BaseParse;
}

impl ObjectImpl for Ac4Parse {}

impl GstObjectImpl for Ac4Parse {}

impl ElementImpl for Ac4Parse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "AC4 audio stream parser",
                "Codec/Parser/Audio",
                "Parses AC4 audio streams",
                "Pablo García Sancho <pgarcia@fluendo.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_caps = gst::Caps::builder("audio/x-ac4")
                .field("framed", true)
                .field("rate", gst::List::new([44100, 48000]))
                .field(
                    "framerate",
                    gst::FractionRange::new(
                        gst::Fraction::new(0, 1),
                        gst::Fraction::new(i32::MAX, 1),
                    ),
                )
                .field("bitstream-version", gst::List::new([1, 2]))
                .field("alignment", "frame")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            let sink_caps = gst::Caps::builder_full()
                .structure(gst::Structure::new_empty("audio/x-ac4"))
                .structure(gst::Structure::new_empty("audio/ac4"))
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseParseImpl for Ac4Parse {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting AC4 parser");

        self.obj().set_min_frame_size(MIN_AC4_FRAME_SIZE as u32);

        let mut state = self.state.lock().unwrap();
        *state = State::default();

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping AC4 parser");
        Ok(())
    }

    fn handle_frame(
        &self,
        mut frame: gst_base::BaseParseFrame,
    ) -> Result<(gst::FlowSuccess, u32), gst::FlowError> {
        let input = frame.buffer().ok_or_else(|| {
            gst::error!(CAT, imp = self, "No input buffer");
            gst::FlowError::Error
        })?;

        let map = input.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map input buffer readable");
            gst::FlowError::Error
        })?;

        // Check minimum size
        if map.size() < MIN_AC4_FRAME_SIZE {
            gst::trace!(CAT, imp = self, "Buffer too small: {} bytes", map.size());
            return Ok((gst::FlowSuccess::Ok, 1));
        }

        // Find syncword
        let has_crc = match Self::find_syncword(&map) {
            Some((0, has_crc)) => {
                // Syncword at start
                gst::log!(
                    CAT,
                    imp = self,
                    "AC4 syncword found at offset 0 (CRC: {})",
                    has_crc
                );
                has_crc
            }
            Some((offset, has_crc)) => {
                // Syncword found but not at start - skip to it
                gst::log!(
                    CAT,
                    imp = self,
                    "AC4 syncword found at offset {} (CRC: {}), skipping",
                    offset,
                    has_crc
                );
                return Ok((gst::FlowSuccess::Ok, offset as u32));
            }
            None => {
                // No syncword found - skip all but last 3 bytes
                // (syncword might span buffer boundaries)
                let skip = map.size().saturating_sub(3);
                gst::log!(
                    CAT,
                    imp = self,
                    "No AC4 syncword found, skipping {} bytes",
                    skip
                );
                return Ok((gst::FlowSuccess::Ok, skip as u32));
            }
        };

        // Get frame size
        let (framesize, extended_frame_size) = match Self::get_framesize(&map, has_crc) {
            Ok(result) => result,
            Err(_) => {
                gst::error!(CAT, imp = self, "Failed to parse frame size");
                return Ok((gst::FlowSuccess::Ok, 1));
            }
        };

        if framesize == 0 {
            gst::error!(CAT, imp = self, "Invalid frame size: 0");
            return Ok((gst::FlowSuccess::Ok, 1));
        }

        gst::log!(
            CAT,
            imp = self,
            "AC4 frame: size={}, has_crc={}, extended={}",
            framesize,
            has_crc,
            extended_frame_size
        );

        // Need more data? BaseParse will accumulate
        if framesize > map.size() {
            gst::trace!(
                CAT,
                imp = self,
                "Need more data: have {}, need {}",
                map.size(),
                framesize
            );
            drop(map);
            // Tell BaseParse to accumulate at least this much data before calling us again
            self.obj().set_min_frame_size(framesize as u32);
            return Ok((gst::FlowSuccess::Ok, 0));
        }

        // Sync validation - verify next frame has a valid syncword when resyncing
        if self.obj().lost_sync() && !self.obj().is_draining() {
            gst::debug!(
                CAT,
                imp = self,
                "Resyncing; checking for next frame syncword"
            );

            if framesize + 2 > map.size() {
                gst::warning!(CAT, imp = self, "Not enough data to check next syncword");
                drop(map);
                self.obj()
                    .set_min_frame_size((framesize + MIN_AC4_FRAME_SIZE) as u32);
                return Ok((gst::FlowSuccess::Ok, 0));
            }

            // Check if next syncword is valid
            let next_word = u16::from_be_bytes([map[framesize], map[framesize + 1]]);
            if next_word != AC4_SYNCWORD_NO_CRC && next_word != AC4_SYNCWORD_WITH_CRC {
                gst::debug!(
                    CAT,
                    imp = self,
                    "0x{:04x} not valid AC-4 syncword, skipping",
                    next_word
                );
                return Ok((gst::FlowSuccess::Ok, 2));
            }

            gst::debug!(CAT, imp = self, "Valid syncword found, ending resync");
        }

        // Parse TOC to extract frame information
        let mut state = self.state.lock().unwrap();
        let frame_info = match self.parse_toc(&map, extended_frame_size, &mut state) {
            Ok(info) => {
                gst::log!(
                    CAT,
                    imp = self,
                    "Parsed frame info: rate={}, framerate={}/{}, bitstream_version={}, source_change={}",
                    info.rate,
                    info.framerate_numerator,
                    info.framerate_denominator,
                    info.bitstream_version,
                    info.source_change
                );
                info
            }
            Err(e) => {
                gst::error!(CAT, imp = self, "Failed to parse TOC: {}", e);
                drop(state);
                return Ok((gst::FlowSuccess::Ok, 1));
            }
        };

        drop(map);

        // Set alignment to Frame on first frame
        if state.alignment == Alignment::None {
            state.alignment = Alignment::Frame;
        }

        // Set caps on first frame, when sample rate or frame rate changes, or on source change
        let caps_changed = state.sample_rate != Some(frame_info.rate)
            || state.frame_rate_numerator != Some(frame_info.framerate_numerator)
            || state.frame_rate_denominator != Some(frame_info.framerate_denominator)
            || state.bitstream_version != Some(frame_info.bitstream_version)
            || frame_info.source_change;

        if caps_changed {
            state.sample_rate = Some(frame_info.rate);
            state.frame_rate_numerator = Some(frame_info.framerate_numerator);
            state.frame_rate_denominator = Some(frame_info.framerate_denominator);
            state.bitstream_version = Some(frame_info.bitstream_version);

            let caps = gst::Caps::builder("audio/x-ac4")
                .field("framed", true)
                .field("alignment", "frame")
                .field("rate", frame_info.rate as i32)
                .field(
                    "framerate",
                    gst::Fraction::new(
                        frame_info.framerate_numerator as i32,
                        frame_info.framerate_denominator as i32,
                    ),
                )
                .field("bitstream-version", frame_info.bitstream_version as i32)
                .build();

            gst::debug!(CAT, imp = self, "Setting caps: {}", caps);
            self.obj()
                .src_pad()
                .push_event(gst::event::Caps::new(&caps));
        }

        drop(state);

        // Calculate buffer duration based on frame rate
        // duration = 1 second * framerate_denominator / framerate_numerator
        let duration = gst::ClockTime::SECOND
            .mul_div_ceil(
                frame_info.framerate_denominator as u64,
                frame_info.framerate_numerator as u64,
            )
            .unwrap_or(gst::ClockTime::ZERO);

        gst::log!(
            CAT,
            imp = self,
            "Frame duration: {}/{} fps = {:?}",
            frame_info.framerate_numerator,
            frame_info.framerate_denominator,
            duration
        );

        let output = frame.buffer_mut().unwrap();
        output.set_duration(duration);

        // Finish frame - BaseParse will push it downstream
        self.obj().finish_frame(frame, framesize as u32)?;

        // Reset min frame size for next frame
        self.obj().set_min_frame_size(MIN_AC4_FRAME_SIZE as u32);

        Ok((gst::FlowSuccess::Ok, 0))
    }

    fn sink_caps(&self, filter: Option<&gst::Caps>) -> gst::Caps {
        let template_caps = self.obj().sink_pad().pad_template_caps();

        // Query downstream (SRC!!) peer pads
        let mut peer_caps = {
            if let Some(f) = filter {
                let mut f_copy = f.to_owned();
                remove_fields(&mut f_copy);
                self.obj().src_pad().peer_query_caps(Some(&f_copy))
            } else {
                self.obj().src_pad().peer_query_caps(None)
            }
        };
        remove_fields(&mut peer_caps);

        let mut negotiated_caps =
            peer_caps.intersect_with_mode(&template_caps, gst::CapsIntersectMode::First);

        if let Some(f) = filter {
            negotiated_caps =
                f.intersect_with_mode(&negotiated_caps, gst::CapsIntersectMode::First);
        }

        negotiated_caps
    }
}

// Remove "framed" field from the upstream caps filter to prevent it from interfiring with the caps negotiation,
// as some upstream elements may not have "framed" in their caps
fn remove_fields(caps: &mut gst::Caps) {
    let caps_mut = caps.make_mut();
    for s in caps_mut.iter_mut() {
        s.remove_field("framed");
    }
}
