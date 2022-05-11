// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use super::ffi;

use gst::prelude::*;

use std::ptr;

use glib::translate::*;
use gst::glib;

use std::mem;

glib::wrapper! {
    pub struct RTPJitterBuffer(Object<ffi::RTPJitterBuffer>);

    match fn {
        type_ => || ffi::ts_rtp_jitter_buffer_get_type(),
    }
}

// SAFETY: We ensure that we never get another reference to the jitterbuffer
unsafe impl Send for RTPJitterBuffer {}

impl IntoGlib for RTPJitterBufferMode {
    type GlibType = ffi::RTPJitterBufferMode;

    fn into_glib(self) -> ffi::RTPJitterBufferMode {
        match self {
            RTPJitterBufferMode::None => ffi::RTP_JITTER_BUFFER_MODE_NONE,
            RTPJitterBufferMode::Slave => ffi::RTP_JITTER_BUFFER_MODE_SLAVE,
            RTPJitterBufferMode::Buffer => ffi::RTP_JITTER_BUFFER_MODE_BUFFER,
            RTPJitterBufferMode::Synced => ffi::RTP_JITTER_BUFFER_MODE_SYNCED,
            RTPJitterBufferMode::__Unknown(value) => value,
        }
    }
}

impl FromGlib<ffi::RTPJitterBufferMode> for RTPJitterBufferMode {
    unsafe fn from_glib(value: ffi::RTPJitterBufferMode) -> Self {
        match value {
            0 => RTPJitterBufferMode::None,
            1 => RTPJitterBufferMode::Slave,
            2 => RTPJitterBufferMode::Buffer,
            4 => RTPJitterBufferMode::Synced,
            value => RTPJitterBufferMode::__Unknown(value),
        }
    }
}

pub struct RTPJitterBufferItem(Option<ptr::NonNull<ffi::RTPJitterBufferItem>>);

unsafe impl Send for RTPJitterBufferItem {}

impl RTPJitterBufferItem {
    pub fn new(
        buffer: gst::Buffer,
        dts: impl Into<Option<gst::ClockTime>>,
        pts: impl Into<Option<gst::ClockTime>>,
        seqnum: Option<u16>,
        rtptime: u32,
    ) -> RTPJitterBufferItem {
        unsafe {
            let ptr = ptr::NonNull::new(glib::ffi::g_slice_alloc0(mem::size_of::<
                ffi::RTPJitterBufferItem,
            >()) as *mut ffi::RTPJitterBufferItem)
            .expect("Allocation failed");
            ptr::write(
                ptr.as_ptr(),
                ffi::RTPJitterBufferItem {
                    data: buffer.into_glib_ptr() as *mut _,
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                    r#type: 0,
                    dts: dts.into().into_glib(),
                    pts: pts.into().into_glib(),
                    seqnum: seqnum.map(|s| s as u32).unwrap_or(u32::MAX),
                    count: 1,
                    rtptime,
                },
            );

            RTPJitterBufferItem(Some(ptr))
        }
    }

    pub fn into_buffer(mut self) -> gst::Buffer {
        unsafe {
            let item = self.0.take().expect("Invalid wrapper");
            let buf = item.as_ref().data as *mut gst::ffi::GstBuffer;
            glib::ffi::g_slice_free1(
                mem::size_of::<ffi::RTPJitterBufferItem>(),
                item.as_ptr() as *mut _,
            );
            from_glib_full(buf)
        }
    }

    pub fn dts(&self) -> Option<gst::ClockTime> {
        unsafe {
            let item = self.0.as_ref().expect("Invalid wrapper");
            if item.as_ref().dts == gst::ffi::GST_CLOCK_TIME_NONE {
                None
            } else {
                Some(item.as_ref().dts.nseconds())
            }
        }
    }

    pub fn pts(&self) -> Option<gst::ClockTime> {
        unsafe {
            let item = self.0.as_ref().expect("Invalid wrapper");
            if item.as_ref().pts == gst::ffi::GST_CLOCK_TIME_NONE {
                None
            } else {
                Some(item.as_ref().pts.nseconds())
            }
        }
    }

