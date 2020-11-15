// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::ffi;
use std::mem;

#[derive(Copy, Clone, Debug)]
#[allow(unused)]
pub enum Status {
    Ok,
    Ready,
    Clear,
}

#[derive(Copy, Clone, Debug)]
pub struct Error;

pub struct CaptionFrame(ffi::caption_frame_t);

unsafe impl Send for CaptionFrame {}
unsafe impl Sync for CaptionFrame {}

impl CaptionFrame {
    pub fn new() -> Self {
        unsafe {
            let mut frame = mem::MaybeUninit::uninit();
            ffi::caption_frame_init(frame.as_mut_ptr());
            Self(frame.assume_init())
        }
    }

    #[allow(unused)]
    pub fn decode(&mut self, cc_data: u16, timestamp: f64) -> Result<Status, Error> {
        unsafe {
            let res = ffi::caption_frame_decode(&mut self.0, cc_data, timestamp);
            match res {
                ffi::libcaption_stauts_t_LIBCAPTION_OK => Ok(Status::Ok),
                ffi::libcaption_stauts_t_LIBCAPTION_READY => Ok(Status::Ready),
                ffi::libcaption_stauts_t_LIBCAPTION_CLEAR => Ok(Status::Clear),
                _ => Err(Error),
            }
        }
    }

    #[allow(unused)]
    pub fn to_text(&self, full: bool) -> Result<String, Error> {
        unsafe {
            let mut data = Vec::with_capacity(ffi::CAPTION_FRAME_TEXT_BYTES as usize);

            let len = ffi::caption_frame_to_text(
                &self.0 as *const _ as *mut _,
                data.as_ptr() as *mut _,
                if full { 1 } else { 0 },
            );
            data.set_len(len as usize);

            String::from_utf8(data).map_err(|_| Error)
        }
    }
}

impl Default for CaptionFrame {
    fn default() -> Self {
        Self::new()
    }
}
