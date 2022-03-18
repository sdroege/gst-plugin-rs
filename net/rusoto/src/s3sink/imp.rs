// Copyright (C) 2019 Amazon.com, Inc. or its affiliates <mkolny@amazon.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::TryFutureExt;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_log, gst_trace, gst_warning};

use gst_base::subclass::prelude::*;

use futures::future;
use rusoto_core::{region::Region, request::HttpClient};
use rusoto_credential::StaticProvider;
use rusoto_s3::{
    AbortMultipartUploadRequest, CompleteMultipartUploadRequest, CompletedMultipartUpload,
    CompletedPart, CreateMultipartUploadRequest, S3Client, UploadPartRequest, S3,
};

use once_cell::sync::Lazy;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::s3url::*;
use crate::s3utils::{self, duration_from_millis, duration_to_millis, RetriableError, WaitError};

use super::OnError;

const DEFAULT_MULTIPART_UPLOAD_ON_ERROR: OnError = OnError::DoNothing;
// General setting for create / abort requests
const DEFAULT_REQUEST_TIMEOUT_MSEC: u64 = 10_000;
const DEFAULT_RETRY_DURATION_MSEC: u64 = 60_000;
// This needs to be independently configurable, as the part size can be upto 5GB
const DEFAULT_UPLOAD_PART_REQUEST_TIMEOUT_MSEC: u64 = 10_000;
const DEFAULT_UPLOAD_PART_RETRY_DURATION_MSEC: u64 = 60_000;
// CompletedMultipartUpload can take minutes to complete, so we need a longer value here
// https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html
const DEFAULT_COMPLETE_REQUEST_TIMEOUT_MSEC: u64 = 600_000; // 10 minutes
const DEFAULT_COMPLETE_RETRY_DURATION_MSEC: u64 = 3_600_000; // 60 minutes

struct Started {
    client: S3Client,
    buffer: Vec<u8>,
    upload_id: String,
    part_number: i64,
    completed_parts: Vec<CompletedPart>,
}

impl Started {
    pub fn new(client: S3Client, buffer: Vec<u8>, upload_id: String) -> Started {
        Started {
            client,
            buffer,
            upload_id,
            part_number: 0,
            completed_parts: Vec::new(),
        }
    }

    pub fn increment_part_number(&mut self) -> Result<i64, gst::ErrorMessage> {
        // https://docs.aws.amazon.com/AmazonS3/latest/dev/qfacts.html
        const MAX_MULTIPART_NUMBER: i64 = 10000;

        if self.part_number > MAX_MULTIPART_NUMBER {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                [
                    "Maximum number of parts ({}) reached.",
                    MAX_MULTIPART_NUMBER
                ]
            ));
        }

        self.part_number += 1;
        Ok(self.part_number)
    }
}

enum State {
    Stopped,
    Started(Started),
}

impl Default for State {
    fn default() -> State {
        State::Stopped
    }
}

const DEFAULT_BUFFER_SIZE: u64 = 5 * 1024 * 1024;

struct Settings {
    region: Region,
    bucket: Option<String>,
    key: Option<String>,
    content_type: Option<String>,
    buffer_size: u64,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    metadata: Option<gst::Structure>,
    multipart_upload_on_error: OnError,
    request_timeout: Option<Duration>,
    retry_duration: Option<Duration>,
    upload_part_request_timeout: Option<Duration>,
    upload_part_retry_duration: Option<Duration>,
    complete_upload_request_timeout: Option<Duration>,
    complete_upload_retry_duration: Option<Duration>,
}

impl Settings {
    fn to_uri(&self) -> String {
        format!(
            "s3://{}/{}/{}",
            match self.region {
                Region::Custom {
                    ref name,
                    ref endpoint,
                } => {
                    format!(
                        "{}+{}",
                        base32::encode(
                            base32::Alphabet::RFC4648 { padding: true },
                            name.as_bytes(),
                        ),
                        base32::encode(
                            base32::Alphabet::RFC4648 { padding: true },
                            endpoint.as_bytes(),
                        ),
                    )
                }
                _ => {
                    String::from(self.region.name())
                }
            },
            self.bucket.as_ref().unwrap(),
            self.key.as_ref().unwrap()
        )
    }

