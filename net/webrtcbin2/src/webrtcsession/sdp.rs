// SPDX-License-Identifier: MPL-2.0

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::sync::LazyLock;
use std::{collections::BTreeSet, fmt::Display};

use librice::candidate::{Candidate, ParseCandidateError};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc2sdp",
        gst::DebugColorFlags::empty(),
        Some("WebRTC2Sdp"),
    )
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebRTCSdpType {
    Offer,
    Answer,
    PrAnswer,
    Rollback,
}

impl Display for WebRTCSdpType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Offer => "offer",
            Self::Answer => "answer",
            Self::PrAnswer => "pr-answer",
            Self::Rollback => "rollback",
        };
        f.write_str(s)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseWebRTCSdpTypeError {
    #[error("Unknown value")]
    UnknownValue,
}

impl FromStr for WebRTCSdpType {
    type Err = ParseWebRTCSdpTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "offer" => Ok(Self::Offer),
            "answer" => Ok(Self::Answer),
            "pr-answer" => Ok(Self::PrAnswer),
            "rollback" => Ok(Self::Rollback),
            _ => Err(ParseWebRTCSdpTypeError::UnknownValue),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebRTCSdp {
    pub typ: WebRTCSdpType,
    pub id: String,
    pub ice_lite: bool,
    pub ice_ufrag: Option<String>,
    pub ice_pwd: Option<String>,
    pub fingerprints: Vec<Fingerprint>,
    pub setup: Option<DtlsSetup>,
    pub direction: Option<Direction>,
    pub media: Vec<WebRTCSdpMedia>,
}

static VALID_ICE_ALPHABET: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/";

static VALID_SDP_TOKEN_ALPHABET: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!#$%&'*+-.^_`";

impl WebRTCSdp {
    pub fn parse(typ: WebRTCSdpType, session: &str) -> Result<Self, ParseWebRTCSdpError> {
        let session = sdp_types::Session::parse(session.as_bytes())?;

        let mut ice_ufrag = None;
        let mut ice_pwd = None;
        let mut ice_lite = false;
        let mut fingerprints = vec![];
        let mut setup = None;
        let mut direction = None;
        // TODO: bwtype and bandwidth
        for attr in session.attributes.iter() {
            match attr.attribute.as_str() {
                "ice-ufrag" => {
                    ice_ufrag = Some(parse_unique_attribute_with_value(
                        None,
                        "ice-ufrag",
                        &attr.value,
                        &ice_ufrag,
                        |val| parse_ice_attribute(None, "ice-ufrag", val, 4..=256),
                    )?)
                }
                "ice-pwd" => {
                    ice_pwd = Some(parse_unique_attribute_with_value(
                        None,
                        "ice-pwd",
                        &attr.value,
                        &ice_pwd,
                        |val| parse_ice_attribute(None, "ice-pwd", val, 22..=256),
                    )?)
                }
                "ice-lite" => ice_lite = true,
                "fingerprint" => fingerprints.push(
                    attr.value
                        .clone()
                        .and_then(|val| parse_fingerprint(&val))
                        .ok_or_else(|| {
                            ParseWebRTCSdpError::InvalidAttribute("fingerprint".to_string())
                        })?,
                ),
                "setup" => {
                    setup = Some(parse_unique_attribute_with_value(
                        None,
                        "setup",
                        &attr.value,
                        &setup,
                        parse_setup,
                    )?)
                }
                "sendrecv" => {
                    direction = Some(parse_unique_attribute_no_value(
                        None,
                        "sendrecv",
                        &attr.value,
                        &direction,
                        Direction::SendRecv,
                    )?)
                }
                "sendonly" => {
                    direction = Some(parse_unique_attribute_no_value(
                        None,
                        "sendonly",
                        &attr.value,
                        &direction,
                        Direction::SendOnly,
                    )?)
                }
                "recvonly" => {
                    direction = Some(parse_unique_attribute_no_value(
                        None,
                        "recvonly",
                        &attr.value,
                        &direction,
                        Direction::RecvOnly,
                    )?)
                }
                "inactive" => {
                    direction = Some(parse_unique_attribute_no_value(
                        None,
                        "inactive",
                        &attr.value,
                        &direction,
                        Direction::Inactive,
                    )?)
                }
                // TODO: group, tls-id, identity, extmap, ice-options
                key => gst::fixme!(CAT, "unknown session attribute {key}"),
            }
        }

        let mut ret = Self {
            typ,
            id: session.origin.sess_id,
            ice_lite,
            ice_ufrag,
            ice_pwd,
            fingerprints,
            setup,
            direction,
            media: Vec::with_capacity(session.medias.len()),
        };

        for (idx, media) in session.medias.iter().enumerate() {
            let media = WebRTCSdpMedia::from_sdp_types(&ret, idx as u32, media)?;
            // FIXME: bundle
            if media.ice_ufrag.is_none() && ret.ice_ufrag.is_none() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    idx as u32,
                    "ice-ufrag".to_string(),
                ));
            }
            if media.ice_pwd.is_none() && ret.ice_pwd.is_none() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    idx as u32,
                    "ice-pwd".to_string(),
                ));
            }
            if media.setup.is_none() && ret.setup.is_none() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    idx as u32,
                    "setup".to_string(),
                ));
            }
            if media.fingerprints.is_empty() && ret.fingerprints.is_empty() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    idx as u32,
                    "fingerprint".to_string(),
                ));
            }
            ret.media.push(media);
        }

        Ok(ret)
    }

    pub fn to_sdp_string(&self) -> String {
        let mut session = sdp_types::Session {
            origin: sdp_types::Origin {
                username: Some("-".to_string()),
                sess_id: self.id.clone(),
                sess_version: 0,
                nettype: "IN".to_owned(),
                addrtype: "IP4".to_owned(),
                unicast_address: "0.0.0.0".to_owned(),
            },
            session_name: "-".to_owned(),
            session_description: None,
            uri: None,
            emails: vec![],
            phones: vec![],
            connection: None,
            bandwidths: vec![],
            times: vec![sdp_types::Time {
                start_time: 0,
                stop_time: 0,
                repeats: vec![],
            }],
            time_zones: vec![],
            key: None,
            attributes: vec![sdp_types::Attribute {
                attribute: "ice-options".to_owned(),
                value: Some("trickle".to_string()),
            }],
            medias: self
                .media
                .iter()
                .map(|media| media.to_sdp_types())
                .collect(),
        };

        if self.ice_lite {
            session.attributes.push(sdp_types::Attribute {
                attribute: "ice-lite".to_owned(),
                value: None,
            });
        }
        if let Some(ufrag) = self.ice_ufrag.clone() {
            session.attributes.push(sdp_types::Attribute {
                attribute: "ice-ufrag".to_owned(),
                value: Some(ufrag),
            });
        }
        if let Some(pwd) = self.ice_pwd.clone() {
            session.attributes.push(sdp_types::Attribute {
                attribute: "ice-pwd".to_owned(),
                value: Some(pwd),
            });
        }
        for fingerprint in self.fingerprints.iter() {
            session.attributes.push(sdp_types::Attribute {
                attribute: "fingerprint".to_owned(),
                value: Some(fingerprint.to_sdp_attribute_value()),
            });
        }

        let mut ret = vec![];
        session.write(&mut ret).unwrap();
        String::from_utf8(ret).unwrap()
    }

    pub fn rtp_direction_for_mline(&self, mline: usize) -> Direction {
        let MediaSpecifics::Rtp(rtp) = &self.media[mline].specifics else {
            return self.direction.unwrap_or_default();
        };
        rtp.direction
    }
}

