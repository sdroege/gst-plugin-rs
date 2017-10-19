// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensink.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::sync::Mutex;

use url::Url;

use glib;
use gst;
use gst::prelude::*;
use gst_base;
use gst_base::prelude::*;

use gst_plugin::object::*;
use gst_plugin::properties::*;
use gst_plugin::element::*;
use gst_plugin::base_sink::*;
use gst_plugin::uri_handler::*;
use gst_plugin::error::*;

pub use gst_plugin::base_sink::RsBaseSink;

use UriValidator;

pub trait SinkImpl: Send + 'static {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn start(&mut self, sink: &RsBaseSink, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self, sink: &RsBaseSink) -> Result<(), ErrorMessage>;
    fn render(&mut self, sink: &RsBaseSink, buffer: &gst::BufferRef) -> Result<(), FlowError>;
}

struct Sink {
    cat: gst::DebugCategory,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    imp: Mutex<Box<SinkImpl>>,
}

static PROPERTIES: [Property; 1] = [
    Property::String(
        "uri",
        "URI",
        "URI to read from",
        None,
        PropertyMutability::ReadWrite,
    ),
];

impl Sink {
    fn new(sink: &RsBaseSink, sink_info: &SinkInfo) -> Self {
        let sink_impl = (sink_info.create_instance)(sink);

        Self {
            cat: gst::DebugCategory::new(
                "rssink",
                gst::DebugColorFlags::empty(),
                "Rust sink base class",
            ),
            uri: Mutex::new((None, false)),
            uri_validator: sink_impl.uri_validator(),
            imp: Mutex::new(sink_impl),
        }
    }

    fn class_init(klass: &mut RsBaseSinkClass, sink_info: &SinkInfo) {
        klass.set_metadata(
            &sink_info.long_name,
            &sink_info.classification,
            &sink_info.description,
            &sink_info.author,
        );

        let caps = gst::Caps::new_any();
        let pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &RsBaseSink, sink_info: &SinkInfo) -> Box<BaseSinkImpl<RsBaseSink>> {
        element.set_blocksize(4096);

        let imp = Self::new(element, sink_info);
        Box::new(imp)
    }

    fn get_uri(&self, _element: &glib::Object) -> Option<String> {
        let uri_storage = &self.uri.lock().unwrap();
        uri_storage.0.as_ref().map(|uri| String::from(uri.as_str()))
    }

    fn set_uri(&self, element: &glib::Object, uri_str: Option<String>) -> Result<(), glib::Error> {
        let sink = element.clone().dynamic_cast::<RsBaseSink>().unwrap();

        let uri_storage = &mut self.uri.lock().unwrap();

        gst_debug!(self.cat, obj: &sink, "Setting URI {:?}", uri_str);

        if uri_storage.1 {
            return Err(
                UriError::new(gst::URIError::BadState, "Already started".to_string()).into_error(),
            );
        }

        uri_storage.0 = None;

        if let Some(uri_str) = uri_str {
            match Url::parse(uri_str.as_str()) {
                Ok(uri) => {
                    try!((self.uri_validator)(&uri).map_err(|e| e.into_error()));
                    uri_storage.0 = Some(uri);
                    Ok(())
                }
                Err(err) => Err(
                    UriError::new(
                        gst::URIError::BadUri,
                        format!("Failed to parse URI '{}': {}", uri_str, err),
                    ).into_error(),
                ),
            }
        } else {
            Ok(())
        }
    }
}

impl ObjectImpl<RsBaseSink> for Sink {
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

impl ElementImpl<RsBaseSink> for Sink {}

impl BaseSinkImpl<RsBaseSink> for Sink {
    fn start(&self, sink: &RsBaseSink) -> bool {
        gst_debug!(self.cat, obj: sink, "Starting");

        // Don't keep the URI locked while we call start later
        let uri = match *self.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;
                uri.clone()
            }
            (None, _) => {
                gst_error!(self.cat, obj: sink, "No URI given");
                error_msg!(gst::ResourceError::OpenRead, ["No URI given"]).post(sink);
                return false;
            }
        };

        let sink_impl = &mut self.imp.lock().unwrap();
        match sink_impl.start(sink, uri) {
            Ok(..) => {
                gst_trace!(self.cat, obj: sink, "Started successfully");
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: sink, "Failed to start: {:?}", msg);

                self.uri.lock().unwrap().1 = false;
                msg.post(sink);
                false
            }
        }
    }

    fn stop(&self, sink: &RsBaseSink) -> bool {
        let sink_impl = &mut self.imp.lock().unwrap();

        gst_debug!(self.cat, obj: sink, "Stopping");

        match sink_impl.stop(sink) {
            Ok(..) => {
                gst_trace!(self.cat, obj: sink, "Stopped successfully");
                self.uri.lock().unwrap().1 = false;
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: sink, "Failed to stop: {:?}", msg);

                msg.post(sink);
                false
            }
        }
    }

    fn render(&self, sink: &RsBaseSink, buffer: &gst::BufferRef) -> gst::FlowReturn {
        let sink_impl = &mut self.imp.lock().unwrap();

        gst_trace!(self.cat, obj: sink, "Rendering buffer {:?}", buffer,);

        match sink_impl.render(sink, buffer) {
            Ok(()) => gst::FlowReturn::Ok,
            Err(flow_error) => {
                gst_error!(self.cat, obj: sink, "Failed to render: {:?}", flow_error);
                match flow_error {
                    FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                        msg.post(sink);
                    }
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    }
}

impl URIHandlerImpl for Sink {
    fn get_uri(&self, element: &gst::URIHandler) -> Option<String> {
        Sink::get_uri(self, &element.clone().upcast())
    }

    fn set_uri(&self, element: &gst::URIHandler, uri: Option<String>) -> Result<(), glib::Error> {
        Sink::set_uri(self, &element.clone().upcast(), uri)
    }
}

pub struct SinkInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(&RsBaseSink) -> Box<SinkImpl>,
    pub protocols: Vec<String>,
}

struct SinkStatic {
    name: String,
    sink_info: SinkInfo,
}

impl ImplTypeStatic<RsBaseSink> for SinkStatic {
    fn get_name(&self) -> &str {
        self.name.as_str()
    }

    fn new(&self, element: &RsBaseSink) -> Box<BaseSinkImpl<RsBaseSink>> {
        Sink::init(element, &self.sink_info)
    }

    fn class_init(&self, klass: &mut RsBaseSinkClass) {
        Sink::class_init(klass, &self.sink_info);
    }

    fn type_init(&self, token: &TypeInitToken, type_: glib::Type) {
        register_uri_handler(token, type_, self);
    }
}

impl URIHandlerImplStatic<RsBaseSink> for SinkStatic {
    fn get_impl<'a>(&self, imp: &'a Box<BaseSinkImpl<RsBaseSink>>) -> &'a URIHandlerImpl {
        imp.downcast_ref::<Sink>().unwrap()
    }

    fn get_type(&self) -> gst::URIType {
        gst::URIType::Sink
    }

    fn get_protocols(&self) -> Vec<String> {
        self.sink_info.protocols.clone()
    }
}

pub fn sink_register(plugin: &gst::Plugin, sink_info: SinkInfo) {
    let name = sink_info.name.clone();
    let rank = sink_info.rank;

    let sink_static = SinkStatic {
        name: format!("Sink-{}", name),
        sink_info: sink_info,
    };

    let type_ = register_type(sink_static);
    gst::Element::register(plugin, &name, rank, type_);
}
