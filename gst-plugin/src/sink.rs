// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensink.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::ffi::{CStr, CString};
use std::ptr;
use std::mem;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use error::*;

use glib_ffi;
use gobject_ffi;
use gst_ffi;
use gst_base_ffi;

use glib::translate::*;
use gst;
use gst::prelude::*;
use gst_base;
use gst_base::prelude::*;

pub struct SinkWrapper {
    cat: gst::DebugCategory,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    sink: Mutex<Box<Sink>>,
    panicked: AtomicBool,
}

pub trait Sink {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn start(&mut self, sink: &RsSinkWrapper, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self, sink: &RsSinkWrapper) -> Result<(), ErrorMessage>;

    fn render(&mut self, sink: &RsSinkWrapper, buffer: &gst::BufferRef) -> Result<(), FlowError>;
}

impl SinkWrapper {
    fn new(sink: Box<Sink>) -> SinkWrapper {
        SinkWrapper {
            cat: gst::DebugCategory::new(
                "rssink",
                gst::DebugColorFlags::empty(),
                "Rust sink base class",
            ),
            uri: Mutex::new((None, false)),
            uri_validator: sink.uri_validator(),
            sink: Mutex::new(sink),
            panicked: AtomicBool::new(false),
        }
    }

    fn set_uri(&self, sink: &RsSinkWrapper, uri_str: Option<&str>) -> Result<(), UriError> {
        let uri_storage = &mut self.uri.lock().unwrap();

        gst_debug!(self.cat, obj: sink, "Setting URI {:?}", uri_str);

        if uri_storage.1 {
            return Err(UriError::new(
                gst::URIError::BadState,
                "Already started".to_string(),
            ));
        }

        uri_storage.0 = None;

        if let Some(uri_str) = uri_str {
            match Url::parse(uri_str) {
                Ok(uri) => {
                    try!((self.uri_validator)(&uri));
                    uri_storage.0 = Some(uri);
                    Ok(())
                }
                Err(err) => Err(UriError::new(
                    gst::URIError::BadUri,
                    format!("Failed to parse URI '{}': {}", uri_str, err),
                )),
            }
        } else {
            Ok(())
        }
    }

    fn get_uri(&self, _sink: &RsSinkWrapper) -> Option<String> {
        let uri_storage = &self.uri.lock().unwrap();
        uri_storage.0.as_ref().map(|uri| String::from(uri.as_str()))
    }

    fn start(&self, sink: &RsSinkWrapper) -> bool {
        gst_debug!(self.cat, obj: sink, "Starting");

        // Don't keep the URI locked while we call start later
        let uri = match *self.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;
                uri.clone()
            }
            (None, _) => {
                gst_error!(self.cat, obj: sink, "No URI given");
                self.post_message(
                    sink,
                    &error_msg!(gst::ResourceError::OpenWrite, ["No URI given"]),
                );
                return false;
            }
        };

        let sink_impl = &mut self.sink.lock().unwrap();
        match sink_impl.start(sink, uri) {
            Ok(..) => {
                gst_trace!(self.cat, obj: sink, "Started successfully");
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: sink, "Failed to start: {:?}", msg);

                self.uri.lock().unwrap().1 = false;
                self.post_message(sink, msg);
                false
            }
        }
    }

    fn stop(&self, sink: &RsSinkWrapper) -> bool {
        let sink_impl = &mut self.sink.lock().unwrap();

        gst_debug!(self.cat, obj: sink, "Stopping");

        match sink_impl.stop(sink) {
            Ok(..) => {
                gst_trace!(self.cat, obj: sink, "Stopped successfully");
                self.uri.lock().unwrap().1 = false;
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: sink, "Failed to stop: {:?}", msg);

                self.post_message(sink, msg);
                false
            }
        }
    }

    fn render(&self, sink: &RsSinkWrapper, buffer: &gst::BufferRef) -> gst::FlowReturn {
        let sink_impl = &mut self.sink.lock().unwrap();

        gst_trace!(self.cat, obj: sink, "Rendering buffer {:?}", buffer);

        match sink_impl.render(sink, buffer) {
            Ok(..) => gst::FlowReturn::Ok,
            Err(flow_error) => {
                gst_error!(self.cat, obj: sink, "Failed to render: {:?}", flow_error);
                match flow_error {
                    FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                        self.post_message(sink, msg)
                    }
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    }

    fn post_message(&self, sink: &RsSinkWrapper, msg: &ErrorMessage) {
        msg.post(sink);
    }
}

unsafe fn sink_set_uri(
    ptr: *mut RsSink,
    uri_ptr: *const c_char,
    cerr: *mut *mut glib_ffi::GError,
) -> glib_ffi::gboolean {
    let sink: &RsSinkWrapper = &from_glib_borrow(ptr as *mut RsSink);
    let wrap = sink.get_wrap();

    panic_to_error!(wrap, sink, false, {
        let uri_str = if uri_ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(uri_ptr).to_str().unwrap())
        };

        match wrap.set_uri(sink, uri_str) {
            Err(err) => {
                gst_error!(wrap.cat, obj: sink, "Failed to set URI {:?}", err);
                if !cerr.is_null() {
                    let err = err.into_error();
                    *cerr = err.to_glib_full() as *mut _;
                }
                false
            }
            Ok(_) => true,
        }
    }).to_glib()
}

