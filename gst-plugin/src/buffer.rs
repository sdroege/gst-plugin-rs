// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ptr;
use std::mem;
use std::fmt;
use std::slice;
use std::u64;
use std::usize;

use miniobject::*;

use glib;
use gst;

pub struct Buffer(gst::GstBuffer);

#[derive(Derivative)]
#[derivative(Debug)]
pub struct ReadBufferMap<'a> {
    buffer: &'a Buffer,
    #[derivative(Debug="ignore")]
    map_info: gst::GstMapInfo,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct ReadWriteBufferMap<'a> {
    buffer: &'a Buffer,
    #[derivative(Debug="ignore")]
    map_info: gst::GstMapInfo,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct ReadMappedBuffer {
    buffer: GstRc<Buffer>,
    #[derivative(Debug="ignore")]
    map_info: gst::GstMapInfo,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct ReadWriteMappedBuffer {
    buffer: GstRc<Buffer>,
    #[derivative(Debug="ignore")]
    map_info: gst::GstMapInfo,
}

unsafe impl MiniObject for Buffer {
    type PtrType = gst::GstBuffer;
}

impl Buffer {
    pub fn new() -> GstRc<Buffer> {
        unsafe { GstRc::from_owned_ptr(gst::gst_buffer_new()) }
    }

    pub fn new_with_size(size: usize) -> Option<GstRc<Buffer>> {
        let raw = unsafe { gst::gst_buffer_new_allocate(ptr::null_mut(), size, ptr::null_mut()) };
        if raw.is_null() {
            None
        } else {
            Some(unsafe { GstRc::from_owned_ptr(raw) })
        }
    }

    unsafe extern "C" fn vec_drop(vec: glib::gpointer) {
        let vec: Box<Vec<u8>> = Box::from_raw(vec as *mut Vec<u8>);
        drop(vec);
    }

    pub fn from_vec(vec: Vec<u8>) -> Option<GstRc<Buffer>> {
        let raw = unsafe {
            let mut vec = Box::new(vec);
            let maxsize = vec.capacity();
            let size = vec.len();
            let data = vec.as_mut_ptr();
            let user_data = Box::into_raw(vec);
            gst::gst_buffer_new_wrapped_full(gst::GstMemoryFlags::empty(),
                                             data as glib::gpointer,
                                             maxsize,
                                             0,
                                             size,
                                             user_data as glib::gpointer,
                                             Some(Buffer::vec_drop))
        };

        if raw.is_null() {
            None
        } else {
            Some(unsafe { GstRc::from_owned_ptr(raw) })
        }
    }

    pub fn map_read(&self) -> Option<ReadBufferMap> {
        let mut map_info: gst::GstMapInfo = unsafe { mem::zeroed() };
        let res = unsafe {
            gst::gst_buffer_map(self.as_mut_ptr() as *mut gst::GstBuffer,
                                &mut map_info,
                                gst::GST_MAP_READ)
        };
        if res == glib::GTRUE {
            Some(ReadBufferMap {
                     buffer: self,
                     map_info: map_info,
                 })
        } else {
            None
        }
    }

    pub fn map_readwrite(&mut self) -> Option<ReadWriteBufferMap> {
        let mut map_info: gst::GstMapInfo = unsafe { mem::zeroed() };
        let res = unsafe {
            gst::gst_buffer_map(self.as_mut_ptr(), &mut map_info, gst::GST_MAP_READWRITE)
        };
        if res == glib::GTRUE {
            Some(ReadWriteBufferMap {
                     buffer: self,
                     map_info: map_info,
                 })
        } else {
            None
        }
    }

    pub fn into_read_mapped_buffer(buffer: GstRc<Buffer>) -> Option<ReadMappedBuffer> {
        let mut map_info: gst::GstMapInfo = unsafe { mem::zeroed() };
        let res =
            unsafe { gst::gst_buffer_map(buffer.as_mut_ptr(), &mut map_info, gst::GST_MAP_READ) };
        if res == glib::GTRUE {
            Some(ReadMappedBuffer {
                     buffer: buffer,
                     map_info: map_info,
                 })
        } else {
            None
        }
    }

    pub fn into_readwrite_mapped_buffer(buffer: GstRc<Buffer>) -> Option<ReadWriteMappedBuffer> {
        let mut map_info: gst::GstMapInfo = unsafe { mem::zeroed() };
        let res = unsafe {
            gst::gst_buffer_map(buffer.as_mut_ptr(), &mut map_info, gst::GST_MAP_READWRITE)
        };
        if res == glib::GTRUE {
            Some(ReadWriteMappedBuffer {
                     buffer: buffer,
                     map_info: map_info,
                 })
        } else {
            None
        }
    }

    pub fn append(buffer: GstRc<Buffer>, other: GstRc<Buffer>) -> GstRc<Buffer> {
        unsafe {
            GstRc::from_owned_ptr(gst::gst_buffer_append(buffer.into_ptr() as *mut gst::GstBuffer,
                                                         other.into_ptr() as *mut gst::GstBuffer))
        }
    }

    pub fn copy_region(&self, offset: usize, size: Option<usize>) -> Option<GstRc<Buffer>> {
        let size_real = size.unwrap_or(usize::MAX);

        let raw = unsafe {
            gst::gst_buffer_copy_region(self.as_mut_ptr(),
                                        gst::GST_BUFFER_COPY_ALL,
                                        offset,
                                        size_real)
        };

        if raw.is_null() {
            None
        } else {
            Some(unsafe { GstRc::from_owned_ptr(raw) })
        }
    }

    pub fn copy_from_slice(&mut self, offset: usize, slice: &[u8]) -> Result<(), usize> {
        let maxsize = self.get_maxsize();
        let size = slice.len();

        assert!(maxsize >= offset && maxsize - offset >= size);

        let copied = unsafe {
            let src = slice.as_ptr();
            gst::gst_buffer_fill(self.as_mut_ptr(), offset, src as glib::gconstpointer, size)
        };

        if copied == size { Ok(()) } else { Err(copied) }
    }

    pub fn copy_to_slice(&self, offset: usize, slice: &mut [u8]) -> Result<(), usize> {
        let maxsize = self.get_size();
        let size = slice.len();

        assert!(maxsize >= offset && maxsize - offset >= size);

        let copied = unsafe {
            let dest = slice.as_mut_ptr();
            gst::gst_buffer_extract(self.as_mut_ptr(), offset, dest as glib::gpointer, size)
        };

        if copied == size { Ok(()) } else { Err(copied) }
    }

    pub fn get_size(&self) -> usize {
        unsafe { gst::gst_buffer_get_size(self.as_mut_ptr()) }
    }

    pub fn get_maxsize(&self) -> usize {
        let mut maxsize: usize = 0;

        unsafe {
            gst::gst_buffer_get_sizes_range(self.as_mut_ptr(),
                                            0,
                                            -1,
                                            ptr::null_mut(),
                                            &mut maxsize as *mut usize);
        };

        maxsize
    }

    pub fn set_size(&mut self, size: usize) {
        assert!(self.get_maxsize() >= size);

        unsafe {
            gst::gst_buffer_set_size(self.as_mut_ptr(), size as isize);
        }
    }

    pub fn get_offset(&self) -> Option<u64> {
        let offset = self.0.offset;

        if offset == u64::MAX {
            None
        } else {
            Some(offset)
        }
    }

    pub fn set_offset(&mut self, offset: Option<u64>) {
        let offset = offset.unwrap_or(u64::MAX);
        self.0.offset = offset;
    }

    pub fn get_offset_end(&self) -> Option<u64> {
        let offset_end = self.0.offset_end;

        if offset_end == u64::MAX {
            None
        } else {
            Some(offset_end)
        }
    }

    pub fn set_offset_end(&mut self, offset_end: Option<u64>) {
        let offset_end = offset_end.unwrap_or(u64::MAX);
        self.0.offset_end = offset_end;
    }

    pub fn get_pts(&self) -> Option<u64> {
        let pts = self.0.pts;

        if pts == u64::MAX { None } else { Some(pts) }
    }

    pub fn set_pts(&mut self, pts: Option<u64>) {
        let pts = pts.unwrap_or(u64::MAX);
        self.0.pts = pts;
    }

    pub fn get_dts(&self) -> Option<u64> {
        let dts = self.0.dts;

        if dts == u64::MAX { None } else { Some(dts) }
    }

    pub fn set_dts(&mut self, dts: Option<u64>) {
        let dts = dts.unwrap_or(u64::MAX);
        self.0.dts = dts;
    }

    pub fn get_duration(&self) -> Option<u64> {
        let duration = self.0.duration;

        if duration == u64::MAX {
            None
        } else {
            Some(duration)
        }
    }

    pub fn set_duration(&mut self, duration: Option<u64>) {
        let duration = duration.unwrap_or(u64::MAX);
        self.0.duration = duration;
    }

    pub fn get_flags(&self) -> BufferFlags {
        BufferFlags::from_bits_truncate(self.0.mini_object.flags)
    }

    pub fn set_flags(&mut self, flags: BufferFlags) {
        self.0.mini_object.flags = flags.bits();
    }
}

unsafe impl Sync for Buffer {}
unsafe impl Send for Buffer {}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", unsafe { self.as_ptr() })
    }
}

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

