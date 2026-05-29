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

use crate::quinnconnection::*;
use crate::quinnquicmeta::QuinnQuicMeta;
use crate::quinnquicquery::*;
use crate::utils::{
    CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG, Canceller, RUNTIME, WaitError, get_stats,
    make_socket_addr, wait,
};
use crate::{common::*, utils};
use async_channel::{Receiver, Sender, unbounded};
use bytes::Bytes;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::{Builder, JoinHandle};
use tokio::net::lookup_host;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use web_transport_quinn::*;

const DATA_HANDLER_THREAD: &str = "data-handler";
const DEFAULT_ROLE: QuinnQuicRole = QuinnQuicRole::Server;
const DEFAULT_IS_LIVE: bool = false;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "quinnwtsrc",
        gst::DebugColorFlags::empty(),
        Some("Quinn WebTransport client source"),
    )
});

enum QuinnData {
    Datagram(Bytes),
    Stream(u64, Bytes),
    Closed(u64),
    Eos,
}

struct Started {
    session: Arc<Session>,
    data_handler: Option<JoinHandle<()>>,
    // TODO: Use tokio channel
    //
    // We use async-channel to keep a clone of the receive channel around
    // for use in every `create` call. tokio's UnboundedReceiver does not
    // implement clone.
    data_rx: Option<Receiver<QuinnData>>,
    thread_quit: Option<oneshot::Sender<()>>,
    socket_addr: SocketAddr,
}

#[allow(clippy::large_enum_variant)]
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
    address: String,
    port: u16,
    server_name: String,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
    certificate_database_file: Option<PathBuf>,
    keep_alive_interval: u64,
    secure_conn: bool,
    timeout: u32,
    transport_config: QuinnQuicTransportConfig,
    role: QuinnQuicRole,
    url: String,
    is_live: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let transport_config = QuinnQuicTransportConfig::default();
        Settings {
            bind_address: DEFAULT_BIND_ADDR.to_string(),
            bind_port: DEFAULT_BIND_PORT,
            address: DEFAULT_ADDR.to_string(),
            port: DEFAULT_PORT,
            server_name: DEFAULT_SERVER_NAME.to_string(),
            certificate_file: None,
            private_key_file: None,
            certificate_database_file: None,
            keep_alive_interval: 0,
            secure_conn: DEFAULT_SECURE_CONNECTION,
            timeout: DEFAULT_TIMEOUT,
            transport_config,
            role: DEFAULT_ROLE,
            url: DEFAULT_ADDR.to_string(),
            is_live: DEFAULT_IS_LIVE,
        }
    }
}

pub struct QuinnWebTransportSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<utils::Canceller>,
    session: Mutex<Option<Arc<Session>>>,
}

impl Default for QuinnWebTransportSrc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            canceller: Mutex::new(utils::Canceller::default()),
            session: Mutex::new(None),
        }
    }
}

impl GstObjectImpl for QuinnWebTransportSrc {}

impl ElementImpl for QuinnWebTransportSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Quinn WebTransport Source",
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

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::NullToReady {
            let settings = self.settings.lock().unwrap();

            /*
             * Fail the state change if a secure connection was requested but
             * no certificate path was provided.
             */
            if settings.secure_conn && settings.certificate_file.is_none() {
                gst::error!(
                    CAT,
                    imp = self,
                    "Certificate or private key file not provided for secure connection"
                );
                return Err(gst::StateChangeError);
            }
        }
        self.parent_change_state(transition)
    }
}

