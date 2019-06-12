// Copyright (C) 2019 Amazon.com, Inc. or its affiliates <mkolny@amazon.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_base;
use gst_base::subclass::prelude::*;

use futures::prelude::*;
use futures::sync::oneshot;

use rusoto_core::region::Region;

use tokio::runtime;

use rusoto_s3::{
    CompleteMultipartUploadRequest, CompletedMultipartUpload, CompletedPart,
    CreateMultipartUploadRequest, S3Client, UploadPartRequest, S3,
};

use std::convert::From;
use std::str::FromStr;
use std::sync::Mutex;

use crate::s3utils;

struct Started {
    buffer: Vec<u8>,
    upload_id: String,
    part_number: i64,
    completed_parts: Vec<CompletedPart>,
}

impl Started {
    pub fn new(buffer: Vec<u8>, upload_id: String) -> Started {
        Started {
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
            return Err(gst_error_msg!(
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

pub struct S3Sink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    cat: gst::DebugCategory,
    runtime: runtime::Runtime,
    canceller: Mutex<Option<oneshot::Sender<()>>>,
    client: Mutex<S3Client>,
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

static PROPERTIES: [subclass::Property; 4] = [
    subclass::Property("bucket", |name| {
        glib::ParamSpec::string(
            name,
            "S3 Bucket",
            "The bucket of the file to write",
            None,
            glib::ParamFlags::READWRITE, /* + GST_PARAM_MUTABLE_READY) */
        )
    }),
    subclass::Property("key", |name| {
        glib::ParamSpec::string(
            name,
            "S3 Key",
            "The key of the file to write",
            None,
            glib::ParamFlags::READWRITE, /* + GST_PARAM_MUTABLE_READY) */
        )
    }),
    subclass::Property("region", |name| {
        glib::ParamSpec::string(
            name,
            "AWS Region",
            "An AWS region (e.g. eu-west-2).",
            None,
            glib::ParamFlags::READWRITE, /* + GST_PARAM_MUTABLE_READY) */
        )
    }),
    subclass::Property("part-size", |name| {
        glib::ParamSpec::uint64(
            name,
            "Part size",
            "A size (in bytes) of an individual part used for multipart upload.",
            5 * 1024 * 1024,        // 5 MB
            5 * 1024 * 1024 * 1024, // 5 GB
            DEFAULT_BUFFER_SIZE,
            glib::ParamFlags::READWRITE, /* + GST_PARAM_MUTABLE_READY) */
        )
    }),
];

impl S3Sink {
    fn flush_current_buffer(
        &self,
        element: &gst_base::BaseSink,
    ) -> Result<(), Option<gst::ErrorMessage>> {
        let upload_part_req = self.create_upload_part_request()?;
        let part_number = upload_part_req.part_number;

        let upload_part_req_future = self
            .client
            .lock()
            .unwrap()
            .upload_part(upload_part_req)
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to upload part: {}", err]
                )
            });

        let output = s3utils::wait(&self.canceller, &self.runtime, upload_part_req_future)?;

        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };
        state.completed_parts.push(CompletedPart {
            e_tag: output.e_tag.clone(),
            part_number: Some(part_number),
        });
        gst_info!(self.cat, obj: element, "Uploaded part {}", part_number);

        Ok(())
    }

    fn create_upload_part_request(&self) -> Result<UploadPartRequest, gst::ErrorMessage> {
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
            bucket: settings.bucket.as_ref().unwrap().to_owned(),
            key: settings.key.as_ref().unwrap().to_owned(),
            upload_id: state.upload_id.to_owned(),
            part_number,
            ..Default::default()
        })
    }

    fn create_complete_multipart_upload_request(&self) -> CompleteMultipartUploadRequest {
        let mut state = self.state.lock().unwrap();

        let started_state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => unreachable!("Cannot stop before start"),
        };

        started_state
            .completed_parts
            .sort_by(|a, b| a.part_number.cmp(&b.part_number));

        let completed_upload = CompletedMultipartUpload {
            parts: Some(std::mem::replace(
                &mut started_state.completed_parts,
                Vec::new(),
            )),
        };

        let settings = self.settings.lock().unwrap();
        CompleteMultipartUploadRequest {
            bucket: settings.bucket.as_ref().unwrap().to_owned(),
            key: settings.key.as_ref().unwrap().to_owned(),
            upload_id: started_state.upload_id.to_owned(),
            multipart_upload: Some(completed_upload),
            ..Default::default()
        }
    }

    fn create_create_multipart_upload_request(
        &self,
    ) -> Result<CreateMultipartUploadRequest, gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        if settings.bucket.is_none() || settings.key.is_none() {
            return Err(gst_error_msg!(
                gst::ResourceError::Settings,
                ["Bucket or key is not defined"]
            ));
        }

        let bucket = settings.bucket.as_ref().unwrap();
        let key = settings.key.as_ref().unwrap();

        Ok(CreateMultipartUploadRequest {
            bucket: bucket.clone(),
            key: key.clone(),
            content_type: settings.content_type.clone(),
            ..Default::default()
        })
    }

    fn finalize_upload(&self, element: &gst_base::BaseSink) -> Result<(), gst::ErrorMessage> {
        if self.flush_current_buffer(element).is_err() {
            return Err(gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to flush internal buffer."]
            ));
        }

        let complete_req = self.create_complete_multipart_upload_request();
        let complete_req_future = self
            .client
            .lock()
            .unwrap()
            .complete_multipart_upload(complete_req)
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::Write,
                    ["Failed to complete multipart upload: {}.", err.to_string()]
                )
            });

        s3utils::wait(&self.canceller, &self.runtime, complete_req_future)
            .map_err(|err| {
                err.unwrap_or(gst_error_msg!(
                    gst::LibraryError::Failed,
                    ["Interrupted during stop"]
                ))
            })
            .map(|_| ())
    }

    fn start(&self) -> Result<Started, gst::ErrorMessage> {
        let create_multipart_req = self.create_create_multipart_upload_request()?;
        let create_multipart_req_future = self
            .client
            .lock()
            .unwrap()
            .create_multipart_upload(create_multipart_req)
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to create multipart upload: {}", err]
                )
            });
        let response = s3utils::wait(&self.canceller, &self.runtime, create_multipart_req_future)
            .map_err(|err| {
            err.unwrap_or(gst_error_msg!(
                gst::LibraryError::Failed,
                ["Interrupted during start"]
            ))
        })?;

        let upload_id = response.upload_id.ok_or(gst_error_msg!(
            gst::ResourceError::Failed,
            ["Failed to get multipart upload ID"]
        ))?;

        Ok(Started::new(
            Vec::with_capacity(self.settings.lock().unwrap().buffer_size as usize),
            upload_id,
        ))
    }

    fn update_buffer(
        &self,
        src: &[u8],
        element: &gst_base::BaseSink,
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

        if let Some(_) = canceller.take() {
            /* We don't do anything, the Sender will be dropped, and that will cause the
             * Receiver to be cancelled */
        }
    }
}

