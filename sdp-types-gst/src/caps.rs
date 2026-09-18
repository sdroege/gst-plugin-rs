// Copyright (C) 2024 Nirbheek Chauhan <nirbheek@centricular.com>
// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Conversion between `application/x-rtp` caps and SDP media lines.

use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gst::glib;
use gst::prelude::*;
use mykey::payload::{KeyDataSubPayload, KeyValidity, Payload, SrtpParamType};
use sdp_types::{
    Crypto, CryptoSuite, Direction, ExtMap, Fmtp, MediaType, Rid, RidDirection, RtpMap,
    TransportProto,
};

use super::{
    CAT,
    attrs::{KeyMgmtExt, RtcpFb, SKIPPED_OTHER_ATTRIBUTES},
    error::CapsError,
    media::Media,
    payload_table::guess_payload_info,
};

/// H264 profile <-> profile-level-id, kept in sync with the
/// `h264_get_profile_idc()` table in the C SDP library (`gstsdpmessage.c`).
fn h264_profile_idc(profile: &str) -> Option<(u8, u8)> {
    const CSF0: u8 = 0x80; // Baseline compatible
    const CSF1: u8 = 0x40; // Main compatible
    const CSF3: u8 = 0x10;
    const CSF4: u8 = 0x08;
    const CSF5: u8 = 0x04;

    Some(match profile {
        "constrained-baseline" => (66, CSF0 | CSF1),
        "baseline" => (66, CSF0),
        "main" => (77, CSF1),
        "extended" => (88, CSF3),
        "constrained-high" => (100, CSF4 | CSF5),
        "progressive-high" => (100, CSF4),
        "high-10" => (110, 0),
        "high-10-intra" => (110, CSF3),
        "progressive-high-10" => (110, CSF4),
        "high-4:2:2" => (122, 0),
        "high-4:2:2-intra" => (122, CSF3),
        "high-4:4:4" => (244, 0),
        "high-4:4:4-intra" => (244, CSF3),
        "cavlc-4:4:4-intra" => (44, 0),
        "multiview-high" => (118, 0),
        "stereo-high" => (128, 0),
        "scalable-baseline" => (83, 0),
        "scalable-constrained-baseline" => (83, CSF5),
        "scalable-high" => (86, 0),
        "scalable-constrained-high" => (86, CSF5),
        "scalable-high-intra" => (86, CSF3),
        _ => return None,
    })
}

// fmtp parameters skipped during SDP->caps since they would override the
// caps fields set from the rtpmap.
const RESERVED_FMTP_KEYS: [&str; 5] = [
    "media",
    "payload",
    "clock-rate",
    "encoding-name",
    "encoding-params",
];

const IGNORED_RTP_CAPS_FIELDS: [&str; 9] = [
    "media",
    "timestamp-offset",
    "seqnum-offset",
    "encoding-name",
    "encoding-params",
    "ssrc",
    "clock-rate",
    "mki",
    "roc",
];

/// Rejects u32 values that do not fit into an i32 caps field: SDP
/// attributes are arbitrary-length numbers, caps fields are not.
fn u32_to_i32(value: u32, name: &str) -> Result<i32, CapsError> {
    if value > i32::MAX as u32 {
        return Err(CapsError::FieldValue {
            name: name.to_string(),
            reason: format!("{value} does not fit into an i32"),
        });
    }

    Ok(value as i32)
}

/// Inserts an extmap collected from the caps structures, rejecting the same
/// id with different values: emitting both would produce invalid SDP with
/// duplicate a=extmap lines.
fn insert_extmap(extmap: &mut BTreeSet<ExtMap>, new: ExtMap) -> Result<(), CapsError> {
    if extmap.contains(&new) {
        return Ok(());
    }
    if extmap.iter().any(|extmap| extmap.id == new.id) {
        return Err(CapsError::DuplicateExtMapId { id: new.id });
    }
    extmap.insert(new);

    Ok(())
}

fn caps_media_type(s: &gst::StructureRef) -> Result<MediaType, CapsError> {
    let media = s
        .get::<&str>("media")
        .map_err(|_| CapsError::MissingField {
            name: "media".into(),
        })?;
    let media_type = media.parse::<MediaType>().map_err(CapsError::MediaType)?;
    if !matches!(
        media_type,
        MediaType::Audio | MediaType::Video | MediaType::Application
    ) {
        return Err(CapsError::UnsupportedMediaType(media.to_string()));
    }
    Ok(media_type)
}

