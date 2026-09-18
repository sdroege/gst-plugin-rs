// Copyright (C) 2024 Nirbheek Chauhan <nirbheek@centricular.com>
// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Typed SDP attribute values and attribute parsing helpers.

use std::str::FromStr;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sdp_types::{Direction, KeyMgmtMethod, RtcpFbCcm, RtcpFbNack, RtcpFbVal};

use super::{CAT, error::MediaError};

// Attributes never mapped to/from `a-`/`x-` caps fields: RTSP specifics.
pub(crate) const SKIPPED_OTHER_ATTRIBUTES: [&str; 2] = ["control", "range"];

pub(crate) fn parse_payload(s: &str) -> Result<u8, MediaError> {
    let pt = s
        .parse::<u8>()
        .map_err(|_| MediaError::InvalidPayloadType(s.to_string()))?;
    if pt > 127 {
        return Err(MediaError::InvalidPayloadType(s.to_string()));
    }
    Ok(pt)
}

pub(crate) fn direction_attr_value(name: &str) -> Option<Direction> {
    Direction::from_str(name).ok()
}

/// Converts an attribute without special handling into its `sdp_types`
/// form, preserving a missing value.
pub(crate) fn other_attribute((name, value): &(String, Option<String>)) -> sdp_types::Attribute {
    match value {
        Some(value) => sdp_types::Attribute::with_value(name.clone(), value.clone()),
        None => sdp_types::Attribute::new(name.clone()),
    }
}

/// The `a=rtcp-fb` values that map to caps fields: the feedback parameters
/// of `nack`/`ccm` beyond `pli`/`fir` and any other feedback type are kept
/// as-is in `other`.
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
    fn bitor(mut self, rhs: Self) -> Self::Output {
        self |= rhs;
        self
    }
}

impl std::ops::BitAnd for RtcpFb {
    type Output = RtcpFb;
    fn bitand(mut self, rhs: Self) -> Self::Output {
        self &= rhs;
        self
    }
}

impl std::ops::BitOrAssign for RtcpFb {
    fn bitor_assign(&mut self, rhs: Self) {
        self.nack |= rhs.nack;
        self.nack_pli |= rhs.nack_pli;
        self.ccm_fir |= rhs.ccm_fir;
        self.transport_cc |= rhs.transport_cc;
        for i in rhs.other {
            if !self.other.contains(&i) {
                self.other.push(i);
            }
        }
    }
}

impl std::ops::BitAndAssign for RtcpFb {
    fn bitand_assign(&mut self, rhs: Self) {
        self.nack &= rhs.nack;
        self.nack_pli &= rhs.nack_pli;
        self.ccm_fir &= rhs.ccm_fir;
        self.transport_cc &= rhs.transport_cc;
        self.other.retain(|i| rhs.other.contains(i));
    }
}

impl RtcpFb {
    /// Parses an `a=rtcp-fb` attribute value, returns the feedback and the
    /// payload type, `None` if the wildcard payload type was used.
    pub(crate) fn from_sdp_str(s: &str) -> Result<(Self, Option<u8>), MediaError> {
        let invalid = |reason: &str| MediaError::InvalidAttribute {
            name: "rtcp-fb".into(),
            reason: reason.into(),
        };
        let mut tokens = s.split(' ');
        // split(' ') always yields at least one token, possibly empty: the
        // payload type parsing below rejects it
        let pt_str = tokens.next().unwrap_or_default();
        let payload = if pt_str == "*" {
            None
        } else {
            Some(parse_payload(pt_str)?)
        };
        if tokens.next().is_none() {
            return Err(invalid("missing feedback type"));
        };
        // Value part without the payload type, kept as-is for feedback types
        // without caps fields.
        let tail = s
            .split_once(' ')
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();

        let val = match sdp_types::RtcpFb::from_str(s) {
            Ok(fb) => fb.val,
            Err(err) => {
                gst::fixme!(CAT, "Unparsable rtcp-fb value {s:?}: {err}");
                return Ok((
                    RtcpFb {
                        other: vec![tail],
                        ..Default::default()
                    },
                    payload,
                ));
            }
        };

        let mut ret = RtcpFb::default();
        match val {
            RtcpFbVal::Nack(None) => ret.nack = true,
            RtcpFbVal::Nack(Some(RtcpFbNack::Pli)) => ret.nack_pli = true,
            RtcpFbVal::Ccm(RtcpFbCcm::Fir) => ret.ccm_fir = true,
            RtcpFbVal::TransportCc => ret.transport_cc = true,
            _ => {
                gst::fixme!(CAT, "Unsupported rtcp-fb value {tail:?}");
                ret.other = vec![tail];
            }
        }
        Ok((ret, payload))
    }