static KNOWN_MEDIA_ATTRIBUTES: [&str; 8] = [
    "fingerprint",
    "ice-ufrag",
    "ice-pwd",
    "candidate",
    "remote-candidates",
    "end-of-candidates",
    "setup",
    "mid",
];

static KNOWN_RTP_MEDIA_ATTRIBUTES: [&str; 11] = [
    "sendrecv",
    "sendonly",
    "recvonly",
    "inactive",
    "rtcp-mux",
    "rtcp-mux-only",
    "rtcp-rsize",
    "rtcp",
    "rtcp-fb",
    "rtpmap",
    "fmtp",
];

static KNOWN_SCTP_MEDIA_ATTRIBUTES: [&str; 1] = ["sctp-port"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRTCSdpMedia {
    pub media: MediaType,
    pub port: u16,
    pub ice_ufrag: Option<String>,
    pub ice_pwd: Option<String>,
    pub candidates: Vec<Candidate>,
    pub end_of_candidates: bool,
    pub setup: Option<DtlsSetup>,
    pub mid: Option<String>,
    pub bundle_only: bool,
    pub fingerprints: Vec<Fingerprint>,
    pub specifics: MediaSpecifics,
}

impl WebRTCSdpMedia {
    pub fn new_rtp(media: MediaType) -> Self {
        Self {
            media,
            port: 9,
            ice_ufrag: None,
            ice_pwd: None,
            candidates: vec![],
            end_of_candidates: false,
            setup: None,
            mid: None,
            bundle_only: false,
            fingerprints: vec![],
            specifics: MediaSpecifics::Rtp(RtpMedia {
                direction: Direction::SendRecv,
                rtcp_mux: true,
                rtcp_mux_only: true,
                rtcp_rsize: false,
                rtcp_fb: None,
                extmap: Default::default(),
                formats: vec![],
                rtpmaps: Default::default(),
                rtcp_fbs: Default::default(),
                fmtps: Default::default(),
            }),
        }
    }

    fn from_sdp_types(
        session: &WebRTCSdp,
        mline: u32,
        media: &sdp_types::Media,
    ) -> Result<Self, ParseWebRTCSdpError> {
        let media_type = parse_media_type(&media.media)?;
        let is_rtp = [MediaType::Audio, MediaType::Video].contains(&media_type)
            && is_valid_rtp_profile(&media.proto);
        let is_sctp = [MediaType::Application].contains(&media_type)
            && is_valid_datachannel_profile(&media.proto);

        let mut ice_ufrag = None;
        let mut ice_pwd = None;
        let mut candidates = vec![];
        let mut end_of_candidates = false;
        let mut setup = None;
        let mut mid = None;
        let mut bundle_only = None;
        let mut fingerprints = vec![];
        for attr in media.attributes.iter() {
            match attr.attribute.as_str() {
                "ice-ufrag" => {
                    ice_ufrag = Some(parse_unique_attribute_with_value(
                        Some(mline),
                        "ice-ufrag",
                        &attr.value,
                        &ice_ufrag,
                        |val| parse_ice_attribute(Some(mline), "ice-ufrag", val, 4..=256),
                    )?)
                }
                "ice-pwd" => {
                    ice_pwd = Some(parse_unique_attribute_with_value(
                        Some(mline),
                        "ice-pwd",
                        &attr.value,
                        &ice_pwd,
                        |val| parse_ice_attribute(Some(mline), "ice-pwd", val, 22..=256),
                    )?)
                }
                "candidate" => {
                    let candidate = Candidate::from_sdp_string(&format!(
                        "a=candidate:{}",
                        attr.value.clone().ok_or(
                            ParseWebRTCSdpError::InvalidCandidate(ParseCandidateError::Malformed,)
                                .with_mline(Some(mline)),
                        )?
                    ))?;
                    candidates.push(candidate);
                }
                // remote-candidate attributes are parsed but unused
                "remote-candidates" => {
                    let _candidate = Candidate::from_sdp_string(
                        attr.value
                            .clone()
                            .ok_or(
                                ParseWebRTCSdpError::InvalidCandidate(
                                    ParseCandidateError::Malformed,
                                )
                                .with_mline(Some(mline)),
                            )?
                            .as_str(),
                    )
                    .map_err(|e| ParseWebRTCSdpError::InvalidMediaCandidate(mline, e))?;
                }
                "end-of-candidates" => {
                    if attr.value.is_some() {
                        return Err(ParseWebRTCSdpError::InvalidAttribute(
                            "end-of_candidates".to_string(),
                        )
                        .with_mline(Some(mline)));
                    } else {
                        end_of_candidates = true
                    }
                }
                "setup" => {
                    setup = Some(parse_unique_attribute_with_value(
                        None,
                        "setup",
                        &attr.value,
                        &setup,
                        parse_setup,
                    )?)
                }
                "mid" => {
                    mid = Some(parse_unique_attribute_with_value(
                        Some(mline),
                        "mid",
                        &attr.value,
                        &mid,
                        |val| {
                            if !val.chars().all(|c| VALID_SDP_TOKEN_ALPHABET.contains(c)) {
                                Err(ParseWebRTCSdpError::InvalidAttribute("mid".to_string()))
                            } else {
                                Ok(val.to_string())
                            }
                        },
                    )?)
                }
                "bundle-only" => {
                    bundle_only = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "bundle-only",
                        &attr.value,
                        &bundle_only,
                        true,
                    )?)
                }
                "fingerprint" => fingerprints.push(
                    attr.value
                        .clone()
                        .and_then(|val| parse_fingerprint(&val))
                        .ok_or_else(|| {
                            ParseWebRTCSdpError::InvalidAttribute("fingerprint".to_string())
                        })?,
                ),
                key if !KNOWN_RTP_MEDIA_ATTRIBUTES.contains(&key)
                    && !KNOWN_SCTP_MEDIA_ATTRIBUTES.contains(&key) =>
                {
                    gst::fixme!(CAT, "unknown media {mline} attribute {key}")
                }
                // TODO: ice-options,
                _ => (),
            }
        }

        let specifics = if is_rtp {
            MediaSpecifics::Rtp(RtpMedia::parse(session, mline, media)?)
        } else if is_sctp {
            MediaSpecifics::Datachannel(DataChannelMedia::parse(mline, media)?)
        } else {
            return Err(ParseWebRTCSdpError::UnsupportedMedia(mline));
        };

        Ok(Self {
            media: media_type,
            port: media.port,
            ice_ufrag,
            ice_pwd,
            candidates,
            end_of_candidates,
            setup,
            mid,
            bundle_only: bundle_only.unwrap_or(false),
            fingerprints,
            specifics,
        })
    }

    fn to_sdp_types(&self) -> sdp_types::Media {
        let fmt = if let MediaSpecifics::Rtp(rtp) = &self.specifics {
            rtp.formats.iter().fold(String::new(), |mut s, pt| {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&pt.to_string());
                s
            })
        } else {
            String::new()
        };
        let mut ret = sdp_types::Media {
            media: self.media.as_str().to_owned(),
            port: self.port,
            num_ports: None,
            proto: "UDP/TLS/RTP/SAVPF".to_string(),
            fmt,
            media_title: None,
            connections: vec![sdp_types::Connection {
                nettype: "IN".to_string(),
                addrtype: "IP4".to_string(),
                connection_address: "0.0.0.0".to_string(),
            }],
            bandwidths: vec![],
            key: None,
            attributes: vec![],
        };

        if let Some(ufrag) = self.ice_ufrag.clone() {
            ret.attributes.push(sdp_types::Attribute {
                attribute: "ice-ufrag".to_owned(),
                value: Some(ufrag),
            });
        }
        if let Some(pwd) = self.ice_pwd.clone() {
            ret.attributes.push(sdp_types::Attribute {
                attribute: "ice-pwd".to_owned(),
                value: Some(pwd),
            });
        }
        if let Some(setup) = self.setup {
            ret.attributes.push(sdp_types::Attribute {
                attribute: "setup".to_owned(),
                value: Some(setup.as_str().to_owned()),
            });
        }
        if let Some(mid) = self.mid.clone() {
            ret.attributes.push(sdp_types::Attribute {
                attribute: "mid".to_owned(),
                value: Some(mid),
            });
        }
        if self.bundle_only {
            ret.attributes.push(sdp_types::Attribute {
                attribute: "bundle-only".to_owned(),
                value: None,
            });
        }

        for cand in self.candidates.iter() {
            let mut s = cand.to_sdp_string();
            if s.starts_with("a=candidate:") {
                s = s[12..].to_string();
            }
            ret.attributes.push(sdp_types::Attribute {
                attribute: "candidate".to_owned(),
                value: Some(s),
            });
        }
        if self.end_of_candidates {
            ret.attributes.push(sdp_types::Attribute {
                attribute: "end-of-candidates".to_owned(),
                value: None,
            });
        }
        for fingerprint in self.fingerprints.iter() {
            ret.attributes.push(sdp_types::Attribute {
                attribute: "fingerprint".to_owned(),
                value: Some(fingerprint.to_sdp_attribute_value()),
            });
        }

        match &self.specifics {
            MediaSpecifics::Rtp(rtp) => {
                if rtp.rtcp_mux {
                    ret.attributes.push(sdp_types::Attribute {
                        attribute: "rtcp-mux".to_owned(),
                        value: None,
                    });
                }
                if rtp.rtcp_mux_only {
                    ret.attributes.push(sdp_types::Attribute {
                        attribute: "rtcp-mux-only".to_owned(),
                        value: None,
                    });
                }
                if rtp.rtcp_rsize {
                    ret.attributes.push(sdp_types::Attribute {
                        attribute: "rtcp-rsize".to_owned(),
                        value: None,
                    });
                }
                ret.attributes.push(sdp_types::Attribute {
                    attribute: rtp.direction.as_str().to_owned(),
                    value: None,
                });
                for (payload, fb) in rtp
                    .rtcp_fbs
                    .iter()
                    .map(|(pt, fb)| (pt.to_string(), fb))
                    .chain(rtp.rtcp_fb.iter().map(|fb| ("*".to_string(), fb)))
                {
                    if fb.nack {
                        ret.attributes.push(sdp_types::Attribute {
                            attribute: "rtcp-fb".to_string(),
                            value: Some(format!("{payload} nack")),
                        });
                    }
                    if fb.nack_pli {
                        ret.attributes.push(sdp_types::Attribute {
                            attribute: "rtcp-fb".to_string(),
                            value: Some(format!("{payload} nack pli")),
                        });
                    }
                    if fb.ccm_fir {
                        ret.attributes.push(sdp_types::Attribute {
                            attribute: "rtcp-fb".to_string(),
                            value: Some(format!("{payload} ccm fir")),
                        });
                    }
                    if fb.transport_cc {
                        ret.attributes.push(sdp_types::Attribute {
                            attribute: "rtcp-fb".to_string(),
                            value: Some(format!("{payload} transport-cc")),
                        });
                    }
                }
                for ext in rtp.extmap.iter() {
                    let mut val = ext.id.to_string();
                    if ext.direction != Direction::SendRecv {
                        val.push('/');
                        val.push_str(ext.direction.as_str());
                    }
                    val.push(' ');
                    val.push_str(&ext.name);
                    val.push(' ');
                    if let Some(params) = ext.params.as_ref() {
                        val.push_str(params);
                    }
                    ret.attributes.push(sdp_types::Attribute {
                        attribute: "extmap".to_string(),
                        value: Some(val),
                    });
                }
                for (pt, map) in rtp.rtpmaps.iter() {
                    let mut val = format!("{pt} {}/{}", map.name, map.clock_rate);
                    if let Some(params) = map.params.as_ref() {
                        val.push('/');
                        val.push_str(params);
                    }
                    ret.attributes.push(sdp_types::Attribute {
                        attribute: "rtpmap".to_string(),
                        value: Some(val),
                    });
                }
                for (pt, fmtp) in rtp.fmtps.iter() {
                    ret.attributes.push(sdp_types::Attribute {
                        attribute: "fmtp".to_string(),
                        value: Some(format!("{pt} {fmtp}")),
                    });
                }
            }
            MediaSpecifics::Datachannel(channel) => {
                ret.attributes.push(sdp_types::Attribute {
                    attribute: "sctp-port".to_owned(),
                    value: Some(channel.sctp_port.to_string()),
                });
            }
        }

        ret
    }
}

