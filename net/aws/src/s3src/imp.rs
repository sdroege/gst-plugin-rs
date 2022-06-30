// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use bytes::Bytes;
use futures::future;
use once_cell::sync::Lazy;
use std::sync::Mutex;
use std::time::Duration;

use aws_sdk_s3::config;
use aws_sdk_s3::{Client, Credentials, RetryConfig};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use crate::s3url::*;
use crate::s3utils::{self, duration_from_millis, duration_to_millis, WaitError};

const DEFAULT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_REQUEST_TIMEOUT_MSEC: u64 = 15000;
const DEFAULT_RETRY_DURATION_MSEC: u64 = 60_000;

#[allow(clippy::large_enum_variant)]
enum StreamingState {
    Stopped,
    Started {
        url: GstS3Url,
        client: Client,
        size: u64,
    },
}

impl Default for StreamingState {
    fn default() -> StreamingState {
        StreamingState::Stopped
    }
}

struct Settings {
    url: Option<GstS3Url>,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    retry_attempts: u32,
    request_timeout: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        let duration = Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MSEC);
        Self {
            url: None,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            request_timeout: duration,
        }
    }
}

#[derive(Default)]
pub struct S3Src {
    settings: Mutex<Settings>,
    state: Mutex<StreamingState>,
    canceller: Mutex<Option<future::AbortHandle>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "awss3src",
        gst::DebugColorFlags::empty(),
        Some("Amazon S3 Source"),
    )
});

impl S3Src {
    fn cancel(&self) {
        let mut canceller = self.canceller.lock().unwrap();

        if let Some(c) = canceller.take() {
            c.abort()
        };
    }

    fn connect(self: &S3Src, url: &GstS3Url) -> Result<Client, gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
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
                "aws-s3-src",
            )),
            _ => None,
        };

        let sdk_config =
            s3utils::wait_config(&self.canceller, url.region.clone(), timeout_config, cred)
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

        let config = config::Builder::from(&sdk_config)
            .retry_config(RetryConfig::new().with_max_attempts(settings.retry_attempts))
            .build();

        Ok(Client::from_conf(config))
    }

    fn set_uri(self: &S3Src, _: &super::S3Src, url_str: Option<&str>) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();

        if let StreamingState::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Cannot set URI on a started s3src",
            ));
        }

        let mut settings = self.settings.lock().unwrap();

        if url_str.is_none() {
            settings.url = None;
            return Ok(());
        }

        let url_str = url_str.unwrap();
        match parse_s3_url(url_str) {
            Ok(s3url) => {
                settings.url = Some(s3url);
                Ok(())
            }
            Err(_) => Err(glib::Error::new(
                gst::URIError::BadUri,
                "Could not parse URI",
            )),
        }
    }

    fn head(
        self: &S3Src,
        src: &super::S3Src,
        client: &Client,
        url: &GstS3Url,
    ) -> Result<u64, gst::ErrorMessage> {
        let head_object = client
            .head_object()
            .set_bucket(Some(url.bucket.clone()))
            .set_key(Some(url.object.clone()))
            .set_version_id(url.version.clone());
        let head_object_future = head_object.send();

        let output =
            s3utils::wait(&self.canceller, head_object_future).map_err(|err| match err {
                WaitError::FutureError(err) => gst::error_msg!(
                    gst::ResourceError::NotFound,
                    ["Failed to get HEAD object: {:?}", err]
                ),
                WaitError::Cancelled => {
                    gst::error_msg!(
                        gst::LibraryError::Failed,
                        ["Head object request interrupted"]
                    )
                }
            })?;

        gst::info!(
            CAT,
            obj: src,
            "HEAD success, content length = {}",
            output.content_length
        );

        Ok(output.content_length as u64)
    }

    /* Returns the bytes, Some(error) if one occured, or a None error if interrupted */
    fn get(
        self: &S3Src,
        src: &super::S3Src,
        offset: u64,
        length: u64,
    ) -> Result<Bytes, Option<gst::ErrorMessage>> {
        let state = self.state.lock().unwrap();

        let (url, client) = match *state {
            StreamingState::Started {
                ref url,
                ref client,
                ..
            } => (url, client),
            StreamingState::Stopped => {
                return Err(Some(gst::error_msg!(
                    gst::LibraryError::Failed,
                    ["Cannot GET before start()"]
                )));
            }
        };

        let get_object = client
            .get_object()
            .set_bucket(Some(url.bucket.clone()))
            .set_key(Some(url.object.clone()))
            .set_range(Some(format!("bytes={}-{}", offset, offset + length - 1)))
            .set_version_id(url.version.clone());

        gst::debug!(
            CAT,
            obj: src,
            "Requesting range: {}-{}",
            offset,
            offset + length - 1
        );

        let get_object_future = get_object.send();

        let mut output =
            s3utils::wait(&self.canceller, get_object_future).map_err(|err| match err {
                WaitError::FutureError(err) => Some(gst::error_msg!(
                    gst::ResourceError::Read,
                    ["Could not read: {}", err]
                )),
                WaitError::Cancelled => None,
            })?;

        gst::debug!(CAT, obj: src, "Read {} bytes", output.content_length);

        s3utils::wait_stream(&self.canceller, &mut output.body).map_err(|err| match err {
            WaitError::FutureError(err) => Some(gst::error_msg!(
                gst::ResourceError::Read,
                ["Could not read: {}", err]
            )),
            WaitError::Cancelled => None,
        })
    }
}

