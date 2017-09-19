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

pub struct SourceWrapper {
    cat: gst::DebugCategory,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    source: Mutex<Box<Source>>,
    panicked: AtomicBool,
}

pub trait Source: Send + 'static {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn is_seekable(&self, src: &RsSrcWrapper) -> bool;
    fn get_size(&self, src: &RsSrcWrapper) -> Option<u64>;

    fn start(&mut self, src: &RsSrcWrapper, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self, src: &RsSrcWrapper) -> Result<(), ErrorMessage>;
    fn fill(
        &mut self,
        src: &RsSrcWrapper,
        offset: u64,
        length: u32,
        buffer: &mut gst::BufferRef,
    ) -> Result<(), FlowError>;
    fn seek(
        &mut self,
        src: &RsSrcWrapper,
        start: u64,
        stop: Option<u64>,
    ) -> Result<(), ErrorMessage>;
}

impl SourceWrapper {
    fn new(source: Box<Source>) -> SourceWrapper {
        SourceWrapper {
            cat: gst::DebugCategory::new(
                "rssrc",
                gst::DebugColorFlags::empty(),
                "Rust source base class",
            ),
            uri: Mutex::new((None, false)),
            uri_validator: source.uri_validator(),
            source: Mutex::new(source),
            panicked: AtomicBool::new(false),
        }
    }

    fn set_uri(&self, src: &RsSrcWrapper, uri_str: Option<&str>) -> Result<(), UriError> {
        let uri_storage = &mut self.uri.lock().unwrap();

        gst_debug!(self.cat, obj: src, "Setting URI {:?}", uri_str);

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

    fn get_uri(&self, _src: &RsSrcWrapper) -> Option<String> {
        let uri_storage = &self.uri.lock().unwrap();
        uri_storage.0.as_ref().map(|uri| String::from(uri.as_str()))
    }

    fn is_seekable(&self, src: &RsSrcWrapper) -> bool {
        let source_impl = &self.source.lock().unwrap();
        source_impl.is_seekable(src)
    }

    fn get_size(&self, src: &RsSrcWrapper) -> u64 {
        let source_impl = &self.source.lock().unwrap();
        source_impl.get_size(src).unwrap_or(u64::MAX)
    }

    fn start(&self, src: &RsSrcWrapper) -> bool {
        gst_debug!(self.cat, obj: src, "Starting");

        // Don't keep the URI locked while we call start later
        let uri = match *self.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;
                uri.clone()
            }
            (None, _) => {
                gst_error!(self.cat, obj: src, "No URI given");
                self.post_message(
                    src,
                    &error_msg!(gst::ResourceError::OpenRead, ["No URI given"]),
                );
                return false;
            }
        };

        let source_impl = &mut self.source.lock().unwrap();
        match source_impl.start(src, uri) {
            Ok(..) => {
                gst_trace!(self.cat, obj: src, "Started successfully");
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: src, "Failed to start: {:?}", msg);

                self.uri.lock().unwrap().1 = false;
                self.post_message(src, msg);
                false
            }
        }
    }

    fn stop(&self, src: &RsSrcWrapper) -> bool {
        let source_impl = &mut self.source.lock().unwrap();

        gst_debug!(self.cat, obj: src, "Stopping");

        match source_impl.stop(src) {
            Ok(..) => {
                gst_trace!(self.cat, obj: src, "Stopped successfully");
                self.uri.lock().unwrap().1 = false;
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: src, "Failed to stop: {:?}", msg);

                self.post_message(src, msg);
                false
            }
        }
    }

    fn fill(
        &self,
        src: &RsSrcWrapper,
        offset: u64,
        length: u32,
        buffer: &mut gst::BufferRef,
    ) -> gst::FlowReturn {
        let source_impl = &mut self.source.lock().unwrap();

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
                        self.post_message(src, msg)
                    }
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    }

    fn seek(&self, src: &RsSrcWrapper, start: u64, stop: Option<u64>) -> bool {
        let source_impl = &mut self.source.lock().unwrap();

        gst_debug!(self.cat, obj: src, "Seeking to {:?}-{:?}", start, stop);

        match source_impl.seek(src, start, stop) {
            Ok(..) => true,
            Err(ref msg) => {
                gst_error!(self.cat, obj: src, "Failed to seek {:?}", msg);
                self.post_message(src, msg);
                false
            }
        }
    }

    fn post_message(&self, src: &RsSrcWrapper, msg: &ErrorMessage) {
        msg.post(src);
    }
}

