// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::sync::Mutex;

use bytes::Bytes;
use futures::sync::oneshot;
use futures::{Future, Stream};
use rusoto_s3::*;
use tokio::runtime;

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;

use gst;
use gst::subclass::prelude::*;

use gst_base;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use crate::s3url::*;
use crate::s3utils;

#[allow(clippy::large_enum_variant)]
enum StreamingState {
    Stopped,
    Started {
        url: GstS3Url,
        client: S3Client,
        size: u64,
    },
}

pub struct S3Src {
    url: Mutex<Option<GstS3Url>>,
    state: Mutex<StreamingState>,
    runtime: runtime::Runtime,
    canceller: Mutex<Option<oneshot::Sender<Bytes>>>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "rusotos3src",
        gst::DebugColorFlags::empty(),
        Some("Amazon S3 Source"),
    );
}

static PROPERTIES: [subclass::Property; 1] = [subclass::Property("uri", |name| {
    glib::ParamSpec::string(
        name,
        "URI",
        "The S3 object URI",
        None,
        glib::ParamFlags::READWRITE, /* + GST_PARAM_MUTABLE_READY) */
    )
})];

impl S3Src {
    fn cancel(&self) {
        let mut canceller = self.canceller.lock().unwrap();

        if let Some(_) = canceller.take() {
            /* We don't do anything, the Sender will be dropped, and that will cause the
             * Receiver to be cancelled */
        }
    }

    fn connect(self: &S3Src, url: &GstS3Url) -> Result<S3Client, gst::ErrorMessage> {
        Ok(S3Client::new(url.region.clone()))
    }

    fn set_uri(
        self: &S3Src,
        _: &gst_base::BaseSrc,
        url_str: Option<&str>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();

        if let StreamingState::Started { .. } = *state {
            return Err(gst::Error::new(
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
            Err(_) => Err(gst::Error::new(
                gst::URIError::BadUri,
                "Could not parse URI",
            )),
        }
    }

    fn head(
        self: &S3Src,
        src: &gst_base::BaseSrc,
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

        let output = s3utils::wait(
            &self.canceller,
            &self.runtime,
            response.map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::NotFound,
                    ["Failed to HEAD object: {}", err]
                )
            }),
        )
        .map_err(|err| {
            err.unwrap_or_else(|| {
                gst_error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
            })
        })?;