impl ObjectImpl for QuinnWebTransportSrc {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj().set_format(gst::Format::Time);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("certificate-file")
                    .nick("Certificate file")
                    .blurb("Path to certificate chain for the private key file in PEM format")
                    .build(),
                glib::ParamSpecString::builder("private-key-file")
                    .nick("Private key file")
                    .blurb("Path to a PKCS1, PKCS8 or SEC1 private key file in PEM format")
                    .build(),
                glib::ParamSpecString::builder("certificate-database-file")
                    .nick("Certificate database file")
                    .blurb("Path to a certificate database file in PEM format used for certificate validation")
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
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .nick("Connection statistics")
                    .blurb("Connection statistics")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("secure-connection")
                    .nick("Use secure connection.")
                    .blurb("Use certificates for QUIC connection. False: Insecure connection, True: Secure connection.")
                    .default_value(DEFAULT_SECURE_CONNECTION)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("role", DEFAULT_ROLE)
                    .nick("WebTransport role")
                    .blurb("WebTransport session role to use.")
                    .build(),
                glib::ParamSpecString::builder("server-name")
                    .nick("QUIC server name")
                    .blurb("Name of the QUIC server which is in server certificate in case of server role")
                    .build(),
                glib::ParamSpecString::builder("address")
                    .nick("QUIC server address")
                    .blurb("Address of the QUIC server e.g. 127.0.0.1")
                    .build(),
                glib::ParamSpecUInt::builder("port")
                    .nick("QUIC server port")
                    .blurb("Port of the QUIC server e.g. 5000")
                    .maximum(65535)
                    .default_value(DEFAULT_PORT as u32)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("is-live")
                    .nick("Is live")
                    .blurb("Act like a live source")
                    .default_value(DEFAULT_IS_LIVE)
                    .readwrite()
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "certificate-file" => {
                let value: String = value.get().unwrap();
                settings.certificate_file = Some(value.into());
            }
            "private-key-file" => {
                let value: String = value.get().unwrap();
                settings.private_key_file = Some(value.into());
            }
            "certificate-database-file" => {
                let value: String = value.get().unwrap();
                settings.certificate_database_file = Some(value.into());
            }
            "keep-alive-interval" => {
                settings.keep_alive_interval = value.get().expect("type checked upstream");
            }
            "timeout" => {
                settings.timeout = value.get().expect("type checked upstream");
            }
            "secure-connection" => {
                settings.secure_conn = value.get().expect("type checked upstream");
            }
            "url" => {
                settings.url = value.get::<String>().expect("type checked upstream");
            }
            "role" => {
                settings.role = value.get::<QuinnQuicRole>().expect("type checked upstream");
            }
            "server-name" => {
                settings.server_name = value.get::<String>().expect("type checked upstream");
            }
            "address" => {
                settings.address = value.get::<String>().expect("type checked upstream");
            }
            "port" => {
                settings.port = value.get::<u32>().expect("type checked upstream") as u16;
            }
            "is-live" => {
                settings.is_live = value.get::<bool>().expect("type checked upstream");
                self.obj().set_live(settings.is_live);
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "certificate-file" => {
                let certfile = settings.certificate_file.as_ref();
                certfile.to_value()
            }
            "private-key-file" => {
                let privkey = settings.private_key_file.as_ref();
                privkey.to_value()
            }
            "certificate-database-file" => {
                let certfile = settings.certificate_database_file.as_ref();
                certfile.to_value()
            }
            "keep-alive-interval" => settings.keep_alive_interval.to_value(),
            "timeout" => settings.timeout.to_value(),
            "url" => settings.url.to_value(),
            "secure-connection" => settings.secure_conn.to_value(),
            "stats" => {
                let state = self.state.lock().unwrap();
                match *state {
                    State::Started(ref state) => {
                        get_stats(Some((**state.session).stats())).to_value()
                    }
                    State::Stopped => get_stats(None).to_value(),
                }
            }
            "role" => settings.role.to_value(),
            "server-name" => settings.server_name.to_value(),
            "address" => settings.address.to_value(),
            "port" => (settings.port as u32).to_value(),
            "is-live" => settings.is_live.to_value(),
            _ => unimplemented!(),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for QuinnWebTransportSrc {
    const NAME: &'static str = "GstQuinnWebTransportSrc";
    type Type = super::QuinnWebTransportSrc;
    type ParentType = gst_base::PushSrc;
}

impl BaseSrcImpl for QuinnWebTransportSrc {
    fn is_seekable(&self) -> bool {
        false
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Context(c) => {
                gst::debug!(CAT, imp = self, "Handling Context query");

                if let Some(context) = self.set_session_context() {
                    gst::info!(CAT, imp = self, "Setting Quinn Session Context");
                    c.set_context(&context);
                    return true;
                }

                let (role, url, endpoint_config) = match self.get_epconfig() {
                    Ok(config) => config,
                    Err(err) => {
                        gst::error!(CAT, imp = self, "Failed to get endpoint config: {}", err);
                        return false;
                    }
                };

                match self.setup_session(role, url, endpoint_config) {
                    Ok(session) => {
                        let mut conn_guard = self.session.lock().unwrap();
                        *conn_guard = Some(session);
                        drop(conn_guard);

                        match self.set_session_context() {
                            Some(context) => {
                                gst::info!(CAT, imp = self, "Setting Quinn Session Context");
                                c.set_context(&context);
                                true
                            }
                            None => {
                                gst::error!(CAT, imp = self, "Failed to set context");
                                false
                            }
                        }
                    }
                    Err(e) => {
                        gst::error!(CAT, imp = self, "Failed to setup session, {e:?}");
                        false
                    }
                }
            }
            _ => BaseSrcImplExt::parent_query(self, query),
        }
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            unreachable!("QuinnWebTransportSrc already started");
        }
        drop(state);

        let (role, url, endpoint_config) = self.get_epconfig()?;
        let server_addr = endpoint_config.server_addr;

        let sess_guard = self.session.lock().unwrap();
        let session = match *sess_guard {
            Some(ref s) => {
                // We will end up here if downstream MoQ demuxer
                // requested for a Session before we could
                // set up.
                gst::info!(
                    CAT,
                    imp = self,
                    "Using existing connection with ID: {}",
                    s.stable_id()
                );
                s.clone()
            }
            None => self.setup_session(role, url, endpoint_config)?,
        };
        drop(sess_guard);

        let sess_clone = session.clone();
        let (tx_quit, rx_quit): (oneshot::Sender<()>, oneshot::Receiver<()>) = oneshot::channel();
        let (data_tx, data_rx): (Sender<QuinnData>, Receiver<QuinnData>) = unbounded();

        let self_ = self.ref_counted();
        let data_handler = Builder::new()
            .name(DATA_HANDLER_THREAD.to_string())
            .spawn(move || {
                self_.handle_data(sess_clone, data_tx, rx_quit);
                gst::debug!(CAT, imp = self_, "Data handler thread exit");
            })
            .unwrap();

        let mut state = self.state.lock().unwrap();
        *state = State::Started(Started {
            session,
            data_handler: Some(data_handler),
            data_rx: Some(data_rx),
            thread_quit: Some(tx_quit),
            socket_addr: server_addr,
        });
        drop(state);

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if let State::Started(ref mut state) = *state {
            if let Some(channel) = state.thread_quit.take() {
                gst::debug!(CAT, imp = self, "Signalling threads to exit");
                let _ = channel.send(());
            }

            gst::debug!(CAT, imp = self, "Joining data handler thread");
            if let Some(handle) = state.data_handler.take() {
                match handle.join() {
                    Ok(_) => gst::debug!(CAT, imp = self, "Joined data handler thread"),
                    Err(e) => {
                        gst::error!(CAT, imp = self, "Failed to join data handler thread: {e:?}")
                    }
                }
            }

            RUNTIME.block_on(async {
                state
                    .session
                    .close(CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG.as_bytes());
                state.session.closed().await;
            });

            SharedConnection::remove(state.socket_addr);
        }

        *state = State::Stopped;

        Ok(())
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
}

impl PushSrcImpl for QuinnWebTransportSrc {
    fn create(
        &self,
        _buffer: Option<&mut gst::BufferRef>,
    ) -> Result<CreateSuccess, gst::FlowError> {
        loop {
            // We do not want `create` to return when a stream is closed,
            // but, wait for one of the other streams to receive data.
            match self.get() {
                Ok(Some(QuinnData::Stream(stream_id, bytes))) => {
                    break Ok(self.create_buffer(bytes, Some(stream_id)));
                }
                Ok(Some(QuinnData::Datagram(bytes))) => {
                    break Ok(self.create_buffer(bytes, None));
                }
                Ok(Some(QuinnData::Eos)) => {
                    gst::debug!(CAT, imp = self, "End of stream");
                    break Err(gst::FlowError::Eos);
                }
                Ok(None) => {
                    gst::debug!(CAT, imp = self, "End of stream");
                    break Err(gst::FlowError::Eos);
                }
                Err(None) => {
                    gst::debug!(CAT, imp = self, "Flushing");
                    break Err(gst::FlowError::Flushing);
                }
                Err(Some(err)) => {
                    gst::error!(CAT, imp = self, "Could not GET: {}", err);
                    break Err(gst::FlowError::Error);
                }
                Ok(Some(QuinnData::Closed(stream_id))) => {
                    // Send custom downstream event for demuxer to close
                    // and remove the stream.
                    let srcpad = self.obj().static_pad("src").expect("source pad expected");
                    close_stream(&srcpad, stream_id);
                }
            }
        }
    }
}

impl QuinnWebTransportSrc {
    fn create_buffer(&self, bytes: Bytes, stream_id: Option<u64>) -> CreateSuccess {
        gst::trace!(
            CAT,
            imp = self,
            "Pushing buffer of {} bytes for stream: {stream_id:?}",
            bytes.len()
        );

        let mut buffer = gst::Buffer::from_slice(bytes);
        {
            let buffer = buffer.get_mut().unwrap();
            match stream_id {
                Some(id) => QuinnQuicMeta::add(buffer, id, false),
                None => QuinnQuicMeta::add(buffer, 0, true),
            };
        }

        CreateSuccess::NewBuffer(buffer)
    }