unsafe fn source_set_uri(
    ptr: *mut RsSrc,
    uri_ptr: *const c_char,
    cerr: *mut *mut glib_ffi::GError,
) -> glib_ffi::gboolean {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    panic_to_error!(wrap, src, false, {
        let uri_str = if uri_ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(uri_ptr).to_str().unwrap())
        };

        match wrap.set_uri(src, uri_str) {
            Err(err) => {
                gst_error!(wrap.cat, obj: src, "Failed to set URI {:?}", err);
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

unsafe fn source_get_uri(ptr: *mut RsSrc) -> *mut c_char {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    panic_to_error!(wrap, src, None, { wrap.get_uri(src) }).to_glib_full()
}

unsafe extern "C" fn source_is_seekable(ptr: *mut gst_base_ffi::GstBaseSrc) -> glib_ffi::gboolean {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    panic_to_error!(wrap, src, false, { wrap.is_seekable(src) }).to_glib()
}

unsafe extern "C" fn source_get_size(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    size: *mut u64,
) -> glib_ffi::gboolean {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    panic_to_error!(wrap, src, false, {
        *size = wrap.get_size(src);
        true
    }).to_glib()
}

unsafe extern "C" fn source_start(ptr: *mut gst_base_ffi::GstBaseSrc) -> glib_ffi::gboolean {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    panic_to_error!(wrap, src, false, { wrap.start(src) }).to_glib()
}

unsafe extern "C" fn source_stop(ptr: *mut gst_base_ffi::GstBaseSrc) -> glib_ffi::gboolean {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    panic_to_error!(wrap, src, false, { wrap.stop(src) }).to_glib()
}

unsafe extern "C" fn source_fill(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    offset: u64,
    length: u32,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();
    let buffer = gst::BufferRef::from_mut_ptr(buffer);

    panic_to_error!(wrap, src, gst::FlowReturn::Error, {
        wrap.fill(src, offset, length, buffer)
    }).to_glib()
}

unsafe extern "C" fn source_seek(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    segment: *mut gst_ffi::GstSegment,
) -> glib_ffi::gboolean {
    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    let start = (*segment).start;
    let stop = (*segment).stop;

    panic_to_error!(wrap, src, false, {
        wrap.seek(src, start, if stop == u64::MAX { None } else { Some(stop) })
    }).to_glib()
}

unsafe extern "C" fn source_query(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    query_ptr: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean {
    let src_klass = &**(ptr as *mut *mut RsSrcClass);
    let source_info = &*src_klass.source_info;
    let parent_klass = &*(src_klass.parent_vtable as *const gst_base_ffi::GstBaseSrcClass);

    let src: &RsSrcWrapper = &from_glib_borrow(ptr as *mut RsSrc);
    let wrap = src.get_wrap();

    let query = gst::QueryRef::from_mut_ptr(query_ptr);

    panic_to_error!(wrap, src, false, {
        use gst::QueryView;

        match query.view_mut() {
            QueryView::Scheduling(ref mut q) if source_info.push_only => {
                q.set(gst::SCHEDULING_FLAG_SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            _ => parent_klass
                .query
                .map(|f| from_glib(f(ptr, query_ptr)))
                .unwrap_or(false),
        }
    }).to_glib()
}

pub struct SourceInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(&RsSrcWrapper) -> Box<Source>,
    pub protocols: Vec<String>,
    pub push_only: bool,
}

glib_wrapper! {
    pub struct RsSrcWrapper(Object<RsSrc>): [gst_base::BaseSrc => gst_base_ffi::GstBaseSrc,
                                             gst::Element => gst_ffi::GstElement,
                                             gst::Object => gst_ffi::GstObject,
                                             gst::URIHandler => gst_ffi::GstURIHandler,
                                            ];

    match fn {
        get_type => || rs_src_get_type(),
    }
}

impl RsSrcWrapper {
    fn get_wrap(&self) -> &SourceWrapper {
        let stash = self.to_glib_none();
        let src: *mut RsSrc = stash.0;

        unsafe { &*((*src).wrap) }
    }
}

#[repr(u32)]
enum Properties {
    PropURI = 1u32,
}

#[repr(C)]
pub struct RsSrc {
    parent: gst_base_ffi::GstPushSrc,
    wrap: *mut SourceWrapper,
}

#[repr(C)]
pub struct RsSrcClass {
    parent_class: gst_base_ffi::GstPushSrcClass,
    source_info: *const SourceInfo,
    protocols: *const Vec<*const c_char>,
    parent_vtable: glib_ffi::gconstpointer,
}

unsafe fn rs_src_get_type() -> glib_ffi::GType {
    use std::sync::{Once, ONCE_INIT};

    static mut TYPE: glib_ffi::GType = gobject_ffi::G_TYPE_INVALID;
    static ONCE: Once = ONCE_INIT;

    ONCE.call_once(|| {
        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<RsSrcClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(source_class_init),
            class_finalize: None,
            class_data: ptr::null_mut(),
            instance_size: mem::size_of::<RsSrc>() as u16,
            n_preallocs: 0,
            instance_init: Some(source_init),
            value_table: ptr::null(),
        };

        let type_name = {
            let mut idx = 0;

            loop {
                let type_name = CString::new(format!("RsSrc-{}", idx)).unwrap();
                if gobject_ffi::g_type_from_name(type_name.as_ptr()) == gobject_ffi::G_TYPE_INVALID
                {
                    break type_name;
                }
                idx += 1;
            }
        };

        TYPE = gobject_ffi::g_type_register_static(
            gst_base_ffi::gst_base_src_get_type(),
            type_name.as_ptr(),
            &type_info,
            gobject_ffi::GTypeFlags::empty(),
        );

        let iface_info = gobject_ffi::GInterfaceInfo {
            interface_init: Some(source_uri_handler_init),
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

unsafe extern "C" fn source_finalize(obj: *mut gobject_ffi::GObject) {
    let src = &mut *(obj as *mut RsSrc);

    drop(Box::from_raw(src.wrap));
    src.wrap = ptr::null_mut();

    let src_klass = &**(obj as *const *const RsSrcClass);
    let parent_klass = &*(src_klass.parent_vtable as *const gobject_ffi::GObjectClass);
    parent_klass.finalize.map(|f| f(obj));
}

unsafe extern "C" fn source_set_property(
    obj: *mut gobject_ffi::GObject,
    id: u32,
    value: *mut gobject_ffi::GValue,
    _pspec: *mut gobject_ffi::GParamSpec,
) {
    let src = obj as *mut RsSrc;

    match mem::transmute(id) {
        Properties::PropURI => {
            let uri_ptr = gobject_ffi::g_value_get_string(value);
            source_set_uri(src, uri_ptr, ptr::null_mut());
        }
        _ => unreachable!(),
    }
}

unsafe extern "C" fn source_get_property(
    obj: *mut gobject_ffi::GObject,
    id: u32,
    value: *mut gobject_ffi::GValue,
    _pspec: *mut gobject_ffi::GParamSpec,
) {
    let src = obj as *mut RsSrc;

    match mem::transmute(id) {
        Properties::PropURI => {
            let uri_ptr = source_get_uri(src);
            gobject_ffi::g_value_take_string(value, uri_ptr);
        }
        _ => unreachable!(),
    }
}

unsafe extern "C" fn source_sub_class_init(
    klass: glib_ffi::gpointer,
    klass_data: glib_ffi::gpointer,
) {
    let source_info = &*(klass_data as *const SourceInfo);

    {
        let element_klass = &mut *(klass as *mut gst_ffi::GstElementClass);

        gst_ffi::gst_element_class_set_metadata(
            element_klass,
            source_info.long_name.to_glib_none().0,
            source_info.classification.to_glib_none().0,
            source_info.description.to_glib_none().0,
            source_info.author.to_glib_none().0,
        );

        // TODO: Methods + source_info.caps
        let caps = gst::Caps::new_any();
        let pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        gst_ffi::gst_element_class_add_pad_template(element_klass, pad_template.to_glib_full());
    }

    {
        let src_klass = &mut *(klass as *mut RsSrcClass);

        src_klass.source_info = source_info;
        let mut protocols = Box::new(Vec::with_capacity(source_info.protocols.len()));
        for p in &source_info.protocols {
            protocols.push(p.to_glib_full());
        }
        protocols.push(ptr::null());
        src_klass.protocols = Box::into_raw(protocols) as *const Vec<*const c_char>;
    }
}

unsafe extern "C" fn source_class_init(klass: glib_ffi::gpointer, _klass_data: glib_ffi::gpointer) {
    {
        let gobject_klass = &mut *(klass as *mut gobject_ffi::GObjectClass);

        gobject_klass.set_property = Some(source_set_property);
        gobject_klass.get_property = Some(source_get_property);
        gobject_klass.finalize = Some(source_finalize);

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
        let basesrc_klass = &mut *(klass as *mut gst_base_ffi::GstBaseSrcClass);

        basesrc_klass.start = Some(source_start);
        basesrc_klass.stop = Some(source_stop);
        basesrc_klass.is_seekable = Some(source_is_seekable);
        basesrc_klass.get_size = Some(source_get_size);
        basesrc_klass.fill = Some(source_fill);
        basesrc_klass.do_seek = Some(source_seek);
        basesrc_klass.query = Some(source_query);
    }

    {
        let src_klass = &mut *(klass as *mut RsSrcClass);

        src_klass.parent_vtable = gobject_ffi::g_type_class_peek_parent(klass);
    }
}

unsafe extern "C" fn source_init(
    instance: *mut gobject_ffi::GTypeInstance,
    klass: glib_ffi::gpointer,
) {
    let src = &mut *(instance as *mut RsSrc);
    let src_klass = &*(klass as *const RsSrcClass);
    let source_info = &*src_klass.source_info;

    let wrap = Box::new(SourceWrapper::new((source_info.create_instance)(
        &RsSrcWrapper::from_glib_borrow(instance as *mut _),
    )));
    src.wrap = Box::into_raw(wrap);

    let src = &RsSrcWrapper::from_glib_borrow(src as *mut _);
    src.set_blocksize(4096);
}

unsafe extern "C" fn source_uri_handler_get_type(_type: glib_ffi::GType) -> gst_ffi::GstURIType {
    gst::URIType::Src.to_glib()
}

unsafe extern "C" fn source_uri_handler_get_protocols(
    type_: glib_ffi::GType,
) -> *const *const c_char {
    let klass = gobject_ffi::g_type_class_peek(type_);
    let src_klass = &*(klass as *const RsSrcClass);
    (*src_klass.protocols).as_ptr()
}

unsafe extern "C" fn source_uri_handler_get_uri(
    uri_handler: *mut gst_ffi::GstURIHandler,
) -> *mut c_char {
    source_get_uri(uri_handler as *mut RsSrc)
}

unsafe extern "C" fn source_uri_handler_set_uri(
    uri_handler: *mut gst_ffi::GstURIHandler,
    uri: *const c_char,
    err: *mut *mut glib_ffi::GError,
) -> glib_ffi::gboolean {
    source_set_uri(uri_handler as *mut RsSrc, uri, err)
}

unsafe extern "C" fn source_uri_handler_init(
    iface: glib_ffi::gpointer,
    _iface_data: glib_ffi::gpointer,
) {
    let uri_handler_iface = &mut *(iface as *mut gst_ffi::GstURIHandlerInterface);

    uri_handler_iface.get_type = Some(source_uri_handler_get_type);
    uri_handler_iface.get_protocols = Some(source_uri_handler_get_protocols);
    uri_handler_iface.get_uri = Some(source_uri_handler_get_uri);
    uri_handler_iface.set_uri = Some(source_uri_handler_set_uri);
}

pub fn source_register(plugin: &gst::Plugin, source_info: SourceInfo) {
    unsafe {
        let parent_type = rs_src_get_type();
        let type_name = format!("RsSrc-{}", source_info.name);

        let name = source_info.name.clone();
        let rank = source_info.rank;

        let source_info = Box::new(source_info);
        let source_info_ptr = Box::into_raw(source_info) as glib_ffi::gpointer;

        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<RsSrcClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(source_sub_class_init),
            class_finalize: None,
            class_data: source_info_ptr,
            instance_size: mem::size_of::<RsSrc>() as u16,
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
