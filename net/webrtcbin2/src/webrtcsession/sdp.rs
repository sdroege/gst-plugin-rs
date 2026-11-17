// SPDX-License-Identifier: MPL-2.0

//! WebRTC-specific SDP parsing, validation and offer/answer negotiation
//! helpers on top of the `sdp_types_gst` types.

use std::fmt::Display;
use std::str::FromStr;
use std::sync::LazyLock;

use sdp_types::{Direction, GroupSemantics, Setup};
use sdp_types_gst::{Media, RtcpFb, Session};

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

#[derive(Debug, thiserror::Error)]
pub enum ParseWebRTCSdpError {
    #[error("Syntax error: {0}")]
    Syntax(#[from] sdp_types_gst::ParseError),
    #[error("Invalid '{}' attribute", .0)]
    InvalidAttribute(String),
    #[error("Invalid '{}' attribute in media {}", .1, .0)]
    InvalidMediaAttribute(u32, String),
    #[error("Media {} is missing required attribute '{}'", .0, .1)]
    MissingRequiredMediaAttribute(u32, String),
    #[error("Unsupported media with mline {}", .0)]
    UnsupportedMedia(u32),
    #[error("More than one a=group:BUNDLE attribute")]
    MultipleBundleGroups,
}

/// Datachannel media (`m=` line with a DTLS/SCTP transport protocol).
pub fn is_datachannel(media: &Media) -> bool {
    media.proto().is_sctp()
}

/// Whether an answer rejects this media line: data channels are unsupported,
/// and a zero port without `a=bundle-only` disables it (RFC 8843 section
/// 7.5.3).
pub fn is_rejected_media(media: &Media) -> bool {
    is_datachannel(media) || (media.port() == 0 && !media.bundle_only())
}

/// Select the offerer-tagged `m=` section for an answer (RFC 8843 section
/// 7.3.1), skipping moved-out, rejected and zero-port sections. Returns `None`
/// if no tag qualifies.
pub fn select_bundle_tag(sdp: &Session) -> Option<usize> {
    for tag in sdp.bundle_tags() {
        let Some((idx, media)) = sdp
            .media()
            .iter()
            .enumerate()
            .find(|(_, media)| media.mid().is_some_and(|mid| mid.0 == *tag))
        else {
            continue;
        };
        if is_rejected_media(media) || media.port() == 0 {
            continue;
        }
        return Some(idx);
    }

    None
}

/// Validation of the WebRTC-specific SDP constraints on top of the syntax
/// checks of `Session::parse()`.
pub fn validate_webrtc_sdp(sdp: &Session) -> Result<(), ParseWebRTCSdpError> {
    // All bundled media are multiplexed onto a single transport, so a second
    // BUNDLE group (allowed by RFC 8843 section 5) cannot be represented.
    if sdp
        .groups()
        .iter()
        .filter(|group| matches!(group.semantics, GroupSemantics::Bundle))
        .count()
        > 1
    {
        return Err(ParseWebRTCSdpError::MultipleBundleGroups);
    }

    let bundle = sdp.bundle_tags();
    for bundle_tag in bundle {
        if !bundle_tag.is_ascii() {
            return Err(ParseWebRTCSdpError::InvalidAttribute(
                "group:BUNDLE".to_string(),
            ));
        }
    }

    // RFC 8843 section 7.2.1: the offerer must not suggest a bundle-only
    // "m=" section as the tagged section.
    if let Some(first_tag) = bundle.first()
        && let Some(first_media) = sdp
            .media()
            .iter()
            .find(|media| media.mid().is_some_and(|mid| mid.0 == *first_tag))
        && first_media.bundle_only()
    {
        return Err(ParseWebRTCSdpError::InvalidAttribute(
            "group:BUNDLE".to_string(),
        ));
    }

    let bundle_idx = sdp.bundle_idx();
    for (idx, media) in sdp.media().iter().enumerate() {
        let mline = idx as u32;

        if media.bundle_only() {
            let Some(mid) = media.mid() else {
                return Err(ParseWebRTCSdpError::InvalidMediaAttribute(
                    mline,
                    "bundle-only".to_string(),
                ));
            };
            // RFC 8843 section 6: bundle-only is only defined with a zero
            // port.
            if media.port() != 0 {
                return Err(ParseWebRTCSdpError::InvalidMediaAttribute(
                    mline,
                    "bundle-only".to_string(),
                ));
            }
            if !bundle.contains(&mid.0) {
                gst::warning!(
                    CAT,
                    "bundle group {bundle:?} does not contain mid {} with a=bundle-only",
                    mid.0
                );
                return Err(ParseWebRTCSdpError::InvalidMediaAttribute(
                    mline,
                    "mid".into(),
                ));
            }
        }

        if is_datachannel(media) {
            if media.sctp_port().is_none() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    mline,
                    "sctp-port".to_string(),
                ));
            }
        } else if !media.proto().is_rtp() {
            return Err(ParseWebRTCSdpError::UnsupportedMedia(mline));
        } else if media.rtcp_mux_only() && !media.rtcp_mux() {
            return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                mline,
                "rtcp-mux".to_string(),
            ));
        }

        // A zero port without bundle-only rejects or disables the media line,
        // so it has no transport.
        if media.port() == 0 && !media.bundle_only() {
            continue;
        }

        let is_bundled =
            bundle_idx.is_some() && media.mid().is_some_and(|mid| bundle.contains(&mid.0));

        // The tagged media line provides the transport for the whole BUNDLE
        // group. Other media lines need their own.
        if !is_bundled || Some(idx) == bundle_idx {
            if media.ice_ufrag().is_none() && sdp.ice_ufrag().is_none() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    mline,
                    "ice-ufrag".to_string(),
                ));
            }
            if media.ice_pwd().is_none() && sdp.ice_pwd().is_none() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    mline,
                    "ice-pwd".to_string(),
                ));
            }
            if media.setup().is_none() && sdp.setup().is_none() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    mline,
                    "setup".to_string(),
                ));
            }
            if media.fingerprints().is_empty() && sdp.fingerprints().is_empty() {
                return Err(ParseWebRTCSdpError::MissingRequiredMediaAttribute(
                    mline,
                    "fingerprint".to_string(),
                ));
            }
        }
    }

    Ok(())
}