fn parse_media_type(media: &str) -> Result<MediaType, ParseWebRTCSdpError> {
    MediaType::from_str(media)
        .map_err(|_| ParseWebRTCSdpError::InvalidAttribute("media".to_string()))
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MediaType {
    Audio,
    Video,
    Text,
    Application,
    Message,
}

impl MediaType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Text => "text",
            Self::Application => "application",
            Self::Message => "message",
        }
    }
}

impl FromStr for MediaType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "audio" => Ok(Self::Audio),
            "video" => Ok(Self::Video),
            "text" => Ok(Self::Text),
            "application" => Ok(Self::Application),
            "message" => Ok(Self::Message),
            _ => Err(()),
        }
    }
}

fn parse_ice_attribute(
    mline: Option<u32>,
    key: &str,
    value: &str,
    range: RangeInclusive<usize>,
) -> Result<String, ParseWebRTCSdpError> {
    if range.contains(&value.len()) && value.chars().all(|c| VALID_ICE_ALPHABET.contains(c)) {
        Ok(value.to_string())
    } else {
        Err(ParseWebRTCSdpError::InvalidAttribute(key.to_string()).with_mline(mline))
    }
}

fn parse_fingerprint(fingerprint: &str) -> Option<Fingerprint> {
    let mut it = fingerprint.split(" ");
    let hash_func = HashFunc::from_str(it.next()?).ok()?;
    let hash_str = it.next()?;
    if it.next().is_some() {
        // we are not expecting another value
        return None;
    }

    let mut hash_value = vec![];
    for byte in hash_str.split(":") {
        let byte = u8::from_str_radix(byte, 16).ok()?;
        hash_value.push(byte);
    }

    Some(Fingerprint {
        hash_func,
        hash_value,
    })
}

