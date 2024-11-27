// Copyright (C) 2024, Fluendo S.A.
//      Author: Andoni Morales Alastruey <amorales@fluendo.com>
//
// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::utils::{
    client_endpoint, get_stats, make_socket_addr, server_endpoint, wait, Canceller,
    QuinnQuicEndpointConfig, WaitError, CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG,
};
use crate::{common::*, utils};
use bytes::{buf, Bytes};
use futures::future;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;
use quinn::{Connection, ConnectionError, TransportConfig};
use rustls::server;
use std::borrow::Borrow;
use std::fmt::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use tokio::net::lookup_host;
use web_transport_quinn::{ReadError, RecvStream, Session, SessionError, ALPN};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "quinnwtclientsrc",
        gst::DebugColorFlags::empty(),
        Some("Quinn WebTransport client source"),
    )
});

struct Started {
    session: Session,
    stream: Option<RecvStream>,
}

#[derive(Default)]
enum State {
    #[default]
    Stopped,
    Started(Started),
}

#[derive(Debug)]
struct Settings {
    bind_address: String,
    bind_port: u16,
    caps: gst::Caps,
    certificate_file: Option<PathBuf>,
    keep_alive_interval: u64,
    timeout: u32,
    transport_config: QuinnQuicTransportConfig,
    url: String,
    use_datagram: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let mut transport_config = QuinnQuicTransportConfig::default();
        // Required for the WebTransport handshake
        transport_config.max_concurrent_bidi_streams = 2u32.into();
        transport_config.max_concurrent_uni_streams = 1u32.into();

        Settings {
            caps: gst::Caps::new_any(),
            bind_address: DEFAULT_BIND_ADDR.to_string(),
            bind_port: DEFAULT_BIND_PORT,
            certificate_file: None,
            keep_alive_interval: 0,
            timeout: DEFAULT_TIMEOUT,
            transport_config,
            url: DEFAULT_ADDR.to_string(),
            use_datagram: DEFAULT_USE_DATAGRAM,
        }
    }
}

pub struct QuinnWebTransportClientSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<utils::Canceller>,
}

impl Default for QuinnWebTransportClientSrc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            canceller: Mutex::new(utils::Canceller::default()),
        }
    }
}

impl GstObjectImpl for QuinnWebTransportClientSrc {}