unsafe fn sink_get_uri(ptr: *mut RsSink) -> *mut c_char {
    let sink: &RsSinkWrapper = &from_glib_borrow(ptr as *mut RsSink);
    let wrap = sink.get_wrap();

    panic_to_error!(wrap, sink, None, { wrap.get_uri(sink) }).to_glib_full()
}

unsafe extern "C" fn sink_start(ptr: *mut gst_base_ffi::GstBaseSink) -> glib_ffi::gboolean {
    let sink: &RsSinkWrapper = &from_glib_borrow(ptr as *mut RsSink);
    let wrap = sink.get_wrap();

    panic_to_error!(wrap, sink, false, { wrap.start(sink) }).to_glib()
}

unsafe extern "C" fn sink_stop(ptr: *mut gst_base_ffi::GstBaseSink) -> glib_ffi::gboolean {
    let sink: &RsSinkWrapper = &from_glib_borrow(ptr as *mut RsSink);
    let wrap = sink.get_wrap();

    panic_to_error!(wrap, sink, true, { wrap.stop(sink) }).to_glib()
}

unsafe extern "C" fn sink_render(
    ptr: *mut gst_base_ffi::GstBaseSink,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn {
    let sink: &RsSinkWrapper = &from_glib_borrow(ptr as *mut RsSink);
    let wrap = sink.get_wrap();
    let buffer = gst::BufferRef::from_ptr(buffer);

    panic_to_error!(
        wrap,
        sink,
        gst::FlowReturn::Error,
        { wrap.render(sink, buffer) }
    ).to_glib()
}

pub struct SinkInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(&RsSinkWrapper) -> Box<Sink>,
    pub protocols: Vec<String>,
}

glib_wrapper! {
    pub struct RsSinkWrapper(Object<RsSink>): [gst_base::BaseSink => gst_base_ffi::GstBaseSink,
                                               gst::Element => gst_ffi::GstElement,
                                               gst::Object => gst_ffi::GstObject,
                                               gst::URIHandler => gst_ffi::GstURIHandler,
                                              ];

    match fn {
        get_type => || rs_sink_get_type(),
    }
}
impl RsSinkWrapper {
    fn get_wrap(&self) -> &SinkWrapper {
        let stash = self.to_glib_none();
        let sink: *mut RsSink = stash.0;

        unsafe { &*((*sink).wrap) }
    }
}

#[repr(u32)]
enum Properties {
    PropURI = 1u32,
}

#[repr(C)]
pub struct RsSink {
    parent: gst_base_ffi::GstBaseSink,
    wrap: *mut SinkWrapper,
}

#[repr(C)]
pub struct RsSinkClass {
    parent_class: gst_base_ffi::GstBaseSinkClass,
    sink_info: *const SinkInfo,
    protocols: *const Vec<*const c_char>,
    parent_vtable: glib_ffi::gconstpointer,
}

