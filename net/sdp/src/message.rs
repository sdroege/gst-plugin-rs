// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Parsing and serialization of SAP messages as defined in RFC 2974.

use std::{
    borrow::Cow,
    io::{Read as _, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use anyhow::{Context, anyhow, bail};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use fnv_rs::FnvHasher as _;

use sdp_types_gst::Session;

// Header layout, see RFC 2974:
//
//   Byte 0:    0b001 (version) | A | R | T | E | C
//   Byte 1:    authentication length (32-bit words after the source)
//   Bytes 2-3: message identifier hash
//   Bytes 4..: originating source (4 or 16 bytes)
/// Mask for the version field
const FLAG_VERSION_MASK: u8 = 0b1110_0000;
/// The version field, which must be set to 001
const FLAG_VERSION: u8 = 0b0010_0000;
/// 1 = 128-bit IPv6 originating source, otherwise 32-bit IPv4
const FLAG_IPV6_SOURCE: u8 = 0b0001_0000;
const FLAG_DELETION: u8 = 0b0000_0100;
const FLAG_ENCRYPTED: u8 = 0b0000_0010;
/// The payload type and payload are zlib compressed
const FLAG_COMPRESSED: u8 = 0b0000_0001;

/// Minimum packet size with a 32-bit originating source address
const MIN_PACKET_SIZE_V4: usize = 8;
/// Minimum packet size with a 128-bit originating source address
const MIN_PACKET_SIZE_V6: usize = 20;

const SDP_PAYLOAD_TYPE: &str = "application/sdp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Announcement,
    Deletion,
}

#[derive(Clone, Debug)]
pub struct SapMessage {
    pub message_type: MessageType,
    pub msg_id_hash: u16,
    pub originating_source: IpAddr,
    /// The session description. For deletions this usually only contains
    /// the origin of the deleted session.
    pub session: Session,
}

impl SapMessage {
    pub fn announcement(session: &Session, originating_source: IpAddr) -> Self {
        Self {
            message_type: MessageType::Announcement,
            msg_id_hash: msg_id_hash(session),
            originating_source,
            session: session.clone(),
        }
    }

    pub fn deletion(session: &Session, originating_source: IpAddr) -> Self {
        let origin = session.origin().clone();
        Self {
            message_type: MessageType::Deletion,
            msg_id_hash: msg_id_hash(session),
            originating_source,
            session: Session::builder().origin(origin).build(),
        }
    }

    pub fn serialize(&self, compress: bool) -> Result<Vec<u8>, anyhow::Error> {
        let session = &self.session;

        let mut flags = FLAG_VERSION;
        if self.originating_source.is_ipv6() {
            flags |= FLAG_IPV6_SOURCE;
        }
        if self.message_type == MessageType::Deletion {
            flags |= FLAG_DELETION;
        }
        if compress {
            flags |= FLAG_COMPRESSED;
        }

        let mut packet = Vec::new();
        packet.push(flags);
        packet.push(0u8);
        packet.extend_from_slice(&self.msg_id_hash.to_be_bytes());
        match self.originating_source {
            IpAddr::V4(addr) => packet.extend_from_slice(&addr.octets()),
            IpAddr::V6(addr) => packet.extend_from_slice(&addr.octets()),
        }

        if compress {
            let mut encoder = ZlibEncoder::new(&mut packet, Compression::default());
            encoder
                .write_all(SDP_PAYLOAD_TYPE.as_bytes())
                .context("Failed compressing SAP message")?;
            encoder
                .write_all(b"\0")
                .context("Failed compressing SAP message")?;
            self.write_payload(session, &mut encoder)?;
            encoder.finish().context("Failed compressing SAP message")?;
        } else {
            packet.extend_from_slice(SDP_PAYLOAD_TYPE.as_bytes());
            packet.push(0);
            self.write_payload(session, &mut packet)?;
        }

        Ok(packet)
    }

    fn write_payload(
        &self,
        session: &Session,
        writer: &mut dyn Write,
    ) -> Result<(), anyhow::Error> {
        match self.message_type {
            MessageType::Announcement => writer
                .write_all(session.to_sdp_string().as_bytes())
                .context("Failed writing session description")?,
            MessageType::Deletion => {
                let origin = session.origin();
                let username = origin.username.as_deref().unwrap_or("-");
                let line = format!(
                    "o={username} {} {} {} {} {}\r\n",
                    origin.sess_id,
                    origin.sess_version,
                    origin.nettype,
                    origin.addrtype,
                    origin.unicast_address
                );
                writer
                    .write_all(line.as_bytes())
                    .context("Failed writing origin line")?;
            }
        }

        Ok(())
    }

    /// Parses a SAP message, accepting both SAPv1 and SAPv2 packets
    pub fn parse(packet: &[u8]) -> Result<SapMessage, anyhow::Error> {
        if packet.len() < 2 {
            bail!("SAP packet too short");
        }

        let flags = packet[0];
        if (flags & FLAG_VERSION_MASK) != FLAG_VERSION {
            bail!("Unsupported SAP version");
        }
        if (flags & FLAG_ENCRYPTED) != 0 {
            bail!("Encrypted SAP messages are not supported");
        }

        let auth_len = u32::from(packet[1]);
        if auth_len != 0 {
            bail!("Authenticated SAP messages are not supported");
        }

        let message_type = if (flags & FLAG_DELETION) != 0 {
            MessageType::Deletion
        } else {
            MessageType::Announcement
        };

        let min_packet_size = if (flags & FLAG_IPV6_SOURCE) != 0 {
            MIN_PACKET_SIZE_V6
        } else {
            MIN_PACKET_SIZE_V4
        };
        if packet.len() < min_packet_size {
            bail!("SAP packet too short");
        }

        let msg_id_hash = u16::from_be_bytes([packet[2], packet[3]]);
        let originating_source = if (flags & FLAG_IPV6_SOURCE) != 0 {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&packet[4..20]);
            IpAddr::V6(Ipv6Addr::from(bytes))
        } else {
            IpAddr::V4(Ipv4Addr::from([packet[4], packet[5], packet[6], packet[7]]))
        };

        let body = if (flags & FLAG_COMPRESSED) != 0 {
            let mut decoder = ZlibDecoder::new(&packet[min_packet_size..]);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .context("Failed decompressing SAP message")?;
            Cow::Owned(decompressed)
        } else {
            Cow::Borrowed(&packet[min_packet_size..])
        };

        let (payload_type, payload) = if body.starts_with(b"v=") {
            // No payload type field, the payload starts with the SDP version line
            (None, body.as_ref())
        } else if let Some(pos) = body.iter().position(|&byte| byte == 0) {
            let payload_type = String::from_utf8(body[..pos].to_vec())
                .map_err(|err| anyhow!("Invalid payload type encoding: {err}"))?;
            (Some(payload_type), &body[pos + 1..])
        } else {
            // No NUL byte, so no payload type field
            (None, body.as_ref())
        };
        if let Some(payload_type) = &payload_type
            && payload_type != SDP_PAYLOAD_TYPE
        {
            bail!("Unsupported SAP payload type {payload_type:?}");
        }

        let payload = std::str::from_utf8(payload)
            .map_err(|err| anyhow!("SAP payload is not valid UTF-8: {err}"))?;
        let session = match message_type {
            MessageType::Announcement => {
                Session::parse(payload).map_err(|err| anyhow!("Failed parsing SDP: {err}"))?
            }
            MessageType::Deletion => {
                if payload.is_empty() {
                    bail!("Session deletion without origin field");
                }
                // According to RFC 2974 the deletion payload is the SDP "o=" line without the "v="
                // version line. Most senders repeat the full announcement payload though.
                //
                // We need to re-add the "v=0" line if missing to be able to parse the SDP.
                let sdp = if payload.starts_with("v=") {
                    Cow::Borrowed(payload)
                } else {
                    Cow::Owned(format!("v=0\r\n{payload}"))
                };
                Session::parse(&sdp).map_err(|err| anyhow!("Failed parsing SDP: {err}"))?
            }
        };

        Ok(SapMessage {
            message_type,
            msg_id_hash,
            originating_source,
            session,
        })
    }
}

