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
//

use std::ptr;
use std::mem;
use std::os::raw::c_void;
use std::slice;
use std::u64;
use std::usize;

use utils::*;
use miniobject::*;

#[derive(Debug)]
pub struct Buffer(*mut c_void);

#[repr(C)]
#[derive(Debug)]
struct GstMapInfo {
    memory: *mut c_void,
    flags: i32,
    data: *mut c_void,
    size: usize,
    maxsize: usize,
    user_data: [*mut c_void; 4],
    _gst_reserved: [*const c_void; 4],
}

#[derive(Debug)]
pub struct ReadBufferMap<'a> {
    buffer: &'a Buffer,
    map_info: GstMapInfo,
}

#[derive(Debug)]
pub struct ReadWriteBufferMap<'a> {
    buffer: &'a Buffer,
    map_info: GstMapInfo,
}

#[derive(Debug)]
pub struct ReadMappedBuffer {
    buffer: GstRc<Buffer>,
    map_info: GstMapInfo,
}

#[derive(Debug)]
pub struct ReadWriteMappedBuffer {
    buffer: GstRc<Buffer>,
    map_info: GstMapInfo,
}

unsafe impl MiniObject for Buffer {
    unsafe fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    unsafe fn replace_ptr(&mut self, ptr: *mut c_void) {
        self.0 = ptr
    }

    unsafe fn new_from_ptr(ptr: *mut c_void) -> Self {
        Buffer(ptr)
    }
}

impl Buffer {
    pub fn new() -> GstRc<Buffer> {
        extern "C" {
            fn gst_buffer_new() -> *mut c_void;
        }

        unsafe { GstRc::new_from_owned_ptr(gst_buffer_new()) }
    }

    pub fn new_with_size(size: usize) -> Option<GstRc<Buffer>> {
        extern "C" {
            fn gst_buffer_new_allocate(allocator: *const c_void,
                                       size: usize,
                                       params: *const c_void)
                                       -> *mut c_void;
        }

        let raw = unsafe { gst_buffer_new_allocate(ptr::null(), size, ptr::null()) };
        if raw.is_null() {
            None
        } else {
            Some(unsafe { GstRc::new_from_owned_ptr(raw) })
        }
    }

    extern "C" fn vec_drop(vec: *mut c_void) {
        let vec: Box<Vec<u8>> = unsafe { Box::from_raw(vec as *mut Vec<u8>) };
        drop(vec);
    }

    pub fn new_from_vec(vec: Vec<u8>) -> Option<GstRc<Buffer>> {
        extern "C" {
            fn gst_buffer_new_wrapped_full(flags: u32,
                                           data: *mut u8,
                                           maxsize: usize,
                                           offset: usize,
                                           size: usize,
                                           user_data: *mut c_void,
                                           destroy_notify: extern "C" fn(*mut c_void))
                                           -> *mut c_void;
        }

        let raw = unsafe {
            let mut vec = Box::new(vec);
            let maxsize = vec.capacity();
            let size = vec.len();
            let data = vec.as_mut_ptr();
            let user_data = Box::into_raw(vec);
            gst_buffer_new_wrapped_full(0,
                                        data,
                                        maxsize,
                                        0,
                                        size,
                                        user_data as *mut c_void,
                                        Buffer::vec_drop)
        };

        if raw.is_null() {
            None
        } else {
            Some(unsafe { GstRc::new_from_owned_ptr(raw) })
        }
    }

    pub fn map_read(&self) -> Option<ReadBufferMap> {
        extern "C" {
            fn gst_buffer_map(buffer: *mut c_void,
                              map: *mut GstMapInfo,
                              flags: MapFlags)
                              -> GBoolean;
        }

        let mut map_info: GstMapInfo = unsafe { mem::zeroed() };
        let res =
            unsafe { gst_buffer_map(self.0, &mut map_info as *mut GstMapInfo, MAP_FLAG_READ) };
        if res.to_bool() {
            Some(ReadBufferMap {
                buffer: self,
                map_info: map_info,
            })
        } else {
            None
        }
    }

    pub fn map_readwrite(&mut self) -> Option<ReadWriteBufferMap> {
        extern "C" {
            fn gst_buffer_map(buffer: *mut c_void,
                              map: *mut GstMapInfo,
                              flags: MapFlags)
                              -> GBoolean;
        }

        let mut map_info: GstMapInfo = unsafe { mem::zeroed() };
        let res =
            unsafe { gst_buffer_map(self.0, &mut map_info as *mut GstMapInfo, MAP_FLAG_READWRITE) };
        if res.to_bool() {
            Some(ReadWriteBufferMap {
                buffer: self,
                map_info: map_info,
            })
        } else {
            None
        }
    }

