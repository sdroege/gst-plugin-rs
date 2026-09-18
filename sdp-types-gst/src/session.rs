// Copyright (C) 2024 Nirbheek Chauhan <nirbheek@centricular.com>
// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! An SDP session: parsing a whole announcement and rendering it back.

use sdp_types::{
    AddrType, Direction, Group, GroupSemantics, IceLite, IceOptions, IcePwd, IceUfrag, KeyMgmt,
    NetType, Origin, TimeZone, TypedAttribute,
};

use super::{
    CAT,
    attrs::{check_connection, direction_attr_value, other_attribute},
    error::{
        MediaError, ParseError, parse_typed, parse_unique_attribute_no_value, parse_unique_flag,
        parse_unique_typed, require_value,
    },
    media::Media,
};

/// Parse the value of a session-level `a=group` attribute (RFC 5888 section 5).
///
/// Only BUNDLE groups are acted upon, so a group with another semantics is
/// never an error, not even one without any identification-tag: an application
/// has to ignore the groups it does not understand (RFC 5888 section 6) instead
/// of discarding the whole announcement.
fn parse_group(value: &str) -> Result<Group, sdp_types::AttributeError> {
    let group = value.parse::<Group>()?;

    // RFC 8843 section 5: an "m=" line is only part of the BUNDLE group if its
    // identification-tag is listed in it, so an empty list is no group.
    if group.mid_tags.is_empty() && matches!(group.semantics, GroupSemantics::Bundle) {
        return Err(sdp_types::AttributeError::ParamNotFound {
            param: "Media identification tags".to_string(),
            attr: "group".to_string(),
        });
    }

    Ok(group)
}

/// An SDP session.
///
/// Session-level `c=` data is kept separate from media-level `c=` lines; use
/// `media.connections()` and fall back to `sdp.connection()`.
///
/// The round-trip through [`Session::from_sdp_types`] and
/// [`Session::to_sdp_types`] is lossy: the `k=` key is dropped, attributes are
/// re-emitted in a fixed order, and the identification-tags of an `a=group`
/// line are separated by a single space again. Unhandled `a=` lines survive in
/// `other_attributes` but move to the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    origin: Origin,
    name: String,
    information: Option<String>,
    uri: Option<String>,
    emails: Vec<String>,
    phones: Vec<String>,
    connection: Option<sdp_types::Connection>,
    bandwidths: Vec<sdp_types::Bandwidth>,
    times: Vec<sdp_types::Time>,
    time_zones: Vec<TimeZone>,
    groups: Vec<Group>,
    ice_lite: bool,
    ice_ufrag: Option<IceUfrag>,
    ice_pwd: Option<IcePwd>,
    ice_options: Option<IceOptions>,
    fingerprints: Vec<sdp_types::Fingerprint>,
    setup: Option<sdp_types::Setup>,
    crypto: Vec<sdp_types::Crypto>,
    key_mgmt: Vec<KeyMgmt>,
    direction: Option<Direction>,
    other_attributes: Vec<(String, Option<String>)>,
    media: Vec<Media>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            origin: Origin {
                username: None,
                sess_id: "0".to_string(),
                sess_version: 0,
                nettype: NetType::In,
                addrtype: AddrType::Ip4,
                unicast_address: "0.0.0.0".to_string(),
            },
            name: "-".to_string(),
            information: None,
            uri: None,
            emails: vec![],
            phones: vec![],
            connection: None,
            bandwidths: vec![],
            times: vec![],
            time_zones: vec![],
            groups: vec![],
            ice_lite: false,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: None,
            fingerprints: vec![],
            setup: None,
            crypto: vec![],
            key_mgmt: vec![],
            direction: None,
            other_attributes: vec![],
            media: vec![],
        }
    }
}

impl Session {
    /// Creates a new [`SessionBuilder`].
    pub fn builder() -> SessionBuilder {
        SessionBuilder {
            inner: Self::default(),
        }
    }

    val_accessors! {
        ice_lite, set_ice_lite: bool,
        direction, set_direction: Option<Direction>,
    }
    opt_accessors! {
        connection, connection_mut, set_connection: Option<sdp_types::Connection>,
        ice_ufrag, ice_ufrag_mut, set_ice_ufrag: Option<IceUfrag>,
        ice_pwd, ice_pwd_mut, set_ice_pwd: Option<IcePwd>,
        ice_options, ice_options_mut, set_ice_options: Option<IceOptions>,
        setup, setup_mut, set_setup: Option<sdp_types::Setup>,
    }
    opt_str_accessors! {
        information, information_mut, set_information,
        uri, uri_mut, set_uri,
    }
    vec_accessors! {
        emails, emails_mut, set_emails: Vec<String>,
        phones, phones_mut, set_phones: Vec<String>,
        bandwidths, bandwidths_mut, set_bandwidths: Vec<sdp_types::Bandwidth>,
        times, times_mut, set_times: Vec<sdp_types::Time>,
        time_zones, time_zones_mut, set_time_zones: Vec<TimeZone>,
        groups, groups_mut, set_groups: Vec<Group>,
        fingerprints, fingerprints_mut, set_fingerprints: Vec<sdp_types::Fingerprint>,
        crypto, crypto_mut, set_crypto: Vec<sdp_types::Crypto>,
        key_mgmt, key_mgmt_mut, set_key_mgmt: Vec<KeyMgmt>,
        other_attributes, other_attributes_mut, set_other_attributes: Vec<(String, Option<String>)>,
        media, media_mut, set_media: Vec<Media>,
    }
    ref_accessors! {
        origin, origin_mut, set_origin: Origin,
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, value: String) {
        self.name = value;
    }

    pub fn name_mut(&mut self) -> &mut String {
        &mut self.name
    }

    /// Parses an SDP session, failing on the first invalid media line.
    pub fn parse(session: &str) -> Result<Self, ParseError> {
        let session = sdp_types::Session::parse(session.as_bytes()).map_err(ParseError::Syntax)?;
        Self::from_sdp_types(&session)
    }

    /// Parses an SDP session, skipping invalid media lines and returning
    /// their errors alongside the session.
    pub fn parse_skip_invalid(session: &str) -> Result<(Self, Vec<(u32, MediaError)>), ParseError> {
        let session = sdp_types::Session::parse(session.as_bytes()).map_err(ParseError::Syntax)?;
        Self::from_sdp_types_skip_invalid(&session)
    }