impl ToOwned for Buffer {
    type Owned = GstRc<Buffer>;

    fn to_owned(&self) -> GstRc<Buffer> {
        unsafe { GstRc::from_unowned_ptr(self.as_ptr()) }
    }
}

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
        unsafe {
            gst::gst_buffer_unmap(self.buffer.as_mut_ptr(), &mut self.map_info);
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
        unsafe {
            gst::gst_buffer_unmap(self.buffer.as_mut_ptr(), &mut self.map_info);
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
        unsafe {
            gst::gst_buffer_unmap(self.buffer.as_mut_ptr(), &mut self.map_info);
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
        unsafe {
            gst::gst_buffer_unmap(self.buffer.as_mut_ptr(), &mut self.map_info);
        }
    }
}

unsafe impl Sync for ReadWriteMappedBuffer {}
unsafe impl Send for ReadWriteMappedBuffer {}

// FIXME: Duplicate of gst::GstBufferFlags with nicer naming
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn init() {
        unsafe {
            gst::gst_init(ptr::null_mut(), ptr::null_mut());
        }
    }

    #[test]
    fn test_fields() {
        init();

        let mut buffer = Buffer::new();

        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(Some(1));
            buffer.set_dts(Some(2));
            buffer.set_offset(Some(3));
            buffer.set_offset_end(Some(4));
            buffer.set_duration(Some(5));
        }
        assert_eq!(buffer.get_pts(), Some(1));
        assert_eq!(buffer.get_dts(), Some(2));
        assert_eq!(buffer.get_offset(), Some(3));
        assert_eq!(buffer.get_offset_end(), Some(4));
        assert_eq!(buffer.get_duration(), Some(5));
    }

    #[test]
    fn test_writability() {
        init();

        let mut buffer = Buffer::from_vec(vec![1, 2, 3, 4]).unwrap();
        {
            let data = buffer.map_read().unwrap();
            assert_eq!(data.as_slice(), vec![1, 2, 3, 4].as_slice());
        }
        assert_ne!(buffer.get_mut(), None);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(Some(1));
        }

        let mut buffer2 = buffer.clone();
        assert_eq!(buffer.get_mut(), None);

        unsafe {
            assert_eq!(buffer2.as_ptr(), buffer.as_ptr());
        }

        {
            let buffer2 = buffer2.make_mut();
            unsafe {
                assert_ne!(buffer2.as_ptr(), buffer.as_ptr());
            }

            buffer2.set_pts(Some(2));

            let mut data = buffer2.map_readwrite().unwrap();
            assert_eq!(data.as_slice(), vec![1, 2, 3, 4].as_slice());
            data.as_mut_slice()[0] = 0;
        }

        assert_eq!(buffer.get_pts(), Some(1));
        assert_eq!(buffer2.get_pts(), Some(2));

        {
            let data = buffer.map_read().unwrap();
            assert_eq!(data.as_slice(), vec![1, 2, 3, 4].as_slice());

            let data = buffer2.map_read().unwrap();
            assert_eq!(data.as_slice(), vec![0, 2, 3, 4].as_slice());
        }
    }
}
