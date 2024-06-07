// Copyright (C) 2019 Amazon.com, Inc. or its affiliates <mkolny@amazon.com>
// Copyright (C) 2023 Asymptotic Inc
//      Author: Arun Raghavan <arun@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use aws_sdk_s3::{
    config::{self, retry::RetryConfig, Credentials, Region},
    error::ProvideErrorMetadata,
    operation::put_object::builders::PutObjectFluentBuilder,
    primitives::ByteStream,
    Client,
};

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::convert::From;
use std::sync::Mutex;
use std::time::Duration;

use crate::s3url::*;
use crate::s3utils::{self, duration_from_millis, duration_to_millis, WaitError};

const DEFAULT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_FLUSH_INTERVAL_BUFFERS: u64 = 1;
const DEFAULT_FLUSH_INTERVAL_BYTES: u64 = 0;
const DEFAULT_FLUSH_INTERVAL_TIME: gst::ClockTime = gst::ClockTime::from_nseconds(0);
const DEFAULT_FLUSH_ON_ERROR: bool = false;
const DEFAULT_FORCE_PATH_STYLE: bool = false;

// General setting for create / abort requests
const DEFAULT_REQUEST_TIMEOUT_MSEC: u64 = 15_000;

struct Started {
    client: Client,
    buffer: Vec<u8>,
    start_pts: Option<gst::ClockTime>,
    num_buffers: u64,
    need_flush: bool,
}

impl Started {
    pub fn new(client: Client, buffer: Vec<u8>) -> Started {
        Started {
            client,
            buffer,
            start_pts: gst::ClockTime::NONE,
            num_buffers: 0,
            need_flush: false,
        }
    }
}

#[derive(Default)]
enum State {
    #[default]
    Stopped,
    Started(Started),
}

struct Settings {
    region: Region,
    bucket: Option<String>,
    key: Option<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    metadata: Option<gst::Structure>,
    retry_attempts: u32,
    request_timeout: Duration,
    endpoint_uri: Option<String>,
    force_path_style: bool,
    flush_interval_buffers: u64,
    flush_interval_bytes: u64,
    flush_interval_time: Option<gst::ClockTime>,
    flush_on_error: bool,
}

impl Settings {
    fn to_uri(&self) -> String {
        GstS3Url {
            region: self.region.clone(),
            bucket: self.bucket.clone().unwrap(),
            object: self.key.clone().unwrap(),
            version: None,
        }
        .to_string()
    }

    fn to_metadata(&self, imp: &S3PutObjectSink) -> Option<HashMap<String, String>> {
        self.metadata.as_ref().map(|structure| {
            let mut hash = HashMap::new();

            for (key, value) in structure.iter() {
                if let Ok(Ok(value_str)) = value.transform::<String>().map(|v| v.get()) {
                    gst::log!(CAT, imp: imp, "metadata '{}' -> '{}'", key, value_str);
                    hash.insert(key.to_string(), value_str);
                } else {
                    gst::warning!(
                        CAT,
                        imp: imp,
                        "Failed to convert metadata '{}' to string ('{:?}')",
                        key,
                        value
                    );
                }
            }

            hash
        })
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            region: Region::new("us-west-2"),
            bucket: None,
            key: None,
            content_type: None,
            content_disposition: None,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            metadata: None,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MSEC),
            endpoint_uri: None,
            force_path_style: DEFAULT_FORCE_PATH_STYLE,
            flush_interval_buffers: DEFAULT_FLUSH_INTERVAL_BUFFERS,
            flush_interval_bytes: DEFAULT_FLUSH_INTERVAL_BYTES,
            flush_interval_time: Some(DEFAULT_FLUSH_INTERVAL_TIME),
            flush_on_error: DEFAULT_FLUSH_ON_ERROR,
        }
    }
}

#[derive(Default)]
pub struct S3PutObjectSink {
    url: Mutex<Option<GstS3Url>>,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<s3utils::Canceller>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "awss3putobjectsink",
        gst::DebugColorFlags::empty(),
        Some("Amazon S3 PutObject Sink"),
    )
});