    /// Converts a parsed SDP session, failing on the first invalid media
    /// line.
    pub fn from_sdp_types(session: &sdp_types::Session) -> Result<Self, ParseError> {
        let (sdp, errors) = Self::from_sdp_types_skip_invalid(session)?;
        if let Some((mline, err)) = errors.into_iter().next() {
            return Err(ParseError::Media { mline, source: err });
        }
        Ok(sdp)
    }

    /// Converts a parsed SDP session, skipping invalid media lines and
    /// returning their errors alongside the session.
    pub fn from_sdp_types_skip_invalid(
        session: &sdp_types::Session,
    ) -> Result<(Self, Vec<(u32, MediaError)>), ParseError> {
        if let Some(connection) = session.connection.as_ref()
            && let Err(err) = check_connection(connection)
        {
            // Keep the raw value; consumers can still decide how to use it.
            gst::warning!(CAT, "Ignoring invalid session connection: {err}");
        }

        let mut ret = Self {
            origin: session.origin.clone(),
            name: session.session_name.clone(),
            information: session.session_description.clone(),
            uri: session.uri.clone(),
            emails: session.emails.clone(),
            phones: session.phones.clone(),
            connection: session.connection.clone(),
            bandwidths: session.bandwidths.clone(),
            times: session.times.clone(),
            time_zones: session.time_zones.clone(),
            ..Self::default()
        };
        for attr in session.attributes.iter() {
            let key = attr.attribute.as_str();

            if let Some(dir) = direction_attr_value(key) {
                ret.direction = Some(parse_unique_attribute_no_value(
                    key,
                    &attr.value,
                    &ret.direction,
                    dir,
                )?);
                continue;
            }

            match key {
                Group::NAME => {
                    let value = require_value(key, &attr.value)?;
                    ret.groups
                        .push(parse_group(value).map_err(|source| ParseError::Group { source })?);
                }
                IceLite::NAME => ret.ice_lite = parse_unique_flag(key, &attr.value, ret.ice_lite)?,
                IceUfrag::NAME => {
                    ret.ice_ufrag = Some(parse_unique_typed::<IceUfrag, ParseError>(
                        key,
                        &attr.value,
                        &ret.ice_ufrag,
                    )?)
                }
                IcePwd::NAME => {
                    ret.ice_pwd = Some(parse_unique_typed::<IcePwd, ParseError>(
                        key,
                        &attr.value,
                        &ret.ice_pwd,
                    )?)
                }
                IceOptions::NAME => {
                    ret.ice_options = Some(parse_unique_typed::<IceOptions, ParseError>(
                        key,
                        &attr.value,
                        &ret.ice_options,
                    )?)
                }
                sdp_types::Fingerprint::NAME => {
                    ret.fingerprints
                        .push(parse_typed::<sdp_types::Fingerprint, ParseError>(
                            key,
                            require_value(key, &attr.value)?,
                        )?)
                }
                sdp_types::Setup::NAME => {
                    ret.setup = Some(parse_unique_typed::<sdp_types::Setup, ParseError>(
                        key,
                        &attr.value,
                        &ret.setup,
                    )?)
                }
                sdp_types::Crypto::NAME => {
                    ret.crypto
                        .push(parse_typed::<sdp_types::Crypto, ParseError>(
                            key,
                            require_value(key, &attr.value)?,
                        )?)
                }
                KeyMgmt::NAME => ret.key_mgmt.push(parse_typed::<KeyMgmt, ParseError>(
                    key,
                    require_value(key, &attr.value)?,
                )?),
                _ => {
                    ret.other_attributes
                        .push((attr.attribute.clone(), attr.value.clone()));
                }
            }
        }

        let mut media = Vec::with_capacity(session.medias.len());
        let mut errors = vec![];
        for (mline, m) in session.medias.iter().enumerate() {
            let mline = mline as u32;
            match Media::from_sdp_types(ret.direction, mline, m) {
                Ok(m) => media.push(m),
                Err(err) => errors.push((mline, err)),
            }
        }
        ret.media = media;

        Ok((ret, errors))
    }

    /// Key management attributes of the media line at `media_idx`, falling
    /// back to the session-level ones when the media line has none (RFC
    /// 4567 section 4.1), or `None` if there is no such media line.
    pub fn effective_key_mgmt(&self, media_idx: usize) -> Option<&[KeyMgmt]> {
        let media = self.media.get(media_idx)?;
        Some(if media.key_mgmt().is_empty() {
            &self.key_mgmt
        } else {
            media.key_mgmt()
        })
    }

    /// Crypto attributes of the media line at `media_idx`, falling back to
    /// the session-level ones when the media line has none (RFC 4567
    /// section 4.1), or `None` if there is no such media line.
    pub fn effective_crypto(&self, media_idx: usize) -> Option<&[sdp_types::Crypto]> {
        let media = self.media.get(media_idx)?;
        Some(if media.crypto().is_empty() {
            &self.crypto
        } else {
            media.crypto()
        })
    }

    /// The tags of the first session-level `a=group:BUNDLE` group, or an
    /// empty slice if the group has to be ignored.
    ///
    /// RFC 8843 section 5 allows several BUNDLE groups but only the first one
    /// is handled here, use `groups()` to see all of them. RFC 5888 section 6
    /// requires all `m=` lines to have a `mid` and every tag to resolve.
    pub fn bundle_tags(&self) -> &[String] {
        // RFC 5888 section 6: any media line without a mid disables grouping.
        if self.media.iter().any(|media| media.mid().is_none()) {
            return &[];
        }

        let Some(group) = self
            .groups
            .iter()
            .find(|group| matches!(group.semantics, GroupSemantics::Bundle))
        else {
            return &[];
        };

        // RFC 5888 section 6: ignore a group with an unresolvable tag.
        if group.mid_tags.iter().any(|tag| {
            !self
                .media
                .iter()
                .any(|media| media.mid().is_some_and(|mid| mid.0 == *tag))
        }) {
            return &[];
        }

        &group.mid_tags
    }