    fn to_metadata(&self, element: &super::S3Sink) -> Option<HashMap<String, String>> {
        self.metadata.as_ref().map(|structure| {
            let mut hash = HashMap::new();

            for (key, value) in structure.iter() {
                if let Ok(Ok(value_str)) = value.transform::<String>().map(|v| v.get()) {
                    gst_log!(CAT, obj: element, "metadata '{}' -> '{}'", key, value_str);
                    hash.insert(key.to_string(), value_str);
                } else {
                    gst_warning!(
                        CAT,
                        obj: element,
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
            region: Region::default(),
            bucket: None,
            key: None,
            content_type: None,
            buffer_size: DEFAULT_BUFFER_SIZE,
            access_key: None,
            secret_access_key: None,
            metadata: None,
            multipart_upload_on_error: DEFAULT_MULTIPART_UPLOAD_ON_ERROR,
            request_timeout: Some(Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MSEC)),
            retry_duration: Some(Duration::from_millis(DEFAULT_RETRY_DURATION_MSEC)),
            upload_part_request_timeout: Some(Duration::from_millis(
                DEFAULT_UPLOAD_PART_REQUEST_TIMEOUT_MSEC,
            )),
            upload_part_retry_duration: Some(Duration::from_millis(
                DEFAULT_UPLOAD_PART_RETRY_DURATION_MSEC,
            )),
            complete_upload_request_timeout: Some(Duration::from_millis(
                DEFAULT_COMPLETE_REQUEST_TIMEOUT_MSEC,
            )),
            complete_upload_retry_duration: Some(Duration::from_millis(
                DEFAULT_COMPLETE_RETRY_DURATION_MSEC,
            )),
        }
    }
}

#[derive(Default)]
pub struct S3Sink {
    url: Mutex<Option<GstS3Url>>,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<Option<future::AbortHandle>>,
    abort_multipart_canceller: Mutex<Option<future::AbortHandle>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rusotos3sink",
        gst::DebugColorFlags::empty(),
        Some("Amazon S3 Sink"),
    )
});

impl S3Sink {
    fn flush_current_buffer(
        &self,
        element: &super::S3Sink,
    ) -> Result<(), Option<gst::ErrorMessage>> {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();
        let state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        let part_number = state.increment_part_number()?;
        let body = std::mem::replace(
            &mut state.buffer,
            Vec::with_capacity(settings.buffer_size as usize),
        );
        let upload_id = &state.upload_id;
        let client = &state.client;

        let upload_part_req_future = || {
            client
                .upload_part(self.create_upload_part_request(&body, part_number, upload_id))
                .map_err(RetriableError::Rusoto)
        };

        let output = s3utils::wait_retry(
            &self.canceller,
            settings.upload_part_request_timeout,
            settings.upload_part_retry_duration,
            upload_part_req_future,
        )
        .map_err(|err| match err {
            WaitError::FutureError(err) => {
                match settings.multipart_upload_on_error {
                    OnError::Abort => {
                        gst_log!(
                            CAT,
                            obj: element,
                            "Aborting multipart upload request with id: {}",
                            state.upload_id
                        );
                        match self.abort_multipart_upload_request(state) {
                            Ok(()) => {
                                gst_log!(
                                    CAT,
                                    obj: element,
                                    "Aborting multipart upload request succeeded."
                                );
                            }
                            Err(err) => gst_error!(
                                CAT,
                                obj: element,
                                "Aborting multipart upload failed: {}",
                                err.to_string()
                            ),
                        }
                    }
                    OnError::Complete => {
                        gst_log!(
                            CAT,
                            obj: element,
                            "Completing multipart upload request with id: {}",
                            state.upload_id
                        );
                        match self.complete_multipart_upload_request(state) {
                            Ok(()) => {
                                gst_log!(
                                    CAT,
                                    obj: element,
                                    "Complete multipart upload request succeeded."
                                );
                            }
                            Err(err) => gst_error!(
                                CAT,
                                obj: element,
                                "Completing multipart upload failed: {}",
                                err.to_string()
                            ),
                        }
                    }
                    OnError::DoNothing => (),
                }
                Some(gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to upload part: {:?}", err]
                ))
            }
            WaitError::Cancelled => None,
        })?;