    pub fn seqnum(&self) -> Option<u16> {
        unsafe {
            let item = self.0.as_ref().expect("Invalid wrapper");
            if item.as_ref().seqnum == u32::MAX {
                None
            } else {
                Some(item.as_ref().seqnum as u16)
            }
        }
    }

    #[allow(dead_code)]
    pub fn rtptime(&self) -> u32 {
        unsafe {
            let item = self.0.as_ref().expect("Invalid wrapper");
            item.as_ref().rtptime
        }
    }
}

impl Drop for RTPJitterBufferItem {
    fn drop(&mut self) {
        unsafe {
            if let Some(ref item) = self.0 {
                if !item.as_ref().data.is_null() {
                    gst::ffi::gst_mini_object_unref(item.as_ref().data as *mut _);
                }

                glib::ffi::g_slice_free1(
                    mem::size_of::<ffi::RTPJitterBufferItem>(),
                    item.as_ptr() as *mut _,
                );
            }
        }
    }
}

pub struct RTPPacketRateCtx(Box<ffi::RTPPacketRateCtx>);

unsafe impl Send for RTPPacketRateCtx {}

impl RTPPacketRateCtx {
    pub fn new() -> RTPPacketRateCtx {
        unsafe {
            let mut ptr = std::mem::MaybeUninit::uninit();
            ffi::ts_gst_rtp_packet_rate_ctx_reset(ptr.as_mut_ptr(), -1);
            RTPPacketRateCtx(Box::new(ptr.assume_init()))
        }
    }

    pub fn reset(&mut self, clock_rate: i32) {
        unsafe { ffi::ts_gst_rtp_packet_rate_ctx_reset(&mut *self.0, clock_rate) }
    }

    pub fn update(&mut self, seqnum: u16, ts: u32) -> u32 {
        unsafe { ffi::ts_gst_rtp_packet_rate_ctx_update(&mut *self.0, seqnum, ts) }
    }

    pub fn max_dropout(&mut self, time_ms: i32) -> u32 {
        unsafe { ffi::ts_gst_rtp_packet_rate_ctx_get_max_dropout(&mut *self.0, time_ms) }
    }

    pub fn max_misorder(&mut self, time_ms: i32) -> u32 {
        unsafe { ffi::ts_gst_rtp_packet_rate_ctx_get_max_misorder(&mut *self.0, time_ms) }
    }
}

impl Default for RTPPacketRateCtx {
    fn default() -> Self {
        RTPPacketRateCtx::new()
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy)]
pub enum RTPJitterBufferMode {
    r#None,
    Slave,
    Buffer,
    Synced,
    __Unknown(i32),
}

impl RTPJitterBuffer {
    pub fn new() -> RTPJitterBuffer {
        unsafe { from_glib_full(ffi::ts_rtp_jitter_buffer_new()) }
    }

    #[allow(dead_code)]
    pub fn mode(&self) -> RTPJitterBufferMode {
        unsafe { from_glib(ffi::ts_rtp_jitter_buffer_get_mode(self.to_glib_none().0)) }
    }

    #[allow(dead_code)]
    pub fn set_mode(&self, mode: RTPJitterBufferMode) {
        unsafe { ffi::ts_rtp_jitter_buffer_set_mode(self.to_glib_none().0, mode.into_glib()) }
    }

    #[allow(dead_code)]
    pub fn delay(&self) -> gst::ClockTime {
        unsafe {
            try_from_glib(ffi::ts_rtp_jitter_buffer_get_delay(self.to_glib_none().0))
                .expect("undefined delay")
        }
    }

    pub fn set_delay(&self, delay: gst::ClockTime) {
        unsafe { ffi::ts_rtp_jitter_buffer_set_delay(self.to_glib_none().0, delay.into_glib()) }
    }

    pub fn set_clock_rate(&self, clock_rate: u32) {
        unsafe { ffi::ts_rtp_jitter_buffer_set_clock_rate(self.to_glib_none().0, clock_rate) }
    }