unsafe fn rs_sink_get_type() -> glib_ffi::GType {
    use std::sync::{Once, ONCE_INIT};

    static mut TYPE: glib_ffi::GType = gobject_ffi::G_TYPE_INVALID;
    static ONCE: Once = ONCE_INIT;

    ONCE.call_once(|| {
        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<RsSinkClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(sink_class_init),
            class_finalize: None,
            class_data: ptr::null_mut(),
            instance_size: mem::size_of::<RsSink>() as u16,
            n_preallocs: 0,
            instance_init: Some(sink_init),
            value_table: ptr::null(),
        };

        let type_name = {
            let mut idx = 0;

            loop {
                let type_name = CString::new(format!("RsSink-{}", idx)).unwrap();
                if gobject_ffi::g_type_from_name(type_name.as_ptr()) == gobject_ffi::G_TYPE_INVALID
                {
                    break type_name;
                }
                idx += 1;
            }
        };

        TYPE = gobject_ffi::g_type_register_static(
            gst_base_ffi::gst_base_sink_get_type(),
            type_name.as_ptr(),
            &type_info,
            gobject_ffi::GTypeFlags::empty(),
        );

        let iface_info = gobject_ffi::GInterfaceInfo {
            interface_init: Some(sink_uri_handler_init),
            interface_finalize: None,
            interface_data: ptr::null_mut(),
        };
        gobject_ffi::g_type_add_interface_static(
            TYPE,
            gst_ffi::gst_uri_handler_get_type(),
            &iface_info,
        );
    });

    TYPE
}

unsafe extern "C" fn sink_finalize(obj: *mut gobject_ffi::GObject) {
    let sink = &mut *(obj as *mut RsSink);

    drop(Box::from_raw(sink.wrap));
    sink.wrap = ptr::null_mut();

    let sink_klass = &**(obj as *const *mut RsSinkClass);
    let parent_klass = &*(sink_klass.parent_vtable as *const gobject_ffi::GObjectClass);
    parent_klass.finalize.map(|f| f(obj));
}

unsafe extern "C" fn sink_set_property(
    obj: *mut gobject_ffi::GObject,
    id: u32,
    value: *mut gobject_ffi::GValue,
    _pspec: *mut gobject_ffi::GParamSpec,
) {
    let sink = obj as *mut RsSink;

    match mem::transmute(id) {
        Properties::PropURI => {
            let uri_ptr = gobject_ffi::g_value_get_string(value);
            sink_set_uri(sink, uri_ptr, ptr::null_mut());
        }
        _ => unreachable!(),
    }
}

unsafe extern "C" fn sink_get_property(
    obj: *mut gobject_ffi::GObject,
    id: u32,
    value: *mut gobject_ffi::GValue,
    _pspec: *mut gobject_ffi::GParamSpec,
) {
    let sink = obj as *mut RsSink;

    match mem::transmute(id) {
        Properties::PropURI => {
            let uri_ptr = sink_get_uri(sink);
            gobject_ffi::g_value_take_string(value, uri_ptr);
        }
        _ => unreachable!(),
    }
}

unsafe extern "C" fn sink_sub_class_init(
    klass: glib_ffi::gpointer,
    klass_data: glib_ffi::gpointer,
) {
    let sink_info = &*(klass_data as *const SinkInfo);

    {
        let element_klass = &mut *(klass as *mut gst_ffi::GstElementClass);

        gst_ffi::gst_element_class_set_metadata(
            element_klass,
            sink_info.long_name.to_glib_none().0,
            sink_info.classification.to_glib_none().0,
            sink_info.description.to_glib_none().0,
            sink_info.author.to_glib_none().0,
        );

        // TODO: Methods + sink_info.caps
        let caps = gst::Caps::new_any();
        let pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        );
        gst_ffi::gst_element_class_add_pad_template(element_klass, pad_template.to_glib_full());
    }

    {
        let sink_klass = &mut *(klass as *mut RsSinkClass);

        sink_klass.sink_info = sink_info;
        let mut protocols = Box::new(Vec::with_capacity(sink_info.protocols.len()));
        for p in &sink_info.protocols {
            let p_cstr = CString::new(p.clone().into_bytes()).unwrap();
            protocols.push(p_cstr.into_raw() as *const c_char);
        }
        protocols.push(ptr::null());
        sink_klass.protocols = Box::into_raw(protocols) as *const Vec<*const c_char>;
    }
}

