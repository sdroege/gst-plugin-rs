// SPDX-License-Identifier: MPL-2.0

use anyhow::Error;
use clap::Parser;
use gst_plugin_webrtc_signalling::handlers::Handler;
use gst_plugin_webrtc_signalling::server::{Server, ServerError};
use gst_plugin_webrtc_tls_utils::create_tls_acceptor;
use std::time::Duration;
use tokio::{net::TcpListener, task};
use tracing::{info, warn};
use tracing_subscriber::prelude::*;

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
    /// Private key to use
    #[clap(short, long)]
    key: Option<String>,
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

    // We do the same when `run-signalling-server=true`
    let host = match args.host.as_str() {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "[::1]".to_string(),
        host => host.to_string(),
    };
    let addr = format!("{}:{}", host, args.port);

    // Create the event loop and TCP listener we'll accept connections on.
    let listener = TcpListener::bind(&addr).await?;

    let acceptor = if let (Some(cert), Some(key)) = (&args.cert, &args.key) {
        create_tls_acceptor(cert, key).await.ok()
    } else {
        None
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