impl S3PutObjectSink {
    fn check_thresholds(
        &self,
        state: &Started,
        pts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
    ) -> bool {
        let settings = self.settings.lock().unwrap();

        #[allow(clippy::if_same_then_else)]
        #[allow(clippy::needless_bool)]
        // Verbose if/else form for readability
        if settings.flush_interval_buffers > 0
            && (state.num_buffers % settings.flush_interval_buffers) == 0
        {
            true
        } else if settings.flush_interval_bytes > 0
            && (state.buffer.len() as u64 % settings.flush_interval_bytes) == 0
        {
            true
        } else if settings.flush_interval_time.is_some()
            && settings.flush_interval_time.unwrap() != DEFAULT_FLUSH_INTERVAL_TIME
            && state.start_pts.is_some()
            && pts.is_some()
            && duration.is_some()
            && (pts.unwrap() - state.start_pts.unwrap() + duration.unwrap())
                % settings.flush_interval_time.unwrap()
                == gst::ClockTime::from_nseconds(0)
        {
            true
        } else {
            false
        }
    }

    fn flush_buffer(&self) -> Result<(), Option<gst::ErrorMessage>> {
        let put_object_req = self.create_put_object_request();

        let put_object_req_future = put_object_req.send();
        let _output =
            s3utils::wait(&self.canceller, put_object_req_future).map_err(|err| match err {
                WaitError::FutureError(err) => Some(gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to upload object: {err}: {}", err.meta()]
                )),
                WaitError::Cancelled => None,
            })?;

        gst::debug!(CAT, imp: self, "Upload complete");

