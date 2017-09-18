// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::CString;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use std::u32;
use std::u64;
use std::ptr;
use std::mem;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ops::Deref;
use std::cmp;

use error::*;

use glib_ffi;
use gobject_ffi;
use gst_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;
use gst_base;
use gst_base::prelude::*;

pub type StreamIndex = u32;

#[derive(Debug)]
pub enum SeekResult {
    TooEarly,
    Ok(u64),
    Eos,
}

#[derive(Debug)]
pub enum HandleBufferResult {
    NeedMoreData,
    Again,
    // NeedDataFromOffset(u64),
    StreamAdded(Stream),
    HaveAllStreams,
    StreamChanged(Stream),
    // StreamsAdded(Vec<Stream>), // Implies HaveAllStreams
    StreamsChanged(Vec<Stream>),
    // TODO need something to replace/add new streams
    // TODO should probably directly implement the GstStreams new world order
    BufferForStream(StreamIndex, gst::Buffer),
    Eos(Option<StreamIndex>),
}

pub trait Demuxer {
    fn start(
        &mut self,
        demuxer: &RsDemuxerWrapper,
        upstream_size: Option<u64>,
        random_access: bool,
    ) -> Result<(), ErrorMessage>;
    fn stop(&mut self, demuxer: &RsDemuxerWrapper) -> Result<(), ErrorMessage>;

    fn seek(
        &mut self,
        demuxer: &RsDemuxerWrapper,
        start: u64,
        stop: Option<u64>,
    ) -> Result<SeekResult, ErrorMessage>;
    fn handle_buffer(
        &mut self,
        demuxer: &RsDemuxerWrapper,
        buffer: Option<gst::Buffer>,
    ) -> Result<HandleBufferResult, FlowError>;
    fn end_of_stream(&mut self, demuxer: &RsDemuxerWrapper) -> Result<(), ErrorMessage>;

    fn is_seekable(&self, demuxer: &RsDemuxerWrapper) -> bool;
    fn get_position(&self, demuxer: &RsDemuxerWrapper) -> Option<u64>;
    fn get_duration(&self, demuxer: &RsDemuxerWrapper) -> Option<u64>;
}

#[derive(Debug)]
pub struct Stream {
    pub index: StreamIndex,
    pub caps: gst::Caps,
    pub stream_id: String,
}

impl Stream {
    pub fn new(index: StreamIndex, caps: gst::Caps, stream_id: String) -> Stream {
        Stream {
            index: index,
            caps: caps,
            stream_id: stream_id,
        }
    }
}

pub struct DemuxerWrapper {
    cat: gst::DebugCategory,
    demuxer: Mutex<Box<Demuxer>>,
    panicked: AtomicBool,
}

impl DemuxerWrapper {
    fn new(demuxer: Box<Demuxer>) -> DemuxerWrapper {
        DemuxerWrapper {
            cat: gst::DebugCategory::new(
                "rsdemux",
                gst::DebugColorFlags::empty(),
                "Rust demuxer base class",
            ),
            demuxer: Mutex::new(demuxer),
            panicked: AtomicBool::new(false),
        }
    }