/// Parse an SDP session and validate the WebRTC-specific constraints.
pub fn parse_webrtc_sdp(session: &str) -> Result<Session, ParseWebRTCSdpError> {
    let sdp = Session::parse(session)?;
    validate_webrtc_sdp(&sdp)?;
    Ok(sdp)
}

pub fn all_supported_fb() -> RtcpFb {
    // TODO: support more rtcp-fb options
    RtcpFb {
        nack: false,
        nack_pli: false,
        ccm_fir: false,
        transport_cc: false,
        other: vec![],
    }
}

/// Intersect the RTP parameters of an offer and an answer into the answer.
///
/// The answer is consumed and returned with the intersection applied.
pub fn answer_media(offer: &Media, answer: Media) -> Media {
    let mut ret = answer;
    ret.set_direction(ret.direction().intersect_with_remote(offer.direction()));
    ret.set_rtcp_mux(ret.rtcp_mux() && offer.rtcp_mux());
    ret.set_rtcp_mux_only(ret.rtcp_mux_only() && offer.rtcp_mux_only());
    ret.set_rtcp_rsize(ret.rtcp_rsize() && offer.rtcp_rsize());
    let mut supported_formats = vec![];
    for fmt in ret.formats().iter() {
        if !offer.formats().contains(fmt) {
            continue;
        }
        let offer_rtpmap = offer.rtpmaps().get(fmt);
        let answer_rtpmap = ret.rtpmaps().get(fmt);
        if offer_rtpmap.is_none() != answer_rtpmap.is_none()
            && offer_rtpmap.is_some_and(|offer| {
                answer_rtpmap.is_some_and(|answer| {
                    !offer
                        .encoding_name
                        .eq_ignore_ascii_case(&answer.encoding_name)
                        || offer.clock_rate != answer.clock_rate
                        || offer.encoding_params != answer.encoding_params
                })
            })
        {
            gst::trace!(
                CAT,
                "offer rtpmap for format {fmt}: {offer_rtpmap:?}, does not match answer rtpmap {answer_rtpmap:?}"
            );
            continue;
        }
        // TODO: ignore red/rtx/ulpfec as they are not supported yet.
        let is_supported_rtpmap = |name: &str| {
            !name.eq_ignore_ascii_case("rtx")
                && !name.eq_ignore_ascii_case("red")
                && !name.eq_ignore_ascii_case("ulpfec")
                && !name.eq_ignore_ascii_case("telephone-event")
        };
        if let Some(rtpmap) = offer_rtpmap
            && !is_supported_rtpmap(&rtpmap.encoding_name)
        {
            continue;
        }
        let offer_fmtp = offer.fmtps().get(fmt);
        let answer_fmtp = ret.fmtps().get(fmt);
        if offer_fmtp != answer_fmtp {
            gst::fixme!(
                CAT,
                "offer fmtp for format {fmt}: {offer_fmtp:?}, does not match answer fmtp {answer_fmtp:?}"
            );
            // TODO: media-specific equivalence
            //continue;
        }
        supported_formats.push(*fmt);
    }
    ret.formats_mut()
        .retain(|fmt| supported_formats.contains(fmt));
    ret.rtpmaps_mut()
        .retain(|fmt, _val| supported_formats.contains(fmt));
    ret.fmtps_mut()
        .retain(|fmt, _val| supported_formats.contains(fmt));
    ret.rtcp_fbs_mut().retain(|fmt, fb| {
        *fb &= all_supported_fb();
        supported_formats.contains(fmt)
    });
    if let Some(rtcp_fb) = ret.rtcp_fb_mut() {
        *rtcp_fb &= offer.rtcp_fb().cloned().unwrap_or_default() & all_supported_fb();
    }
    // webrtcbin2 does not implement RTP header extensions, so remove them
    // from the answer (RFC 8285 section 7).
    ret.extmap_mut().clear();
    ret
}

