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

use slog::Logger;

use utils::*;
use error::*;
use buffer::*;
use miniobject::*;
use log::*;
use plugin::Plugin;
use caps::*;

use glib_ffi;
use gobject_ffi;
use gst_ffi;
use gst_base_ffi;

#[derive(Debug)]
pub enum SinkError {
    Failure,
    OpenFailed,
    NotFound,
    WriteFailed,
    SeekFailed,
}

impl ToGError for SinkError {
    fn to_gerror(&self) -> (u32, i32) {
        match *self {
            SinkError::Failure => (gst_library_error_domain(), 1),
            SinkError::OpenFailed => (gst_resource_error_domain(), 6),
            SinkError::NotFound => (gst_resource_error_domain(), 3),
            SinkError::WriteFailed => (gst_resource_error_domain(), 10),
            SinkError::SeekFailed => (gst_resource_error_domain(), 11),
        }
    }
}

pub struct SinkWrapper {
    raw: *mut gst_ffi::GstElement,
    logger: Logger,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    sink: Mutex<Box<Sink>>,
    panicked: AtomicBool,
}

pub trait Sink {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn start(&mut self, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self) -> Result<(), ErrorMessage>;

    fn render(&mut self, buffer: &Buffer) -> Result<(), FlowError>;
}

impl SinkWrapper {
    fn new(raw: *mut gst_ffi::GstElement, sink: Box<Sink>) -> SinkWrapper {
        SinkWrapper {
            raw: raw,
            logger: Logger::root(
                GstDebugDrain::new(
                    Some(unsafe { &Element::new(raw) }),
                    "rssink",
                    0,
                    "Rust sink base class",
                ),
                o!(),
            ),
            uri: Mutex::new((None, false)),
            uri_validator: sink.uri_validator(),
            sink: Mutex::new(sink),
            panicked: AtomicBool::new(false),
        }
    }

    fn set_uri(&self, uri_str: Option<&str>) -> Result<(), UriError> {
        let uri_storage = &mut self.uri.lock().unwrap();

        debug!(self.logger, "Setting URI {:?}", uri_str);

        if uri_storage.1 {
            return Err(UriError::new(
                UriErrorKind::BadState,
                Some("Already started".to_string()),
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
                    UriErrorKind::BadUri,
                    Some(format!("Failed to parse URI '{}': {}", uri_str, err)),
                )),
            }
        } else {
            Ok(())
        }
    }

    fn get_uri(&self) -> Option<String> {
        let uri_storage = &self.uri.lock().unwrap();
        uri_storage.0.as_ref().map(|uri| String::from(uri.as_str()))
    }

    fn start(&self) -> bool {
        debug!(self.logger, "Starting");

        // Don't keep the URI locked while we call start later
        let uri = match *self.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;
                uri.clone()
            }
            (None, _) => {
                error!(self.logger, "No URI given");
                self.post_message(&error_msg!(SinkError::OpenFailed, ["No URI given"]));
                return false;
            }
        };

        let sink = &mut self.sink.lock().unwrap();
        match sink.start(uri) {
            Ok(..) => {
                trace!(self.logger, "Started successfully");
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to start: {:?}", msg);

                self.uri.lock().unwrap().1 = false;
                self.post_message(msg);
                false
            }
        }
    }

    fn stop(&self) -> bool {
        let sink = &mut self.sink.lock().unwrap();

        debug!(self.logger, "Stopping");

        match sink.stop() {
            Ok(..) => {
                trace!(self.logger, "Stopped successfully");
                self.uri.lock().unwrap().1 = false;
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to stop: {:?}", msg);

                self.post_message(msg);
                false
            }
        }
    }

    fn render(&self, buffer: &Buffer) -> gst_ffi::GstFlowReturn {
        let sink = &mut self.sink.lock().unwrap();

        trace!(self.logger, "Rendering buffer {:?}", buffer);

        match sink.render(buffer) {
            Ok(..) => gst_ffi::GST_FLOW_OK,
            Err(flow_error) => {
                error!(self.logger, "Failed to render: {:?}", flow_error);
                match flow_error {
                    FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                        self.post_message(msg)
                    }
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    }

    fn post_message(&self, msg: &ErrorMessage) {
        unsafe {
            msg.post(self.raw);
        }
    }
}

unsafe fn sink_set_uri(
    ptr: *const RsSink,
    uri_ptr: *const c_char,
    cerr: *mut *mut glib_ffi::GError,
) -> glib_ffi::gboolean {
    let sink = &*(ptr as *const RsSink);
    let wrap: &SinkWrapper = &*sink.wrap;

    panic_to_error!(wrap, glib_ffi::GFALSE, {
        let uri_str = if uri_ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(uri_ptr).to_str().unwrap())
        };

        match wrap.set_uri(uri_str) {
            Err(err) => {
                error!(wrap.logger, "Failed to set URI {:?}", err);
                err.into_gerror(cerr);
                glib_ffi::GFALSE
            }
            Ok(_) => glib_ffi::GTRUE,
        }
    })
}

