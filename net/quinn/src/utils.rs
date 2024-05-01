// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//G
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::future;
use futures::prelude::*;
use gst::ErrorMessage;
use once_cell::sync::Lazy;
use quinn::{ClientConfig, Endpoint, ServerConfig};
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::runtime;

pub const CONNECTION_CLOSE_CODE: u32 = 0;
pub const CONNECTION_CLOSE_MSG: &str = "Stopped";

#[derive(Error, Debug)]
pub enum WaitError {
    #[error("Future aborted")]
    FutureAborted,
    #[error("Future returned an error: {0}")]
    FutureError(ErrorMessage),
}

pub static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .thread_name("gst-quic-runtime")
        .build()
        .unwrap()
});

pub fn wait<F, T>(
    canceller: &Mutex<Option<future::AbortHandle>>,
    future: F,
    timeout: u32,
) -> Result<T, WaitError>
where
    F: Send + Future<Output = T>,
    T: Send + 'static,
{
    let mut canceller_guard = canceller.lock().unwrap();
    let (abort_handle, abort_registration) = future::AbortHandle::new_pair();

    if canceller_guard.is_some() {
        return Err(WaitError::FutureError(gst::error_msg!(
            gst::ResourceError::Failed,
            ["Old Canceller should not exist"]
        )));
    }

    canceller_guard.replace(abort_handle);
    drop(canceller_guard);

    let future = async {
        if timeout == 0 {
            Ok(future.await)
        } else {
            let res = tokio::time::timeout(Duration::from_secs(timeout.into()), future).await;

            match res {
                Ok(r) => Ok(r),
                Err(e) => Err(gst::error_msg!(
                    gst::ResourceError::Read,
                    ["Request timeout, elapsed: {}", e.to_string()]
                )),
            }
        }
    };

    let future = async {
        match future::Abortable::new(future, abort_registration).await {
            Ok(Ok(res)) => Ok(res),

            Ok(Err(err)) => Err(WaitError::FutureError(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Future resolved with an error {:?}", err]
            ))),

            Err(future::Aborted) => Err(WaitError::FutureAborted),
        }
    };

    let res = RUNTIME.block_on(future);

    canceller_guard = canceller.lock().unwrap();
    *canceller_guard = None;

    res
}

pub fn make_socket_addr(addr: &str) -> Result<SocketAddr, WaitError> {
    match addr.parse::<SocketAddr>() {
        Ok(address) => Ok(address),
        Err(e) => Err(WaitError::FutureError(gst::error_msg!(
            gst::ResourceError::Failed,
            ["Invalid address: {}", e]
        ))),
    }
}

/*
 * Following functions are taken from Quinn documentation/repository
 */
struct SkipServerVerification;

impl SkipServerVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

fn configure_client(
    secure_conn: bool,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
    alpns: Vec<String>,
) -> Result<ClientConfig, Box<dyn Error>> {
    let mut crypto = if secure_conn {
        let (certs, key) = read_certs_from_file(certificate_file, private_key_file)?;
        let mut cert_store = rustls::RootCertStore::empty();

        for cert in &certs {
            cert_store.add(cert)?;
        }

        rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(Arc::new(cert_store))
            .with_client_auth_cert(certs, key)?
    } else {
        rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth()
    };

    let alpn_protocols: Vec<Vec<u8>> = alpns
        .iter()
        .map(|x| x.as_bytes().to_vec())
        .collect::<Vec<_>>();
    crypto.alpn_protocols = alpn_protocols;
    crypto.key_log = Arc::new(rustls::KeyLogFile::new());

    Ok(ClientConfig::new(Arc::new(crypto)))
}

fn read_certs_from_file(
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
) -> Result<(Vec<rustls::Certificate>, rustls::PrivateKey), Box<dyn Error>> {
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
    let cert_file = certificate_file
        .clone()
        .expect("Expected path to certificates be valid");
    let key_file = private_key_file.expect("Expected path to certificates be valid");

    let certs: Vec<rustls::Certificate> = {
        let cert_file = File::open(cert_file.as_path())?;
        let mut cert_file_rdr = BufReader::new(cert_file);
        let cert_vec = rustls_pemfile::certs(&mut cert_file_rdr)?;
        cert_vec.into_iter().map(rustls::Certificate).collect()
    };

    let key: rustls::PrivateKey = {
        let key_file = File::open(key_file.as_path())?;
        let mut key_file_rdr = BufReader::new(key_file);

        let keys_iter = rustls_pemfile::read_all(&mut key_file_rdr)?;
        let key_item = keys_iter
            .into_iter()
            .next()
            .ok_or("Certificate should have at least one private key")?;

        match key_item {
            rustls_pemfile::Item::RSAKey(key) => rustls::PrivateKey(key),
            rustls_pemfile::Item::PKCS8Key(key) => rustls::PrivateKey(key),
            _ => unimplemented!(),
        }
    };

    Ok((certs, key))
}

fn configure_server(
    server_name: &str,
    secure_conn: bool,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
    alpns: Vec<String>,
) -> Result<(ServerConfig, Vec<rustls::Certificate>), Box<dyn Error>> {
    let (certs, key) = if secure_conn {
        read_certs_from_file(certificate_file, private_key_file)?
    } else {
        let cert = rcgen::generate_simple_self_signed(vec![server_name.into()]).unwrap();
        let cert_der = cert.serialize_der().unwrap();
        let priv_key = cert.serialize_private_key_der();
        let priv_key = rustls::PrivateKey(priv_key);
        let cert_chain = vec![rustls::Certificate(cert_der)];

        (cert_chain, priv_key)
    };

    let mut crypto = if secure_conn {
        let mut cert_store = rustls::RootCertStore::empty();
        for cert in &certs {
            cert_store.add(cert)?;
        }

        let auth_client = rustls::server::AllowAnyAuthenticatedClient::new(cert_store);
        rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_client_cert_verifier(Arc::new(auth_client))
            .with_single_cert(certs.clone(), key)
    } else {
        rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(certs.clone(), key)
    }?;

    let alpn_protocols: Vec<Vec<u8>> = alpns
        .iter()
        .map(|x| x.as_bytes().to_vec())
        .collect::<Vec<_>>();
    crypto.alpn_protocols = alpn_protocols;
    crypto.key_log = Arc::new(rustls::KeyLogFile::new());
    let mut server_config = ServerConfig::with_crypto(Arc::new(crypto));

    Arc::get_mut(&mut server_config.transport)
        .unwrap()
        .max_concurrent_bidi_streams(0_u8.into())
        .max_concurrent_uni_streams(1_u8.into());

    Ok((server_config, certs))
}

pub fn server_endpoint(
    server_addr: SocketAddr,
    server_name: &str,
    secure_conn: bool,
    alpns: Vec<String>,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
) -> Result<Endpoint, Box<dyn Error>> {
    let (server_config, _) = configure_server(
        server_name,
        secure_conn,
        certificate_file,
        private_key_file,
        alpns,
    )?;
    let endpoint = Endpoint::server(server_config, server_addr)?;

    Ok(endpoint)
}

pub fn client_endpoint(
    client_addr: SocketAddr,
    secure_conn: bool,
    alpns: Vec<String>,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
) -> Result<Endpoint, Box<dyn Error>> {
    let client_cfg = configure_client(secure_conn, certificate_file, private_key_file, alpns)?;
    let mut endpoint = Endpoint::client(client_addr)?;

    endpoint.set_default_client_config(client_cfg);

    Ok(endpoint)
}