/// Converts `application/x-rtp` caps (one structure per payload type) into a
/// single SDP media line.
pub fn caps_to_media(caps: &gst::CapsRef) -> Result<Media, CapsError> {
    if caps.is_empty() {
        return Err(CapsError::Empty);
    }
    let first = caps.structure(0).expect("checked non-empty");
    if !first.has_name("application/x-rtp") {
        return Err(CapsError::NotRtpCaps(first.name().to_string()));
    }
    let media_type = caps_media_type(first)?;
    let mut direction = None;
    let mut ptime = None;
    let mut maxptime = None;
    let mut formats = vec![];
    let mut rtpmaps = BTreeMap::<u8, RtpMap>::new();
    let mut fmtps = BTreeMap::<u8, Fmtp>::new();
    let mut framesizes = BTreeMap::<u8, String>::new();
    let mut rtcp_fbs = BTreeMap::<u8, RtcpFb>::new();
    let mut extmap = BTreeSet::<ExtMap>::new();
    let mut ssrcs = BTreeMap::<(u32, String), String>::new();
    let mut rids: Vec<Rid> = vec![];
    let mut other_attributes: Vec<(String, Option<String>)> = vec![];

    for s in caps.iter() {
        if !s.has_name("application/x-rtp") {
            return Err(CapsError::NotRtpCaps(s.name().to_string()));
        }
        let structure_media_type = caps_media_type(s)?;
        if structure_media_type != media_type {
            return Err(CapsError::MixedMediaTypes);
        }

        let payload = s
            .get::<i32>("payload")
            .map_err(|_| CapsError::MissingField {
                name: "payload".into(),
            })?;
        if !(0..=127).contains(&payload) {
            return Err(CapsError::InvalidPayloadType { payload });
        }
        let pt = payload as u8;
        if formats.contains(&pt) {
            return Err(CapsError::DuplicatePayloadType { pt });
        }
        let clock_rate = s
            .get::<i32>("clock-rate")
            .map_err(|_| CapsError::MissingField {
                name: "clock-rate".into(),
            })?;
        if clock_rate <= 0 {
            return Err(CapsError::InvalidClockRate { clock_rate, pt });
        }
        let encoding_name = s.get::<&str>("encoding-name").ok();
        let encoding_params = s.get::<&str>("encoding-params").ok();
        let is_h264 = encoding_name.is_some_and(|n| n.eq_ignore_ascii_case("H264"));

        let mut fmtp_parts: Vec<String> = vec![];
        let mut h264_profile = None;

        for field in s.fields() {
            let name = field.as_str();
            if IGNORED_RTP_CAPS_FIELDS.contains(&name)
                || name.starts_with("srtp-")
                || name.starts_with("srtcp-")
                || name.starts_with("x-gst-rtsp-server-rtx-time")
            {
                continue;
            }

            if name == "direction" {
                let dir = s.get::<&str>("direction").map_err(|_| CapsError::Field {
                    name: "direction".into(),
                })?;
                direction = Some(
                    dir.parse::<Direction>()
                        .map_err(|err| CapsError::EnumField {
                            name: "direction".into(),
                            source: err,
                        })?,
                );
                continue;
            }
            if name == "ptime" || name == "maxptime" {
                let value = s.get::<i32>(name).map_err(|_| CapsError::Field {
                    name: name.to_string(),
                })?;
                if value <= 0 {
                    return Err(CapsError::FieldValue {
                        name: name.to_string(),
                        reason: "must be a positive integer".into(),
                    });
                }
                if name == "ptime" {
                    ptime = Some(value as u32);
                } else {
                    maxptime = Some(value as u32);
                }
                continue;
            }
            if let Some(fb_name) = name.strip_prefix("rtcp-fb-") {
                if s.has_field_with_type(name, glib::Type::BOOL)
                    && s.get::<bool>(name).unwrap_or(false)
                {
                    let fb = match fb_name {
                        "nack" => RtcpFb {
                            nack: true,
                            ..Default::default()
                        },
                        "nack-pli" => RtcpFb {
                            nack_pli: true,
                            ..Default::default()
                        },
                        "ccm-fir" => RtcpFb {
                            ccm_fir: true,
                            ..Default::default()
                        },
                        "transport-cc" => RtcpFb {
                            transport_cc: true,
                            ..Default::default()
                        },
                        other => RtcpFb {
                            other: vec![other.replace('-', " ")],
                            ..Default::default()
                        },
                    };
                    rtcp_fbs
                        .entry(pt)
                        .and_modify(|entry| *entry |= fb.clone())
                        .or_insert(fb);
                }
                continue;
            }
            if let Some(id) = name.strip_prefix("extmap-") {
                let id = id.parse::<u8>().map_err(|err| CapsError::NumericField {
                    name: name.to_string(),
                    source: err,
                })?;
                // 0 is invalid
                if id == 0 {
                    continue;
                }
                let value = s.value(name).expect("caps field must have a value");
                if value.is_type(glib::Type::STRING) {
                    let uri = value.get::<&str>().map_err(|_| CapsError::Field {
                        name: name.to_string(),
                    })?;
                    insert_extmap(
                        &mut extmap,
                        ExtMap::with_direction(id, Direction::SendRecv, uri),
                    )?;
                } else if value.is_type(gst::Array::static_type()) {
                    let array = value.get::<gst::Array>().map_err(|_| CapsError::Field {
                        name: name.to_string(),
                    })?;
                    if array.len() != 3 {
                        return Err(CapsError::FieldValue {
                            name: name.to_string(),
                            reason: "expected an array of three strings".into(),
                        });
                    }
                    let dir = array[0].get::<&str>().map_err(|_| CapsError::FieldValue {
                        name: name.to_string(),
                        reason: "invalid extmap direction".into(),
                    })?;
                    let direction = if dir.is_empty() {
                        Direction::SendRecv
                    } else {
                        dir.parse::<Direction>()
                            .map_err(|err| CapsError::EnumField {
                                name: name.to_string(),
                                source: err,
                            })?
                    };
                    let uri = array[1].get::<&str>().map_err(|_| CapsError::FieldValue {
                        name: name.to_string(),
                        reason: "invalid extmap URI".into(),
                    })?;
                    let params = array[2].get::<&str>().map_err(|_| CapsError::FieldValue {
                        name: name.to_string(),
                        reason: "invalid extmap attributes".into(),
                    })?;
                    insert_extmap(
                        &mut extmap,
                        ExtMap::builder(id, uri)
                            .direction(direction)
                            .attributes_if_some((!params.is_empty()).then_some(params))
                            .build(),
                    )?;
                } else {
                    return Err(CapsError::FieldValue {
                        name: name.to_string(),
                        reason: "must be a string or an array of three strings".into(),
                    });
                }
                continue;
            }
            if let Some(rest) = name.strip_prefix("ssrc-") {
                if !s.has_field_with_type(name, glib::Type::STRING) {
                    continue;
                }
                let Some((id, attr)) = rest.split_once('-') else {
                    return Err(CapsError::FieldValue {
                        name: name.to_string(),
                        reason: "expected 'ssrc-<ssrc-id>-<attribute>'".into(),
                    });
                };
                let id = id.parse::<u32>().map_err(|err| CapsError::NumericField {
                    name: name.to_string(),
                    source: err,
                })?;
                let value = s.get::<String>(name).map_err(|_| CapsError::Field {
                    name: name.to_string(),
                })?;
                ssrcs.insert((id, attr.to_string()), value);
                continue;
            }
            if let Some(id) = name.strip_prefix("rid-") {
                let value = s.value(name).expect("caps field must have a value").clone();
                let rid = if value.is_type(glib::Type::STRING) {
                    let direction = value
                        .get::<String>()
                        .map_err(|_| CapsError::Field {
                            name: name.to_string(),
                        })?
                        .parse::<RidDirection>()
                        .map_err(|_| CapsError::FieldValue {
                            name: name.to_string(),
                            reason: "invalid rid direction".into(),
                        })?;
                    Rid {
                        id: id.to_string(),
                        direction,
                        params: vec![],
                    }
                } else if value.is_type(gst::Array::static_type()) {
                    let array = value.get::<gst::Array>().map_err(|_| CapsError::Field {
                        name: name.to_string(),
                    })?;
                    if array.is_empty() {
                        return Err(CapsError::FieldValue {
                            name: name.to_string(),
                            reason: "expected a non-empty array".into(),
                        });
                    }
                    let direction = array[0]
                        .get::<String>()
                        .map_err(|_| CapsError::FieldValue {
                            name: name.to_string(),
                            reason: "invalid rid direction".into(),
                        })?
                        .parse::<RidDirection>()
                        .map_err(|_| CapsError::FieldValue {
                            name: name.to_string(),
                            reason: "invalid rid direction".into(),
                        })?;
                    let params = array[1..]
                        .iter()
                        .map(|value| {
                            value.get::<String>().map_err(|_| CapsError::FieldValue {
                                name: name.to_string(),
                                reason: "invalid rid parameter".into(),
                            })
                        })
                        .collect::<Result<Vec<_>, CapsError>>()?;
                    Rid {
                        id: id.to_string(),
                        direction,
                        params,
                    }
                } else {
                    return Err(CapsError::FieldValue {
                        name: name.to_string(),
                        reason: "must be a string or an array of strings".into(),
                    });
                };
                match rids.iter().find(|existing| existing.id == rid.id) {
                    // Shared between the caps structures, emit each id
                    // only once.
                    Some(existing) if existing == &rid => (),
                    Some(_) => {
                        gst::warning!(
                            CAT,
                            "Conflicting definitions of rid '{}' in caps, keeping the first one",
                            rid.id
                        );
                    }
                    None => rids.push(rid),
                }
                continue;
            }
            if name == "a-framesize" {
                if !s.has_field_with_type(name, glib::Type::STRING) {
                    continue;
                }
                framesizes.insert(
                    pt,
                    s.get::<String>(name).map_err(|_| CapsError::Field {
                        name: name.to_string(),
                    })?,
                );
                continue;
            }
            // Generic SDP attribute passthrough: caps field 'a-<name>' maps to
            // SDP attribute '<name>', 'x-<name>' to SDP attribute 'x-<name>'.
            if name.starts_with("a-") || name.starts_with("x-") {
                if !s.has_field_with_type(name, glib::Type::STRING) {
                    continue;
                }
                let value = s.get::<String>(name).map_err(|_| CapsError::Field {
                    name: name.to_string(),
                })?;
                let attribute = name.strip_prefix("a-").unwrap_or(name);
                match other_attributes
                    .iter()
                    .find(|(existing, _)| existing.as_str() == attribute)
                {
                    // Shared between the caps structures, emit each
                    // attribute only once.
                    Some((_, Some(existing))) if existing == &value => (),
                    Some(_) => {
                        gst::warning!(
                            CAT,
                            "Conflicting values for attribute '{attribute}' in caps, keeping the first one"
                        );
                    }
                    None => other_attributes.push((attribute.to_string(), Some(value))),
                }
                continue;
            }
            // "profile" is an internal H264 alias for "profile-level-id"
            // and never becomes an fmtp parameter.
            if is_h264 && name == "profile" {
                h264_profile = Some(s.get::<&str>("profile").map_err(|_| CapsError::Field {
                    name: "profile".into(),
                })?);
                continue;
            }
            if s.has_field_with_type(name, glib::Type::STRING) {
                let value = s.get::<&str>(name).map_err(|_| CapsError::Field {
                    name: name.to_string(),
                })?;
                fmtp_parts.push(format!("{name}={value}"));
            }
        }

        if let Some(profile) = h264_profile {
            // Convert the profile back into fmtp parameters, adding defaults
            // if not present in the caps to match the C SDP implementation.
            if !s.has_field("level-asymmetry-allowed") {
                fmtp_parts.push("level-asymmetry-allowed=1".to_string());
            }
            if !s.has_field("profile-level-id") {
                match h264_profile_idc(profile) {
                    Some((profile_idc, constraint_flags)) => fmtp_parts.push(format!(
                        "profile-level-id={profile_idc:02x}{constraint_flags:02x}1f"
                    )),
                    None => gst::fixme!(
                        CAT,
                        "Can't convert profile {profile} back into profile-level-id"
                    ),
                }
            }
        }

        if !fmtp_parts.is_empty() {
            let fmtp = format!("{pt} {}", fmtp_parts.join(";"))
                .parse::<Fmtp>()
                .map_err(|err| CapsError::Fmtp { pt, source: err })?;
            fmtps.insert(pt, fmtp);
        }
        if let Some(encoding_name) = encoding_name {
            rtpmaps.insert(
                pt,
                RtpMap::builder(pt, encoding_name.to_ascii_uppercase(), clock_rate as u32)
                    .encoding_params_if_some(
                        encoding_params.map(|params| params.to_ascii_lowercase()),
                    )
                    .build(),
            );
        } else if pt >= 96 {
            // RFC 8866 §6.6: dynamic payload types require an rtpmap, which
            // cannot be generated without an encoding name
            return Err(CapsError::MissingEncodingName { pt });
        }
        formats.push(pt);
    }

    let proto = if rtcp_fbs.values().any(|fb| !fb.sdp_params().is_empty()) {
        TransportProto::RtpAvpf
    } else {
        TransportProto::RtpAvp
    };
    let media = Media::builder(media_type)
        .formats(formats)
        .rtpmaps(rtpmaps)
        .fmtps(fmtps)
        .framesizes(framesizes)
        .rtcp_fbs(rtcp_fbs)
        .extmap(extmap)
        .ssrcs(ssrcs)
        .rids(rids)
        .ptime(ptime)
        .maxptime(maxptime)
        .other_attributes(other_attributes)
        .direction(direction.unwrap_or(Direction::SendRecv))
        .proto(proto)
        .build();

    gst::trace!(CAT, "produced media {media:?}");
    Ok(media)
}

