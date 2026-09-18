// Copyright (C) 2024 Nirbheek Chauhan <nirbheek@centricular.com>
// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! A single SDP media line and its attributes.

use std::collections::{BTreeMap, BTreeSet};

use sdp_types::{
    Candidate, Connection, Crypto, Direction, EndOfCandidates, ExtMap, Fingerprint, Fmtp,
    IceOptions, IcePwd, IceUfrag, KeyMgmt, MaxMessageSize, MediaType, Mid, Msid, RemoteCandidate,
    RemoteCandidates, Rid, Rtcp, RtcpMux, RtcpMuxOnly, RtcpRsize, RtpMap, SctpMap, SctpPort, Setup,
    Ssrc, TransportProto, TypedAttribute,
};

use super::{
    CAT,
    attrs::{
        RtcpFb, SKIPPED_OTHER_ATTRIBUTES, check_connection, direction_attr_value, other_attribute,
        parse_payload,
    },
    error::{
        MediaError, parse_typed, parse_unique_attribute_no_value,
        parse_unique_attribute_with_value, parse_unique_flag, parse_unique_typed, require_value,
    },
};

/// A single media of a [`crate::Session`].
///
/// Only audio, video and application media with RTP, SCTP or another
/// proto/profile transport string are supported, anything else is rejected
/// at parse time.
///
/// The round-trip through [`Media::from_sdp_types`] and
/// [`Media::to_sdp_types`] is lossy: media-level `i=` and `b=` lines are
/// dropped, invalid `c=` lines are dropped, and attributes are re-emitted in
/// a fixed order. Unhandled `a=` lines survive in `other_attributes` but move
/// to the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    media: MediaType,
    port: u16,
    num_ports: Option<u16>,
    proto: TransportProto,
    connections: Vec<Connection>,
    direction: Direction,
    rtcp_mux: bool,
    rtcp_mux_only: bool,
    rtcp_rsize: bool,
    // Explicit RTCP port/address, from `a=rtcp` (RFC 3605).
    rtcp: Option<Rtcp>,
    formats: Vec<u8>,
    // Raw `fmt` of the `m=` line when the tokens are not payload type
    // numbers (e.g. "webrtc-datachannel"), written back unchanged.
    fmt_raw: Option<String>,
    rtpmaps: BTreeMap<u8, RtpMap>,
    fmtps: BTreeMap<u8, Fmtp>,
    framesizes: BTreeMap<u8, String>,
    // Feedback for the wildcard payload type ("a=rtcp-fb:* ...").
    rtcp_fb: Option<RtcpFb>,
    rtcp_fbs: BTreeMap<u8, RtcpFb>,
    extmap: BTreeSet<ExtMap>,
    // (SSRC ID, attribute) -> value, from `a=ssrc:<id> <attr>:<value>`.
    ssrcs: BTreeMap<(u32, String), String>,
    rids: Vec<Rid>,
    ptime: Option<u32>,
    maxptime: Option<u32>,
    control: Option<String>,
    mid: Option<Mid>,
    msid: Option<Msid>,
    bundle_only: bool,
    ice_ufrag: Option<IceUfrag>,
    ice_pwd: Option<IcePwd>,
    ice_options: Option<IceOptions>,
    fingerprints: Vec<Fingerprint>,
    setup: Option<Setup>,
    candidates: Vec<Candidate>,
    end_of_candidates: bool,
    remote_candidates: Vec<RemoteCandidate>,
    // SCTP/datachannel transport attributes.
    sctp_port: Option<SctpPort>,
    max_message_size: Option<MaxMessageSize>,
    sctpmap: Option<SctpMap>,
    crypto: Vec<Crypto>,
    key_mgmt: Vec<KeyMgmt>,
    // Attributes without any special handling, kept as-is.
    other_attributes: Vec<(String, Option<String>)>,
}