impl ObjectSubclass for S3Sink {
    const NAME: &'static str = "S3Sink";
    type ParentType = gst_base::BaseSink;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            cat: gst::DebugCategory::new(
                "s3sink",
                gst::DebugColorFlags::empty(),
                Some("Amazon S3 Sink"),
            ),
            canceller: Mutex::new(None),
            runtime: runtime::Builder::new()
                .core_threads(1)
                .name_prefix("S3-sink-runtime")
                .build()
                .unwrap(),
            client: Mutex::new(S3Client::new(Region::default())),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Amazon S3 sink",
            "Source/Network",
            "Writes an object to Amazon S3",
            "Marcin Kolny <mkolny@amazon.com>",
        );

        let caps = gst::Caps::new_any();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for S3Sink {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];
        let mut settings = self.settings.lock().unwrap();

        match *prop {
            subclass::Property("bucket", ..) => {
                settings.bucket = value.get::<String>();
            }
            subclass::Property("key", ..) => {
                settings.key = value.get::<String>();
            }
            subclass::Property("region", ..) => {
                let region = Region::from_str(&value.get::<String>().unwrap()).unwrap();
                if settings.region != region {
                    let mut client = self.client.lock().unwrap();
                    std::mem::replace(&mut *client, S3Client::new(region.clone()));
                    settings.region = region;
                }
            }
            subclass::Property("part-size", ..) => {
                settings.buffer_size = value.get::<u64>().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];
        let settings = self.settings.lock().unwrap();

        match *prop {
            subclass::Property("key", ..) => Ok(settings.key.to_value()),
            subclass::Property("bucket", ..) => Ok(settings.bucket.to_value()),
            subclass::Property("region", ..) => Ok(settings.region.name().to_value()),
            subclass::Property("part-size", ..) => Ok(settings.buffer_size.to_value()),
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for S3Sink {}

impl BaseSinkImpl for S3Sink {
    fn start(&self, _element: &gst_base::BaseSink) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if let State::Started(_) = *state {
            unreachable!("S3Sink already started");
        }

        *state = State::Started(self.start()?);

        Ok(())
    }

    fn stop(&self, element: &gst_base::BaseSink) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        *state = State::Stopped;
        gst_info!(self.cat, obj: element, "Stopped");

        Ok(())
    }

    fn render(
        &self,
        element: &gst_base::BaseSink,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let State::Stopped = *self.state.lock().unwrap() {
            gst_element_error!(element, gst::CoreError::Failed, ["Not started yet"]);
            return Err(gst::FlowError::Error);
        }

        gst_trace!(self.cat, obj: element, "Rendering {:?}", buffer);
        let map = buffer.map_readable().ok_or_else(|| {
            gst_element_error!(element, gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        match self.update_buffer(&map, element) {
            Ok(_) => Ok(gst::FlowSuccess::Ok),
            Err(err) => match err {
                Some(error_message) => {
                    gst_error!(
                        self.cat,
                        obj: element,
                        "Multipart upload failed: {}",
                        error_message
                    );
                    element.post_error_message(&error_message);
                    Err(gst::FlowError::Error)
                }
                _ => {
                    gst_info!(self.cat, obj: element, "Upload interrupted. Flushing...");
                    Err(gst::FlowError::Flushing)
                }
            },
        }
    }

    fn unlock(&self, _element: &gst_base::BaseSink) -> Result<(), gst::ErrorMessage> {
        self.cancel();

        Ok(())
    }

    fn event(&self, element: &gst_base::BaseSink, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Eos(_) => {
                if let Err(error_message) = self.finalize_upload(element) {
                    gst_error!(
                        self.cat,
                        obj: element,
                        "Failed to finalize the upload: {}",
                        error_message
                    );
                    return false;
                }
            }
            _ => (),
        }

        BaseSinkImplExt::parent_event(self, element, event)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(Some(plugin), "s3sink", gst::Rank::None, S3Sink::get_type())
}
