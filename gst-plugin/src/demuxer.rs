//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//
//  This library is free software; you can redistribute it and/or
//  modify it under the terms of the GNU Library General Public
//  License as published by the Free Software Foundation; either
//  version 2 of the License, or (at your option) any later version.
//
//  This library is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
//  Library General Public License for more details.
//
//  You should have received a copy of the GNU Library General Public
//  License along with this library; if not, write to the
//  Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
//  Boston, MA 02110-1301, USA.

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::CString;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use std::u32;
use std::u64;

use slog::*;

use utils::*;
use error::*;
use buffer::*;
use log::*;
use plugin::Plugin;

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
    BufferForStream(StreamIndex, Buffer),
    Eos(Option<StreamIndex>),
}

pub trait Demuxer {
    fn start(&mut self,
             upstream_size: Option<u64>,
             random_access: bool)
             -> Result<(), ErrorMessage>;
    fn stop(&mut self) -> Result<(), ErrorMessage>;

    fn seek(&mut self, start: u64, stop: Option<u64>) -> Result<SeekResult, ErrorMessage>;
    fn handle_buffer(&mut self, buffer: Option<Buffer>) -> Result<HandleBufferResult, FlowError>;
    fn end_of_stream(&mut self) -> Result<(), ErrorMessage>;

    fn is_seekable(&self) -> bool;
    fn get_position(&self) -> Option<u64>;
    fn get_duration(&self) -> Option<u64>;
}

#[derive(Debug)]
pub struct Stream {
    pub index: StreamIndex,
    pub format: String,
    pub stream_id: String,
}

impl Stream {
    pub fn new(index: StreamIndex, format: String, stream_id: String) -> Stream {
        Stream {
            index: index,
            format: format,
            stream_id: stream_id,
        }
    }
}

pub struct DemuxerWrapper {
    raw: *mut c_void,
    logger: Logger,
    demuxer: Mutex<Box<Demuxer>>,
    panicked: AtomicBool,
}

impl DemuxerWrapper {
    fn new(raw: *mut c_void, demuxer: Box<Demuxer>) -> DemuxerWrapper {
        DemuxerWrapper {
            raw: raw,
            logger: Logger::root(GstDebugDrain::new(Some(unsafe { &Element::new(raw) }),
                                                    "rsdemux",
                                                    0,
                                                    "Rust demuxer base class"),
                                 None),
            demuxer: Mutex::new(demuxer),
            panicked: AtomicBool::new(false),
        }
    }

    fn start(&self, upstream_size: u64, random_access: bool) -> bool {
        let demuxer = &mut self.demuxer.lock().unwrap();

        debug!(self.logger,
               "Starting with upstream size {} and random access {}",
               upstream_size,
               random_access);

        let upstream_size = if upstream_size == u64::MAX {
            None
        } else {
            Some(upstream_size)
        };

        match demuxer.start(upstream_size, random_access) {
            Ok(..) => {
                trace!(self.logger, "Successfully started");
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to start: {:?}", msg);
                self.post_message(msg);
                false
            }
        }

    }
    fn stop(&self) -> bool {
        let demuxer = &mut self.demuxer.lock().unwrap();

        debug!(self.logger, "Stopping");

        match demuxer.stop() {
            Ok(..) => {
                trace!(self.logger, "Successfully stop");
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to stop: {:?}", msg);
                self.post_message(msg);
                false
            }
        }
    }

    fn is_seekable(&self) -> bool {
        let demuxer = &self.demuxer.lock().unwrap();

        let seekable = demuxer.is_seekable();
        debug!(self.logger, "Seekable {}", seekable);

        seekable
    }


    fn get_position(&self, position: &mut u64) -> GBoolean {
        let demuxer = &self.demuxer.lock().unwrap();

        match demuxer.get_position() {
            None => {
                trace!(self.logger, "Got no position");
                *position = u64::MAX;
                GBoolean::False
            }
            Some(pos) => {
                trace!(self.logger, "Returning position {}", pos);
                *position = pos;
                GBoolean::True
            }
        }

    }

    fn get_duration(&self, duration: &mut u64) -> GBoolean {
        let demuxer = &self.demuxer.lock().unwrap();

        match demuxer.get_duration() {
            None => {
                trace!(self.logger, "Got no duration");
                *duration = u64::MAX;
                GBoolean::False
            }
            Some(dur) => {
                trace!(self.logger, "Returning duration {}", dur);
                *duration = dur;
                GBoolean::True
            }
        }
    }