        state.completed_parts.push(CompletedPart {
            e_tag: output.e_tag,
            part_number: Some(part_number),
        });
        gst_info!(CAT, obj: element, "Uploaded part {}", part_number);

        Ok(())
    }

    fn create_upload_part_request(
        &self,
        body: &[u8],
        part_number: i64,
        upload_id: &str,
    ) -> UploadPartRequest {
        let url = self.url.lock().unwrap();

        UploadPartRequest {
            body: Some(rusoto_core::ByteStream::from(body.to_owned())),
            bucket: url.as_ref().unwrap().bucket.to_owned(),
            key: url.as_ref().unwrap().object.to_owned(),
            upload_id: upload_id.to_owned(),
            part_number,
            ..Default::default()
        }
    }

    fn create_complete_multipart_upload_request(
        &self,
        started_state: &Started,
        completed_upload: CompletedMultipartUpload,
    ) -> CompleteMultipartUploadRequest {
        let url = self.url.lock().unwrap();
        CompleteMultipartUploadRequest {
            bucket: url.as_ref().unwrap().bucket.to_owned(),
            key: url.as_ref().unwrap().object.to_owned(),
            upload_id: started_state.upload_id.to_owned(),
            multipart_upload: Some(completed_upload),
            ..Default::default()
        }
    }

    fn create_create_multipart_upload_request(
        &self,
        url: &GstS3Url,
        settings: &Settings,
    ) -> CreateMultipartUploadRequest {
        CreateMultipartUploadRequest {
            bucket: url.bucket.clone(),
            key: url.object.clone(),
            content_type: settings.content_type.clone(),
            metadata: settings.to_metadata(&self.instance()),
            ..Default::default()
        }
    }

    fn create_abort_multipart_upload_request(
        &self,
        url: &GstS3Url,
        started_state: &Started,
    ) -> AbortMultipartUploadRequest {
        AbortMultipartUploadRequest {
            bucket: url.bucket.clone(),
            expected_bucket_owner: None,
            key: url.object.clone(),
            request_payer: None,
            upload_id: started_state.upload_id.to_owned(),
        }
    }

