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
use std::marker::PhantomData;
use std::u64;
use std::usize;
use std::fmt::{Display, Formatter};
use std::fmt::Error as FmtError;
use std::error::Error;
use std::ops::{Deref, DerefMut};

use utils::*;

#[derive(Debug)]
pub struct Buffer {
    raw: *mut c_void,
    owned: bool,
}

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
    buffer: Buffer,
    map_info: GstMapInfo,
}

#[derive(Debug)]
pub struct ReadWriteMappedBuffer {
    buffer: Buffer,
    map_info: GstMapInfo,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BufferError {
    NotWritable,
    NotEnoughSpace,
    Unknown,
}

impl Display for BufferError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        f.write_str(self.description())
    }
}

impl Error for BufferError {
    fn description(&self) -> &str {
        match *self {
            BufferError::NotWritable => "Not Writable",
            BufferError::NotEnoughSpace => "Not Enough Space",
            BufferError::Unknown => "Unknown",
        }
    }
}

impl Buffer {
    pub unsafe fn new_from_ptr(raw: *mut c_void) -> Buffer {
        extern "C" {
            fn gst_mini_object_ref(obj: *mut c_void) -> *mut c_void;
        };
        Buffer {
            raw: gst_mini_object_ref(raw),
            owned: true,
        }
    }

    pub unsafe fn new_from_ptr_owned(raw: *mut c_void) -> Buffer {
        Buffer {
            raw: raw,
            owned: true,
        }
    }

    unsafe fn new_from_ptr_scoped(raw: *mut c_void) -> Buffer {
        Buffer {
            raw: raw,
            owned: false,
        }
    }

    pub fn new() -> Buffer {
        extern "C" {
            fn gst_buffer_new() -> *mut c_void;
        }

        let raw = unsafe { gst_buffer_new() };
        Buffer {
            raw: raw,
            owned: true,
        }
    }

    pub fn new_with_size(size: usize) -> Option<Buffer> {
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
            Some(Buffer {
                raw: raw,
                owned: true,
            })
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
            unsafe { gst_buffer_map(self.raw, &mut map_info as *mut GstMapInfo, MAP_FLAG_READ) };
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
        let res = unsafe {
            gst_buffer_map(self.raw,
                           &mut map_info as *mut GstMapInfo,
                           MAP_FLAG_READWRITE)
        };
        if res.to_bool() {
            Some(ReadWriteBufferMap {
                buffer: self,
                map_info: map_info,
            })
        } else {
            None
        }
    }

    pub fn into_read_mapped_buffer(self) -> Option<ReadMappedBuffer> {
        extern "C" {
            fn gst_buffer_map(buffer: *mut c_void,
                              map: *mut GstMapInfo,
                              flags: MapFlags)
                              -> GBoolean;
        }

        let mut map_info: GstMapInfo = unsafe { mem::zeroed() };
        let res =
            unsafe { gst_buffer_map(self.raw, &mut map_info as *mut GstMapInfo, MAP_FLAG_READ) };
        if res.to_bool() {
            Some(ReadMappedBuffer {
                buffer: self,
                map_info: map_info,
            })
        } else {
            None
        }
    }

    pub fn into_readwrite_mapped_buffer(self) -> Option<ReadWriteMappedBuffer> {
        extern "C" {
            fn gst_buffer_map(buffer: *mut c_void,
                              map: *mut GstMapInfo,
                              flags: MapFlags)
                              -> GBoolean;
        }

        let mut map_info: GstMapInfo = unsafe { mem::zeroed() };
        let res = unsafe {
            gst_buffer_map(self.raw,
                           &mut map_info as *mut GstMapInfo,
                           MAP_FLAG_READWRITE)
        };
        if res.to_bool() {
            Some(ReadWriteMappedBuffer {
                buffer: self,
                map_info: map_info,
            })
        } else {
            None
        }
    }

    pub fn is_writable(&self) -> bool {
        extern "C" {
            fn gst_mini_object_is_writable(obj: *const c_void) -> GBoolean;
        }

        let res = unsafe { gst_mini_object_is_writable(self.raw) };

        res.to_bool()
    }

    pub fn make_writable(self: Buffer) -> Buffer {
        extern "C" {
            fn gst_mini_object_make_writable(obj: *mut c_void) -> *mut c_void;
        }

        let raw = unsafe { gst_mini_object_make_writable(self.raw) };

        Buffer {
            raw: raw,
            owned: true,
        }
    }

    pub fn copy(&self) -> Buffer {
        extern "C" {
            fn gst_mini_object_copy(obj: *const c_void) -> *mut c_void;
        }
        unsafe { Buffer::new_from_ptr_owned(gst_mini_object_copy(self.raw)) }
    }

