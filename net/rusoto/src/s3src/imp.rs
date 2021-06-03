// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::sync::Mutex;

use bytes::Bytes;
use futures::future;
use once_cell::sync::Lazy;
use rusoto_s3::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info};

use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use crate::s3url::*;
use crate::s3utils::{self, WaitError};

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
pub struct S3Src {
    url: Mutex<Option<GstS3Url>>,
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
        S3Client::new(url.region.clone())
    }

    fn set_uri(self: &S3Src, _: &super::S3Src, url_str: Option<&str>) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();

        if let StreamingState::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Cannot set URI on a started s3src",
            ));
        }

        let mut url = self.url.lock().unwrap();

        if url_str.is_none() {
            *url = None;
            return Ok(());
        }

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

    fn head(
        self: &S3Src,
        src: &super::S3Src,
        client: &S3Client,
        url: &GstS3Url,
    ) -> Result<u64, gst::ErrorMessage> {
        let request = HeadObjectRequest {
            bucket: url.bucket.clone(),
            key: url.object.clone(),
            version_id: url.version.clone(),
            ..Default::default()
        };

        let response = client.head_object(request);

        let output = s3utils::wait(&self.canceller, response).map_err(|err| match err {
            WaitError::FutureError(err) => gst::error_msg!(
                gst::ResourceError::NotFound,
                ["Failed to HEAD object: {}", err]
            ),
            WaitError::Cancelled => {
                gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
            }
        })?;

        if let Some(size) = output.content_length {
            gst_info!(CAT, obj: src, "HEAD success, content length = {}", size);
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

        let request = GetObjectRequest {
            bucket: url.bucket.clone(),
            key: url.object.clone(),
            range: Some(format!("bytes={}-{}", offset, offset + length - 1)),
            version_id: url.version.clone(),
            ..Default::default()
        };

        gst_debug!(
            CAT,
            obj: src,
            "Requesting range: {}-{}",
            offset,
            offset + length - 1
        );

        let response = client.get_object(request);

        let output = s3utils::wait(&self.canceller, response).map_err(|err| match err {
            WaitError::FutureError(err) => Some(gst::error_msg!(
                gst::ResourceError::Read,
                ["Could not read: {}", err]
            )),
            WaitError::Cancelled => None,
        })?;

        gst_debug!(
            CAT,
            obj: src,
            "Read {} bytes",
            output.content_length.unwrap()
        );

        s3utils::wait_stream(&self.canceller, &mut output.body.unwrap()).map_err(|err| match err {
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
    const NAME: &'static str = "RusotoS3Src";
    type Type = super::S3Src;
    type ParentType = gst_base::BaseSrc;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for S3Src {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpec::new_string(
                "uri",
                "URI",
                "The S3 object URI",
                None,
                glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
            )]
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
            _ => unimplemented!(),
        }
    }

    fn property(&self, _: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
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

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_format(gst::Format::Bytes);
        /* Set a larger default blocksize to make read more efficient */
        obj.set_blocksize(256 * 1024);
    }
}

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
        self.url.lock().unwrap().as_ref().map(|s| s.to_string())
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

        let s3url = match *self.url.lock().unwrap() {
            Some(ref url) => url.clone(),
            None => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["Cannot start without a URL being set"]
                ));
            }
        };

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
        if let gst::QueryView::Scheduling(ref mut q) = query.view_mut() {
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
                gst_error!(CAT, obj: src, "Could not GET: {}", err);
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