/// Parses a unique attribute with a `u32` value (e.g. `a=ptime:`).
fn parse_unique_u32(
    key: &str,
    value: &Option<String>,
    existing: &Option<u32>,
) -> Result<u32, MediaError> {
    parse_unique_attribute_with_value(key, value, existing, |val| {
        val.parse::<u32>()
            .map_err(|source| MediaError::NumericAttribute {
                name: key.to_string(),
                source,
            })
    })
}

impl Media {
    pub fn new(media: MediaType) -> Self {
        Self {
            media,
            // WebRTC convention for a not-yet-assigned transport: port 9 is
            // the discard port, unlike 0 which means rejected/discontinued
            // (RFC 8866, RFC 3264). Callers must set a real port before the
            // SDP is used.
            port: 9,
            num_ports: None,
            proto: TransportProto::RtpAvp,
            connections: vec![],
            direction: Direction::SendRecv,
            rtcp_mux: false,
            rtcp_mux_only: false,
            rtcp_rsize: false,
            rtcp: None,
            formats: vec![],
            fmt_raw: None,
            rtpmaps: Default::default(),
            fmtps: Default::default(),
            framesizes: Default::default(),
            rtcp_fb: None,
            rtcp_fbs: Default::default(),
            extmap: Default::default(),
            ssrcs: Default::default(),
            rids: vec![],
            ptime: None,
            maxptime: None,
            control: None,
            mid: None,
            msid: None,
            bundle_only: false,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: None,
            fingerprints: vec![],
            setup: None,
            candidates: vec![],
            end_of_candidates: false,
            remote_candidates: vec![],
            sctp_port: None,
            max_message_size: None,
            sctpmap: None,
            crypto: vec![],
            key_mgmt: vec![],
            other_attributes: vec![],
        }
    }

    /// Creates a new [`MediaBuilder`].
    pub fn builder(media: MediaType) -> MediaBuilder {
        MediaBuilder {
            inner: Self::new(media),
        }
    }

    val_accessors! {
        media, set_media: MediaType,
        port, set_port: u16,
        num_ports, set_num_ports: Option<u16>,
        direction, set_direction: Direction,
        rtcp_mux, set_rtcp_mux: bool,
        rtcp_mux_only, set_rtcp_mux_only: bool,
        rtcp_rsize, set_rtcp_rsize: bool,
        bundle_only, set_bundle_only: bool,
        ptime, set_ptime: Option<u32>,
        maxptime, set_maxptime: Option<u32>,
        end_of_candidates, set_end_of_candidates: bool,
    }
    opt_accessors! {
        rtcp_fb, rtcp_fb_mut, set_rtcp_fb: Option<RtcpFb>,
        setup, setup_mut, set_setup: Option<Setup>,
        mid, mid_mut, set_mid: Option<Mid>,
        msid, msid_mut, set_msid: Option<Msid>,
        ice_ufrag, ice_ufrag_mut, set_ice_ufrag: Option<IceUfrag>,
        ice_pwd, ice_pwd_mut, set_ice_pwd: Option<IcePwd>,
        ice_options, ice_options_mut, set_ice_options: Option<IceOptions>,
        sctp_port, sctp_port_mut, set_sctp_port: Option<SctpPort>,
        max_message_size, max_message_size_mut, set_max_message_size: Option<MaxMessageSize>,
        sctpmap, sctpmap_mut, set_sctpmap: Option<SctpMap>,
        rtcp, rtcp_mut, set_rtcp: Option<Rtcp>,
    }

    /// The first media-level `c=` line, if any.
    pub fn connection(&self) -> Option<&Connection> {
        self.connections.first()
    }

    /// The first media-level `c=` line, if any.
    pub fn connection_mut(&mut self) -> Option<&mut Connection> {
        self.connections.first_mut()
    }

    /// Replaces the media-level `c=` lines with at most one connection.
    pub fn set_connection(&mut self, value: Option<Connection>) {
        self.connections = value.into_iter().collect();
    }

