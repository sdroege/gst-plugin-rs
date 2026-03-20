// GStreamer RTSP Source 2
//
// Copyright (C) 2026 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use rtsp_types::Method;
use std::str::FromStr;
use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub enum DigestAlgorithm {
    Md5,
    Sha256,
    Sha512,
}

impl FromStr for DigestAlgorithm {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("md5") {
            Ok(Self::Md5)
        } else if s.eq_ignore_ascii_case("sha-256") {
            Ok(Self::Sha256)
        } else if s.eq_ignore_ascii_case("sha-512-256") {
            Ok(Self::Sha512)
        } else {
            Err(format!("Unsupported algorithm: {s}"))
        }
    }
}

impl std::fmt::Display for DigestAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            DigestAlgorithm::Md5 => write!(f, "MD5"),
            DigestAlgorithm::Sha256 => write!(f, "SHA-256"),
            DigestAlgorithm::Sha512 => write!(f, "SHA-512-256"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DigestParams {
    pub realm: String,
    pub nonce: String,
    pub algorithm: DigestAlgorithm,
    pub qop: Option<String>,
    pub opaque: Option<String>,
}

impl Default for DigestParams {
    fn default() -> Self {
        DigestParams {
            realm: String::new(),
            nonce: String::new(),
            algorithm: DigestAlgorithm::Md5,
            qop: None,
            opaque: None,
        }
    }
}

// Adapted from `rtsp-types` crate, `quoted_string` implementation.
fn get_quoted_string(input: &str) -> Option<(&str, &str)> {
    if !input.starts_with('"') {
        return None;
    }

    // Skip opening quote
    let mut chars = input.char_indices().skip(1);
    while let Some((idx, ch)) = chars.next() {
        if ch == '\\' {
            // Jump over the escaped character
            let _ = chars.next();
        } else if ch == '"' {
            // Found closing quote
            let (quoted, remainder) = input.split_at(idx + 1);
            return Some((quoted, remainder));
        }
    }

    // Malformed: no closing quote
    None
}

fn unescape_value(input: &str) -> String {
    // Strips quotes (if present) and perform a single-pass unescape.
    let inner = if input.starts_with('"') && input.ends_with('"') && input.len() >= 2 {
        &input[1..input.len() - 1]
    } else {
        input
    };

    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                result.push(next);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

fn process_part(part: &str, params: &mut DigestParams) {
    let Some((key, value)) = part.trim().split_once('=') else {
        return;
    };

    let key = key.trim();
    let unescaped = unescape_value(value.trim());

    match key {
        "realm" => params.realm = unescaped,
        "nonce" => params.nonce = unescaped,
        "algorithm" => {
            if let Ok(alg) = DigestAlgorithm::from_str(&unescaped) {
                params.algorithm = alg;
            }
        }
        "qop" => params.qop = Some(unescaped),
        "opaque" => params.opaque = Some(unescaped),
        _ => {}
    }
}

pub fn parse_digest_params(challenge: &str) -> Option<DigestParams> {
    let mut input = challenge.strip_prefix("Digest ")?.trim();
    let mut params = DigestParams::default();

    while !input.is_empty() {
        // Find the next comma or end of string, respecting quotes
        let mut comma_pos = None;
        let mut char_indices = input.char_indices().peekable();

        while let Some((idx, ch)) = char_indices.next() {
            if ch == '"' {
                // Jump the iterator past the quoted block
                if let Some((quoted, _)) = get_quoted_string(&input[idx..]) {
                    // Fast-forward the iterator
                    for _ in 0..quoted.chars().count() - 1 {
                        char_indices.next();
                    }
                } else {
                    // Unclosed quote
                    return None;
                }
            } else if ch == ',' {
                comma_pos = Some(idx);
                break;
            }
        }

        // Process the segment before the comma (or the whole thing)
        let (part, rest) = match comma_pos {
            Some(pos) => (&input[..pos], &input[pos + 1..]),
            None => (input, ""),
        };

        process_part(part, &mut params);
        input = rest.trim();
    }

    // Digest MUST have a realm and a nonce
    if params.realm.is_empty() || params.nonce.is_empty() {
        return None;
    }

    Some(params)
}

pub fn compute_digest_response(
    params: &DigestParams,
    method: &Method,
    uri: &Url,
    username: &str,
    password: &str,
    cnonce: &str,
    nc: &str,
) -> String {
    use md5::{
        Digest,
        digest::{OutputSizeUser, generic_array::ArrayLength},
    };
    use std::ops::Add;

    fn compute_digest<D: Digest>(
        params: &DigestParams,
        method: &Method,
        uri: &Url,
        username: &str,
        password: &str,
        cnonce: &str,
        nc: &str,
    ) -> String
    where
        <D as OutputSizeUser>::OutputSize: Add,
        <<D as OutputSizeUser>::OutputSize as Add>::Output: ArrayLength<u8>,
    {
        let ha1 = {
            let mut hasher = D::new();
            hasher.update(format!("{}:{}:{}", username, params.realm, password));
            format!("{:x}", hasher.finalize())
        };

        let ha2 = {
            let mut hasher = D::new();
            hasher.update(format!("{}:{}", <&str>::from(method), uri));
            format!("{:x}", hasher.finalize())
        };

        if let Some(qop) = &params.qop {
            let mut hasher = D::new();
            hasher.update(format!(
                "{}:{}:{}:{}:{}:{}",
                ha1, params.nonce, nc, cnonce, qop, ha2
            ));
            format!("{:x}", hasher.finalize())
        } else {
            let mut hasher = D::new();
            hasher.update(format!("{}:{}:{}", ha1, params.nonce, ha2));
            format!("{:x}", hasher.finalize())
        }
    }

    match params.algorithm {
        DigestAlgorithm::Md5 => {
            use md5::Md5;
            compute_digest::<Md5>(params, method, uri, username, password, cnonce, nc)
        }
        DigestAlgorithm::Sha256 => {
            use sha2::Sha256;
            compute_digest::<Sha256>(params, method, uri, username, password, cnonce, nc)
        }
        DigestAlgorithm::Sha512 => {
            use sha2::Sha512_256;
            compute_digest::<Sha512_256>(params, method, uri, username, password, cnonce, nc)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_gstreamer_challenge() {
        let challenge =
            "Digest realm=\"GStreamer RTSP Server\", nonce=\"c8aa9f5031ccfec3\", algorithm=MD5";
        let params = parse_digest_params(challenge).expect("Should parse valid challenge");

        assert_eq!(params.realm, "GStreamer RTSP Server");
        assert_eq!(params.nonce, "c8aa9f5031ccfec3");
        assert_eq!(params.algorithm, DigestAlgorithm::Md5);
    }

    #[test]
    fn test_commas_inside_quotes() {
        let challenge =
            "Digest realm=\"Living Room, Camera 1\", nonce=\"12345\", qop=\"auth,auth-int\"";
        let params = parse_digest_params(challenge).expect("Should handle internal commas");

        assert_eq!(params.realm, "Living Room, Camera 1");
        assert_eq!(params.nonce, "12345");
        assert_eq!(params.qop, Some("auth,auth-int".to_string()));
    }

    #[test]
    fn test_unquoted_values() {
        // Some headers might not quote the algorithm or qop
        let challenge = "Digest realm=\"test\", nonce=\"abc\", algorithm=MD5, qop=auth";
        let params = parse_digest_params(challenge).unwrap();

        assert_eq!(params.nonce, "abc");
        assert_eq!(params.qop, Some("auth".to_string()));
    }

    #[test]
    fn test_extra_whitespace_and_trailing_commas() {
        let challenge = "Digest   realm = \"space\" ,  nonce= \"123\" , ";
        let params = parse_digest_params(challenge).unwrap();

        assert_eq!(params.realm, "space");
        assert_eq!(params.nonce, "123");
    }

    #[test]
    fn test_malformed_prefix() {
        let challenge = "Basic realm=\"wrong_type\"";
        let params = parse_digest_params(challenge);
        assert!(params.is_none(), "Should return None for non-Digest auth");
    }

    #[test]
    fn test_missing_required_fields() {
        // If the server doesn't send a nonce, we can't do Digest auth
        let challenge = "Digest algorithm=MD5";
        let params = parse_digest_params(challenge);
        assert!(params.is_none(), "Should fail if nonce/realm are missing");
    }

    #[test]
    fn test_escaped_quotes_in_realm() {
        // The realm contains an escaped quote: \"
        let challenge = "Digest realm=\"The \\\"Official\\\" Server\", nonce=\"abc\"";
        let params = parse_digest_params(challenge).unwrap();

        // We expect the internal quotes to be preserved and the backslash removed
        assert_eq!(params.realm, "The \"Official\" Server");
    }

    #[test]
    fn test_unicode_safety() {
        // Test for Unicode. "Sparkle" is 4 bytes. If we used index 'i'
        // incorrectly, this would panic.
        let challenge = "Digest realm=\"✨Sparkle✨\", nonce=\"xyz123\"";
        let params = parse_digest_params(challenge).unwrap();

        assert_eq!(params.realm, "✨Sparkle✨");
        assert_eq!(params.nonce, "xyz123");
    }

    #[test]
    fn test_escaped_backslash() {
        let challenge = "Digest realm=\"D:\\\\Windows\", nonce=\"123\"";
        let params = parse_digest_params(challenge).unwrap();

        assert_eq!(params.realm, "D:\\Windows");
    }

    #[test]
    fn test_multiple_escapes_and_commas() {
        // Comma inside quotes, and escaped characters
        let challenge = "Digest realm=\"Hello, \\\"User\\\"\", nonce=\"nonce,with,commas\"";
        let params = parse_digest_params(challenge).unwrap();

        assert_eq!(params.realm, "Hello, \"User\"");
        assert_eq!(params.nonce, "nonce,with,commas");
    }

    #[test]
    fn test_unclosed_quote_failure() {
        let challenge = "Digest realm=\"Unclosed quote, nonce=\"123\"";
        let params = parse_digest_params(challenge);
        assert!(params.is_none());
    }

    #[test]
    fn test_escaped_backslash_at_end() {
        let challenge = "Digest realm=\"Ends with backslash\\\\\", nonce=\"abc\"";
        let params = parse_digest_params(challenge).unwrap();
        assert_eq!(params.realm, "Ends with backslash\\");
    }

    #[test]
    fn test_complex_escaping_and_token_mix() {
        // Mixture of quoted with escapes and unquoted tokens
        let challenge = "Digest realm=\"Home \\\"Sweet\\\" Home\", nonce=\"12345\", algorithm=MD5, qop=\"auth\"";
        let params = parse_digest_params(challenge).expect("Should parse with valid nonce");

        assert_eq!(params.realm, "Home \"Sweet\" Home");
        assert_eq!(params.nonce, "12345");
        assert_eq!(params.algorithm, DigestAlgorithm::Md5);
        assert_eq!(params.qop, Some("auth".to_string()));
    }

    #[test]
    fn test_unclosed_quote_fails_explicitly() {
        // The quote before 'oops' is never closed
        let challenge = "Digest realm=\"oops, nonce=\"123\"";
        let params = parse_digest_params(challenge);

        // This should be None because the parser reached the end
        // while still 'in_quotes'
        assert!(params.is_none(), "Should fail due to unclosed quote");
    }
}