    fn get(&self) -> Result<Option<QuinnData>, Option<gst::ErrorMessage>> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        drop(settings);

        let state = self.state.lock().unwrap();
        let rx_chan = match *state {
            State::Started(ref started) => started.data_rx.clone(),
            State::Stopped => {
                return Err(Some(gst::error_msg!(
                    gst::LibraryError::Failed,
                    ["Cannot get data before start"]
                )));
            }
        };
        drop(state);

        let rx_chan = rx_chan.expect("Channel must be valid here");

        match wait(&self.canceller, rx_chan.recv(), timeout) {
            Ok(Ok(bytes)) => Ok(Some(bytes)),
            Ok(Err(_)) => Ok(None),
            Err(e) => match e {
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

    fn get_epconfig(
        &self,
    ) -> Result<(QuinnQuicRole, Option<url::Url>, QuinnQuicEndpointConfig), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        let role = settings.role;
        let timeout = settings.timeout;
        let secure_conn = settings.secure_conn;
        let server_name = settings.server_name.clone();
        let certificate_file = settings.certificate_file.clone();
        let private_key_file = settings.private_key_file.clone();
        let certificate_database_file = settings.certificate_database_file.clone();
        let keep_alive_interval = settings.keep_alive_interval;
        let transport_config = settings.transport_config;

        let client_addr = match role {
            QuinnQuicRole::Client => Some(
                make_socket_addr(
                    format!("{}:{}", settings.bind_address, settings.bind_port).as_str(),
                )
                .unwrap(),
            ),
            QuinnQuicRole::Server => None,
        };

        let (server_addr, url) = if role == QuinnQuicRole::Client {
            let url = url::Url::parse(&settings.url).map_err(|err| {
                gst::error_msg!(gst::ResourceError::Failed, ["Failed to parse URL: {}", err])
            })?;
            drop(settings);

            let server_port = url.port().unwrap_or(443);
            if !url.has_host() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Cannot parse host for URL"]
                ));
            }
            let host = url.host_str().unwrap();