    pub fn append(self, other: Buffer) -> Buffer {
        extern "C" {
            fn gst_buffer_append(buf1: *mut c_void, buf2: *mut c_void) -> *mut c_void;
        }

        let raw = unsafe { gst_buffer_append(self.raw, other.raw) };

        Buffer {
            raw: raw,
            owned: true,
        }
    }

    pub fn copy_region(&self, offset: usize, size: Option<usize>) -> Option<Buffer> {
        extern "C" {
            fn gst_buffer_copy_region(buf: *mut c_void,
                                      flags: BufferCopyFlags,
                                      offset: usize,
                                      size: usize)
                                      -> *mut c_void;
        }

        let size_real = size.unwrap_or(usize::MAX);

        let raw =
            unsafe { gst_buffer_copy_region(self.raw, BUFFER_COPY_FLAG_ALL, offset, size_real) };

        if raw.is_null() {
            None
        } else {
            Some(Buffer {
                raw: raw,
                owned: true,
            })
        }
    }

    pub fn copy_from_slice(&mut self, offset: usize, slice: &[u8]) -> Result<(), BufferError> {
        if !self.is_writable() {
            return Err(BufferError::NotWritable);
        }

        let maxsize = self.get_maxsize();
        let size = slice.len();
        if maxsize < offset || maxsize - offset < size {
            return Err(BufferError::NotEnoughSpace);
        }

        extern "C" {
            fn gst_buffer_fill(buffer: *mut c_void,
                               offset: usize,
                               src: *const u8,
                               size: usize)
                               -> usize;
        };

        let copied = unsafe {
            let src = slice.as_ptr();
            gst_buffer_fill(self.raw, offset, src, size)
        };

        if copied < size {
            Err(BufferError::Unknown)
        } else {
            Ok(())
        }
    }

    pub fn copy_to_slice(&self, offset: usize, slice: &mut [u8]) -> Result<(), BufferError> {
        let maxsize = self.get_size();
        let size = slice.len();
        if maxsize < offset || maxsize - offset < size {
            return Err(BufferError::NotEnoughSpace);
        }

        extern "C" {
            fn gst_buffer_extract(buffer: *mut c_void,
                                  offset: usize,
                                  src: *mut u8,
                                  size: usize)
                                  -> usize;
        };

        let copied = unsafe {
            let src = slice.as_mut_ptr();
            gst_buffer_extract(self.raw, offset, src, size)
        };

        if copied < size {
            Err(BufferError::Unknown)
        } else {
            Ok(())
        }
    }

    pub fn get_size(&self) -> usize {
        extern "C" {
            fn gst_buffer_get_size(obj: *const c_void) -> usize;
        }

        unsafe { gst_buffer_get_size(self.raw) }
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
            gst_buffer_get_sizes_range(self.raw,
                                       0,
                                       -1,
                                       ptr::null_mut(),
                                       &mut maxsize as *mut usize);
        };

