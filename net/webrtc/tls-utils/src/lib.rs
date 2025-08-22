// SPDX-License-Identifier: MPL-2.0

use std::{
    fs::File,
    io::{BufReader, Cursor},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_rustls::{rustls, TlsAcceptor, TlsConnector};

fn read_certs_from_file(
    certificate_file: PathBuf,
) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, Box<dyn std::error::Error>> {
    let cert_file = File::open(&certificate_file)?;
    let mut cert_file_rdr = BufReader::new(cert_file);

    let certs_result = rustls_pemfile::certs(&mut cert_file_rdr);
    let mut certs = Vec::new();

    for cert_result in certs_result {
        match cert_result {
            Ok(cert) => certs.push(cert),
            Err(e) => {
                return Err(format!("Failed to parse certificate: {e}").into());
            }
        }
    }

    if certs.is_empty() {
        return Err(format!(
            "No valid certificates found in {}",
            certificate_file.display()
        )
        .into());
    }

    Ok(certs)
}

fn read_private_key_from_file(
    private_key_file: PathBuf,
) -> Result<rustls_pki_types::PrivateKeyDer<'static>, Box<dyn std::error::Error>> {
    let key_file = File::open(&private_key_file)?;
    let mut key_file_rdr = BufReader::new(key_file);
    let items_result = rustls_pemfile::read_all(&mut key_file_rdr);

    for item_result in items_result {
        let item = item_result.map_err(|e| {
            format!(
                "Failed to parse PEM item in {}: {e}",
                private_key_file.display(),
            )
        })?;

        match item {
            rustls_pemfile::Item::Pkcs1Key(key) => {
                return Ok(rustls_pki_types::PrivateKeyDer::from(key));
            }
            rustls_pemfile::Item::Pkcs8Key(key) => {
                return Ok(rustls_pki_types::PrivateKeyDer::from(key));
            }
            rustls_pemfile::Item::Sec1Key(key) => {
                return Ok(rustls_pki_types::PrivateKeyDer::from(key));
            }
            _ => continue,
        }
    }

    Err(format!(
        "No valid private key found in {}",
        private_key_file.display()
    )
    .into())
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer,
        _intermediates: &[rustls_pki_types::CertificateDer],
        _server_name: &rustls::pki_types::ServerName,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn get_root_certstore<P: AsRef<Path>>(
    cafile_path: P,
) -> Result<rustls::RootCertStore, std::io::Error> {
    let mut root_cert_store = rustls::RootCertStore::empty();

    let certs = {
        let cert_file = tokio::fs::read(&cafile_path).await?;
        let mut cert_file_rdr = BufReader::new(Cursor::new(cert_file));
        let cert_vec = rustls_pemfile::certs(&mut cert_file_rdr);
        cert_vec.into_iter().map(|c| c.unwrap()).collect::<Vec<_>>()
    };

    let _ = root_cert_store.add_parsable_certificates(certs);

    Ok(root_cert_store)
}

pub async fn create_tls_acceptor(
    certificate_file: &str,
    private_key_file: &str,
) -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
    let ring_provider = rustls::crypto::ring::default_provider();
    let certs = read_certs_from_file(certificate_file.into())?;
    let key = read_private_key_from_file(private_key_file.into())?;

    let config = rustls::ServerConfig::builder_with_provider(ring_provider.into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub async fn create_tls_connector<P: AsRef<Path>>(
    certificate_file: Option<P>,
    insecure_tls: bool,
) -> Result<TlsConnector, std::io::Error> {
    let ring_provider = rustls::crypto::ring::default_provider();

    if insecure_tls || certificate_file.is_none() {
        let config = rustls::ClientConfig::builder_with_provider(ring_provider.into())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth();

        Ok(TlsConnector::from(Arc::new(config)))
    } else {
        let root_cert_store = get_root_certstore(certificate_file.unwrap()).await?;
        let config = rustls::ClientConfig::builder_with_provider(ring_provider.into())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();

        Ok(TlsConnector::from(Arc::new(config)))
    }
}
