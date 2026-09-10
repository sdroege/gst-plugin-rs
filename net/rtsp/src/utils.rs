use crate::rtspsrc::RtspSrc2TlsValidationFlags;
use gst::prelude::*;
use rustls::crypto::ring::sign::any_supported_type;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{self, ClientConfig},
};
use x509_parser::{asn1_rs, prelude::*};

fn read_certs_from_file<P: AsRef<Path>>(
    certificate_file: &P,
) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, Box<dyn std::error::Error>> {
    use rustls_pki_types::pem::PemObject;

    /*
     * NOTE:
     *
     * Certificate file here should correspond to fullchain.pem where
     * fullchain.pem = cert.pem + chain.pem.
     * fullchain.pem DOES NOT include a CA's Root Certificates.
     *
     * One typically uses chain.pem (or the first certificate in it) when asked
     * for a CA bundle or CA certificate.
     *
     * One typically uses fullchain.pem when asked for the entire certificate
     * chain in a single file. For example, this is the case of modern day
     * Apache and nginx.
     */
    rustls_pki_types::CertificateDer::pem_file_iter(certificate_file)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn read_private_key_from_file<P: AsRef<Path>>(
    private_key_file: &P,
) -> Result<rustls_pki_types::PrivateKeyDer<'static>, Box<dyn std::error::Error>> {
    use rustls_pki_types::pem::PemObject;

    rustls_pki_types::PrivateKeyDer::from_pem_file(private_key_file).map_err(Into::into)
}

fn client_config(
    rtsp_src: gst::glib::WeakRef<super::rtspsrc::RtspSrc>,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
    tls_validation_flags: RtspSrc2TlsValidationFlags,
) -> Result<ClientConfig, Box<dyn std::error::Error>> {
    use rustls_platform_verifier::BuilderVerifierExt;

    let resolver = ClientCertResolver::new(rtsp_src, certificate_file, private_key_file);
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    if tls_validation_flags == RtspSrc2TlsValidationFlags::ValidateAll {
        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_platform_verifier()
            .unwrap()
            .with_client_cert_resolver(Arc::new(resolver));
        return Ok(builder);
    }

    let platform_verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> = Arc::new(
        rustls_platform_verifier::Verifier::new(provider.clone())
            .map_err(|e| format!("Failed to create platform verifier: {e}"))?,
    );

    let server_cert_verifier = RtspServerCertVerifier::new(tls_validation_flags, platform_verifier);

    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(server_cert_verifier)
        .with_client_cert_resolver(Arc::new(resolver));

    Ok(builder)
}

pub fn create_tls_connector(
    rtsp_src: gst::glib::WeakRef<super::rtspsrc::RtspSrc>,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
    tls_validation: RtspSrc2TlsValidationFlags,
) -> Result<TlsConnector, Box<dyn std::error::Error>> {
    let config = client_config(rtsp_src, certificate_file, private_key_file, tls_validation)?;
    Ok(TlsConnector::from(Arc::new(config)))
}

#[derive(Debug)]
struct ClientCert {
    // Path to certificate chain for the private key file in PEM format
    certificate_file: Option<PathBuf>,
    // Path to a PKCS1, PKCS8 or SEC1 private key file in PEM format
    private_key_file: Option<PathBuf>,
}

impl From<gst::Structure> for ClientCert {
    fn from(s: gst::Structure) -> Self {
        Self {
            certificate_file: s.get::<String>("certificate-file").ok().map(Into::into),
            private_key_file: s.get::<String>("private-key-file").ok().map(Into::into),
        }
    }
}

#[derive(Debug)]
struct ClientCertResolver {
    rtsp_src: gst::glib::WeakRef<super::rtspsrc::RtspSrc>,
    cert: ClientCert,
}

impl ClientCertResolver {
    fn new(
        rtsp_src: gst::glib::WeakRef<super::rtspsrc::RtspSrc>,
        certificate_file: Option<PathBuf>,
        private_key_file: Option<PathBuf>,
    ) -> Self {
        Self {
            rtsp_src,
            cert: ClientCert {
                certificate_file,
                private_key_file,
            },
        }
    }

