// SPDX-License-Identifier: MPL-2.0

use clap::Parser;
use gst_plugin_webrtc_signalling::handlers::Handler;
use gst_plugin_webrtc_signalling::server::{Server, ServerError};
use tokio::io::AsyncReadExt;
use tokio::task;
use tracing_subscriber::prelude::*;

use anyhow::Error;
use std::time::Duration;
use tokio::fs;
use tokio::net::TcpListener;
use tokio_native_tls::native_tls::TlsAcceptor;
use tracing::{info, warn};

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[clap(about, version, author)]
/// Program arguments
struct Args {
    /// Address to listen on
    #[clap(long, default_value = "0.0.0.0")]
    host: String,
    /// Port to listen on
    #[clap(short, long, default_value_t = 8443)]
    port: u16,
    /// TLS certificate to use
    #[clap(short, long)]
    cert: Option<String>,
    /// password to TLS certificate
    #[clap(long)]
    cert_password: Option<String>,
}

fn initialize_logging(envvar_name: &str) -> Result<(), Error> {
    tracing_log::LogTracer::init()?;
    let env_filter = tracing_subscriber::EnvFilter::try_from_env(envvar_name)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_target(true)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        );
    let subscriber = tracing_subscriber::Registry::default()
        .with(env_filter)
        .with(fmt_layer);
    tracing::subscriber::set_global_default(subscriber)?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();
    let server = Server::spawn(Handler::new);

    initialize_logging("WEBRTCSINK_SIGNALLING_SERVER_LOG")?;

    let addr = format!("{}:{}", args.host, args.port);

    // Create the event loop and TCP listener we'll accept connections on.
    let listener = TcpListener::bind(&addr).await?;

    let acceptor = match args.cert {
        Some(cert) => {
            let mut file = fs::File::open(cert).await?;
            let mut identity = vec![];
            file.read_to_end(&mut identity).await?;
            let identity = tokio_native_tls::native_tls::Identity::from_pkcs12(
                &identity,
                args.cert_password.as_deref().unwrap_or(""),
            )
            .unwrap();
            Some(tokio_native_tls::TlsAcceptor::from(
                TlsAcceptor::new(identity).unwrap(),
            ))
        }
        None => None,
    };

    info!("Listening on: {}", addr);

    while let Ok((stream, address)) = listener.accept().await {
        let mut server_clone = server.clone();
        info!("Accepting connection from {}", address);

        if let Some(acceptor) = acceptor.clone() {
            tokio::spawn(async move {
                match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(stream)) => server_clone.accept_async(stream).await,
                    Ok(Err(err)) => {
                        warn!("Failed to accept TLS connection from {}: {}", address, err);
                        Err(ServerError::TLSHandshake(err))
                    }
                    Err(elapsed) => {
                        warn!("TLS connection timed out {} after {}", address, elapsed);
                        Err(ServerError::TLSHandshakeTimeout(elapsed))
                    }
                }
            });
        } else {
            task::spawn(async move { server_clone.accept_async(stream).await });
        }
    }

    Ok(())
}
