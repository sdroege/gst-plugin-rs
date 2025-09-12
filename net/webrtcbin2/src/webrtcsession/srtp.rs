// SPDX-License-Identifier: MPL-2.0

use crate::webrtcsession::dtls::KeyMaterial;

// keep in sync with values in gstsrtp
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
pub enum SrtpCipher {
    Null = 0,
    Aes128Icm,
    Aes256Icm,
    Aes128Gcm,
    Aes256Gcm,
}

impl SrtpCipher {
    pub fn gst_enum_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Aes128Icm => "aes-128-icm",
            Self::Aes256Icm => "aes-256-icm",
            Self::Aes128Gcm => "aes-128-gcm",
            Self::Aes256Gcm => "aes-256-gcm",
        }
    }
}

// keep in sync with the values in gstrtsp
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
pub enum SrtpAuth {
    Null = 0,
    HmacSha1_32,
    HmacSha1_80,
}

impl SrtpAuth {
    pub fn gst_enum_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::HmacSha1_32 => "hmac-sha1-32",
            Self::HmacSha1_80 => "hmac-sha1-80",
        }
    }
}

#[derive(Debug)]
pub struct SrtpKeyMaterial {
    auth: SrtpAuth,
    cipher: SrtpCipher,
    encode_key: Vec<u8>,
    decode_key: Vec<u8>,
}

impl SrtpKeyMaterial {
    pub fn from_dtls(material: &KeyMaterial, is_client: bool) -> Self {
        let mut client_key = material.client_key().to_vec();
        client_key.extend_from_slice(material.client_salt());
        let mut server_key = material.server_key().to_vec();
        server_key.extend_from_slice(material.server_salt());
        let (encode_key, decode_key) = if is_client {
            (client_key, server_key)
        } else {
            (server_key, client_key)
        };
        SrtpKeyMaterial {
            auth: material.srtp_auth(),
            cipher: material.srtp_cipher(),
            encode_key,
            decode_key,
        }
    }

    pub fn auth(&self) -> SrtpAuth {
        self.auth
    }

    pub fn cipher(&self) -> SrtpCipher {
        self.cipher
    }

    pub fn encode_key(&self) -> &[u8] {
        &self.encode_key
    }

    pub fn decode_key(&self) -> &[u8] {
        &self.decode_key
    }
}