impl ElementImpl for QuinnWebTransportClientSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Quinn WebTransport Client Source",
                "Source/Network/QUIC",
                "Receive data over the network via WebTransport",
                "Andoni Morales Alastruey <amorales@fluendo.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl ObjectImpl for QuinnWebTransportClientSrc {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj().set_format(gst::Format::Time);
        self.obj().set_live(true);
        self.obj().set_do_timestamp(true);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("caps")
                    .blurb("The caps of the source pad")
                    .build(),
                glib::ParamSpecString::builder("certificate-file")
                    .nick("Certificate file")
                    .blurb("Path to certificate chain in single file")
                    .build(),
		        glib::ParamSpecUInt64::builder("keep-alive-interval")
                    .nick("QUIC connection keep alive interval in ms")
                    .blurb("Keeps QUIC connection alive by periodically pinging the server. Value set in ms, 0 disables this feature")
		            .default_value(0)
                    .readwrite()
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout WebTransport endpoint requests (0 = No timeout).")
                    .maximum(3600)
                    .default_value(DEFAULT_TIMEOUT)
                    .readwrite()
                    .build(),
                glib::ParamSpecString::builder("url")
                    .nick("Server URL")
                    .blurb("URL of the HTTP/3 server to connect to.")
                    .build(),
                glib::ParamSpecBoolean::builder("use-datagram")
                    .nick("Use datagram")
                    .blurb("Use datagram for lower latency, unreliable messaging")
                    .default_value(false)
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .nick("Connection statistics")
                    .blurb("Connection statistics")
                    .read_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "caps" => {
                settings.caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_any);

                let srcpad = self.obj().static_pad("src").expect("source pad expected");
                srcpad.mark_reconfigure();
            }
            "certificate-file" => {
                let value: String = value.get().unwrap();
                settings.certificate_file = Some(value.into());
            }
            "keep-alive-interval" => {
                settings.keep_alive_interval = value.get().expect("type checked upstream");
            }
            "timeout" => {
                settings.timeout = value.get().expect("type checked upstream");
            }
            "url" => {
                settings.url = value.get::<String>().expect("type checked upstream");
            }
            "use-datagram" => {
                settings.use_datagram = value.get::<bool>().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "caps" => settings.caps.to_value(),
            "certificate-file" => {
                let certfile = settings.certificate_file.as_ref();
                certfile.and_then(|file| file.to_str()).to_value()
            }
            "keep-alive-interval" => settings.keep_alive_interval.to_value(),
            "timeout" => settings.timeout.to_value(),
            "url" => settings.url.to_value(),
            "use-datagram" => settings.use_datagram.to_value(),
            "stats" => {
                let state = self.state.lock().unwrap();
                match *state {
                    State::Started(ref state) => get_stats(Some(state.session.stats())).to_value(),
                    State::Stopped => get_stats(None).to_value(),
                }
            }
            _ => unimplemented!(),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for QuinnWebTransportClientSrc {
    const NAME: &'static str = "GstQuinnWebTransportClientSrc";
    type Type = super::QuinnWebTransportClientSrc;
    type ParentType = gst_base::BaseSrc;
}

impl BaseSrcImpl for QuinnWebTransportClientSrc {
    fn is_seekable(&self) -> bool {
        false
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        drop(settings);

        let mut state = self.state.lock().unwrap();

        if let State::Started { .. } = *state {
            unreachable!("QuinnWebTransportClientSrc already started");
        }

        match wait(&self.canceller, self.init_session(), timeout) {
            Ok(Ok((c, s))) => {
                *state = State::Started(Started {
                    session: c,
                    stream: s,
                });

                gst::info!(CAT, imp = self, "Started");

                Ok(())
            }
            Ok(Err(e)) | Err(e) => match e {
                WaitError::FutureAborted => {
                    gst::warning!(CAT, imp = self, "Connection aborted");
                    Ok(())
                }
                WaitError::FutureError(err) => {
                    gst::error!(CAT, imp = self, "Connection request failed: {}", err);
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Connection request failed: {}", err]
                    ))
                }
            },
        }
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if let State::Started(ref mut state) = *state {
            let session = &state.session;

            session.close(
                CONNECTION_CLOSE_CODE.into(),
                CONNECTION_CLOSE_MSG.as_bytes(),
            );
        }

        *state = State::Stopped;

        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Scheduling(q) = query.view_mut() {
            q.set(
                gst::SchedulingFlags::SEQUENTIAL | gst::SchedulingFlags::BANDWIDTH_LIMITED,
                1,
                -1,
                0,
            );
            q.add_scheduling_modes(&[gst::PadMode::Pull, gst::PadMode::Push]);
            return true;
        }

        BaseSrcImplExt::parent_query(self, query)
    }

    fn create(
        &self,
        offset: u64,
        buffer: Option<&mut gst::BufferRef>,
        length: u32,
    ) -> Result<CreateSuccess, gst::FlowError> {
        let data = self.get(offset, u64::from(length));

        match data {
            Ok(bytes) => {
                if bytes.is_empty() {
                    gst::debug!(CAT, imp = self, "End of stream");
                    return Err(gst::FlowError::Eos);
                }

                if let Some(buffer) = buffer {
                    if let Err(copied_bytes) = buffer.copy_from_slice(0, bytes.as_ref()) {
                        buffer.set_size(copied_bytes);
                    }
                    Ok(CreateSuccess::FilledBuffer)
                } else {
                    Ok(CreateSuccess::NewBuffer(gst::Buffer::from_slice(bytes)))
                }
            }
            Err(None) => Err(gst::FlowError::Flushing),
            Err(Some(err)) => {
                gst::error!(CAT, imp = self, "Could not GET: {}", err);
                Err(gst::FlowError::Error)
            }
        }
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        let mut canceller = self.canceller.lock().unwrap();
        canceller.abort();
        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut canceller = self.canceller.lock().unwrap();
        if matches!(&*canceller, Canceller::Cancelled) {
            *canceller = Canceller::None;
        }
        Ok(())
    }

    fn caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        let settings = self.settings.lock().unwrap();

        let mut tmp_caps = settings.caps.clone();

        gst::debug!(CAT, imp = self, "Advertising our own caps: {:?}", &tmp_caps);

        if let Some(filter_caps) = filter {
            gst::debug!(
                CAT,
                imp = self,
                "Intersecting with filter caps: {:?}",
                &filter_caps
            );

            tmp_caps = filter_caps.intersect_with_mode(&tmp_caps, gst::CapsIntersectMode::First);
        };

        gst::debug!(CAT, imp = self, "Returning caps: {:?}", &tmp_caps);

        Some(tmp_caps)
    }
}