fn parse_setup(setup: &str) -> Result<DtlsSetup, ParseWebRTCSdpError> {
    DtlsSetup::from_str(setup)
        .map_err(|_| ParseWebRTCSdpError::InvalidAttribute("setup".to_string()))
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DtlsSetup {
    Active,
    Passive,
    ActPass,
    HoldConn,
}

impl DtlsSetup {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Passive => "passive",
            Self::ActPass => "actpass",
            Self::HoldConn => "holdconn",
        }
    }

    pub(crate) fn intersect_with_remote(this: Option<Self>, remote: Option<Self>) -> Option<Self> {
        match (this, remote) {
            (Some(Self::Active | Self::ActPass), Some(Self::Passive | Self::ActPass)) => {
                Some(Self::Active)
            }
            (Some(Self::Passive | Self::ActPass), Some(Self::Active | Self::ActPass)) => {
                Some(Self::Passive)
            }
            _ => None,
        }
    }

    pub(crate) fn answer_direction(offer: Option<Self>) -> Self {
        match offer {
            Some(Self::Active) => Self::Passive,
            _ => Self::Active,
        }
    }
}

impl FromStr for DtlsSetup {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "passive" => Ok(Self::Passive),
            "actpass" => Ok(Self::ActPass),
            "holdconn" => Ok(Self::HoldConn),
            _ => Err(()),
        }
    }
}

fn parse_unique_attribute_no_value<T>(
    mline: Option<u32>,
    key: &str,
    value_str: &Option<String>,
    existing: &Option<T>,
    new_value: T,
) -> Result<T, ParseWebRTCSdpError> {
    if existing.is_some() {
        Err(ParseWebRTCSdpError::MultipleAttributes(key.to_string()).with_mline(mline))
    } else if value_str.is_some() {
        Err(ParseWebRTCSdpError::InvalidAttribute(key.to_string()).with_mline(mline))
    } else {
        Ok(new_value)
    }
}

fn parse_unique_attribute_with_value<T, F: FnOnce(&str) -> Result<T, ParseWebRTCSdpError>>(
    mline: Option<u32>,
    key: &str,
    value_str: &Option<String>,
    existing: &Option<T>,
    check_value: F,
) -> Result<T, ParseWebRTCSdpError> {
    if existing.is_some() {
        Err(ParseWebRTCSdpError::MultipleAttributes(key.to_string()).with_mline(mline))
    } else if let Some(v) = value_str {
        check_value(v.as_str())
    } else {
        Err(ParseWebRTCSdpError::InvalidAttribute(key.to_string()).with_mline(mline))
    }
}

/*
fn is_feedback_rtp_profile(profile: &str) -> bool {
    static FEEDBACK_RTP_PROFILES: [&str; 4] = [
        "RTP/AVPF",
        "RTP/SAVPF",
        "TCP/DTLS/RTP/SAVPF",
        "UDP/TLS/RTP/SAVPF",
    ];
    FEEDBACK_RTP_PROFILES.contains(&profile)
}
*/
fn is_valid_rtp_profile(profile: &str) -> bool {
    static RTP_PROFILES: [&str; 8] = [
        "RTP/AVP",
        "RTP/AVPF",
        "RTP/SAVP",
        "RTP/SAVPF",
        "TCP/DTLS/RTP/SAVP",
        "TCP/DTLS/RTP/SAVPF",
        "UDP/TLS/RTP/SAVP",
        "UDP/TLS/RTP/SAVPF",
    ];
    RTP_PROFILES.contains(&profile)
}

fn is_valid_datachannel_profile(profile: &str) -> bool {
    static DATACHANNEL_PROFILES: [&str; 3] = ["UDP/DTLS/SCTP", "TCP/DTLS/SCTP", "DTLS/SCTP"];
    DATACHANNEL_PROFILES.contains(&profile)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    hash_func: HashFunc,
    hash_value: Vec<u8>,
}

impl Fingerprint {
    pub fn new(func: HashFunc, value: Vec<u8>) -> Self {
        Self {
            hash_func: func,
            hash_value: value,
        }
    }