    fn start(
        &self,
        demuxer: &RsDemuxerWrapper,
        upstream_size: Option<u64>,
        random_access: bool,
    ) -> bool {
        let demuxer_impl = &mut self.demuxer.lock().unwrap();

        gst_debug!(
            self.cat,
            obj: demuxer,
            "Starting with upstream size {:?} and random access {}",
            upstream_size,
            random_access
        );

        match demuxer_impl.start(demuxer, upstream_size, random_access) {
            Ok(..) => {
                gst_trace!(self.cat, obj: demuxer, "Successfully started",);
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: demuxer, "Failed to start: {:?}", msg);
                self.post_message(demuxer, msg);
                false
            }
        }
    }
    fn stop(&self, demuxer: &RsDemuxerWrapper) -> bool {
        let demuxer_impl = &mut self.demuxer.lock().unwrap();

        gst_debug!(self.cat, obj: demuxer, "Stopping");

        match demuxer_impl.stop(demuxer) {
            Ok(..) => {
                gst_trace!(self.cat, obj: demuxer, "Successfully stop");
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: demuxer, "Failed to stop: {:?}", msg);
                self.post_message(demuxer, msg);
                false
            }
        }
    }

    fn is_seekable(&self, demuxer: &RsDemuxerWrapper) -> bool {
        let demuxer_impl = &self.demuxer.lock().unwrap();

        let seekable = demuxer_impl.is_seekable(demuxer);
        gst_debug!(self.cat, obj: demuxer, "Seekable {}", seekable);

        seekable
    }


    fn get_position(&self, demuxer: &RsDemuxerWrapper) -> Option<u64> {
        let demuxer_impl = &self.demuxer.lock().unwrap();

        demuxer_impl.get_position(demuxer)
    }

    fn get_duration(&self, demuxer: &RsDemuxerWrapper) -> Option<u64> {
        let demuxer_impl = &self.demuxer.lock().unwrap();

        demuxer_impl.get_duration(demuxer)
    }

    fn seek(&self, demuxer: &RsDemuxerWrapper, start: u64, stop: u64, offset: &mut u64) -> bool {
        let stop = if stop == u64::MAX { None } else { Some(stop) };

        gst_debug!(self.cat, obj: demuxer, "Seeking to {:?}-{:?}", start, stop);

        let res = {
            let mut demuxer_impl = &mut self.demuxer.lock().unwrap();

            match demuxer_impl.seek(demuxer, start, stop) {
                Ok(res) => res,
                Err(ref msg) => {
                    gst_error!(self.cat, obj: demuxer, "Failed to seek: {:?}", msg);
                    self.post_message(demuxer, msg);
                    return false;
                }
            }
        };

        match res {
            SeekResult::TooEarly => {
                gst_debug!(self.cat, obj: demuxer, "Seeked too early");
                false
            }
            SeekResult::Ok(off) => {
                gst_trace!(self.cat, obj: demuxer, "Seeked successfully");
                *offset = off;
                true
            }
            SeekResult::Eos => {
                gst_debug!(self.cat, obj: demuxer, "Seeked after EOS");
                *offset = u64::MAX;

                demuxer.stream_eos(None);

                true
            }
        }
    }

    fn handle_buffer(&self, demuxer: &RsDemuxerWrapper, buffer: gst::Buffer) -> gst::FlowReturn {
        let mut res = {
            let mut demuxer_impl = &mut self.demuxer.lock().unwrap();

            gst_trace!(self.cat, obj: demuxer, "Handling buffer {:?}", buffer);

            match demuxer_impl.handle_buffer(demuxer, Some(buffer)) {
                Ok(res) => res,
                Err(flow_error) => {
                    gst_error!(
                        self.cat,
                        obj: demuxer,
                        "Failed handling buffer: {:?}",
                        flow_error
                    );
                    match flow_error {
                        FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                            self.post_message(demuxer, msg)
                        }
                        _ => (),
                    }
                    return flow_error.to_native();
                }
            }
        };

        // Loop until AllEos, NeedMoreData or error when pushing downstream
        loop {
            gst_trace!(self.cat, obj: demuxer, "Handled {:?}", res);

            match res {
                HandleBufferResult::NeedMoreData => {
                    return gst::FlowReturn::Ok;
                }
                HandleBufferResult::StreamAdded(stream) => {
                    demuxer.add_stream(stream.index, stream.caps, &stream.stream_id);
                }
                HandleBufferResult::HaveAllStreams => {
                    demuxer.added_all_streams();
                }
                HandleBufferResult::StreamChanged(stream) => {
                    demuxer.stream_format_changed(stream.index, stream.caps);
                }
                HandleBufferResult::StreamsChanged(streams) => for stream in streams {
                    demuxer.stream_format_changed(stream.index, stream.caps);
                },
                HandleBufferResult::BufferForStream(index, buffer) => {
                    let flow_ret = demuxer.stream_push_buffer(index, buffer);

                    if flow_ret != gst::FlowReturn::Ok {
                        return flow_ret;
                    }
                }
                HandleBufferResult::Eos(index) => {
                    demuxer.stream_eos(index);
                    return gst::FlowReturn::Eos;
                }
                HandleBufferResult::Again => {
                    // nothing, just call again
                }
            };

            gst_trace!(self.cat, obj: demuxer, "Calling again");

            res = {
                let mut demuxer_impl = &mut self.demuxer.lock().unwrap();
                match demuxer_impl.handle_buffer(demuxer, None) {
                    Ok(res) => res,
                    Err(flow_error) => {
                        gst_error!(
                            self.cat,
                            obj: demuxer,
                            "Failed calling again: {:?}",
                            flow_error
                        );
                        match flow_error {
                            FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                                self.post_message(demuxer, msg)
                            }
                            _ => (),
                        }
                        return flow_error.to_native();
                    }
                }
            }
        }
    }

    fn end_of_stream(&self, demuxer: &RsDemuxerWrapper) {
        let mut demuxer_impl = &mut self.demuxer.lock().unwrap();

        gst_debug!(self.cat, obj: demuxer, "End of stream");
        match demuxer_impl.end_of_stream(demuxer) {
            Ok(_) => (),
            Err(ref msg) => {
                gst_error!(self.cat, obj: demuxer, "Failed end of stream: {:?}", msg);
                self.post_message(demuxer, msg);
            }
        }
    }

    fn post_message(&self, demuxer: &RsDemuxerWrapper, msg: &ErrorMessage) {
        msg.post(demuxer);
    }
}

