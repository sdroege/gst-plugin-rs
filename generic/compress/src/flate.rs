// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/// Compression method shared by `flatecompress` and `flatedecompress`.
/// The method is inferred from caps negotiation and stored as `Option<FlateMethod>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlateMethod {
    Zlib = 1,
    Deflate = 2,
}

impl FlateMethod {
    /// All variants; useful for building caps or iterating methods.
    pub const ALL: [Self; 2] = [Self::Zlib, Self::Deflate];

    /// Returns the compressed media type.
    pub fn media_type(self) -> &'static str {
        match self {
            FlateMethod::Zlib => "application/x-zlib-compressed",
            FlateMethod::Deflate => "application/x-deflate-compressed",
        }
    }

    /// Returns whether to include a zlib header.
    pub fn zlib_header(self) -> bool {
        match self {
            FlateMethod::Zlib => true,
            FlateMethod::Deflate => false,
        }
    }
}
