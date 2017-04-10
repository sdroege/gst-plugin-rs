// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::ffi::{CStr, CString};
use std::ptr;
use std::mem;
use std::u64;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use slog::*;

use plugin::Plugin;
use utils::*;
use error::*;
use buffer::*;
use miniobject::*;
use log::*;
use caps::*;

use glib;
use gobject;
use gst;
use gst_base;

#[derive(Debug)]
pub enum SourceError {
    Failure,
    OpenFailed,
    NotFound,
    ReadFailed,
    SeekFailed,
}

impl ToGError for SourceError {
    fn to_gerror(&self) -> (u32, i32) {
        match *self {
            SourceError::Failure => (gst_library_error_domain(), 1),
            SourceError::OpenFailed => (gst_resource_error_domain(), 5),
            SourceError::NotFound => (gst_resource_error_domain(), 3),
            SourceError::ReadFailed => (gst_resource_error_domain(), 9),
            SourceError::SeekFailed => (gst_resource_error_domain(), 11),
        }
    }
}

pub struct SourceWrapper {
    raw: *mut gst::GstElement,
    logger: Logger,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    source: Mutex<Box<Source>>,
    panicked: AtomicBool,
}

pub trait Source {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn is_seekable(&self) -> bool;
    fn get_size(&self) -> Option<u64>;

    fn start(&mut self, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self) -> Result<(), ErrorMessage>;
    fn fill(&mut self, offset: u64, length: u32, buffer: &mut Buffer) -> Result<(), FlowError>;
    fn seek(&mut self, start: u64, stop: Option<u64>) -> Result<(), ErrorMessage>;
}

impl SourceWrapper {
    fn new(raw: *mut gst::GstElement, source: Box<Source>) -> SourceWrapper {
        SourceWrapper {
            raw: raw,
            logger: Logger::root(GstDebugDrain::new(Some(unsafe { &Element::new(raw) }),
                                                    "rssrc",
                                                    0,
                                                    "Rust source base class"),
                                 None),
            uri: Mutex::new((None, false)),
            uri_validator: source.uri_validator(),
            source: Mutex::new(source),
            panicked: AtomicBool::new(false),
        }
    }

    fn set_uri(&self, uri_str: Option<&str>) -> Result<(), UriError> {
        let uri_storage = &mut self.uri.lock().unwrap();

        debug!(self.logger, "Setting URI {:?}", uri_str);

        if uri_storage.1 {
            return Err(UriError::new(UriErrorKind::BadState, Some("Already started".to_string())));
        }

        uri_storage.0 = None;

        if let Some(uri_str) = uri_str {
            match Url::parse(uri_str) {
                Ok(uri) => {
                    try!((self.uri_validator)(&uri));
                    uri_storage.0 = Some(uri);
                    Ok(())
                }
                Err(err) => {
                    Err(UriError::new(UriErrorKind::BadUri,
                                      Some(format!("Failed to parse URI '{}': {}", uri_str, err))))
                }
            }
        } else {
            Ok(())
        }
    }

    fn get_uri(&self) -> Option<String> {
        let uri_storage = &self.uri.lock().unwrap();
        uri_storage.0.as_ref().map(|uri| String::from(uri.as_str()))
    }

    fn is_seekable(&self) -> bool {
        let source = &self.source.lock().unwrap();
        source.is_seekable()
    }

