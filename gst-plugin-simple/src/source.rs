// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::u64;

use std::sync::Mutex;

use url::Url;

use glib;
use gst;
use gst::prelude::*;
use gst_base::prelude::*;

use gobject_subclass::object::*;

use gst_plugin::base_src::*;
use gst_plugin::element::*;
use gst_plugin::error::*;
use gst_plugin::uri_handler::*;

pub use gst_plugin::base_src::BaseSrc;

use error::*;

use UriValidator;

pub trait SourceImpl: Send + 'static {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn is_seekable(&self, src: &BaseSrc) -> bool;
    fn get_size(&self, src: &BaseSrc) -> Option<u64>;

    fn start(&mut self, src: &BaseSrc, uri: Url) -> Result<(), gst::ErrorMessage>;
    fn stop(&mut self, src: &BaseSrc) -> Result<(), gst::ErrorMessage>;
    fn fill(
        &mut self,
        src: &BaseSrc,
        offset: u64,
        length: u32,
        buffer: &mut gst::BufferRef,
    ) -> Result<(), FlowError>;
    fn seek(
        &mut self,
        src: &BaseSrc,
        start: u64,
        stop: Option<u64>,
    ) -> Result<(), gst::ErrorMessage>;
}

struct Source {
    cat: gst::DebugCategory,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    imp: Mutex<Box<SourceImpl>>,
    push_only: bool,
}

static PROPERTIES: [Property; 1] = [Property::String(
    "uri",
    "URI",
    "URI to read from",
    None,
    PropertyMutability::ReadWrite,
)];

impl Source {
    fn new(source: &BaseSrc, source_info: &SourceInfo) -> Self {
        let source_impl = (source_info.create_instance)(source);

        Self {
            cat: gst::DebugCategory::new(
                "rssource",
                gst::DebugColorFlags::empty(),
                "Rust source base class",
            ),
            uri: Mutex::new((None, false)),
            uri_validator: source_impl.uri_validator(),
            imp: Mutex::new(source_impl),
            push_only: source_info.push_only,
        }
    }

    fn class_init(klass: &mut BaseSrcClass, source_info: &SourceInfo) {
        klass.set_metadata(
            &source_info.long_name,
            &source_info.classification,
            &source_info.description,
            &source_info.author,
        );

        let caps = gst::Caps::new_any();
        let pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &BaseSrc, source_info: &SourceInfo) -> Box<BaseSrcImpl<BaseSrc>> {
        element.set_blocksize(4096);

        let imp = Self::new(element, source_info);
        Box::new(imp)
    }

    fn get_uri(&self, _element: &glib::Object) -> Option<String> {
        let uri_storage = &self.uri.lock().unwrap();
        uri_storage.0.as_ref().map(|uri| String::from(uri.as_str()))
    }

    fn set_uri(&self, element: &glib::Object, uri_str: Option<String>) -> Result<(), glib::Error> {
        let src = element.clone().dynamic_cast::<BaseSrc>().unwrap();

        let uri_storage = &mut self.uri.lock().unwrap();

        gst_debug!(self.cat, obj: &src, "Setting URI {:?}", uri_str);

        if uri_storage.1 {
            return Err(
                UriError::new(gst::URIError::BadState, "Already started".to_string()).into(),
            );
        }

        uri_storage.0 = None;

        if let Some(uri_str) = uri_str {
            match Url::parse(uri_str.as_str()) {
                Ok(uri) => {
                    try!((self.uri_validator)(&uri).map_err(|e| e.into()));
                    uri_storage.0 = Some(uri);
                    Ok(())
                }
                Err(err) => Err(UriError::new(
                    gst::URIError::BadUri,
                    format!("Failed to parse URI '{}': {}", uri_str, err),
                )
                .into()),
            }
        } else {
            Ok(())
        }
    }
}

impl ObjectImpl<BaseSrc> for Source {
    fn set_property(&self, obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("uri", ..) => {
                self.set_uri(obj, value.get()).unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("uri", ..) => Ok(self.get_uri(obj).to_value()),
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl<BaseSrc> for Source {}

impl BaseSrcImpl<BaseSrc> for Source {
    fn start(&self, src: &BaseSrc) -> bool {
        gst_debug!(self.cat, obj: src, "Starting");

        // Don't keep the URI locked while we call start later
        let uri = match *self.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;
                uri.clone()
            }
            (None, _) => {
                gst_error!(self.cat, obj: src, "No URI given");
                gst_element_error!(src, gst::ResourceError::OpenRead, ["No URI given"]);
                return false;
            }
        };

        let source_impl = &mut self.imp.lock().unwrap();
        match source_impl.start(src, uri) {
            Ok(..) => {
                gst_trace!(self.cat, obj: src, "Started successfully");
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: src, "Failed to start: {:?}", msg);

                self.uri.lock().unwrap().1 = false;
                src.post_error_message(msg);
                false
            }
        }
    }

