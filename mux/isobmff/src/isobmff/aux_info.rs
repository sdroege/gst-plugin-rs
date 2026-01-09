// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::isobmff::boxes::{FULL_BOX_VERSION_0, FULL_BOX_VERSION_1, write_full_box};
use crate::isobmff::{AuxiliaryInformation, AuxiliaryInformationData};
use anyhow::Error;

impl AuxiliaryInformation {
    pub(crate) fn write_full_saiz(
        &self,
        v: &mut Vec<u8>,
        data: &AuxiliaryInformationData,
    ) -> Result<(), Error> {
        if let Some(aux_info_type) = self.aux_info_type {
            write_full_box(v, b"saiz", FULL_BOX_VERSION_0, 1, |v| {
                v.extend(aux_info_type);
                v.extend(self.aux_info_type_parameter.to_be_bytes());
                data.write_saiz_entries(v)
            })?;
        } else {
            write_full_box(v, b"saiz", FULL_BOX_VERSION_0, 0, |v| {
                data.write_saiz_entries(v)
            })?;
        }
        Ok(())
    }

    pub(crate) fn write_full_saio(
        &self,
        v: &mut Vec<u8>,
        data: &AuxiliaryInformationData,
    ) -> Result<(), Error> {
        let version = if *data.chunk_offsets.iter().max().unwrap_or(&0) > (u32::MAX as u64) {
            FULL_BOX_VERSION_1
        } else {
            FULL_BOX_VERSION_0
        };
        if let Some(aux_info_type) = self.aux_info_type {
            write_full_box(v, b"saio", version, 1, |v| {
                v.extend(aux_info_type);
                v.extend(self.aux_info_type_parameter.to_be_bytes());
                if version == FULL_BOX_VERSION_0 {
                    data.write_saio_offsets_v0(v)
                } else {
                    data.write_saio_offsets_v1(v)
                }
            })?;
        } else {
            write_full_box(v, b"saio", version, 0, |v| {
                if version == FULL_BOX_VERSION_0 {
                    data.write_saio_offsets_v0(v)
                } else {
                    data.write_saio_offsets_v1(v)
                }
            })?;
        }
        Ok(())
    }
}

impl AuxiliaryInformationData {
    fn write_saiz_entries(&self, v: &mut Vec<u8>) -> Result<(), Error> {
        assert!(!self.entry_lengths.is_empty());
        let first_entry_length = self.entry_lengths[0];
        if self
            .entry_lengths
            .iter()
            .all(|entry_size| *entry_size == first_entry_length)
        {
            v.extend(first_entry_length.to_be_bytes());
            v.extend((self.entry_lengths.len() as u32).to_be_bytes());
        } else {
            v.extend(0u8.to_be_bytes());
            v.extend((self.entry_lengths.len() as u32).to_be_bytes());
            v.extend(&self.entry_lengths);
        }
        Ok(())
    }

    fn write_saio_offsets_v0(&self, v: &mut Vec<u8>) -> Result<(), Error> {
        v.extend((self.chunk_offsets.len() as u32).to_be_bytes());
        for offset in &self.chunk_offsets {
            v.extend((*offset as u32).to_be_bytes());
        }
        Ok(())
    }

    fn write_saio_offsets_v1(&self, v: &mut Vec<u8>) -> Result<(), Error> {
        v.extend((self.chunk_offsets.len() as u32).to_be_bytes());
        for offset in &self.chunk_offsets {
            v.extend(offset.to_be_bytes());
        }
        Ok(())
    }
}