/// SRTP keying material derived from the key management attributes, as the
/// caps fields used by the SRTP elements.
#[derive(Clone)]
struct SrtpKeying {
    srtp_cipher: &'static str,
    srtp_auth: &'static str,
    srtcp_cipher: &'static str,
    srtcp_auth: &'static str,
    key: gst::Buffer,
    roc: Option<u32>,
    mki: Option<gst::Buffer>,
}

/// Based on `mikey_to_caps` in rtspsrc2 (`net/rtsp/src/rtspsrc/sdp.rs`),
/// checked against the C `gst_mikey_message_to_caps()`. Unlike rtspsrc2, the
/// fields are only produced when a key was found. Cipher/auth without a key
/// are useless for consumers.
fn keying_from_mikey(mikey: &mykey::MikeyMessage) -> Option<SrtpKeying> {
    let mut srtp_cipher = "aes-128-icm";
    let mut srtp_auth = "hmac-sha1-80";
    let mut srtcp_cipher = srtp_cipher;
    let mut srtcp_auth = srtp_auth;

    let mut srtp_enc_on = None;
    let mut srtcp_enc_on = None;
    let mut srtp_auth_on = None;

    // The first crypto session (SRTP ID map entry).
    let srtp = mikey.header.cs_id_map.first()?;

    if let Some(sp) = mikey.payloads.iter().find_map(|p| match p {
        Payload::Sp(sp) if sp.policy_no == srtp.policy_no => Some(sp),
        _ => None,
    }) {
        // Only SRTP (proto type 0) is supported.
        if sp.proto_type != 0 {
            return None;
        }

        for param in &sp.params {
            match param.param_type {
                t if t == SrtpParamType::EncryptionAlg as u8 => match param.param_value.first() {
                    Some(0) => srtp_cipher = "null",
                    Some(1) | Some(2) => srtp_cipher = "aes-128-icm",
                    Some(6) => srtp_cipher = "aes-128-gcm",
                    Some(enc_alg) => {
                        gst::warning!(
                            CAT,
                            "Unexpected encryption algorithm {enc_alg}, using {srtp_cipher}"
                        );
                    }
                    None => (),
                },
                t if t == SrtpParamType::SessionEncKeyLen as u8 => {
                    match param.param_value.first() {
                        // Already covered by the encryption algorithm.
                        Some(16) => (),
                        Some(32) => {
                            if srtp_cipher == "aes-128-icm" {
                                srtp_cipher = "aes-256-icm";
                            } else if srtp_cipher == "aes-128-gcm" {
                                srtp_cipher = "aes-256-gcm";
                            }
                        }
                        Some(key_len) => {
                            gst::warning!(
                                CAT,
                                "Unexpected session encryption key length {key_len}"
                            );
                        }
                        None => (),
                    }
                }
                t if t == SrtpParamType::AuthAlg as u8 => match param.param_value.first() {
                    Some(0) => srtp_auth = "null",
                    Some(1) => srtp_auth = "hmac-sha1-80",
                    Some(auth_alg) => {
                        gst::warning!(CAT, "Unexpected authentication algorithm {auth_alg}");
                    }
                    None => (),
                },
                t if t == SrtpParamType::SessionAuthKeyLen as u8 => {
                    match param.param_value.first() {
                        Some(4) => srtp_auth = "hmac-sha1-32",
                        Some(10) => srtp_auth = "hmac-sha1-80",
                        Some(key_len) => {
                            gst::warning!(CAT, "Unexpected authentication key length {key_len}");
                        }
                        None => (),
                    }
                }
                t if t == SrtpParamType::SrtpEncryption as u8 => {
                    srtp_enc_on = param.param_value.first().copied();
                }
                t if t == SrtpParamType::SrtcpEncryption as u8 => {
                    srtcp_enc_on = param.param_value.first().copied();
                }
                t if t == SrtpParamType::SrtpAuthentication as u8 => {
                    srtp_auth_on = param.param_value.first().copied();
                }
                param_type => {
                    // Salt key length, PRF, key derivation rate and other
                    // policies without a caps mapping are ignored to match
                    // the C implementation.
                    if param_type <= SrtpParamType::SrtpPrefixLen as u8 {
                        gst::trace!(CAT, "Ignoring security policy parameter type {param_type}");
                    } else {
                        gst::warning!(
                            CAT,
                            "Unexpected security policy parameter type {param_type}"
                        );
                    }
                }
            }
        }

        srtcp_cipher = srtp_cipher;
        srtcp_auth = srtp_auth;

        if srtp_enc_on == Some(0) {
            srtp_cipher = "null";
        }
        if srtcp_enc_on == Some(0) {
            srtcp_cipher = "null";
        }
        if srtp_auth_on == Some(0) {
            srtp_auth = "null";
        }
    }

    // The first key data sub-payload of the first unencrypted KEMAC
    // payload, see RFC 3830 section 6.13. The key is key ++ salt, the
    // KV=SPI is the MKI in the SRTP case.
    let (key, mki) = mikey.payloads.iter().find_map(|p| {
        let Payload::Kemac(kemac) = p else {
            return None;
        };
        if kemac.enc_alg != mykey::payload::EncAlg::Null
            || kemac.mac_alg != mykey::payload::MacAlg::Null
        {
            return None;
        }
        let (sub, _) = KeyDataSubPayload::parse(kemac.enc_data.as_slice()).ok()?;
        let mut key_data = sub.key_data;
        key_data.extend_from_slice(sub.salt.as_deref().unwrap_or_default());
        let mki = match sub.validity {
            KeyValidity::Spi(ref mki) if !mki.is_empty() => {
                Some(gst::Buffer::from_slice(mki.clone()))
            }
            _ => None,
        };
        Some((gst::Buffer::from_slice(key_data), mki))
    })?;

    Some(SrtpKeying {
        srtp_cipher,
        srtp_auth,
        srtcp_cipher,
        srtcp_auth,
        key,
        roc: Some(srtp.roc),
        mki,
    })
}

