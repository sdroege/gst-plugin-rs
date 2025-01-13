// GStreamer RTP Vorbis Depayloader - Vorbis Config Header Handling
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use bitstream_io::{BigEndian, ByteRead, ByteReader, FromByteStream};
use bitstream_io::{BitRead, BitReader, LittleEndian};

use std::collections::VecDeque;
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

#[derive(Clone, Default, Debug)]
pub(crate) struct VorbisInfo {
    channels: u8,
    rate: i32,
}

impl VorbisInfo {
    fn from_headers(id_header: &[u8], setup_header: &[u8]) -> anyhow::Result<Self> {
        use VorbisConfigParseError::*;
        use anyhow::bail;

        if !id_header.starts_with("\x01vorbis".as_bytes()) {
            Err(InvalidHeader {
                name: "id",
                reason: "wrong header prefix",
            })?;
        }

        if id_header.len() < 22 {
            Err(WrongConfigHeaderSize {
                name: "id",
                required: 22,
                available: id_header.len(),
            })?;
        }

        let channels = id_header[11];
        let rate = u32::from_le_bytes([id_header[12], id_header[13], id_header[14], id_header[15]]);

        if channels == 0 || rate == 0 || rate > 192000 {
            Err(InvalidHeader {
                name: "id",
                reason: "invalid channels or sample rate value",
            })?;
        }

        let rate = rate as i32;

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

        Ok(VorbisInfo { channels, rate })
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn rate(&self) -> i32 {
        self.rate
    }
}

#[derive(Clone, Default, Debug)]
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

        // Create a packed config that we can parse with all the error checking that entails.
        // We don't really need to do that, we can assume the headers from the caps are correct,
        // but why not catch problems early if we can.
        let packed = Self::pack_headers(&id_map, &comment_map, &setup_map)?;

        let mut br = ByteReader::endian(Cursor::new(&packed), BigEndian);
        br.parse::<VorbisHeaders>()
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

#[derive(Clone, Default, Debug)]
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

    pub(crate) fn rate(&self) -> i32 {
        self.headers.info.rate()
    }

    pub(crate) fn into_headers(self) -> [Vec<u8>; 3] {
        self.headers.into_vecs()
    }

    // Return a single packed header config encoded as base64 string, for use in SDPs
    pub(crate) fn configuration_string(&self) -> Result<String, ()> {
        use data_encoding::Encoding;

        let mut output = String::new();

        static BASE: Encoding = data_encoding::BASE64;
        let mut encoder = BASE.new_encoder(&mut output);

        encoder.append(&1u32.to_be_bytes()); // number of packed headers: 1

        // packed header
        encoder.append(&self.ident.to_be_bytes()[1..4]); // 24-bit ident

        let id_len = self.headers.headers[0].len();
        let comment_len = self.headers.headers[1].len();
        let setup_len = self.headers.headers[2].len();

        let total_len = id_len + comment_len + setup_len;

        if total_len > u16::MAX as usize {
            // Todo: could decimate the comment header (e.g. remove cover art) until everything fits
            return Err(());
        }

        encoder.append(&(total_len as u16).to_be_bytes());
        encoder.append(&write_xiph_length(3 - 1)); // n_headers - 1
        encoder.append(&write_xiph_length(id_len));
        encoder.append(&write_xiph_length(comment_len));
        // setup_len is implicit

        encoder.append(&self.headers.headers[0]);
        encoder.append(&self.headers.headers[1]);
        encoder.append(&self.headers.headers[2]);

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