    /// All media-level `c=` lines.
    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }

    /// All media-level `c=` lines.
    pub fn connections_mut(&mut self) -> &mut Vec<Connection> {
        &mut self.connections
    }

    /// Replaces all media-level `c=` lines.
    pub fn set_connections(&mut self, value: Vec<Connection>) {
        self.connections = value;
    }

    opt_str_accessors! {
        fmt_raw, fmt_raw_mut, set_fmt_raw,
        control, control_mut, set_control,
    }
    vec_accessors! {
        formats, formats_mut, set_formats: Vec<u8>,
        rids, rids_mut, set_rids: Vec<Rid>,
        fingerprints, fingerprints_mut, set_fingerprints: Vec<Fingerprint>,
        candidates, candidates_mut, set_candidates: Vec<Candidate>,
        remote_candidates, remote_candidates_mut, set_remote_candidates: Vec<RemoteCandidate>,
        crypto, crypto_mut, set_crypto: Vec<Crypto>,
        key_mgmt, key_mgmt_mut, set_key_mgmt: Vec<KeyMgmt>,
        other_attributes, other_attributes_mut, set_other_attributes: Vec<(String, Option<String>)>,
    }
    ref_accessors! {
        proto, proto_mut, set_proto: TransportProto,
        rtpmaps, rtpmaps_mut, set_rtpmaps: BTreeMap<u8, RtpMap>,
        fmtps, fmtps_mut, set_fmtps: BTreeMap<u8, Fmtp>,
        framesizes, framesizes_mut, set_framesizes: BTreeMap<u8, String>,
        rtcp_fbs, rtcp_fbs_mut, set_rtcp_fbs: BTreeMap<u8, RtcpFb>,
        extmap, extmap_mut, set_extmap: BTreeSet<ExtMap>,
        ssrcs, ssrcs_mut, set_ssrcs: BTreeMap<(u32, String), String>,
    }

    /// Parses a single media line of a session.
    ///
    /// `session_direction` is the session-level direction used when the
    /// media has none of its own, `mline` is the index of this media in
    /// the session and is only used in log messages.
    pub fn from_sdp_types(
        session_direction: Option<Direction>,
        mline: u32,
        media: &sdp_types::Media,
    ) -> Result<Self, MediaError> {
        let media_type = media
            .media
            .parse::<MediaType>()
            .map_err(MediaError::MediaType)?;
        if !matches!(
            media_type,
            MediaType::Audio | MediaType::Video | MediaType::Application
        ) {
            return Err(MediaError::UnsupportedMediaType(media.media.clone()));
        }
        // Anything that looks like a proto/profile pair is accepted (e.g.
        // "TCP/RTP/AVP" which has no dedicated constant), bare words without
        // a profile are rejected.
        let proto = TransportProto::from(media.proto.as_str());
        if !proto.is_rtp()
            && !proto.is_sctp()
            && !matches!(&proto, TransportProto::Other(other) if other.contains('/'))
        {
            return Err(MediaError::UnsupportedProtocol(media.proto.clone()));
        }

        let formats = media
            .fmt
            .split_whitespace()
            .map(parse_payload)
            .collect::<Result<Vec<_>, MediaError>>();
        let (formats, fmt_raw) = match formats {
            Ok(formats) => (formats, None),
            // Not a list of payload type numbers (e.g. "webrtc-datachannel"
            // for SCTP data channels), keep it as-is.
            Err(_) => (vec![], Some(media.fmt.trim().to_string())),
        };

        let mut connections = Vec::with_capacity(media.connections.len());
        for connection in &media.connections {
            if let Err(err) = check_connection(connection) {
                gst::warning!(CAT, "Ignoring invalid connection in media {mline}: {err}");
                continue;
            }
            connections.push(connection.clone());
        }

        let mut direction = None;
        let mut ret = Self::new(media_type);
        ret.port = media.port;
        ret.num_ports = media.num_ports;
        ret.proto = proto;
        ret.connections = connections;
        ret.formats = formats;
        ret.fmt_raw = fmt_raw;

        for attr in media.attributes.iter() {
            let key = attr.attribute.as_str();

            if let Some(dir) = direction_attr_value(key) {
                direction = Some(parse_unique_attribute_no_value(
                    key,
                    &attr.value,
                    &direction,
                    dir,
                )?);
                continue;
            }

            // The attributes that sdp-types has no typed counterpart for are
            // still keyed by string name here and in `to_sdp_types()`.
            match key {
                RtcpMux::NAME => ret.rtcp_mux = parse_unique_flag(key, &attr.value, ret.rtcp_mux)?,
                RtcpMuxOnly::NAME => {
                    ret.rtcp_mux_only = parse_unique_flag(key, &attr.value, ret.rtcp_mux_only)?
                }
                RtcpRsize::NAME => {
                    ret.rtcp_rsize = parse_unique_flag(key, &attr.value, ret.rtcp_rsize)?
                }
                "control" => {
                    ret.control = Some(parse_unique_attribute_with_value(
                        key,
                        &attr.value,
                        &ret.control,
                        |val| {
                            if val.is_empty() {
                                return Err(MediaError::InvalidAttribute {
                                    name: key.to_string(),
                                    reason: "empty value".into(),
                                });
                            }
                            Ok(val.to_string())
                        },
                    )?)
                }
                Mid::NAME => {
                    ret.mid = Some(parse_unique_typed::<Mid, MediaError>(
                        key,
                        &attr.value,
                        &ret.mid,
                    )?)
                }
                "bundle-only" => {
                    ret.bundle_only = parse_unique_flag(key, &attr.value, ret.bundle_only)?
                }
                IceUfrag::NAME => {
                    ret.ice_ufrag = Some(parse_unique_typed::<IceUfrag, MediaError>(
                        key,
                        &attr.value,
                        &ret.ice_ufrag,
                    )?)
                }
                IcePwd::NAME => {
                    ret.ice_pwd = Some(parse_unique_typed::<IcePwd, MediaError>(
                        key,
                        &attr.value,
                        &ret.ice_pwd,
                    )?)
                }
                IceOptions::NAME => {
                    ret.ice_options = Some(parse_unique_typed::<IceOptions, MediaError>(
                        key,
                        &attr.value,
                        &ret.ice_options,
                    )?)
                }
                Fingerprint::NAME => ret
                    .fingerprints
                    .push(parse_typed::<Fingerprint, MediaError>(
                        key,
                        require_value(key, &attr.value)?,
                    )?),
                Setup::NAME => {
                    ret.setup = Some(parse_unique_typed::<Setup, MediaError>(
                        key,
                        &attr.value,
                        &ret.setup,
                    )?)
                }
                Candidate::NAME => ret.candidates.push(parse_typed::<Candidate, MediaError>(
                    key,
                    require_value(key, &attr.value)?,
                )?),
                EndOfCandidates::NAME => {
                    ret.end_of_candidates =
                        parse_unique_flag(key, &attr.value, ret.end_of_candidates)?
                }
                RemoteCandidates::NAME => ret.remote_candidates.extend(
                    parse_typed::<RemoteCandidates, MediaError>(
                        key,
                        require_value(key, &attr.value)?,
                    )?
                    .0,
                ),
                Msid::NAME => {
                    ret.msid = Some(parse_unique_typed::<Msid, MediaError>(
                        key,
                        &attr.value,
                        &ret.msid,
                    )?)
                }
                SctpPort::NAME => {
                    ret.sctp_port = Some(parse_unique_typed::<SctpPort, MediaError>(
                        key,
                        &attr.value,
                        &ret.sctp_port,
                    )?)
                }
                MaxMessageSize::NAME => {
                    ret.max_message_size = Some(parse_unique_typed::<MaxMessageSize, MediaError>(
                        key,
                        &attr.value,
                        &ret.max_message_size,
                    )?)
                }
                SctpMap::NAME => {
                    ret.sctpmap = Some(parse_unique_typed::<SctpMap, MediaError>(
                        key,
                        &attr.value,
                        &ret.sctpmap,
                    )?)
                }
                Crypto::NAME => ret.crypto.push(parse_typed::<Crypto, MediaError>(
                    key,
                    require_value(key, &attr.value)?,
                )?),
                KeyMgmt::NAME => ret.key_mgmt.push(parse_typed::<KeyMgmt, MediaError>(
                    key,
                    require_value(key, &attr.value)?,
                )?),
                Rtcp::NAME => {
                    ret.rtcp = Some(parse_unique_typed::<Rtcp, MediaError>(
                        key,
                        &attr.value,
                        &ret.rtcp,
                    )?)
                }
                sdp_types::RtcpFb::NAME => {
                    let value = require_value(key, &attr.value)?;
                    let (fb, payload) = RtcpFb::from_sdp_str(value)?;
                    if let Some(payload) = payload {
                        if !ret.formats.contains(&payload) {
                            return Err(MediaError::UnknownPayloadType {
                                name: key.to_string(),
                                pt: payload,
                            });
                        }
                        ret.rtcp_fbs
                            .entry(payload)
                            .and_modify(|entry| *entry |= fb.clone())
                            .or_insert(fb);
                    } else {
                        *ret.rtcp_fb.get_or_insert_with(RtcpFb::default) |= fb;
                    }
                }
                RtpMap::NAME => {
                    let value = require_value(key, &attr.value)?;
                    let rtpmap = parse_typed::<RtpMap, MediaError>(key, value)?;
                    if !ret.formats.contains(&rtpmap.payload_type) {
                        return Err(MediaError::UnknownPayloadType {
                            name: key.to_string(),
                            pt: rtpmap.payload_type,
                        });
                    }
                    let payload_type = rtpmap.payload_type;
                    if ret.rtpmaps.insert(payload_type, rtpmap).is_some() {
                        return Err(MediaError::DuplicateForPayloadType {
                            name: key.to_string(),
                            pt: payload_type,
                        });
                    }
                }
                Fmtp::NAME => {
                    let value = require_value(key, &attr.value)?;
                    let fmtp = parse_typed::<Fmtp, MediaError>(key, value)?;
                    if !ret.formats.contains(&fmtp.fmt) {
                        return Err(MediaError::UnknownPayloadType {
                            name: key.to_string(),
                            pt: fmtp.fmt,
                        });
                    }
                    let fmt = fmtp.fmt;
                    if ret.fmtps.insert(fmt, fmtp).is_some() {
                        return Err(MediaError::DuplicateForPayloadType {
                            name: key.to_string(),
                            pt: fmt,
                        });
                    }
                }
                "framesize" => {
                    let value = require_value(key, &attr.value)?;
                    let Some((pt, dims)) = value.split_once(' ') else {
                        return Err(MediaError::InvalidAttribute {
                            name: key.to_string(),
                            reason: "missing payload type".into(),
                        });
                    };
                    let pt = parse_payload(pt)?;
                    if !ret.formats.contains(&pt) {
                        return Err(MediaError::UnknownPayloadType {
                            name: key.to_string(),
                            pt,
                        });
                    }
                    use std::collections::btree_map::Entry;
                    match ret.framesizes.entry(pt) {
                        Entry::Vacant(vacant) => {
                            vacant.insert(dims.to_string());
                        }
                        Entry::Occupied(_) => {
                            gst::fixme!(
                                CAT,
                                "multiple 'framesize' attributes for payload type {pt} in media {mline}, keeping the first one"
                            );
                        }
                    }
                }
                ExtMap::NAME => {
                    let value = require_value(key, &attr.value)?;
                    let ext = parse_typed::<ExtMap, MediaError>(key, value)?;
                    if ext.id == 15 {
                        return Err(MediaError::InvalidAttribute {
                            name: key.to_string(),
                            reason: "reserved id 15".into(),
                        });
                    }
                    ret.extmap.insert(ext);
                }
                Ssrc::NAME => {
                    let value = require_value(key, &attr.value)?;
                    let ssrc = parse_typed::<Ssrc, MediaError>(key, value)?;
                    let name = ssrc.attribute.as_str().to_string();
                    if ret
                        .ssrcs
                        .insert((ssrc.ssrc_id, name.clone()), ssrc.value.unwrap_or_default())
                        .is_some()
                    {
                        return Err(MediaError::InvalidAttribute {
                            name: key.to_string(),
                            reason: format!("multiple attributes for {} {name}", ssrc.ssrc_id),
                        });
                    }
                }
                Rid::NAME => ret.rids.push(parse_typed::<Rid, MediaError>(
                    key,
                    require_value(key, &attr.value)?,
                )?),
                "ptime" => ret.ptime = Some(parse_unique_u32(key, &attr.value, &ret.ptime)?),
                "maxptime" => {
                    ret.maxptime = Some(parse_unique_u32(key, &attr.value, &ret.maxptime)?)
                }
                _ => {
                    if !SKIPPED_OTHER_ATTRIBUTES.contains(&key) {
                        gst::fixme!(CAT, "unknown media {mline} attribute {key}");
                    }
                    ret.other_attributes
                        .push((key.to_string(), attr.value.clone()));
                }
            }
        }

        ret.direction = direction
            .or(session_direction)
            .unwrap_or(Direction::SendRecv);

        Ok(ret)
    }

    /// The `fmt` list of the `m=` line rendered back as a string.
    pub fn fmt_string(&self) -> String {
        self.fmt_raw.clone().unwrap_or_else(|| {
            self.formats
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        })
    }

    /// Converts back into a `sdp_types` media line.
    pub fn to_sdp_types(&self) -> sdp_types::Media {
        let mut builder =
            sdp_types::Media::builder(self.media, self.port, self.proto.clone(), self.fmt_string())
                .num_ports_if_some(self.num_ports)
                .connections(self.connections.clone())
                .attribute(self.direction)
                .attribute_if(RtcpMux, self.rtcp_mux)
                .attribute_if(RtcpMuxOnly, self.rtcp_mux_only)
                .attribute_if(RtcpRsize, self.rtcp_rsize)
                .attribute_if_some(self.rtcp.clone())
                .attribute_with_value_if_some("control", self.control.as_ref())
                .attribute_if_some(self.mid.clone())
                .attribute_if_some(self.msid.clone())
                .attribute_from_str_if("bundle-only", self.bundle_only)
                .attribute_if_some(self.ice_ufrag.clone())
                .attribute_if_some(self.ice_pwd.clone())
                .attribute_if_some(self.ice_options.clone());

        builder = builder
            .attributes(self.fingerprints.clone())
            .attribute_if_some(self.setup.clone())
            .attributes(self.candidates.clone())
            .attribute_if(EndOfCandidates, self.end_of_candidates);
        // The RFC 8839 section 5.2 grammar allows one attribute with all
        // entries while its example uses one single-entry attribute per
        // component. Emit the multi-entry form, parsing accepts both.
        builder = builder.attribute_if_some(
            (!self.remote_candidates.is_empty())
                .then(|| RemoteCandidates::new(self.remote_candidates.iter().cloned())),
        );
        builder = builder
            .attribute_if_some(self.sctp_port)
            .attribute_if_some(self.max_message_size)
            .attribute_if_some(self.sctpmap.clone())
            .attributes(self.crypto.clone())
            .attributes(self.key_mgmt.clone());

        for (payload, fb) in self
            .rtcp_fbs
            .iter()
            .map(|(pt, fb)| (pt.to_string(), fb))
            .chain(self.rtcp_fb.iter().map(|fb| ("*".to_string(), fb)))
        {
            for param in fb.sdp_params() {
                builder = builder.attribute_with_value("rtcp-fb", format!("{payload} {param}"));
            }
        }

        builder = builder
            .attribute_with_value_if_some("ptime", self.ptime)
            .attribute_with_value_if_some("maxptime", self.maxptime)
            .attributes(self.rtpmaps.values().cloned())
            .attributes(self.framesizes.iter().map(|(pt, dims)| {
                sdp_types::Attribute::with_value("framesize", format!("{pt} {dims}"))
            }))
            .attributes(self.fmtps.values().cloned())
            .attributes(self.extmap.iter().cloned().map(|mut ext| {
                if matches!(ext.direction, Some(Direction::SendRecv)) {
                    ext.direction = None;
                }
                ext
            }))
            .attributes(self.ssrcs.iter().map(|((id, attr), val)| {
                let value = if val.is_empty() {
                    format!("{id} {attr}")
                } else {
                    format!("{id} {attr}:{val}")
                };
                sdp_types::Attribute::with_value("ssrc", value)
            }))
            .attributes(self.rids.clone())
            .attributes(self.other_attributes.iter().map(other_attribute));

        builder.build()
    }
}