    fn to_sdp_attribute_value(&self) -> String {
        let mut ret = self.hash_func.as_str().to_owned();
        self.hash_value.iter().fold(false, |first_run, byte| {
            if !first_run {
                ret.push(' ');
            } else {
                ret.push(':');
            }
            ret.push_str(&format!("{byte:02X?}"));
            true
        });
        ret
    }

    pub fn func(&self) -> HashFunc {
        self.hash_func
    }

    pub fn value(&self) -> &[u8] {
        &self.hash_value
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFunc {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Md5,
    Md2,
}

impl HashFunc {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Sha1 => "sha-1",
            Self::Sha224 => "sha-224",
            Self::Sha256 => "sha-256",
            Self::Sha384 => "sha-384",
            Self::Sha512 => "sha-512",
            Self::Md5 => "md5",
            Self::Md2 => "md2",
        }
    }
}

impl FromStr for HashFunc {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha-1" => Ok(Self::Sha1),
            "sha-224" => Ok(Self::Sha224),
            "sha-256" => Ok(Self::Sha256),
            "sha-384" => Ok(Self::Sha384),
            "sha-512" => Ok(Self::Sha512),
            "md5" => Ok(Self::Md5),
            "md2" => Ok(Self::Md2),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaSpecifics {
    Rtp(RtpMedia),
    Datachannel(DataChannelMedia),
}

impl MediaSpecifics {
    pub fn rtp(&self) -> Option<&RtpMedia> {
        if let Self::Rtp(rtp) = self {
            Some(rtp)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RtpMap {
    pub name: String,
    pub clock_rate: u32,
    pub params: Option<String>,
}

impl RtpMap {
    fn from_str(s: &str) -> Result<(u8, Self), ()> {
        let mut s = s.splitn(2, " ");
        let pt = s.next().and_then(parse_payload).ok_or(())?;
        let params = s.next().ok_or(())?;
        let mut s = params.split("/");
        let enc_name = s.next().ok_or(())?;
        let clock_rate = s.next().and_then(|cr| cr.parse::<u32>().ok()).ok_or(())?;
        let enc_params = s.next().map(|s| s.to_string());
        if s.next().is_some() {
            return Err(());
        }
        Ok((
            pt,
            RtpMap {
                name: enc_name.to_string(),
                clock_rate,
                params: enc_params,
            },
        ))
    }
}

fn parse_payload(s: &str) -> Option<u8> {
    let pt = s.parse::<u8>().ok()?;
    if pt > 127 {
        return None;
    }
    Some(pt)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RtcpFb {
    pub nack: bool,
    pub nack_pli: bool,
    pub ccm_fir: bool,
    pub transport_cc: bool,
    pub other: Vec<String>,
}

impl std::ops::BitOr for RtcpFb {
    type Output = RtcpFb;
    fn bitor(self, rhs: Self) -> Self::Output {
        let mut other = self.other;
        for i in rhs.other {
            if !other.contains(&i) {
                other.push(i);
            }
        }
        RtcpFb {
            nack: self.nack | rhs.nack,
            nack_pli: self.nack_pli | rhs.nack_pli,
            ccm_fir: self.ccm_fir | rhs.ccm_fir,
            transport_cc: self.transport_cc | rhs.transport_cc,
            other,
        }
    }
}

impl std::ops::BitOrAssign for RtcpFb {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.clone() | rhs;
    }
}

impl RtcpFb {
    fn from_str(s: &str) -> Result<(Self, Option<u8>), ()> {
        let mut s = s.split(" ");
        let Some(pt_str) = s.next() else {
            return Err(());
        };
        let payload = if pt_str == "*" {
            None
        } else {
            Some(parse_payload(pt_str).ok_or(())?)
        };
        match s.next().ok_or(())? {
            "nack" => match s.next() {
                Some("pli") => match s.next() {
                    None => Ok((
                        RtcpFb {
                            nack_pli: true,
                            ..Default::default()
                        },
                        payload,
                    )),
                    Some(s) => {
                        gst::fixme!(CAT, "Unknown rtcp-fb value nack {s:?}");
                        Ok((
                            RtcpFb {
                                other: vec![format!("nack pli {s}")],
                                ..Default::default()
                            },
                            payload,
                        ))
                    }
                },
                None => Ok((
                    RtcpFb {
                        nack: true,
                        ..Default::default()
                    },
                    payload,
                )),
                Some(s) => {
                    gst::fixme!(CAT, "Unknown rtcp-fb value nack {s:?}");
                    Ok((
                        RtcpFb {
                            other: vec![format!("nack {s}")],
                            ..Default::default()
                        },
                        payload,
                    ))
                }
            },
            "ccm" => match s.next() {
                Some("fir") => match s.next() {
                    None => Ok((
                        RtcpFb {
                            ccm_fir: true,
                            ..Default::default()
                        },
                        payload,
                    )),
                    Some(s) => {
                        gst::fixme!(CAT, "Unknown rtcp-fb value ccm fir {s:?}");
                        Ok((
                            RtcpFb {
                                other: vec![format!("ccm fir {s}")],
                                ..Default::default()
                            },
                            payload,
                        ))
                    }
                },
                s => {
                    gst::fixme!(CAT, "Unknown rtcp-fb value ccm {s:?}");
                    let s = s.map(|s| format!(" {s}")).unwrap_or_else(String::new);
                    Ok((
                        RtcpFb {
                            other: vec![format!("ccm{s}")],
                            ..Default::default()
                        },
                        payload,
                    ))
                }
            },
            "transport-cc" => match s.next() {
                None => Ok((
                    RtcpFb {
                        transport_cc: true,
                        ..Default::default()
                    },
                    payload,
                )),
                Some(s) => {
                    gst::fixme!(CAT, "Unknown rtcp-fb value transport-cc {s:?}");
                    Ok((
                        RtcpFb {
                            other: vec![format!("transport-cc {s}")],
                            ..Default::default()
                        },
                        payload,
                    ))
                }
            },
            s => {
                gst::fixme!(CAT, "Unknown rtcp-fb value {s:?}");
                Ok((
                    RtcpFb {
                        other: vec![s.to_string()],
                        ..Default::default()
                    },
                    payload,
                ))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RtpExtension {
    pub id: u8,
    pub direction: Direction,
    pub name: String,
    pub params: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpMedia {
    pub direction: Direction,
    pub rtcp_mux: bool,
    pub rtcp_mux_only: bool,
    pub rtcp_rsize: bool,
    pub rtcp_fb: Option<RtcpFb>,
    pub extmap: BTreeSet<RtpExtension>,
    pub formats: Vec<u8>,
    pub rtpmaps: BTreeMap<u8, RtpMap>,
    pub rtcp_fbs: BTreeMap<u8, RtcpFb>,
    pub fmtps: BTreeMap<u8, String>,
}

impl RtpMedia {
    fn parse(
        session: &WebRTCSdp,
        mline: u32,
        media: &sdp_types::Media,
    ) -> Result<Self, ParseWebRTCSdpError> {
        let is_rtp = ["audio", "video"].contains(&media.media.as_ref())
            && is_valid_rtp_profile(&media.proto);
        assert!(is_rtp);

        let mut direction = None;
        let mut rtcp_mux = None;
        let mut rtcp_mux_only = None;
        let mut rtcp_rsize = None;
        let mut rtcp_fb_all = None;
        let mut rtpmaps = BTreeMap::<u8, RtpMap>::new();
        let extmap = BTreeSet::new();
        let mut fmtps = BTreeMap::<u8, String>::new();
        let mut rtcp_fbs = BTreeMap::<u8, RtcpFb>::new();
        let formats = media.fmt.split(" ").try_fold(vec![], |mut fmts, item| {
            parse_payload(item)
                .ok_or(ParseWebRTCSdpError::UnsupportedMedia(mline))
                .map(|fmt| {
                    fmts.push(fmt);
                    fmts
                })
        })?;
        for attr in media.attributes.iter() {
            match attr.attribute.as_str() {
                "sendrecv" => {
                    direction = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "sendrecv",
                        &attr.value,
                        &direction,
                        Direction::SendRecv,
                    )?)
                }
                "sendonly" => {
                    direction = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "sendonly",
                        &attr.value,
                        &direction,
                        Direction::SendOnly,
                    )?)
                }
                "recvonly" => {
                    direction = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "recvonly",
                        &attr.value,
                        &direction,
                        Direction::RecvOnly,
                    )?)
                }
                "inactive" => {
                    direction = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "inactive",
                        &attr.value,
                        &direction,
                        Direction::Inactive,
                    )?)
                }
                "rtcp-mux" => {
                    rtcp_mux = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "rtcp-mux",
                        &attr.value,
                        &rtcp_mux,
                        true,
                    )?)
                }
                "rtcp-mux-only" => {
                    rtcp_mux_only = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "rtcp-mux-only",
                        &attr.value,
                        &rtcp_mux_only,
                        true,
                    )?)
                }
                "rtcp-rsize" => {
                    rtcp_rsize = Some(parse_unique_attribute_no_value(
                        Some(mline),
                        "rtcp-rsize",
                        &attr.value,
                        &rtcp_rsize,
                        true,
                    )?)
                }
                "rtcp" => (), // ignored
                "rtcp-fb" => {
                    let Some(ref fb) = attr.value else {
                        return Err(ParseWebRTCSdpError::InvalidAttribute("rtcp-fb".to_string()));
                    };
                    let (rtcp_fb, payload) = RtcpFb::from_str(fb).map_err(|_| {
                        ParseWebRTCSdpError::InvalidAttribute("rtcp-fb".to_string())
                    })?;
                    if let Some(payload) = payload {
                        if !formats.contains(&payload) {
                            gst::fixme!(
                                CAT,
                                "rtcp-fb attribute uses a payload {payload} that does not exist in the m-line {mline}"
                            );
                            return Err(ParseWebRTCSdpError::InvalidAttribute(
                                "rtcp-fb".to_string(),
                            ));
                        }
                        rtcp_fbs
                            .entry(payload)
                            .and_modify(|fb| *fb |= rtcp_fb.clone())
                            .or_insert(rtcp_fb);
                    } else {
                        *rtcp_fb_all.get_or_insert(rtcp_fb.clone()) |= rtcp_fb;
                    }
                }
                "fmtp" => {
                    let Some(ref fb) = attr.value else {
                        return Err(ParseWebRTCSdpError::InvalidAttribute("fmtp".to_string()));
                    };
                    let mut s = fb.splitn(2, " ");
                    let Some(payload) = s.next().and_then(parse_payload) else {
                        return Err(ParseWebRTCSdpError::InvalidAttribute("fmtp".to_string()));
                    };
                    if !formats.contains(&payload) {
                        return Err(ParseWebRTCSdpError::InvalidAttribute("fmtp".to_string()));
                    }
                    let Some(fmtp) = s.next() else {
                        return Err(ParseWebRTCSdpError::InvalidAttribute("fmtp".to_string()));
                    };
                    match fmtps.entry(payload) {
                        std::collections::btree_map::Entry::Occupied(_occupied) => {
                            return Err(ParseWebRTCSdpError::MultipleMediaAttributes(
                                mline,
                                "fmtp".to_string(),
                            ));
                        }
                        std::collections::btree_map::Entry::Vacant(vacant) => {
                            vacant.insert(fmtp.to_string());
                        }
                    }
                }
                "rtpmap" => {
                    let Some(ref rtpmap) = attr.value else {
                        return Err(ParseWebRTCSdpError::InvalidAttribute("rtpmap".to_string()));
                    };
                    let (payload, rtpmap) = RtpMap::from_str(rtpmap)
                        .map_err(|_| ParseWebRTCSdpError::InvalidAttribute("rtpmap".to_string()))?;
                    match rtpmaps.entry(payload) {
                        std::collections::btree_map::Entry::Vacant(vacant) => {
                            vacant.insert(rtpmap);
                        }
                        std::collections::btree_map::Entry::Occupied(_occupied) => {
                            return Err(ParseWebRTCSdpError::MultipleMediaAttributes(
                                mline,
                                "rtpmap".to_string(),
                            ));
                        }
                    }
                }
                // TODO: ptime, maxptime, ssrc, extmap,
                // msid, imageattr, rid, simulcast,
                key if !KNOWN_MEDIA_ATTRIBUTES.contains(&key) => {
                    gst::fixme!(CAT, "unknown media {mline} attribute {key}")
                }
                _ => (),
            }
        }

        let rtcp_mux = rtcp_mux.unwrap_or(false);
        let rtcp_mux_only = rtcp_mux_only.unwrap_or(false);

        if rtcp_mux_only && !rtcp_mux {
            return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                mline,
                "rtcp-mux".to_string(),
            ));
        }

        Ok(Self {
            direction: direction
                .or(session.direction)
                .unwrap_or(Direction::SendRecv),
            rtcp_mux,
            rtcp_mux_only,
            rtcp_rsize: rtcp_rsize.unwrap_or(false),
            rtcp_fb: rtcp_fb_all,
            formats,
            rtpmaps,
            rtcp_fbs,
            fmtps,
            extmap,
        })
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    #[default]
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::SendRecv => "sendrecv",
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::Inactive => "inactive",
        }
    }

    pub fn has_send(self) -> bool {
        matches!(self, Self::SendRecv | Self::SendOnly)
    }

    pub fn has_recv(self) -> bool {
        matches!(self, Self::SendRecv | Self::RecvOnly)
    }

    pub fn reverse(self) -> Self {
        match self {
            Self::SendRecv => Self::SendRecv,
            Self::SendOnly => Self::RecvOnly,
            Self::RecvOnly => Self::SendOnly,
            Self::Inactive => Self::Inactive,
        }
    }

    pub fn intersect_with_answer(self, answer: Self) -> Self {
        match (self, answer) {
            (Self::Inactive, _)
            | (_, Self::Inactive)
            | (Self::RecvOnly, Self::RecvOnly)
            | (Self::SendOnly, Self::SendOnly) => Self::Inactive,
            (Self::SendRecv, Self::SendRecv) => Self::SendRecv,
            (Self::SendOnly | Self::SendRecv, Self::SendRecv | Self::RecvOnly) => Self::RecvOnly,
            (Self::RecvOnly | Self::SendRecv, Self::SendRecv | Self::SendOnly) => Self::SendOnly,
        }
    }
}