/// Create the answer media for an offer media line without a local
/// transceiver. Remote-only attributes such as `a=ssrc`, `a=msid`, `a=rid`,
/// `a=remote-candidates` or `a=crypto` are not copied.
pub fn answer_media_from_offer(offer: &Media) -> Media {
    Media::builder(offer.media())
        .direction(Direction::RecvOnly)
        .rtcp_mux(true)
        .rtcp_mux_only(false)
        .rtcp_rsize(false)
        .formats(offer.formats().to_vec())
        .rtpmaps(offer.rtpmaps().clone())
        .fmtps(offer.fmtps().clone())
        .rtcp_fb(offer.rtcp_fb().cloned())
        .rtcp_fbs(offer.rtcp_fbs().clone())
        .mid(offer.mid().cloned())
        .build()
}

pub fn media_intersect(mine: &Media, other: &Media) -> Option<Media> {
    if mine.media() != other.media() {
        return None;
    }
    gst::trace!(CAT, "attempting to intersect:");
    gst::trace!(CAT, "                   ours: {mine:?}");
    gst::trace!(CAT, "                  other: {other:?}");
    if is_datachannel(mine) || is_datachannel(other) {
        // FIXME data channel
        return None;
    }
    let intersected = answer_media(mine, other.clone());
    gst::trace!(CAT, "                 result: {intersected:?}");
    if intersected.formats().is_empty() {
        return None;
    }
    Some(intersected)
}

/// Intersect two DTLS setup roles, `None` when they cannot be combined.
pub fn intersect_setup(this: Option<&Setup>, remote: Option<&Setup>) -> Option<Setup> {
    match (this, remote) {
        (Some(Setup::Active | Setup::ActPass), Some(Setup::Passive | Setup::ActPass)) => {
            Some(Setup::Active)
        }
        (Some(Setup::Passive | Setup::ActPass), Some(Setup::Active | Setup::ActPass)) => {
            Some(Setup::Passive)
        }
        _ => None,
    }
}

/// The DTLS setup role to use in the answer to `offer`.
pub fn answer_setup(offer: Option<&Setup>) -> Setup {
    match offer {
        Some(Setup::Active) => Setup::Passive,
        _ => Setup::Active,
    }
}