    fn seek(&self, start: u64, stop: u64, offset: &mut u64) -> bool {
        extern "C" {
            fn gst_rs_demuxer_stream_eos(raw: *mut c_void, index: u32);
        };

        let stop = if stop == u64::MAX { None } else { Some(stop) };

        debug!(self.logger, "Seeking to {:?}-{:?}", start, stop);

        let res = {
            let mut demuxer = &mut self.demuxer.lock().unwrap();

            match demuxer.seek(start, stop) {
                Ok(res) => res,
                Err(ref msg) => {
                    error!(self.logger, "Failed to seek: {:?}", msg);
                    self.post_message(msg);
                    return false;
                }
            }
        };

        match res {
            SeekResult::TooEarly => {
                debug!(self.logger, "Seeked too early");
                false
            }
            SeekResult::Ok(off) => {
                trace!(self.logger, "Seeked successfully");
                *offset = off;
                true
            }
            SeekResult::Eos => {
                debug!(self.logger, "Seeked after EOS");
                *offset = u64::MAX;

                unsafe {
                    gst_rs_demuxer_stream_eos(self.raw, u32::MAX);
                }

                true
            }
        }
    }

    fn handle_buffer(&self, buffer: Buffer) -> GstFlowReturn {
        extern "C" {
            fn gst_rs_demuxer_stream_eos(raw: *mut c_void, index: u32);
            fn gst_rs_demuxer_add_stream(raw: *mut c_void,
                                         index: u32,
                                         format: *const c_char,
                                         stream_id: *const c_char);
            fn gst_rs_demuxer_added_all_streams(raw: *mut c_void);
            // fn gst_rs_demuxer_remove_all_streams(raw: *mut c_void);
            fn gst_rs_demuxer_stream_format_changed(raw: *mut c_void,
                                                    index: u32,
                                                    format: *const c_char);
            fn gst_rs_demuxer_stream_push_buffer(raw: *mut c_void,
                                                 index: u32,
                                                 buffer: *mut c_void)
                                                 -> GstFlowReturn;
        };

        let mut res = {
            let mut demuxer = &mut self.demuxer.lock().unwrap();

            trace!(self.logger, "Handling buffer {:?}", buffer);

            match demuxer.handle_buffer(Some(buffer)) {
                Ok(res) => res,
                Err(flow_error) => {
                    error!(self.logger, "Failed handling buffer: {:?}", flow_error);
                    match flow_error {
                        FlowError::NotNegotiated(ref msg) |
                        FlowError::Error(ref msg) => self.post_message(msg),
                        _ => (),
                    }
                    return flow_error.to_native();
                }
            }
        };

        // Loop until AllEos, NeedMoreData or error when pushing downstream
        loop {
            trace!(self.logger, "Handled {:?}", res);

            match res {
                HandleBufferResult::NeedMoreData => {
                    return GstFlowReturn::Ok;
                }
                HandleBufferResult::StreamAdded(stream) => {
                    let format_cstr = CString::new(stream.format.as_bytes()).unwrap();
                    let stream_id_cstr = CString::new(stream.stream_id.as_bytes()).unwrap();

                    unsafe {
                        gst_rs_demuxer_add_stream(self.raw,
                                                  stream.index,
                                                  format_cstr.as_ptr(),
                                                  stream_id_cstr.as_ptr());
                    }
                }
                HandleBufferResult::HaveAllStreams => unsafe {
                    gst_rs_demuxer_added_all_streams(self.raw);
                },
                HandleBufferResult::StreamChanged(stream) => {
                    let format_cstr = CString::new(stream.format.as_bytes()).unwrap();

                    unsafe {
                        gst_rs_demuxer_stream_format_changed(self.raw,
                                                             stream.index,
                                                             format_cstr.as_ptr());
                    }
                }
                HandleBufferResult::StreamsChanged(streams) => {
                    for stream in streams {
                        let format_cstr = CString::new(stream.format.as_bytes()).unwrap();

                        unsafe {
                            gst_rs_demuxer_stream_format_changed(self.raw,
                                                                 stream.index,
                                                                 format_cstr.as_ptr());
                        }
                    }
                }
                HandleBufferResult::BufferForStream(index, buffer) => {
                    let flow_ret = unsafe {
                        gst_rs_demuxer_stream_push_buffer(self.raw, index, buffer.into_ptr())
                    };
                    if flow_ret != GstFlowReturn::Ok {
                        return flow_ret;
                    }
                }
                HandleBufferResult::Eos(index) => {
                    let index = index.unwrap_or(u32::MAX);

                    unsafe {
                        gst_rs_demuxer_stream_eos(self.raw, index);
                    }

                    return GstFlowReturn::Eos;
                }
                HandleBufferResult::Again => {
                    // nothing, just call again
                }
            };

            trace!(self.logger, "Calling again");

            res = {
                let mut demuxer = &mut self.demuxer.lock().unwrap();
                match demuxer.handle_buffer(None) {
                    Ok(res) => res,
                    Err(flow_error) => {
                        error!(self.logger, "Failed calling again: {:?}", flow_error);
                        match flow_error {
                            FlowError::NotNegotiated(ref msg) |
                            FlowError::Error(ref msg) => self.post_message(msg),
                            _ => (),
                        }
                        return flow_error.to_native();
                    }
                }
            }
        }
    }