/// Builder for [`Media`], starting from the [`Media::new`] defaults.
#[derive(Debug, Clone)]
pub struct MediaBuilder {
    inner: Media,
}

impl MediaBuilder {
    builder_fns! {
        media: MediaType,
        port: u16,
        num_ports: Option<u16>,
        proto: TransportProto,
        direction: Direction,
        rtcp_mux: bool,
        rtcp_mux_only: bool,
        rtcp_rsize: bool,
        rtcp: Option<Rtcp>,
        formats: Vec<u8>,
        fmt_raw: Option<String>,
        rtpmaps: BTreeMap<u8, RtpMap>,
        fmtps: BTreeMap<u8, Fmtp>,
        framesizes: BTreeMap<u8, String>,
        rtcp_fb: Option<RtcpFb>,
        rtcp_fbs: BTreeMap<u8, RtcpFb>,
        extmap: BTreeSet<ExtMap>,
        ssrcs: BTreeMap<(u32, String), String>,
        rids: Vec<Rid>,
        ptime: Option<u32>,
        maxptime: Option<u32>,
        control: Option<String>,
        mid: Option<Mid>,
        msid: Option<Msid>,
        bundle_only: bool,
        ice_ufrag: Option<IceUfrag>,
        ice_pwd: Option<IcePwd>,
        ice_options: Option<IceOptions>,
        fingerprints: Vec<Fingerprint>,
        setup: Option<Setup>,
        candidates: Vec<Candidate>,
        end_of_candidates: bool,
        remote_candidates: Vec<RemoteCandidate>,
        sctp_port: Option<SctpPort>,
        max_message_size: Option<MaxMessageSize>,
        sctpmap: Option<SctpMap>,
        crypto: Vec<Crypto>,
        key_mgmt: Vec<KeyMgmt>,
        other_attributes: Vec<(String, Option<String>)>,
    }