        maxsize
    }

    pub fn set_size(&mut self, size: usize) -> Result<(), BufferError> {
        extern "C" {
            fn gst_buffer_set_size(obj: *const c_void, size: usize);
        }

        if !self.is_writable() {
            return Err(BufferError::NotWritable);
        }

        if self.get_maxsize() < size {
            return Err(BufferError::NotEnoughSpace);
        }

        unsafe {
            gst_buffer_set_size(self.raw, size);
        }

        Ok(())
    }

    pub fn get_offset(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_offset(buf: *const c_void) -> u64;
        }

        let offset = unsafe { gst_rs_buffer_get_offset(self.raw) };

        if offset == u64::MAX {
            None
        } else {
            Some(offset)
        }
    }

    pub fn set_offset(&mut self, offset: Option<u64>) -> Result<(), BufferError> {
        if !self.is_writable() {
            return Err(BufferError::NotWritable);
        }

        extern "C" {
            fn gst_rs_buffer_set_offset(buf: *const c_void, offset: u64);
        }

        let offset = match offset {
            None => u64::MAX,
            Some(offset) => offset,
        };

        unsafe {
            gst_rs_buffer_set_offset(self.raw, offset);
        }

        Ok(())
    }

    pub fn get_offset_end(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_offset_end(buf: *const c_void) -> u64;
        }

        let offset_end = unsafe { gst_rs_buffer_get_offset_end(self.raw) };

        if offset_end == u64::MAX {
            None
        } else {
            Some(offset_end)
        }
    }

    pub fn set_offset_end(&mut self, offset_end: Option<u64>) -> Result<(), BufferError> {
        if !self.is_writable() {
            return Err(BufferError::NotWritable);
        }

        extern "C" {
            fn gst_rs_buffer_set_offset_end(buf: *const c_void, offset_end: u64);
        }

        let offset_end = match offset_end {
            None => u64::MAX,
            Some(offset_end) => offset_end,
        };

        unsafe {
            gst_rs_buffer_set_offset_end(self.raw, offset_end);
        }

        Ok(())
    }

    pub fn get_pts(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_pts(buf: *const c_void) -> u64;
        }

        let pts = unsafe { gst_rs_buffer_get_pts(self.raw) };

        if pts == u64::MAX { None } else { Some(pts) }
    }

    pub fn set_pts(&mut self, pts: Option<u64>) -> Result<(), BufferError> {
        if !self.is_writable() {
            return Err(BufferError::NotWritable);
        }

        extern "C" {
            fn gst_rs_buffer_set_pts(buf: *const c_void, pts: u64);
        }

        let pts = match pts {
            None => u64::MAX,
            Some(pts) => pts,
        };

        unsafe {
            gst_rs_buffer_set_pts(self.raw, pts);
        }

        Ok(())
    }

    pub fn get_dts(&self) -> Option<u64> {
        extern "C" {
            fn gst_rs_buffer_get_dts(buf: *const c_void) -> u64;
        }

        let dts = unsafe { gst_rs_buffer_get_dts(self.raw) };

        if dts == u64::MAX { None } else { Some(dts) }
    }

    pub fn set_dts(&mut self, dts: Option<u64>) -> Result<(), BufferError> {
        if !self.is_writable() {
            return Err(BufferError::NotWritable);
        }

        extern "C" {
            fn gst_rs_buffer_set_dts(buf: *const c_void, dts: u64);
        }

        let dts = match dts {
            None => u64::MAX,
            Some(dts) => dts,
        };

        unsafe {
            gst_rs_buffer_set_dts(self.raw, dts);
        }

        Ok(())
    }

    pub fn get_flags(&self) -> BufferFlags {
        extern "C" {
            fn gst_rs_buffer_get_flags(buf: *const c_void) -> BufferFlags;
        }

        unsafe { gst_rs_buffer_get_flags(self.raw) }
    }

    pub fn set_flags(&mut self, flags: BufferFlags) -> Result<(), BufferError> {
        if !self.is_writable() {
            return Err(BufferError::NotWritable);
        }

        extern "C" {
            fn gst_rs_buffer_set_flags(buf: *const c_void, flags: BufferFlags);
        }

        unsafe {
            gst_rs_buffer_set_flags(self.raw, flags);
        }

        Ok(())
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        extern "C" {
            fn gst_mini_object_unref(obj: *mut c_void);
        }

        if self.owned {
            unsafe { gst_mini_object_unref(self.raw) }
        }
    }
}

unsafe impl Sync for Buffer {}
unsafe impl Send for Buffer {}

impl Clone for Buffer {
    fn clone(&self) -> Buffer {
        extern "C" {
            fn gst_mini_object_ref(obj: *mut c_void) -> *mut c_void;
        }

        let raw = unsafe { gst_mini_object_ref(self.raw) };

        Buffer {
            raw: raw,
            owned: true,
        }
    }
}

impl<'a> ReadBufferMap<'a> {
    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.map_info.data as *const u8, self.map_info.size) }
    }

    pub fn get_size(&self) -> usize {
        self.map_info.size
    }
}

impl<'a> Drop for ReadBufferMap<'a> {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        unsafe {
            gst_buffer_unmap(self.buffer.raw, &mut self.map_info as *mut GstMapInfo);
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
}

impl<'a> Drop for ReadWriteBufferMap<'a> {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        unsafe {
            gst_buffer_unmap(self.buffer.raw, &mut self.map_info as *mut GstMapInfo);
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
        &self.buffer
    }
}

impl Drop for ReadMappedBuffer {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        unsafe {
            gst_buffer_unmap(self.buffer.raw, &mut self.map_info as *mut GstMapInfo);
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
        &self.buffer
    }
}

impl Drop for ReadWriteMappedBuffer {
    fn drop(&mut self) {
        extern "C" {
            fn gst_buffer_unmap(buffer: *mut c_void, map: *mut GstMapInfo);
        };

        unsafe {
            gst_buffer_unmap(self.buffer.raw, &mut self.map_info as *mut GstMapInfo);
        }
    }
}

unsafe impl Sync for ReadWriteMappedBuffer {}
unsafe impl Send for ReadWriteMappedBuffer {}

#[repr(C)]
pub struct ScopedBufferPtr(*mut c_void);

pub struct ScopedBuffer<'a> {
    buffer: Buffer,
    #[allow(dead_code)]
    phantom: PhantomData<&'a ScopedBufferPtr>,
}

impl<'a> ScopedBuffer<'a> {
    pub unsafe fn new(ptr: &'a ScopedBufferPtr) -> ScopedBuffer<'a> {
        ScopedBuffer {
            buffer: Buffer::new_from_ptr_scoped(ptr.0),
            phantom: PhantomData,
        }
    }
}

impl<'a> Deref for ScopedBuffer<'a> {
    type Target = Buffer;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl<'a> DerefMut for ScopedBuffer<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

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
