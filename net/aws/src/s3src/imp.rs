// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use bytes::Bytes;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use aws_sdk_s3::{
    config::{self, retry::RetryConfig, Credentials},
    Client,
};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use crate::s3url::*;
use crate::s3utils::{self, duration_from_millis, duration_to_millis, WaitError};

const DEFAULT_FORCE_PATH_STYLE: bool = false;
const DEFAULT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_REQUEST_TIMEOUT_MSEC: u64 = 15000;
const DEFAULT_RETRY_DURATION_MSEC: u64 = 60_000;

#[derive(Default)]
#[allow(clippy::large_enum_variant)]
enum StreamingState {
    #[default]
    Stopped,
    Started {
        url: GstS3Url,
        client: Client,
        size: Option<u64>,
    },
}

struct Settings {
    url: Option<GstS3Url>,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    retry_attempts: u32,
    request_timeout: Duration,
    endpoint_uri: Option<String>,
    force_path_style: bool,
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
            endpoint_uri: None,
            force_path_style: DEFAULT_FORCE_PATH_STYLE,
        }
    }
}

#[derive(Default)]
pub struct S3Src {
    settings: Mutex<Settings>,
    state: Mutex<StreamingState>,
    canceller: Mutex<s3utils::Canceller>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "awss3src",
        gst::DebugColorFlags::empty(),
        Some("Amazon S3 Source"),
    )
});

impl S3Src {
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

        let config_builder = config::Builder::from(&sdk_config)
            .force_path_style(settings.force_path_style)
            .retry_config(RetryConfig::standard().with_max_attempts(settings.retry_attempts));

        let config = if let Some(ref uri) = settings.endpoint_uri {
            config_builder.endpoint_url(uri).build()
        } else {
            config_builder.build()
        };

        Ok(Client::from_conf(config))
    }

    fn set_uri(self: &S3Src, url_str: Option<&str>) -> Result<(), glib::Error> {
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
        client: &Client,
        url: &GstS3Url,
    ) -> Result<Option<u64>, gst::ErrorMessage> {
        let head_object = client
            .head_object()
            .set_bucket(Some(url.bucket.clone()))
            .set_key(Some(url.object.clone()))
            .set_version_id(url.version.clone());
        let head_object_future = head_object.send();

        let output =
            s3utils::wait(&self.canceller, head_object_future).map_err(|err| match &err {
                WaitError::FutureError(_) => gst::error_msg!(
                    gst::ResourceError::NotFound,
                    ["Failed to get HEAD object: {err}"]
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
            imp = self,
            "HEAD success, content length = {:?}",
            output.content_length
        );

        Ok(output.content_length.map(|size| size as u64))
    }

    /* Returns the bytes, Some(error) if one occurred, or a None error if interrupted */
    fn get(self: &S3Src, offset: u64, length: u64) -> Result<Bytes, Option<gst::ErrorMessage>> {
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
            imp = self,
            "Requesting range: {}-{}",
            offset,
            offset + length - 1
        );

        let get_object_future = get_object.send();

        let mut output =
            s3utils::wait(&self.canceller, get_object_future).map_err(|err| match &err {
                WaitError::FutureError(_) => Some(gst::error_msg!(
                    gst::ResourceError::Read,
                    ["Could not read: {err}"]
                )),
                WaitError::Cancelled => None,
            })?;

        gst::debug!(CAT, imp = self, "Read {:?} bytes", output.content_length);

        s3utils::wait_stream(&self.canceller, &mut output.body).map_err(|err| match err {
            WaitError::FutureError(err) => Some(gst::error_msg!(
                gst::ResourceError::Read,
                ["Could not read: {err}"]
            )),
            WaitError::Cancelled => None,
        })
    }
}

#[glib::object_subclass]
impl ObjectSubclass for S3Src {
    const NAME: &'static str = "GstAwsS3Src";
    type Type = super::S3Src;
    type ParentType = gst_base::BaseSrc;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for S3Src {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
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
                glib::ParamSpecInt64::builder("request-timeout")
                    .nick("Request timeout")
                    .blurb("Timeout for each S3 request (in ms, set to -1 for infinity)")
                    .minimum(-1)
                    .default_value(DEFAULT_REQUEST_TIMEOUT_MSEC as i64)
                    .build(),
                glib::ParamSpecInt64::builder("retry-duration")
                    .nick("Retry duration")
                    .blurb("How long we should retry S3 requests before giving up (in ms, set to -1 for infinity) (Deprecated. Use retry-attempts.)")
                    .minimum(-1)
                    .default_value(DEFAULT_RETRY_DURATION_MSEC as i64)
                    .build(),
                glib::ParamSpecUInt::builder("retry-attempts")
                    .nick("Retry attempts")
                    .blurb("Number of times AWS SDK attempts a request before abandoning the request")
                    .minimum(1)
                    .maximum(10)
                    .default_value(DEFAULT_RETRY_ATTEMPTS)
                    .build(),
                glib::ParamSpecString::builder("endpoint-uri")
                    .nick("S3 endpoint URI")
                    .blurb("The S3 endpoint URI to use")
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

        match pspec.name() {
            "uri" => {
                drop(settings);
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
                    1
                };
                settings.retry_attempts = retry_attempts as u32;
            }
            "retry-attempts" => {
                settings.retry_attempts = value.get::<u32>().expect("type checked upstream");
            }
            "endpoint-uri" => {
                settings.endpoint_uri = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
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
            "endpoint-uri" => settings.endpoint_uri.to_value(),
            "force-path-style" => settings.force_path_style.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_format(gst::Format::Bytes);
        /* Set a larger default blocksize to make read more efficient */
        obj.set_blocksize(256 * 1024);
    }
}

impl GstObjectImpl for S3Src {}

impl ElementImpl for S3Src {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
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
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
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

    fn uri(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.url.as_ref().map(|s| s.to_string())
    }

    fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
        self.set_uri(Some(uri))
    }
}

impl BaseSrcImpl for S3Src {
    fn is_seekable(&self) -> bool {
        true
    }

    fn size(&self) -> Option<u64> {
        let state = self.state.lock().unwrap();
        match *state {
            StreamingState::Stopped => None,
            StreamingState::Started { size, .. } => size,
        }
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
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
            let size = self.head(&s3client, &s3url)?;

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

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if let StreamingState::Stopped = *state {
            unreachable!("Cannot stop before start");
        }

        *state = StreamingState::Stopped;

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
            q.add_scheduling_modes([gst::PadMode::Push, gst::PadMode::Pull]);
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
        // FIXME: sanity check on offset and length
        let data = self.get(offset, u64::from(length));

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
                gst::error!(CAT, imp = self, "Could not GET: {}", err);
                Err(gst::FlowError::Error)
            }
        }
    }

    /* FIXME: implement */
    fn do_seek(&self, _: &mut gst::Segment) -> bool {
        true
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
}