/// The message identifier hash.
///
/// RFC 2974 does not specify how it is calculated, so use a FNV32 hash
/// over the serialized SDP.
///
/// RFC 2974 §6: announcers SHOULD NOT set the message identifier hash to
/// zero because listeners may discard such packets, so zero is mapped to
/// one instead.
pub fn msg_id_hash(sdp: &Session) -> u16 {
    let hash = fnv_rs::Fnv32::hash(sdp.to_sdp_string().as_bytes());
    let bytes = hash.as_bytes();
    let hash = u16::from_be_bytes([bytes[2], bytes[3]]);
    if hash == 0 { 1 } else { hash }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdp_types::{Attribute, Connection, Media, MediaType, Time, TransportProto};

    /// The example session from RFC 8866, section 2.
    fn test_session() -> Session {
        let mut origin = sdp_types::Origin::with_ip_addr(
            "3724394400",
            3724394405,
            "198.51.100.1".parse::<IpAddr>().unwrap(),
        );
        origin.set_username("jdoe");
        let mut session = sdp_types::Session::new(origin, "Call to John Smith");
        session.set_session_description("SDP Offer #1");
        session.set_uri("http://www.jdoe.example.com/home.html");
        session
            .emails
            .push("Jane Doe <jane@jdoe.example.com>".to_string());
        session.phones.push("+1 617 555-6011".to_string());
        session.connection = Some(Connection::from_ip_addr(
            "198.51.100.1".parse::<IpAddr>().unwrap(),
        ));
        session.add_time(Time::new(0, 0));
        session.add_media(Media::new(
            MediaType::Audio,
            49170,
            TransportProto::RtpAvp,
            "0",
        ));
        session.add_media(Media::new(
            MediaType::Audio,
            49180,
            TransportProto::RtpAvp,
            "0",
        ));
        let mut media = Media::new(MediaType::Video, 51372, TransportProto::RtpAvp, "99");
        media.add_connection(Connection::from_ip_addr(
            "2001:db8::2".parse::<IpAddr>().unwrap(),
        ));
        media.add_attribute(Attribute::with_value("rtpmap", "99 h263-1998/90000"));
        session.add_media(media);

        let mut sdp_text = Vec::new();
        session
            .write(&mut sdp_text)
            .expect("writing to a Vec cannot fail");
        Session::parse(std::str::from_utf8(&sdp_text).unwrap()).expect("test session is valid SDP")
    }

    #[test]
    fn serialize_and_parse_roundtrip() {
        let sdp = test_session();
        let source = IpAddr::V4(Ipv4Addr::new(10, 47, 16, 5));

        for (compress, message_type) in [
            (false, MessageType::Announcement),
            (true, MessageType::Announcement),
            (false, MessageType::Deletion),
            (true, MessageType::Deletion),
        ] {
            let message = match message_type {
                MessageType::Announcement => SapMessage::announcement(&sdp, source),
                MessageType::Deletion => SapMessage::deletion(&sdp, source),
            };
            let packet = message.serialize(compress).expect("serialization failed");
            assert_eq!((packet[0] & FLAG_COMPRESSED) != 0, compress);

            let parsed = SapMessage::parse(&packet).expect("parsing failed");
            assert_eq!(parsed.message_type, message_type);
            assert_eq!(parsed.msg_id_hash, msg_id_hash(&sdp));
            assert_eq!(parsed.originating_source, source);

            let parsed_session = &parsed.session;
            assert_eq!(parsed_session.origin(), sdp.origin());
            if message_type == MessageType::Announcement {
                assert_eq!(parsed_session, &sdp);
            } else {
                // Deletion payloads only contain the origin line
                assert!(parsed_session.name().is_empty());
                assert!(parsed_session.media().is_empty());
            }
        }

        let source = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1));

        let message = SapMessage::announcement(&sdp, source);
        let packet = message.serialize(true).expect("serialization failed");

        let parsed = SapMessage::parse(&packet).expect("parsing failed");
        assert_eq!(parsed.originating_source, source);
        assert_eq!(parsed.session, sdp);
    }

    #[test]
    fn parse_sapv1_packet() {
        // SAPv1 packets have no payload type field
        let sdp = test_session();
        let source = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let sdp_text = sdp.to_sdp_string();

        let mut packet = vec![FLAG_VERSION, 0, 0xab, 0xcd];
        packet.extend_from_slice(&[192, 168, 1, 1]);
        packet.extend_from_slice(sdp_text.as_bytes());

        let parsed = SapMessage::parse(&packet).expect("parsing failed");
        assert_eq!(parsed.message_type, MessageType::Announcement);
        assert_eq!(parsed.msg_id_hash, 0xabcd);
        assert_eq!(parsed.originating_source, source);
        assert_eq!(parsed.session, sdp);
    }

    #[test]
    fn parse_invalid_packets() {
        let sdp = test_session();
        let source = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let packet = SapMessage::announcement(&sdp, source)
            .serialize(false)
            .expect("serialization failed");

        let mut invalid = packet.clone();
        invalid[0] &= !FLAG_VERSION;
        assert!(SapMessage::parse(&invalid).is_err(), "SAPv0");

        let mut invalid = packet.clone();
        invalid[0] |= FLAG_ENCRYPTED;
        assert!(SapMessage::parse(&invalid).is_err(), "encrypted");

        let mut invalid = packet.clone();
        invalid[1] = 1;
        assert!(SapMessage::parse(&invalid).is_err(), "authenticated");

        assert!(SapMessage::parse(&[]).is_err(), "empty");
        assert!(SapMessage::parse(&packet[..5]).is_err(), "truncated header");

        let mut invalid = packet.clone();
        invalid[0] |= FLAG_DELETION;
        invalid.truncate(8);
        assert!(
            SapMessage::parse(&invalid).is_err(),
            "deletion without origin"
        );

        let mut packet = vec![FLAG_VERSION, 0, 0, 0, 10, 0, 0, 1];
        packet.extend_from_slice(b"text/plain\0v=0\r\n");
        assert!(SapMessage::parse(&packet).is_err());

        // Invalid SDP is rejected completely
        let mut packet = vec![FLAG_VERSION, 0, 0, 0, 10, 0, 0, 1];
        packet.extend_from_slice(b"application/sdp\0v=0\r\nnot-a-field\r\n");
        assert!(SapMessage::parse(&packet).is_err(), "invalid SDP");
    }
}
