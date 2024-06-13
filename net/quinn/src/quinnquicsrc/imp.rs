// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::utils::{
    client_endpoint, make_socket_addr, server_endpoint, wait, Canceller, WaitError,
    CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG,
};
use crate::{common::*, utils};
use bytes::Bytes;
use futures::future;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;
use once_cell::sync::Lazy;
use quinn::{Connection, ConnectionError, ReadError, RecvStream};
use std::path::PathBuf;
use std::sync::Mutex;

const DEFAULT_ROLE: QuinnQuicRole = QuinnQuicRole::Server;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "quinnquicsrc",
        gst::DebugColorFlags::empty(),
        Some("Quinn QUIC Source"),
    )
});

struct Started {
    connection: Connection,
    stream: Option<RecvStream>,
}

#[derive(Default)]
enum State {
    #[default]
    Stopped,
    Started(Started),
}

#[derive(Clone, Debug)]
struct Settings {
    address: String,
    port: u16,
    server_name: String,
    bind_address: String,
    bind_port: u16,
    alpns: Vec<String>,
    role: QuinnQuicRole,
    timeout: u32,
    keep_alive_interval: u64,
    secure_conn: bool,
    caps: gst::Caps,
    use_datagram: bool,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            address: DEFAULT_ADDR.to_string(),
            port: DEFAULT_PORT,
            server_name: DEFAULT_SERVER_NAME.to_string(),
            bind_address: DEFAULT_BIND_ADDR.to_string(),
            bind_port: DEFAULT_BIND_PORT,
            alpns: vec![DEFAULT_ALPN.to_string()],
            role: DEFAULT_ROLE,
            timeout: DEFAULT_TIMEOUT,
            keep_alive_interval: 0,
            secure_conn: DEFAULT_SECURE_CONNECTION,
            caps: gst::Caps::new_any(),
            use_datagram: false,
            certificate_file: None,
            private_key_file: None,
        }
    }
}

pub struct QuinnQuicSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<utils::Canceller>,
}

impl Default for QuinnQuicSrc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            canceller: Mutex::new(utils::Canceller::default()),
        }
    }
}

impl GstObjectImpl for QuinnQuicSrc {}

impl ElementImpl for QuinnQuicSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Quinn QUIC Source",
                "Source/Network/QUIC",
                "Receive data over the network via QUIC",
                "Sanchayan Maity <sanchayan@asymptotic.io>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
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
            if settings.secure_conn
                && (settings.certificate_file.is_none() || settings.private_key_file.is_none())
            {
                gst::error!(
                    CAT,
                    imp: self,
                    "Certificate or private key file not provided for secure connection"
                );
                return Err(gst::StateChangeError);
            }
        }

        self.parent_change_state(transition)
    }
}

impl ObjectImpl for QuinnQuicSrc {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj().set_format(gst::Format::Bytes);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
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
                glib::ParamSpecString::builder("bind-address")
                    .nick("QUIC client bind address")
                    .blurb("Address to bind QUIC client e.g. 0.0.0.0")
                    .build(),
                glib::ParamSpecUInt::builder("bind-port")
                    .nick("QUIC client port")
                    .blurb("Port to bind QUIC client e.g. 5001")
                    .maximum(65535)
                    .default_value(DEFAULT_BIND_PORT as u32)
                    .readwrite()
                    .build(),
		gst::ParamSpecArray::builder("alpn-protocols")
                    .nick("QUIC ALPN values")
                    .blurb("QUIC connection Application-Layer Protocol Negotiation (ALPN) values")
                    .element_spec(&glib::ParamSpecString::builder("alpn-protocol").build())
                    .build(),
		glib::ParamSpecEnum::builder_with_default("role", DEFAULT_ROLE)
                    .nick("QUIC role")
                    .blurb("QUIC connection role to use.")
		    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout QUIC endpoint requests (0 = No timeout).")
                    .maximum(3600)
                    .default_value(DEFAULT_TIMEOUT)
                    .readwrite()
                    .build(),
		glib::ParamSpecUInt64::builder("keep-alive-interval")
                    .nick("QUIC connection keep alive interval in ms")
                    .blurb("Keeps QUIC connection alive by periodically pinging the server. Value set in ms, 0 disables this feature")
		    .default_value(0)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("secure-connection")
                    .nick("Use secure connection")
                    .blurb("Use certificates for QUIC connection. False: Insecure connection, True: Secure connection.")
                    .default_value(DEFAULT_SECURE_CONNECTION)
                    .build(),
                glib::ParamSpecString::builder("certificate-file")
                    .nick("Certificate file")
                    .blurb("Path to certificate chain in single file")
                    .build(),
                glib::ParamSpecString::builder("private-key-file")
                    .nick("Private key file")
                    .blurb("Path to a PKCS8 or RSA private key file")
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("caps")
                    .blurb("The caps of the source pad")
                    .build(),
                glib::ParamSpecBoolean::builder("use-datagram")
                    .nick("Use datagram")
                    .blurb("Use datagram for lower latency, unreliable messaging")
                    .default_value(false)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "server-name" => {
                settings.server_name = value.get::<String>().expect("type checked upstream");
            }
            "address" => {
                settings.address = value.get::<String>().expect("type checked upstream");
            }
            "port" => {
                settings.port = value.get::<u32>().expect("type checked upstream") as u16;
            }
            "bind-address" => {
                settings.bind_address = value.get::<String>().expect("type checked upstream");
            }
            "bind-port" => {
                settings.bind_port = value.get::<u32>().expect("type checked upstream") as u16;
            }
            "alpn-protocols" => {
                settings.alpns = value
                    .get::<gst::ArrayRef>()
                    .expect("type checked upstream")
                    .as_slice()
                    .iter()
                    .map(|alpn| {
                        alpn.get::<&str>()
                            .expect("type checked upstream")
                            .to_string()
                    })
                    .collect::<Vec<String>>()
            }
            "role" => {
                settings.role = value.get::<QuinnQuicRole>().expect("type checked upstream");
            }
            "caps" => {
                settings.caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_any);

