// Copyright (C) 2016-2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use hyperx::header::{
    AcceptRanges, ByteRangeSpec, ContentLength, ContentRange, ContentRangeSpec, Headers, Range,
    RangeUnit,
};
use reqwest::{Client, Response};
use std::io::Read;
use std::sync::Mutex;
use std::u64;
use url::Url;

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

const DEFAULT_LOCATION: Option<Url> = None;

#[derive(Debug, Clone)]
struct Settings {
    location: Option<Url>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            location: DEFAULT_LOCATION,
        }
    }
}

static PROPERTIES: [subclass::Property; 1] = [subclass::Property("location", |name| {
    glib::ParamSpec::string(
        name,
        "File Location",
        "URL to read from",
        None,
        glib::ParamFlags::READWRITE,
    )
})];

#[derive(Debug)]
enum State {
    Stopped,
    Started {
        uri: Url,
        response: Response,
        seekable: bool,
        position: u64,
        size: Option<u64>,
        start: u64,
        stop: Option<u64>,
    },
}

impl Default for State {
    fn default() -> Self {
        State::Stopped
    }
}

#[derive(Debug)]
pub struct HttpSrc {
    cat: gst::DebugCategory,
    client: Client,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl HttpSrc {
    fn set_location(
        &self,
        _element: &gst_base::BaseSrc,
        uri: Option<String>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Changing the `location` property on a started `httpsrc` is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();

        let uri = match uri {
            Some(uri) => uri,
            None => {
                settings.location = None;
                return Ok(());
            }
        };

        let uri = Url::parse(uri.as_str()).map_err(|err| {
            glib::Error::new(
                gst::URIError::BadUri,
                format!("Failed to parse URI '{}': {:?}", uri, err,).as_str(),
            )
        })?;

        if uri.scheme() != "http" && uri.scheme() != "https" {
            return Err(glib::Error::new(
                gst::URIError::UnsupportedProtocol,
                format!("Unsupported URI scheme '{}'", uri.scheme(),).as_str(),
            ));
        }

        settings.location = Some(uri);

        Ok(())
    }

    fn do_request(
        &self,
        src: &gst_base::BaseSrc,
        uri: Url,
        start: u64,
        stop: Option<u64>,
    ) -> Result<State, gst::ErrorMessage> {
        let cat = self.cat;
        let req = self.client.get(uri.clone());

        let mut headers = Headers::new();

        match (start != 0, stop) {
            (false, None) => (),
            (true, None) => {
                headers.set(Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]));
            }
            (_, Some(stop)) => {
                headers.set(Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop - 1)]));
            }
        }

        // Add all headers for the request here
        let req = req.headers(headers.into());

        gst_debug!(cat, obj: src, "Doing new request {:?}", req);

        let response = try!(req.send().or_else(|err| {
            gst_error!(cat, obj: src, "Request failed: {:?}", err);
            Err(gst_error_msg!(
                gst::ResourceError::Read,
                ["Failed to fetch {}: {}", uri, err.to_string()]
            ))
        }));

        if !response.status().is_success() {
            gst_error!(cat, obj: src, "Request status failed: {:?}", response);
            return Err(gst_error_msg!(
                gst::ResourceError::Read,
                ["Failed to fetch {}: {}", uri, response.status()]
            ));
        }

        let headers = Headers::from(response.headers());
        let size = headers.get().map(|&ContentLength(cl)| cl + start);

        let accept_byte_ranges = if let Some(&AcceptRanges(ref ranges)) = headers.get() {
            ranges.iter().any(|u| *u == RangeUnit::Bytes)
        } else {
            false
        };

        let seekable = size.is_some() && accept_byte_ranges;

        let position = if let Some(&ContentRange(ContentRangeSpec::Bytes {
            range: Some((range_start, _)),
            ..
        })) = headers.get()
        {
            range_start
        } else {
            start
        };

        if position != start {
            return Err(gst_error_msg!(
                gst::ResourceError::Seek,
                ["Failed to seek to {}: Got {}", start, position]
            ));
        }

        gst_debug!(cat, obj: src, "Request successful: {:?}", response);

        Ok(State::Started {
            uri,
            response,
            seekable,
            position: 0,
            size,
            start,
            stop,
        })
    }
}

impl ObjectImpl for HttpSrc {
    glib_object_impl!();

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();

                let location = value.get::<String>();
                let res = self.set_location(element, location);

                if let Err(err) = res {
                    gst_error!(
                        self.cat,
                        obj: element,
                        "Failed to set property `location`: {:?}",
                        err
                    );
                }
            }
            _ => unimplemented!(),
        };
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let settings = self.settings.lock().unwrap();
                let location = settings.location.as_ref().map(Url::to_string);

                Ok(location.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();
        element.set_format(gst::Format::Bytes);
    }
}