        if let Some(size) = output.content_length {
            gst_info!(CAT, obj: src, "HEAD success, content length = {}", size);
            Ok(size as u64)
        } else {
            Err(gst_error_msg!(
                gst::ResourceError::Read,
                ["Failed to get content length"]
            ))
        }
    }

    /* Returns the bytes, Some(error) if one occured, or a None error if interrupted */
    fn get(
        self: &S3Src,
        src: &gst_base::BaseSrc,
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
                return Err(Some(gst_error_msg!(
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

        /* Drop the state lock now that we're done with it and need the next part to be
         * interruptible */
        drop(state);

        let output = s3utils::wait(
            &self.canceller,
            &self.runtime,
            response.map_err(|err| {
                gst_error_msg!(gst::ResourceError::Read, ["Could not read: {}", err])
            }),
        )?;

        gst_debug!(
            CAT,
            obj: src,
            "Read {} bytes",
            output.content_length.unwrap()
        );

        s3utils::wait(
            &self.canceller,
            &self.runtime,
            output.body.unwrap().concat2().map_err(|err| {
                gst_error_msg!(gst::ResourceError::Read, ["Could not read: {}", err])
            }),
        )
    }
}

impl ObjectSubclass for S3Src {
    const NAME: &'static str = "RusotoS3Src";
    type ParentType = gst_base::BaseSrc;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            url: Mutex::new(None),
            state: Mutex::new(StreamingState::Stopped),
            runtime: runtime::Builder::new()
                .core_threads(1)
                .name_prefix("rusotos3src-runtime")
                .build()
                .unwrap(),
            canceller: Mutex::new(None),
        }
    }

    fn type_init(typ: &mut subclass::InitializingType<Self>) {
        typ.add_interface::<gst::URIHandler>();
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Amazon S3 source",
            "Source/Network",
            "Reads an object from Amazon S3",
            "Arun Raghavan <arun@arunraghavan.net>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for S3Src {
    glib_object_impl!();

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];
        let basesrc = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();

        match *prop {
            subclass::Property("uri", ..) => {
                let _ = self.set_uri(basesrc, value.get().expect("type checked upstream"));
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            subclass::Property("uri", ..) => {
                let url = match *self.url.lock().unwrap() {
                    Some(ref url) => url.to_string(),
                    None => "".to_string(),
                };

                Ok(url.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let basesrc = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();
        basesrc.set_format(gst::Format::Bytes);
        /* Set a larger default blocksize to make read more efficient */
        basesrc.set_blocksize(256 * 1024);
    }
}

impl ElementImpl for S3Src {
    // No overrides
}

impl URIHandlerImpl for S3Src {
    fn get_uri(&self, _: &gst::URIHandler) -> Option<String> {
        self.url.lock().unwrap().as_ref().map(|s| s.to_string())
    }

    fn set_uri(&self, element: &gst::URIHandler, uri: &str) -> Result<(), glib::Error> {
        let basesrc = element.dynamic_cast_ref::<gst_base::BaseSrc>().unwrap();
        self.set_uri(basesrc, Some(uri))
    }

    fn get_uri_type() -> gst::URIType {
        gst::URIType::Src
    }

    fn get_protocols() -> Vec<String> {
        vec!["s3".to_string()]
    }
}

impl BaseSrcImpl for S3Src {
    fn is_seekable(&self, _: &gst_base::BaseSrc) -> bool {
        true
    }

    fn get_size(&self, _: &gst_base::BaseSrc) -> Option<u64> {
        match *self.state.lock().unwrap() {
            StreamingState::Stopped => None,
            StreamingState::Started { size, .. } => Some(size),
        }
    }

    fn start(&self, src: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        let state = self.state.lock().unwrap();

        if let StreamingState::Started { .. } = *state {
            unreachable!("RusotoS3Src is already started");
        }

        /* Drop the lock as self.head() needs it */
        drop(state);

        let s3url = match *self.url.lock().unwrap() {
            Some(ref url) => url.clone(),
            None => {
                return Err(gst_error_msg!(
                    gst::ResourceError::Settings,
                    ["Cannot start without a URL being set"]
                ));
            }
        };

        let s3client = self.connect(&s3url)?;

        let size = self.head(src, &s3client, &s3url)?;

        let mut state = self.state.lock().unwrap();

        *state = StreamingState::Started {
            url: s3url,
            client: s3client,
            size,
        };

        Ok(())
    }

    fn stop(&self, _: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if let StreamingState::Stopped = *state {
            unreachable!("Cannot stop before start");
        }

        *state = StreamingState::Stopped;

        Ok(())
    }

    fn query(&self, src: &gst_base::BaseSrc, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryView::Scheduling(ref mut q) => {
                q.set(
                    gst::SchedulingFlags::SEQUENTIAL | gst::SchedulingFlags::BANDWIDTH_LIMITED,
                    1,
                    -1,
                    0,
                );
                q.add_scheduling_modes(&[gst::PadMode::Push, gst::PadMode::Pull]);
                return true;
            }
            _ => (),
        }

        BaseSrcImplExt::parent_query(self, src, query)
    }

    fn create(
        &self,
        src: &gst_base::BaseSrc,
        offset: u64,
        length: u32,
    ) -> Result<gst::Buffer, gst::FlowError> {
        // FIXME: sanity check on offset and length
        let data = self.get(src, offset, u64::from(length));

        match data {
            /* Got data */
            Ok(bytes) => Ok(gst::Buffer::from_slice(bytes)),
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
    fn do_seek(&self, _: &gst_base::BaseSrc, _: &mut gst::Segment) -> bool {
        true
    }

    fn unlock(&self, _: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        self.cancel();
        Ok(())
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rusotos3src",
        gst::Rank::Primary,
        S3Src::get_type(),
    )
}