/// Maps an `a=crypto` inline key (RFC 4568) to the same caps fields. Only
/// the SRTP suites of RFC 4568 are supported, anything else is skipped.
fn keying_from_crypto(crypto: &Crypto) -> Option<SrtpKeying> {
    let (srtp_cipher, srtp_auth) = match &crypto.crypto_suite {
        CryptoSuite::AesCm128HmacSha1_80 => ("aes-128-icm", "hmac-sha1-80"),
        CryptoSuite::AesCm128HmacSha1_32 => ("aes-128-icm", "hmac-sha1-32"),
        suite => {
            gst::debug!(CAT, "Skipping unsupported crypto suite {}", suite.as_str());
            return None;
        }
    };

    let key_param = crypto.key_params.first()?;
    let key = match STANDARD.decode(key_param.key_and_salt.trim()) {
        Ok(key) => key,
        Err(err) => {
            gst::warning!(CAT, "Failed to base64 decode crypto key: {err}");
            return None;
        }
    };
    if key.is_empty() {
        return None;
    }

    // The MKI is sent in network byte order in SRTP packets.
    let mki = key_param.mki_and_length.and_then(|(mki, length)| {
        let length = usize::try_from(length).ok()?;
        if !(1..=4).contains(&length) {
            gst::warning!(CAT, "Invalid MKI length {length}, ignoring");
            return None;
        }
        Some(gst::Buffer::from_slice(
            mki.to_be_bytes()[4 - length..].to_vec(),
        ))
    });

    Some(SrtpKeying {
        srtp_cipher,
        srtp_auth,
        srtcp_cipher: srtp_cipher,
        srtcp_auth: srtp_auth,
        key: gst::Buffer::from_slice(key),
        roc: None,
        mki,
    })
}

/// The first key management attribute of the media line that provides a
/// usable key, preferring `a=key-mgmt` MIKEY over `a=crypto`.
fn media_keying(media: &Media) -> Option<SrtpKeying> {
    let keying = media
        .key_mgmt()
        .iter()
        .find_map(|km| km.mikey().ok().and_then(|mikey| keying_from_mikey(&mikey)))
        .or_else(|| media.crypto().iter().find_map(keying_from_crypto));
    if keying.is_some() && media.key_mgmt().len() + media.crypto().len() > 1 {
        gst::trace!(
            CAT,
            "Multiple key management attributes, using the first one providing a key"
        );
    }
    keying
}