        Ok(())
    }

    fn create_put_object_request(&self) -> PutObjectFluentBuilder {
        let url = self.url.lock().unwrap();
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        let body = Some(ByteStream::from(state.buffer.clone()));

        let bucket = Some(url.as_ref().unwrap().bucket.to_owned());
        let key = Some(url.as_ref().unwrap().object.to_owned());
        let metadata = settings.to_metadata(self);

        let client = &state.client;

        client
            .put_object()
            .set_body(body)
            .set_bucket(bucket)
            .set_key(key)
            .set_metadata(metadata)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        if let State::Started { .. } = *state {
            unreachable!("Element should be started");
        }

        let s3url = {
            let url = self.url.lock().unwrap();
            match *url {
                Some(ref url) => url.clone(),
                None => {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Settings,
                        ["Cannot start without a URL being set"]
                    ));
                }
            }
        };

        let timeout_config = s3utils::timeout_config(settings.request_timeout);

        let cred = match (
            settings.access_key.as_ref(),
            settings.secret_access_key.as_ref(),
        ) {
            (Some(access_key), Some(secret_access_key)) => Some(Credentials::new(
                access_key.clone(),
                secret_access_key.clone(),
                settings.session_token.clone(),
                None,
                "aws-s3-putobject-sink",
            )),
            _ => None,
        };

        let sdk_config =
            s3utils::wait_config(&self.canceller, s3url.region.clone(), timeout_config, cred)
                .map_err(|err| match err {
                    WaitError::FutureError(err) => gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to create SDK config: {}", err]
                    ),
                    WaitError::Cancelled => {
                        gst::error_msg!(
                            gst::LibraryError::Failed,
                            ["SDK config request interrupted during start"]
                        )
                    }
                })?;

        let config_builder = config::Builder::from(&sdk_config)
            .force_path_style(settings.force_path_style)
            .retry_config(RetryConfig::standard().with_max_attempts(settings.retry_attempts));

        let config = if let Some(ref uri) = settings.endpoint_uri {
            config_builder.endpoint_url(uri).build()
        } else {
            config_builder.build()
        };

        let client = Client::from_conf(config);

        *state = State::Started(Started::new(client, Vec::new()));

        Ok(())
    }

    fn set_uri(self: &S3PutObjectSink, url_str: Option<&str>) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();

        if let State::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Cannot set URI on a started s3sink",
            ));
        }

        let mut url = self.url.lock().unwrap();

        if url_str.is_none() {
            *url = None;
            return Ok(());
        }

        gst::debug!(CAT, imp: self, "Setting uri to {:?}", url_str);

        let url_str = url_str.unwrap();
        match parse_s3_url(url_str) {
            Ok(s3url) => {
                *url = Some(s3url);
                Ok(())
            }
            Err(_) => Err(glib::Error::new(
                gst::URIError::BadUri,
                "Could not parse URI",
            )),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for S3PutObjectSink {
    const NAME: &'static str = "GstAwsS3PutObjectSink";
    type Type = super::S3PutObjectSink;
    type ParentType = gst_base::BaseSink;
}

impl ObjectImpl for S3PutObjectSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("bucket")
                    .nick("S3 Bucket")
                    .blurb("The bucket of the file to write")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("key")
                    .nick("S3 Key")
                    .blurb("The key of the file to write")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("region")
                    .nick("AWS Region")
                    .blurb("An AWS region (e.g. eu-west-2).")
                    .default_value(Some("us-west-2"))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("uri")
                    .nick("URI")
                    .blurb("The S3 object URI")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("access-key")
                    .nick("Access Key")
                    .blurb("AWS Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("secret-access-key")
                    .nick("Secret Access Key")
                    .blurb("AWS Secret Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-token")
                    .nick("Session Token")
                    .blurb("AWS temporary Session Token from STS")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("metadata")
                    .nick("Metadata")
                    .blurb("A map of metadata to store with the object in S3; field values need to be convertible to strings.")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("retry-attempts")
                    .nick("Retry attempts")
                    .blurb("Number of times AWS SDK attempts a request before abandoning the request")
                    .minimum(1)
                    .maximum(10)
                    .default_value(DEFAULT_RETRY_ATTEMPTS)
                    .build(),
                glib::ParamSpecInt64::builder("request-timeout")
                    .nick("Request timeout")
                    .blurb("Timeout for general S3 requests (in ms, set to -1 for infinity)")
                    .minimum(-1)
                    .default_value(DEFAULT_REQUEST_TIMEOUT_MSEC as i64)
                    .build(),
                glib::ParamSpecString::builder("endpoint-uri")
                    .nick("S3 endpoint URI")
                    .blurb("The S3 endpoint URI to use")
                    .build(),
                glib::ParamSpecString::builder("content-type")
                    .nick("content-type")
                    .blurb("Content-Type header to set for uploaded object")
                    .build(),
                glib::ParamSpecString::builder("content-disposition")
                    .nick("content-disposition")
                    .blurb("Content-Disposition header to set for uploaded object")
                    .build(),
                glib::ParamSpecUInt64::builder("flush-interval-buffers")
                    .nick("Flush interval in buffers")
                    .blurb("Number of buffers to accumulate before doing a write (0 => disable)")
                    .default_value(DEFAULT_FLUSH_INTERVAL_BUFFERS)
                    .build(),
                glib::ParamSpecUInt64::builder("flush-interval-bytes")
                    .nick("Flush interval in bytes")
                    .blurb("Number of bytes to accumulate before doing a write (0 => disable)")
                    .default_value(DEFAULT_FLUSH_INTERVAL_BYTES)
                    .build(),
                glib::ParamSpecUInt64::builder("flush-interval-time")
                    .nick("Flush interval in duration")
                    .blurb("Total duration of buffers to accumulate before doing a write (0 => disable)")
                    .default_value(DEFAULT_FLUSH_INTERVAL_TIME.nseconds())
                    .build(),
                glib::ParamSpecBoolean::builder("flush-on-error")
                    .nick("Flush on error")
                    .blurb("Whether to write out the data on error (like stopping without an EOS)")
                    .default_value(DEFAULT_FLUSH_ON_ERROR)
                    .build(),
                glib::ParamSpecBoolean::builder("force-path-style")
                    .nick("Force path style")
                    .blurb("Force client to use path-style addressing for buckets")
                    .default_value(DEFAULT_FORCE_PATH_STYLE)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        gst::debug!(
            CAT,
            imp: self,
            "Setting property '{}' to '{:?}'",
            pspec.name(),
            value
        );

        match pspec.name() {
            "bucket" => {
                settings.bucket = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if settings.key.is_some() {
                    let _ = self.set_uri(Some(&settings.to_uri()));
                }
            }
            "key" => {
                settings.key = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if settings.bucket.is_some() {
                    let _ = self.set_uri(Some(&settings.to_uri()));
                }
            }
            "region" => {
                let region = value.get::<String>().expect("type checked upstream");
                settings.region = Region::new(region);
                if settings.key.is_some() && settings.bucket.is_some() {
                    let _ = self.set_uri(Some(&settings.to_uri()));
                }
            }
            "uri" => {
                let _ = self.set_uri(value.get().expect("type checked upstream"));
            }
            "access-key" => {
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "session-token" => {
                settings.session_token = value.get().expect("type checked upstream");
            }
            "metadata" => {
                settings.metadata = value.get().expect("type checked upstream");
            }
            "retry-attempts" => {
                settings.retry_attempts = value.get::<u32>().expect("type checked upstream");
            }
            "request-timeout" => {
                settings.request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "endpoint-uri" => {
                settings.endpoint_uri = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if settings.key.is_some() && settings.bucket.is_some() {
                    let _ = self.set_uri(Some(&settings.to_uri()));
                }
            }
            "content-type" => {
                settings.content_type = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "content-disposition" => {
                settings.content_disposition = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "flush-interval-buffers" => {
                settings.flush_interval_buffers =
                    value.get::<u64>().expect("type checked upstream");
            }
            "flush-interval-bytes" => {
                settings.flush_interval_bytes = value.get::<u64>().expect("type checked upstream");
            }
            "flush-interval-time" => {
                settings.flush_interval_time = value
                    .get::<Option<gst::ClockTime>>()
                    .expect("type checked upstream");
            }
            "flush-on-error" => {
                settings.flush_on_error = value.get::<bool>().expect("type checked upstream");
            }
            "force-path-style" => {
                settings.force_path_style = value.get::<bool>().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "key" => settings.key.to_value(),
            "bucket" => settings.bucket.to_value(),
            "region" => settings.region.to_string().to_value(),
            "uri" => {
                let url = self.url.lock().unwrap();
                let url = match *url {
                    Some(ref url) => url.to_string(),
                    None => "".to_string(),
                };

                url.to_value()
            }
            "access-key" => settings.access_key.to_value(),
            "secret-access-key" => settings.secret_access_key.to_value(),
            "session-token" => settings.session_token.to_value(),
            "metadata" => settings.metadata.to_value(),
            "retry-attempts" => settings.retry_attempts.to_value(),
            "request-timeout" => duration_to_millis(Some(settings.request_timeout)).to_value(),
            "endpoint-uri" => settings.endpoint_uri.to_value(),
            "content-type" => settings.content_type.to_value(),
            "content-disposition" => settings.content_disposition.to_value(),
            "flush-interval-buffers" => settings.flush_interval_buffers.to_value(),
            "flush-interval-bytes" => settings.flush_interval_bytes.to_value(),
            "flush-interval-time" => settings.flush_interval_time.to_value(),
            "flush-on-error" => settings.flush_on_error.to_value(),
            "force-path-style" => settings.force_path_style.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for S3PutObjectSink {}

impl ElementImpl for S3PutObjectSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Amazon S3 PutObject sink",
                "Source/Network",
                "Writes an object to Amazon S3 using PutObject (mostly useful for small files)",
                "Arun Raghavan <arun@asymptotic.io>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSinkImpl for S3PutObjectSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.start()
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        if let State::Started(ref started_state) = *state {
            if settings.flush_on_error && started_state.need_flush {
                drop(settings);
                drop(state);

                gst::warning!(CAT, imp: self, "Stopped without EOS, but flushing");
                if let Err(error_message) = self.flush_buffer() {
                    gst::error!(
                        CAT,
                        imp: self,
                        "Failed to finalize the upload: {:?}",
                        error_message
                    );
                }

                state = self.state.lock().unwrap();
            }
        }

        *state = State::Stopped;
        gst::info!(CAT, imp: self, "Stopped");

        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let started_state = match *state {
            State::Started(ref mut s) => s,
            State::Stopped => {
                gst::element_imp_error!(self, gst::CoreError::Failed, ["Not started yet"]);
                return Err(gst::FlowError::Error);
            }
        };

        if started_state.start_pts.is_none() {
            started_state.start_pts = buffer.pts();
        }

        started_state.num_buffers += 1;
        started_state.need_flush = true;

        gst::trace!(CAT, imp: self, "Rendering {:?}", buffer);
        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        started_state.buffer.extend_from_slice(map.as_slice());

        if !self.check_thresholds(started_state, buffer.pts(), buffer.duration()) {
            return Ok(gst::FlowSuccess::Ok);
        }

        drop(state);

        match self.flush_buffer() {
            Ok(_) => Ok(gst::FlowSuccess::Ok),
            Err(err) => match err {
                Some(error_message) => {
                    gst::error!(CAT, imp: self, "Upload failed: {}", error_message);
                    self.post_error_message(error_message);
                    Err(gst::FlowError::Error)
                }
                _ => {
                    gst::info!(CAT, imp: self, "Upload interrupted. Flushing...");
                    Err(gst::FlowError::Flushing)
                }
            },
        }
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        let mut canceller = self.canceller.lock().unwrap();
        canceller.abort();
        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut canceller = self.canceller.lock().unwrap();
        *canceller = s3utils::Canceller::None;
        Ok(())
    }

    fn event(&self, event: gst::Event) -> bool {
        if let gst::EventView::Eos(_) = event.view() {
            let mut state = self.state.lock().unwrap();

            if let State::Started(ref mut started_state) = *state {
                started_state.need_flush = false;
            }

            drop(state);

            if let Err(error_message) = self.flush_buffer() {
                gst::error!(
                    CAT,
                    imp: self,
                    "Failed to finalize the upload: {:?}",
                    error_message
                );
                return false;
            }
        }

        BaseSinkImplExt::parent_event(self, event)
    }
}
