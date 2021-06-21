// Copyright (C) 2019 Amazon.com, Inc. or its affiliates <mkolny@amazon.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_trace};

use gst_base::subclass::prelude::*;

use futures::future;
use rusoto_core::region::Region;
use rusoto_s3::{
    CompleteMultipartUploadRequest, CompletedMultipartUpload, CompletedPart,
    CreateMultipartUploadRequest, S3Client, UploadPartRequest, S3,
};

use once_cell::sync::Lazy;

use std::convert::From;
use std::str::FromStr;
use std::sync::Mutex;

use crate::s3url::*;
use crate::s3utils::{self, WaitError};

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
}

impl Settings {
    fn to_uri(&self) -> String {
        format!(
            "s3://{}/{}/{}",
            self.region.name(),
            self.bucket.as_ref().unwrap(),
            self.key.as_ref().unwrap()
        )
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
        }
    }
}

#[derive(Default)]
pub struct S3Sink {
    url: Mutex<Option<GstS3Url>>,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<Option<future::AbortHandle>>,
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
        let upload_part_req = self.create_upload_part_request()?;
        let part_number = upload_part_req.part_number;

        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        let upload_part_req_future = state.client.upload_part(upload_part_req);

        let output =
            s3utils::wait(&self.canceller, upload_part_req_future).map_err(|err| match err {
                WaitError::FutureError(err) => Some(gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to upload part: {}", err]
                )),
                WaitError::Cancelled => None,
            })?;

        state.completed_parts.push(CompletedPart {
            e_tag: output.e_tag,
            part_number: Some(part_number),
        });
        gst_info!(CAT, obj: element, "Uploaded part {}", part_number);

        Ok(())
    }

    fn create_upload_part_request(&self) -> Result<UploadPartRequest, gst::ErrorMessage> {
        let url = self.url.lock().unwrap();
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        let part_number = state.increment_part_number()?;
        Ok(UploadPartRequest {
            body: Some(rusoto_core::ByteStream::from(std::mem::replace(
                &mut state.buffer,
                Vec::with_capacity(settings.buffer_size as usize),
            ))),
            bucket: url.as_ref().unwrap().bucket.to_owned(),
            key: url.as_ref().unwrap().object.to_owned(),
            upload_id: state.upload_id.to_owned(),
            part_number,
            ..Default::default()
        })
    }

    fn create_complete_multipart_upload_request(
        &self,
        started_state: &mut Started,
    ) -> CompleteMultipartUploadRequest {
        started_state
            .completed_parts
            .sort_by(|a, b| a.part_number.cmp(&b.part_number));

        let completed_upload = CompletedMultipartUpload {
            parts: Some(std::mem::take(&mut started_state.completed_parts)),
        };

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
            ..Default::default()
        }
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

        let complete_req = self.create_complete_multipart_upload_request(started_state);
        let complete_req_future = started_state.client.complete_multipart_upload(complete_req);

        s3utils::wait(&self.canceller, complete_req_future)
            .map(|_| ())
            .map_err(|err| match err {
                WaitError::FutureError(err) => gst::error_msg!(
                    gst::ResourceError::Write,
                    ["Failed to complete multipart upload: {}.", err.to_string()]
                ),
                WaitError::Cancelled => {
                    gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during stop"])
                }
            })
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

        let client = S3Client::new(s3url.region.clone());

        let create_multipart_req = self.create_create_multipart_upload_request(&s3url, &settings);
        let create_multipart_req_future = client.create_multipart_upload(create_multipart_req);

        let response = s3utils::wait(&self.canceller, create_multipart_req_future).map_err(
            |err| match err {
                WaitError::FutureError(err) => gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to create multipart upload: {}", err]
                ),
                WaitError::Cancelled => {
                    gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
                }
            },
        )?;

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
                glib::ParamSpec::new_string(
                    "bucket",
                    "S3 Bucket",
                    "The bucket of the file to write",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string(
                    "key",
                    "S3 Key",
                    "The key of the file to write",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string(
                    "region",
                    "AWS Region",
                    "An AWS region (e.g. eu-west-2).",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_uint64(
                    "part-size",
                    "Part size",
                    "A size (in bytes) of an individual part used for multipart upload.",
                    5 * 1024 * 1024,        // 5 MB
                    5 * 1024 * 1024 * 1024, // 5 GB
                    DEFAULT_BUFFER_SIZE,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string(
                    "uri",
                    "URI",
                    "The S3 object URI",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
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
                settings.region =
                    Region::from_str(&value.get::<String>().expect("type checked upstream"))
                        .unwrap();
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
            _ => unimplemented!(),
        }
    }
}

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