pub struct DemuxerInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(&RsDemuxerWrapper) -> Box<Demuxer>,
    pub input_caps: gst::Caps,
    pub output_caps: gst::Caps,
}

struct OrderedPad(pub gst::Pad);

impl Deref for OrderedPad {
    type Target = gst::Pad;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<gst::Pad> for OrderedPad {
    fn as_ref(&self) -> &gst::Pad {
        &self.0
    }
}

impl PartialEq for OrderedPad {
    fn eq(&self, other: &Self) -> bool {
        self.get_name() == other.get_name()
    }
}

impl Eq for OrderedPad {}

impl PartialOrd for OrderedPad {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedPad {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.get_name().cmp(&other.get_name())
    }
}

glib_wrapper! {
    pub struct RsDemuxerWrapper(Object<RsDemuxer>): [gst::Element => gst_ffi::GstElement,
                                                     gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || rs_demuxer_get_type(),
    }
}

impl RsDemuxerWrapper {
    fn get_wrap(&self) -> &DemuxerWrapper {
        let stash = self.to_glib_none();
        let demuxer: *mut RsDemuxer = stash.0;

        unsafe { &*((*demuxer).wrap) }
    }

    fn get_private(&self) -> &RsDemuxerPrivate {
        let stash = self.to_glib_none();
        let demuxer: *mut RsDemuxer = stash.0;

        unsafe { &*((*demuxer).private) }
    }

    fn change_state(&self, transition: gst::StateChange) -> gst::StateChangeReturn {
        let mut ret = gst::StateChangeReturn::Success;
        let wrap = self.get_wrap();
        let private = self.get_private();

        gst_trace!(wrap.cat, obj: self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                // TODO
                private.group_id.set(gst::util_group_id_next());
            }
            _ => (),
        }

        ret = self.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::PausedToReady => {
                private.flow_combiner.borrow_mut().clear();
                let mut srcpads = private.srcpads.borrow_mut();
                for (_, pad) in srcpads.iter().by_ref() {
                    self.remove_pad(pad.as_ref()).unwrap();
                }
                srcpads.clear();
            }
            _ => (),
        }