unsafe fn sink_get_uri(ptr: *const RsSink) -> *mut c_char {
    let sink = &*(ptr as *const RsSink);
    let wrap: &SinkWrapper = &*sink.wrap;

    panic_to_error!(wrap, ptr::null_mut(), {
        match wrap.get_uri() {
            Some(uri_str) => glib_ffi::g_strndup(uri_str.as_ptr() as *const c_char, uri_str.len()),
            None => ptr::null_mut(),
        }
    })
}

unsafe extern "C" fn sink_start(ptr: *mut gst_base_ffi::GstBaseSink) -> glib_ffi::gboolean {
    let sink = &*(ptr as *const RsSink);
    let wrap: &SinkWrapper = &*sink.wrap;

    panic_to_error!(wrap, glib_ffi::GFALSE, {
        if wrap.start() {
            glib_ffi::GTRUE
        } else {
            glib_ffi::GFALSE
        }
    })
}

unsafe extern "C" fn sink_stop(ptr: *mut gst_base_ffi::GstBaseSink) -> glib_ffi::gboolean {
    let sink = &*(ptr as *const RsSink);
    let wrap: &SinkWrapper = &*sink.wrap;

    panic_to_error!(wrap, glib_ffi::GTRUE, {
        if wrap.stop() {
            glib_ffi::GTRUE
        } else {
            glib_ffi::GFALSE
        }
    })
}

unsafe extern "C" fn sink_render(
    ptr: *mut gst_base_ffi::GstBaseSink,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn {
    let sink = &*(ptr as *const RsSink);
    let wrap: &SinkWrapper = &*sink.wrap;
    let buffer: &Buffer = Buffer::from_ptr(buffer);

    panic_to_error!(wrap, gst_ffi::GST_FLOW_ERROR, { wrap.render(buffer) })
}

pub struct SinkInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(Element) -> Box<Sink>,
    pub protocols: Vec<String>,
}

#[repr(C)]
struct RsSink {
    parent: gst_base_ffi::GstBaseSink,
    wrap: *mut SinkWrapper,
    sink_info: *const SinkInfo,
}

#[repr(C)]
struct RsSinkClass {
    parent_class: gst_base_ffi::GstBaseSinkClass,
    sink_info: *const SinkInfo,
    protocols: *const Vec<*const c_char>,
    parent_vtable: glib_ffi::gconstpointer,
}

unsafe extern "C" fn sink_finalize(obj: *mut gobject_ffi::GObject) {
    let sink = &mut *(obj as *mut RsSink);

    drop(Box::from_raw(sink.wrap));

    let sink_klass = &**(obj as *const *const RsSinkClass);
    let parent_klass = &*(sink_klass.parent_vtable as *const gobject_ffi::GObjectClass);
    parent_klass.finalize.map(|f| f(obj));
}