    fn end_of_stream(&self) {
        let mut demuxer = &mut self.demuxer.lock().unwrap();

        debug!(self.logger, "End of stream");
        match demuxer.end_of_stream() {
            Ok(_) => (),
            Err(ref msg) => {
                error!(self.logger, "Failed end of stream: {:?}", msg);
                self.post_message(msg);
            }
        }
    }

    fn post_message(&self, msg: &ErrorMessage) {
        unsafe {
            msg.post(self.raw);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_new(demuxer: *mut c_void,
                                     create_instance: fn(Element) -> Box<Demuxer>)
                                     -> *mut DemuxerWrapper {
    let instance = create_instance(Element::new(demuxer));
    Box::into_raw(Box::new(DemuxerWrapper::new(demuxer, instance)))
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_drop(ptr: *mut DemuxerWrapper) {
    let _ = Box::from_raw(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_start(ptr: *const DemuxerWrapper,
                                       upstream_size: u64,
                                       random_access: GBoolean)
                                       -> GBoolean {
    let wrap: &DemuxerWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        GBoolean::from_bool(wrap.start(upstream_size, random_access.to_bool()))
    })
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_stop(ptr: *const DemuxerWrapper) -> GBoolean {
    let wrap: &DemuxerWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::True, {
        GBoolean::from_bool(wrap.stop())
    })
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_is_seekable(ptr: *const DemuxerWrapper) -> GBoolean {
    let wrap: &DemuxerWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        GBoolean::from_bool(wrap.is_seekable())
    })
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_get_position(ptr: *const DemuxerWrapper,
                                              position: *mut u64)
                                              -> GBoolean {
    let wrap: &DemuxerWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let position = &mut *position;
        wrap.get_position(position)
    })
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_get_duration(ptr: *const DemuxerWrapper,
                                              duration: *mut u64)
                                              -> GBoolean {
    let wrap: &DemuxerWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let duration = &mut *duration;
        wrap.get_duration(duration)
    })
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_seek(ptr: *mut DemuxerWrapper,
                                      start: u64,
                                      stop: u64,
                                      offset: *mut u64)
                                      -> GBoolean {

    let wrap: &mut DemuxerWrapper = &mut *ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let offset = &mut *offset;

        GBoolean::from_bool(wrap.seek(start, stop, offset))
    })
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_handle_buffer(ptr: *mut DemuxerWrapper,
                                               buffer: *mut c_void)
                                               -> GstFlowReturn {
    let wrap: &mut DemuxerWrapper = &mut *ptr;

    panic_to_error!(wrap, GstFlowReturn::Error, {
        let buffer = Buffer::new_from_ptr_owned(buffer);
        wrap.handle_buffer(buffer)
    })
}

#[no_mangle]
pub unsafe extern "C" fn demuxer_end_of_stream(ptr: *mut DemuxerWrapper) {
    let wrap: &mut DemuxerWrapper = &mut *ptr;

    panic_to_error!(wrap, (), {
        wrap.end_of_stream();
    })
}

pub struct DemuxerInfo<'a> {
    pub name: &'a str,
    pub long_name: &'a str,
    pub description: &'a str,
    pub classification: &'a str,
    pub author: &'a str,
    pub rank: i32,
    pub create_instance: fn(Element) -> Box<Demuxer>,
    pub input_formats: &'a str,
    pub output_formats: &'a str,
}

pub fn demuxer_register(plugin: &Plugin, demuxer_info: &DemuxerInfo) {
    extern "C" {
        fn gst_rs_demuxer_register(plugin: *const c_void,
                                   name: *const c_char,
                                   long_name: *const c_char,
                                   description: *const c_char,
                                   classification: *const c_char,
                                   author: *const c_char,
                                   rank: i32,
                                   create_instance: *const c_void,
                                   input_format: *const c_char,
                                   output_formats: *const c_char)
                                   -> GBoolean;
    }

    let cname = CString::new(demuxer_info.name).unwrap();
    let clong_name = CString::new(demuxer_info.long_name).unwrap();
    let cdescription = CString::new(demuxer_info.description).unwrap();
    let cclassification = CString::new(demuxer_info.classification).unwrap();
    let cauthor = CString::new(demuxer_info.author).unwrap();
    let cinput_format = CString::new(demuxer_info.input_formats).unwrap();
    let coutput_formats = CString::new(demuxer_info.output_formats).unwrap();

    unsafe {
        gst_rs_demuxer_register(plugin.as_ptr(),
                                cname.as_ptr(),
                                clong_name.as_ptr(),
                                cdescription.as_ptr(),
                                cclassification.as_ptr(),
                                cauthor.as_ptr(),
                                demuxer_info.rank,
                                demuxer_info.create_instance as *const c_void,
                                cinput_format.as_ptr(),
                                coutput_formats.as_ptr());
    }
}