    pub fn into_read_mapped_buffer(buffer: GstRc<Buffer>) -> Option<ReadMappedBuffer> {
        extern "C" {
            fn gst_buffer_map(buffer: *mut c_void,
                              map: *mut GstMapInfo,
                              flags: MapFlags)
                              -> GBoolean;
        }

        let mut map_info: GstMapInfo = unsafe { mem::zeroed() };
        let res =
            unsafe { gst_buffer_map(buffer.0, &mut map_info as *mut GstMapInfo, MAP_FLAG_READ) };
        if res.to_bool() {
            Some(ReadMappedBuffer {
                buffer: buffer,
                map_info: map_info,
            })
        } else {
            None
        }
    }

    pub fn into_readwrite_mapped_buffer(buffer: GstRc<Buffer>) -> Option<ReadWriteMappedBuffer> {
        extern "C" {
            fn gst_buffer_map(buffer: *mut c_void,
                              map: *mut GstMapInfo,
                              flags: MapFlags)
                              -> GBoolean;
        }

        let mut map_info: GstMapInfo = unsafe { mem::zeroed() };
        let res = unsafe {
            gst_buffer_map(buffer.0,
                           &mut map_info as *mut GstMapInfo,
                           MAP_FLAG_READWRITE)
        };
        if res.to_bool() {
            Some(ReadWriteMappedBuffer {
                buffer: buffer,
                map_info: map_info,
            })
        } else {
            None
        }
    }

    pub fn append(buffer: GstRc<Buffer>, other: GstRc<Buffer>) -> GstRc<Buffer> {
        extern "C" {
            fn gst_buffer_append(buf1: *mut c_void, buf2: *mut c_void) -> *mut c_void;
        }

        unsafe { GstRc::new_from_owned_ptr(gst_buffer_append(buffer.into_ptr(), other.into_ptr())) }
    }

    pub fn copy_region(&self, offset: usize, size: Option<usize>) -> Option<GstRc<Buffer>> {
        extern "C" {
            fn gst_buffer_copy_region(buf: *mut c_void,
                                      flags: BufferCopyFlags,
                                      offset: usize,
                                      size: usize)
                                      -> *mut c_void;
        }

        let size_real = size.unwrap_or(usize::MAX);

        let raw =
            unsafe { gst_buffer_copy_region(self.0, BUFFER_COPY_FLAG_ALL, offset, size_real) };

        if raw.is_null() {
            None
        } else {
            Some(unsafe { GstRc::new_from_owned_ptr(raw) })
        }
    }

    pub fn copy_from_slice(&mut self, offset: usize, slice: &[u8]) -> Result<(), usize> {
        let maxsize = self.get_maxsize();
        let size = slice.len();

        assert!(maxsize >= offset && maxsize - offset >= size);

        extern "C" {
            fn gst_buffer_fill(buffer: *mut c_void,
                               offset: usize,
                               src: *const u8,
                               size: usize)
                               -> usize;
        };

        let copied = unsafe {
            let src = slice.as_ptr();
            gst_buffer_fill(self.0, offset, src, size)
        };

        if copied == size { Ok(()) } else { Err(copied) }
    }

    pub fn copy_to_slice(&self, offset: usize, slice: &mut [u8]) -> Result<(), usize> {
        let maxsize = self.get_size();
        let size = slice.len();

        assert!(maxsize >= offset && maxsize - offset >= size);

        extern "C" {
            fn gst_buffer_extract(buffer: *mut c_void,
                                  offset: usize,
                                  src: *mut u8,
                                  size: usize)
                                  -> usize;
        };

        let copied = unsafe {
            let src = slice.as_mut_ptr();
            gst_buffer_extract(self.0, offset, src, size)
        };

        if copied == size { Ok(()) } else { Err(copied) }
    }

    pub fn get_size(&self) -> usize {
        extern "C" {
            fn gst_buffer_get_size(obj: *const c_void) -> usize;
        }

        unsafe { gst_buffer_get_size(self.0) }
    }

    pub fn get_maxsize(&self) -> usize {
        extern "C" {
            fn gst_buffer_get_sizes_range(obj: *const c_void,
                                          idx: u32,
                                          length: i32,
                                          offset: *mut usize,
                                          maxsize: *mut usize)
                                          -> usize;
        }

        let mut maxsize: usize = 0;

        unsafe {
            gst_buffer_get_sizes_range(self.0, 0, -1, ptr::null_mut(), &mut maxsize as *mut usize);
        };

        maxsize
    }

