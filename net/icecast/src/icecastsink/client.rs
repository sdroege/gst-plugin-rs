// GStreamer Icecast Sink - client
//
// Copyright (C) 2023-2025 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::imp::CAT;
use super::mediaformat::*;

use atomic_refcell::AtomicRefCell;

use data_encoding::BASE64;

use futures::future;
use futures::prelude::*;

use gst::glib;

use httparse::Response;

use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};

use rustls_pki_types::ServerName;

use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Duration;

use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio_rustls::TlsConnector;

use url::Url;

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum TcpOrTlsStream {
    Plain(tokio::net::TcpStream),
    Tls(tokio_rustls::client::TlsStream<tokio::net::TcpStream>),
}

impl AsyncWrite for TcpOrTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            TcpOrTlsStream::Plain(ref mut s) => Pin::new(s).poll_write(cx, buf),
            TcpOrTlsStream::Tls(ref mut s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TcpOrTlsStream::Plain(ref mut s) => Pin::new(s).poll_flush(cx),
            TcpOrTlsStream::Tls(ref mut s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TcpOrTlsStream::Plain(ref mut s) => Pin::new(s).poll_shutdown(cx),
            TcpOrTlsStream::Tls(ref mut s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for TcpOrTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TcpOrTlsStream::Plain(ref mut s) => Pin::new(s).poll_read(cx, buf),
            TcpOrTlsStream::Tls(ref mut s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum State {
    // Initiated TCP connection to server
    Connecting {
        join_handle: JoinHandle<Result<TcpOrTlsStream, gst::ErrorMessage>>,
    },
    // Streaming thread is waiting for connection + initial handshakes to complete
    WaitingForConnect,
    // Streaming data
    Streaming {
        stream: TcpOrTlsStream,
    },
    Error,
}

#[derive(Default, Debug)]
enum Canceller {
    #[default]
    None,
    Armed(future::AbortHandle),
    Cancelled,
}

impl Canceller {
    fn cancel(&mut self) {
        if let Canceller::Armed(ref abort_handle) = *self {
            abort_handle.abort();
        }

        *self = Canceller::Cancelled;
    }
}

#[derive(Debug)]
pub(super) struct IceClient {
    // Connection State
    state: AtomicRefCell<State>,
    caps_tx: AtomicRefCell<Option<oneshot::Sender<MediaFormat>>>,

    canceller: Mutex<Canceller>,

    log_id: glib::GString, // For debug logging
}

// Hardcoded for now
const USER_AGENT: &str = concat!(
    "GStreamer icecastsink ",
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("COMMIT_ID")
);

static RUNTIME: LazyLock<runtime::Runtime> = LazyLock::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

impl IceClient {
    // Starts a task which will connect to the server and will wait for caps to come through,
    // before finishing the handshake and returning the TCP stream to which we can send data later.
    pub(super) fn new(
        url: Url,
        public: bool,
        stream_name: Option<String>,
        log_id: glib::GString,
    ) -> Result<Self, gst::ErrorMessage> {
        // FIXME: make connect and handshake abortable
        let (abort_handle, _abort_registration) = future::AbortHandle::new_pair();

        let (caps_tx, caps_rx) = oneshot::channel();

        let debug_log_id = log_id.clone();

        gst::info!(
            CAT,
            id = &log_id,
            "Initiating connection to server (in new thread).. "
        );

        let join_handle = RUNTIME.spawn(async move {
            let public = public as i32;

            let scheme = url.scheme();
            let host_name = url.host_str().unwrap();
            let port = url.port().unwrap_or(8000);
            let path = url.path();
            let username = url.username();
            let password = url.password().unwrap_or("");

            gst::info!(
                CAT,
                id = &log_id,
                "Connecting via {scheme} to server {host_name}:{port}.."
            );

            let stream = TcpStream::connect(format!("{host_name}:{port}"))
                .await
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to connect to server {host_name}:{port}: {err}"]
                    )
                })?;

            gst::info!(CAT, id = &log_id, "Connected to server {host_name}:{port}");

            let stream = match (scheme, stream) {
                // TLS
                ("ice+https", stream) => {
                    let provider = Arc::new(rustls::crypto::ring::default_provider());

                    let config = ClientConfig::builder_with_provider(provider)
                        .with_safe_default_protocol_versions()
                        .unwrap()
                        .with_platform_verifier()
                        .unwrap()
                        .with_no_client_auth();

                    let connector = TlsConnector::from(Arc::new(config));
                    let dnsname = ServerName::try_from(host_name.to_string()).map_err(|err| {
                        gst::error_msg!(
                            gst::ResourceError::Write,
                            ["Server name failed for '{host_name}': {err}"]
                        )
                    })?;

                    gst::info!(CAT, id = &log_id, "TLS connect..");

                    let stream = connector.connect(dnsname, stream).await.map_err(|err| {
                        gst::error_msg!(
                            gst::ResourceError::Write,
                            ["TLS handshake with server failed: {err}"]
                        )
                    })?;

                    gst::info!(CAT, id = &log_id, "TLS setup done");

                    TcpOrTlsStream::Tls(stream)
                }
                // Nothing to do for plain TCP
                ("ice+http", stream) => TcpOrTlsStream::Plain(stream),
                _ => unreachable!(),
            };

            let mut stream = BufReader::new(stream);

            gst::info!(CAT, id = &log_id, "Sending OPTIONS request to server..");

            let options_request = format!(
                "\
                OPTIONS * HTTP/1.1\r\n\
                Host: {host_name}:{port}\r\n\
                User-Agent: {USER_AGENT}\r\n\
                Connection: keep-alive\r\n\
                \r\n"
            );

            stream
                .write_all(options_request.as_bytes())
                .await
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to send OPTIONS request to server: {err}"]
                    )
                })?;

            const MAX_RESPONSE_LENGTH: usize = 2048; // 2kB

            let mut response = String::new();

            // This uses String which assumes the response (and any data that follows) is UTF-8,
            // which is a reasonable assumption in our case. In the unlikely case that it's not
            // the task will panic and the panic will be caught gracefully when joining later.
            while !response.ends_with("\r\n\r\n") {
                stream.read_line(&mut response).await.map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to complete OPTIONS handshake with server: {err}"]
                    )
                })?;

                if response.len() > MAX_RESPONSE_LENGTH {
                    return Err(gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Excessive server OPTIONS response length"]
                    ));
                }
            }

            // Server quirk/bug: sends bogus \r\n in middle of response headers (rsas 1.0.3)
            // We get an extra stanza after the first \r\n\r\n, fold that into the original response.
            // Headers are technically case-insensitive, but we're dealing with a specific server here.
            if response.contains("Server: rocketstreamingserver")
                && !response.contains("Content-Length: ")
                && stream.buffer().ends_with(b"\r\n\r\n")
            {
                response.pop().unwrap();
                response.pop().unwrap();
                while !response.ends_with("\r\n\r\n") {
                    stream.read_line(&mut response).await.map_err(|err| {
                        gst::error_msg!(
                            gst::ResourceError::OpenWrite,
                            ["Failed to complete OPTIONS handshake with server: {err}"]
                        )
                    })?;

                    if response.len() > MAX_RESPONSE_LENGTH {
                        return Err(gst::error_msg!(
                            gst::ResourceError::OpenRead,
                            ["Excessive server OPTIONS response length"]
                        ));
                    }
                }
            }

            gst::info!(CAT, id = &log_id, "OPTIONS response: {response}");

            // Parse OPTIONS response
            let mut r_headers = [httparse::EMPTY_HEADER; 32];
            let mut r = Response::new(&mut r_headers);

            use httparse::Status::{Complete, Partial};

            match r.parse(response.as_bytes()) {
                Ok(Complete(_)) => {
                    gst::trace!(CAT, id = &log_id, "Parsed OPTIONS response: {r:?}");

                    match r.code {
                        // 200-204 - success
                        Some(200..=204) => Ok(()),
                        // 401 - Authentication required - ignore, we'll just authenticate the PUT
                        Some(401) => Ok(()),
                        // 405 - Method Not Allowed - not promising, but ignore for now
                        Some(405) => Ok(()),
                        // Everything else is unexpected
                        _ => Err(gst::error_msg!(
                            gst::ResourceError::OpenWrite,
                            [
                                "Error probing server via OPTIONS request: {} {}",
                                r.code.unwrap(),
                                r.reason.unwrap_or("")
                            ]
                        )),
                    }
                }
                Ok(Partial) => Err(gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to parse OPTIONS response from server: partial response"]
                )),
                Err(err) => Err(gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to parse OPTIONS response from server: {err:?}"]
                )),
            }?;

            // There should be no content body in the response, but better be safe
            if let Some(mut content_len) = r
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .and_then(|s| s.parse::<usize>().ok())
            {
                gst::debug!(CAT, id = &log_id, "Content-Length: {content_len} bytes");
                while content_len > 0 {
                    let n_bytes = content_len.min(4096);
                    gst::trace!(CAT, id = &log_id, "Reading {n_bytes} content bytes");
                    let mut buf = vec![0u8; n_bytes];
                    let _ = stream.read_exact(&mut buf).await.map_err(|err| {
                        gst::error_msg!(
                            gst::ResourceError::OpenWrite,
                            ["Failed to read content after OPTIONS response: {err}"]
                        )
                    })?;
                    content_len -= n_bytes;
                }
            }

            //  Discard any remaining bytes in the read buffer
            if !stream.buffer().is_empty() {
                let n_bytes = stream.buffer().len();
                gst::warning!(
                    CAT,
                    id = &log_id,
                    "Discarding {n_bytes} excess bytes after OPTIONS response!"
                );
                let mut buf = vec![0u8; n_bytes];
                let _ = stream.read_exact(&mut buf).await.unwrap();
            }

            // Wait for initial caps to come through (errors will be handled below)
            gst::info!(
                CAT,
                id = &log_id,
                "Waiting for initial caps, media format.."
            );

            let media_format: MediaFormat = caps_rx.await.map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Read,
                    ["Failed to receive media format: {err}"]
                )
            })?;

            let (content_type, ice_audio_info) = {
                if media_format == MediaFormat::None {
                    return Err(gst::error_msg!(
                        gst::StreamError::Format,
                        ["No media format configured"]
                    ));
                }

                gst::info!(CAT, id = &log_id, "Media format: {media_format:?}");
                let content_type = media_format.content_type().unwrap();

                // TODO: doesn't include e.g. bitrate=128
                let ice_audio_info = media_format.ice_audio_info().unwrap();

                (content_type, ice_audio_info)
            };

            let auth_header = if !username.is_empty() || !password.is_empty() {
                format!(
                    "Authorization: Basic {}",
                    BASE64.encode(format!("{username}:{password}").as_bytes())
                )
            } else {
                String::new()
            };

            let mut ice_headers = String::with_capacity(1024);
            ice_headers.push_str(&format!("Ice-audio-info: {ice_audio_info}\r\n"));
            ice_headers.push_str(&format!("Ice-public: {public}\r\n"));
            if let Some(stream_name) = stream_name {
                ice_headers.push_str(&format!("Ice-name: {stream_name}\r\n"));
            }

            let put_request = format!(
                "\
                PUT {path} HTTP/1.1\r\n\
                Host: {host_name}:{port}\r\n\
                {auth_header}\r\n\
                User-Agent: {USER_AGENT}\r\n\
                Content-Type: {content_type}\r\n\
                {ice_headers}\
                Expect: 100-continue\r\n\
                \r\n"
            );
            gst::info!(CAT, id = &log_id, "PUT request: {put_request}");

            stream
                .write_all(put_request.as_bytes())
                .await
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to send PUT request to server: {err}"]
                    )
                })?;

            response.clear();
            while !response.ends_with("\r\n\r\n") {
                stream.read_line(&mut response).await.map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to receive PUT request response: {err}"]
                    )
                })?;

                if response.len() > MAX_RESPONSE_LENGTH {
                    return Err(gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Excessive server PUT request response length"]
                    ));
                }
            }

            gst::info!(CAT, id = &log_id, "PUT response: {response}");

            // Parse PUT response
            let mut r_headers = [httparse::EMPTY_HEADER; 32];
            let mut r = Response::new(&mut r_headers);

            match r.parse(response.as_bytes()) {
                Ok(Complete(_)) => {
                    gst::trace!(CAT, id = &log_id, "Parsed PUT response: {r:?}");

                    match r.code {
                        // 100-continue is what we're expecting
                        Some(100) => Ok(()),
                        // 200-204 - success - not what we were expecting, but good enough
                        Some(200..=204) => Ok(()),
                        // 401 - Authentication required
                        Some(401) => Err(if auth_header.is_empty() {
                            gst::error_msg!(
                                gst::ResourceError::NotAuthorized,
                                ["Server requires authorization, but no username and/or password configured"]
                            )} else {
                            gst::error_msg!(
                                gst::ResourceError::NotAuthorized,
                                ["Server didn't accept credentials for mount point {path}"]
                            )}
                        ),
                        // 403 - Forbidden / Content Type Not Supported
                        Some(403) => match r.reason {
                            Some("Content-type not supported") => Err(gst::error_msg!(
                                gst::ResourceError::Settings,
                                ["Server doesn't support content type {content_type}"]
                            )),
                            Some("Mountpoint in use") => Err(gst::error_msg!(
                                gst::ResourceError::Busy,
                                ["Mount point {path} already in use by another client"]
                            )),
                            _ => Err(gst::error_msg!(
                                gst::ResourceError::Settings,
                                ["Server didn't accept content type {content_type} on mount point {path} ({})", r.reason.unwrap_or("no reason")]
                            )),
                        }
                        // 404 - Not Found
                        Some(404) => Err(gst::error_msg!(
                            gst::ResourceError::NotFound,
                            ["Server didn't accept mount point {path} ({})", r.reason.unwrap_or("no reason")]
                        )),
                        // 405 - Method Not Allowed
                        Some(405) => Err(gst::error_msg!(
                            gst::ResourceError::OpenWrite,
                            ["Server doesn't support PUT method, upgrade your server!"]
                        )),
                        // Everything else is unexpected
                        _ => Err(gst::error_msg!(
                            gst::ResourceError::OpenWrite,
                            [
                                "Error sending PUT request: {} {}",
                                r.code.unwrap(),
                                r.reason.unwrap_or("")
                            ]
                        )),
                    }
                }
                Ok(Partial) => Err(gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to parse PUT response from server: partial response"]
                )),
                Err(err) => Err(gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to parse PUT response from server: {err}"]
                )),
            }?;

            //  Discard any remaining bytes in the read buffer
            if !stream.buffer().is_empty() {
                let n_bytes = stream.buffer().len();
                gst::warning!(
                    CAT,
                    id = &log_id,
                    "Discarding {n_bytes} excess bytes after PUT response!"
                );
                let mut buf = vec![0u8; n_bytes];
                let _ = stream.read_exact(&mut buf).await.unwrap();
            }

            Ok(stream.into_inner()) // Return TcpStream
        });

        let client = IceClient {
            state: AtomicRefCell::new(State::Connecting { join_handle }),
            caps_tx: AtomicRefCell::new(Some(caps_tx)),
            canceller: Mutex::new(Canceller::Armed(abort_handle)),
            log_id: debug_log_id,
        };

        Ok(client)
    }

    // Send media format over oneshot channel to connection thread that's waiting for
    // it in order to complete the handshake with the server and prepare it for streaming.
    pub(super) fn set_media_format(&self, media_format: MediaFormat) {
        gst::info!(CAT, id = &self.log_id, "{media_format:?}");

        let mut caps_tx_storage = self.caps_tx.borrow_mut();

        if let Some(caps_tx) = caps_tx_storage.take() {
            let _ = caps_tx.send(media_format);
        }
    }

    // Could be called from any thread
    pub(super) fn cancel(&self) {
        gst::info!(CAT, id = &self.log_id, "Cancelling..");

        let mut canceller = self.canceller.lock().unwrap();

        canceller.cancel();

        gst::log!(CAT, id = &self.log_id, "Cancelled!");
    }

    // Must only be called from streaming thread
    pub(super) fn wait_for_connection_and_handshake(
        &self,
        timeout_millisecs: u32,
    ) -> Result<(), Option<gst::ErrorMessage>> {
        let mut state = self.state.borrow_mut();

        if !matches!(*state, State::Connecting { .. }) {
            return Ok(());
        }

        let mut temp = State::WaitingForConnect;
        std::mem::swap(&mut *state, &mut temp);
        let State::Connecting { join_handle } = temp else {
            unreachable!()
        };

        gst::info!(
            CAT,
            id = &self.log_id,
            "Waiting for connection + handshake to server to complete"
        );

        let future = async {
            // Join failures can happen if the async task panics
            join_handle
                .await
                .map_err(|err| gst::error_msg!(gst::LibraryError::Failed, ["{err}"]))?
        };

        let res = self.sync_wait(future, timeout_millisecs);

        let stream = match res {
            Ok(res) => res,
            Err(err) => {
                gst::debug!(CAT, id = &self.log_id, "Error {err:?}");
                *state = State::Error;
                return Err(err);
            }
        };

        *state = State::Streaming { stream };
        gst::info!(CAT, id = &self.log_id, "Ready to stream");
        Ok(())
    }

    pub(super) fn send_data(
        &self,
        data: &[u8],
        timeout_millisecs: u32,
    ) -> Result<(), Option<gst::ErrorMessage>> {
        let mut state = self.state.borrow_mut();

        let State::Streaming { ref mut stream } = *state else {
            unreachable!();
        };

        gst::trace!(
            CAT,
            id = &self.log_id,
            "Sending {} bytes of data..",
            data.len()
        );

        let future = async move {
            stream.write_all(data).await.map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Write,
                    ["Failed to send data to server: {err}"]
                )
            })
        };

        let res = self.sync_wait(future, timeout_millisecs);

        gst::trace!(CAT, id = &self.log_id, "Done sending data: {res:?}");

        if let Err(err) = res {
            gst::debug!(CAT, id = &self.log_id, "Error {err:?}");
            *state = State::Error;
            return Err(err);
        }

        Ok(())
    }

    fn sync_wait<F, T>(&self, future: F, timeout: u32) -> Result<T, Option<gst::ErrorMessage>>
    where
        F: Send + Future<Output = Result<T, gst::ErrorMessage>>,
        T: Send + 'static,
    {
        let mut canceller = self.canceller.lock().unwrap();
        if matches!(*canceller, Canceller::Cancelled) {
            return Err(None);
        }
        let (abort_handle, abort_registration) = future::AbortHandle::new_pair();
        *canceller = Canceller::Armed(abort_handle);
        drop(canceller);

        // Wrap in a timeout
        let future = async {
            if timeout == 0 {
                future.await
            } else {
                let res = tokio::time::timeout(Duration::from_millis(timeout.into()), future).await;

                match res {
                    Ok(res) => res,
                    Err(_) => Err(gst::error_msg!(
                        gst::ResourceError::Write,
                        ["Request timeout"]
                    )),
                }
            }
        };

        let future = async {
            match future::Abortable::new(future, abort_registration).await {
                Ok(res) => res.map_err(Some),
                Err(_) => Err(None),
            }
        };

        let res = RUNTIME.block_on(future);

        /* Clear out the canceller */
        let mut canceller = self.canceller.lock().unwrap();
        if matches!(*canceller, Canceller::Cancelled) {
            return Err(None);
        }
        *canceller = Canceller::None;

        res
    }
}
