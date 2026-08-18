// APE tag parser
//
// Copyright (C) 2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt;

use crate::apedemux::apedemux::CAT;

pub const HEADER_FOOTER_SIZE: usize = 32;

#[derive(Debug)]
pub enum ItemType {
    Text,
    Binary,
    Locator,
    Unknown,
}

pub const APE_TAG_FLAG_CONTAINS_HEADER: u32 = 1 << 31;
pub const APE_TAG_FLAG_CONTAINS_FOOTER: u32 = 1 << 30;
pub const APE_TAG_FLAG_IS_HEADER: u32 = 1 << 29;

pub struct Flags(u32);

impl fmt::Debug for Flags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Flags(0x{:08x?})", self.0)
    }
}

impl Flags {
    fn from_bytes(data: [u8; 4]) -> Self {
        let flags = u32::from_le_bytes(data);

        Self(flags)
    }

    pub fn has_header(&self) -> bool {
        (self.0 & APE_TAG_FLAG_CONTAINS_HEADER) != 0
    }

    pub fn has_footer(&self) -> bool {
        (self.0 & APE_TAG_FLAG_CONTAINS_FOOTER) != 0
    }

    #[allow(unused)]
    pub fn is_header(&self) -> bool {
        (self.0 & APE_TAG_FLAG_IS_HEADER) != 0
    }

    #[allow(unused)]
    pub fn is_footer(&self) -> bool {
        !self.is_header()
    }

    fn item_type(&self) -> ItemType {
        match (self.0 >> 1) & 0b11 {
            0 => ItemType::Text,
            1 => ItemType::Binary,
            2 => ItemType::Locator,
            _ => ItemType::Unknown,
        }
    }
}

#[derive(Debug)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    fn from_bytes(data: [u8; 4]) -> Self {
        let version_num = u32::from_le_bytes(data);

        let major = version_num / 1000;
        let minor = version_num % 1000;

        Self { major, minor }
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.major == other.major && self.minor == other.minor
    }
}

// This could be a header or a footer
#[derive(Debug)]
pub struct Marker {
    pub version: Version,
    pub size: usize,
    pub n_items: usize,
    pub global_flags: Flags,
}

impl Marker {
    pub fn from_bytes(data: &[u8; 32]) -> anyhow::Result<Self> {
        if !data.starts_with(b"APETAGEX") {
            anyhow::bail!("Expected APETAGEX header at start of marker");
        }

        let (chunks, _) = data.as_chunks::<4>();

        // Skip APETAGEX header and 64 MBZ bits at the end
        let chunks = &chunks[2..][..4];

        let version = Version::from_bytes(chunks[0]);

        let size = u32::from_le_bytes(chunks[1]) as usize;
        let n_items = u32::from_le_bytes(chunks[2]) as usize;

        let global_flags = match version.major {
            1 => Flags::from_bytes([0, 0, 0, 0]),
            2 => Flags::from_bytes(chunks[3]),
            _ => anyhow::bail!(
                "Unsupported APE tag version {}.{}",
                version.major,
                version.minor
            ),
        };

        Ok(Self {
            version,
            size,
            n_items,
            global_flags,
        })
    }

    pub fn has_header(&self) -> bool {
        self.global_flags.has_header()
    }

    pub fn has_footer(&self) -> bool {
        self.global_flags.has_footer()
    }

    pub fn items<'a>(&self, data: &'a [u8]) -> ItemIter<'a> {
        ItemIter::new(data)
    }
}

pub enum ItemValue<'a> {
    Text(&'a str),
    Locator(&'a str),
    Binary(&'a [u8]),
    #[allow(unused)]
    Unknown(&'a [u8]),
}

// Custom Debug impl so we don't print the full image data into the debug log
impl fmt::Debug for ItemValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemValue::Text(s) => write!(f, "Text('{s}')"),
            ItemValue::Locator(s) => write!(f, "Locator('{s}')"),
            ItemValue::Unknown(_) => write!(f, "Unknown"),
            ItemValue::Binary(data) => {
                if data.len() > 64 {
                    write!(f, "Binary({:02x?}..)", &data[0..64])
                } else {
                    write!(f, "Binary({data:02x?}..)")
                }
            }
        }
    }
}

impl PartialEq for Marker {
    fn eq(&self, other: &Self) -> bool {
        const FLAG_MASK: u32 = !APE_TAG_FLAG_IS_HEADER;

        self.version == other.version
            && self.size == other.size
            && self.n_items == other.n_items
            && self.global_flags.0 & FLAG_MASK == other.global_flags.0 & FLAG_MASK
    }
}

#[derive(Debug)]
pub struct Item<'a> {
    pub key: &'a str,
    pub value: ItemValue<'a>,
}

pub struct ItemIter<'a> {
    data: &'a [u8],
}

impl<'a> ItemIter<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
}

impl<'a> Iterator for ItemIter<'a> {
    type Item = Item<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = loop {
            let data = &self.data;

            if data.is_empty() {
                return None;
            }

            if data.len() < 4 + 4 + 2 + 1 {
                gst::warning!(
                    CAT,
                    "Not enough data left for item header, {} bytes",
                    data.len()
                );
                return None;
            }

            let value_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            let flags = Flags::from_bytes([data[4], data[5], data[6], data[7]]);

            let data = &data[8..];

            // split_once() would probably be nicer, but it's still nightly API as of October 2026
            let Some(terminator_pos) = data.iter().position(|&b| b == b'\0') else {
                gst::warning!(CAT, "Missing key terminator");
                return None;
            };

            let (key, value) = data.split_at(terminator_pos);

            // First byte in value will be the nul terminator, so skip that
            let value = &value[1..];

            if value.len() < value_len {
                gst::warning!(
                    CAT,
                    "Not enough data left for value, have {} bytes, need {value_len} bytes",
                    value.len(),
                );
                return None;
            }

            let (value, remainder) = value.split_at(value_len);
            self.data = remainder;

            // From here on we can skip to the next item

            let key = match str::from_utf8(key) {
                Ok(s) => s,
                Err(e) => {
                    gst::warning!(CAT, "key is not valid UTF-8: {e}, skipping item");
                    continue;
                }
            };

            if !key.is_ascii() {
                gst::warning!(CAT, "key is not ASCII, skipping item");
                continue;
            }

            let value = match flags.item_type() {
                ItemType::Text => {
                    let text = match str::from_utf8(value) {
                        Ok(s) => s,
                        Err(e) => {
                            gst::warning!(CAT, "Text value is not valid UTF-8: {e}, skipping item");
                            continue;
                        }
                    };

                    ItemValue::Text(text)
                }

                ItemType::Locator => {
                    let text = match str::from_utf8(value) {
                        Ok(s) => s,
                        Err(e) => {
                            gst::warning!(
                                CAT,
                                "Locator value is not valid UTF-8: {e}, skipping item"
                            );
                            continue;
                        }
                    };

                    // Todo: check allowed format?
                    ItemValue::Locator(text)
                }

                ItemType::Binary => ItemValue::Binary(value),

                _ => ItemValue::Unknown(value),
            };

            break Item { key, value };
        };

        Some(item)
    }
}