        ret
    }

    fn add_stream(&self, index: u32, caps: gst::Caps, stream_id: &str) {
        let private = self.get_private();

        let mut srcpads = private.srcpads.borrow_mut();
        assert!(!srcpads.contains_key(&index));

        let templ = self.get_pad_template("src_%u").unwrap();
        let name = format!("src_{}", index);
        let pad = gst::Pad::new_from_template(&templ, Some(name.as_str()));
        pad.set_query_function(RsDemuxerWrapper::src_query);
        pad.set_event_function(RsDemuxerWrapper::src_event);

        pad.set_active(true).unwrap();

        let full_stream_id = pad.create_stream_id(self, stream_id).unwrap();
        pad.push_event(
            gst::Event::new_stream_start(&full_stream_id)
                .group_id(private.group_id.get())
                .build(),
        );
        pad.push_event(gst::Event::new_caps(&caps).build());

        let mut segment = gst::Segment::default();
        segment.init(gst::Format::Time);
        pad.push_event(gst::Event::new_segment(&segment).build());

        private.flow_combiner.borrow_mut().add_pad(&pad);
        self.add_pad(&pad).unwrap();

        srcpads.insert(index, OrderedPad(pad));
    }

    fn added_all_streams(&self) {
        let private = self.get_private();

        self.no_more_pads();
        private.group_id.set(gst::util_group_id_next());
    }

    fn stream_format_changed(&self, index: u32, caps: gst::Caps) {
        let private = self.get_private();
        let srcpads = private.srcpads.borrow();

        if let Some(pad) = srcpads.get(&index) {
            pad.push_event(gst::Event::new_caps(&caps).build());
        }
    }

    fn stream_eos(&self, index: Option<u32>) {
        let private = self.get_private();
        let srcpads = private.srcpads.borrow();

        let event = gst::Event::new_eos().build();
        match index {
            Some(index) => if let Some(pad) = srcpads.get(&index) {
                pad.push_event(event);
            },
            None => for (_, pad) in srcpads.iter().by_ref() {
                pad.push_event(event.clone());
            },
        };
    }

    fn stream_push_buffer(&self, index: u32, buffer: gst::Buffer) -> gst::FlowReturn {
        let private = self.get_private();
        let srcpads = private.srcpads.borrow();

        if let Some(pad) = srcpads.get(&index) {
            private
                .flow_combiner
                .borrow_mut()
                .update_flow(pad.push(buffer))
        } else {
            gst::FlowReturn::Error
        }
    }

    fn remove_all_streams(&self) {
        let private = self.get_private();
        private.flow_combiner.borrow_mut().clear();
        let mut srcpads = private.srcpads.borrow_mut();
        for (_, pad) in srcpads.iter().by_ref() {
            self.remove_pad(pad.as_ref()).unwrap();
        }
        srcpads.clear();
    }

    fn parent_change_state(&self, transition: gst::StateChange) -> gst::StateChangeReturn {
        unsafe {
            let stash = self.to_glib_none();
            let demuxer: *mut RsDemuxer = stash.0;
            let demuxer_klass = &**(demuxer as *const *const RsDemuxerClass);
            let parent_klass = &*(demuxer_klass.parent_vtable as *const gst_ffi::GstElementClass);

            parent_klass
                .change_state
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, transition.to_glib()))
                })
                .unwrap_or(gst::StateChangeReturn::Failure)
        }
    }

    fn sink_activate(_pad: &gst::Pad, parent: &Option<gst::Object>) -> bool {
        let this = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsDemuxerWrapper>()
            .unwrap();
        let private = this.get_private();

        let mode = {
            let mut query = gst::Query::new_scheduling();
            if !private.sinkpad.peer_query(query.get_mut().unwrap()) {
                return false;
            }

            // TODO
            //if (gst_query_has_scheduling_mode_with_flags (query, GST_PAD_MODE_PULL, GST_SCHEDULING_FLAG_SEEKABLE)) {
            //  GST_DEBUG_OBJECT (demuxer, "Activating in PULL mode");
            //  mode = GST_PAD_MODE_PULL;
            //} else {
            //GST_DEBUG_OBJECT (demuxer, "Activating in PUSH mode");
            //}
            gst::PadMode::Push
        };

        let mut query = gst::Query::new_duration(gst::Format::Bytes);
        let upstream_size = if private.sinkpad.peer_query(query.get_mut().unwrap()) {
            use gst::QueryView;

            match query.view() {
                QueryView::Duration(ref d) => Some(d.get().1 as u64),
                _ => unreachable!(),
            }
        } else {
            None
        };
        private.upstream_size.set(upstream_size);

        match private.sinkpad.activate_mode(mode, true) {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    fn sink_activatemode(
        _pad: &gst::Pad,
        parent: &Option<gst::Object>,
        mode: gst::PadMode,
        active: bool,
    ) -> bool {
        let this = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsDemuxerWrapper>()
            .unwrap();
        let private = this.get_private();
        let wrap = this.get_wrap();

        if active {
            if !wrap.start(
                &this,
                private.upstream_size.get(),
                mode == gst::PadMode::Pull,
            ) {
                return false;
            }

            if mode == gst::PadMode::Pull {
                // TODO
                // private.sinkpad.start_task(...)
            }

            true
        } else {
            if mode == gst::PadMode::Pull {
                let _ = private.sinkpad.stop_task();
            }

            wrap.stop(&this)
        }
    }

    fn sink_chain(
        _pad: &gst::Pad,
        parent: &Option<gst::Object>,
        buffer: gst::Buffer,
    ) -> gst::FlowReturn {
        let this = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsDemuxerWrapper>()
            .unwrap();
        let wrap = this.get_wrap();
        wrap.handle_buffer(&this, buffer)
    }

    fn sink_event(pad: &gst::Pad, parent: &Option<gst::Object>, event: gst::Event) -> bool {
        use gst::EventView;

        let this = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsDemuxerWrapper>()
            .unwrap();
        let wrap = this.get_wrap();

        match event.view() {
            EventView::Eos(..) => {
                wrap.end_of_stream(&this);
                pad.event_default(parent.as_ref(), event)
            }
            EventView::Segment(..) => pad.event_default(parent.as_ref(), event),
            _ => pad.event_default(parent.as_ref(), event),
        }
    }

    fn src_query(pad: &gst::Pad, parent: &Option<gst::Object>, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        let this = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsDemuxerWrapper>()
            .unwrap();
        let wrap = this.get_wrap();

        match query.view_mut() {
            QueryView::Position(ref mut q) => {
                let (fmt, _) = q.get();
                if fmt == gst::Format::Time {
                    let position = wrap.get_position(&this);
                    gst_trace!(wrap.cat, obj: &this, "Returning position {:?}", position);

                    match position {
                        None => return false,
                        Some(position) => {
                            q.set(fmt, position as i64);
                            return true;
                        }
                    }
                } else {
                    return false;
                }
            }
            QueryView::Duration(ref mut q) => {
                let (fmt, _) = q.get();
                if fmt == gst::Format::Time {
                    let duration = wrap.get_duration(&this);
                    gst_trace!(wrap.cat, obj: &this, "Returning duration {:?}", duration);

                    match duration {
                        None => return false,
                        Some(duration) => {
                            q.set(fmt, duration as i64);
                            return true;
                        }
                    }
                } else {
                    return false;
                }
            }
            _ => (),
        }

        // FIXME: Have to do it outside the match because otherwise query is already mutably
        // borrowed by the query view.
        pad.query_default(parent.as_ref(), query)
    }

    fn src_event(pad: &gst::Pad, parent: &Option<gst::Object>, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Seek(..) => {
                // TODO: Implement
                false
            }
            _ => pad.event_default(parent.as_ref(), event),
        }
    }
}