    #[allow(dead_code)]
    pub fn clock_rate(&self) -> u32 {
        unsafe { ffi::ts_rtp_jitter_buffer_get_clock_rate(self.to_glib_none().0) }
    }

    pub fn calculate_pts(
        &self,
        dts: impl Into<Option<gst::ClockTime>>,
        estimated_dts: bool,
        rtptime: u32,
        base_time: impl Into<Option<gst::ClockTime>>,
        gap: i32,
        is_rtx: bool,
    ) -> Option<gst::ClockTime> {
        unsafe {
            from_glib(ffi::ts_rtp_jitter_buffer_calculate_pts(
                self.to_glib_none().0,
                dts.into().into_glib(),
                estimated_dts.into_glib(),
                rtptime,
                base_time.into().into_glib(),
                gap,
                is_rtx.into_glib(),
            ))
        }
    }

    pub fn insert(&self, mut item: RTPJitterBufferItem) -> (bool, bool, i32) {
        unsafe {
            let mut head = mem::MaybeUninit::uninit();
            let mut percent = mem::MaybeUninit::uninit();
            let ptr = item.0.take().expect("Invalid wrapper");
            let ret: bool = from_glib(ffi::ts_rtp_jitter_buffer_insert(
                self.to_glib_none().0,
                ptr.as_ptr(),
                head.as_mut_ptr(),
                percent.as_mut_ptr(),
            ));
            if !ret {
                item.0 = Some(ptr);
            }
            (ret, from_glib(head.assume_init()), percent.assume_init())
        }
    }

    pub fn find_earliest(&self) -> (Option<gst::ClockTime>, Option<u16>) {
        unsafe {
            let mut pts = mem::MaybeUninit::uninit();
            let mut seqnum = mem::MaybeUninit::uninit();

            ffi::ts_rtp_jitter_buffer_find_earliest(
                self.to_glib_none().0,
                pts.as_mut_ptr(),
                seqnum.as_mut_ptr(),
            );
            let pts = from_glib(pts.assume_init());
            let seqnum = seqnum.assume_init();

            let seqnum = if seqnum == u32::MAX {
                None
            } else {
                Some(seqnum as u16)
            };

            (pts, seqnum)
        }
    }

    pub fn pop(&self) -> (Option<RTPJitterBufferItem>, i32) {
        unsafe {
            let mut percent = mem::MaybeUninit::uninit();
            let item = ffi::ts_rtp_jitter_buffer_pop(self.to_glib_none().0, percent.as_mut_ptr());

            (
                if item.is_null() {
                    None
                } else {
                    Some(RTPJitterBufferItem(Some(ptr::NonNull::new_unchecked(item))))
                },
                percent.assume_init(),
            )
        }
    }

    pub fn peek(&self) -> (Option<gst::ClockTime>, Option<u16>) {
        unsafe {
            let item = ffi::ts_rtp_jitter_buffer_peek(self.to_glib_none().0);
            if item.is_null() {
                (None, None)
            } else {
                let seqnum = (*item).seqnum;
                let seqnum = if seqnum == u32::MAX {
                    None
                } else {
                    Some(seqnum as u16)
                };
                (from_glib((*item).pts), seqnum)
            }
        }
    }

    pub fn flush(&self) {
        unsafe extern "C" fn free_item(item: glib::ffi::gpointer, _: glib::ffi::gpointer) {
            let _ =
                RTPJitterBufferItem(Some(ptr::NonNull::new(item as *mut _).expect("NULL item")));
        }

        unsafe {
            ffi::ts_rtp_jitter_buffer_flush(self.to_glib_none().0, Some(free_item));
        }
    }

    pub fn reset_skew(&self) {
        unsafe { ffi::ts_rtp_jitter_buffer_reset_skew(self.to_glib_none().0) }
    }

    pub fn num_packets(&self) -> u32 {
        unsafe { ffi::ts_rtp_jitter_buffer_num_packets(self.to_glib_none().0) }
    }
}

impl Default for RTPJitterBuffer {
    fn default() -> Self {
        RTPJitterBuffer::new()
    }
}