    pub fn set_size(&mut self, size: usize) {
        extern "C" {
            fn gst_buffer_set_size(obj: *const c_void, size: usize);
        }

        assert!(self.get_maxsize() >= size);

        unsafe {
            gst_buffer_set_size(self.0, size);
        }
    }

    pub fn get_offset(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_offset(buf: *const c_void) -> u64;
        }

        let offset = unsafe { gst_rs_buffer_get_offset(self.0) };

        if offset == u64::MAX {
            None
        } else {
            Some(offset)
        }
    }

    pub fn set_offset(&mut self, offset: Option<u64>) {
        extern "C" {
            fn gst_rs_buffer_set_offset(buf: *const c_void, offset: u64);
        }

        let offset = offset.unwrap_or(u64::MAX);

        unsafe {
            gst_rs_buffer_set_offset(self.0, offset);
        }
    }

    pub fn get_offset_end(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_offset_end(buf: *const c_void) -> u64;
        }

        let offset_end = unsafe { gst_rs_buffer_get_offset_end(self.0) };

        if offset_end == u64::MAX {
            None
        } else {
            Some(offset_end)
        }
    }

    pub fn set_offset_end(&mut self, offset_end: Option<u64>) {
        extern "C" {
            fn gst_rs_buffer_set_offset_end(buf: *const c_void, offset_end: u64);
        }

        let offset_end = offset_end.unwrap_or(u64::MAX);

        unsafe {
            gst_rs_buffer_set_offset_end(self.0, offset_end);
        }
    }

    pub fn get_pts(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_pts(buf: *const c_void) -> u64;
        }

        let pts = unsafe { gst_rs_buffer_get_pts(self.0) };

        if pts == u64::MAX { None } else { Some(pts) }
    }

    pub fn set_pts(&mut self, pts: Option<u64>) {
        extern "C" {
            fn gst_rs_buffer_set_pts(buf: *const c_void, pts: u64);
        }

        let pts = pts.unwrap_or(u64::MAX);

        unsafe {
            gst_rs_buffer_set_pts(self.0, pts);
        }
    }

    pub fn get_dts(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_dts(buf: *const c_void) -> u64;
        }

        let dts = unsafe { gst_rs_buffer_get_dts(self.0) };

        if dts == u64::MAX { None } else { Some(dts) }
    }

    pub fn set_dts(&mut self, dts: Option<u64>) {
        extern "C" {
            fn gst_rs_buffer_set_dts(buf: *const c_void, dts: u64);
        }

        let dts = dts.unwrap_or(u64::MAX);

        unsafe {
            gst_rs_buffer_set_dts(self.0, dts);
        }
    }

    pub fn get_duration(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_duration(buf: *const c_void) -> u64;
        }

        let duration = unsafe { gst_rs_buffer_get_duration(self.0) };

        if duration == u64::MAX {
            None
        } else {
            Some(duration)
        }
    }

    pub fn set_duration(&mut self, duration: Option<u64>) {
        extern "C" {
            fn gst_rs_buffer_set_duration(buf: *const c_void, duration: u64);
        }

        let duration = duration.unwrap_or(u64::MAX);

        unsafe {
            gst_rs_buffer_set_duration(self.0, duration);
        }
    }

    pub fn get_flags(&self) -> BufferFlags {
        extern "C" {
            fn gst_rs_buffer_get_flags(buf: *const c_void) -> BufferFlags;
        }

        unsafe { gst_rs_buffer_get_flags(self.0) }
    }

    pub fn set_flags(&mut self, flags: BufferFlags) {
        extern "C" {
            fn gst_rs_buffer_set_flags(buf: *const c_void, flags: BufferFlags);
        }

        unsafe {
            gst_rs_buffer_set_flags(self.0, flags);
        }
    }
}

unsafe impl Sync for Buffer {}
unsafe impl Send for Buffer {}

impl PartialEq for Buffer {
    fn eq(&self, other: &Buffer) -> bool {
        if self.get_size() != other.get_size() {
            return false;
        }

        let self_map = self.map_read();
        let other_map = other.map_read();

        match (self_map, other_map) {
            (Some(self_map), Some(other_map)) => self_map.as_slice().eq(other_map.as_slice()),
            _ => false,
        }
    }
}

impl Eq for Buffer {}

impl<'a> ReadBufferMap<'a> {
    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.map_info.data as *const u8, self.map_info.size) }
    }

    pub fn get_size(&self) -> usize {
        self.map_info.size
    }

    pub fn get_buffer(&self) -> &Buffer {
        self.buffer
    }
}

