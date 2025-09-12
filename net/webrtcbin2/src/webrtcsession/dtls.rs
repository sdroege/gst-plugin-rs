// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::LazyLock,
    time::{Duration, Instant},
};

use openssl::{
    asn1::{Asn1Integer, Asn1Time, Asn1Type},
    bn::BigNum,
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, PKeyRef, Private},
    srtp::SrtpProfileId,
    ssl::{
        HandshakeError, MidHandshakeSslStream, Ssl, SslContext, SslOptions, SslStream,
        SslVerifyMode,
    },
    x509::{X509, X509Name, X509Ref},
};
use rand::distr::SampleString;

use crate::webrtcsession::srtp::{SrtpAuth, SrtpCipher};

const DTLS_SRTP_KEY_NAME: &str = "EXTRACTOR-dtls_srtp";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc2dtls",
        gst::DebugColorFlags::empty(),
        Some("WebRTC2DTLS"),
    )
});

#[derive(Debug)]
enum HandshakeState {
    Init(Ssl, Bio),
    Handshaking(MidHandshakeSslStream<Bio>),
    Complete(SslStream<Bio>),
    Empty,
}

impl HandshakeState {
    fn complete(&mut self) -> Result<&mut SslStream<Bio>, std::io::Error> {
        if let Self::Complete(stream) = self {
            return Ok(stream);
        }

        let mut state = Self::Empty;
        core::mem::swap(self, &mut state);

        let ret = match state {
            Self::Init(ssl, io) => {
                gst::trace!(CAT, "Starting DTLS connection");
                let client = io.client.expect("set_client must be called");
                if client {
                    ssl.connect(io)
                } else {
                    ssl.accept(io)
                }
            }
            Self::Handshaking(mid) => mid.handshake(),
            Self::Complete(_) | Self::Empty => unreachable!(),
        };

        match ret {
            Ok(mut stream) => {
                gst::info!(
                    CAT,
                    "Succesful DTLS connection {} {}",
                    stream.ssl().version_str(),
                    stream.ssl().current_cipher().unwrap().name()
                );
                if let Ok(keymat) = export_srtp_key_material(&mut stream) {
                    stream.get_mut().set_key_material(keymat);
                }
                *self = Self::Complete(stream);
                self.complete()
            }
            Err(HandshakeError::WouldBlock(mid)) => {
                *self = Self::Handshaking(mid);
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "Would block",
                ))
            }
            Err(HandshakeError::SetupFailure(e)) => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
            }
            Err(HandshakeError::Failure(mid)) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                mid.into_error(),
            )),
        }
    }

    fn inner_mut(&mut self) -> &mut Bio {
        match self {
            Self::Init(_, io) => io,
            Self::Handshaking(mid) => mid.get_mut(),
            Self::Complete(stream) => stream.get_mut(),
            Self::Empty => unreachable!(),
        }
    }

    fn inner(&self) -> &Bio {
        match self {
            Self::Init(_, io) => io,
            Self::Handshaking(mid) => mid.get_ref(),
            Self::Complete(stream) => stream.get_ref(),
            Self::Empty => unreachable!(),
        }
    }

    fn peer_cert_hash(&self, algo: MessageDigest) -> Option<Vec<u8>> {
        let Self::Complete(stream) = self else {
            return None;
        };
        let x509 = stream.ssl().peer_certificate()?;
        x509.digest(algo).ok().map(|b| b.to_vec())
    }
}

fn keying_material_length(key: &openssl::srtp::SrtpProtectionProfileRef) -> Option<usize> {
    match key.id() {
        openssl::srtp::SrtpProfileId::SRTP_AES128_CM_SHA1_80 => Some(2 * (16 + 14)),
        openssl::srtp::SrtpProfileId::SRTP_AEAD_AES_128_GCM => Some(2 * (16 + 12)),
        _ => None,
    }
}

fn key_size(key_type: openssl::srtp::SrtpProfileId) -> usize {
    match key_type {
        openssl::srtp::SrtpProfileId::SRTP_AES128_CM_SHA1_80 => 16,
        openssl::srtp::SrtpProfileId::SRTP_AES128_CM_SHA1_32 => 16,
        openssl::srtp::SrtpProfileId::SRTP_AEAD_AES_128_GCM => 16,
        _ => unreachable!(),
    }
}