#[glib::object_subclass]
impl ObjectSubclass for S3Src {
    const NAME: &'static str = "AwsS3Src";
    type Type = super::S3Src;
    type ParentType = gst_base::BaseSrc;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for S3Src {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "uri",
                    "URI",
                    "The S3 object URI",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "access-key",
                    "Access Key",
                    "AWS Access Key",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "secret-access-key",
                    "Secret Access Key",
                    "AWS Secret Access Key",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "session-token",
                    "Session Token",
                    "AWS temporary Session Token from STS",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecInt64::new(
                    "request-timeout",
                    "Request timeout",
                    "Timeout for each S3 request (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_REQUEST_TIMEOUT_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "retry-duration",
                    "Retry duration",
                    "How long we should retry S3 requests before giving up (in ms, set to -1 for infinity) (Deprecated. Use retry-attempts.)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_RETRY_DURATION_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecUInt::new(
                    "retry-attempts",
                    "Retry attempts",
                    "Number of times AWS SDK attempts a request before abandoning the request",
                    1,
                    10,
                    DEFAULT_RETRY_ATTEMPTS,
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "uri" => {
                drop(settings);
                let _ = self.set_uri(obj, value.get().expect("type checked upstream"));
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
            "request-timeout" => {
                settings.request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "retry-duration" => {
                /*
                 * To maintain backwards compatibility calculate retry attempts
                 * by dividing the provided duration from request timeout.
                 */
                let value = value.get::<i64>().expect("type checked upstream");
                let request_timeout = duration_to_millis(Some(settings.request_timeout));
                let retry_attempts = if value > request_timeout {
                    value / request_timeout
                } else {
                    request_timeout / value
                };
                settings.retry_attempts = retry_attempts as u32;
            }
            "retry-attempts" => {
                settings.retry_attempts = value.get::<u32>().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "uri" => {
                let url = match settings.url {
                    Some(ref url) => url.to_string(),
                    None => "".to_string(),
                };

                url.to_value()
            }
            "access-key" => settings.access_key.to_value(),
            "secret-access-key" => settings.secret_access_key.to_value(),
            "session-token" => settings.session_token.to_value(),
            "request-timeout" => duration_to_millis(Some(settings.request_timeout)).to_value(),
            "retry-duration" => {
                let request_timeout = duration_to_millis(Some(settings.request_timeout));
                (settings.retry_attempts as i64 * request_timeout).to_value()
            }
            "retry-attempts" => settings.retry_attempts.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_format(gst::Format::Bytes);
        /* Set a larger default blocksize to make read more efficient */
        obj.set_blocksize(256 * 1024);
    }
}

impl GstObjectImpl for S3Src {}

impl ElementImpl for S3Src {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Amazon S3 source",
                "Source/Network",
                "Reads an object from Amazon S3",
                "Arun Raghavan <arun@arunraghavan.net>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl URIHandlerImpl for S3Src {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["s3"]
    }

    fn uri(&self, _: &Self::Type) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.url.as_ref().map(|s| s.to_string())
    }

    fn set_uri(&self, element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        self.set_uri(element, Some(uri))
    }
}

impl BaseSrcImpl for S3Src {
    fn is_seekable(&self, _: &Self::Type) -> bool {
        true
    }

    fn size(&self, _: &Self::Type) -> Option<u64> {
        let state = self.state.lock().unwrap();
        match *state {
            StreamingState::Stopped => None,
            StreamingState::Started { size, .. } => Some(size),
        }
    }

    fn start(&self, src: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if let StreamingState::Started { .. } = *state {
            unreachable!("AwsS3Src is already started");
        }

        let settings = self.settings.lock().unwrap();
        let s3url = match settings.url {
            Some(ref url) => url.clone(),
            None => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["Cannot start without a URL being set"]
                ));
            }
        };
        drop(settings);

