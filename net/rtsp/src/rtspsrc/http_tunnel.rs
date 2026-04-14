// GStreamer RTSP Source 2
//
// Copyright (C) 2026 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use httparse::{Response, Status};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub struct TunnelStream {
    stream: TcpStream,
    sink: mpsc::UnboundedSender<Bytes>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
pub enum TunnelError {
    Io(std::io::Error),
    Http(String),
    ConnectionClosed,
    ResponseTooLarge,
}

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelError::Io(e) => write!(f, "IO error: {e}"),
            TunnelError::Http(e) => write!(f, "HTTP error: {e}"),
            TunnelError::ConnectionClosed => write!(f, "Connection closed"),
            TunnelError::ResponseTooLarge => write!(f, "Response headers too large"),
        }
    }
}

impl std::error::Error for TunnelError {}

impl From<std::io::Error> for TunnelError {
    fn from(error: std::io::Error) -> Self {
        TunnelError::Io(error)
    }
}

impl From<httparse::Error> for TunnelError {
    fn from(error: httparse::Error) -> Self {
        TunnelError::Http(format!("HTTP parsing error: {error}"))
    }
}

impl From<&str> for TunnelError {
    fn from(error: &str) -> Self {
        TunnelError::Http(error.to_string())
    }
}

async fn connect_stream(hostname: &str, port: u16) -> Result<TcpStream, TunnelError> {
    let tcp_stream = TcpStream::connect(format!("{hostname}:{port}")).await?;
    Ok(tcp_stream)
}

async fn parse_http_response(stream: &mut TcpStream) -> Result<u16, TunnelError> {
    let mut response_buf = vec![0u8; 8192];
    let mut total_read = 0;

    loop {
        let n = stream.read(&mut response_buf[total_read..]).await?;
        if n == 0 {
            return Err(TunnelError::ConnectionClosed);
        }

        total_read += n;

        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = Response::new(&mut headers);

        match response.parse(&response_buf[..total_read])? {
            Status::Complete(_) => {
                let status_code = response.code.ok_or("Missing status code")?;
                return Ok(status_code);
            }
            Status::Partial => {
                if total_read >= response_buf.len() {
                    return Err(TunnelError::ResponseTooLarge);
                }
                // Need more data, continue reading
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_http_request(
    method: &str,
    user_agent: &str,
    path: &str,
    session_id: &str,
    hostname: &str,
    port: u16,
    extra_headers: &[(String, String)],
    is_post: bool,
) -> String {
    // See gst-plugins-base/gst-libs/gst/rtsp/gstrtspconnection.c.
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {hostname}:{port}\r\n\
         User-Agent: {user_agent}\r\n\
         x-sessioncookie: {session_id}\r\n\
         Accept: application/x-rtsp-tunnelled\r\n\
         Cache-Control: no-cache\r\n"
    );

    // RTSP over HTTP specification:
    // https://web.archive.org/web/20240215060448if_/https://opensource.apple.com/source/QuickTimeStreamingServer/QuickTimeStreamingServer-412.42/Documentation/RTSP_Over_HTTP.pdf
    if is_post {
        // Expires is given this particular value in the past to ensure
        // compatibility with proxies that might not understand cache
        // control directive.
        // Content-Length is chosen as 32767 to keep the channel open.
        // Also see Page 5 of RTSP over HTTP specification.
        request.push_str(
            "Content-Type: application/x-rtsp-tunnelled\r\n\
             Pragma: no-cache\r\n\
             Expires: Sun, 9 Jan 1972 00:00:00 GMT\r\n\
             Content-Length: 32767\r\n",
        );
    }

    for (key, value) in extra_headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }

    request.push_str("\r\n");

    request
}

impl TunnelStream {
    pub async fn new(
        url: url::Url,
        extra_headers: &[(String, String)],
        session_id: String,
        user_agent: &str,
    ) -> Result<Self, TunnelError> {
        let hostname = url
            .host_str()
            .ok_or(TunnelError::Http("Missing hostname".to_string()))?
            .to_string();
        let port = url.port().unwrap_or(554);
        let path = url.path().to_string();

        gst::debug!(
            super::imp::CAT,
            "Setting up HTTP tunnel to {hostname}:{port}"
        );

        let mut stream = connect_stream(&hostname, port).await?;

        let get_request = build_http_request(
            "GET",
            user_agent,
            &path,
            &session_id,
            &hostname,
            port,
            extra_headers,
            false,
        );

        gst::trace!(super::imp::CAT, "GET request: {get_request}");

        stream.write_all(get_request.as_bytes()).await?;
        stream.flush().await?;

        let status = parse_http_response(&mut stream).await?;
        if status != 200 {
            return Err(TunnelError::Http(format!(
                "GET request failed with status {status}"
            )));
        }

        gst::debug!(super::imp::CAT, "GET connection established");

        let mut write_stream = connect_stream(&hostname, port).await?;

        let post_request = build_http_request(
            "POST",
            user_agent,
            &path,
            &session_id,
            &hostname,
            port,
            extra_headers,
            true,
        );

        gst::trace!(super::imp::CAT, "POST request: {post_request}");

        write_stream.write_all(post_request.as_bytes()).await?;
        write_stream.flush().await?;

        let status = parse_http_response(&mut write_stream).await?;
        if status != 200 {
            return Err(TunnelError::Http(format!(
                "POST request failed with status: {status}"
            )));
        }

        gst::debug!(super::imp::CAT, "POST connection established");

        let (sink, mut write_rx) = mpsc::unbounded_channel::<Bytes>();

        let handle = tokio::spawn(async move {
            while let Some(data) = write_rx.recv().await {
                if let Err(err) = write_stream.write_all(&data).await {
                    gst::error!(super::imp::CAT, "POST write failed: {err:?}");
                    break;
                }

                if let Err(err) = write_stream.flush().await {
                    gst::error!(super::imp::CAT, "POST flush failed: {err:?}");
                    break;
                }
            }

            gst::debug!(super::imp::CAT, "POST writer task exiting");
        });

        gst::info!(super::imp::CAT, "HTTP tunnel established successfully");

        Ok(Self {
            stream,
            sink,
            handle,
        })
    }
}

impl AsyncRead for TunnelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TunnelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let encoded = BASE64.encode(buf);
        let bytes = Bytes::from(encoded);

        match self.sink.send(bytes) {
            Ok(_) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "POST channel closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.handle.abort();
        Poll::Ready(Ok(()))
    }
}
