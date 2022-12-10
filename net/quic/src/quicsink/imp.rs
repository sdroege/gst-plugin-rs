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
use std::net::SocketAddr;
use std::sync::Mutex;

static DEFAULT_SERVER_NAME: &str = "localhost";
static DEFAULT_SERVER_ADDR: &str = "127.0.0.1:5000";
static DEFAULT_CLIENT_ADDR: &str = "127.0.0.1:5001";
const DEFAULT_TIMEOUT: u32 = 15;
const DEFAULT_SECURE_CONNECTION: bool = true;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("quicsink", gst::DebugColorFlags::empty(), Some("QUIC Sink"))
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
    client_address: SocketAddr,
    server_address: SocketAddr,
    server_name: String,
    timeout: u32,
    secure_conn: bool,
    use_datagram: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            client_address: DEFAULT_CLIENT_ADDR.parse::<SocketAddr>().unwrap(),
            server_address: DEFAULT_SERVER_ADDR.parse::<SocketAddr>().unwrap(),
            server_name: DEFAULT_SERVER_NAME.to_string(),
            timeout: DEFAULT_TIMEOUT,
            secure_conn: DEFAULT_SECURE_CONNECTION,
            use_datagram: false,
        }
    }
}

pub struct QuicSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<Option<future::AbortHandle>>,
}

impl Default for QuicSink {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            canceller: Mutex::new(None),
        }
    }
}

impl GstObjectImpl for QuicSink {}

impl ElementImpl for QuicSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "QUIC Sink",
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
}

impl ObjectImpl for QuicSink {
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
                    .blurb("Address of the QUIC server to connect to e.g. 127.0.0.1:5000")
                    .build(),
                glib::ParamSpecString::builder("client-address")
                    .nick("QUIC client address")
                    .blurb("Address to be used by this QUIC client e.g. 127.0.0.1:5001")
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout QUIC endpoint requests (0 = No timeout).")
                    .maximum(3600)
                    .default_value(DEFAULT_TIMEOUT)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoolean::builder("secure-connection")
                    .nick("Use secure connection")
                    .blurb("Use certificates for QUIC connection. False: Insecure connection, True: Secure connection.")
                    .default_value(DEFAULT_SECURE_CONNECTION)
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
        match pspec.name() {
            "server-name" => {
                let mut settings = self.settings.lock().unwrap();
                settings.server_name = value.get::<String>().expect("type checked upstream");
            }
            "server-address" => {
                let addr = value.get::<String>().expect("type checked upstream");
                let addr = make_socket_addr(&addr);
                match addr {
                    Ok(server_address) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.server_address = server_address;
                    }
                    Err(e) => gst::element_imp_error!(
                        self,
                        gst::ResourceError::Failed,
                        ["Invalid server address: {}", e]
                    ),
                }
            }
            "client-address" => {
                let addr = value.get::<String>().expect("type checked upstream");
                let addr = make_socket_addr(&addr);
                match addr {
                    Ok(client_address) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.client_address = client_address;
                    }
                    Err(e) => gst::element_imp_error!(
                        self,
                        gst::ResourceError::Failed,
                        ["Invalid client address: {}", e]
                    ),
                }
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                settings.timeout = value.get().expect("type checked upstream");
            }
            "secure-connection" => {
                let mut settings = self.settings.lock().unwrap();
                settings.secure_conn = value.get().expect("type checked upstream");
            }
            "use-datagram" => {
                let mut settings = self.settings.lock().unwrap();
                settings.use_datagram = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "server-name" => {
                let settings = self.settings.lock().unwrap();
                settings.server_name.to_value()
            }
            "server-address" => {
                let settings = self.settings.lock().unwrap();
                settings.server_address.to_string().to_value()
            }
            "client-address" => {
                let settings = self.settings.lock().unwrap();
                settings.client_address.to_string().to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            "secure-connection" => {
                let settings = self.settings.lock().unwrap();
                settings.secure_conn.to_value()
            }
            "use-datagram" => {
                let settings = self.settings.lock().unwrap();
                settings.use_datagram.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for QuicSink {
    const NAME: &'static str = "GstQUICSink";
    type Type = super::QuicSink;
    type ParentType = gst_base::BaseSink;
}

impl BaseSinkImpl for QuicSink {
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
                match wait(&self.canceller, send.finish(), timeout) {
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

impl QuicSink {
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
        let use_datagram;
        let secure_conn;

        {
            let settings = self.settings.lock().unwrap();
            client_addr = settings.client_address;
            server_addr = settings.server_address;
            server_name = settings.server_name.clone();
            use_datagram = settings.use_datagram;
            secure_conn = settings.secure_conn;
        }

        let endpoint = client_endpoint(client_addr, secure_conn).map_err(|err| {
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
