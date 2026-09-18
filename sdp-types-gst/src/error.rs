// Copyright (C) 2024 Nirbheek Chauhan <nirbheek@centricular.com>
// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Typed errors for SDP parsing and attribute helpers.

use std::num::ParseIntError;

use thiserror::Error;

/// Error returned when parsing an SDP session fails.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The session is not syntactically valid SDP.
    #[error("invalid SDP syntax: {0}")]
    Syntax(#[from] sdp_types::ParserError),
    /// A session-level attribute is invalid.
    #[error(transparent)]
    SessionAttribute(#[from] AttributeParseError),
    /// The session-level connection data (`c=` line) is invalid.
    #[error("invalid session connection data: {source}")]
    Connection {
        #[source]
        source: MediaError,
    },
    /// A session-level grouping attribute (`a=group:`) is invalid.
    #[error("invalid session group attribute: {source}")]
    Group {
        #[source]
        source: sdp_types::AttributeError,
    },
    /// The value of a session-level attribute could not be parsed.
    #[error("invalid session attribute '{name}': {source}")]
    AttributeValue {
        name: String,
        #[source]
        source: sdp_types::AttributeError,
    },
    /// One of the media lines of the session is invalid.
    #[error("invalid media {mline}: {source}")]
    Media {
        /// Index of the invalid media line in the session.
        mline: u32,
        #[source]
        source: MediaError,
    },
}

/// Something invalid about a single SDP media line.
#[derive(Debug, Error)]
pub enum MediaError {
    /// The media type of the `m=` line is not a known type.
    #[error("invalid media type")]
    MediaType(#[source] sdp_types::ParseEnumError),
    /// The media type of the `m=` line is not supported.
    #[error("unsupported media type '{0}'")]
    UnsupportedMediaType(String),
    /// The transport protocol of the `m=` line is not supported.
    #[error("unsupported transport protocol '{0}'")]
    UnsupportedProtocol(String),
    /// An attribute has too many occurrences, an unexpected value or is
    /// missing a value.
    #[error(transparent)]
    Attribute(#[from] AttributeParseError),
    /// The value of an attribute could not be parsed.
    #[error("invalid '{name}' attribute")]
    TypedAttribute {
        name: String,
        #[source]
        source: sdp_types::AttributeError,
    },
    /// The numeric value of an attribute could not be parsed.
    #[error("invalid '{name}' attribute")]
    NumericAttribute {
        name: String,
        #[source]
        source: ParseIntError,
    },
    /// The value of an attribute is malformed.
    #[error("invalid '{name}' attribute: {reason}")]
    InvalidAttribute { name: String, reason: String },
    /// Something is not a valid RTP payload type number.
    #[error("invalid payload type '{0}'")]
    InvalidPayloadType(String),
    /// An attribute refers to a payload type that is not part of the `m=`
    /// line.
    #[error("'{name}' attribute for payload type {pt} not present in media")]
    UnknownPayloadType { name: String, pt: u8 },
    /// An attribute occurred multiple times for the same payload type.
    #[error("multiple '{name}' attributes for payload type {pt}")]
    DuplicateForPayloadType { name: String, pt: u8 },
    /// The address of the connection line could not be parsed.
    #[error("invalid connection address '{address}'")]
    ConnectionAddress { address: String },
    /// The time-to-live of the connection line could not be parsed.
    #[error("invalid time-to-live in connection address '{address}'")]
    ConnectionTtl { address: String },
}

/// An attribute with too many occurrences, an unexpected value or a missing
/// value. Shared between session- and media-level attributes.
#[derive(Debug, Error)]
pub enum AttributeParseError {
    /// The attribute occurred more than once but is only allowed once.
    #[error("multiple '{0}' attributes")]
    Duplicate(String),
    /// A value-less attribute was given a value.
    #[error("attribute '{0}' must not have a value")]
    UnexpectedValue(String),
    /// An attribute with a value was given none.
    #[error("attribute '{0}' requires a value")]
    MissingValue(String),
}

/// Error returned when converting between `application/x-rtp` caps and an SDP
/// media fails.
#[derive(Debug, Error)]
pub enum CapsError {
    /// The caps are empty.
    #[error("caps are empty")]
    Empty,
    /// The caps do not describe RTP media.
    #[error("caps are not RTP caps: '{0}'")]
    NotRtpCaps(String),
    /// The caps structures do not all share the same media type.
    #[error("caps structures with mixed media types are not supported")]
    MixedMediaTypes,
    /// The media type of the caps is not a known type.
    #[error("invalid media type")]
    MediaType(#[source] sdp_types::ParseEnumError),
    /// The media type of the caps is not supported.
    #[error("unsupported media type '{0}'")]
    UnsupportedMediaType(String),
    /// A mandatory caps field is missing.
    #[error("caps structure is missing a '{name}' field")]
    MissingField { name: String },
    /// The payload type is not in the valid range.
    #[error("invalid payload type '{payload}', must be 0-127")]
    InvalidPayloadType { payload: i32 },
    /// The same payload type appears in multiple caps structures.
    #[error("duplicate payload type {pt} in caps")]
    DuplicatePayloadType { pt: u8 },
    /// The clock rate is not a positive number.
    #[error("invalid clock rate {clock_rate} for payload type {pt}")]
    InvalidClockRate { clock_rate: i32, pt: u8 },
    /// Neither the caps nor the media contain any payload types.
    #[error("{what} without any payload types")]
    NoPayloadTypes { what: &'static str },
    /// A dynamic payload type has no rtpmap and the clock rate could not be
    /// guessed from the payload tables.
    #[error("no rtpmap for payload type {pt} and the clock rate could not be guessed")]
    NoRtpMapGuess { pt: u8 },
    /// A dynamic payload type without an encoding name, so no rtpmap can be
    /// generated for it.
    #[error("no encoding name for dynamic payload type {pt}, no rtpmap can be generated")]
    MissingEncodingName { pt: u8 },
    /// The same extmap id with different values in multiple caps structures.
    #[error("duplicate extmap id {id} in caps")]
    DuplicateExtMapId { id: u8 },
    /// The payload tables do not know a clock rate for the payload type.
    #[error("no clock rate for payload type {pt}")]
    NoClockRate { pt: u8 },
    /// A caps field has a type or value that could not be used.
    #[error("invalid caps field '{name}'")]
    Field { name: String },
    /// A caps field has a value that could not be used.
    #[error("invalid caps field '{name}': {reason}")]
    FieldValue { name: String, reason: String },
    /// The value of an enum-like caps field could not be parsed.
    #[error("invalid '{name}' caps field")]
    EnumField {
        name: String,
        #[source]
        source: sdp_types::ParseEnumError,
    },
    /// The numeric value of a caps field could not be parsed.
    #[error("invalid '{name}' caps field")]
    NumericField {
        name: String,
        #[source]
        source: ParseIntError,
    },
    /// The fmtp parameters of a caps structure could not be parsed.
    #[error("invalid fmtp parameters for payload type {pt}")]
    Fmtp {
        pt: u8,
        #[source]
        source: sdp_types::AttributeError,
    },
}

/// Checks that a unique attribute did not occur yet and carries no value.
pub(crate) fn parse_unique_attribute_no_value<T>(
    key: &str,
    value_str: &Option<String>,
    existing: &Option<T>,
    new_value: T,
) -> Result<T, AttributeParseError> {
    if existing.is_some() {
        Err(AttributeParseError::Duplicate(key.to_string()))
    } else if value_str.is_some() {
        Err(AttributeParseError::UnexpectedValue(key.to_string()))
    } else {
        Ok(new_value)
    }
}

/// Checks that a unique attribute did not occur yet and has a value,
/// parsing it with `check_value`.
pub(crate) fn parse_unique_attribute_with_value<T, E, F: FnOnce(&str) -> Result<T, E>>(
    key: &str,
    value_str: &Option<String>,
    existing: &Option<T>,
    check_value: F,
) -> Result<T, E>
where
    E: From<AttributeParseError>,
{
    if existing.is_some() {
        Err(AttributeParseError::Duplicate(key.to_string()).into())
    } else if let Some(v) = value_str {
        check_value(v.as_str())
    } else {
        Err(AttributeParseError::MissingValue(key.to_string()).into())
    }
}

/// Error type for a typed attribute value: session- and media-level
/// attributes map parse failures to different error types.
pub(crate) trait TypedAttributeError: From<AttributeParseError> {
    fn typed_attribute(name: &str, source: sdp_types::AttributeError) -> Self;
}

impl TypedAttributeError for MediaError {
    fn typed_attribute(name: &str, source: sdp_types::AttributeError) -> Self {
        MediaError::TypedAttribute {
            name: name.to_string(),
            source,
        }
    }
}

impl TypedAttributeError for ParseError {
    fn typed_attribute(name: &str, source: sdp_types::AttributeError) -> Self {
        ParseError::AttributeValue {
            name: name.to_string(),
            source,
        }
    }
}

/// Parses an attribute value with `T::from_str()`, mapping the error via
/// `E::typed_attribute()`.
pub(crate) fn parse_typed<T, E>(key: &str, val: &str) -> Result<T, E>
where
    T: std::str::FromStr<Err = sdp_types::AttributeError>,
    E: TypedAttributeError,
{
    val.parse()
        .map_err(|source| E::typed_attribute(key, source))
}

/// Parses a unique, valueless flag attribute (e.g. `a=rtcp-mux`).
pub(crate) fn parse_unique_flag(
    key: &str,
    value: &Option<String>,
    existing: bool,
) -> Result<bool, AttributeParseError> {
    if existing {
        Err(AttributeParseError::Duplicate(key.to_string()))
    } else if value.is_some() {
        Err(AttributeParseError::UnexpectedValue(key.to_string()))
    } else {
        Ok(true)
    }
}

/// Parses a unique attribute with a typed value (e.g. `a=mid:`).
pub(crate) fn parse_unique_typed<T, E>(
    key: &str,
    value: &Option<String>,
    existing: &Option<T>,
) -> Result<T, E>
where
    T: std::str::FromStr<Err = sdp_types::AttributeError>,
    E: TypedAttributeError,
{
    parse_unique_attribute_with_value(key, value, existing, |val| parse_typed::<T, E>(key, val))
}

/// Returns the value of an attribute that must have one.
pub(crate) fn require_value<'a>(
    key: &str,
    value: &'a Option<String>,
) -> Result<&'a str, AttributeParseError> {
    value
        .as_deref()
        .ok_or_else(|| AttributeParseError::MissingValue(key.to_string()))
}
