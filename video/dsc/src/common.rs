// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use std::fmt;

/// Hash method enum shared between signer and verifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstDscHashMethod")]
pub enum HashMethod {
    #[enum_value(name = "SHA-1", nick = "sha1")]
    Sha1,
    #[enum_value(name = "SHA-224", nick = "sha224")]
    Sha224,
    #[enum_value(name = "SHA-256", nick = "sha256")]
    #[default]
    Sha256,
    #[enum_value(name = "SHA-384", nick = "sha384")]
    Sha384,
    #[enum_value(name = "SHA-512", nick = "sha512")]
    Sha512,
}

impl HashMethod {
    pub fn digest_size(self) -> usize {
        match self {
            HashMethod::Sha1 => 20,
            HashMethod::Sha224 => 28,
            HashMethod::Sha256 => 32,
            HashMethod::Sha384 => 48,
            HashMethod::Sha512 => 64,
        }
    }
}

impl fmt::Display for HashMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            HashMethod::Sha1 => "sha1",
            HashMethod::Sha224 => "sha224",
            HashMethod::Sha256 => "sha256",
            HashMethod::Sha384 => "sha384",
            HashMethod::Sha512 => "sha512",
        };
        write!(f, "{}", s)
    }
}

impl TryFrom<u8> for HashMethod {
    type Error = u8;
    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(HashMethod::Sha1),
            1 => Ok(HashMethod::Sha224),
            2 => Ok(HashMethod::Sha256),
            3 => Ok(HashMethod::Sha384),
            4 => Ok(HashMethod::Sha512),
            unknown => Err(unknown),
        }
    }
}

impl From<HashMethod> for u8 {
    fn from(method: HashMethod) -> Self {
        match method {
            HashMethod::Sha1 => 0,
            HashMethod::Sha224 => 1,
            HashMethod::Sha256 => 2,
            HashMethod::Sha384 => 3,
            HashMethod::Sha512 => 4,
        }
    }
}

/// Timeout applied to network fetches of key material.
pub(crate) const URI_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn is_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Fetches the content behind `uri`.
///
/// `file:` and `http(s):` URIs are supported. Plain filesystem paths,
/// including relative ones, have no URI scheme and are read directly.
/// Unsupported URI schemes are rejected.
pub(crate) fn fetch_uri(
    uri: &str,
    timeout: std::time::Duration,
) -> std::result::Result<Vec<u8>, String> {
    if is_windows_drive_path(uri) {
        return std::fs::read(uri).map_err(|e| format!("Failed to read {uri}: {e}"));
    }

    match url::Url::parse(uri) {
        Ok(url) => match url.scheme() {
            "file" => {
                let path = url
                    .to_file_path()
                    .map_err(|_| format!("Invalid file URI: {uri}"))?;
                std::fs::read(&path).map_err(|e| format!("Failed to read {}: {e}", path.display()))
            }
            "http" | "https" => reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| format!("Failed to create HTTP client: {e}"))?
                .get(url.as_str())
                .send()
                .map_err(|e| format!("HTTP request failed: {e}"))?
                .error_for_status()
                .map_err(|e| format!("HTTP error: {e}"))?
                .bytes()
                .map(|b| b.to_vec())
                .map_err(|e| format!("Failed to read HTTP response: {e}")),
            scheme => Err(format!(
                "Unsupported URI scheme '{scheme}' in key source URI: {uri}"
            )),
        },
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            std::fs::read(uri).map_err(|e| format!("Failed to read {uri}: {e}"))
        }
        Err(e) => Err(format!("Invalid URI or path '{uri}': {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_method_round_trip() {
        for (byte, method) in [
            (0u8, HashMethod::Sha1),
            (1, HashMethod::Sha224),
            (2, HashMethod::Sha256),
            (3, HashMethod::Sha384),
            (4, HashMethod::Sha512),
        ] {
            assert_eq!(HashMethod::try_from(byte), Ok(method));
            assert_eq!(u8::from(method), byte);
        }
    }

    #[test]
    fn test_hash_method_rejects_unknown_values() {
        for byte in [5u8, 42, u8::MAX] {
            assert_eq!(HashMethod::try_from(byte), Err(byte));
        }
    }
}