fn salt_size(key_type: openssl::srtp::SrtpProfileId) -> usize {
    match key_type {
        openssl::srtp::SrtpProfileId::SRTP_AES128_CM_SHA1_80 => 14,
        openssl::srtp::SrtpProfileId::SRTP_AES128_CM_SHA1_32 => 14,
        openssl::srtp::SrtpProfileId::SRTP_AEAD_AES_128_GCM => 12,
        _ => unreachable!(),
    }
}

fn export_srtp_key_material(stream: &mut SslStream<Bio>) -> Result<KeyMaterial, std::io::Error> {
    let srtp_profile = stream
        .ssl()
        .selected_srtp_profile()
        .ok_or_else(|| std::io::Error::other("Failed to negotiate SRTP profile"))?;

    let mat_len = keying_material_length(srtp_profile).ok_or_else(|| {
        std::io::Error::other(format!("Unknown SRTP profile {}", srtp_profile.name()))
    })?;

    let mut keymat = vec![0_u8; mat_len];
    stream
        .ssl()
        .export_keying_material(&mut keymat, DTLS_SRTP_KEY_NAME, None)?;
    gst::debug!(CAT, "negotiated SRTP: {}", srtp_profile.name());

    Ok(KeyMaterial {
        method: srtp_profile.id(),
        key: keymat,
    })
}

pub struct KeyMaterial {
    method: openssl::srtp::SrtpProfileId,
    key: Vec<u8>,
}

impl KeyMaterial {
    pub fn client_key(&self) -> &[u8] {
        let keysize = key_size(self.method);
        &self.key[..keysize]
    }

    pub fn client_salt(&self) -> &[u8] {
        let keysize = key_size(self.method);
        let saltsize = salt_size(self.method);
        &self.key[2 * keysize..][..saltsize]
    }

    pub fn server_key(&self) -> &[u8] {
        let keysize = key_size(self.method);
        &self.key[keysize..][..keysize]
    }

    pub fn server_salt(&self) -> &[u8] {
        let keysize = key_size(self.method);
        let saltsize = salt_size(self.method);
        &self.key[2 * keysize + saltsize..][..saltsize]
    }

    pub fn srtp_auth(&self) -> SrtpAuth {
        match self.method {
            SrtpProfileId::SRTP_AES128_CM_SHA1_80
            | SrtpProfileId::SRTP_AES128_F8_SHA1_80
            | SrtpProfileId::SRTP_NULL_SHA1_80 => SrtpAuth::HmacSha1_80,
            SrtpProfileId::SRTP_AES128_CM_SHA1_32
            | SrtpProfileId::SRTP_AES128_F8_SHA1_32
            | SrtpProfileId::SRTP_NULL_SHA1_32 => SrtpAuth::HmacSha1_32,
            SrtpProfileId::SRTP_AEAD_AES_256_GCM | SrtpProfileId::SRTP_AEAD_AES_128_GCM => {
                unreachable!()
            }
            _ => unreachable!(),
        }
    }
    pub fn srtp_cipher(&self) -> SrtpCipher {
        match self.method {
            SrtpProfileId::SRTP_AES128_CM_SHA1_80
            | SrtpProfileId::SRTP_AES128_F8_SHA1_80
            | SrtpProfileId::SRTP_AES128_CM_SHA1_32
            | SrtpProfileId::SRTP_AES128_F8_SHA1_32 => SrtpCipher::Aes128Icm,
            SrtpProfileId::SRTP_NULL_SHA1_32 => SrtpCipher::Null,
            SrtpProfileId::SRTP_AEAD_AES_256_GCM => SrtpCipher::Aes256Gcm,
            SrtpProfileId::SRTP_AEAD_AES_128_GCM => SrtpCipher::Aes128Gcm,
            _ => unreachable!(),
        }
    }
}

impl core::fmt::Debug for KeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyMaterial").field("key", &"...").finish()
    }
}

#[derive(Debug, Default)]
struct Bio {
    client: Option<bool>,
    incoming: Vec<u8>,
    outgoing: VecDeque<Vec<u8>>,
    keymat: Option<KeyMaterial>,
}

impl Bio {
    pub fn set_client(&mut self, client: bool) {
        if self.client.is_some_and(|existing| existing != client) {
            panic!("set_client should only be called once");
        }
        gst::debug!(CAT, "setting client-ness to {client}");
        self.client = Some(client);
    }
    fn push_incoming(&mut self, buf: &[u8]) {
        self.incoming.extend_from_slice(buf)
    }