impl QuinnWebTransportClientSrc {
    async fn read_stream(
        &self,
        stream: &mut RecvStream,
        length: usize,
    ) -> Result<Bytes, WaitError> {
        match stream.read_chunk(length, true).await {
            Ok(Some(chunk)) => Ok(chunk.bytes),
            Ok(None) => Ok(Bytes::new()),
            Err(err) => match err {
                ReadError::SessionError(conn_err) => match conn_err {
                    SessionError::ConnectionError(ce) => {
                        gst::info!(CAT, imp = self, "Connection error, {}", ce);
                        Ok(Bytes::new())
                    }
                    SessionError::SendDatagramError(sde) => {
                        gst::info!(CAT, imp = self, "Send datagram error, {}", sde);
                        Ok(Bytes::new())
                    }
                    SessionError::WebTransportError(wte) => {
                        gst::info!(CAT, imp = self, "WebTransport error, {}", wte);
                        Ok(Bytes::new())
                    }
                },
                ReadError::ClosedStream => {
                    gst::info!(CAT, imp = self, "Stream closed");
                    Ok(Bytes::new())
                }
                ReadError::Reset(r) => {
                    gst::info!(CAT, imp = self, "Reset, {}", r);
                    Ok(Bytes::new())
                }
                ReadError::InvalidReset(ir) => {
                    gst::info!(CAT, imp = self, "Invalid Reset, {}", ir);
                    Ok(Bytes::new())
                }
                _ => Err(WaitError::FutureError(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Stream read error: {}", err]
                ))),
            },
        }
    }

    async fn read_datagram(&self, session: &Session) -> Result<Bytes, WaitError> {
        match session.read_datagram().await {
            Ok(bytes) => Ok(bytes),
            Err(err) => match err {
                SessionError::ConnectionError(ce) => {
                    gst::info!(CAT, imp = self, "Connection error, {}", ce);
                    Ok(Bytes::new())
                }
                SessionError::SendDatagramError(de) => {
                    gst::info!(CAT, imp = self, "Error sending datagram, {}", de);
                    Ok(Bytes::new())
                }
                SessionError::WebTransportError(we) => {
                    gst::info!(CAT, imp = self, "WebTransport error, {}", we);
                    Ok(Bytes::new())
                }
            },
        }
    }

    fn get(&self, _offset: u64, length: u64) -> Result<Bytes, Option<gst::ErrorMessage>> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        let use_datagram = settings.use_datagram;
        drop(settings);

        let mut state = self.state.lock().unwrap();

        let (session, stream) = match *state {
            State::Started(Started {
                ref session,
                ref mut stream,
            }) => (session, stream),
            State::Stopped => {
                return Err(Some(gst::error_msg!(
                    gst::LibraryError::Failed,
                    ["Cannot get data before start"]
                )));
            }
        };

        let future = async {
            if use_datagram {
                self.read_datagram(session).await
            } else {
                let recv = stream.as_mut().unwrap();
                self.read_stream(recv, length as usize).await
            }
        };

        match wait(&self.canceller, future, timeout) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) | Err(e) => match e {
                WaitError::FutureAborted => {
                    gst::warning!(CAT, imp = self, "Read from stream request aborted");
                    Err(None)
                }
                WaitError::FutureError(e) => {
                    gst::error!(CAT, imp = self, "Failed to read from stream: {}", e);
                    Err(Some(e))
                }
            },
        }
    }

    async fn init_session(&self) -> Result<(Session, Option<RecvStream>), WaitError> {
        let (use_datagram, url, mut endpoint_config) = {
            let settings = self.settings.lock().unwrap();

            let client_addr = make_socket_addr(
                format!("{}:{}", settings.bind_address, settings.bind_port).as_str(),
            )?;

            let url = url::Url::parse(&settings.url).map_err(|err| {
                WaitError::FutureError(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to parse URL: {}", err]
                ))
            })?;

            (
                settings.use_datagram,
                url.clone(),
                QuinnQuicEndpointConfig {
                    server_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 4443), // This will be filled in correctly later
                    server_name: DEFAULT_SERVER_NAME.to_string(),
                    client_addr: Some(client_addr),
                    secure_conn: true,
                    alpns: vec![HTTP3_ALPN.to_string()],
                    certificate_file: settings.certificate_file.clone(),
                    private_key_file: None,
                    keep_alive_interval: settings.keep_alive_interval,
                    transport_config: settings.transport_config,
                    with_client_auth: false,
                },
            )
        };

        let server_port = url.port().unwrap_or(443);

        let host = url.host_str().ok_or_else(|| {
            WaitError::FutureError(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Cannot parse host for URL: {}", url.as_str()]
            ))
        })?;

        // Look up the DNS entry.
        let mut remotes = lookup_host((host, server_port)).await.map_err(|_| {
            WaitError::FutureError(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Cannot resolve host name for URL: {}", url.as_str()]
            ))
        })?;

        // Use the first entry.
        endpoint_config.server_addr = match remotes.next() {
            Some(remote) => Ok(remote),
            None => Err(WaitError::FutureError(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Cannot resolve host name for URL: {}", url.as_str()]
            ))),
        }?;

        let client = client_endpoint(&endpoint_config).map_err(|err| {
            WaitError::FutureError(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Failed to configure endpoint: {}", err]
            ))
        })?;

        let session = web_transport_quinn::connect(&client, &url)
            .await
            .map_err(|err| {
                WaitError::FutureError(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to connect to server: {}", err]
                ))
            })?;

        let stream = if !use_datagram {
            let (_, stream) = session.accept_bi().await.map_err(|err| {
                WaitError::FutureError(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to open stream: {}", err]
                ))
            })?;
            Some(stream)
        } else {
            let max_datagram_size = session.max_datagram_size();
            gst::info!(
                CAT,
                imp = self,
                "Datagram size reported by peer: {max_datagram_size}"
            );
            None
        };

        gst::info!(
            CAT,
            imp = self,
            "Remote connection accepted: {}",
            session.remote_address()
        );

        Ok((session, stream))
    }
}