            // Look up the DNS entry.
            let mut remotes = match wait(&self.canceller, lookup_host((host, server_port)), timeout)
            {
                Ok(Ok(remotes)) => remotes,
                Ok(Err(err)) => {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Host lookup request failed: {err}"]
                    ));
                }
                Err(err) => match err {
                    WaitError::FutureAborted => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Host lookup request aborted"]
                        ));
                    }
                    WaitError::FutureError(err) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Host lookup request failed: {err}"]
                        ));
                    }
                },
            };

            let server_addr = match remotes.next() {
                Some(remote) => Ok(remote),
                None => Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Cannot resolve host name for URL"]
                )),
            }?;

            drop(remotes);

            (server_addr, Some(url))
        } else {
            (
                make_socket_addr(format!("{}:{}", settings.address, settings.port).as_str())
                    .unwrap(),
                None,
            )
        };

        Ok((
            role,
            url,
            QuinnQuicEndpointConfig {
                server_addr,
                server_name,
                client_addr,
                secure_conn,
                alpns: vec![HTTP3_ALPN.to_string()],
                certificate_file,
                private_key_file,
                certificate_database_file,
                keep_alive_interval,
                transport_config,
                webtransport: true,
            },
        ))
    }

    fn handle_connection_error(&self, session_err: SessionError) {
        match session_err {
            SessionError::ConnectionError(err) => {
                gst::error!(CAT, imp = self, "Connection error: {err:?}");
            }
            SessionError::WebTransportError(err) => {
                gst::error!(CAT, imp = self, "WebTransport error: {err:?}");
            }
            SessionError::SendDatagramError(err) => {
                gst::error!(CAT, imp = self, "Send Datagram error: {err:?}");
            }
        }
    }

    fn handle_data(
        &self,
        session: Arc<Session>,
        sender: Sender<QuinnData>,
        receiver: oneshot::Receiver<()>,
    ) {
        let blocksize = self.obj().blocksize() as usize;
        gst::info!(CAT, imp = self, "Using a blocksize of {blocksize} for read",);

        let stream_idx = AtomicU64::new(0);

        let tx_send = |data: QuinnData| async {
            if let Err(err) = sender.send(data).await {
                gst::error!(CAT, imp = self, "Error sending data: {err:?}");
            }
        };

        RUNTIME.block_on(async {
            let (stream_tx, mut stream_rx) = unbounded_channel::<(u64, RecvStream)>();
            let (dg_tx, mut dg_rx) = unbounded_channel::<Bytes>();
            let (err_tx, mut err_rx) = unbounded_channel::<SessionError>();

            let sess = session.clone();
            let err_tx_clone = err_tx.clone();

            tokio::spawn(async move {
                loop {
                    match sess.accept_uni().await {
                        Ok(s) => {
                            let idx = stream_idx.load(Ordering::Relaxed);
                            stream_idx.fetch_add(1, Ordering::Relaxed);

                            if stream_tx.send((idx, s)).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = err_tx_clone.send(err);
                            break;
                        }
                    }
                }
            });

            let sess = session.clone();
            let err_tx_clone = err_tx.clone();
            tokio::spawn(async move {
                loop {
                    match sess.read_datagram().await {
                        Ok(bytes) => {
                            if dg_tx.send(bytes).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = err_tx_clone.send(err);
                            break;
                        }
                    }
                }
            });

            let mut quit = std::pin::pin!(receiver);

            loop {
                tokio::select! {
                    biased;
                    _ = &mut quit => {
                        tx_send(QuinnData::Eos).await;
                        break;
                    }
                    res = err_rx.recv() => {
                        if let Some(err) = res {
                            self.handle_connection_error(err);
                            tx_send(QuinnData::Eos).await;
                        }
                        break;
                    }
                    res = stream_rx.recv() => {
                        match res {
                            Some((stream_id, stream)) => {
                                tokio::spawn(read_stream(stream, stream_id, sender.clone(), blocksize, err_tx.clone()));
                            }
                            None => break,
                        }
                    }
                    res = dg_rx.recv() => {
                        match res {
                            Some(bytes) => tx_send(QuinnData::Datagram(bytes)).await,
                            None => break,
                        }
                    }
                }
            }
        });

        gst::info!(CAT, imp = self, "Quit data handler thread");
    }

    fn setup_session(
        &self,
        role: QuinnQuicRole,
        url: Option<url::Url>,
        endpoint_config: QuinnQuicEndpointConfig,
    ) -> Result<Arc<Session>, gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        drop(settings);

        let mut shared_connection = SharedConnection::get_or_init(endpoint_config.server_addr);
        shared_connection.set_endpoint_config(endpoint_config);

        let session = match shared_connection.connection() {
            Some(c) => match c {
                QuinnConnection::WebTransport(sess) => {
                    gst::info!(
                        CAT,
                        imp = self,
                        "Using existing session with ID: {}",
                        sess.stable_id()
                    );

                    sess
                }
                QuinnConnection::Quic(_) => unreachable!(),
            },
            None => match wait(
                &self.canceller,
                shared_connection.connect(role, url),
                timeout,
            ) {
                Ok(Ok(_)) => {
                    let c = shared_connection
                        .connection()
                        .expect("Connection should be valid here");
                    match c {
                        QuinnConnection::Quic(_) => unreachable!(),
                        QuinnConnection::WebTransport(sess) => {
                            gst::info!(
                                CAT,
                                imp = self,
                                "Using existing session with ID: {}",
                                sess.stable_id()
                            );

                            sess
                        }
                    }
                }
                Ok(Err(e)) | Err(e) => match e {
                    WaitError::FutureAborted => {
                        gst::warning!(CAT, imp = self, "Session aborted");
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Session request aborted"]
                        ));
                    }
                    WaitError::FutureError(err) => {
                        gst::error!(CAT, imp = self, "Session request failed: {}", err);
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Session request failed: {err}"]
                        ));
                    }
                },
            },
        };

        gst::info!(
            CAT,
            imp = self,
            "Remote session established: {}",
            session.remote_address()
        );

        Ok(session)
    }

    fn set_session_context(&self) -> Option<gst::Context> {
        let sess_guard = self.session.lock().unwrap();

        if let Some(ref session) = *sess_guard {
            gst::debug!(CAT, imp = self, "Using already established Connection");

            let conn = QuinnConnectionContext(Arc::new(QuinnConnectionContextInner {
                connection: QuinnConnection::WebTransport(session.clone()),
            }));

            let mut context = gst::Context::new(QUINN_CONNECTION_CONTEXT, true);
            {
                let context = context.get_mut().unwrap();
                let s = context.structure_mut();
                s.set("connection", conn);
            };

            self.obj().set_context(&context);

            let _ = self.obj().post_message(
                gst::message::HaveContext::builder(context.clone())
                    .src(&*self.obj())
                    .build(),
            );

            return Some(context);
        }

        None
    }
}

async fn read_stream(
    mut s: RecvStream,
    stream_id: u64,
    sender: Sender<QuinnData>,
    blocksize: usize,
    err_tx: UnboundedSender<SessionError>,
) {
    loop {
        match s.read_chunk(blocksize, true).await {
            Ok(Some(chunk)) => {
                if let Err(err) = sender.send(QuinnData::Stream(stream_id, chunk.bytes)).await {
                    gst::error!(CAT, "Error sending data: {err:?}");
                    break;
                }
            }
            Ok(None) | Err(ReadError::ClosedStream) => {
                gst::debug!(CAT, "Stream ended: {stream_id}");
                let _ = sender.send(QuinnData::Closed(stream_id)).await;
                break;
            }
            Err(ReadError::SessionError(err)) => {
                gst::error!(CAT, "Connection lost: {err:?}");
                let _ = err_tx.send(err);
                break;
            }
            Err(rerr) => {
                gst::error!(CAT, "Read error on stream {stream_id}: {rerr:?}");
                let _ = sender.send(QuinnData::Closed(stream_id)).await;
                break;
            }
        }
    }
}