    fn get_size(&self) -> u64 {
        let source = &self.source.lock().unwrap();
        source.get_size().unwrap_or(u64::MAX)
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
                self.post_message(&error_msg!(SourceError::OpenFailed, ["No URI given"]));
                return false;
            }
        };

        let source = &mut self.source.lock().unwrap();
        match source.start(uri) {
            Ok(..) => {
                trace!(self.logger, "Started successfully");
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to start: {:?}", msg);

                self.uri
                    .lock()
                    .unwrap()
                    .1 = false;
                self.post_message(msg);
                false
            }
        }
    }

    fn stop(&self) -> bool {
        let source = &mut self.source.lock().unwrap();

        debug!(self.logger, "Stopping");

        match source.stop() {
            Ok(..) => {
                trace!(self.logger, "Stopped successfully");
                self.uri
                    .lock()
                    .unwrap()
                    .1 = false;
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to stop: {:?}", msg);

                self.post_message(msg);
                false
            }
        }
    }

    fn fill(&self, offset: u64, length: u32, buffer: &mut Buffer) -> gst::GstFlowReturn {
        let source = &mut self.source.lock().unwrap();

        trace!(self.logger,
               "Filling buffer {:?} with offset {} and length {}",
               buffer,
               offset,
               length);

        match source.fill(offset, length, buffer) {
            Ok(()) => gst::GST_FLOW_OK,
            Err(flow_error) => {
                error!(self.logger, "Failed to fill: {:?}", flow_error);
                match flow_error {
                    FlowError::NotNegotiated(ref msg) |
                    FlowError::Error(ref msg) => self.post_message(msg),
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    }

    fn seek(&self, start: u64, stop: Option<u64>) -> bool {
        let source = &mut self.source.lock().unwrap();

        debug!(self.logger, "Seeking to {:?}-{:?}", start, stop);

        match source.seek(start, stop) {
            Ok(..) => true,
            Err(ref msg) => {
                error!(self.logger, "Failed to seek {:?}", msg);
                self.post_message(msg);
                false
            }
        }
    }

    fn post_message(&self, msg: &ErrorMessage) {
        unsafe {
            msg.post(self.raw);
        }
    }
}

unsafe fn source_set_uri(ptr: *const RsSrc,
                         uri_ptr: *const c_char,
                         cerr: *mut *mut glib::GError)
                         -> glib::gboolean {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;

    panic_to_error!(wrap, glib::GFALSE, {
        let uri_str = if uri_ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(uri_ptr).to_str().unwrap())
        };

        match wrap.set_uri(uri_str) {
            Err(err) => {
                error!(wrap.logger, "Failed to set URI {:?}", err);
                err.into_gerror(cerr);
                glib::GFALSE
            }
            Ok(_) => glib::GTRUE,
        }
    })
}

unsafe fn source_get_uri(ptr: *const RsSrc) -> *mut c_char {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;

    panic_to_error!(wrap, ptr::null_mut(), {
        match wrap.get_uri() {
            Some(uri_str) => {
                let uri_cstr = CString::new(uri_str).unwrap();
                glib::g_strdup(uri_cstr.as_ptr())
            }
            None => ptr::null_mut(),
        }
    })
}

