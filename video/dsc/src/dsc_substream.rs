// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use anyhow::{Result, anyhow};
use gst::slice::ByteSliceExt;
use sha1::{Digest as _, Sha1};
use sha2::{Sha224, Sha256, Sha384, Sha512};
use std::sync::LazyLock;

use crate::common::HashMethod;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "dsc-substream",
        gst::DebugColorFlags::empty(),
        Some("DSC Substream Manager"),
    )
});

#[derive(Clone)]
pub struct DscSubstream {
    hasher: Option<DscHasher>,
}

#[derive(Clone)]
enum DscHasher {
    Sha1(Sha1),
    Sha224(Sha224),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
}

impl DscHasher {
    fn new(hash_method: HashMethod) -> Self {
        match hash_method {
            HashMethod::Sha1 => Self::Sha1(Sha1::new()),
            HashMethod::Sha224 => Self::Sha224(Sha224::new()),
            HashMethod::Sha256 => Self::Sha256(Sha256::new()),
            HashMethod::Sha384 => Self::Sha384(Sha384::new()),
            HashMethod::Sha512 => Self::Sha512(Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha1(h) => h.update(data),
            Self::Sha224(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
            Self::Sha384(h) => h.update(data),
            Self::Sha512(h) => h.update(data),
        }
    }

    fn finalize(self) -> Vec<u8> {
        match self {
            Self::Sha1(h) => h.finalize().to_vec(),
            Self::Sha224(h) => h.finalize().to_vec(),
            Self::Sha256(h) => h.finalize().to_vec(),
            Self::Sha384(h) => h.finalize().to_vec(),
            Self::Sha512(h) => h.finalize().to_vec(),
        }
    }
}

impl DscSubstream {
    pub fn new(hash_method: HashMethod) -> Result<Self> {
        let hasher = DscHasher::new(hash_method);
        Ok(Self {
            hasher: Some(hasher),
        })
    }

    pub fn add_to_substream(&mut self, data: &[u8]) -> Result<()> {
        if self.hasher.is_none() {
            return Err(anyhow!("Substream hasher not initialized"));
        }

        if let Some(ref mut hasher) = self.hasher {
            gst::trace!(
                CAT,
                "DscSubstream::add_to_substream - adding {} bytes",
                data.len()
            );
            gst::trace!(CAT, "  First 32 bytes: {}", data.dump_range(..32));
            gst::trace!(
                CAT,
                "  Last 32 bytes: {}",
                data.dump_range(data.len().saturating_sub(32)..)
            );

            gst::debug!(
                CAT,
                "  → Hashing {} bytes: {}...",
                data.len(),
                data.dump_range(..std::cmp::min(16, data.len()))
            );

            hasher.update(data);
        }

        Ok(())
    }

    pub fn finalize(&mut self) -> Result<Vec<u8>> {
        if let Some(hasher) = self.hasher.take() {
            let digest = hasher.finalize();
            gst::debug!(CAT, "Finalized substream digest: {} bytes", digest.len());
            gst::debug!(CAT, "  Full digest: {}", digest.dump_range(..));
            Ok(digest)
        } else {
            Err(anyhow!("Substream already finalized"))
        }
    }
}

pub struct DscSubstreamManager {
    hash_method_byte: u8,
    content_uuid: Option<[u8; 16]>,

    substreams: Vec<Option<DscSubstream>>,

    pub last_digest: Option<Vec<u8>>,
}

impl DscSubstreamManager {
    pub fn new(
        hash_method: HashMethod,
        hash_method_byte: u8,
        content_uuid: Option<[u8; 16]>,
    ) -> Result<Self> {
        // Initialize the first substream
        let substream = DscSubstream::new(hash_method)?;

        Ok(Self {
            hash_method_byte,
            content_uuid,
            substreams: vec![Some(substream)],
            last_digest: None,
        })
    }

    pub fn add_to_substream(&mut self, substream_id: usize, data: &[u8]) -> Result<()> {
        if substream_id >= self.substreams.len() {
            return Err(anyhow!("Invalid substream ID: {}", substream_id));
        }

        if self.substreams[substream_id].is_none() {
            return Err(anyhow!("Substream {} not initialized", substream_id));
        }

        gst::trace!(
            CAT,
            "DscSubstreamManager::add_to_substream - substream {}, {} bytes total",
            substream_id,
            data.len()
        );

        if let Some(ref mut substream) = self.substreams[substream_id] {
            substream.add_to_substream(data)?;
        }

        Ok(())
    }

    // Creates the data packet that will be signed: [ref_digest][current_digest][hash_method][uuid?]
    pub fn create_data_packet(&mut self, substream_id: usize) -> Result<Vec<u8>> {
        let current_digest = self.finalize_substream(substream_id)?;

        gst::debug!(CAT, "Creating data packet for substream {}", substream_id);
        gst::debug!(
            CAT,
            "Current digest ({} bytes): {}",
            current_digest.len(),
            current_digest.dump_range(..)
        );

        // current_digest + ref_digest (which is the same size) + hash_method_byte
        let mut total_capacity = current_digest.len() * 2 + 1; //
        if let Some(ref uuid) = self.content_uuid {
            total_capacity += uuid.len();
        }
        let mut data_packet = Vec::with_capacity(total_capacity);

        // Reference digest (all 0xFF for first GOP, or last digest from previous GOP)
        let ref_digest = if let Some(ref last) = self.last_digest {
            gst::debug!(
                CAT,
                "Using previous digest as reference ({} bytes)",
                last.len()
            );
            last.clone()
        } else {
            gst::debug!(CAT, "First GOP - using all 0xFF as reference digest");
            vec![0xFF; current_digest.len()]
        };

        gst::debug!(
            CAT,
            "Reference digest ({} bytes): {}",
            ref_digest.len(),
            ref_digest.dump_range(..std::cmp::min(32, ref_digest.len()))
        );
        data_packet.extend_from_slice(&ref_digest);

        // Current digest
        data_packet.extend_from_slice(&current_digest);

        // Hash method type byte
        data_packet.push(self.hash_method_byte);
        gst::debug!(CAT, "Hash method byte: {}", self.hash_method_byte);

        // Content UUID (if present)
        if let Some(ref uuid) = self.content_uuid {
            data_packet.extend_from_slice(uuid);
            gst::debug!(CAT, "Added content UUID: {}", uuid.dump_range(..));
        }

        gst::debug!(
            CAT,
            "Final data packet ({} bytes): first 32: {}, last 32: {}",
            data_packet.len(),
            data_packet.dump_range(..std::cmp::min(32, data_packet.len())),
            data_packet.dump_range(data_packet.len().saturating_sub(32)..)
        );

        // Store current digest for next GOP
        self.last_digest = Some(current_digest);

        Ok(data_packet)
    }

    fn finalize_substream(&mut self, substream_id: usize) -> Result<Vec<u8>> {
        if let Some(ref mut substream) = self
            .substreams
            .get_mut(substream_id)
            .and_then(|s| s.as_mut())
        {
            let digest = substream.finalize()?;
            self.substreams[substream_id] = None;
            Ok(digest)
        } else {
            Err(anyhow!(
                "Substream {} not found or already finalized",
                substream_id
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dsc_substream_manager_basic() {
        let hash_method = HashMethod::Sha256;
        let mut manager = DscSubstreamManager::new(hash_method, 2, None).unwrap();

        // Add some test NAL unit data
        let nal_data1 = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x80]; // SPS-like
        let nal_data2 = vec![0x00, 0x00, 0x00, 0x01, 0x68, 0x48, 0x90]; // PPS-like

        manager.add_to_substream(0, &nal_data1).unwrap();
        manager.add_to_substream(0, &nal_data2).unwrap();

        // Create data packet (this finalizes the substream)
        let data_packet = manager.create_data_packet(0).unwrap();

        // Should contain: zero_digest + current_digest + hash_method_byte
        // For SHA256: 32 + 32 + 1 = 65 bytes
        assert_eq!(data_packet.len(), 65);
        assert_eq!(data_packet[64], 2); // hash_method_byte
    }

    #[test]
    fn test_dsc_substream_manager_with_content_uuid() {
        let hash_method = HashMethod::Sha256;
        let content_uuid = Some([0u8; 16]);
        let mut manager = DscSubstreamManager::new(hash_method, 2, content_uuid).unwrap();

        let nal_data = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        manager.add_to_substream(0, &nal_data).unwrap();

        let data_packet = manager.create_data_packet(0).unwrap();

        // Should contain: zero_digest + current_digest + hash_method_byte + uuid
        // For SHA256: 32 + 32 + 1 + 16 = 81 bytes
        assert_eq!(data_packet.len(), 81);
        assert_eq!(&data_packet[65..81], &[0u8; 16]); // UUID at the end
    }
}