    pub fn connection(mut self, value: Option<Connection>) -> Self {
        self.inner.connections = value.into_iter().collect();
        self
    }

    pub fn connections(mut self, value: Vec<Connection>) -> Self {
        self.inner.connections = value;
        self
    }

    pub fn build(self) -> Media {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use crate::tests::{MEDIA_AUDIO, MEDIA_VIDEO, SESSION_HEADER, init};
    use std::net::Ipv4Addr;

    // The public sdp_types bridge used by sapsink/sapsrc.
    #[test]
    fn sdp_types_bridge() {
        init();
        let text = format!("{SESSION_HEADER}{MEDIA_AUDIO}{MEDIA_VIDEO}");
        let session = sdp_types::Session::parse(text.as_bytes()).unwrap();
        let medias: Vec<Media> = session
            .medias
            .iter()
            .enumerate()
            .map(|(idx, m)| Media::from_sdp_types(None, idx as u32, m).unwrap())
            .collect();

        let mut session = sdp_types::Session::new(
            sdp_types::Origin::builder_with_ip_addr("1234567890", 0, Ipv4Addr::UNSPECIFIED)
                .username("-")
                .build(),
            "-",
        );
        session.medias = medias.iter().map(|m| m.to_sdp_types()).collect();
        let mut buf = Vec::new();
        session.write(&mut buf).unwrap();
        let reparsed = Session::parse(&String::from_utf8(buf).unwrap()).unwrap();
        assert_eq!(reparsed.media(), medias);
    }

    #[test]
    fn multiple_connections() {
        init();
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}\
             m=audio 0 RTP/AVP 96\r\n\
             c=IN IP4 192.0.2.1\r\n\
             c=IN IP6 2001:db8::1\r\n\
             a=rtpmap:96 opus/48000/2\r\n"
        ))
        .unwrap();
        let media = &sdp.media()[0];
        assert_eq!(
            media
                .connections()
                .iter()
                .map(|c| c.connection_address.as_str())
                .collect::<Vec<_>>(),
            ["192.0.2.1", "2001:db8::1"]
        );
        assert_eq!(media.connection().unwrap().connection_address, "192.0.2.1");
        assert_eq!(
            Session::parse(&sdp.to_sdp_string()).unwrap().media()[0],
            *media
        );
    }

    #[test]
    fn crypto_key_mgmt_errors() {
        use crate::ParseError;

        init();
        let key = "WnFW7GRz0pWUCge8Ff93kiBhYc1yLLoHLOoyQnBsPrhU";
        let err = Session::parse(&format!(
            concat!(
                "{SESSION_HEADER}",
                "{MEDIA_AUDIO}",
                "a=crypto:1 AES_CM_128_HMAC_SHA1_80 master key 1 {key}\r\n"
            ),
            SESSION_HEADER = SESSION_HEADER,
            MEDIA_AUDIO = MEDIA_AUDIO,
            key = key,
        ))
        .unwrap_err();
        match err {
            ParseError::Media { source, .. } => match source {
                MediaError::TypedAttribute { name, .. } => assert_eq!(name, "crypto"),
                err => panic!("unexpected error: {err:?}"),
            },
            err => panic!("unexpected error: {err:?}"),
        }

        let err = Session::parse(&format!(
            concat!("{SESSION_HEADER}", "{MEDIA_AUDIO}", "a=key-mgmt\r\n"),
            SESSION_HEADER = SESSION_HEADER,
            MEDIA_AUDIO = MEDIA_AUDIO,
        ))
        .unwrap_err();
        assert!(matches!(err, ParseError::Media { .. }), "{err:?}");
    }

    #[test]
    fn rtcp_attribute() {
        init();
        for (rtcp_attr, address) in [
            ("a=rtcp:53020 IN IP4 192.0.2.1\r\n", Some("192.0.2.1")),
            // Port-only form.
            ("a=rtcp:53020\r\n", None),
        ] {
            let session = Session::parse(&format!(
                concat!(
                    "{SESSION_HEADER}",
                    "{MEDIA_AUDIO}",
                    "{rtcp_attr}",
                    "{MEDIA_VIDEO}"
                ),
                SESSION_HEADER = SESSION_HEADER,
                MEDIA_AUDIO = MEDIA_AUDIO,
                MEDIA_VIDEO = MEDIA_VIDEO,
                rtcp_attr = rtcp_attr,
            ))
            .unwrap();
            let rtcp = session.media()[0].rtcp().expect("parsed rtcp attribute");
            assert_eq!(rtcp.port, 53020);
            assert_eq!(rtcp.connection_address.as_deref(), address);
            assert_eq!(session.media()[1].rtcp(), None);
            assert_eq!(
                Session::from_sdp_types(&session.to_sdp_types()).unwrap(),
                session
            );
        }
    }

    #[test]
    fn fmt_string() {
        init();
        let mut media = Media::new(MediaType::Audio);
        media.set_formats(vec![0, 96]);
        assert_eq!(media.fmt_string(), "0 96");
        media.set_fmt_raw(Some("webrtc-datachannel".to_string()));
        assert_eq!(media.fmt_string(), "webrtc-datachannel");
    }
}