        if let Ok(s3client) = self.connect(&s3url) {
            let size = self.head(src, &s3client, &s3url)?;

            *state = StreamingState::Started {
                url: s3url,
                client: s3client,
                size,
            };

            Ok(())
        } else {
            Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Cannot connect to S3 resource"]
            ))
        }
    }

    fn stop(&self, _: &Self::Type) -> Result<(), gst::ErrorMessage> {
        // First, stop any asynchronous tasks if we're running, as they will have the state lock
        self.cancel();

        let mut state = self.state.lock().unwrap();

        if let StreamingState::Stopped = *state {
            unreachable!("Cannot stop before start");
        }

        *state = StreamingState::Stopped;

        Ok(())
    }

    fn query(&self, src: &Self::Type, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Scheduling(q) = query.view_mut() {
            q.set(
                gst::SchedulingFlags::SEQUENTIAL | gst::SchedulingFlags::BANDWIDTH_LIMITED,
                1,
                -1,
                0,
            );
            q.add_scheduling_modes(&[gst::PadMode::Push, gst::PadMode::Pull]);
            return true;
        }

        BaseSrcImplExt::parent_query(self, src, query)
    }

    fn create(
        &self,
        src: &Self::Type,
        offset: u64,
        buffer: Option<&mut gst::BufferRef>,
        length: u32,
    ) -> Result<CreateSuccess, gst::FlowError> {
        // FIXME: sanity check on offset and length
        let data = self.get(src, offset, u64::from(length));

        match data {
            /* Got data */
            Ok(bytes) => {
                if let Some(buffer) = buffer {
                    if let Err(copied_bytes) = buffer.copy_from_slice(0, bytes.as_ref()) {
                        buffer.set_size(copied_bytes);
                    }
                    Ok(CreateSuccess::FilledBuffer)
                } else {
                    Ok(CreateSuccess::NewBuffer(gst::Buffer::from_slice(bytes)))
                }
            }
            /* Interrupted */
            Err(None) => Err(gst::FlowError::Flushing),
            /* Actual Error */
            Err(Some(err)) => {
                gst::error!(CAT, obj: src, "Could not GET: {}", err);
                Err(gst::FlowError::Error)
            }
        }
    }

    /* FIXME: implement */
    fn do_seek(&self, _: &Self::Type, _: &mut gst::Segment) -> bool {
        true
    }

    fn unlock(&self, _: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.cancel();
        Ok(())
    }
}