unsafe extern "C" fn sink_class_init(klass: glib_ffi::gpointer, _klass_data: glib_ffi::gpointer) {
    {
        let gobject_klass = &mut *(klass as *mut gobject_ffi::GObjectClass);
        gobject_klass.set_property = Some(sink_set_property);
        gobject_klass.get_property = Some(sink_get_property);
        gobject_klass.finalize = Some(sink_finalize);

        gobject_ffi::g_object_class_install_property(
            klass as *mut gobject_ffi::GObjectClass,
            1,
            gobject_ffi::g_param_spec_string(
                "uri".to_glib_none().0,
                "URI".to_glib_none().0,
                "URI to read from".to_glib_none().0,
                ptr::null_mut(),
                gobject_ffi::G_PARAM_READWRITE,
            ),
        );
    }

    {
        let basesink_klass = &mut *(klass as *mut gst_base_ffi::GstBaseSinkClass);

        basesink_klass.start = Some(sink_start);
        basesink_klass.stop = Some(sink_stop);
        basesink_klass.render = Some(sink_render);
    }

    {
        let sink_klass = &mut *(klass as *mut RsSinkClass);

        sink_klass.parent_vtable = gobject_ffi::g_type_class_peek_parent(klass);
    }
}

unsafe extern "C" fn sink_init(
    instance: *mut gobject_ffi::GTypeInstance,
    klass: glib_ffi::gpointer,
) {
    let sink = &mut *(instance as *mut RsSink);
    let sink_klass = &*(klass as *const RsSinkClass);
    let sink_info = &*sink_klass.sink_info;

    let wrap = Box::new(SinkWrapper::new((sink_info.create_instance)(
        &RsSinkWrapper::from_glib_borrow(instance as *mut _),
    )));
    sink.wrap = Box::into_raw(wrap);

    let sink = &RsSinkWrapper::from_glib_borrow(sink as *mut _);
    sink.set_sync(false);
}

unsafe extern "C" fn sink_uri_handler_get_type(_type: glib_ffi::GType) -> gst_ffi::GstURIType {
    gst::URIType::Sink.to_glib()
}

unsafe extern "C" fn sink_uri_handler_get_protocols(
    type_: glib_ffi::GType,
) -> *const *const c_char {
    let klass = gobject_ffi::g_type_class_peek(type_);
    let sink_klass = &*(klass as *const RsSinkClass);
    (*sink_klass.protocols).as_ptr()
}

unsafe extern "C" fn sink_uri_handler_get_uri(
    uri_handler: *mut gst_ffi::GstURIHandler,
) -> *mut c_char {
    sink_get_uri(uri_handler as *mut RsSink)
}

unsafe extern "C" fn sink_uri_handler_set_uri(
    uri_handler: *mut gst_ffi::GstURIHandler,
    uri: *const c_char,
    err: *mut *mut glib_ffi::GError,
) -> glib_ffi::gboolean {
    sink_set_uri(uri_handler as *mut RsSink, uri, err)
}

unsafe extern "C" fn sink_uri_handler_init(
    iface: glib_ffi::gpointer,
    _iface_data: glib_ffi::gpointer,
) {
    let uri_handler_iface = &mut *(iface as *mut gst_ffi::GstURIHandlerInterface);

    uri_handler_iface.get_type = Some(sink_uri_handler_get_type);
    uri_handler_iface.get_protocols = Some(sink_uri_handler_get_protocols);
    uri_handler_iface.get_uri = Some(sink_uri_handler_get_uri);
    uri_handler_iface.set_uri = Some(sink_uri_handler_set_uri);
}

pub fn sink_register(plugin: &gst::Plugin, sink_info: SinkInfo) {
    unsafe {
        let parent_type = rs_sink_get_type();
        let type_name = format!("RsSink-{}", sink_info.name);

        let name = sink_info.name.clone();
        let rank = sink_info.rank;

        let sink_info = Box::new(sink_info);
        let sink_info_ptr = Box::into_raw(sink_info) as glib_ffi::gpointer;

        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<RsSinkClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(sink_sub_class_init),
            class_finalize: None,
            class_data: sink_info_ptr,
            instance_size: mem::size_of::<RsSink>() as u16,
            n_preallocs: 0,
            instance_init: None,
            value_table: ptr::null(),
        };

        let type_ = gobject_ffi::g_type_register_static(
            parent_type,
            type_name.to_glib_none().0,
            &type_info,
            gobject_ffi::GTypeFlags::empty(),
        );

        gst::Element::register(plugin, &name, rank, from_glib(type_));
    }
}
