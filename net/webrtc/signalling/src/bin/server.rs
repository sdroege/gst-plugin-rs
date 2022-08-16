use async_std::task;
use clap::Parser;
use tracing_subscriber::prelude::*;
use webrtcsink_signalling::handlers::Handler;
use webrtcsink_signalling::server::Server;

use anyhow::Error;
use async_native_tls::TlsAcceptor;
use async_std::fs::File as AsyncFile;
use async_std::net::TcpListener;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[clap(about, version, author)]
/// Program arguments
struct Args {
    /// Address to listen on
    #[clap(short, long, default_value = "0.0.0.0")]
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

fn main() -> Result<(), Error> {
    let args = Args::parse();
    let server = Server::spawn(|stream| Handler::new(stream));

    initialize_logging("WEBRTCSINK_SIGNALLING_SERVER_LOG")?;

    task::block_on(async move {
        let addr = format!("{}:{}", args.host, args.port);

        // Create the event loop and TCP listener we'll accept connections on.
        let listener = TcpListener::bind(&addr).await?;

        let acceptor = match args.cert {
            Some(cert) => {
                let key = AsyncFile::open(cert).await?;
                Some(TlsAcceptor::new(key, args.cert_password.as_deref().unwrap_or("")).await?)
            }
            None => None,
        };

        info!("Listening on: {}", addr);

        while let Ok((stream, _)) = listener.accept().await {
            let mut server_clone = server.clone();

            let address = match stream.peer_addr() {
                Ok(address) => address,
                Err(err) => {
                    warn!("Connected peer with no address: {}", err);
                    continue;
                }
            };

            info!("Accepting connection from {}", address);

            if let Some(ref acceptor) = acceptor {
                let stream = match acceptor.accept(stream).await {
                    Ok(stream) => stream,
                    Err(err) => {
                        warn!("Failed to accept TLS connection from {}: {}", address, err);
                        continue;
                    }
                };
                task::spawn(async move { server_clone.accept_async(stream).await });
            } else {
                task::spawn(async move { server_clone.accept_async(stream).await });
            }
        }

        Ok(())
    })
}