    /// Add an `a=group:BUNDLE` tag to the first BUNDLE group, creating the
    /// group if necessary.
    pub fn add_bundle_tag(&mut self, tag: &str) {
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|group| matches!(group.semantics, GroupSemantics::Bundle))
        {
            group.mid_tags.push(tag.to_string());
        } else {
            self.groups.push(Group {
                semantics: GroupSemantics::Bundle,
                mid_tags: vec![tag.to_string()],
            });
        }
    }

    /// Index of the media line the first `a=group:BUNDLE` group starts with.
    ///
    /// That media line provides the transport of the whole group (RFC 8843
    /// section 5). `None` if there is no BUNDLE group, or if its first tag
    /// does not identify any media line.
    pub fn bundle_idx(&self) -> Option<usize> {
        let bundle = self.bundle_tags();
        if bundle.is_empty() {
            None
        } else {
            self.media
                .iter()
                .position(|media| media.mid().as_ref().is_some_and(|mid| mid.0 == bundle[0]))
        }
    }

    /// Session-level attribute without any special handling, `None` if the
    /// attribute is not present, `Some(None)` for a valueless attribute.
    pub fn attribute(&self, name: &str) -> Option<Option<&str>> {
        self.other_attributes
            .iter()
            .find(|(attribute, _)| attribute == name)
            .map(|(_, value)| value.as_deref())
    }

    /// All session-level attributes as `a-` caps fields (valueless
    /// attributes as empty strings), skipping the attributes named in `skip`.
    /// This includes typed attributes such as `a=ice-ufrag` or `a=group`, not
    /// only `other_attributes`.
    pub fn session_caps_fields(&self, skip: &[&str]) -> Vec<(String, String)> {
        self.to_sdp_types()
            .attributes
            .iter()
            .filter(|attr| !skip.contains(&attr.attribute.as_str()))
            .map(|attr| {
                (
                    format!("a-{}", attr.attribute),
                    attr.value.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Direction of the media line at `mline`: SCTP media use the session
    /// direction, RTP media their own. Panics on an out-of-range media
    /// index.
    pub fn rtp_direction_for_mline(&self, mline: usize) -> Direction {
        if self.media[mline].proto().is_sctp() {
            self.direction.unwrap_or_default()
        } else {
            self.media[mline].direction()
        }
    }

    /// Renders the session as an SDP string.
    pub fn to_sdp_string(&self) -> String {
        self.to_sdp_types().to_string()
    }

    /// Converts to a `sdp_types::Session`.
    pub fn to_sdp_types(&self) -> sdp_types::Session {
        let mut builder = sdp_types::Session::builder(self.origin.clone(), &self.name)
            .session_description_if_some(self.information.as_deref())
            .uri_if_some(self.uri.as_deref())
            .emails(self.emails.iter())
            .phones(self.phones.iter())
            .connection_if_some(self.connection.clone());
        // RFC 8866 §5.9 requires at least one t= line: sessions without
        // timing are serialized as permanent sessions
        let default_times = [sdp_types::Time::new(0, 0)];
        let times: &[sdp_types::Time] = if self.times.is_empty() {
            &default_times
        } else {
            &self.times
        };
        builder = builder
            .bandwidths(self.bandwidths.iter().cloned())
            .times(times.iter().cloned())
            .time_zones(self.time_zones.iter().cloned());
        builder = builder
            .attributes(self.groups.clone())
            .attribute_if(IceLite, self.ice_lite)
            .attribute_if_some(self.ice_ufrag.clone())
            .attribute_if_some(self.ice_pwd.clone())
            .attribute_if_some(self.ice_options.clone());
        builder = builder
            .attributes(self.fingerprints.clone())
            .attribute_if_some(self.setup.clone())
            .attributes(self.crypto.clone())
            .attributes(self.key_mgmt.clone());

        builder = builder.attribute_if_some(self.direction);
        builder = builder.attributes(self.other_attributes.iter().map(other_attribute));
        builder = builder.medias(self.media.iter().map(|media| media.to_sdp_types()));

        builder.build()
    }
}

/// Builder for [`Session`], starting from the [`Session::default`] defaults.
#[derive(Debug, Clone)]
pub struct SessionBuilder {
    inner: Session,
}

impl SessionBuilder {
    builder_fns! {
        origin: Origin,
        name: String,
        information: Option<String>,
        uri: Option<String>,
        emails: Vec<String>,
        phones: Vec<String>,
        connection: Option<sdp_types::Connection>,
        bandwidths: Vec<sdp_types::Bandwidth>,
        times: Vec<sdp_types::Time>,
        time_zones: Vec<TimeZone>,
        groups: Vec<Group>,
        ice_lite: bool,
        ice_ufrag: Option<IceUfrag>,
        ice_pwd: Option<IcePwd>,
        ice_options: Option<IceOptions>,
        fingerprints: Vec<sdp_types::Fingerprint>,
        setup: Option<sdp_types::Setup>,
        crypto: Vec<sdp_types::Crypto>,
        key_mgmt: Vec<KeyMgmt>,
        direction: Option<Direction>,
        other_attributes: Vec<(String, Option<String>)>,
        media: Vec<Media>,
    }

    pub fn build(self) -> Session {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{MEDIA_AUDIO, MEDIA_VIDEO, SESSION_HEADER, init};
    use crate::{
        attrs::{KeyMgmtExt, RtcpFb},
        caps::caps_to_media,
    };
    use sdp_types::{
        AddrType, Bandwidth, Candidate, CandidateAddress, CandidateType, Connection, ExtMap, Group,
        GroupSemantics, KeyMgmtMethod, MediaType, NetType, RemoteCandidate, Repeat, Rid,
        RidDirection, Time, TimeZone, TransportProto,
    };

    #[test]
    fn parse_audio() {
        init();
        let sdp = Session::parse(&format!("{SESSION_HEADER}{MEDIA_AUDIO}")).unwrap();
        assert_eq!(sdp.direction, None);
        assert_eq!(sdp.media.len(), 1);
        let media = &sdp.media[0];
        assert_eq!(media.media(), MediaType::Audio);
        assert_eq!(media.port(), 5004);
        assert_eq!(media.num_ports(), Some(2));
        assert_eq!(
            media.connection().cloned(),
            Some(Connection::new(
                NetType::In,
                AddrType::Ip4,
                "239.255.42.42/16"
            ))
        );
        assert_eq!(media.direction(), Direction::SendOnly);
        assert!(media.rtcp_mux());
        assert_eq!(media.formats(), [96]);
        assert_eq!(media.ptime(), Some(20));
        let rtpmap = media.rtpmaps().get(&96).unwrap();
        assert_eq!(rtpmap.encoding_name, "opus");
        assert_eq!(rtpmap.clock_rate, 48000);
        assert_eq!(rtpmap.encoding_params.as_deref(), Some("2"));
        let fmtp = media.fmtps().get(&96).unwrap();
        assert_eq!(fmtp.format_specific_params.len(), 2);
        assert_eq!(fmtp.format_specific_params[0].param, "sprop-stereo");
        assert_eq!(fmtp.format_specific_params[0].val.as_deref(), Some("1"));
        assert_eq!(
            media.rtcp_fb().cloned(),
            Some(RtcpFb {
                transport_cc: true,
                ..Default::default()
            })
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
            media
                .extmap()
                .contains(&ExtMap::builder(1, "urn:ietf:params:rtp-hdrext:sdes:mid").build())
        );
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
        assert_eq!(
            media.rids(),
            [
                Rid {
                    id: "1".to_string(),
                    direction: RidDirection::Send,
                    params: vec![],
                },
                Rid {
                    id: "2".to_string(),
                    direction: RidDirection::Recv,
                    params: vec!["apt=96".to_string()],
                },
            ]
        );
        assert!(
            media
                .other_attributes()
                .contains(&("tool".to_string(), Some("test".to_string())))
        );
        assert!(
            media
                .other_attributes()
                .contains(&("x-custom".to_string(), Some("val".to_string())))
        );
    }

    #[test]
    fn session_direction_fallback() {
        init();
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}a=sendonly\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             c=IN IP4 239.255.42.42\r\n\
             a=rtpmap:96 opus/48000/2\r\n"
        ))
        .unwrap();
        assert_eq!(sdp.direction, Some(Direction::SendOnly));
        assert_eq!(sdp.media[0].direction(), Direction::SendOnly);
    }

    #[test]
    fn session_header() {
        init();
        let sdp = Session::parse(&format!("{SESSION_HEADER}m=audio 0 RTP/AVP 96\r\n")).unwrap();
        assert_eq!(sdp.origin.username.as_deref(), Some("alice"));
        assert_eq!(sdp.origin.sess_id, "1234567890");
        assert_eq!(sdp.origin.sess_version, 0);
        assert_eq!(sdp.origin.nettype, NetType::In);
        assert_eq!(sdp.origin.addrtype, AddrType::Ip4);
        assert_eq!(sdp.origin.unicast_address, "0.0.0.0");
        assert_eq!(sdp.name, "-");
        assert_eq!(sdp.information.as_deref(), Some("session information"));
        assert_eq!(sdp.uri.as_deref(), Some("rtsp://example.com/session"));
        assert_eq!(sdp.emails, ["alice@example.com"]);
        assert_eq!(sdp.phones, ["+49 1234 56789"]);
        assert_eq!(
            sdp.connection,
            Some(Connection::new(
                NetType::In,
                AddrType::Ip4,
                "224.12.4.1/127"
            ))
        );
        assert_eq!(
            sdp.bandwidths,
            [Bandwidth {
                bwtype: "AS".to_string(),
                bandwidth: 1024,
            }]
        );
        assert_eq!(
            sdp.times,
            [Time {
                start_time: 0,
                stop_time: 0,
                repeats: vec![Repeat {
                    repeat_interval: 60,
                    active_duration: 30,
                    offsets: vec![15],
                }],
            }]
        );
        assert_eq!(
            sdp.time_zones,
            [TimeZone {
                adjustment_time: 2882880000,
                offset: -3600,
            }]
        );
        assert_eq!(
            sdp.groups,
            [Group {
                semantics: GroupSemantics::Bundle,
                mid_tags: vec!["0".to_string(), "1".to_string()],
            }]
        );
        assert_eq!(
            sdp.other_attributes,
            [("tool".to_string(), Some("session".to_string()))]
        );
        // The session-level c= line is kept separate from the media lines.
        assert_eq!(sdp.media[0].connection(), None);
        assert_eq!(Session::parse(&sdp.to_sdp_string()).unwrap(), sdp);
    }

    #[test]
    fn sdp_roundtrip() {
        init();
        let sdp = Session::parse(&format!("{SESSION_HEADER}{MEDIA_AUDIO}{MEDIA_VIDEO}")).unwrap();
        let sdp2 = Session::parse(&sdp.to_sdp_string()).unwrap();
        assert_eq!(sdp2, sdp);
    }

    #[test]
    fn invalid_sdp() {
        init();
        for media in [
            // fmtp / rtcp-fb for a payload type not in the media line.
            concat!("m=audio 0 RTP/AVP 96\r\n", "a=fmtp:97 sprop-stereo=1\r\n",),
            concat!("m=audio 0 RTP/AVP 96\r\n", "a=rtcp-fb:97 nack\r\n",),
            // Duplicated attributes for the same payload type.
            concat!(
                "m=audio 0 RTP/AVP 96\r\n",
                "a=fmtp:96 sprop-stereo=1\r\n",
                "a=fmtp:96 usedtx=1\r\n",
            ),
            // Reserved header extension ID.
            concat!("m=audio 0 RTP/AVP 96\r\n", "a=extmap:15 uri\r\n",),
            // Invalid attribute values.
            concat!("m=audio 0 RTP/AVP 96\r\n", "a=rid:1 bad\r\n",),
            // Duplicated or conflicting media-level attributes.
            concat!(
                "m=audio 0 RTP/AVP 96\r\n",
                "a=ptime:20\r\n",
                "a=ptime:10\r\n",
            ),
            // framesize for an unknown payload type or without dimensions.
            concat!("m=audio 0 RTP/AVP 96\r\n", "a=framesize:97 1280x720\r\n",),
            concat!("m=audio 0 RTP/AVP 96\r\n", "a=framesize:96\r\n",),
        ] {
            let sdp = format!("{SESSION_HEADER}{media}");
            assert!(Session::parse(&sdp).is_err(), "expected error for\n{sdp}");
        }
    }

    fn parse_media_error(media: &str) -> MediaError {
        match Session::parse(&format!("{SESSION_HEADER}{media}")).unwrap_err() {
            ParseError::Media { source, .. } => source,
            err => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    fn typed_errors() {
        use crate::AttributeParseError;

        init();
        assert!(matches!(
            Session::parse("v=1\r\n"),
            Err(ParseError::Syntax(_))
        ));
        assert!(matches!(
            Session::parse(&format!("{SESSION_HEADER}a=sendonly\r\na=inactive\r\n")),
            Err(ParseError::SessionAttribute(
                AttributeParseError::Duplicate(_)
            ))
        ));
        assert!(matches!(
            parse_media_error("m=image 0 RTP/AVP 96\r\n"),
            MediaError::UnsupportedMediaType(media) if media == "image"
        ));
        assert!(matches!(
            parse_media_error("m=audio 0 FOO 96\r\n"),
            MediaError::UnsupportedProtocol(_)
        ));
        // Unknown profiles containing a '/' are kept as Other.
        assert!(
            Session::parse(&format!(
                "{SESSION_HEADER}m=audio 0 FOO/BAR 96\r\na=rtpmap:96 opus/48000/2\r\n"
            ))
            .is_ok()
        );
        assert!(matches!(
            parse_media_error("m=audio 0 RTP/AVP 96\r\na=rtcp-fb:x nack\r\n"),
            MediaError::InvalidPayloadType(_)
        ));
        assert!(matches!(
            parse_media_error(concat!(
                "m=audio 0 RTP/AVP 96\r\n",
                "a=rtpmap:97 opus/48000/2\r\n"
            )),
            MediaError::UnknownPayloadType { .. }
        ));
        assert!(matches!(
            parse_media_error(concat!(
                "m=audio 0 RTP/AVP 96\r\n",
                "a=rtpmap:96 opus/48000/2\r\n",
                "a=rtpmap:96 opus/48000/2\r\n"
            )),
            MediaError::DuplicateForPayloadType { .. }
        ));
        assert!(matches!(
            parse_media_error(concat!("m=audio 0 RTP/AVP 96\r\n", "a=ptime:abc\r\n",)),
            MediaError::NumericAttribute { .. }
        ));
        assert!(matches!(
            parse_media_error(concat!(
                "m=audio 0 RTP/AVP 96\r\n",
                "a=sendonly\r\n",
                "a=inactive\r\n"
            )),
            MediaError::Attribute(AttributeParseError::Duplicate(_))
        ));
        // RFC 8866 section 5.7 allows an FQDN as session connection address.
        assert!(Session::parse("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nc=IN IP4 foo\r\n").is_ok());
        assert_eq!(
            Session::parse(&format!("{SESSION_HEADER}m=audio 0 FOO 96\r\n"))
                .unwrap_err()
                .to_string(),
            "invalid media 0: unsupported transport protocol 'FOO'"
        );
    }

    #[test]
    fn parse_skip_invalid() {
        init();
        let session = format!(
            "{SESSION_HEADER}\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 opus/48000/2\r\n\
             m=image 0 RTP/AVP 96\r\n\
             m=video 0 udp 97\r\n\
             a=rtpmap:97 H264/90000\r\n"
        );
        let (sdp, errors) = Session::parse_skip_invalid(&session).unwrap();
        assert_eq!(sdp.media.len(), 1);
        assert_eq!(sdp.media[0].media(), MediaType::Audio);
        assert_eq!(
            errors.iter().map(|(mline, _)| *mline).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(matches!(errors[0].1, MediaError::UnsupportedMediaType(_)));
        assert!(matches!(errors[1].1, MediaError::UnsupportedProtocol(_)));

        // The strict parse fails on the first invalid media line.
        match Session::parse(&session).unwrap_err() {
            ParseError::Media { mline, source } => {
                assert_eq!(mline, 1);
                assert!(matches!(source, MediaError::UnsupportedMediaType(_)));
            }
            err => panic!("unexpected error: {err}"),
        }
    }
    #[test]
    fn media_model() {
        init();
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}\
             m=video 0 RTP/SAVPF 96\r\n\
             a=control:stream=0\r\n\
             a=mid:0\r\n\
             a=bundle-only\r\n\
             a=rtcp-mux-only\r\n\
             a=rtcp-rsize\r\n\
             a=rtpmap:96 H264/90000\r\n"
        ))
        .unwrap();
        let media = &sdp.media[0];
        assert_eq!(media.proto(), &TransportProto::RtpSavpf);
        assert_eq!(media.control(), Some("stream=0"));
        assert_eq!(media.mid().cloned(), Some(sdp_types::Mid("0".into())));
        assert!(media.bundle_only());
        assert!(media.rtcp_mux_only());
        assert!(media.rtcp_rsize());
        assert!(media.other_attributes().is_empty());
        assert_eq!(Session::parse(&sdp.to_sdp_string()).unwrap(), sdp);
    }

    #[test]
    fn ice_dtls() {
        init();
        let sdp = Session::parse(concat!(
            "v=0\r\n",
            "o=bob 2890844526 2890844526 IN IP4 host.example.com\r\n",
            "s=-\r\n",
            "t=0 0\r\n",
            "a=ice-lite\r\n",
            "a=ice-ufrag:session\r\n",
            "a=ice-pwd:sessionpwdsessionpwd12\r\n",
            "a=ice-options:trickle renomination\r\n",
            "a=fingerprint:sha-256 DA:74:05:4A:06:BF:9A:86:59:2A:37:26:8E:3C:82:4C:D9:6D:67:38:74:8B:0B:9C:D0:57:41:77:C5:A5:7B:1F\r\n",
            "a=setup:actpass\r\n",
            "m=audio 9 UDP/TLS/RTP/SAVPF 96\r\n",
            "c=IN IP4 0.0.0.0\r\n",
            "a=rtpmap:96 opus/48000/2\r\n",
            "a=ice-ufrag:media\r\n",
            "a=ice-options:renomination\r\n",
            "a=fingerprint:sha-1 6E:05:3B:51:76:23:FC:BD:4D:99:6C:F9:E4:9E:A5:A3:FD:79:AA:09\r\n",
            "a=setup:passive\r\n",
            "a=candidate:1 1 udp 2113937151 192.168.1.1 50000 typ host generation 0\r\n",
            "a=candidate:14 1 udp 16777215 1.2.3.4 40000 typ srflx raddr 192.168.1.1 rport 50000\r\n",
            "a=end-of-candidates\r\n",
            "a=remote-candidates:1 1.2.3.4 5678 2 5.6.7.8 9012\r\n",
        ))
        .unwrap();

        assert!(sdp.ice_lite);
        assert_eq!(sdp.ice_ufrag, Some(sdp_types::IceUfrag("session".into())));
        assert_eq!(
            sdp.ice_pwd,
            Some(sdp_types::IcePwd("sessionpwdsessionpwd12".into()))
        );
        assert_eq!(
            sdp.ice_options,
            Some(sdp_types::IceOptions(vec![
                "trickle".into(),
                "renomination".into()
            ]))
        );
        assert_eq!(sdp.fingerprints.len(), 1);
        assert_eq!(sdp.fingerprints[0].hash_func.as_str(), "sha-256");
        assert_eq!(sdp.fingerprints[0].fingerprint.len(), 32);
        assert_eq!(sdp.setup, Some(sdp_types::Setup::ActPass));
        assert!(sdp.other_attributes.is_empty());

        let media = &sdp.media[0];
        assert_eq!(
            media.ice_ufrag().cloned(),
            Some(sdp_types::IceUfrag("media".into()))
        );
        assert_eq!(media.ice_pwd(), None);
        assert_eq!(
            media.ice_options().cloned(),
            Some(sdp_types::IceOptions(vec!["renomination".into()]))
        );
        assert_eq!(media.fingerprints()[0].hash_func.as_str(), "sha-1");
        assert_eq!(media.setup().cloned(), Some(sdp_types::Setup::Passive));
        assert_eq!(media.candidates().len(), 2);
        assert_eq!(
            media.candidates()[0],
            Candidate {
                foundation: "1".into(),
                component_id: 1,
                transport: "udp".into(),
                priority: 2113937151,
                address: CandidateAddress::IpAddr("192.168.1.1".parse().unwrap()),
                port: 50000,
                typ: CandidateType::Host,
                rel_addr: None,
                rel_port: None,
                extensions: vec![("generation".into(), "0".into())],
            }
        );
        assert_eq!(
            media.candidates()[1].rel_addr,
            Some("192.168.1.1".parse().unwrap())
        );
        assert_eq!(media.candidates()[1].rel_port, Some(50000));
        assert!(media.end_of_candidates());
        assert_eq!(
            media.remote_candidates(),
            [
                RemoteCandidate::new(1, "1.2.3.4".parse::<std::net::IpAddr>().unwrap(), 5678),
                RemoteCandidate::new(2, "5.6.7.8".parse::<std::net::IpAddr>().unwrap(), 9012),
            ]
        );
        assert!(media.other_attributes().is_empty());

        let rendered = sdp.to_sdp_string();
        assert!(rendered.contains("a=candidate:14 1 udp 16777215 1.2.3.4 40000 typ srflx raddr 192.168.1.1 rport 50000\r\n"), "{rendered}");
        assert!(
            rendered.contains("a=remote-candidates:1 1.2.3.4 5678 2 5.6.7.8 9012\r\n"),
            "{rendered}"
        );
        assert_eq!(Session::parse(&rendered).unwrap(), sdp);

        match Session::parse(concat!(
            "v=0\r\n",
            "o=- 1 1 IN IP4 0.0.0.0\r\n",
            "s=-\r\n",
            "t=0 0\r\n",
            "a=setup:bogus\r\n"
        ))
        .unwrap_err()
        {
            ParseError::AttributeValue { ref name, .. } => assert_eq!(name, "setup"),
            err => panic!("unexpected error: {err}"),
        }
        assert!(matches!(
            Session::parse(&format!(
                "{SESSION_HEADER}m=audio 0 RTP/AVP 96\r\na=candidate:1 1 udp\r\n"
            ))
            .unwrap_err(),
            ParseError::Media { .. }
        ));
    }

    #[test]
    fn remote_candidates_one_per_line() {
        init();
        // The RFC 8839 section 5.2 example uses one single-entry attribute
        // per component, parsing accumulates them into one list.
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}{MEDIA_AUDIO}\
             a=remote-candidates:1 192.0.2.3 45664\r\n\
             a=remote-candidates:2 192.0.2.3 45665\r\n"
        ))
        .unwrap();
        assert_eq!(
            sdp.media()[0].remote_candidates(),
            [
                RemoteCandidate::new(1, "192.0.2.3".parse::<std::net::IpAddr>().unwrap(), 45664),
                RemoteCandidate::new(2, "192.0.2.3".parse::<std::net::IpAddr>().unwrap(), 45665),
            ]
        );

        // Re-emitted as a single multi-entry attribute.
        let rendered = sdp.to_sdp_string();
        assert!(
            rendered.contains("a=remote-candidates:1 192.0.2.3 45664 2 192.0.2.3 45665\r\n"),
            "{rendered}"
        );
        assert_eq!(Session::parse(&rendered).unwrap(), sdp);
    }

    #[test]
    fn datachannel() {
        init();
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
a=mid:data\r\n\
a=sctp-port:5000\r\n\
a=max-message-size:262144\r\n"
        ))
        .unwrap();
        let media = &sdp.media[0];
        assert_eq!(media.proto(), &TransportProto::UdpDtlsSctp);
        assert!(media.formats().is_empty());
        assert_eq!(media.fmt_raw(), Some("webrtc-datachannel"));
        assert_eq!(media.mid().cloned(), Some(sdp_types::Mid("data".into())));
        assert_eq!(media.sctp_port().cloned(), Some(sdp_types::SctpPort(5000)));
        assert_eq!(
            media.max_message_size().cloned(),
            Some(sdp_types::MaxMessageSize(262144))
        );
        assert!(media.other_attributes().is_empty());
        assert_eq!(Session::parse(&sdp.to_sdp_string()).unwrap(), sdp);
    }

    #[test]
    fn session_header_only() {
        init();
        let sdp =
            Session::parse("v=0\r\no=- 3498989708992231200 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n")
                .unwrap();
        assert_eq!(sdp.origin.sess_id, "3498989708992231200");
        assert_eq!(sdp.origin.username, None);
        assert!(sdp.media.is_empty());
    }

    #[test]
    fn rtp_profiles() {
        init();
        // RTP profiles are stored and rendered back unchanged.
        for profile in [
            "RTP/AVP",
            "RTP/AVPF",
            "RTP/SAVP",
            "RTP/SAVPF",
            "UDP/TLS/RTP/SAVP",
            "UDP/TLS/RTP/SAVPF",
            "TCP/DTLS/RTP/SAVP",
            "TCP/DTLS/RTP/SAVPF",
            "TCP/RTP/AVP",
        ] {
            let sdp =
                Session::parse(&format!("{SESSION_HEADER}m=audio 0 {profile} 96\r\n")).unwrap();
            let rendered = sdp.to_sdp_string();
            assert!(
                rendered.contains(&format!("m=audio 0 {profile} 96")),
                "{profile}: {rendered}"
            );
            assert_eq!(Session::parse(&rendered).unwrap(), sdp, "{profile}");
        }

        // Media produced from caps render as RTP/AVP without feedback and
        // as RTP/AVPF once feedback is present.
        for (fields, avpf) in [
            ("payload=96,clock-rate=48000", false),
            ("payload=96,clock-rate=48000,rtcp-fb-nack=true", true),
        ] {
            let caps = format!("application/x-rtp,media=audio,encoding-name=OPUS,{fields}")
                .parse::<gst::Caps>()
                .unwrap();
            let media = caps_to_media(&caps).unwrap();
            assert_eq!(
                media.proto(),
                &if avpf {
                    TransportProto::RtpAvpf
                } else {
                    TransportProto::RtpAvp
                }
            );
            let rendered = Session {
                media: vec![media],
                ..Default::default()
            }
            .to_sdp_string();
            assert_eq!(rendered.contains("RTP/AVPF"), avpf, "{rendered}");
        }
    }

    #[test]
    fn crypto_key_mgmt() {
        init();
        let key = "WnFW7GRz0pWUCge8Ff93kiBhYc1yLLoHLOoyQnBsPrhU";
        let sdp = Session::parse(&format!(
            concat!(
                "{SESSION_HEADER}",
                "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{key}|2^31|1:4\r\n",
                "a=key-mgmt:mikey AQIDBA==\r\n",
                "a=key-mgmt:internal\r\n",
                "{MEDIA_AUDIO}",
                "a=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:{key}\r\n",
                "a=key-mgmt:rawurl AQIDBAU=\r\n",
            ),
            SESSION_HEADER = SESSION_HEADER,
            MEDIA_AUDIO = MEDIA_AUDIO,
            key = key,
        ))
        .unwrap();

        assert_eq!(
            sdp.crypto,
            vec![
                format!("1 AES_CM_128_HMAC_SHA1_80 inline:{key}|2^31|1:4")
                    .parse()
                    .unwrap()
            ]
        );
        assert_eq!(sdp.key_mgmt[0].method, KeyMgmtMethod::Mikey);
        assert_eq!(sdp.key_mgmt[0].data.as_deref(), Some("AQIDBA=="));
        assert!(sdp.key_mgmt[0].mikey().is_err(), "not a MIKEY message");
        assert_eq!(
            sdp.key_mgmt[1],
            KeyMgmt {
                method: KeyMgmtMethod::Other("internal".into()),
                data: None,
            }
        );

        let media = &sdp.media[0];
        assert_eq!(
            media.crypto(),
            vec![
                format!("2 AES_CM_128_HMAC_SHA1_32 inline:{key}")
                    .parse()
                    .unwrap()
            ]
        );
        assert_eq!(
            media.key_mgmt()[0].method,
            KeyMgmtMethod::Other("rawurl".into())
        );

        // The typed attributes must not leak into the catch-all buckets.
        for (name, _) in sdp.other_attributes.iter().chain(media.other_attributes()) {
            assert!(name != "crypto" && name != "key-mgmt", "{name}");
        }

        assert_eq!(Session::parse(&sdp.to_sdp_string()).unwrap(), sdp);

        // sdp-types rejects the master key parameter form (RFC 4568 section 6.2).
        let err = Session::parse(&format!(
            concat!(
                "{SESSION_HEADER}",
                "a=crypto:1 AES_CM_128_HMAC_SHA1_80 master key 1 {key}\r\n"
            ),
            SESSION_HEADER = SESSION_HEADER,
            key = key,
        ))
        .unwrap_err();
        assert!(matches!(err, ParseError::AttributeValue { ref name, .. } if name == "crypto"));

        let err = Session::parse(&format!(
            concat!("{SESSION_HEADER}", "a=key-mgmt:\r\n"),
            SESSION_HEADER = SESSION_HEADER
        ))
        .unwrap_err();
        assert!(
            matches!(err, ParseError::AttributeValue { ref name, .. } if name == "key-mgmt")
                || matches!(err, ParseError::SessionAttribute(_)),
            "{err:?}"
        );
    }

    #[test]
    fn effective_key_attributes() {
        init();

        // Media-level attributes take precedence over session-level ones.
        let sdp = Session::parse(&format!(
            concat!(
                "{SESSION_HEADER}a=key-mgmt:internal\r\n",
                "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:AAAA\r\n",
                "{MEDIA_AUDIO}a=key-mgmt:mikey AAAAAAAAAw==\r\n",
                "a=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:AAAA\r\n",
            ),
            SESSION_HEADER = SESSION_HEADER,
            MEDIA_AUDIO = MEDIA_AUDIO,
        ))
        .unwrap();
        assert_eq!(
            sdp.effective_key_mgmt(0).unwrap()[0].method,
            KeyMgmtMethod::Mikey
        );
        assert_eq!(sdp.effective_crypto(0).unwrap()[0].tag, 2);

        // Session-level ones apply when the media line has none, and media
        // lines without any keying produce empty slices.
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}a=key-mgmt:internal\r\n{MEDIA_AUDIO}"
        ))
        .unwrap();
        assert_eq!(
            sdp.effective_key_mgmt(0).unwrap()[0].method,
            KeyMgmtMethod::Other("internal".into())
        );
        assert!(sdp.effective_crypto(0).unwrap().is_empty());
        let sdp = Session::parse(&format!("{SESSION_HEADER}{MEDIA_AUDIO}")).unwrap();
        assert!(sdp.effective_key_mgmt(0).unwrap().is_empty());

        // Unknown media lines return None.
        assert!(sdp.effective_key_mgmt(1).is_none());
        assert!(sdp.effective_crypto(1).is_none());
    }

    #[test]
    fn bundle_helpers() {
        init();
        let mut audio = Media::new(MediaType::Audio);
        audio.set_mid(Some(sdp_types::Mid("a".to_string())));
        let mut video = Media::new(MediaType::Video);
        video.set_mid(Some(sdp_types::Mid("v".to_string())));
        let mut sdp = Session::builder().media(vec![audio, video]).build();
        assert!(sdp.bundle_tags().is_empty());
        assert_eq!(sdp.bundle_idx(), None);

        sdp.add_bundle_tag("v");
        sdp.add_bundle_tag("a");
        assert_eq!(sdp.groups.len(), 1);
        assert_eq!(sdp.groups[0].semantics, GroupSemantics::Bundle);
        assert_eq!(sdp.bundle_tags().to_vec(), ["v", "a"]);
        // The index of the first tag, not of the first media line.
        assert_eq!(sdp.bundle_idx(), Some(1));
    }

    #[test]
    fn bundle_ignored_for_unresolvable_tag() {
        init();
        // RFC 5888 section 6: ignore a group with an unresolvable tag.
        let sdp = Session::parse(&format!(
            "{HEADER}{}",
            concat!(
                "a=group:BUNDLE a missing\r\n",
                "m=audio 5004 RTP/AVP 96\r\n",
                "c=IN IP4 239.255.42.42\r\n",
                "a=mid:a\r\n",
            )
        ))
        .unwrap();
        assert_eq!(sdp.groups.len(), 1);
        assert!(sdp.bundle_tags().is_empty());
        assert_eq!(sdp.bundle_idx(), None);
    }

    // Session-level part without the `a=group` line of `SESSION_HEADER`.
    const HEADER: &str = concat!(
        "v=0\r\n",
        "o=alice 1234567890 0 IN IP4 0.0.0.0\r\n",
        "s=-\r\n",
        "t=0 0\r\n",
    );

    #[test]
    fn bundle_group_case_insensitive() {
        init();
        let sdp = Session::parse(&format!(
            "{HEADER}{}",
            concat!(
                "a=group:bundle v a\r\n",
                "m=audio 5004 RTP/AVP 96\r\nc=IN IP4 239.255.42.42\r\na=mid:a\r\n",
                "m=video 7004 RTP/AVP 96\r\nc=IN IP4 239.255.42.43\r\na=mid:v\r\n",
            )
        ))
        .unwrap();
        assert_eq!(sdp.bundle_tags().to_vec(), ["v", "a"]);
        assert_eq!(sdp.bundle_idx(), Some(1));
        // The registered spelling is what is written back out.
        assert!(sdp.to_sdp_string().contains("a=group:BUNDLE v a\r\n"));
    }

    #[test]
    fn group_attributes() {
        init();
        let sdp = Session::parse(&format!(
            "{HEADER}{}{MEDIA_VIDEO}",
            concat!(
                "a=group:LS 1  2 \r\n",
                "a=group:anat\t3\r\n",
                "a=group:BUNDLE 0 1\r\n",
                "a=group:BUNDLE 2 3\r\n",
            )
        ))
        .unwrap();
        assert_eq!(sdp.groups.len(), 4);
        // Whitespace is normalized to single spaces, so no empty tags.
        assert_eq!(sdp.groups[0].semantics, GroupSemantics::LS);
        assert_eq!(sdp.groups[0].mid_tags, ["1", "2"]);
        assert_eq!(sdp.groups[1].semantics, GroupSemantics::ANAT);
        assert_eq!(sdp.groups[1].mid_tags, ["3"]);
        // The helpers ignore the groups because the media line has no mid
        // (RFC 5888 section 6).
        assert!(sdp.bundle_tags().is_empty());
        // Groups survive a round-trip.
        assert_eq!(Session::parse(&sdp.to_sdp_string()).unwrap(), sdp);
    }

    #[test]
    fn group_without_tags() {
        init();
        // A BUNDLE group with no identification-tag describes no media line.
        assert!(matches!(
            Session::parse(&format!("{HEADER}a=group:BUNDLE\r\n{MEDIA_VIDEO}")),
            Err(ParseError::Group { .. })
        ));
        // A group that is not acted upon must not make the announcement fail.
        assert!(Session::parse(&format!("{HEADER}a=group:LS\r\n{MEDIA_VIDEO}")).is_ok());
    }

    #[test]
    fn session_attributes() {
        init();
        let sdp = Session::builder()
            .ice_ufrag(Some(IceUfrag("abcd".to_string())))
            .other_attributes(vec![
                ("control".to_string(), Some("rtsp://example".to_string())),
                ("x-flag".to_string(), None),
            ])
            .build();
        assert_eq!(sdp.attribute("control"), Some(Some("rtsp://example")));
        assert_eq!(sdp.attribute("x-flag"), Some(None));
        assert_eq!(sdp.attribute("missing"), None);
        assert_eq!(
            sdp.session_caps_fields(&["control"]),
            vec![
                ("a-ice-ufrag".to_string(), "abcd".to_string()),
                ("a-x-flag".to_string(), String::new()),
            ]
        );
        assert_eq!(
            sdp.session_caps_fields(&[]),
            vec![
                ("a-ice-ufrag".to_string(), "abcd".to_string()),
                ("a-control".to_string(), "rtsp://example".to_string()),
                ("a-x-flag".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn media_directions() {
        init();
        let mut audio = Media::new(MediaType::Audio);
        audio.set_direction(Direction::SendOnly);
        let mut data = Media::new(MediaType::Application);
        data.set_proto(TransportProto::UdpDtlsSctp);
        data.set_direction(Direction::Inactive);
        let mut sdp = Session::builder().media(vec![audio, data]).build();
        // RTP media use their own direction, SCTP media the session one.
        assert_eq!(sdp.rtp_direction_for_mline(0), Direction::SendOnly);
        assert_eq!(sdp.rtp_direction_for_mline(1), Direction::SendRecv);
        sdp.direction = Some(Direction::RecvOnly);
        assert_eq!(sdp.rtp_direction_for_mline(1), Direction::RecvOnly);
    }
}
