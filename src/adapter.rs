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

use buffer::*;
use std::collections::VecDeque;
use std::cmp;

#[derive(Debug)]
pub struct Adapter {
    deque: VecDeque<ReadMappedBuffer>,
    size: usize,
    skip: usize,
    scratch: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdapterError {
    NotEnoughData,
}

impl Adapter {
    pub fn new() -> Adapter {
        Adapter {
            deque: VecDeque::new(),
            size: 0,
            skip: 0,
            scratch: Vec::new(),
        }
    }

    pub fn push(&mut self, buffer: Buffer) {
        let size = buffer.get_size();

        self.size += size;
        self.deque.push_back(buffer.into_read_mapped_buffer().unwrap());
    }

    pub fn clear(&mut self) {
        self.deque.clear();
        self.size = 0;
        self.skip = 0;
        self.scratch.clear();
    }

    pub fn get_available(&self) -> usize {
        self.size
    }

    pub fn peek(&mut self, size: usize) -> Result<&[u8], AdapterError> {
        if self.size < size {
            return Err(AdapterError::NotEnoughData);
        }

        if size == 0 {
            return Ok(&[]);
        }

        if let Some(front) = self.deque.front() {
            if front.get_size() - self.skip >= size {
                return Ok(&front.as_slice()[self.skip..self.skip + size]);
            }
        }

        self.scratch.truncate(0);
        self.scratch.reserve(size);
        {
            let data = self.scratch.as_mut_slice();

            let mut skip = self.skip;
            let mut left = size;
            let mut idx = 0;

            for item in &self.deque {
                let data_item = item.as_slice();

                let to_copy = cmp::min(left, data_item.len() - skip);
                data[idx..idx + to_copy].copy_from_slice(&data_item[skip..skip + to_copy]);
                skip = 0;
                idx += to_copy;
                left -= to_copy;
                if left == 0 {
                    break;
                }
            }
            assert_eq!(left, 0);
        }

        Ok(self.scratch.as_slice())
    }

    pub fn get_buffer(&mut self, size: usize) -> Result<Buffer, AdapterError> {
        if self.size < size {
            return Err(AdapterError::NotEnoughData);
        }

        if size == 0 {
            return Ok(Buffer::new());
        }

        if let Some(sub) = {
            if let Some(front) = self.deque.front() {
                if front.get_size() - self.skip >= size {
                    let new = front.get_buffer().copy_region(self.skip, Some(size)).unwrap();
                    Some(new)
                } else {
                    None
                }
            } else {
                None
            }
        } {
            self.flush(size).unwrap();
            return Ok(sub);
        }

        let mut new = Buffer::new_with_size(size).unwrap();
        {
            let mut map = new.map_readwrite().unwrap();
            let data = map.as_mut_slice();

            let mut skip = self.skip;
            let mut left = size;
            let mut idx = 0;

            for item in &self.deque {
                let data_item = item.as_slice();

                let to_copy = cmp::min(left, data_item.len() - skip);
                data[idx..idx + to_copy].copy_from_slice(&data_item[skip..skip + to_copy]);
                skip = 0;
                idx += to_copy;
                left -= to_copy;
                if left == 0 {
                    break;
                }
            }
            assert_eq!(left, 0);
        }
        self.flush(size).unwrap();
        Ok(new)
    }

    pub fn flush(&mut self, size: usize) -> Result<(), AdapterError> {
        if self.size < size {
            return Err(AdapterError::NotEnoughData);
        }

        if size == 0 {
            return Ok(());
        }

        let mut left = size;
        while left > 0 {
            let front_size = self.deque.front().unwrap().get_size() - self.skip;

            if front_size <= left {
                self.deque.pop_front();
                self.size -= front_size;
                self.skip = 0;
                left -= front_size;
            } else {
                self.skip += left;
                self.size -= left;
                left = 0;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer::*;
    use std::ptr;
    use std::os::raw::c_void;

    fn init() {
        extern "C" {
            fn gst_init(argc: *mut c_void, argv: *mut c_void);
        }

        unsafe {
            gst_init(ptr::null_mut(), ptr::null_mut());
        }
    }

    #[test]
    fn test_push_get() {
        init();

        let mut a = Adapter::new();

        a.push(Buffer::new_with_size(100).unwrap());
        assert_eq!(a.get_available(), 100);
        a.push(Buffer::new_with_size(20).unwrap());
        assert_eq!(a.get_available(), 120);

        let b = a.get_buffer(20).unwrap();
        assert_eq!(a.get_available(), 100);
        assert_eq!(b.get_size(), 20);
        let b = a.get_buffer(90).unwrap();
        assert_eq!(a.get_available(), 10);
        assert_eq!(b.get_size(), 90);

        a.push(Buffer::new_with_size(20).unwrap());
        assert_eq!(a.get_available(), 30);

        let b = a.get_buffer(20).unwrap();
        assert_eq!(a.get_available(), 10);
        assert_eq!(b.get_size(), 20);
        let b = a.get_buffer(10).unwrap();
        assert_eq!(a.get_available(), 0);
        assert_eq!(b.get_size(), 10);

        let b = a.get_buffer(1);
        assert_eq!(b.err().unwrap(), AdapterError::NotEnoughData);
    }
}
