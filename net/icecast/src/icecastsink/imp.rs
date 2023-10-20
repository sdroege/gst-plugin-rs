// GStreamer Icecast Sink
//
// Copyright (C) 2023-2025 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Icecast protocol specification: https://gist.github.com/ePirat/adc3b8ba00d85b7e3870

/**
 * SECTION:element-icecastsink
 * @see_also: shout2send
 *
 * Sends an audio stream to an Icecast server.
 *
 * ## Example pipelines
 *
 * |[
 * gst-launch-1.0 uridecodebin3 uri=file:///path/to/audio.file ! audioconvert ! audioresample ! voaacenc ! icecastsink location='ice+http://source:password@rocketserver:8000/radio'
 * ]| This will decode an audio file and re-encode it to AAC-LC and stream it to a local icecast server.
 *
 * |[
 * gst-launch-1.0 uridecodebin3 uri=sdp:///path/to/dante-avio.sdp ! queue ! audioconvert ! audioresample ! voaacenc ! icecastsink location='ice+http://source:password@rocketserver:8000/radio'
 * ]| This will receive an AES67 audio stream specified in the SDP file from the local network, re-encode it to AAC-LC and stream it to a local icecast server.
 *
 * Since: plugins-rs-0.15.0
 */
use atomic_refcell::AtomicRefCell;

use std::sync::{Arc, LazyLock, Mutex};

use url::Url;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use crate::icecastsink::client;
use crate::icecastsink::mediaformat::*;

const DEFAULT_LOCATION: Option<Url> = None;
const DEFAULT_TIMEOUT: u32 = 10_000; // in millisecs
const DEFAULT_PUBLIC: bool = true;
const DEFAULT_STREAM_NAME: Option<String> = None;

#[derive(Debug, Clone)]
struct Settings {
    location: Option<Url>,
    timeout: u32, // millisecs
    public: bool,
    stream_name: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            location: DEFAULT_LOCATION,
            timeout: DEFAULT_TIMEOUT,
            public: DEFAULT_PUBLIC,
            stream_name: DEFAULT_STREAM_NAME,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
enum State {
    #[default]
    // Nothing happening yet
    Stopped,
    // Initiated TCP connection to server and waiting for connect/handshake to complete
    Connecting,
    // Streaming data
    Streaming,
    // Error
    Error,
    // Cancelled
    Cancelled,
}

#[derive(Debug, Default)]
pub struct IcecastSink {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
    client: Mutex<Option<Arc<client::IceClient>>>,
}

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "icecastsink",
        gst::DebugColorFlags::empty(),
        Some("Icecast sink"),
    )
});

impl IcecastSink {
    fn set_location(&self, uri: Option<&str>) -> Result<(), glib::Error> {
        if self.obj().current_state() != gst::State::Null {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Changing the `location` property on a started `icecastsink` is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();

        let Some(uri) = uri else {
            settings.location = DEFAULT_LOCATION;
            return Ok(());
        };

        let uri = Url::parse(uri).map_err(|err| {
            glib::Error::new(
                gst::URIError::BadUri,
                &format!("Failed to parse URI '{uri}': {err:?}"),
            )
        })?;

        if uri.scheme() != "ice+http" {
            return Err(glib::Error::new(
                gst::URIError::UnsupportedProtocol,
                &format!("Unsupported URI scheme '{}'", uri.scheme()),
            ));
        }

        settings.location = Some(uri);

        Ok(())
    }
}

impl ObjectImpl for IcecastSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Icecast server, credentials and mount path, e.g. ice+http://source:p4ssw0rd@ingest.smoothjazz.radio:8000/radio")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Timeout for network activity, in milliseconds")
                    .maximum(60_000)
                    .default_value(DEFAULT_TIMEOUT)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("public")
                    .nick("Public")
                    .blurb("Whether the stream should be listed on the server's stream directory")
                    .default_value(DEFAULT_PUBLIC)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("stream-name")
                    .nick("Stream Name")
                    .blurb("Name of the stream (if not configured server-side for the mount point)")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }
    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let res = match pspec.name() {
            "location" => {
                let location = value.get::<Option<&str>>().expect("type checked upstream");
                self.set_location(location)
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get().expect("type checked upstream");
                settings.timeout = timeout;
                Ok(())
            }
            "public" => {
                let mut settings = self.settings.lock().unwrap();
                let public = value.get().expect("type checked upstream");
                settings.public = public;
                Ok(())
            }
            "stream-name" => {
                let mut settings = self.settings.lock().unwrap();
                settings.stream_name = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                Ok(())
            }
            name => unimplemented!("Property '{name}'"),
        };

        if let Err(err) = res {
            gst::error!(
                CAT,
                imp = self,
                "Failed to set property `{}`: {:?}",
                pspec.name(),
                err
            );
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => {
                let settings = self.settings.lock().unwrap();
                let location = settings.location.as_ref().map(Url::to_string);

                location.to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            "public" => {
                let settings = self.settings.lock().unwrap();
                settings.public.to_value()
            }
            "stream-name" => {
                let settings = self.settings.lock().unwrap();
                settings.stream_name.to_value()
            }
            name => unimplemented!("Property '{name}'"),
        }
    }
}

impl GstObjectImpl for IcecastSink {}