    fn pop_outgoing(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
    }

    fn set_key_material(&mut self, key: KeyMaterial) {
        self.keymat = Some(key);
    }
}

impl std::io::Write for Bio {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        gst::trace!(CAT, "outgoing {} bytes", buf.len());
        self.outgoing.push_back(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Read for Bio {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.incoming.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Would Block",
            ));
        }

        let n = buf.len().min(self.incoming.len());

        gst::trace!(CAT, "returning {n} bytes");
        buf[..n].copy_from_slice(&self.incoming[..n]);
        if n == self.incoming.len() {
            self.incoming.truncate(0);
        } else {
            self.incoming.drain(..n);
        }

        Ok(n)
    }
}

pub fn generate_cert() -> (PKey<Private>, X509) {
    let pkey = openssl::pkey::PKey::ec_gen("prime256v1").unwrap();

    let mut x509 = X509::builder().unwrap();
    x509.set_version(2).unwrap(); // V3 (0-indexed)

    // random 64 bits as serial
    let mut serial = [0_u8; 8];
    openssl::rand::rand_bytes(&mut serial).unwrap();
    let serial = BigNum::from_slice(&serial).unwrap();
    let asn_serial = Asn1Integer::from_bn(&serial).unwrap();
    x509.set_serial_number(&asn_serial).unwrap();

    let common = rand::distr::Alphanumeric.sample_string(&mut rand::rng(), 16);
    let mut cn = X509Name::builder().unwrap();
    cn.append_entry_by_nid_with_type(Nid::COMMONNAME, &common, Asn1Type::UTF8STRING)
        .unwrap();
    let cn = cn.build();
    x509.set_issuer_name(&cn).unwrap();
    x509.set_subject_name(&cn).unwrap();

    x509.set_not_before(
        &Asn1Time::from_unix(
            (std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                - std::time::Duration::from_secs(600))
            .as_secs() as _,
        )
        .unwrap(),
    )
    .unwrap();

    x509.set_not_after(&Asn1Time::days_from_now(365).unwrap())
        .unwrap();
    x509.set_pubkey(&pkey).unwrap();

    x509.sign(&pkey, MessageDigest::sha256()).unwrap();
    let x509 = x509.build();

    (pkey, x509)
}

fn create_ssl_context(pkey: &PKeyRef<Private>, cert: &X509Ref) -> SslContext {
    let mut builder = openssl::ssl::SslContext::builder(openssl::ssl::SslMethod::dtls()).unwrap();
    // TODO: more SRTP algos
    builder
        .set_tlsext_use_srtp("SRTP_AES128_CM_SHA1_80")
        .unwrap();
    builder
        .set_cipher_list("ALL:!ADH:!LOW:!EXP:!MD5:@STRENGTH")
        .unwrap();
    let mode = SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT;
    builder.set_verify_callback(mode, |_ok, _ctx| true);
    builder.set_private_key(pkey).unwrap();
    builder.set_certificate(cert).unwrap();
    let mut options = SslOptions::empty();
    options |= SslOptions::NO_QUERY_MTU;
    // require DTLS >= 1.2
    options |= SslOptions::NO_DTLSV1;
    builder.set_options(options);

    builder.build()
}

pub struct TlsImpl {
    cert: X509,
    _context: SslContext,
    tls: HandshakeState,
    pending_events: VecDeque<DtlsEvent>,
}

impl core::fmt::Debug for TlsImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsImpl").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum DtlsEvent {
    Connected,
    KeyingMaterial(KeyMaterial),
}

#[derive(Debug)]
pub enum DtlsPollRet {
    WaitUntil(Instant),
    Closed,
}

impl Default for TlsImpl {
    fn default() -> Self {
        let (key, cert) = generate_cert();
        let context = create_ssl_context(&key, &cert);
        let mut ssl = Ssl::new(&context).expect("Failed to create Ssl");
        ssl.set_mtu(1400).unwrap();
        let tls = HandshakeState::Init(ssl, Bio::default());

        Self {
            cert,
            _context: context,
            tls,
            pending_events: Default::default(),
        }
    }
}

