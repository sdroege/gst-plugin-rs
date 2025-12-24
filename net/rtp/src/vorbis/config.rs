// GStreamer RTP Vorbis Depayloader - Vorbis Config Header Handling
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use bitstream_io::{BigEndian, LittleEndian};
use bitstream_io::{BitRead, BitReader, FromBitStream};
use bitstream_io::{ByteRead, ByteReader, FromByteStream};

use std::collections::VecDeque;
use std::fmt::Debug;
use std::io::{Cursor, SeekFrom};

use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpvorbisconfig",
        gst::DebugColorFlags::empty(),
        Some("RTP Vorbis configuration header handling"),
    )
});

/// Errors that can be produced when parsing a `VorbisConfig` or creating `VorbisHeaders` from caps
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub(crate) enum VorbisConfigParseError {
    #[error("Unsupported Vorbis version {}.", .0)]
    UnsupportedVersion(u32),

    #[error(
        "Unsupported variable-size coded xiph length. Don't support or expect lengths of more than 4 bytes."
    )]
    UnsupportedXiphLength,

    #[error("Unexpected number of headers ({}), expected exactly three headers", .0)]
    UnexpectedNumberOfHeaders(usize),

    #[error("Short {name} header size. Required: {required} bytes, available {available} bytes")]
    WrongConfigHeaderSize {
        name: &'static str,
        required: usize,
        available: usize,
    },

    #[error("Unexpectedly large packed header size ({}), max allowed {}", .0, .1)]
    TooLarge(usize, usize),

    #[error("Invalid {name} header: {reason}")]
    InvalidHeader {
        name: &'static str,
        reason: &'static str,
    },

    #[error("Invalid caps: {reason}")]
    InvalidCaps { reason: &'static str },
}

#[derive(Clone)]
pub(crate) struct VorbisBlockModes {
    n_modes: usize,
    blockflags: u64,
}

impl Debug for VorbisBlockModes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VorbisBlockModes")
            .field("n_modes", &self.n_modes)
            .field(
                "blockflags",
                &format_args!("{:0width$b}", self.blockflags, width = self.n_modes),
            )
            .finish()
    }
}

impl FromBitStream for VorbisBlockModes {
    type Error = anyhow::Error;