/// Converts a single SDP media line into `application/x-rtp` caps, one
/// structure per payload type of the media.
pub fn media_to_caps(media: &Media) -> Result<gst::Caps, CapsError> {
    let media_type = media.media().to_string();
    let keying = media_keying(media);
    let mut caps = gst::Caps::new_empty();
    {
        let caps = caps.get_mut().expect("newly created caps");
        for &pt in media.formats() {
            let mut s = gst::Structure::builder("application/x-rtp").build();
            s.set("media", &media_type);
            s.set("payload", pt as i32);
            s.set("direction", media.direction().as_str());
            if let Some(ptime) = media.ptime() {
                s.set("ptime", u32_to_i32(ptime, "ptime")?);
            }
            if let Some(maxptime) = media.maxptime() {
                s.set("maxptime", u32_to_i32(maxptime, "maxptime")?);
            }

            // rtpmap, with clock rate / encoding name guessing via the
            // payload tables where necessary.
            let (encoding_name, clock_rate, encoding_params) = match media.rtpmaps().get(&pt) {
                Some(rtpmap) if rtpmap.clock_rate > 0 => (
                    rtpmap.encoding_name.to_ascii_uppercase(),
                    rtpmap.clock_rate,
                    rtpmap
                        .encoding_params
                        .as_ref()
                        .map(|params| params.to_ascii_lowercase()),
                ),
                rtpmap => {
                    let info = guess_payload_info(
                        &media_type,
                        pt,
                        rtpmap.map(|rtpmap| rtpmap.encoding_name.as_str()),
                    )
                    .ok_or(CapsError::NoRtpMapGuess { pt })?;
                    if info.clock_rate == 0 {
                        return Err(CapsError::NoClockRate { pt });
                    }
                    let encoding_params = rtpmap
                        .and_then(|rtpmap| rtpmap.encoding_params.clone())
                        .map(|params| params.to_ascii_lowercase())
                        .or_else(|| {
                            info.encoding_params
                                .map(|params| params.to_ascii_lowercase())
                        });
                    (
                        info.encoding_name.to_ascii_uppercase(),
                        info.clock_rate,
                        encoding_params,
                    )
                }
            };
            s.set("clock-rate", u32_to_i32(clock_rate, "clock-rate")?);
            s.set("encoding-name", &encoding_name);
            if let Some(params) = &encoding_params {
                s.set("encoding-params", params);
            }

            if let Some(fmtp) = media.fmtps().get(&pt) {
                for param in &fmtp.format_specific_params {
                    let key = param.param.trim().to_ascii_lowercase();
                    if key.is_empty()
                        || RESERVED_FMTP_KEYS.contains(&key.as_str())
                        || param.val.is_none()
                        || (encoding_name == "H264" && key == "profile-level-id")
                    {
                        continue;
                    }
                    // Some servers pad the values, strip them to match the C
                    // SDP implementation.
                    s.set(key, param.val.as_deref().map(str::trim).unwrap());
                }
            }

            // H264 profile-level-id <-> profile/level caps fields.
            if encoding_name == "H264" {
                if let Some(value) = media.fmtps().get(&pt).and_then(|fmtp| {
                    fmtp.format_specific_params
                        .iter()
                        .find(|param| param.param.eq_ignore_ascii_case("profile-level-id"))
                        .and_then(|param| param.val.as_ref())
                }) {
                    let bytes = (value.len() == 6 && value.is_ascii()).then(|| {
                        (0..3)
                            .map(|i| u8::from_str_radix(&value[i * 2..i * 2 + 2], 16))
                            .collect::<std::result::Result<Vec<u8>, _>>()
                    });
                    if let Some(Ok(bytes)) = bytes {
                        let sps: &[u8; 3] = bytes.as_slice().try_into().unwrap();
                        let profile = gst_pbutils::codec_utils_h264_get_profile(sps);
                        let level = gst_pbutils::codec_utils_h264_get_level(sps);
                        if let (Ok(profile), Ok(level)) = (profile, level) {
                            s.set("profile", profile);
                            s.set("level", level);
                        } else {
                            gst::warning!(
                                CAT,
                                "Failed to parse profile-level-id {value}, ignoring..."
                            );
                        }
                    } else {
                        gst::warning!(CAT, "Failed to parse profile-level-id {value}, ignoring...");
                    }
                }
                // Adjust caps for level asymmetry.
                if s.get_optional::<&str>("level-asymmetry-allowed") != Ok(Some("0"))
                    && s.has_field("level")
                {
                    s.remove_field("level");
                }
                s.remove_field("level-asymmetry-allowed");
            }

            if let Some(dims) = media.framesizes().get(&pt) {
                s.set("a-framesize", dims);
            }

            let fb = match (media.rtcp_fb(), media.rtcp_fbs().get(&pt)) {
                (Some(wildcard), Some(fb)) => Some(wildcard.clone() | fb.clone()),
                (Some(wildcard), None) => Some(wildcard.clone()),
                (None, Some(fb)) => Some(fb.clone()),
                (None, None) => None,
            };
            if let Some(fb) = fb {
                if fb.nack {
                    s.set("rtcp-fb-nack", true);
                }
                if fb.nack_pli {
                    s.set("rtcp-fb-nack-pli", true);
                }
                if fb.ccm_fir {
                    s.set("rtcp-fb-ccm-fir", true);
                }
                if fb.transport_cc {
                    s.set("rtcp-fb-transport-cc", true);
                }
                for other in &fb.other {
                    s.set(format!("rtcp-fb-{}", other.replace(' ', "-")), true);
                }
            }

            for ext in media.extmap() {
                let direction = ext.direction.unwrap_or(Direction::SendRecv);
                let name = format!("extmap-{}", ext.id);
                if direction == Direction::SendRecv && ext.attributes.is_none() {
                    s.set(name, &ext.uri);
                } else {
                    s.set(
                        name,
                        gst::Array::new([
                            direction.as_str(),
                            ext.uri.as_str(),
                            ext.attributes.as_deref().unwrap_or(""),
                        ])
                        .to_send_value(),
                    );
                }
            }

            for ((id, attr), value) in media.ssrcs() {
                s.set(format!("ssrc-{id}-{attr}"), value);
            }
            for rid in media.rids() {
                let name = format!("rid-{}", rid.id);
                if rid.params.is_empty() {
                    s.set(name, rid.direction.to_string());
                } else {
                    let mut array = gst::Array::new([rid.direction.as_str()]);
                    for param in &rid.params {
                        array.append(param.clone());
                    }
                    s.set(name, array.to_send_value());
                }
            }

            // Generic SDP attribute passthrough, the reverse of
            // caps_to_media().
            for (name, value) in media.other_attributes() {
                if SKIPPED_OTHER_ATTRIBUTES.contains(&name.as_str()) {
                    continue;
                }
                let name = if name.starts_with("x-") {
                    name.clone()
                } else {
                    format!("a-{name}")
                };
                s.set(name, value.as_deref().unwrap_or_default());
            }

            // Keep the raw key management attributes as caps fields too.
            if let Some(key_mgmt) = media.key_mgmt().first() {
                s.set("a-key-mgmt", key_mgmt.to_string());
            }
            if let Some(crypto) = media.crypto().first() {
                s.set("a-crypto", crypto.to_string());
            }

            if let Some(keying) = &keying {
                s.set("srtp-cipher", keying.srtp_cipher);
                s.set("srtp-auth", keying.srtp_auth);
                s.set("srtcp-cipher", keying.srtcp_cipher);
                s.set("srtcp-auth", keying.srtcp_auth);
                s.set("srtp-key", &keying.key);
                if let Some(roc) = keying.roc {
                    s.set("roc", roc);
                }
                if let Some(mki) = &keying.mki {
                    s.set("mki", mki);
                }
            }

            caps.append_structure(s);
        }
    }

    if caps.is_empty() {
        return Err(CapsError::NoPayloadTypes { what: "media" });
    }
    gst::trace!(CAT, "produced caps {caps}");
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::RtcpFb;
    use crate::session::Session;
    use crate::tests::{MEDIA_AUDIO, MEDIA_VIDEO, SESSION_HEADER, init};
    use base64::engine::general_purpose::STANDARD;
    use sdp_types::{KeyMgmt, KeyMgmtMethod};

    fn audio_media() -> Media {
        Session::parse(&format!("{SESSION_HEADER}{MEDIA_AUDIO}"))
            .unwrap()
            .media()[0]
            .clone()
    }

    #[test]
    fn media_to_caps_audio() {
        init();
        let caps = media_to_caps(&audio_media()).unwrap();
        assert_eq!(caps.len(), 1);
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("media").unwrap(), "audio");
        assert_eq!(s.get::<i32>("payload").unwrap(), 96);
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "OPUS");
        assert_eq!(s.get::<i32>("clock-rate").unwrap(), 48000);
        assert_eq!(s.get::<&str>("encoding-params").unwrap(), "2");
        assert_eq!(s.get::<&str>("direction").unwrap(), "sendonly");
        assert_eq!(s.get::<i32>("ptime").unwrap(), 20);
        assert_eq!(s.get::<&str>("sprop-stereo").unwrap(), "1");
        assert_eq!(s.get::<&str>("usedtx").unwrap(), "1");
        assert_eq!(
            s.get::<&str>("extmap-1").unwrap(),
            "urn:ietf:params:rtp-hdrext:sdes:mid"
        );
        let extmap2 = s.get::<gst::Array>("extmap-2").unwrap();
        assert_eq!(extmap2.len(), 3);
        assert_eq!(extmap2[0].get::<&str>().unwrap(), "sendonly");
        assert_eq!(
            extmap2[1].get::<&str>().unwrap(),
            "urn:ietf:params:rtp-hdrext:ssrc-audio-level"
        );
        assert_eq!(extmap2[2].get::<&str>().unwrap(), "vad=on");
        assert!(s.get::<bool>("rtcp-fb-nack").unwrap());
        assert!(s.get::<bool>("rtcp-fb-transport-cc").unwrap());
        assert_eq!(s.get::<&str>("ssrc-1234-cname").unwrap(), "foo");
        assert_eq!(s.get::<&str>("rid-1").unwrap(), "send");
        let rid2 = s.get::<gst::Array>("rid-2").unwrap();
        assert_eq!(rid2.len(), 2);
        assert_eq!(rid2[0].get::<&str>().unwrap(), "recv");
        assert_eq!(rid2[1].get::<&str>().unwrap(), "apt=96");
        assert_eq!(s.get::<&str>("a-tool").unwrap(), "test");
        assert_eq!(s.get::<&str>("x-custom").unwrap(), "val");
    }

    #[test]
    fn static_payload_types() {
        init();
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}\
             m=audio 0 RTP/AVP 0 8\r\n\
             c=IN IP4 239.255.42.42\r\n"
        ))
        .unwrap();
        let caps = media_to_caps(&sdp.media()[0]).unwrap();
        assert_eq!(caps.len(), 2);
        for (idx, (pt, encoding_name)) in [(0, "PCMU"), (8, "PCMA")].iter().enumerate() {
            let s = caps.structure(idx).unwrap();
            assert_eq!(s.get::<i32>("payload").unwrap(), *pt);
            assert_eq!(s.get::<&str>("encoding-name").unwrap(), *encoding_name);
            assert_eq!(s.get::<i32>("clock-rate").unwrap(), 8000);
            assert_eq!(s.get::<&str>("encoding-params").unwrap(), "1");
        }
    }

    #[test]
    fn h264_profile() {
        init();
        let sdp = Session::parse(&format!("{SESSION_HEADER}{MEDIA_VIDEO}")).unwrap();
        let caps = media_to_caps(&sdp.media()[0]).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "H264");
        assert_eq!(s.get::<i32>("clock-rate").unwrap(), 90000);
        assert_eq!(s.get::<&str>("profile").unwrap(), "constrained-baseline");
        assert!(!s.has_field("level"));
        assert!(!s.has_field("profile-level-id"));
        assert!(!s.has_field("level-asymmetry-allowed"));
        assert_eq!(s.get::<&str>("packetization-mode").unwrap(), "1");
        assert_eq!(s.get::<&str>("a-framesize").unwrap(), "1280x720");
        assert!(s.get::<bool>("rtcp-fb-nack-pli").unwrap());
        assert!(s.get::<bool>("rtcp-fb-ccm-fir").unwrap());
        assert_eq!(s.get::<&str>("direction").unwrap(), "sendrecv");

        // And back into SDP: the profile becomes a profile-level-id with the
        // level-asymmetry-allowed default added.
        let media = caps_to_media(&caps).unwrap();
        let fmtp = media.fmtps().get(&96).unwrap();
        for (key, value) in [
            ("profile-level-id", "42c01f"),
            ("level-asymmetry-allowed", "1"),
            ("packetization-mode", "1"),
        ] {
            assert_eq!(
                fmtp.format_specific_params
                    .iter()
                    .find(|param| param.param == key)
                    .and_then(|param| param.val.as_deref()),
                Some(value),
                "missing or wrong fmtp parameter {key}"
            );
        }

        // Round-trip through caps again, the profile has to survive.
        let caps2 = media_to_caps(&media).unwrap();
        assert_eq!(
            caps2.structure(0).unwrap().get::<&str>("profile").unwrap(),
            "constrained-baseline"
        );
    }

    #[test]
    fn caps_to_media_and_back() {
        init();
        let s = gst::Structure::builder("application/x-rtp")
            .field("media", "audio")
            .field("payload", 96i32)
            .field("clock-rate", 48000i32)
            .field("encoding-name", "OPUS")
            .field("encoding-params", "2")
            .field("direction", "sendrecv")
            .field("ptime", 20i32)
            .field("sprop-stereo", "1")
            .field("rtcp-fb-nack", true)
            .field("a-tool", "test")
            .field("x-custom", "val")
            .field("extmap-1", "urn:ietf:params:rtp-hdrext:sdes:mid")
            .field(
                "extmap-2",
                gst::Array::new([
                    "sendonly",
                    "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
                    "vad=on",
                ])
                .to_send_value(),
            )
            .field("ssrc-1234-cname", "foo")
            .field("rid-1", "send")
            .field("rid-2", gst::Array::new(["recv", "apt=96"]).to_send_value())
            .build();
        let caps = gst::Caps::builder_full().structure(s).build();

        let media = caps_to_media(&caps).unwrap();
        assert_eq!(media.media(), MediaType::Audio);
        assert_eq!(media.port(), 9);
        assert_eq!(media.formats(), [96]);
        assert_eq!(media.direction(), Direction::SendRecv);
        assert_eq!(media.ptime(), Some(20));
        assert_eq!(media.rtpmaps().get(&96).unwrap().clock_rate, 48000);
        assert_eq!(
            media.fmtps().get(&96).unwrap().format_specific_params[0].param,
            "sprop-stereo"
        );
        assert_eq!(
            media.rtcp_fbs().get(&96).unwrap(),
            &RtcpFb {
                nack: true,
                ..Default::default()
            }
        );
        assert_eq!(media.extmap().len(), 2);
        assert!(
            media.extmap().contains(
                &ExtMap::builder(2, "urn:ietf:params:rtp-hdrext:ssrc-audio-level")
                    .direction(Direction::SendOnly)
                    .attributes_if_some(Some("vad=on"))
                    .build()
            )
        );
        assert_eq!(
            media.ssrcs().get(&(1234, "cname".to_string())).unwrap(),
            "foo"
        );
        assert_eq!(media.rids().len(), 2);
        assert_eq!(media.rids()[1].id, "2");
        assert_eq!(media.rids()[1].params, ["apt=96"]);
        assert_eq!(
            media.other_attributes(),
            [
                ("tool".to_string(), Some("test".to_string())),
                ("x-custom".to_string(), Some("val".to_string())),
            ]
        );

        // And back into caps.
        let caps = media_to_caps(&media).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("sprop-stereo").unwrap(), "1");
        assert_eq!(
            s.get::<&str>("extmap-1").unwrap(),
            "urn:ietf:params:rtp-hdrext:sdes:mid"
        );
        assert!(s.get::<bool>("rtcp-fb-nack").unwrap());
        assert_eq!(s.get::<&str>("a-tool").unwrap(), "test");
        assert_eq!(s.get::<&str>("x-custom").unwrap(), "val");
    }

    #[test]
    fn shared_caps_fields() {
        init();
        // media_to_caps() sets the shared fields on every structure, they
        // must only occur once in the media.
        let structure = |payload: i32, tool: &str| {
            gst::Structure::builder("application/x-rtp")
                .field("media", "audio")
                .field("payload", payload)
                .field("clock-rate", 48000i32)
                .field("encoding-name", "OPUS")
                .field("a-tool", tool)
                .field("rid-1", "send")
                .build()
        };
        let caps = gst::Caps::builder_full()
            .structure(structure(96, "test"))
            .structure(structure(97, "test"))
            .build();
        let media = caps_to_media(&caps).unwrap();
        assert_eq!(
            media.other_attributes(),
            [("tool".to_string(), Some("test".to_string()))]
        );
        assert_eq!(
            media.rids(),
            [Rid {
                id: "1".to_string(),
                direction: RidDirection::Send,
                params: vec![],
            }]
        );

        // Conflicting values are not merged, the first one wins.
        let caps = gst::Caps::builder_full()
            .structure(structure(96, "first"))
            .structure(structure(97, "second"))
            .build();
        let media = caps_to_media(&caps).unwrap();
        assert_eq!(
            media.other_attributes(),
            [("tool".to_string(), Some("first".to_string()))]
        );
    }

    #[test]
    fn typed_caps_errors() {
        init();
        type ErrorCase = (&'static str, fn(&CapsError) -> bool);
        let cases: &[ErrorCase] = &[
            ("EMPTY", |err| matches!(err, CapsError::Empty)),
            ("audio/x-raw", |err| matches!(err, CapsError::NotRtpCaps(_))),
            ("application/x-rtp,media=audio,payload=96", |err| {
                matches!(err, CapsError::MissingField { .. })
            }),
            (
                "application/x-rtp,media=audio,payload=96,clock-rate=48000,direction=foo",
                |err| matches!(err, CapsError::EnumField { .. }),
            ),
            (
                "application/x-rtp,media=audio,payload=96,clock-rate=48000",
                |err| matches!(err, CapsError::MissingEncodingName { pt: 96 }),
            ),
            // The same payload type in multiple structures.
            (
                concat!(
                    "application/x-rtp,media=audio,payload=96,clock-rate=48000,encoding-name=OPUS;",
                    "application/x-rtp,media=audio,payload=96,clock-rate=48000,encoding-name=OPUS"
                ),
                |err| matches!(err, CapsError::DuplicatePayloadType { pt: 96 }),
            ),
            // Structures with mixed media types.
            (
                concat!(
                    "application/x-rtp,media=audio,payload=96,clock-rate=48000,encoding-name=OPUS;",
                    "application/x-rtp,media=video,payload=97,clock-rate=90000,encoding-name=H264"
                ),
                |err| matches!(err, CapsError::MixedMediaTypes),
            ),
        ];
        for (caps, matches_err) in cases {
            let caps = caps.parse::<gst::Caps>().unwrap();
            let err = caps_to_media(&caps).unwrap_err();
            assert!(matches_err(&err), "{caps}: {err}");
        }

        // Dynamic payload type without rtpmap that cannot be guessed.
        let mut media = Media::new(MediaType::Video);
        media.set_formats(vec![96]);
        assert!(matches!(
            media_to_caps(&media),
            Err(CapsError::NoRtpMapGuess { pt: 96 })
        ));
    }
    #[test]
    fn rtcp_fb_wildcard_and_unknown() {
        init();
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}\
             m=audio 0 RTP/AVP 96 97\r\n\
             a=rtpmap:96 opus/48000/2\r\n\
             a=rtpmap:97 mpeg4-generic/48000/2\r\n\
             a=rtcp-fb:* nack\r\n\
             a=rtcp-fb:96 goog-remb\r\n"
        ))
        .unwrap();
        // Wildcard feedback applies to every payload type of the media,
        // specific feedback only to its own.
        let caps = media_to_caps(&sdp.media()[0]).unwrap();
        assert_eq!(caps.len(), 2);
        for s in caps.iter() {
            let pt = s.get::<i32>("payload").unwrap();
            assert!(s.get::<bool>("rtcp-fb-nack").unwrap(), "pt {pt}");
            assert_eq!(s.has_field("rtcp-fb-goog-remb"), pt == 96, "pt {pt}");
        }
        // Unknown feedback types survive as free-form parameters.
        let media = caps_to_media(&caps).unwrap();
        assert_eq!(media.rtcp_fbs()[&96].other, ["goog remb"]);
        assert!(media.rtcp_fbs()[&96].nack);
        assert_eq!(
            media.rtcp_fbs()[&97],
            RtcpFb {
                nack: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn fmtp_edge_cases() {
        init();
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 opus/48000/2\r\n\
             a=fmtp:96 sprop-stereo=1;bare;payload=97;Kmax=5; x-foo= bar ;=baz\r\n"
        ))
        .unwrap();
        let caps = media_to_caps(&sdp.media()[0]).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("sprop-stereo").unwrap(), "1");
        // Keys are lower-cased, valueless and empty-key parameters are
        // dropped and reserved keys never override the real caps fields.
        assert_eq!(s.get::<&str>("kmax").unwrap(), "5");
        assert!(!s.has_field("bare"));
        assert_eq!(s.get::<i32>("payload").unwrap(), 96);
        assert_eq!(s.get::<&str>("x-foo").unwrap(), "bar");
        assert!(!s.fields().any(|field| field.as_str().is_empty()));
    }

    #[test]
    fn rtpmap_guessing() {
        init();
        // A zero clock rate triggers the payload table fallback, for dynamic
        // payload types the lookup happens via the encoding name.
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}m=audio 0 RTP/AVP 96\r\na=rtpmap:96 opus/0\r\n"
        ))
        .unwrap();
        let caps = media_to_caps(&sdp.media()[0]).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("encoding-name").unwrap(), "OPUS");
        assert_eq!(s.get::<i32>("clock-rate").unwrap(), 48000);

        // Unknown names, media type mismatches and known names without a
        // default clock rate cannot be resolved and fail instead of producing
        // caps without a clock rate.
        for rtpmap in ["nonexistent/0", "H264/0", "rtx/0", "telephone-event/0"] {
            let sdp = Session::parse(&format!(
                "{SESSION_HEADER}m=audio 0 RTP/AVP 96\r\na=rtpmap:96 {rtpmap}\r\n"
            ))
            .unwrap();
            assert!(media_to_caps(&sdp.media()[0]).is_err(), "{rtpmap}");
        }

        // Static payload types with a zero clock rate are resolved via the
        // payload type, lowercase table entries are uppercased in the caps,
        // and dynamic types via the encoding name.
        for (mline, rtpmap, encoding_name, clock_rate) in [
            ("m=audio 0 RTP/AVP 0\r\n", "0 PCMU/0", "PCMU", 8000i32),
            ("m=video 0 RTP/AVP 28\r\n", "28 nv/0", "NV", 90000),
            (
                "m=audio 0 RTP/AVP 96\r\n",
                "96 x-pn-realaudio/0",
                "X-PN-REALAUDIO",
                1000,
            ),
        ] {
            let sdp =
                Session::parse(&format!("{SESSION_HEADER}{mline}a=rtpmap:{rtpmap}\r\n")).unwrap();
            let caps = media_to_caps(&sdp.media()[0]).unwrap();
            let s = caps.structure(0).unwrap();
            assert_eq!(
                s.get::<&str>("encoding-name").unwrap(),
                encoding_name,
                "{rtpmap}"
            );
            assert_eq!(s.get::<i32>("clock-rate").unwrap(), clock_rate, "{rtpmap}");
        }
    }

    #[test]
    fn h264_invalid_profile_level_id() {
        init();
        // Malformed values are ignored with a warning instead of failing or
        // panicking on the hex parsing.
        for value in ["42e0", "zzz01f", "42e01g", "42e01f00", "42e日"] {
            let sdp = Session::parse(&format!(
                "{SESSION_HEADER}\
                 m=video 0 RTP/AVP 96\r\n\
                 a=rtpmap:96 H264/90000\r\n\
                 a=fmtp:96 profile-level-id={value};packetization-mode=1\r\n"
            ))
            .unwrap();
            let caps = media_to_caps(&sdp.media()[0]).unwrap();
            let s = caps.structure(0).unwrap();
            assert!(!s.has_field("profile"), "{value}");
            assert!(!s.has_field("level"), "{value}");
            assert!(!s.has_field("profile-level-id"), "{value}");
            assert_eq!(s.get::<&str>("packetization-mode").unwrap(), "1");
        }

        // With level asymmetry explicitly disabled the level is kept.
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}\
             m=video 0 RTP/AVP 96\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 profile-level-id=42e01f;level-asymmetry-allowed=0\r\n"
        ))
        .unwrap();
        let caps = media_to_caps(&sdp.media()[0]).unwrap();
        let s = caps.structure(0).unwrap();
        assert!(s.has_field("profile"));
        assert!(s.has_field("level"));
        assert!(!s.has_field("level-asymmetry-allowed"));

        // A caps profile without an entry in the reverse table produces no
        // profile-level-id, but the level-asymmetry-allowed default is added.
        let caps = gst::Caps::builder_full()
            .structure(
                gst::Structure::builder("application/x-rtp")
                    .field("media", "video")
                    .field("payload", 96i32)
                    .field("clock-rate", 90000i32)
                    .field("encoding-name", "H264")
                    .field("profile", "nonexistent-profile")
                    .build(),
            )
            .build();
        let media = caps_to_media(&caps).unwrap();
        let params = &media.fmtps().get(&96).unwrap().format_specific_params;
        assert!(
            params
                .iter()
                .any(|p| p.param == "level-asymmetry-allowed" && p.val.as_deref() == Some("1"))
        );
        assert!(!params.iter().any(|p| p.param == "profile-level-id"));
    }

    #[test]
    fn crypto_media_to_caps() {
        init();
        let key: Vec<u8> = (1..=30).collect();
        let sdp = format!(
            "{SESSION_HEADER}{MEDIA_AUDIO}a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{}\r\n",
            STANDARD.encode(&key),
        );
        let media = Session::parse(&sdp).unwrap().media()[0].clone();
        let caps = media_to_caps(&media).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("srtp-cipher").unwrap(), "aes-128-icm");
        assert_eq!(s.get::<&str>("srtp-auth").unwrap(), "hmac-sha1-80");
        assert_eq!(s.get::<&str>("srtcp-cipher").unwrap(), "aes-128-icm");
        assert_eq!(s.get::<&str>("srtcp-auth").unwrap(), "hmac-sha1-80");
        let key_buf = s.get::<gst::Buffer>("srtp-key").unwrap();
        assert_eq!(key_buf.map_readable().unwrap().as_slice(), key.as_slice());
        assert!(!s.has_field("roc"));
        assert!(!s.has_field("mki"));
        assert_eq!(
            s.get::<&str>("a-crypto").unwrap(),
            media.crypto()[0].to_string()
        );

        // The derived srtp-* fields are not reconstructed, but a=crypto is
        // kept as a generic attribute.
        let media = caps_to_media(&caps).unwrap();
        assert!(media.crypto().is_empty() && media.key_mgmt().is_empty());
        assert!(
            media
                .other_attributes()
                .iter()
                .all(|(name, _)| !name.contains("srtp") && name != "mki" && name != "roc"),
            "{:?}",
            media.other_attributes(),
        );
        assert!(
            media
                .other_attributes()
                .iter()
                .any(|(name, _)| name == "crypto")
        );
    }

    #[test]
    fn crypto_mki_to_caps() {
        init();
        let sdp = format!(
            "{SESSION_HEADER}{MEDIA_AUDIO}a=crypto:1 AES_CM_128_HMAC_SHA1_32 inline:{}|2^31|3735928559:2\r\n",
            STANDARD.encode(vec![0xaau8; 30]),
        );
        let media = Session::parse(&sdp).unwrap().media()[0].clone();
        let caps = media_to_caps(&media).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("srtp-auth").unwrap(), "hmac-sha1-32");
        let mki = s.get::<gst::Buffer>("mki").unwrap();
        assert_eq!(mki.map_readable().unwrap().as_slice(), &[0xbe, 0xef]);
    }

    #[test]
    fn crypto_unsupported_suite() {
        init();
        let sdp = format!(
            "{SESSION_HEADER}{MEDIA_AUDIO}a=crypto:1 AEAD_AES_256_GCM inline:{}\r\n",
            STANDARD.encode(vec![0xaau8; 44]),
        );
        let media = Session::parse(&sdp).unwrap().media()[0].clone();
        let caps = media_to_caps(&media).unwrap();
        let s = caps.structure(0).unwrap();
        assert!(!s.has_field("srtp-cipher"));
        assert!(!s.has_field("srtp-key"));

        let s = media_to_caps(&audio_media()).unwrap();
        assert!(!s.structure(0).unwrap().has_field("srtp-cipher"));
    }

    // Hand-crafted MIKEY message (mykey wire format): PSK-Init header with
    // one SRTP-ID entry (policy 0, ROC 42), an SP payload (AES-CM-128, 32
    // bit auth key length, SRTCP encryption off) and an unencrypted KEMAC
    // payload carrying a TGK-salt key data sub-payload with a KV=SPI (the
    // MKI). Returns the message and the expected key ++ salt.
    fn mikey_message() -> (Vec<u8>, Vec<u8>) {
        let key: Vec<u8> = (0..16).collect();
        let salt: Vec<u8> = (16..30).collect();

        let mut msg = vec![
            1, 0, 10, 0, // v1, PSK-Init, SP payload next, PRF 0
            0x11, 0x22, 0x33, 0x44, // crypto system ID
            1, 0, // one crypto session, SRTP-ID map
            0, 0x0b, 0xad, 0xc0, 0xde, 0, 0, 0, 42, // SRTP-ID: policy, SSRC, ROC
            1, 0, 0, 0, 18, // SP: KEMAC next, policy 0, SRTP, 18 bytes of params
            0, 1, 1, // encryption algorithm: AES-CM-128
            3, 1, 4, // auth key length: 32 bit
            4, 1, 14, // salt key length: no caps field
            7, 1, 1, // SRTP encryption: on
            8, 1, 0, // SRTCP encryption: off
            10, 1, 1, // SRTP authentication: on
            0, 0, 0, 39, // KEMAC: last payload, NULL encryption, 39 bytes key data
            0, 0x11, 0, 16, // TGK sub-payload with KV=SPI, 16-byte key
        ];
        msg.extend_from_slice(&key);
        msg.extend_from_slice(&[0, salt.len() as u8]);
        msg.extend_from_slice(&salt);
        msg.extend_from_slice(&[2, 0xbe, 0xef]); // 2-byte MKI
        msg.push(0); // NULL MAC

        let mut expected = key;
        expected.extend_from_slice(&salt);
        (msg, expected)
    }

    #[test]
    fn mikey_media_to_caps() {
        init();
        let (msg, key) = mikey_message();
        let mut media = audio_media();
        media.set_key_mgmt(vec![KeyMgmt::with_data(
            KeyMgmtMethod::Mikey,
            STANDARD.encode(msg),
        )]);
        let caps = media_to_caps(&media).unwrap();
        let s = caps.structure(0).unwrap();
        assert_eq!(s.get::<&str>("srtp-cipher").unwrap(), "aes-128-icm");
        assert_eq!(s.get::<&str>("srtp-auth").unwrap(), "hmac-sha1-32");
        // The salt key length parameter is ignored, the SRTCP encryption
        // parameter after it still applies.
        assert_eq!(s.get::<&str>("srtcp-cipher").unwrap(), "null");
        assert_eq!(s.get::<&str>("srtcp-auth").unwrap(), "hmac-sha1-32");
        let key_buf = s.get::<gst::Buffer>("srtp-key").unwrap();
        assert_eq!(key_buf.map_readable().unwrap().as_slice(), key.as_slice());
        assert_eq!(s.get::<u32>("roc").unwrap(), 42);
        let mki = s.get::<gst::Buffer>("mki").unwrap();
        assert_eq!(mki.map_readable().unwrap().as_slice(), &[0xbe, 0xef]);
        assert_eq!(
            s.get::<&str>("a-key-mgmt").unwrap(),
            media.key_mgmt()[0].to_string()
        );
    }
}