unsafe extern "C" fn source_is_seekable(ptr: *mut gst_base::GstBaseSrc) -> glib::gboolean {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;

    panic_to_error!(wrap, glib::GFALSE, {
        if wrap.is_seekable() {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

unsafe extern "C" fn source_get_size(ptr: *mut gst_base::GstBaseSrc,
                                     size: *mut u64)
                                     -> glib::gboolean {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;

    panic_to_error!(wrap, glib::GFALSE, {
        *size = wrap.get_size();
        glib::GTRUE
    })
}

unsafe extern "C" fn source_start(ptr: *mut gst_base::GstBaseSrc) -> glib::gboolean {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;

    panic_to_error!(wrap, glib::GFALSE, {
        if wrap.start() {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

unsafe extern "C" fn source_stop(ptr: *mut gst_base::GstBaseSrc) -> glib::gboolean {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;

    panic_to_error!(wrap, glib::GTRUE, {
        if wrap.stop() {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

unsafe extern "C" fn source_fill(ptr: *mut gst_base::GstBaseSrc,
                                 offset: u64,
                                 length: u32,
                                 buffer: *mut gst::GstBuffer)
                                 -> gst::GstFlowReturn {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;
    let buffer = GstRefPtr(buffer);

    panic_to_error!(wrap, gst::GST_FLOW_ERROR, {
        let mut buffer: GstRef<Buffer> = GstRef::new(&buffer);
        wrap.fill(offset, length, buffer.get_mut().unwrap())
    })
}

unsafe extern "C" fn source_seek(ptr: *mut gst_base::GstBaseSrc,
                                 segment: *mut gst::GstSegment)
                                 -> glib::gboolean {
    let src = &*(ptr as *const RsSrc);
    let wrap: &SourceWrapper = &*src.wrap;

    let start = (*segment).start;
    let stop = (*segment).stop;

    panic_to_error!(wrap, glib::GFALSE, {
        if wrap.seek(start, if stop == u64::MAX { None } else { Some(stop) }) {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

pub struct SourceInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(Element) -> Box<Source>,
    pub protocols: Vec<String>,
    pub push_only: bool,
}

#[repr(C)]
struct RsSrc {
    parent: gst_base::GstPushSrc,
    wrap: *mut SourceWrapper,
    source_info: *const SourceInfo,
}

#[repr(C)]
struct RsSrcClass {
    parent_class: gst_base::GstPushSrcClass,
    source_info: *const SourceInfo,
    protocols: *const Vec<*const c_char>,
    parent_vtable: glib::gconstpointer,
}

unsafe extern "C" fn source_finalize(obj: *mut gobject::GObject) {
    let src = &mut *(obj as *mut RsSrc);

    drop(Box::from_raw(src.wrap));

    let src_klass = &**(obj as *const *const RsSrcClass);
    let parent_klass = &*(src_klass.parent_vtable as *const gobject::GObjectClass);
    parent_klass.finalize.map(|f| f(obj));
}

unsafe extern "C" fn source_set_property(obj: *mut gobject::GObject,
                                         id: u32,
                                         value: *mut gobject::GValue,
                                         _pspec: *mut gobject::GParamSpec) {
    let src = &*(obj as *const RsSrc);

    match id {
        1 => {
            let uri_ptr = gobject::g_value_get_string(value);
            source_set_uri(src, uri_ptr, ptr::null_mut());
        }
        _ => unreachable!(),
    }
}

unsafe extern "C" fn source_get_property(obj: *mut gobject::GObject,
                                         id: u32,
                                         value: *mut gobject::GValue,
                                         _pspec: *mut gobject::GParamSpec) {
    let src = &*(obj as *const RsSrc);

    match id {
        1 => {
            let uri_ptr = source_get_uri(src);
            gobject::g_value_take_string(value, uri_ptr);
        }
        _ => unreachable!(),
    }
}

unsafe extern "C" fn source_class_init(klass: glib::gpointer, klass_data: glib::gpointer) {
    let src_klass = &mut *(klass as *mut RsSrcClass);
    let source_info = &*(klass_data as *const SourceInfo);

    {
        let gobject_klass = &mut src_klass.parent_class
                                     .parent_class
                                     .parent_class
                                     .parent_class
                                     .parent_class;
        gobject_klass.set_property = Some(source_set_property);
        gobject_klass.get_property = Some(source_get_property);
        gobject_klass.finalize = Some(source_finalize);

        let name_cstr = CString::new("uri").unwrap();
        let nick_cstr = CString::new("URI").unwrap();
        let blurb_cstr = CString::new("URI to read from").unwrap();

        gobject::g_object_class_install_property(klass as *mut gobject::GObjectClass, 1,
            gobject::g_param_spec_string(name_cstr.as_ptr(),
                                         nick_cstr.as_ptr(),
                                         blurb_cstr.as_ptr(),
                                         ptr::null_mut(),
                                         gobject::G_PARAM_READWRITE));
    }

    {
        let element_klass = &mut src_klass.parent_class.parent_class.parent_class;

        let longname_cstr = CString::new(source_info.long_name.clone()).unwrap();
        let classification_cstr = CString::new(source_info.description.clone()).unwrap();
        let description_cstr = CString::new(source_info.classification.clone()).unwrap();
        let author_cstr = CString::new(source_info.author.clone()).unwrap();

        gst::gst_element_class_set_static_metadata(element_klass,
                                                   longname_cstr.into_raw(),
                                                   classification_cstr.into_raw(),
                                                   description_cstr.into_raw(),
                                                   author_cstr.into_raw());

        let caps = Caps::new_any();
        let templ_name = CString::new("src").unwrap();
        let pad_template = gst::gst_pad_template_new(templ_name.into_raw(),
                                                     gst::GST_PAD_SRC,
                                                     gst::GST_PAD_ALWAYS,
                                                     caps.as_ptr());
        gst::gst_element_class_add_pad_template(element_klass, pad_template);
    }

    {
        let basesrc_klass = &mut src_klass.parent_class.parent_class;
        basesrc_klass.start = Some(source_start);
        basesrc_klass.stop = Some(source_stop);
        basesrc_klass.is_seekable = Some(source_is_seekable);
        basesrc_klass.get_size = Some(source_get_size);
        basesrc_klass.fill = Some(source_fill);
        basesrc_klass.do_seek = Some(source_seek);
    }

    src_klass.source_info = source_info;
    let mut protocols = Box::new(Vec::with_capacity(source_info.protocols.len()));
    for p in &source_info.protocols {
        let p_cstr = CString::new(p.clone().into_bytes()).unwrap();
        protocols.push(p_cstr.into_raw() as *const c_char);
    }
    protocols.push(ptr::null());
    src_klass.protocols = Box::into_raw(protocols) as *const Vec<*const c_char>;
    src_klass.parent_vtable = gobject::g_type_class_peek_parent(klass);
}

unsafe extern "C" fn source_init(instance: *mut gobject::GTypeInstance, klass: glib::gpointer) {
    let src = &mut *(instance as *mut RsSrc);
    let src_klass = &*(klass as *const RsSrcClass);
    let source_info = &*src_klass.source_info;

    src.source_info = source_info;

    let wrap = Box::new(SourceWrapper::new(&mut src.parent.parent.element,
            (source_info.create_instance)(Element::new(&mut src.parent.parent.element))));
    src.wrap = Box::into_raw(wrap);

    gst_base::gst_base_src_set_blocksize(&mut src.parent.parent, 4096);
}

unsafe extern "C" fn source_uri_handler_get_type(_type: glib::GType) -> gst::GstURIType {
    gst::GST_URI_SRC
}

unsafe extern "C" fn source_uri_handler_get_protocols(type_: glib::GType) -> *const *const c_char {
    let klass = gobject::g_type_class_peek(type_);
    let src_klass = &*(klass as *const RsSrcClass);
    (*src_klass.protocols).as_ptr()
}

unsafe extern "C" fn source_uri_handler_get_uri(uri_handler: *mut gst::GstURIHandler)
                                                -> *mut c_char {
    source_get_uri(uri_handler as *const RsSrc)
}

unsafe extern "C" fn source_uri_handler_set_uri(uri_handler: *mut gst::GstURIHandler,
                                                uri: *const c_char,
                                                err: *mut *mut glib::GError)
                                                -> glib::gboolean {
    source_set_uri(uri_handler as *const RsSrc, uri, err)
}

unsafe extern "C" fn source_uri_handler_init(iface: glib::gpointer, _iface_data: glib::gpointer) {
    let uri_handler_iface = &mut *(iface as *mut gst::GstURIHandlerInterface);

    uri_handler_iface.get_type = Some(source_uri_handler_get_type);
    uri_handler_iface.get_protocols = Some(source_uri_handler_get_protocols);
    uri_handler_iface.get_uri = Some(source_uri_handler_get_uri);
    uri_handler_iface.set_uri = Some(source_uri_handler_set_uri);
}

pub fn source_register(plugin: &Plugin, source_info: SourceInfo) {
    unsafe {
        let parent_type = if source_info.push_only {
            gst_base::gst_push_src_get_type()
        } else {
            gst_base::gst_base_src_get_type()
        };
        let mut type_name = String::from("RsSrc-");
        type_name.push_str(&source_info.name);
        let type_name_cstr = CString::new(type_name.into_bytes()).unwrap();

        let name_cstr = CString::new(source_info.name.clone().into_bytes()).unwrap();
        let rank = source_info.rank;

        let source_info = Box::new(source_info);
        let source_info_ptr = Box::into_raw(source_info) as glib::gpointer;

        let type_info = gobject::GTypeInfo {
            class_size: mem::size_of::<RsSrcClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(source_class_init),
            class_finalize: None,
            class_data: source_info_ptr,
            instance_size: mem::size_of::<RsSrc>() as u16,
            n_preallocs: 0,
            instance_init: Some(source_init),
            value_table: ptr::null(),
        };

        let type_ = gobject::g_type_register_static(parent_type,
                                                    type_name_cstr.as_ptr(),
                                                    &type_info,
                                                    gobject::GTypeFlags::empty());

        let iface_info = gobject::GInterfaceInfo {
            interface_init: Some(source_uri_handler_init),
            interface_finalize: None,
            interface_data: ptr::null_mut(),
        };
        gobject::g_type_add_interface_static(type_, gst::gst_uri_handler_get_type(), &iface_info);

        gst::gst_element_register(plugin.as_ptr(), name_cstr.as_ptr(), rank, type_);
    }
}