    fn get_cert_files(&self) -> Option<(PathBuf, PathBuf)> {
        match (&self.cert.certificate_file, &self.cert.private_key_file) {
            (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
            _ => None,
        }
    }

    fn get_cert_files_from_rtsp_src(&self) -> Option<(PathBuf, PathBuf)> {
        self.rtsp_src
            .upgrade()
            .and_then(|obj| obj.emit_by_name::<Option<gst::Structure>>("tls-client-auth", &[]))
            .map(|s| {
                let client_cert = ClientCert::from(s);
                (client_cert.certificate_file, client_cert.private_key_file)
            })
            .and_then(|(cert_file, key_file)| cert_file.zip(key_file))
    }
}

impl rustls::client::ResolvesClientCert for ClientCertResolver {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[rustls::SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let (certificate_file, private_key_file) = self
            .get_cert_files()
            .or_else(|| self.get_cert_files_from_rtsp_src())
            .or(None)?;

        let cert_chain = read_certs_from_file(&certificate_file)
            .map_err(|_| ())
            .ok()?;

        let private_key_der = read_private_key_from_file(&private_key_file)
            .map_err(|_| ())
            .ok()?;

        let private_key = any_supported_type(&private_key_der).ok()?;

        Some(Arc::new(rustls::sign::CertifiedKey {
            cert: cert_chain,
            key: private_key,
            ocsp: None,
        }))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct RtspServerCertVerifier {
    flags: RtspSrc2TlsValidationFlags,
    delegate: Arc<dyn rustls::client::danger::ServerCertVerifier>,
}

impl RtspServerCertVerifier {
    fn new(
        flags: RtspSrc2TlsValidationFlags,
        delegate: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    ) -> Arc<Self> {
        Arc::new(Self { flags, delegate })
    }

    fn is_insecure_algorithm(&self, cert: &x509_parser::certificate::X509Certificate<'_>) -> bool {
        // This validation applies to the CERTIFICATE SIGNATURE ALGORITHM (how
        // the CA signed the cert), not the TLS handshake signature algorithm
        // (CertificateVerify in TLS 1.2/1.3). The handshake signature algorithm
        // is negotiated separately in verify_tls12/tls13_signature.
        //
        // RFC 8446 (TLS 1.3) Section 4.4.2.2 and RFC 5246 (TLS 1.2) Section
        // 7.4.1.4.1 mandate rejection of certificates signed with weak algorithms.
        // This list considers those algorithms, also see `sign_algorithms` in
        // lib/algorithms/sign.c in GnuTLS source.
        // https://gitlab.com/gnutls/gnutls/-/blob/master/lib/algorithms/sign.c?ref_type=heads#L41
        //
        // Cryptographically Broken (must reject):
        //   - OID_PKCS1_MD2WITHRSAENC (MD2)
        //   - OID_PKCS1_MD4WITHRSAENC (MD4)
        //   - OID_PKCS1_MD5WITHRSAENC, OID_MD5_WITH_RSA (MD5)
        //
        // Collision-Vulnerable (must reject per RFC 8446 §4.2.3):
        // - OID_PKCS1_SHA1WITHRSA, OID_SHA1_WITH_RSA (SHA-1)
        //   CAs stopped issuing SHA-1 certs in 2016; major browsers rejected
        //   them by 2017. RFC 8446 prohibits offering SHA-1 in signature_algorithms.
        // - OID_SIG_DSA_WITH_SHA1 (DSA with SHA-1, deprecated since ~2010)
        //
        // Weak Hash Function (reject for certificates):
        //   - OID_SIG_RSA_RIPE_MD160
        //      is weakened; marked _INSECURE_FOR_CERTS by GnuTLS.
        //
        // GOST Algorithms (RFC 9367/not supported in Rustls):
        //   - OID_SIG_GOST_R3410_2012_256
        //   - OID_SIG_GOST_R3410_2012_512
        //   - OID_SIG_GOST_R3411_94_WITH_R3410_2001:
        //     Russian cryptographic standards/Non-standard in western TLS ecosystems.
        //
        // References:
        //   - RFC 8446: TLS 1.3 (Section 4.4.2.2)
        //   - RFC 5246: TLS 1.2 (Section 7.4.1.4.1)
        //   - GnuTLS:   Algorithm security levels (_INSECURE, _INSECURE_FOR_CERTS, SHA1_SECURE_VAL)
        const INSECURE_ALGORITHMS: &[asn1_rs::Oid<'static>] = &[
            oid_registry::OID_PKCS1_MD2WITHRSAENC,
            oid_registry::OID_PKCS1_MD4WITHRSAENC,
            oid_registry::OID_PKCS1_MD5WITHRSAENC,
            oid_registry::OID_PKCS1_SHA1WITHRSA,
            oid_registry::OID_HASH_SHA1,
            oid_registry::OID_SIG_DSA_WITH_SHA1,
            oid_registry::OID_MD5_WITH_RSA,
            oid_registry::OID_SHA1_WITH_RSA,
            oid_registry::OID_SIG_RSA_RIPE_MD160,
            oid_registry::OID_SIG_GOST_R3410_2012_256,
            oid_registry::OID_SIG_GOST_R3410_2012_512,
            oid_registry::OID_SIG_GOST_R3411_94_WITH_R3410_2001,
        ];

        INSECURE_ALGORITHMS.contains(&cert.signature_algorithm.algorithm)
    }
}

impl rustls::client::danger::ServerCertVerifier for RtspServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer,
        intermediates: &[rustls_pki_types::CertificateDer],
        server_name: &rustls::pki_types::ServerName,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if self.flags.is_empty() {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }

        if let Ok((_, ref cert)) = parse_x509_certificate(end_entity) {
            let asn1_now = ASN1Time::from_timestamp(now.as_secs() as i64)
                .map_err(|_| rustls::Error::FailedToGetCurrentTime)?;

            if !self.flags.contains(RtspSrc2TlsValidationFlags::Expired)
                && cert.validity().not_after < asn1_now
            {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::Expired,
                ));
            }