unsafe extern "C" fn sink_set_property(
    obj: *mut gobject_ffi::GObject,
    id: u32,
    value: *mut gobject_ffi::GValue,
    _pspec: *mut gobject_ffi::GParamSpec,
) {
    let sink = &*(obj as *const RsSink);

    match id {
        1 => {
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
    let sink = &*(obj as *const RsSink);

    match id {
        1 => {
            let uri_ptr = sink_get_uri(sink);
            gobject_ffi::g_value_take_string(value, uri_ptr);
        }
        _ => unreachable!(),
    }
}

unsafe extern "C" fn sink_class_init(klass: glib_ffi::gpointer, klass_data: glib_ffi::gpointer) {
    let sink_klass = &mut *(klass as *mut RsSinkClass);
    let sink_info = &*(klass_data as *const SinkInfo);

    {
        let gobject_klass = &mut sink_klass
            .parent_class
            .parent_class
            .parent_class
            .parent_class;
        gobject_klass.set_property = Some(sink_set_property);
        gobject_klass.get_property = Some(sink_get_property);
        gobject_klass.finalize = Some(sink_finalize);

        let name_cstr = CString::new("uri").unwrap();
        let nick_cstr = CString::new("URI").unwrap();
        let blurb_cstr = CString::new("URI to read from").unwrap();

        gobject_ffi::g_object_class_install_property(
            klass as *mut gobject_ffi::GObjectClass,
            1,
            gobject_ffi::g_param_spec_string(
                name_cstr.as_ptr(),
                nick_cstr.as_ptr(),
                blurb_cstr.as_ptr(),
                ptr::null_mut(),
                gobject_ffi::G_PARAM_READWRITE,
            ),
        );
    }

    {
        let element_klass = &mut sink_klass.parent_class.parent_class;

        let longname_cstr = CString::new(sink_info.long_name.clone()).unwrap();
        let classification_cstr = CString::new(sink_info.description.clone()).unwrap();
        let description_cstr = CString::new(sink_info.classification.clone()).unwrap();
        let author_cstr = CString::new(sink_info.author.clone()).unwrap();

        gst_ffi::gst_element_class_set_static_metadata(
            element_klass,
            longname_cstr.into_raw(),
            classification_cstr.into_raw(),
            description_cstr.into_raw(),
            author_cstr.into_raw(),
        );

        let caps = Caps::new_any();
        let templ_name = CString::new("sink").unwrap();
        let pad_template = gst_ffi::gst_pad_template_new(
            templ_name.into_raw(),
            gst_ffi::GST_PAD_SINK,
            gst_ffi::GST_PAD_ALWAYS,
            caps.as_ptr() as *mut gst_ffi::GstCaps,
        );
        gst_ffi::gst_element_class_add_pad_template(element_klass, pad_template);
    }

    {
        let basesink_klass = &mut sink_klass.parent_class;
        basesink_klass.start = Some(sink_start);
        basesink_klass.stop = Some(sink_stop);
        basesink_klass.render = Some(sink_render);
    }

    sink_klass.sink_info = sink_info;
    let mut protocols = Box::new(Vec::with_capacity(sink_info.protocols.len()));
    for p in &sink_info.protocols {
        let p_cstr = CString::new(p.clone().into_bytes()).unwrap();
        protocols.push(p_cstr.into_raw() as *const c_char);
    }
    protocols.push(ptr::null());
    sink_klass.protocols = Box::into_raw(protocols) as *const Vec<*const c_char>;
    sink_klass.parent_vtable = gobject_ffi::g_type_class_peek_parent(klass);
}

unsafe extern "C" fn sink_init(
    instance: *mut gobject_ffi::GTypeInstance,
    klass: glib_ffi::gpointer,
) {
    let sink = &mut *(instance as *mut RsSink);
    let sink_klass = &*(klass as *const RsSinkClass);
    let sink_info = &*sink_klass.sink_info;

    sink.sink_info = sink_info;

    let wrap = Box::new(SinkWrapper::new(
        &mut sink.parent.element,
        (sink_info.create_instance)(Element::new(&mut sink.parent.element)),
    ));
    sink.wrap = Box::into_raw(wrap);

    gst_base_ffi::gst_base_sink_set_sync(&mut sink.parent, glib_ffi::GFALSE);
}

unsafe extern "C" fn sink_uri_handler_get_type(_type: glib_ffi::GType) -> gst_ffi::GstURIType {
    gst_ffi::GST_URI_SINK
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
    sink_get_uri(uri_handler as *const RsSink)
}

unsafe extern "C" fn sink_uri_handler_set_uri(
    uri_handler: *mut gst_ffi::GstURIHandler,
    uri: *const c_char,
    err: *mut *mut glib_ffi::GError,
) -> glib_ffi::gboolean {
    sink_set_uri(uri_handler as *const RsSink, uri, err)
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

pub fn sink_register(plugin: &Plugin, sink_info: SinkInfo) {
    unsafe {
        let parent_type = gst_base_ffi::gst_base_sink_get_type();
        let mut type_name = String::from("RsSink-");
        type_name.push_str(&sink_info.name);
        let type_name_cstr = CString::new(type_name.into_bytes()).unwrap();

        let name_cstr = CString::new(sink_info.name.clone().into_bytes()).unwrap();
        let rank = sink_info.rank;

        let sink_info = Box::new(sink_info);
        let sink_info_ptr = Box::into_raw(sink_info) as glib_ffi::gpointer;

        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<RsSinkClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(sink_class_init),
            class_finalize: None,
            class_data: sink_info_ptr,
            instance_size: mem::size_of::<RsSink>() as u16,
            n_preallocs: 0,
            instance_init: Some(sink_init),
            value_table: ptr::null(),
        };

        let type_ = gobject_ffi::g_type_register_static(
            parent_type,
            type_name_cstr.as_ptr(),
            &type_info,
            gobject_ffi::GTypeFlags::empty(),
        );

        let iface_info = gobject_ffi::GInterfaceInfo {
            interface_init: Some(sink_uri_handler_init),
            interface_finalize: None,
            interface_data: ptr::null_mut(),
        };
        gobject_ffi::g_type_add_interface_static(
            type_,
            gst_ffi::gst_uri_handler_get_type(),
            &iface_info,
        );

        gst_ffi::gst_element_register(plugin.as_ptr(), name_cstr.as_ptr(), rank, type_);
    }
}