                let srcpad = self.obj().static_pad("src").expect("source pad expected");
                srcpad.mark_reconfigure();
            }
            "timeout" => {
                settings.timeout = value.get().expect("type checked upstream");
            }
            "keep-alive-interval" => {
                settings.keep_alive_interval = value.get().expect("type checked upstream");
            }
            "secure-connection" => {
                settings.secure_conn = value.get().expect("type checked upstream");
            }
            "certificate-file" => {
                let value: String = value.get().unwrap();
                settings.certificate_file = Some(value.into());
            }
            "private-key-file" => {
                let value: String = value.get().unwrap();
                settings.private_key_file = Some(value.into());
            }
            "use-datagram" => {
                settings.use_datagram = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "server-name" => settings.server_name.to_value(),
            "address" => settings.address.to_string().to_value(),
            "port" => {
                let port = settings.port as u32;
                port.to_value()
            }
            "bind-address" => settings.bind_address.to_string().to_value(),
            "bind-port" => {
                let port = settings.bind_port as u32;
                port.to_value()
            }
            "alpn-protocols" => {
                let alpns = settings.alpns.iter().map(|v| v.as_str());
                gst::Array::new(alpns).to_value()
            }
            "role" => settings.role.to_value(),
            "caps" => settings.caps.to_value(),
            "timeout" => settings.timeout.to_value(),
            "keep-alive-interval" => settings.keep_alive_interval.to_value(),
            "secure-connection" => settings.secure_conn.to_value(),
            "certificate-file" => {
                let certfile = settings.certificate_file.as_ref();
                certfile.and_then(|file| file.to_str()).to_value()
            }
            "private-key-file" => {
                let privkey = settings.private_key_file.as_ref();
                privkey.and_then(|file| file.to_str()).to_value()
            }
            "use-datagram" => settings.use_datagram.to_value(),
            _ => unimplemented!(),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for QuinnQuicSrc {
    const NAME: &'static str = "GstQuinnQuicSrc";
    type Type = super::QuinnQuicSrc;
    type ParentType = gst_base::BaseSrc;
}

impl BaseSrcImpl for QuinnQuicSrc {
    fn is_seekable(&self) -> bool {
        false
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        drop(settings);

        let mut state = self.state.lock().unwrap();

        if let State::Started { .. } = *state {
            unreachable!("QuicSrc already started");
        }

        match wait(&self.canceller, self.init_connection(), timeout) {
            Ok(Ok((c, s))) => {
                *state = State::Started(Started {
                    connection: c,
                    stream: s,
                });

                gst::info!(CAT, imp: self, "Started");

                Ok(())
            }
            Ok(Err(e)) | Err(e) => match e {
                WaitError::FutureAborted => {
                    gst::warning!(CAT, imp: self, "Connection aborted");
                    Ok(())
                }
                WaitError::FutureError(err) => {
                    gst::error!(CAT, imp: self, "Connection request failed: {}", err);
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
            let connection = &state.connection;

            connection.close(
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
                    gst::debug!(CAT, imp: self, "End of stream");
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
                gst::error!(CAT, imp: self, "Could not GET: {}", err);
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

        gst::debug!(CAT, imp: self, "Advertising our own caps: {:?}", &tmp_caps);

        if let Some(filter_caps) = filter {
            gst::debug!(
                CAT,
                imp: self,
                "Intersecting with filter caps: {:?}",
                &filter_caps
            );

            tmp_caps = filter_caps.intersect_with_mode(&tmp_caps, gst::CapsIntersectMode::First);
        };

        gst::debug!(CAT, imp: self, "Returning caps: {:?}", &tmp_caps);

        Some(tmp_caps)
    }
}

impl QuinnQuicSrc {
    fn get(&self, _offset: u64, length: u64) -> Result<Bytes, Option<gst::ErrorMessage>> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        let use_datagram = settings.use_datagram;
        drop(settings);

        let mut state = self.state.lock().unwrap();

        let (conn, stream) = match *state {
            State::Started(Started {
                ref connection,
                ref mut stream,
            }) => (connection, stream),
            State::Stopped => {
                return Err(Some(gst::error_msg!(
                    gst::LibraryError::Failed,
                    ["Cannot get data before start"]
                )));
            }
        };

        let future = async {
            if use_datagram {
                match conn.read_datagram().await {
                    Ok(bytes) => Ok(bytes),
                    Err(err) => match err {
                        ConnectionError::ApplicationClosed(ac) => {
                            gst::info!(CAT, imp: self, "Application closed connection, {}", ac);
                            Ok(Bytes::new())
                        }
                        ConnectionError::ConnectionClosed(cc) => {
                            gst::info!(CAT, imp: self, "Transport closed connection, {}", cc);
                            Ok(Bytes::new())
                        }
                        _ => Err(WaitError::FutureError(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Datagram read error: {}", err]
                        ))),
                    },
                }
            } else {
                let recv = stream.as_mut().unwrap();

                match recv.read_chunk(length as usize, true).await {
                    Ok(Some(chunk)) => Ok(chunk.bytes),
                    Ok(None) => Ok(Bytes::new()),
                    Err(err) => match err {
                        ReadError::ConnectionLost(conn_err) => match conn_err {
                            ConnectionError::ConnectionClosed(cc) => {
                                gst::info!(CAT, imp: self, "Transport closed connection, {}", cc);
                                Ok(Bytes::new())
                            }
                            ConnectionError::ApplicationClosed(ac) => {
                                gst::info!(CAT, imp: self, "Application closed connection, {}", ac);
                                Ok(Bytes::new())
                            }
                            _ => Err(WaitError::FutureError(gst::error_msg!(
                                gst::ResourceError::Failed,
                                ["Stream read error: {}", conn_err]
                            ))),
                        },
                        ReadError::ClosedStream => {
                            gst::info!(CAT, imp: self, "Stream closed");
                            Ok(Bytes::new())
                        }
                        _ => Err(WaitError::FutureError(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Stream read error: {}", err]
                        ))),
                    },
                }
            }
        };

        match wait(&self.canceller, future, timeout) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) | Err(e) => match e {
                WaitError::FutureAborted => {
                    gst::warning!(CAT, imp: self, "Read from stream request aborted");
                    Err(None)
                }
                WaitError::FutureError(e) => {
                    gst::error!(CAT, imp: self, "Failed to read from stream: {}", e);
                    Err(Some(e))
                }
            },
        }
    }

    async fn init_connection(&self) -> Result<(Connection, Option<RecvStream>), WaitError> {
        let server_addr;
        let server_name;
        let client_addr;
        let alpns;
        let role;
        let use_datagram;
        let keep_alive_interval;
        let secure_conn;
        let cert_file;
        let private_key_file;

        {
            let settings = self.settings.lock().unwrap();

            client_addr = make_socket_addr(
                format!("{}:{}", settings.bind_address, settings.bind_port).as_str(),
            )?;

            server_addr =
                make_socket_addr(format!("{}:{}", settings.address, settings.port).as_str())?;

            server_name = settings.server_name.clone();
            alpns = settings.alpns.clone();
            role = settings.role;
            use_datagram = settings.use_datagram;
            keep_alive_interval = settings.keep_alive_interval;
            secure_conn = settings.secure_conn;
            cert_file = settings.certificate_file.clone();
            private_key_file = settings.private_key_file.clone();
        }

        let connection;

        match role {
            QuinnQuicRole::Server => {
                let endpoint = server_endpoint(
                    server_addr,
                    &server_name,
                    secure_conn,
                    alpns,
                    cert_file,
                    private_key_file,
                )
                .map_err(|err| {
                    WaitError::FutureError(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Failed to configure endpoint: {}", err]
                    ))
                })?;

                let incoming_conn = endpoint.accept().await.unwrap();

                connection = incoming_conn.await.map_err(|err| {
                    WaitError::FutureError(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Connection error: {}", err]
                    ))
                })?;
            }
            QuinnQuicRole::Client => {
                let endpoint = client_endpoint(
                    client_addr,
                    secure_conn,
                    alpns,
                    cert_file,
                    private_key_file,
                    keep_alive_interval,
                )
                .map_err(|err| {
                    WaitError::FutureError(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Failed to configure endpoint: {}", err]
                    ))
                })?;

                connection = endpoint
                    .connect(server_addr, &server_name)
                    .unwrap()
                    .await
                    .map_err(|err| {
                        WaitError::FutureError(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Connection error: {}", err]
                        ))
                    })?;
            }
        }

        let stream = if !use_datagram {
            let res = connection.accept_uni().await.map_err(|err| {
                WaitError::FutureError(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to open stream: {}", err]
                ))
            })?;

            Some(res)
        } else {
            match connection.max_datagram_size() {
                Some(datagram_size) => {
                    gst::info!(CAT, imp: self, "Datagram size reported by peer: {datagram_size}");
                }
                None => {
                    return Err(WaitError::FutureError(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Datagram unsupported by the peer"]
                    )));
                }
            }

            None
        };

        gst::info!(
            CAT,
            imp: self,
            "Remote connection accepted: {}",
            connection.remote_address()
        );

        Ok((connection, stream))
    }
}