impl ElementImpl for IcecastSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Icecast Sink",
                "Sink/Network",
                "Sends an audio stream to an Icecast server",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &[
                    // MP3/MP2/MP1 audio
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 1)
                        .field("layer", gst::IntRange::new(1, 3))
                        .field("channels", gst::IntRange::new(1, 2))
                        .field(
                            "rate",
                            gst::List::new([
                                8000i32, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000,
                            ]),
                        )
                        .field("parsed", true)
                        .build(),
                    // AAC / ADTS
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", gst::List::new([2i32, 4]))
                        .field(
                            "rate",
                            gst::List::new([48000i32, 96000, 44100, 22050, 11025]),
                        )
                        .field("stream-format", "adts")
                        .field("framed", true)
                        .build(),
                    // FLAC - encoder might not set the framed field, add a flacparse if in doubt
                    // (it's the only way to ensure we're getting proper timestamps on the input)
                    gst::Structure::builder("audio/x-flac")
                        .field("channels", gst::IntRange::new(1, 2))
                        .field(
                            "rate",
                            gst::List::new([48000i32, 96000, 44100, 22050, 11025]),
                        )
                        .field("framed", true)
                        .build(),
                    // Ogg Audio
                    gst::Structure::builder("audio/ogg").build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSinkImpl for IcecastSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        let Some(url) = settings.location.as_ref() else {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                ["No location set"]
            ));
        };

        gst::info!(CAT, imp = self, "Location: {url}",);

        let client = client::IceClient::new(
            url.clone(),
            settings.public,
            settings.stream_name.clone(),
            self.obj().name(), // for debug logging
        )?;

        let mut client_guard = self.client.lock().unwrap();

        *client_guard = Some(Arc::new(client));

        let mut state = self.state.borrow_mut();

        *state = State::Connecting;

        // Only resetting format in stop() because re-connect on error goes through
        // start() as well and needs the format to restart the connection.

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.borrow_mut();

        *state = State::Stopped;

        let mut client_guard = self.client.lock().unwrap();

        *client_guard = None;

        gst::info!(CAT, imp = self, "Stopped");

        Ok(())
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Unlocking");

        let client = self.client();

        client.cancel();

        Ok(())
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::info!(CAT, imp = self, "Got caps {caps}");

        let media_format = MediaFormat::from_caps(caps)?;

        gst::info!(CAT, imp = self, "{media_format:?}");

        let client = self.client();

        client.set_media_format(media_format);

        Ok(())
    }

    // Do the initial connect wait in prepare so it's done before the first buffer gets synced
    // (wouldn't this prevent the pipeline from prerolling though if there's a network problem?)
    fn prepare(&self, _buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        let client = self.client();

        match *state {
            // Nothing to do here if we're already connected
            State::Streaming => return Ok(gst::FlowSuccess::Ok),

            // Keep returning the previous error flow return if we get more buffers
            State::Error => return Err(gst::FlowError::Error),
            State::Cancelled => return Err(gst::FlowError::Flushing),

            _ => {}
        }

        let timeout = { self.settings.lock().unwrap().timeout };

        let res = client.wait_for_connection_and_handshake(timeout);

        if let Err(err) = res {
            if let Some(err_msg) = err {
                gst::info!(CAT, imp = self, "Error {err_msg:?}");
                *state = State::Error;
                self.post_error_message(err_msg);
                return Err(gst::FlowError::Error);
            } else {
                gst::debug!(CAT, imp = self, "Cancelled, flushing");
                *state = State::Cancelled;
                return Err(gst::FlowError::Flushing);
            }
        }

        *state = State::Streaming;
        gst::info!(CAT, imp = self, "Ready to stream");

        Ok(gst::FlowSuccess::Ok)
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        // Keep returning the previous error flow return if we get more buffers
        match *state {
            State::Error => return Err(gst::FlowError::Error),
            State::Cancelled => return Err(gst::FlowError::Flushing),
            _ => {}
        }

        debug_assert_eq!(*state, State::Streaming);

        let map = buffer.map_readable().map_err(|_| {
            gst::error_msg!(gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        let write_data = map.as_slice();

        gst::log!(CAT, imp = self, "Sending {buffer:?}..");

        let client = self.client();

        let timeout = self.settings.lock().unwrap().timeout;

        let res = client.send_data(write_data, timeout);

        if let Err(err) = res {
            if let Some(err_msg) = err {
                gst::info!(CAT, imp = self, "Error {err_msg:?}");
                *state = State::Error;
                self.post_error_message(err_msg);
                return Err(gst::FlowError::Error);
            } else {
                gst::debug!(CAT, imp = self, "Cancelled, flushing");
                *state = State::Cancelled;
                return Err(gst::FlowError::Flushing);
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

impl URIHandlerImpl for IcecastSink {
    const URI_TYPE: gst::URIType = gst::URIType::Sink;

    fn protocols() -> &'static [&'static str] {
        &["ice+http"] // TODO: add "ice+https"
    }

    fn uri(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.location.as_ref().map(Url::to_string)
    }

    fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
        self.set_location(Some(uri))
    }
}

impl IcecastSink {
    // Returns ref to client, only taking lock for a short time
    fn client(&self) -> Arc<client::IceClient> {
        let client_guard = self.client.lock().unwrap();

        client_guard.as_ref().unwrap().clone()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for IcecastSink {
    const NAME: &'static str = "GstIcecastSink";
    type Type = super::IcecastSink;
    type ParentType = gst_base::BaseSink;
    type Interfaces = (gst::URIHandler,);
}