    fn from_reader<R: BitRead + ?Sized>(r: &mut R) -> anyhow::Result<Self> {
        let mode_count = 1 + r.read::<6, u8>()? as usize;

        let mut mode_blockflags = 0;

        for m in 0..mode_count {
            let blockflag = r.read_bit()?;
            if blockflag {
                mode_blockflags |= 1u64 << m;
            }
            let windowtype = r.read::<16, u16>()?;
            let transformtype = r.read::<16, u16>()?;
            let _mapping = r.read::<8, u8>()?;
            if windowtype != 0 || transformtype != 0 {
                anyhow::bail!("invalid window type or transform type");
            }
        }

        let framing_bit = r.read_bit()?;
        if !framing_bit {
            anyhow::bail!("invalid framing bit");
        }

        while let Ok(padding_bit) = r.read_bit() {
            if padding_bit {
                anyhow::bail!("invalid padding");
            }
        }

        Ok(VorbisBlockModes {
            n_modes: mode_count,
            blockflags: mode_blockflags,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VorbisInfo {
    channels: u8,
    rate: u32,
    blocksizes: [u16; 2],
    blockmodes: VorbisBlockModes,
}

#[derive(Debug, PartialEq)]
enum PacketSize {
    Short,
    Long,
}

impl VorbisInfo {
    fn from_headers(id_header: &[u8], setup_header: &[u8]) -> anyhow::Result<Self> {
        use VorbisConfigParseError::*;
        use anyhow::bail;

        // Parse ID header
        if !id_header.starts_with("\x01vorbis".as_bytes()) {
            Err(InvalidHeader {
                name: "id",
                reason: "wrong header prefix",
            })?;
        }

        if id_header.len() < 30 {
            Err(WrongConfigHeaderSize {
                name: "id",
                required: 30,
                available: id_header.len(),
            })?;
        }

        let mut br = BitReader::endian(Cursor::new(&id_header[7..]), LittleEndian);

        let vorbis_version = br.read::<32, u32>()?;
        if vorbis_version != 0 {
            Err(UnsupportedVersion(vorbis_version))?;
        }

        let channels = br.read::<8, u8>()?;
        let rate = br.read::<32, u32>()?;

        if channels == 0 || rate == 0 || rate > 192000 {
            Err(InvalidHeader {
                name: "id",
                reason: "invalid channels or sample rate value",
            })?;
        }

        let _bitrate_max = br.read::<32, u32>()?;
        let _bitrate_nom = br.read::<32, u32>()?;
        let _bitrate_min = br.read::<32, u32>()?;

        let blocksize0 = 1u16 << br.read::<4, u8>()?;
        let blocksize1 = 1u16 << br.read::<4, u8>()?;

        // Allowed block sizes: 64, 128, 256, 512, 1024, 2048, 4096, 8192
        #[allow(clippy::manual_range_contains)]
        if blocksize0 < 64 || blocksize0 > 8192 || blocksize1 > 8192 || blocksize1 < blocksize0 {
            Err(InvalidHeader {
                name: "id",
                reason: "invalid blocksize configuration",
            })?;
        }

        let framing_bit = br.read_bit()?;
        if !framing_bit {
            Err(InvalidHeader {
                name: "id",
                reason: "invalid framing bit",
            })?;
        }

        // Parse setup header, at least the interesting bits. Which are of course right at the end.
        if setup_header.len() < 7 {
            Err(WrongConfigHeaderSize {
                name: "setup",
                required: 7,
                available: setup_header.len(),
            })?;
        }

        if !setup_header.starts_with("\x05vorbis".as_bytes()) {
            Err(InvalidHeader {
                name: "setup",
                reason: "wrong header prefix",
            })?;
        }

        // Figure out blockmode parameters which we need to calculate packet durations.
        //
        // Of course this information is at the very end of the setup header with all the codebooks
        // and such. We'll try and guess it by searching for possible values from the end, so that
        // we don't have to parse everything before it, which would be rather tedious.
        //
        let last_byte = *setup_header.last().unwrap();
        if last_byte == 0 {
            Err(InvalidHeader {
                name: "setup",
                reason: "missing framing bit in last byte",
            })?;
        }
        let padding_bits = last_byte.leading_zeros() as usize;

        let mut blockmodes = None;

        for n in (1..=64).rev() {
            let mut br = BitReader::endian(Cursor::new(&setup_header[7..]), LittleEndian);

            let pos_from_end = 6 + n * (1 + 16 + 16 + 8) + 1 + padding_bits;

            if pos_from_end >= setup_header.len() - 7 {
                gst::trace!(CAT, "Trying to find block modes with n={n} -> not viable");
                continue;
            }

            let start_pos = br.seek_bits(SeekFrom::End(pos_from_end as i64)).unwrap();

            gst::trace!(
                CAT,
                "Trying to find block modes with n={n} @ {start_pos} bits.."
            );

            match br.parse::<VorbisBlockModes>() {
                Ok(modes) if modes.n_modes == n => {
                    blockmodes = Some(modes);
                    break;
                }
                _ => continue,
            }
        }

        let Some(blockmodes) = blockmodes else {
            bail!(InvalidHeader {
                name: "setup",
                reason: "could not determine block modes",
            });
        };

        gst::info!(
            CAT,
            "Found block modes: {blockmodes:?}, blockmode bits: {}",
            (blockmodes.n_modes - 1).checked_ilog2().unwrap_or(0) + 1,
        );

        Ok(VorbisInfo {
            channels,
            rate,
            blocksizes: [blocksize0, blocksize1],
            blockmodes,
        })
    }

    pub(crate) fn calc_packet_samples(&self, packet_data: &[u8]) -> anyhow::Result<u64> {
        use anyhow::{Context, bail};

        let mut br = BitReader::endian(Cursor::new(&packet_data), LittleEndian);

        if br.read::<1, u8>().context("packet type bit")? != 0 {
            bail!("Not an audio packet");
        }

        let blockmode_bits = (self.blockmodes.n_modes - 1).checked_ilog2().unwrap_or(0) + 1;

        let blockmode = br.read_var::<u8>(blockmode_bits).context("blockmode")? as usize;

        if blockmode >= self.blockmodes.n_modes {
            gst::warning!(
                CAT,
                "unexpected blockmode {blockmode}, n_modes={}",
                self.blockmodes.n_modes
            );
        }

        use PacketSize::*;

        let packet_size = match self.blockmodes.blockflags & (1 << blockmode) {
            0 => Short,
            _ => Long,
        };

        gst::trace!(CAT, "packet_size: {packet_size:?}");

        let short_size = self.blocksize(Short) as u32;
        let long_size = self.blocksize(Long) as u32;

        /*  v
         * lll:           l/2
         * lls:           3l/4 - s/4
         * lsl:           s/2
         * lss:           s/2
         * sll:           l/4 + s/4
         * sls:           l/2
         * ssl:           s/2
         * sss:           s/2
         */
        let decode_blocksize = if packet_size == Short {
            short_size / 2
        } else {
            let prev_size = match br.read_bit().context("previous_window_flag")? {
                false => Short,
                true => Long,
            };

            let next_size = match br.read_bit().context("next_window_flag")? {
                false => Short,
                true => Long,
            };

            match (prev_size, next_size) {
                (Short, Short) => long_size / 2,
                (Short, Long) => long_size / 4 + short_size / 4,
                (Long, Short) => 3 * (long_size / 4) - short_size / 4,
                (Long, Long) => long_size / 2,
            }
        };

        Ok(decode_blocksize as u64)
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    fn blocksize(&self, size: PacketSize) -> u16 {
        match size {
            PacketSize::Short => self.blocksizes[0],
            PacketSize::Long => self.blocksizes[1],
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VorbisComment {
    vendor: String,
    comments: Vec<String>,
}

impl VorbisComment {
    const MIN_SIZE: usize = 7 + 4 + 4 + 1;

    pub(crate) fn decimate(&mut self, max_size: usize) {
        let mut round = 0;

        loop {
            let current_size = self.size();

            gst::log!(
                CAT,
                "current size: {current_size}, round {round}, target size {max_size}"
            );

            if current_size <= max_size || current_size <= Self::MIN_SIZE {
                break;
            }

            // Find largest comment tag and remove it

            let biggest = self.comments.iter().map(|c| c.len()).enumerate().fold(
                (None, 0),
                |(biggest_idx, biggest_len), (cur_idx, cur_len)| {
                    if biggest_idx.is_none() || biggest_len <= cur_len {
                        (Some(cur_idx), cur_len)
                    } else {
                        (biggest_idx, biggest_len)
                    }
                },
            );

            match biggest {
                // Remove biggest comment
                (Some(idx), len) => {
                    gst::info!(
                        CAT,
                        "removing comment {} @ {idx}, len={len}",
                        self.get_tag_name_for_index(idx)
                    );

                    self.comments.remove(idx);
                }

                // If no more comments are left, there's just the vendor string left to remove
                _ => {
                    gst::info!(CAT, "clearing vendor string");
                    self.vendor
                        .truncate(max_size.saturating_sub(Self::MIN_SIZE));
                }
            }

            round += 1;
        }

        gst::info!(CAT, "decimated size: {}", self.size());
        gst::log!(CAT, "{self:?}");
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut vec = vec![];

        vec.extend_from_slice("\x03vorbis".as_bytes());

        // Vendor string
        vec.extend_from_slice(&(self.vendor.len() as u32).to_le_bytes());
        vec.extend_from_slice(self.vendor.as_bytes());

        // Comments
        vec.extend_from_slice(&(self.comments.len() as u32).to_le_bytes());
        for comment in &self.comments {
            vec.extend_from_slice(&(comment.len() as u32).to_le_bytes());
            vec.extend_from_slice(comment.as_bytes());
        }

        // Framing bit
        vec.push(0x01);

        vec
    }

    fn size(&self) -> usize {
        7 + 4 + self.vendor.len() + 4 + self.comments.iter().fold(0, |acc, c| acc + 4 + c.len()) + 1
    }

    fn get_tag_name_for_index(&self, idx: usize) -> &str {
        match self.comments.get(idx).and_then(|s| s.split_once('=')) {
            Some((left, _right)) => left,
            _ => "<Unknown>",
        }
    }
}

impl FromByteStream for VorbisComment {
    type Error = anyhow::Error;

    fn from_reader<R: ByteRead + ?Sized>(r: &mut R) -> anyhow::Result<Self> {
        use VorbisConfigParseError::*;
        use anyhow::Context;

        // https://xiph.org/vorbis/doc/Vorbis_I_spec.html#x1-620004.2.1

        let mut vorbis_marker = [0u8; 7];

        r.read_bytes(&mut vorbis_marker).context("id header")?;

        if vorbis_marker != "\x03vorbis".as_bytes() {
            Err(InvalidHeader {
                name: "comment",
                reason: "wrong header prefix",
            })?;
        }

        // https://xiph.org/vorbis/doc/Vorbis_I_spec.html#x1-820005

        let vendor_len = r.read::<u32>().context("vendor_length")? as usize;
        let vendor_bytes = r.read_to_vec(vendor_len).context("vendor_string")?;
        let vendor = String::from_utf8(vendor_bytes).context("vendor_string into utf-8")?;

        gst::trace!(CAT, "vendor: {vendor}");

        let list_len = r.read::<u32>().context("user_comment_list_length")? as usize;

        // Not pre-allocating capacity on purpose here since it comes from external data
        let mut comments = vec![];

        for i in 0..list_len {
            let comment_len = r.read::<u32>().context("user_comment_list_length")? as usize;
            let comment_bytes = r.read_to_vec(comment_len).context("comment".to_string())?;
            let comment = String::from_utf8(comment_bytes).context("comment into utf-8")?;

            gst::trace!(
                CAT,
                "comment {i}: {comment:.200} {}",
                if comment.len() >= 200 { "..." } else { "" }
            );

            comments.push(comment);
        }

        let framing_bit = r.read::<u8>().context("framing_bit")?;

        if framing_bit != 0x01 {
            anyhow::bail!("framing bit unset");
        }

        Ok(VorbisComment { vendor, comments })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VorbisHeaders {
    headers: [Vec<u8>; 3],
    info: VorbisInfo,
}

impl VorbisHeaders {
    fn new(
        id_header: Vec<u8>,
        comment_header: Vec<u8>,
        setup_header: Vec<u8>,
    ) -> anyhow::Result<Self> {
        use VorbisConfigParseError::*;

        let info = VorbisInfo::from_headers(&id_header, &setup_header)?;

        gst::info!(CAT, "vorbis info: {:?}", info);

        if comment_header.len() < 7 {
            Err(WrongConfigHeaderSize {
                name: "comment",
                required: 7,
                available: comment_header.len(),
            })?;
        }

        if !comment_header.starts_with("\x03vorbis".as_bytes()) {
            Err(InvalidHeader {
                name: "comment",
                reason: "wrong header prefix",
            })?;
        }

        Ok(VorbisHeaders {
            headers: [id_header, comment_header, setup_header],
            info,
        })
    }

    fn into_vecs(self) -> [Vec<u8>; 3] {
        self.headers
    }

    fn pack_headers(
        id_hdr: &[u8],
        comment_hdr: &[u8],
        setup_hdr: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        use VorbisConfigParseError::*;

        let id_len = id_hdr.len();
        let comment_len = comment_hdr.len();
        let setup_len = setup_hdr.len();
        let total_len = id_len + comment_len + setup_len;
        let max_size = u16::MAX as usize;

        if total_len >= max_size {
            // Do we need to subtract a few bytes for the xiph header packing overhead?
            // Todo: for bonus points we could try and decimate the comment header since it's most likely some big coverart
            Err(TooLarge(total_len, max_size))?
        }

        let mut packed = Vec::<u8>::with_capacity(1 + 4 + 4 + total_len);

        packed.push(3 - 1); // n_headers-1
        packed.extend_from_slice(&write_xiph_length(id_len));
        packed.extend_from_slice(&write_xiph_length(comment_len));
        packed.extend_from_slice(id_hdr);
        packed.extend_from_slice(comment_hdr);
        packed.extend_from_slice(setup_hdr);

        Ok(packed)
    }

    pub(crate) fn from_streamheaders(headers: gst::ArrayRef) -> anyhow::Result<Self> {
        use VorbisConfigParseError::*;

        let headers = headers.as_slice();

        if headers.len() != 3 {
            Err(UnexpectedNumberOfHeaders(headers.len()))?;
        }

        let (Ok(id_buf), Ok(comment_buf), Ok(setup_buf)) = (
            headers[0].get::<gst::Buffer>(),
            headers[1].get::<gst::Buffer>(),
            headers[2].get::<gst::Buffer>(),
        ) else {
            Err(InvalidCaps {
                reason: "Expected buffers in streamheader array in caps",
            })?
        };

        let id_map = id_buf.map_readable().unwrap();
        let comment_map = comment_buf.map_readable().unwrap();
        let setup_map = setup_buf.map_readable().unwrap();

        VorbisHeaders::new(id_map.to_vec(), comment_map.to_vec(), setup_map.to_vec())
    }

    // Make up a 24-bit ident value for these headers
    //
    pub(crate) fn create_ident(&self) -> u32 {
        use fnv_rs::FnvHasher;

        let mut hasher = fnv_rs::Fnv32::new();
        hasher.update(&self.headers[0]);
        hasher.update(&self.headers[1]);
        hasher.update(&self.headers[2]);
        let hash = hasher.finalize();

        let hash32 = u32::from_be_bytes(hash.as_bytes().try_into().unwrap());

        (hash32 & 0x00ffffff) ^ (hash32 >> 24) // Turn 32 bits into 24 bits
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VorbisConfig {
    ident: u32,
    headers: VorbisHeaders,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConfigSource {
    OutOfBand,
    InBand,
}

#[derive(Default)]
pub(crate) struct ConfigPool {
    // Limit number of configurations to avoid unbounded memory usage
    max_configs: usize,

    configs: VecDeque<(ConfigSource, VorbisConfig)>,
}

impl VorbisConfig {
    pub(crate) fn new(ident: u32, headers: VorbisHeaders) -> Self {
        VorbisConfig { ident, headers }
    }

    pub(crate) fn ident(&self) -> u32 {
        self.ident
    }

    pub(crate) fn channels(&self) -> u8 {
        self.headers.info.channels()
    }

    pub(crate) fn rate(&self) -> u32 {
        self.headers.info.rate()
    }

    pub(crate) fn info(&self) -> VorbisInfo {
        self.headers.info.clone()
    }

    pub(crate) fn into_headers(self) -> [Vec<u8>; 3] {
        self.headers.into_vecs()
    }

    // Return a single packed header config encoded as base64 string, for use in SDPs
    pub(crate) fn configuration_string(&self) -> anyhow::Result<String> {
        // Max size of the three headers minus some bytes for the variable-sized preamble
        const MAX_HEADERS_LEN: usize = u16::MAX as usize - (4 * 16);

        use data_encoding::Encoding;

        let mut output = String::new();

        static BASE: Encoding = data_encoding::BASE64;
        let mut encoder = BASE.new_encoder(&mut output);

        encoder.append(&1u32.to_be_bytes()); // number of packed headers: 1

        // packed header
        encoder.append(&self.ident.to_be_bytes()[1..4]); // 24-bit ident

        let id_hdr = &self.headers.headers[0];
        let mut comment_hdr = &self.headers.headers[1]; // comment header to use
        let setup_hdr = &self.headers.headers[2];

        let total_len = id_hdr.len() + comment_hdr.len() + setup_hdr.len();

        gst::log!(CAT, "total len: {total_len}");
        gst::log!(CAT, "id len: {}", id_hdr.len());
        gst::log!(CAT, "comment len: {}", comment_hdr.len());
        gst::log!(CAT, "setup len: {}", setup_hdr.len());

        let mut comment_vec = None; // decimated comment header (storage to keep it alive)

        if total_len >= MAX_HEADERS_LEN {
            // Decimate the comment header (e.g. remove cover art) until everything fits
            let mut br = ByteReader::endian(Cursor::new(&comment_hdr), LittleEndian);
            let mut comment = br.parse::<VorbisComment>()?;
            let max_size = MAX_HEADERS_LEN.saturating_sub(id_hdr.len() + setup_hdr.len());
            comment.decimate(max_size);
            comment_vec = Some(comment.to_vec());
            comment_hdr = comment_vec.as_ref().unwrap();
        }

        // Recalculate, might have changed if we decimated the comment header
        let total_len = id_hdr.len() + comment_hdr.len() + setup_hdr.len();

        // Final check just to be sure, shouldn't happen
        if total_len >= MAX_HEADERS_LEN {
            anyhow::bail!(
                "Vorbis comment too long, \
                can't be packed into RTP vorbis configuration, \
                and failed to decimate it"
            );
        }

        if comment_vec.is_some() {
            gst::log!(CAT, "New total len: {total_len}");
            gst::log!(CAT, "New id len: {}", id_hdr.len());
            gst::log!(CAT, "New comment len: {}", comment_hdr.len());
            gst::log!(CAT, "New setup len: {}", setup_hdr.len());
        }

        encoder.append(&(total_len as u16).to_be_bytes());
        encoder.append(&write_xiph_length(3 - 1)); // n_headers - 1
        encoder.append(&write_xiph_length(id_hdr.len()));
        encoder.append(&write_xiph_length(comment_hdr.len()));
        // setup_len is implicit

        encoder.append(id_hdr);
        encoder.append(comment_hdr);
        encoder.append(setup_hdr);

        encoder.finalize();

        Ok(output)
    }

    pub(crate) fn to_packed(&self) -> anyhow::Result<Vec<u8>> {
        VorbisHeaders::pack_headers(
            &self.headers.headers[0],
            &self.headers.headers[1],
            &self.headers.headers[2],
        )
    }
}

fn read_xiph_length<R: ByteRead + ?Sized>(br: &mut R) -> anyhow::Result<usize> {
    use VorbisConfigParseError::*;
    use anyhow::Context;

    let mut len: usize = 0;

    for i in 0.. {
        let b = br.read::<u8>().context("xiph length")?;

        len = (len << 7) | (b & 0x7f) as usize;

        if b & 0x80 == 0 {
            break;
        }

        if i == 3 {
            Err(UnsupportedXiphLength)?;
        }
    }

    Ok(len)
}

fn write_xiph_length(len: usize) -> Vec<u8> {
    const MORE_FLAG: u8 = 0x80;

    // Could write into a caller-provided stack-allocated buffer,
    // but not really perf sensitive, so KISS for now.
    let mut v = Vec::with_capacity(8);

    let mut len = len;

    loop {
        let b = (len & 0x7f) as u8;

        v.insert(0, b | MORE_FLAG);

        len >>= 7;

        if len == 0 {
            break;
        }
    }

    // Clear more flag for last part
    let last = v.last_mut().expect("last");
    *last &= !MORE_FLAG;

    v
}

#[test]
fn test_vorbis_xiph_length_read_write() {
    for value in [81, 981, 98765] {
        let bytes = write_xiph_length(value);

        eprintln!("{value} ({value:04x}) as xiph length: {bytes:02x?}");

        let mut r = ByteReader::endian(Cursor::new(&bytes), BigEndian);
        let len: usize = read_xiph_length(&mut r).unwrap();

        assert_eq!(len, value);
    }

    const MORE_FLAG: u8 = 0x80;
    assert_eq!(!MORE_FLAG, 0x7f);
}

impl FromByteStream for VorbisHeaders {
    type Error = anyhow::Error;

    fn from_reader<R: ByteRead + ?Sized>(r: &mut R) -> anyhow::Result<Self> {
        use VorbisConfigParseError::*;
        use anyhow::Context;

        let n_headers = 1 + read_xiph_length(r).context("n_headers")?;

        if n_headers != 3 {
            Err(UnexpectedNumberOfHeaders(n_headers))?;
        }

        // Read header lengths. Last header length (setup header) is implicit.
        let id_len = read_xiph_length(r).context("id header length")?;
        let comment_len = read_xiph_length(r).context("comment header length")?;

        gst::trace!(CAT, "id len: {}", id_len);
        gst::trace!(CAT, "comment len: {}", comment_len);

        // Read identification header
        let id_header = r.read_to_vec(id_len).context("id header")?;

        // Read comment header
        let comment_header = r.read_to_vec(comment_len).context("comment header")?;

        // Read setup header
        // This is dumb, but not sure there's a better way to read to the end currently
        let mut setup_header = Vec::with_capacity(4096);
        while let Ok(b) = r.read::<u8>() {
            setup_header.push(b);
        }

        gst::trace!(CAT, "setup len: {}", setup_header.len());

        VorbisHeaders::new(id_header, comment_header, setup_header)
    }
}

// Total length of all headers must be able to fit into a 16 bit length,
// so any individual header should be shorter than this.
const MAX_PACKED_HEADER_SIZE: usize = u16::MAX as usize;

impl FromByteStream for VorbisConfig {
    type Error = anyhow::Error;

    // Parses as single Packed Header of a set of Packed Headers
    // https://www.rfc-editor.org/rfc/rfc5215.html#section-3.2.1
    fn from_reader<R: ByteRead + ?Sized>(r: &mut R) -> anyhow::Result<Self> {
        use VorbisConfigParseError::*;
        use anyhow::Context;

        let ident: u32 = (r.read::<u8>().context("ident")? as u32) << 16
            | (r.read::<u8>().context("ident")? as u32) << 8
            | (r.read::<u8>().context("ident")? as u32);

        // This is actually the length of the id + comment + setup headers, but doesn't
        // include the various lengths fields before the actual headers :eyeroll:
        let mut len = r.read::<u16>().context("packed header length")? as usize;

        if len > MAX_PACKED_HEADER_SIZE {
            Err(TooLarge(len, MAX_PACKED_HEADER_SIZE))?;
        }

        let mut max_buf = vec![0u8; MAX_PACKED_HEADER_SIZE];

        // Peek at lengths fields so we can figure out how many bytes those take up in addition to len
        let buf = &mut max_buf[0..8];
        r.read_bytes(buf).context("packed header lengths")?;

        {
            let mut lengths_reader = ByteReader::endian(Cursor::new(&buf), BigEndian);

            // We expect exactly 3 headers
            if lengths_reader.read::<u8>().context("n_headers")? != 2 {
                Err(UnexpectedNumberOfHeaders(buf[0] as usize + 1))?;
            }

            // ID header length should fit into 7 bits
            if read_xiph_length(&mut lengths_reader).context("id header length")? >= 0x80 {
                Err(InvalidHeader {
                    name: "id",
                    reason: "unexpectedly large ID header length",
                })?;
            }

            // Comment header length could be more than 1 byte of var length
            let _ = read_xiph_length(&mut lengths_reader).context("comment header length")?;

            // Setup header length is implicit, but can also calculate based on total length

            // Current position is size of length fields, add that to the amount of data we need
            // for this packed header
            let pos = lengths_reader.reader().position();
            len += pos as usize;
        }

        max_buf.resize(len, 0);

        // Read rest of data now that we know how many bytes the lengths fields take up
        let buf = &mut max_buf[8..len];

        r.read_bytes(buf).context("packed header")?;

        // Recover the whole data including the peeked bytes
        let buf = &max_buf[0..len];

        let mut hdr_reader = ByteReader::endian(Cursor::new(&buf), BigEndian);
        let headers = hdr_reader.parse::<VorbisHeaders>()?;
        let config = VorbisConfig::new(ident, headers);

        Ok(config)
    }
}

impl ConfigPool {
    pub(crate) fn new(max_configs: usize) -> Self {
        ConfigPool {
            max_configs,
            ..Default::default()
        }
    }

    fn prune_configs(&mut self) {
        let (_, first_config) = self.configs.front().unwrap();
        let first_ident = first_config.ident();

        let mut idx_to_prune = None;

        for (idx, (source, config)) in self.configs.iter().enumerate().rev() {
            // Always keep OutOfBand configs for now, only prune old InBand configs.
            // Configs get moved to the front once activated, so the ones further in
            // the back should be ok to discard.
            if *source == ConfigSource::InBand && config.ident() != first_ident {
                idx_to_prune = Some(idx);
                break;
            }
        }

        if let Some(idx) = idx_to_prune {
            let (source, removed_config) = self.configs.remove(idx).unwrap();

            gst::debug!(
                CAT,
                "Pruned {source:?} config with ident {}",
                removed_config.ident()
            );
        }
    }

    // Should this return an error? What to do if we have more out of band configs than max_configs?
    pub(crate) fn add_config(&mut self, config: VorbisConfig, source: ConfigSource) {
        use ConfigSource::*;

        let ident = config.ident();

        if let Some(existing_pos) = self.configs.iter().position(|(_, c)| c.ident() == ident) {
            // Perhaps we should not overwrite OutOfBand configs with InBand configs because the
            // OOB ones might have full metadata for example that could've been cropped for
            // in-band transmission? For now just always replace the existing one though.
            let (source, _) = &self.configs[existing_pos];

            gst::debug!(
                CAT,
                "Found existing {source:?} config with same ident {ident}, removing"
            );

            self.configs.remove(existing_pos);
        }

        if source == OutOfBand {
            self.configs.push_back((OutOfBand, config));
        } else {
            self.configs.push_front((InBand, config));
        }

        let n_configs = self.configs.len();

        gst::debug!(
            CAT,
            "Added {source:?} config with ident {ident}, {n_configs} configs now in total"
        );

        if n_configs > self.max_configs {
            self.prune_configs();
        }
    }

    pub(crate) fn activate_config(&mut self, ident: u32) -> Result<VorbisConfig, ()> {
        gst::log!(CAT, "Activate config with ident {ident}..");

        let Some(pos) = self.configs.iter().position(|(_, c)| c.ident() == ident) else {
            gst::warning!(CAT, "No config with ident {ident} found");
            return Err(());
        };

        let (source, config) = self.configs.remove(pos).unwrap();

        gst::debug!(CAT, "Found existing {source:?} config with ident {ident}");

        let cloned_config = config.clone();
        self.configs.push_front((source, config));
        Ok(cloned_config)
    }

    pub(crate) fn front(&self) -> Option<&VorbisConfig> {
        self.configs.front().map(|(_, config)| config)
    }
}
