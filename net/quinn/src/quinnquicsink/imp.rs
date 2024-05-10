// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::utils::{
    client_endpoint, make_socket_addr, wait, WaitError, CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG,
};
use bytes::Bytes;
use futures::future;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::subclass::prelude::*;
use once_cell::sync::Lazy;
use quinn::{Connection, SendStream};
use std::path::PathBuf;
use std::sync::Mutex;

static DEFAULT_SERVER_NAME: &str = "localhost";
static DEFAULT_SERVER_ADDR: &str = "127.0.0.1";
static DEFAULT_SERVER_PORT: u16 = 5000;
static DEFAULT_CLIENT_ADDR: &str = "127.0.0.1";
static DEFAULT_CLIENT_PORT: u16 = 5001;

/*
 * For QUIC transport parameters
 * <https://datatracker.ietf.org/doc/html/rfc9000#section-7.4>
 *
 * A HTTP client might specify "http/1.1" and/or "h2" or "h3".
 * Other well-known values are listed in the at IANA registry at
 * <https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids>.
 */
const DEFAULT_ALPN: &str = "gst-quinn";
const DEFAULT_TIMEOUT: u32 = 15;
const DEFAULT_SECURE_CONNECTION: bool = true;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "quinnquicsink",
        gst::DebugColorFlags::empty(),
        Some("Quinn QUIC Sink"),
    )
});

struct Started {
    connection: Connection,
    stream: Option<SendStream>,
}

#[derive(Default)]
enum State {
    #[default]
    Stopped,
    Started(Started),
}

#[derive(Clone, Debug)]
struct Settings {
    client_address: String,
    client_port: u16,
    server_address: String,
    server_port: u16,
    server_name: String,
    alpns: Vec<String>,
    timeout: u32,
    keep_alive_interval: u64,
    secure_conn: bool,
    use_datagram: bool,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            client_address: DEFAULT_CLIENT_ADDR.to_string(),
            client_port: DEFAULT_CLIENT_PORT,
            server_address: DEFAULT_SERVER_ADDR.to_string(),
            server_port: DEFAULT_SERVER_PORT,
            server_name: DEFAULT_SERVER_NAME.to_string(),
            alpns: vec![DEFAULT_ALPN.to_string()],
            timeout: DEFAULT_TIMEOUT,
            keep_alive_interval: 0,
            secure_conn: DEFAULT_SECURE_CONNECTION,
            use_datagram: false,
            certificate_file: None,
            private_key_file: None,
        }
    }
}

pub struct QuinnQuicSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<Option<future::AbortHandle>>,
}

impl Default for QuinnQuicSink {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            canceller: Mutex::new(None),
        }
    }
}

impl GstObjectImpl for QuinnQuicSink {}

impl ElementImpl for QuinnQuicSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Quinn QUIC Sink",
                "Source/Network/QUIC",
                "Send data over the network via QUIC",
                "Sanchayan Maity <sanchayan@asymptotic.io>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![sink_pad_template]
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