#[repr(C)]
pub struct RsDemuxer {
    parent: gst_ffi::GstElement,
    wrap: *mut DemuxerWrapper,
    private: *mut RsDemuxerPrivate,
}

#[repr(C)]
pub struct RsDemuxerClass {
    parent_class: gst_ffi::GstElementClass,
    demuxer_info: *const DemuxerInfo,
    parent_vtable: glib_ffi::gconstpointer,
}

pub struct RsDemuxerPrivate {
    sinkpad: gst::Pad,
    flow_combiner: RefCell<gst_base::FlowCombiner>,
    upstream_size: Cell<Option<u64>>,
    group_id: Cell<u32>,
    srcpads: RefCell<BTreeMap<u32, OrderedPad>>,
}

unsafe fn rs_demuxer_get_type() -> glib_ffi::GType {
    use std::sync::{Once, ONCE_INIT};

    static mut TYPE: glib_ffi::GType = gobject_ffi::G_TYPE_INVALID;
    static ONCE: Once = ONCE_INIT;

    ONCE.call_once(|| {
        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<RsDemuxerClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(demuxer_class_init),
            class_finalize: None,
            class_data: ptr::null_mut(),
            instance_size: mem::size_of::<RsDemuxer>() as u16,
            n_preallocs: 0,
            instance_init: None,
            value_table: ptr::null(),
        };

        let type_name = {
            let mut idx = 0;

            loop {
                let type_name = CString::new(format!("RsDemuxer-{}", idx)).unwrap();
                if gobject_ffi::g_type_from_name(type_name.as_ptr()) == gobject_ffi::G_TYPE_INVALID
                {
                    break type_name;
                }
                idx += 1;
            }
        };

        TYPE = gobject_ffi::g_type_register_static(
            gst_ffi::gst_element_get_type(),
            type_name.as_ptr(),
            &type_info,
            gobject_ffi::GTypeFlags::empty(),
        );
    });

    TYPE
}

unsafe extern "C" fn demuxer_finalize(obj: *mut gobject_ffi::GObject) {
    let demuxer = &mut *(obj as *mut RsDemuxer);

    drop(Box::from_raw(demuxer.wrap));
    demuxer.wrap = ptr::null_mut();

    drop(Box::from_raw(demuxer.private));
    demuxer.private = ptr::null_mut();

    let demuxer_klass = &**(obj as *const *const RsDemuxerClass);
    let parent_klass = &*(demuxer_klass.parent_vtable as *const gobject_ffi::GObjectClass);
    parent_klass.finalize.map(|f| f(obj));
}

