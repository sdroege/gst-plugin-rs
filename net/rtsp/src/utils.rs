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
) -> Result<ClientConfig, Box<dyn std::error::Error>> {
    use rustls_platform_verifier::BuilderVerifierExt;

    let resolver = ClientCertResolver::new(rtsp_src, certificate_file, private_key_file);
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_platform_verifier()
        .unwrap()
        .with_client_cert_resolver(Arc::new(resolver)))
}

pub fn create_tls_connector(
    rtsp_src: gst::glib::WeakRef<super::rtspsrc::RtspSrc>,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
) -> Result<TlsConnector, Box<dyn std::error::Error>> {
    let config = client_config(rtsp_src, certificate_file, private_key_file)?;
    Ok(TlsConnector::from(Arc::new(config)))
}

#[derive(Debug)]
struct ClientCert {
    // Path to certificate chain for the private key file in PEM format
    certificate_file: Option<PathBuf>,
    // Path to a PKCS1, PKCS8 or SEC1 private key file in PEM format
    private_key_file: Option<PathBuf>,
}

#[derive(Debug)]
struct ClientCertResolver {
    rtsp_src: gst::glib::WeakRef<super::rtspsrc::RtspSrc>,
    cert: ClientCert,
}

impl From<gst::Structure> for ClientCert {
    fn from(s: gst::Structure) -> Self {
        Self {
            certificate_file: s.get::<String>("certificate-file").ok().map(Into::into),
            private_key_file: s.get::<String>("private-key-file").ok().map(Into::into),
        }
    }
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