impl ObjectImpl for QuinnQuicSink {
    fn constructed(&self) {
        self.parent_constructed();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("server-name")
                    .nick("QUIC server name")
                    .blurb("Name of the QUIC server which is in server certificate")
                    .build(),
                glib::ParamSpecString::builder("server-address")
                    .nick("QUIC server address")
                    .blurb("Address of the QUIC server to connect to e.g. 127.0.0.1")
                    .build(),
                glib::ParamSpecUInt::builder("server-port")
                    .nick("QUIC server port")
                    .blurb("Port of the QUIC server to connect to e.g. 5000")
                    .maximum(65535)
                    .default_value(DEFAULT_SERVER_PORT as u32)
                    .readwrite()
                    .build(),
                glib::ParamSpecString::builder("client-address")
                    .nick("QUIC client address")
                    .blurb("Address to be used by this QUIC client e.g. 127.0.0.1")
                    .build(),
                glib::ParamSpecUInt::builder("client-port")
                    .nick("QUIC client port")
                    .blurb("Port to be used by this QUIC client e.g. 5001")
                    .maximum(65535)
                    .default_value(DEFAULT_CLIENT_PORT as u32)
                    .readwrite()
                    .build(),
		gst::ParamSpecArray::builder("alpn-protocols")
                    .nick("QUIC ALPN values")
                    .blurb("QUIC connection Application-Layer Protocol Negotiation (ALPN) values")
                    .element_spec(&glib::ParamSpecString::builder("alpn-protocol").build())
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
            "server-address" => {
                settings.server_address = value.get::<String>().expect("type checked upstream");
            }
            "server-port" => {
                settings.server_port = value.get::<u32>().expect("type checked upstream") as u16;
            }
            "client-address" => {
                settings.client_address = value.get::<String>().expect("type checked upstream");
            }
            "client-port" => {
                settings.client_port = value.get::<u32>().expect("type checked upstream") as u16;
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
                    .collect::<Vec<_>>();
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
            "server-address" => settings.server_address.to_string().to_value(),
            "server-port" => {
                let port = settings.server_port as u32;
                port.to_value()
            }
            "client-address" => settings.client_address.to_string().to_value(),
            "client-port" => {
                let port = settings.client_port as u32;
                port.to_value()
            }
            "alpn-protocols" => {
                let alpns = settings.alpns.iter().map(|v| v.as_str());
                gst::Array::new(alpns).to_value()
            }
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
impl ObjectSubclass for QuinnQuicSink {
    const NAME: &'static str = "GstQuinnQuicSink";
    type Type = super::QuinnQuicSink;
    type ParentType = gst_base::BaseSink;
}

impl BaseSinkImpl for QuinnQuicSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        drop(settings);

        let mut state = self.state.lock().unwrap();

        if let State::Started { .. } = *state {
            unreachable!("QuicSink is already started");
        }

        match wait(&self.canceller, self.establish_connection(), timeout) {
            Ok(Ok((c, s))) => {
                *state = State::Started(Started {
                    connection: c,
                    stream: s,
                });

                gst::info!(CAT, imp: self, "Started");

                Ok(())
            }
            Ok(Err(e)) => match e {
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
            Err(e) => {
                gst::error!(CAT, imp: self, "Failed to establish a connection: {:?}", e);
                Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to establish a connection: {:?}", e]
                ))
            }
        }
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        let use_datagram = settings.use_datagram;
        drop(settings);

        let mut state = self.state.lock().unwrap();

        if let State::Started(ref mut state) = *state {
            let connection = &state.connection;
            let mut close_msg = CONNECTION_CLOSE_MSG.to_string();

            if !use_datagram {
                let send = &mut state.stream.as_mut().unwrap();

                // Shutdown stream gracefully
                // send.finish() may fail, but the error is harmless.
                let _ = send.finish();
                match wait(&self.canceller, send.stopped(), timeout) {
                    Ok(r) => {
                        if let Err(e) = r {
                            close_msg = format!("Stream finish request error: {}", e);
                            gst::error!(CAT, imp: self, "{}", close_msg);
                        }
                    }
                    Err(e) => match e {
                        WaitError::FutureAborted => {
                            close_msg = "Stream finish request aborted".to_string();
                            gst::warning!(CAT, imp: self, "{}", close_msg);
                        }
                        WaitError::FutureError(e) => {
                            close_msg = format!("Stream finish request future error: {}", e);
                            gst::error!(CAT, imp: self, "{}", close_msg);
                        }
                    },
                };
            }

            connection.close(CONNECTION_CLOSE_CODE.into(), close_msg.as_bytes());
        }

        *state = State::Stopped;

        gst::info!(CAT, imp: self, "Stopped");

        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let State::Stopped = *self.state.lock().unwrap() {
            gst::element_imp_error!(self, gst::CoreError::Failed, ["Not started yet"]);
            return Err(gst::FlowError::Error);
        }

        gst::trace!(CAT, imp: self, "Rendering {:?}", buffer);

        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        match self.send_buffer(&map) {
            Ok(_) => Ok(gst::FlowSuccess::Ok),
            Err(err) => match err {
                Some(error_message) => {
                    gst::error!(CAT, imp: self, "Data sending failed: {}", error_message);
                    self.post_error_message(error_message);
                    Err(gst::FlowError::Error)
                }
                _ => {
                    gst::info!(CAT, imp: self, "Send interrupted. Flushing...");
                    Err(gst::FlowError::Flushing)
                }
            },
        }
    }
}

impl QuinnQuicSink {
    fn send_buffer(&self, src: &[u8]) -> Result<(), Option<gst::ErrorMessage>> {
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
                    ["Cannot send before start()"]
                )));
            }
        };

        if use_datagram {
            match conn.send_datagram(Bytes::copy_from_slice(src)) {
                Ok(_) => Ok(()),
                Err(e) => Err(Some(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Sending data failed: {}", e]
                ))),
            }
        } else {
            let send = &mut stream.as_mut().unwrap();

            match wait(&self.canceller, send.write_all(src), timeout) {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(Some(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Sending data failed: {}", e]
                ))),
                Err(e) => match e {
                    WaitError::FutureAborted => {
                        gst::warning!(CAT, imp: self, "Sending aborted");
                        Ok(())
                    }
                    WaitError::FutureError(e) => Err(Some(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Sending data failed: {}", e]
                    ))),
                },
            }
        }
    }

    async fn establish_connection(&self) -> Result<(Connection, Option<SendStream>), WaitError> {
        let client_addr;
        let server_addr;
        let server_name;
        let alpns;
        let use_datagram;
        let keep_alive_interval;
        let secure_conn;
        let cert_file;
        let private_key_file;

        {
            let settings = self.settings.lock().unwrap();

            client_addr = make_socket_addr(
                format!("{}:{}", settings.client_address, settings.client_port).as_str(),
            )?;
            server_addr = make_socket_addr(
                format!("{}:{}", settings.server_address, settings.server_port).as_str(),
            )?;

            server_name = settings.server_name.clone();
            alpns = settings.alpns.clone();
            use_datagram = settings.use_datagram;
            keep_alive_interval = settings.keep_alive_interval;
            secure_conn = settings.secure_conn;
            cert_file = settings.certificate_file.clone();
            private_key_file = settings.private_key_file.clone();
        }

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

        let connection = endpoint
            .connect(server_addr, &server_name)
            .unwrap()
            .await
            .map_err(|err| {
                WaitError::FutureError(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Connection error: {}", err]
                ))
            })?;

        let stream = if !use_datagram {
            let res = connection.open_uni().await.map_err(|err| {
                WaitError::FutureError(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to open stream: {}", err]
                ))
            })?;

            Some(res)
        } else {
            None
        };

        Ok((connection, stream))
    }
}