impl ElementImpl for HttpSrc {}

impl BaseSrcImpl for HttpSrc {
    fn is_seekable(&self, _src: &gst_base::BaseSrc) -> bool {
        match *self.state.lock().unwrap() {
            State::Started { seekable, .. } => seekable,
            _ => false,
        }
    }

    fn get_size(&self, _src: &gst_base::BaseSrc) -> Option<u64> {
        match *self.state.lock().unwrap() {
            State::Started { size, .. } => size,
            _ => None,
        }
    }

    fn start(&self, src: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        *state = State::Stopped;

        let uri = self
            .settings
            .lock()
            .unwrap()
            .location
            .as_ref()
            .ok_or_else(|| {
                gst_error_msg!(gst::CoreError::StateChange, ["Can't start without an URI"])
            })
            .map(|uri| uri.clone())?;

        *state = self.do_request(src, uri, 0, None)?;

        Ok(())
    }

    fn stop(&self, _src: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State::Stopped;

        Ok(())
    }

    fn do_seek(&self, src: &gst_base::BaseSrc, segment: &mut gst::Segment) -> bool {
        let segment = segment.downcast_mut::<gst::format::Bytes>().unwrap();

        let mut state = self.state.lock().unwrap();

        let (position, old_stop, uri) = match *state {
            State::Started {
                position,
                stop,
                ref uri,
                ..
            } => (position, stop, uri.clone()),
            State::Stopped => {
                gst_element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return false;
            }
        };

        let start = segment.get_start().expect("No start position given");
        let stop = segment.get_stop();

        if position == start && old_stop == stop.0 {
            return true;
        }

        *state = State::Stopped;
        match self.do_request(src, uri, start, stop.0) {
            Ok(s) => {
                *state = s;
                true
            }
            Err(err) => {
                src.post_error_message(&err);
                false
            }
        }
    }

    fn fill(
        &self,
        src: &gst_base::BaseSrc,
        offset: u64,
        _: u32,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let (response, position) = match *state {
            State::Started {
                ref mut response,
                ref mut position,
                ..
            } => (response, position),
            State::Stopped => {
                gst_element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return Err(gst::FlowError::Error);
            }
        };

        if *position != offset {
            gst_element_error!(
                src,
                gst::ResourceError::Seek,
                ["Got unexpected offset {}, expected {}", offset, position]
            );

            return Err(gst::FlowError::Error);
        }

        let size = {
            let mut map = buffer.map_writable().ok_or_else(|| {
                gst_element_error!(src, gst::LibraryError::Failed, ["Failed to map buffer"]);

                gst::FlowError::Error
            })?;

            let data = map.as_mut_slice();

            response.read(data).map_err(|err| {
                gst_error!(self.cat, obj: src, "Failed to read: {:?}", err);
                gst_element_error!(
                    src,
                    gst::ResourceError::Read,
                    ["Failed to read at {}: {}", offset, err.to_string()]
                );

                gst::FlowError::Error
            })?
        };

        if size == 0 {
            return Err(gst::FlowError::Eos);
        }

        *position += size as u64;

        buffer.set_size(size);

        Ok(gst::FlowSuccess::Ok)
    }
}

impl URIHandlerImpl for HttpSrc {
    fn get_uri(&self, _element: &gst::URIHandler) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.location.as_ref().map(Url::to_string)
    }

    fn set_uri(&self, element: &gst::URIHandler, uri: Option<String>) -> Result<(), glib::Error> {
        let element = element.dynamic_cast_ref::<gst_base::BaseSrc>().unwrap();

        self.set_location(&element, uri)
    }

    fn get_uri_type() -> gst::URIType {
        gst::URIType::Src
    }

    fn get_protocols() -> Vec<String> {
        vec!["http".to_string(), "https".to_string()]
    }
}

impl ObjectSubclass for HttpSrc {
    const NAME: &'static str = "RsHttpSrc";
    type ParentType = gst_base::BaseSrc;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "rshttpsrc",
                gst::DebugColorFlags::empty(),
                "Rust HTTP source",
            ),
            client: Client::new(),
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
        }
    }

    fn type_init(type_: &mut subclass::InitializingType<Self>) {
        type_.add_interface::<gst::URIHandler>();
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "HTTP Source",
            "Source/Network/HTTP",
            "Read stream from an HTTP/HTTPS location",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(plugin, "rshttpsrc", 256 + 100, HttpSrc::get_type())
}