            if !self
                .flags
                .contains(RtspSrc2TlsValidationFlags::NotActivated)
                && cert.validity().not_before > asn1_now
            {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidYet,
                ));
            }

            if !self.flags.contains(RtspSrc2TlsValidationFlags::Insecure)
                && self.is_insecure_algorithm(cert)
            {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::UnsupportedSignatureAlgorithmContext {
                        signature_algorithm_id: Vec::new(),
                        supported_algorithms: Vec::new(),
                    },
                ));
            }
        };

        // Delegate to platform verifier, then filter errors

        match self.delegate.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(rustls::Error::InvalidCertificate(cert_err)) => {
                // Determine which flag(s) could suppress this error.
                let suppress = match &cert_err {
                    rustls::CertificateError::UnknownIssuer => {
                        self.flags.contains(RtspSrc2TlsValidationFlags::UnknownCA)
                    }
                    rustls::CertificateError::NotValidForName
                    | rustls::CertificateError::NotValidForNameContext { .. } => {
                        self.flags.contains(RtspSrc2TlsValidationFlags::BadIdentity)
                    }
                    rustls::CertificateError::Expired
                    | rustls::CertificateError::ExpiredContext { .. } => {
                        self.flags.contains(RtspSrc2TlsValidationFlags::Expired)
                    }
                    rustls::CertificateError::NotValidYet
                    | rustls::CertificateError::NotValidYetContext { .. } => {
                        self.flags.contains(RtspSrc2TlsValidationFlags::NotActivated)
                    }
                    rustls::CertificateError::Revoked => {
                        self.flags.contains(RtspSrc2TlsValidationFlags::Revoked)
                    }
                    rustls::CertificateError::UnsupportedSignatureAlgorithmContext { .. }
                    | rustls::CertificateError::UnsupportedSignatureAlgorithmForPublicKeyContext { .. }
                    | rustls::CertificateError::BadSignature => {
                        self.flags.contains(RtspSrc2TlsValidationFlags::Insecure)
                    }
                    // GENERIC_ERROR: covers OtherError, ApplicationVerificationFailure,
                    // BadEncoding, InvalidOcspResponse, UnhandledCriticalExtension,
                    // UnknownRevocationStatus, ExpiredRevocationList, InvalidPurpose, etc.
                    _ => {
                        self.flags.contains(RtspSrc2TlsValidationFlags::GenericError)
                    }
                };

                if suppress {
                    Ok(rustls::client::danger::ServerCertVerified::assertion())
                } else {
                    Err(rustls::Error::InvalidCertificate(cert_err))
                }
            }
            Err(other_err) => {
                // Non-certificate errors (protocol errors, etc.) — suppress only if
                // GENERIC_ERROR flag is set.
                if self
                    .flags
                    .contains(RtspSrc2TlsValidationFlags::GenericError)
                {
                    Ok(rustls::client::danger::ServerCertVerified::assertion())
                } else {
                    Err(other_err)
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.delegate.supported_verify_schemes()
    }
}