unsafe extern "C" fn demuxer_sub_class_init(
    klass: glib_ffi::gpointer,
    klass_data: glib_ffi::gpointer,
) {
    let demuxer_info = &*(klass_data as *const DemuxerInfo);

    {
        let element_klass = &mut *(klass as *mut gst_ffi::GstElementClass);

        gst_ffi::gst_element_class_set_metadata(
            element_klass,
            demuxer_info.long_name.to_glib_none().0,
            demuxer_info.classification.to_glib_none().0,
            demuxer_info.description.to_glib_none().0,
            demuxer_info.author.to_glib_none().0,
        );

        let pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &demuxer_info.input_caps,
        );
        gst_ffi::gst_element_class_add_pad_template(element_klass, pad_template.to_glib_full());

        let pad_template = gst::PadTemplate::new(
            "src_%u",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &demuxer_info.output_caps,
        );
        gst_ffi::gst_element_class_add_pad_template(element_klass, pad_template.to_glib_full());
    }

    {
        let demuxer_klass = &mut *(klass as *mut RsDemuxerClass);

        demuxer_klass.demuxer_info = demuxer_info;
    }
}

unsafe extern "C" fn demuxer_class_init(
    klass: glib_ffi::gpointer,
    _klass_data: glib_ffi::gpointer,
) {
    {
        let gobject_klass = &mut *(klass as *mut gobject_ffi::GObjectClass);

        gobject_klass.finalize = Some(demuxer_finalize);
    }

    {
        let element_klass = &mut *(klass as *mut gst_ffi::GstElementClass);

        element_klass.change_state = Some(demuxer_change_state);
    }

    {
        let demuxer_klass = &mut *(klass as *mut RsDemuxerClass);

        demuxer_klass.parent_vtable = gobject_ffi::g_type_class_peek_parent(klass);
    }
}

unsafe extern "C" fn demuxer_change_state(
    ptr: *mut gst_ffi::GstElement,
    transition: gst_ffi::GstStateChange,
) -> gst_ffi::GstStateChangeReturn {
    let demuxer: &RsDemuxerWrapper = &from_glib_borrow(ptr as *mut RsDemuxer);
    let wrap = demuxer.get_wrap();

    panic_to_error!(wrap, demuxer, gst::StateChangeReturn::Failure, {
        demuxer.change_state(from_glib(transition))
    }).to_glib()
}

unsafe extern "C" fn demuxer_sub_init(
    instance: *mut gobject_ffi::GTypeInstance,
    klass: glib_ffi::gpointer,
) {
    let demuxer = &mut *(instance as *mut RsDemuxer);
    let demuxer_klass = &*(klass as *const RsDemuxerClass);
    let demuxer_info = &*demuxer_klass.demuxer_info;

    let private = &mut demuxer.private as *mut _;

    let wrap = Box::new(DemuxerWrapper::new((demuxer_info.create_instance)(
        &RsDemuxerWrapper::from_glib_borrow(instance as *mut _),
    )));
    demuxer.wrap = Box::into_raw(wrap);

    let demuxer = &RsDemuxerWrapper::from_glib_borrow(demuxer as *mut _);

    let templ = demuxer.get_pad_template("sink").unwrap();
    let sinkpad = gst::Pad::new_from_template(&templ, "sink");
    sinkpad.set_activate_function(RsDemuxerWrapper::sink_activate);
    sinkpad.set_activatemode_function(RsDemuxerWrapper::sink_activatemode);
    sinkpad.set_chain_function(RsDemuxerWrapper::sink_chain);
    sinkpad.set_event_function(RsDemuxerWrapper::sink_event);
    demuxer.add_pad(&sinkpad).unwrap();

    let private_data = Box::new(RsDemuxerPrivate {
        sinkpad: sinkpad,
        flow_combiner: RefCell::new(Default::default()),
        upstream_size: Cell::new(None),
        group_id: Cell::new(gst::util_group_id_next()),
        srcpads: RefCell::new(BTreeMap::new()),
    });
    *private = Box::into_raw(private_data);
}


pub fn demuxer_register(plugin: &gst::Plugin, demuxer_info: DemuxerInfo) {
    unsafe {
        let parent_type = rs_demuxer_get_type();
        let type_name = format!("RsDemuxer-{}", demuxer_info.name);

        let name = demuxer_info.name.clone();
        let rank = demuxer_info.rank;

        let demuxer_info = Box::new(demuxer_info);
        let demuxer_info_ptr = Box::into_raw(demuxer_info) as glib_ffi::gpointer;

        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<RsDemuxerClass>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(demuxer_sub_class_init),
            class_finalize: None,
            class_data: demuxer_info_ptr,
            instance_size: mem::size_of::<RsDemuxer>() as u16,
            n_preallocs: 0,
            instance_init: Some(demuxer_sub_init),
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