    fn stop(&self, src: &BaseSrc) -> bool {
        let source_impl = &mut self.imp.lock().unwrap();

        gst_debug!(self.cat, obj: src, "Stopping");

        match source_impl.stop(src) {
            Ok(..) => {
                gst_trace!(self.cat, obj: src, "Stopped successfully");
                self.uri.lock().unwrap().1 = false;
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: src, "Failed to stop: {:?}", msg);

                src.post_error_message(msg);
                false
            }
        }
    }

    fn query(&self, src: &BaseSrc, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        match query.view_mut() {
            QueryView::Scheduling(ref mut q) if self.push_only => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                return true;
            }
            _ => (),
        }

        BaseSrcBase::parent_query(src, query)
    }

    fn fill(
        &self,
        src: &BaseSrc,
        offset: u64,
        length: u32,
        buffer: &mut gst::BufferRef,
    ) -> gst::FlowReturn {
        let source_impl = &mut self.imp.lock().unwrap();

        gst_trace!(
            self.cat,
            obj: src,
            "Filling buffer {:?} with offset {} and length {}",
            buffer,
            offset,
            length
        );

        match source_impl.fill(src, offset, length, buffer) {
            Ok(()) => gst::FlowReturn::Ok,
            Err(flow_error) => {
                gst_error!(self.cat, obj: src, "Failed to fill: {:?}", flow_error);
                match flow_error {
                    FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                        src.post_error_message(msg);
                    }
                    _ => (),
                }
                flow_error.into()
            }
        }
    }

    fn do_seek(&self, src: &BaseSrc, segment: &mut gst::Segment) -> bool {
        let source_impl = &mut self.imp.lock().unwrap();

        let segment = match segment.downcast_ref::<gst::format::Bytes>() {
            None => return false,
            Some(segment) => segment,
        };

        let start = match segment.get_start().0 {
            None => return false,
            Some(start) => start,
        };
        let stop = segment.get_stop().0;

        gst_debug!(self.cat, obj: src, "Seeking to {:?}-{:?}", start, stop);

        match source_impl.seek(src, start, stop) {
            Ok(..) => true,
            Err(ref msg) => {
                gst_error!(self.cat, obj: src, "Failed to seek {:?}", msg);
                src.post_error_message(msg);
                false
            }
        }
    }

    fn is_seekable(&self, src: &BaseSrc) -> bool {
        let source_impl = &self.imp.lock().unwrap();
        source_impl.is_seekable(src)
    }

    fn get_size(&self, src: &BaseSrc) -> Option<u64> {
        let source_impl = &self.imp.lock().unwrap();
        source_impl.get_size(src)
    }
}

impl URIHandlerImpl for Source {
    fn get_uri(&self, element: &gst::URIHandler) -> Option<String> {
        Source::get_uri(self, &element.clone().upcast())
    }

    fn set_uri(&self, element: &gst::URIHandler, uri: Option<String>) -> Result<(), glib::Error> {
        Source::set_uri(self, &element.clone().upcast(), uri)
    }
}

pub struct SourceInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(&BaseSrc) -> Box<SourceImpl>,
    pub protocols: Vec<String>,
    pub push_only: bool,
}

struct SourceStatic {
    name: String,
    source_info: SourceInfo,
}

impl ImplTypeStatic<BaseSrc> for SourceStatic {
    fn get_name(&self) -> &str {
        self.name.as_str()
    }

    fn new(&self, element: &BaseSrc) -> Box<BaseSrcImpl<BaseSrc>> {
        Source::init(element, &self.source_info)
    }

    fn class_init(&self, klass: &mut BaseSrcClass) {
        Source::class_init(klass, &self.source_info);
    }

    fn type_init(&self, token: &TypeInitToken, type_: glib::Type) {
        register_uri_handler(token, type_, self);
    }
}

impl URIHandlerImplStatic<BaseSrc> for SourceStatic {
    fn get_impl<'a>(&self, imp: &'a Box<BaseSrcImpl<BaseSrc>>) -> &'a URIHandlerImpl {
        imp.downcast_ref::<Source>().unwrap()
    }

    fn get_type(&self) -> gst::URIType {
        gst::URIType::Src
    }

    fn get_protocols(&self) -> Vec<String> {
        self.source_info.protocols.clone()
    }
}

pub fn source_register(plugin: &gst::Plugin, source_info: SourceInfo) {
    let name = source_info.name.clone();
    let rank = source_info.rank;

    let source_static = SourceStatic {
        name: format!("Source-{}", name),
        source_info,
    };

    let type_ = register_type(source_static);
    gst::Element::register(plugin, &name, rank, type_);
}