    /// Serializes as SDP `a=rtcp-fb` parameter strings.
    pub(crate) fn sdp_params(&self) -> Vec<String> {
        let mut params = Vec::new();
        if self.nack {
            params.push("nack".to_string());
        }
        if self.nack_pli {
            params.push("nack pli".to_string());
        }
        if self.ccm_fir {
            params.push("ccm fir".to_string());
        }
        if self.transport_cc {
            params.push("transport-cc".to_string());
        }
        params.extend(self.other.iter().cloned());
        params
    }
}

/// Validates `c=` connection data (RFC 8866 section 5.7). An FQDN is accepted
/// even though `try_address()` cannot represent it.
pub(crate) fn check_connection(connection: &sdp_types::Connection) -> Result<(), MediaError> {
    match connection.try_address() {
        Ok(_) => Ok(()),
        Err(sdp_types::ConnectionAddressError::InvalidAddress(_)) => Ok(()),
        Err(sdp_types::ConnectionAddressError::InvalidTtl(_)) => Err(MediaError::ConnectionTtl {
            address: connection.connection_address.clone(),
        }),
        Err(sdp_types::ConnectionAddressError::InvalidCount(_)) => {
            Err(MediaError::ConnectionAddress {
                address: connection.connection_address.clone(),
            })
        }
    }
}

/// Extension methods for [`sdp_types::KeyMgmt`].
pub trait KeyMgmtExt {
    /// Decodes the MIKEY message of the "mikey" key management method.
    fn mikey(&self) -> Result<mykey::MikeyMessage, mykey::MikeyError>;
}

impl KeyMgmtExt for sdp_types::KeyMgmt {
    fn mikey(&self) -> Result<mykey::MikeyMessage, mykey::MikeyError> {
        use mykey::{MikeyError, MikeyMessage};

        if self.method != KeyMgmtMethod::Mikey {
            return Err(MikeyError::Parse(format!(
                "key management method '{}' carries no MIKEY message",
                self.method
            )));
        }

        let data = self
            .data
            .as_deref()
            .ok_or_else(|| MikeyError::Parse("missing key management data".into()))?;
        let bytes = STANDARD
            .decode(data.trim())
            .map_err(|err| MikeyError::Parse(format!("base64 decode failed: {err}")))?;

        MikeyMessage::from_bytes(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use sdp_types::{KeyMgmt, KeyMgmtMethod};

    use super::KeyMgmtExt;
    use crate::session::Session;
    use crate::tests::{SESSION_HEADER, init};

    #[test]
    fn connection_variants() {
        init();
        // RFC 8866 section 5.7 also allows an FQDN as unicast-address, which
        // check_connection() accepts even though it is not a valid IP address.
        let sdp = Session::parse(&format!(
            "{SESSION_HEADER}m=audio 0 RTP/AVP 96\r\nc=IN IP4 example.com\r\n"
        ))
        .unwrap();
        assert_eq!(
            sdp.media()[0].connection().unwrap().connection_address,
            "example.com"
        );

        // A syntactically invalid multicast suffix is still rejected.
        for address in ["239.255.42.42/x", "239.255.42.42/256"] {
            let sdp = Session::parse(&format!(
                "{SESSION_HEADER}m=audio 0 RTP/AVP 96\r\nc=IN IP4 {address}\r\n"
            ))
            .unwrap();
            assert!(sdp.media()[0].connections().is_empty(), "{address}");
        }
    }

    #[test]
    fn key_mgmt_mikey() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let msg = mykey::MikeyMessage::new_psk_init(7, 0xCAFEBABE, &[0x2au8; 16], b"shared-secret")
            .unwrap();
        let km = KeyMgmt::with_data(KeyMgmtMethod::Mikey, STANDARD.encode(msg.to_bytes()));
        assert_eq!(km.mikey().unwrap().to_bytes(), msg.to_bytes());

        for km in [
            KeyMgmt::with_data(
                KeyMgmtMethod::Other("internal".into()),
                STANDARD.encode(msg.to_bytes()),
            ),
            KeyMgmt::new(KeyMgmtMethod::Mikey),
            KeyMgmt::with_data(KeyMgmtMethod::Mikey, "not base64!"),
        ] {
            assert!(km.mikey().is_err(), "{km}");
        }
    }
}
