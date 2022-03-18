// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::Mutex;
use std::time::Duration;

use bytes::{buf::BufMut, Bytes, BytesMut};
use futures::future;
use futures::{TryFutureExt, TryStreamExt};
use once_cell::sync::Lazy;
use rusoto_core::request::HttpClient;
use rusoto_credential::StaticProvider;
use rusoto_s3::GetObjectError;
use rusoto_s3::{GetObjectRequest, HeadObjectRequest, S3Client, S3};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use crate::s3url::*;
use crate::s3utils::{self, duration_from_millis, duration_to_millis, RetriableError, WaitError};

const DEFAULT_REQUEST_TIMEOUT_MSEC: u64 = 10_000;
const DEFAULT_RETRY_DURATION_MSEC: u64 = 60_000;

#[allow(clippy::large_enum_variant)]
enum StreamingState {
    Stopped,
    Started {
        url: GstS3Url,
        client: S3Client,
        size: u64,
    },
}

impl Default for StreamingState {
    fn default() -> StreamingState {
        StreamingState::Stopped
    }
}

#[derive(Default)]
struct Settings {
    url: Option<GstS3Url>,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    request_timeout: Option<Duration>,
    retry_duration: Option<Duration>,
}

#[derive(Default)]
pub struct S3Src {
    settings: Mutex<Settings>,
    state: Mutex<StreamingState>,
    canceller: Mutex<Option<future::AbortHandle>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rusotos3src",
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

    fn connect(self: &S3Src, url: &GstS3Url) -> S3Client {
        let settings = self.settings.lock().unwrap();

        match (
            settings.access_key.as_ref(),
            settings.secret_access_key.as_ref(),
        ) {
            (Some(access_key), Some(secret_access_key)) => {
                let creds =
                    StaticProvider::new_minimal(access_key.clone(), secret_access_key.clone());
                S3Client::new_with(
                    HttpClient::new().expect("failed to create request dispatcher"),
                    creds,
                    url.region.clone(),
                )
            }
            _ => S3Client::new(url.region.clone()),
        }
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
        client: &S3Client,
        url: &GstS3Url,
    ) -> Result<u64, gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();

        let head_object_future = || {
            client
                .head_object(HeadObjectRequest {
                    bucket: url.bucket.clone(),
                    key: url.object.clone(),
                    version_id: url.version.clone(),
                    ..Default::default()
                })
                .map_err(RetriableError::Rusoto)
        };

        let output = s3utils::wait_retry(
            &self.canceller,
            settings.request_timeout,
            settings.retry_duration,
            head_object_future,
        )
        .map_err(|err| match err {
            WaitError::FutureError(err) => gst::error_msg!(
                gst::ResourceError::NotFound,
                ["Failed to HEAD object: {:?}", err]
            ),
            WaitError::Cancelled => {
                gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
            }
        })?;

        if let Some(size) = output.content_length {
            gst::info!(CAT, obj: src, "HEAD success, content length = {}", size);
            Ok(size as u64)
        } else {
            Err(gst::error_msg!(
                gst::ResourceError::Read,
                ["Failed to get content length"]
            ))
        }
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

        let settings = self.settings.lock().unwrap();

        let get_object_future = || async {
            gst::debug!(
                CAT,
                obj: src,
                "Requesting range: {}-{}",
                offset,
                offset + length - 1
            );

            let output = client
                .get_object(GetObjectRequest {
                    bucket: url.bucket.clone(),
                    key: url.object.clone(),
                    range: Some(format!("bytes={}-{}", offset, offset + length - 1)),
                    version_id: url.version.clone(),
                    ..Default::default()
                })
                .map_err(RetriableError::Rusoto)
                .await?;

            gst::debug!(
                CAT,
                obj: src,
                "Read {} bytes",
                output.content_length.unwrap()
            );

            let mut collect = BytesMut::new();
            let mut stream = output.body.unwrap();

            // Loop over the stream and collect till we're done
            // FIXME: Can we use TryStreamExt::collect() here?
            while let Some(item) = stream.try_next().map_err(RetriableError::Std).await? {
                collect.put(item)
            }

            Ok::<Bytes, RetriableError<GetObjectError>>(collect.freeze())
        };

        s3utils::wait_retry(
            &self.canceller,
            settings.request_timeout,
            settings.retry_duration,
            get_object_future,
        )
        .map_err(|err| match err {
            WaitError::FutureError(err) => Some(gst::error_msg!(
                gst::ResourceError::Read,
                ["Could not read: {:?}", err]
            )),
            WaitError::Cancelled => None,
        })
    }
}

#[glib::object_subclass]
impl ObjectSubclass for S3Src {
    const NAME: &'static str = "RusotoS3Src";
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
                    "How long we should retry S3 requests before giving up (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_RETRY_DURATION_MSEC as i64,
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
        match pspec.name() {
            "uri" => {
                let _ = self.set_uri(obj, value.get().expect("type checked upstream"));
            }
            "access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "request-timeout" => {
                let mut settings = self.settings.lock().unwrap();
                settings.request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "retry-duration" => {
                let mut settings = self.settings.lock().unwrap();
                settings.retry_duration =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
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
            "request-timeout" => duration_to_millis(settings.request_timeout).to_value(),
            "retry-duration" => duration_to_millis(settings.retry_duration).to_value(),
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
        match *self.state.lock().unwrap() {
            StreamingState::Stopped => None,
            StreamingState::Started { size, .. } => Some(size),
        }
    }

    fn start(&self, src: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if let StreamingState::Started { .. } = *state {
            unreachable!("RusotoS3Src is already started");
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

        let s3client = self.connect(&s3url);
        let size = self.head(src, &s3client, &s3url)?;

        *state = StreamingState::Started {
            url: s3url,
            client: s3client,
            size,
        };

        Ok(())
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