impl TlsImpl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_client(&mut self, client: bool) {
        self.tls.inner_mut().set_client(client);
    }

    pub fn is_client(&self) -> Option<bool> {
        self.tls.inner().client
    }

    pub fn handle_incoming(&mut self, incoming: &[u8]) -> Result<Option<Vec<u8>>, std::io::Error> {
        gst::trace!(CAT, "receive {} bytes", incoming.len());
        self.tls.inner_mut().push_incoming(incoming);

        let tls = match self.tls.complete() {
            Ok(tls) => tls,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    gst::trace!(CAT, "Would Block");
                    return Ok(None);
                }
                gst::warning!(CAT, "DTLS produced error: {e}");
                return Err(e);
            }
        };

        let mut out = vec![0; 2000];
        let len = match tls.read(&mut out) {
            Ok(len) => len,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    gst::trace!(CAT, "Would Block: {:?}", self);
                    return Ok(None);
                }
                gst::warning!(CAT, "DTLS produced error: {e}");
                return Err(e);
            }
        };
        out.resize(len, 0);

        gst::trace!(CAT, "have {} bytes of app data", out.len());
        Ok(Some(out))
    }

    pub fn send(&mut self, data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        let tls = self.tls.complete()?;
        tls.write_all(data).unwrap();
        self.poll_transmit().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NetworkUnreachable, "Failed to send")
        })
    }

    pub fn poll(&mut self, now: Instant) -> DtlsPollRet {
        match self.tls.complete() {
            Ok(tls) => {
                if let Some(key) = tls.get_mut().keymat.take() {
                    self.pending_events.push_back(DtlsEvent::Connected);
                    self.pending_events
                        .push_back(DtlsEvent::KeyingMaterial(key));
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return DtlsPollRet::WaitUntil(now + Duration::from_millis(500));
                } else {
                    return DtlsPollRet::Closed;
                }
            }
        }
        if !self.tls.inner_mut().outgoing.is_empty() || !self.pending_events.is_empty() {
            return DtlsPollRet::WaitUntil(now);
        }
        DtlsPollRet::WaitUntil(now + Duration::from_secs(600))
    }

    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.tls.inner_mut().pop_outgoing()
    }

    pub fn poll_event(&mut self) -> Option<DtlsEvent> {
        self.pending_events.pop_front()
    }

    pub fn local_fingerprint(&self) -> Vec<u8> {
        self.cert.digest(MessageDigest::sha256()).unwrap().to_vec()
    }

    pub fn remote_fingerprint(&self) -> Option<Vec<u8>> {
        self.tls.peer_cert_hash(MessageDigest::sha256())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_io(local: &mut TlsImpl, remote: &mut TlsImpl, now: Instant) -> Option<Vec<u8>> {
        loop {
            let mut handled = false;
            let ret = local.poll(now);
            gst::trace!(CAT, "local ret {ret:?}");
            let ret = remote.poll(now);
            gst::trace!(CAT, "remote ret {ret:?}");
            if let Some(data) = local.poll_transmit() {
                handled = true;
                let reply = remote.handle_incoming(&data).unwrap();
                if reply.is_some() {
                    return reply;
                }
            }
            if let Some(data) = remote.poll_transmit() {
                handled = true;
                let reply = local.handle_incoming(&data).unwrap();
                if reply.is_some() {
                    return reply;
                }
            }
            if !handled {
                return None;
            }
        }
    }

    #[test]
    fn dtls_end_to_end() {
        gst::init().unwrap();

        let mut local = TlsImpl::new();
        local.set_client(true);
        let mut remote = TlsImpl::new();
        remote.set_client(false);

        let now = std::time::Instant::now();

        let data = complete_io(&mut local, &mut remote, now);
        assert!(data.is_none());

        assert!(matches!(local.poll_event().unwrap(), DtlsEvent::Connected));
        let DtlsEvent::KeyingMaterial(material) = local.poll_event().unwrap() else {
            unreachable!();
        };
        assert!(!material.key.is_empty());

        assert!(matches!(remote.poll_event().unwrap(), DtlsEvent::Connected));
        let DtlsEvent::KeyingMaterial(material) = remote.poll_event().unwrap() else {
            unreachable!();
        };
        assert!(!material.key.is_empty());

        let sent = [7; 5];
        let transmit = local.send(&sent).unwrap();
        let received = remote.handle_incoming(&transmit).unwrap().unwrap();
        assert_eq!(&received, sent.as_slice());

        let sent = [17; 9];
        let transmit = remote.send(&sent).unwrap();
        let received = local.handle_incoming(&transmit).unwrap().unwrap();
        assert_eq!(&received, sent.as_slice());
    }
}