impl FromStr for Direction {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sendrecv" => Ok(Self::SendRecv),
            "sendonly" => Ok(Self::SendOnly),
            "recvonly" => Ok(Self::RecvOnly),
            "inactive" => Ok(Self::Inactive),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataChannelMedia {
    pub sctp_port: u16,
}

impl DataChannelMedia {
    fn parse(mline: u32, media: &sdp_types::Media) -> Result<Self, ParseWebRTCSdpError> {
        let is_sctp = ["application"].contains(&media.media.as_ref())
            && is_valid_datachannel_profile(&media.proto);

        let mut sctp_port = None;
        for attr in media.attributes.iter() {
            match attr.attribute.as_str() {
                "sctp-port" if is_sctp => {
                    sctp_port = Some(
                        parse_unique_attribute_with_value(
                            Some(mline),
                            "sctp-port",
                            &attr.value,
                            &sctp_port,
                            |val| {
                                val.parse::<u16>().map_err(|_| {
                                    ParseWebRTCSdpError::InvalidAttribute("sctp-port".to_string())
                                })
                            },
                        )
                        .map_err(|e| e.with_mline(Some(mline)))?,
                    )
                }
                // TODO: max-message-size
                key if !KNOWN_MEDIA_ATTRIBUTES.contains(&key) => {
                    gst::fixme!(CAT, "unknown media {mline} attribute {key}")
                }
                _ => (),
            }
        }

        Ok(Self {
            sctp_port: sctp_port.ok_or_else(|| {
                ParseWebRTCSdpError::MissingRequiredMediaAttribute(mline, "sctp-proto".to_string())
            })?,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseWebRTCSdpError {
    #[error("Syntax error: {0}")]
    Syntax(sdp_types::ParserError),
    #[error("Multiple '{}' attributes were discovered", .0)]
    MultipleAttributes(String),
    #[error("Invalid '{}' attribute", .0)]
    InvalidAttribute(String),
    #[error("Unsupported media with mline {}", .0)]
    UnsupportedMedia(u32),
    #[error("Failed to parse a candidate {:?}", .0)]
    InvalidCandidate(ParseCandidateError),
    #[error("Multiple '{}' attributes were discovered in media {}", .1, .0)]
    MultipleMediaAttributes(u32, String),
    #[error("Invalid '{}' attribute in media {}", .1, .0)]
    InvalidMediaAttribute(u32, String),
    #[error("Failed to parse media {} candidate '{:?}'", .0, .1)]
    InvalidMediaCandidate(u32, ParseCandidateError),
    #[error("Media {} is missing required attribute '{}'", .0, .1)]
    MissingRequiredMediaAttribute(u32, String),
}

impl ParseWebRTCSdpError {
    fn with_mline(self, mline: Option<u32>) -> Self {
        if let Some(mline) = mline {
            match self {
                Self::InvalidAttribute(attr) => Self::InvalidMediaAttribute(mline, attr),
                Self::MultipleAttributes(attr) => Self::MultipleMediaAttributes(mline, attr),
                Self::InvalidCandidate(cand) => Self::InvalidMediaCandidate(mline, cand),
                rest => rest,
            }
        } else {
            self
        }
    }
}

impl From<sdp_types::ParserError> for ParseWebRTCSdpError {
    fn from(value: sdp_types::ParserError) -> Self {
        Self::Syntax(value)
    }
}

impl From<ParseCandidateError> for ParseWebRTCSdpError {
    fn from(value: ParseCandidateError) -> Self {
        Self::InvalidCandidate(value)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    #[test]
    fn test_direction_answer_intersection() {
        let directions = [
            Direction::SendRecv,
            Direction::SendOnly,
            Direction::RecvOnly,
            Direction::Inactive,
        ];
        for dir in directions {
            assert_eq!(
                Direction::Inactive.intersect_with_answer(dir),
                Direction::Inactive
            );
            assert_eq!(
                dir.intersect_with_answer(Direction::Inactive),
                Direction::Inactive
            );
        }
        assert_eq!(
            Direction::SendOnly.intersect_with_answer(Direction::SendOnly),
            Direction::Inactive
        );
        assert_eq!(
            Direction::RecvOnly.intersect_with_answer(Direction::RecvOnly),
            Direction::Inactive
        );

        assert_eq!(
            Direction::SendRecv.intersect_with_answer(Direction::SendRecv),
            Direction::SendRecv
        );

        assert_eq!(
            Direction::SendRecv.intersect_with_answer(Direction::SendOnly),
            Direction::SendOnly
        );
        assert_eq!(
            Direction::RecvOnly.intersect_with_answer(Direction::SendRecv),
            Direction::SendOnly
        );

        assert_eq!(
            Direction::SendRecv.intersect_with_answer(Direction::RecvOnly),
            Direction::RecvOnly
        );
        assert_eq!(
            Direction::SendOnly.intersect_with_answer(Direction::SendRecv),
            Direction::RecvOnly
        );
    }

    static SESSION_HEADER: &str = "v=0\r\n\
o=- 3498989708992231200 0 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
a=ice-options:trickle";

    static MEDIA_AUDIO: &str = "m=audio 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=setup:actpass\r\n\
a=ice-ufrag:W9PZs\r\n\
a=ice-pwd:+N4wEaXW9bV9uo/o9OkVlgMudD+KTDgB\r\n\
a=rtcp-mux\r\n\
a=rtcp-rsize\r\n\
a=sendrecv\r\n\
a=rtpmap:96 OPUS/48000\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=ssrc:3384078950 msid:user3252793596@host-26022109 webrtctransceiver0\r\n\
a=ssrc:3384078950 cname:user3252793596@host-26022109\r\n\
a=mid:audio0\r\n\
a=fingerprint:sha-256 9B:7B:AD:68:EC:00:86:1A:CD:09:01:E7:7E:C5:53:29:1F:91:D8:9E:41:72:5C:5D:D1:A1:38:B2:6C:35:22:58\r\n\
a=rtcp-mux-only";

    #[test]
    fn test_parse_no_media() {
        let sdp = WebRTCSdp::parse(WebRTCSdpType::Offer, SESSION_HEADER).unwrap();
        eprintln!("{:?}", sdp);
        assert_eq!(&sdp.id, "3498989708992231200");
        assert!(sdp.media.is_empty());
    }

    #[test]
    fn test_parse_audio() {
        let sdp = WebRTCSdp::parse(
            WebRTCSdpType::Offer,
            &format!("{}\r\n{}", SESSION_HEADER, MEDIA_AUDIO),
        )
        .unwrap();
        eprintln!("{:?}", sdp);
        assert_eq!(&sdp.id, "3498989708992231200");
        assert_eq!(sdp.media.len(), 1);
        let media = &sdp.media[0];
        assert_eq!(media.media, MediaType::Audio);
        assert_eq!(media.ice_ufrag, Some(String::from("W9PZs")));
        assert_eq!(
            media.ice_pwd,
            Some(String::from("+N4wEaXW9bV9uo/o9OkVlgMudD+KTDgB"))
        );
        assert!(media.candidates.is_empty());
        assert!(!media.end_of_candidates);
        assert_eq!(media.setup, Some(DtlsSetup::ActPass));
        assert_eq!(media.mid, Some(String::from("audio0")));
        assert!(!media.bundle_only);
        assert_eq!(
            media.fingerprints,
            vec![Fingerprint {
                hash_func: HashFunc::Sha256,
                hash_value: vec![
                    0x9b, 0x7b, 0xad, 0x68, 0xec, 0x00, 0x86, 0x1a, 0xcd, 0x09, 0x01, 0xe7, 0x7e,
                    0xc5, 0x53, 0x29, 0x1f, 0x91, 0xd8, 0x9e, 0x41, 0x72, 0x5c, 0x5d, 0xd1, 0xa1,
                    0x38, 0xb2, 0x6c, 0x35, 0x22, 0x58
                ],
            }]
        );
        let MediaSpecifics::Rtp(rtp) = &media.specifics else {
            unreachable!();
        };
        assert_eq!(rtp.direction, Direction::SendRecv);
        assert!(rtp.rtcp_mux);
        assert!(rtp.rtcp_rsize);
        assert!(rtp.rtcp_mux_only);
        assert_eq!(rtp.rtcp_fb, None);
        assert!(rtp.extmap.is_empty());
        assert_eq!(rtp.formats, vec![96]);
        assert_eq!(
            rtp.rtpmaps,
            BTreeMap::from_iter([(
                96,
                RtpMap {
                    name: String::from("OPUS"),
                    clock_rate: 48000,
                    params: None,
                }
            )])
        );
        assert_eq!(
            rtp.rtcp_fbs,
            BTreeMap::from_iter([(
                96,
                RtcpFb {
                    transport_cc: true,
                    ..Default::default()
                }
            )])
        );
        assert!(rtp.fmtps.is_empty());
    }

    #[test]
    fn test_parse_short_ice_ufrag() {
        assert!(matches!(
            WebRTCSdp::parse(
                WebRTCSdpType::Offer,
                &format!(
                    "{}\r\n\
a=ice-ufrag:a\r\n",
                    SESSION_HEADER
                )
            ),
            Err(ParseWebRTCSdpError::InvalidAttribute(_))
        ));
    }

    #[test]
    fn test_parse_short_ice_pwd() {
        assert!(matches!(
            WebRTCSdp::parse(
                WebRTCSdpType::Offer,
                &format!(
                    "{}\r\n\
a=ice-pwd:a\r\n",
                    SESSION_HEADER
                )
            ),
            Err(ParseWebRTCSdpError::InvalidAttribute(_))
        ));
    }

    #[test]
    fn test_parse_candidate() {
        let addr = SocketAddr::new([192, 168, 0, 1].into(), 50000);
        let candidate = librice::candidate::Candidate::builder(
            1,
            librice::candidate::CandidateType::Host,
            librice::candidate::TransportType::Udp,
            "0",
            addr.into(),
        )
        .priority(1000)
        .base_address(addr.into())
        .build();
        let sdp = WebRTCSdp::parse(
            WebRTCSdpType::Offer,
            &format!(
                "{}\r\n\
{}\r\n\
{}\r\n",
                SESSION_HEADER,
                MEDIA_AUDIO,
                candidate.to_sdp_string()
            ),
        )
        .unwrap();
        assert_eq!(sdp.media[0].candidates[0], candidate);
    }

    #[test]
    fn test_write_candidate() {
        let addr = SocketAddr::new([192, 168, 0, 1].into(), 50000);
        let candidate = librice::candidate::Candidate::builder(
            1,
            librice::candidate::CandidateType::Host,
            librice::candidate::TransportType::Udp,
            "0",
            addr.into(),
        )
        .priority(1000)
        .base_address(addr.into())
        .build();

        let mut sdp = WebRTCSdp::parse(
            WebRTCSdpType::Offer,
            &format!(
                "{}\r\n\
{}\r\n",
                SESSION_HEADER, MEDIA_AUDIO,
            ),
        )
        .unwrap();
        sdp.media[0].candidates.push(candidate.clone());
        let sdp_string = sdp.to_sdp_string();
        println!("sdp {sdp_string}");
        let sdp = WebRTCSdp::parse(WebRTCSdpType::Offer, &sdp_string).unwrap();
        assert_eq!(sdp.media[0].candidates[0], candidate);
    }
}