    fn abort_multipart_upload_request(
        &self,
        started_state: &Started,
    ) -> Result<(), gst::ErrorMessage> {
        let s3url = match *self.url.lock().unwrap() {
            Some(ref url) => url.clone(),
            None => unreachable!("Element should be started"),
        };
        let abort_req_future = || {
            let abort_req = self.create_abort_multipart_upload_request(&s3url, started_state);
            started_state
                .client
                .abort_multipart_upload(abort_req)
                .map_err(RetriableError::Rusoto)
        };

        s3utils::wait_retry(
            &self.abort_multipart_canceller,
            Some(Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MSEC)),
            Some(Duration::from_millis(DEFAULT_RETRY_DURATION_MSEC)),
            abort_req_future,
        )
        .map(|_| ())
        .map_err(|err| match err {
            WaitError::FutureError(err) => {
                gst::error_msg!(
                    gst::ResourceError::Write,
                    ["Failed to abort multipart upload: {:?}.", err]
                )
            }
            WaitError::Cancelled => {
                gst::error_msg!(
                    gst::ResourceError::Write,
                    ["Abort multipart upload request interrupted."]
                )
            }
        })
    }

    fn complete_multipart_upload_request(
        &self,
        started_state: &mut Started,
    ) -> Result<(), gst::ErrorMessage> {
        started_state
            .completed_parts
            .sort_by(|a, b| a.part_number.cmp(&b.part_number));

        let completed_upload = CompletedMultipartUpload {
            parts: Some(std::mem::take(&mut started_state.completed_parts)),
        };

        let complete_req_future = || {
            let complete_req = self
                .create_complete_multipart_upload_request(started_state, completed_upload.clone());
            started_state
                .client
                .complete_multipart_upload(complete_req)
                .map_err(RetriableError::Rusoto)
        };

        s3utils::wait_retry(
            &self.canceller,
            Some(Duration::from_millis(DEFAULT_COMPLETE_REQUEST_TIMEOUT_MSEC)),
            Some(Duration::from_millis(DEFAULT_COMPLETE_RETRY_DURATION_MSEC)),
            complete_req_future,
        )
        .map(|_| ())
        .map_err(|err| match err {
            WaitError::FutureError(err) => gst::error_msg!(
                gst::ResourceError::Write,
                ["Failed to complete multipart upload: {:?}.", err]
            ),
            WaitError::Cancelled => {
                gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during stop"])
            }
        })
    }

    fn finalize_upload(&self, element: &super::S3Sink) -> Result<(), gst::ErrorMessage> {
        if self.flush_current_buffer(element).is_err() {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                ["Failed to flush internal buffer."]
            ));
        }

        let mut state = self.state.lock().unwrap();
        let started_state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        self.complete_multipart_upload_request(started_state)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        if let State::Started { .. } = *state {
            unreachable!("Element should be started");
        }

        let s3url = match *self.url.lock().unwrap() {
            Some(ref url) => url.clone(),
            None => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["Cannot start without a URL being set"]
                ));
            }
        };

        let client = match (
            settings.access_key.as_ref(),
            settings.secret_access_key.as_ref(),
        ) {
            (Some(access_key), Some(secret_access_key)) => {
                let creds =
                    StaticProvider::new_minimal(access_key.clone(), secret_access_key.clone());
                S3Client::new_with(
                    HttpClient::new().expect("failed to create request dispatcher"),
                    creds,
                    s3url.region.clone(),
                )
            }
            _ => S3Client::new(s3url.region.clone()),
        };

        let create_multipart_req_future = || {
            let create_multipart_req =
                self.create_create_multipart_upload_request(&s3url, &settings);
            client
                .create_multipart_upload(create_multipart_req)
                .map_err(RetriableError::Rusoto)
        };

        let response = s3utils::wait_retry(
            &self.canceller,
            Some(Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MSEC)),
            Some(Duration::from_millis(DEFAULT_RETRY_DURATION_MSEC)),
            create_multipart_req_future,
        )
        .map_err(|err| match err {
            WaitError::FutureError(err) => gst::error_msg!(
                gst::ResourceError::OpenWrite,
                ["Failed to create multipart upload: {:?}", err]
            ),
            WaitError::Cancelled => {
                gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
            }
        })?;

        let upload_id = response.upload_id.ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::Failed,
                ["Failed to get multipart upload ID"]
            )
        })?;

        *state = State::Started(Started::new(
            client,
            Vec::with_capacity(settings.buffer_size as usize),
            upload_id,
        ));

        Ok(())
    }

    fn update_buffer(
        &self,
        src: &[u8],
        element: &super::S3Sink,
    ) -> Result<(), Option<gst::ErrorMessage>> {
        let mut state = self.state.lock().unwrap();
        let started_state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started already");
            }
        };

        let to_copy = std::cmp::min(
            started_state.buffer.capacity() - started_state.buffer.len(),
            src.len(),
        );

        let (head, tail) = src.split_at(to_copy);
        started_state.buffer.extend_from_slice(head);
        let do_flush = started_state.buffer.capacity() == started_state.buffer.len();
        drop(state);

        if do_flush {
            self.flush_current_buffer(element)?;
        }

        if to_copy < src.len() {
            self.update_buffer(tail, element)?;
        }

        Ok(())
    }

    fn cancel(&self) {
        let mut canceller = self.canceller.lock().unwrap();
        let mut abort_canceller = self.abort_multipart_canceller.lock().unwrap();

        if let Some(c) = abort_canceller.take() {
            c.abort()
        };

        if let Some(c) = canceller.take() {
            c.abort()
        };
    }

    fn set_uri(
        self: &S3Sink,
        object: &super::S3Sink,
        url_str: Option<&str>,
    ) -> Result<(), glib::Error> {
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

        gst_debug!(CAT, obj: object, "Setting uri to {:?}", url_str);

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
impl ObjectSubclass for S3Sink {
    const NAME: &'static str = "RusotoS3Sink";
    type Type = super::S3Sink;
    type ParentType = gst_base::BaseSink;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for S3Sink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "bucket",
                    "S3 Bucket",
                    "The bucket of the file to write",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "key",
                    "S3 Key",
                    "The key of the file to write",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "region",
                    "AWS Region",
                    "An AWS region (e.g. eu-west-2).",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt64::new(
                    "part-size",
                    "Part size",
                    "A size (in bytes) of an individual part used for multipart upload.",
                    5 * 1024 * 1024,        // 5 MB
                    5 * 1024 * 1024 * 1024, // 5 GB
                    DEFAULT_BUFFER_SIZE,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
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
                glib::ParamSpecBoxed::new(
                    "metadata",
                    "Metadata",
                    "A map of metadata to store with the object in S3; field values need to be convertible to strings.",
                    gst::Structure::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecEnum::new(
                    "on-error",
                    "Whether to upload or complete the multipart upload on error",
                    "Do nothing, abort or complete a multipart upload request on error",
                    OnError::static_type(),
                    DEFAULT_MULTIPART_UPLOAD_ON_ERROR as i32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecInt64::new(
                    "request-timeout",
                    "Request timeout",
                    "Timeout for general S3 requests (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_REQUEST_TIMEOUT_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "retry-duration",
                    "Retry duration",
                    "How long we should retry general S3 requests before giving up (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_RETRY_DURATION_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "upload-part-request-timeout",
                    "Upload part request timeout",
                    "Timeout for a single upload part request (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_UPLOAD_PART_REQUEST_TIMEOUT_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "upload-part-retry-duration",
                    "Upload part retry duration",
                    "How long we should retry upload part requests before giving up (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_UPLOAD_PART_RETRY_DURATION_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "complete-upload-request-timeout",
                    "Complete upload request timeout",
                    "Timeout for the complete multipart upload request (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_COMPLETE_REQUEST_TIMEOUT_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "complete-upload-retry-duration",
                    "Complete upload retry duration",
                    "How long we should retry complete multipart upload requests before giving up (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_COMPLETE_RETRY_DURATION_MSEC as i64,
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

        gst_debug!(
            CAT,
            obj: obj,
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
                    let _ = self.set_uri(obj, Some(&settings.to_uri()));
                }
            }
            "key" => {
                settings.key = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if settings.bucket.is_some() {
                    let _ = self.set_uri(obj, Some(&settings.to_uri()));
                }
            }
            "region" => {
                let region = value.get::<String>().expect("type checked upstream");
                settings.region = region
                    .parse::<Region>()
                    .or_else(|_| {
                        let (name, endpoint) = region.split_once('+').ok_or(())?;
                        Ok(Region::Custom {
                            name: name.into(),
                            endpoint: endpoint.into(),
                        })
                    })
                    .unwrap_or_else(|_: ()| panic!("Invalid region '{}'", region));

                if settings.key.is_some() && settings.bucket.is_some() {
                    let _ = self.set_uri(obj, Some(&settings.to_uri()));
                }
            }
            "part-size" => {
                settings.buffer_size = value.get::<u64>().expect("type checked upstream");
            }
            "uri" => {
                let _ = self.set_uri(obj, value.get().expect("type checked upstream"));
            }
            "access-key" => {
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "metadata" => {
                settings.metadata = value.get().expect("type checked upstream");
            }
            "on-error" => {
                settings.multipart_upload_on_error =
                    value.get::<OnError>().expect("type checked upstream");
            }
            "request-timeout" => {
                settings.request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "retry-duration" => {
                settings.retry_duration =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "upload-part-request-timeout" => {
                settings.upload_part_request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "upload-part-retry-duration" => {
                settings.upload_part_retry_duration =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "complete-upload-request-timeout" => {
                settings.complete_upload_request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "complete-upload-retry-duration" => {
                settings.complete_upload_retry_duration =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "key" => settings.key.to_value(),
            "bucket" => settings.bucket.to_value(),
            "region" => settings.region.name().to_value(),
            "part-size" => settings.buffer_size.to_value(),
            "uri" => {
                let url = match *self.url.lock().unwrap() {
                    Some(ref url) => url.to_string(),
                    None => "".to_string(),
                };

                url.to_value()
            }
            "access-key" => settings.access_key.to_value(),
            "secret-access-key" => settings.secret_access_key.to_value(),
            "metadata" => settings.metadata.to_value(),
            "on-error" => settings.multipart_upload_on_error.to_value(),
            "request-timeout" => duration_to_millis(settings.request_timeout).to_value(),
            "retry-duration" => duration_to_millis(settings.retry_duration).to_value(),
            "upload-part-request-timeout" => {
                duration_to_millis(settings.upload_part_request_timeout).to_value()
            }
            "upload-part-retry-duration" => {
                duration_to_millis(settings.upload_part_retry_duration).to_value()
            }
            "complete-upload-request-timeout" => {
                duration_to_millis(settings.complete_upload_request_timeout).to_value()
            }
            "complete-upload-retry-duration" => {
                duration_to_millis(settings.complete_upload_retry_duration).to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for S3Sink {}

impl ElementImpl for S3Sink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Amazon S3 sink",
                "Source/Network",
                "Writes an object to Amazon S3",
                "Marcin Kolny <mkolny@amazon.com>",
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

impl URIHandlerImpl for S3Sink {
    const URI_TYPE: gst::URIType = gst::URIType::Sink;

    fn protocols() -> &'static [&'static str] {
        &["s3"]
    }

    fn uri(&self, _: &Self::Type) -> Option<String> {
        self.url.lock().unwrap().as_ref().map(|s| s.to_string())
    }

    fn set_uri(&self, element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        self.set_uri(element, Some(uri))
    }
}

impl BaseSinkImpl for S3Sink {
    fn start(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.start()
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        *state = State::Stopped;
        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn render(
        &self,
        element: &Self::Type,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let State::Stopped = *self.state.lock().unwrap() {
            gst::element_error!(element, gst::CoreError::Failed, ["Not started yet"]);
            return Err(gst::FlowError::Error);
        }

        gst_trace!(CAT, obj: element, "Rendering {:?}", buffer);
        let map = buffer.map_readable().map_err(|_| {
            gst::element_error!(element, gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        match self.update_buffer(&map, element) {
            Ok(_) => Ok(gst::FlowSuccess::Ok),
            Err(err) => match err {
                Some(error_message) => {
                    gst_error!(
                        CAT,
                        obj: element,
                        "Multipart upload failed: {}",
                        error_message
                    );
                    element.post_error_message(error_message);
                    Err(gst::FlowError::Error)
                }
                _ => {
                    gst_info!(CAT, obj: element, "Upload interrupted. Flushing...");
                    Err(gst::FlowError::Flushing)
                }
            },
        }
    }

    fn unlock(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.cancel();

        Ok(())
    }

    fn event(&self, element: &Self::Type, event: gst::Event) -> bool {
        if let gst::EventView::Eos(_) = event.view() {
            if let Err(error_message) = self.finalize_upload(element) {
                gst_error!(
                    CAT,
                    obj: element,
                    "Failed to finalize the upload: {}",
                    error_message
                );
                return false;
            }
        }

        BaseSinkImplExt::parent_event(self, element, event)
    }
}