impl<'a> Drop for ReadBufferMap<'a> {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        unsafe {
            gst_buffer_unmap(self.buffer.0, &mut self.map_info as *mut GstMapInfo);
        }
    }
}

impl<'a> ReadWriteBufferMap<'a> {
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.map_info.data as *mut u8, self.map_info.size) }
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.map_info.data as *const u8, self.map_info.size) }
    }

    pub fn get_size(&self) -> usize {
        self.map_info.size
    }

    pub fn get_buffer(&self) -> &Buffer {
        self.buffer
    }
}

impl<'a> Drop for ReadWriteBufferMap<'a> {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        unsafe {
            gst_buffer_unmap(self.buffer.0, &mut self.map_info as *mut GstMapInfo);
        }
    }
}

impl ReadMappedBuffer {
    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.map_info.data as *const u8, self.map_info.size) }
    }

    pub fn get_size(&self) -> usize {
        self.map_info.size
    }

    pub fn get_buffer(&self) -> &Buffer {
        self.buffer.as_ref()
    }
}

impl Drop for ReadMappedBuffer {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        if !self.buffer.0.is_null() {
            unsafe {
                gst_buffer_unmap(self.buffer.0, &mut self.map_info as *mut GstMapInfo);
            }
        }
    }
}

unsafe impl Sync for ReadMappedBuffer {}
unsafe impl Send for ReadMappedBuffer {}

impl ReadWriteMappedBuffer {
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.map_info.data as *mut u8, self.map_info.size) }
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.map_info.data as *const u8, self.map_info.size) }
    }

    pub fn get_size(&self) -> usize {
        self.map_info.size
    }

    pub fn get_buffer(&self) -> &Buffer {
        self.buffer.as_ref()
    }
}

impl Drop for ReadWriteMappedBuffer {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        if !self.buffer.0.is_null() {
            unsafe {
                gst_buffer_unmap(self.buffer.0, &mut self.map_info as *mut GstMapInfo);
            }
        }
    }
}

unsafe impl Sync for ReadWriteMappedBuffer {}
unsafe impl Send for ReadWriteMappedBuffer {}

bitflags! {
    #[repr(C)]
    flags MapFlags: u32 {
        const MAP_FLAG_READ  = 0b00000001,
        const MAP_FLAG_WRITE = 0b00000010,
        const MAP_FLAG_READWRITE = MAP_FLAG_READ.bits
                           | MAP_FLAG_WRITE.bits,
    }
}

bitflags! {
    #[repr(C)]
    pub flags BufferFlags: u32 {
        const BUFFER_FLAG_LIVE         = 0b0000000000010000,
        const BUFFER_FLAG_DECODE_ONLY  = 0b0000000000100000,
        const BUFFER_FLAG_DISCONT      = 0b0000000001000000,
        const BUFFER_FLAG_RESYNC       = 0b0000000010000000,
        const BUFFER_FLAG_CORRUPTED    = 0b0000000100000000,
        const BUFFER_FLAG_MARKER       = 0b0000001000000000,
        const BUFFER_FLAG_HEADER       = 0b0000010000000000,
        const BUFFER_FLAG_GAP          = 0b0000100000000000,
        const BUFFER_FLAG_DROPPABLE    = 0b0001000000000000,
        const BUFFER_FLAG_DELTA_UNIT   = 0b0010000000000000,
        const BUFFER_FLAG_TAG_MEMORY   = 0b0100000000000000,
        const BUFFER_FLAG_SYNC_AFTER   = 0b1000000000000000,
    }
}

bitflags! {
    #[repr(C)]
    flags BufferCopyFlags: u32 {
        const BUFFER_COPY_FLAG_FLAGS       = 0b00000001,
        const BUFFER_COPY_FLAG_TIMESTAMPS  = 0b00000010,
        const BUFFER_COPY_FLAG_META        = 0b00000100,
        const BUFFER_COPY_FLAG_MEMORY      = 0b00001000,
        const BUFFER_COPY_FLAG_MERGE       = 0b00010000,
        const BUFFER_COPY_FLAG_DEEP        = 0b00100000,
        const BUFFER_COPY_FLAG_METADATA = BUFFER_COPY_FLAG_FLAGS.bits
                           | BUFFER_COPY_FLAG_TIMESTAMPS.bits
                           | BUFFER_COPY_FLAG_META.bits,
        const BUFFER_COPY_FLAG_ALL = BUFFER_COPY_FLAG_METADATA.bits
                           | BUFFER_COPY_FLAG_MEMORY.bits,
    }
}
