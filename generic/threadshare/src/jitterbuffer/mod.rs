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

use glib_sys as glib_ffi;
use gstreamer_sys as gst_ffi;

use std::ptr;
use std::u32;

#[allow(clippy::module_inception)]
pub mod jitterbuffer;

pub mod ffi {
    use glib_ffi::{gboolean, gpointer, GList, GType};
    use glib_sys as glib_ffi;

    use gst_ffi::GstClockTime;
    use gstreamer_sys as gst_ffi;
    use libc::{c_int, c_uint, c_ulonglong, c_ushort, c_void};

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct RTPJitterBufferItem {
        pub data: gpointer,
        pub next: *mut GList,
        pub prev: *mut GList,
        pub r#type: c_uint,
        pub dts: GstClockTime,
        pub pts: GstClockTime,
        pub seqnum: c_uint,
        pub count: c_uint,
        pub rtptime: c_uint,
    }

    #[repr(C)]
    pub struct RTPJitterBuffer(c_void);

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct RTPPacketRateCtx {
        probed: gboolean,
        clock_rate: c_int,
        last_seqnum: c_ushort,
        last_ts: c_ulonglong,
        avg_packet_rate: c_uint,
    }

    pub type RTPJitterBufferMode = c_int;
    pub const RTP_JITTER_BUFFER_MODE_NONE: RTPJitterBufferMode = 0;
    pub const RTP_JITTER_BUFFER_MODE_SLAVE: RTPJitterBufferMode = 1;
    pub const RTP_JITTER_BUFFER_MODE_BUFFER: RTPJitterBufferMode = 2;
    pub const RTP_JITTER_BUFFER_MODE_SYNCED: RTPJitterBufferMode = 4;

    extern "C" {
        pub fn rtp_jitter_buffer_new() -> *mut RTPJitterBuffer;
        pub fn rtp_jitter_buffer_get_type() -> GType;
        #[allow(dead_code)]
        pub fn rtp_jitter_buffer_get_mode(jbuf: *mut RTPJitterBuffer) -> RTPJitterBufferMode;
        #[allow(dead_code)]
        pub fn rtp_jitter_buffer_set_mode(jbuf: *mut RTPJitterBuffer, mode: RTPJitterBufferMode);
        #[allow(dead_code)]
        pub fn rtp_jitter_buffer_get_delay(jbuf: *mut RTPJitterBuffer) -> GstClockTime;
        pub fn rtp_jitter_buffer_set_delay(jbuf: *mut RTPJitterBuffer, delay: GstClockTime);
        pub fn rtp_jitter_buffer_set_clock_rate(jbuf: *mut RTPJitterBuffer, clock_rate: c_uint);
        #[allow(dead_code)]
        pub fn rtp_jitter_buffer_get_clock_rate(jbuf: *mut RTPJitterBuffer) -> c_uint;
        pub fn rtp_jitter_buffer_reset_skew(jbuf: *mut RTPJitterBuffer);

        pub fn rtp_jitter_buffer_flush(jbuf: *mut RTPJitterBuffer, free_func: glib_ffi::GFunc);
        pub fn rtp_jitter_buffer_find_earliest(
            jbuf: *mut RTPJitterBuffer,
            pts: *mut GstClockTime,
            seqnum: *mut c_uint,
        );
        pub fn rtp_jitter_buffer_calculate_pts(
            jbuf: *mut RTPJitterBuffer,
            dts: GstClockTime,
            estimated_dts: gboolean,
            rtptime: c_uint,
            base_time: GstClockTime,
            gap: c_int,
            is_rtx: gboolean,
        ) -> GstClockTime;
        pub fn rtp_jitter_buffer_insert(
            jbuf: *mut RTPJitterBuffer,
            item: *mut RTPJitterBufferItem,
            head: *mut gboolean,
            percent: *mut c_int,
        ) -> gboolean;
        pub fn rtp_jitter_buffer_pop(
            jbuf: *mut RTPJitterBuffer,
            percent: *mut c_int,
        ) -> *mut RTPJitterBufferItem;
        pub fn rtp_jitter_buffer_peek(jbuf: *mut RTPJitterBuffer) -> *mut RTPJitterBufferItem;

        pub fn gst_rtp_packet_rate_ctx_reset(ctx: *mut RTPPacketRateCtx, clock_rate: c_int);
        pub fn gst_rtp_packet_rate_ctx_update(
            ctx: *mut RTPPacketRateCtx,
            seqnum: c_ushort,
            ts: c_uint,
        ) -> c_uint;
        pub fn gst_rtp_packet_rate_ctx_get_max_dropout(
            ctx: *mut RTPPacketRateCtx,
            time_ms: c_int,
        ) -> c_uint;
        #[allow(dead_code)]
        pub fn gst_rtp_packet_rate_ctx_get_max_disorder(
            ctx: *mut RTPPacketRateCtx,
            time_ms: c_int,
        ) -> c_uint;
    }
}

use glib::glib_wrapper;
use glib::prelude::*;
use glib::translate::*;

use std::mem;

glib_wrapper! {
    pub struct RTPJitterBuffer(Object<ffi::RTPJitterBuffer>);

    match fn {
        get_type => || ffi::rtp_jitter_buffer_get_type(),
    }
}

unsafe impl glib::SendUnique for RTPJitterBuffer {
    fn is_unique(&self) -> bool {
        self.ref_count() == 1
    }
}

impl ToGlib for RTPJitterBufferMode {
    type GlibType = ffi::RTPJitterBufferMode;

    fn to_glib(&self) -> ffi::RTPJitterBufferMode {
        match *self {
            RTPJitterBufferMode::None => ffi::RTP_JITTER_BUFFER_MODE_NONE,
            RTPJitterBufferMode::Slave => ffi::RTP_JITTER_BUFFER_MODE_SLAVE,
            RTPJitterBufferMode::Buffer => ffi::RTP_JITTER_BUFFER_MODE_BUFFER,
            RTPJitterBufferMode::Synced => ffi::RTP_JITTER_BUFFER_MODE_SYNCED,
            RTPJitterBufferMode::__Unknown(value) => value,
        }
    }
}

impl FromGlib<ffi::RTPJitterBufferMode> for RTPJitterBufferMode {
    fn from_glib(value: ffi::RTPJitterBufferMode) -> Self {
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
        dts: gst::ClockTime,
        pts: gst::ClockTime,
        seqnum: Option<u16>,
        rtptime: u32,
    ) -> RTPJitterBufferItem {
        unsafe {
            let ptr = ptr::NonNull::new(glib_sys::g_slice_alloc0(mem::size_of::<
                ffi::RTPJitterBufferItem,
            >()) as *mut ffi::RTPJitterBufferItem)
            .expect("Allocation failed");
            ptr::write(
                ptr.as_ptr(),
                ffi::RTPJitterBufferItem {
                    data: buffer.into_ptr() as *mut _,
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                    r#type: 0,
                    dts: dts.to_glib(),
                    pts: pts.to_glib(),
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
            let buf = item.as_ref().data as *mut gst_ffi::GstBuffer;
            glib_sys::g_slice_free1(
                mem::size_of::<ffi::RTPJitterBufferItem>(),
                item.as_ptr() as *mut _,
            );
            from_glib_full(buf)
        }
    }

    pub fn get_dts(&self) -> gst::ClockTime {
        unsafe {
            let item = self.0.as_ref().expect("Invalid wrapper");
            if item.as_ref().dts == gst_ffi::GST_CLOCK_TIME_NONE {
                gst::CLOCK_TIME_NONE
            } else {
                gst::ClockTime(Some(item.as_ref().dts))
            }
        }
    }

    pub fn get_pts(&self) -> gst::ClockTime {
        unsafe {
            let item = self.0.as_ref().expect("Invalid wrapper");
            if item.as_ref().pts == gst_ffi::GST_CLOCK_TIME_NONE {
                gst::CLOCK_TIME_NONE
            } else {
                gst::ClockTime(Some(item.as_ref().pts))
            }
        }
    }

    pub fn get_seqnum(&self) -> Option<u16> {
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
    pub fn get_rtptime(&self) -> u32 {
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
                    gst_ffi::gst_mini_object_unref(item.as_ref().data as *mut _);
                }

                glib_sys::g_slice_free1(
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
            ffi::gst_rtp_packet_rate_ctx_reset(ptr.as_mut_ptr(), -1);
            RTPPacketRateCtx(Box::new(ptr.assume_init()))
        }
    }

    pub fn reset(&mut self, clock_rate: i32) {
        unsafe { ffi::gst_rtp_packet_rate_ctx_reset(&mut *self.0, clock_rate) }
    }

    pub fn update(&mut self, seqnum: u16, ts: u32) -> u32 {
        unsafe { ffi::gst_rtp_packet_rate_ctx_update(&mut *self.0, seqnum, ts) }
    }

    pub fn get_max_dropout(&mut self, time_ms: i32) -> u32 {
        unsafe { ffi::gst_rtp_packet_rate_ctx_get_max_dropout(&mut *self.0, time_ms) }
    }

    #[allow(dead_code)]
    pub fn get_max_disorder(&mut self, time_ms: i32) -> u32 {
        unsafe { ffi::gst_rtp_packet_rate_ctx_get_max_disorder(&mut *self.0, time_ms) }
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
        unsafe { from_glib_full(ffi::rtp_jitter_buffer_new()) }
    }

    #[allow(dead_code)]
    pub fn get_mode(&self) -> RTPJitterBufferMode {
        unsafe { from_glib(ffi::rtp_jitter_buffer_get_mode(self.to_glib_none().0)) }
    }

    #[allow(dead_code)]
    pub fn set_mode(&self, mode: RTPJitterBufferMode) {
        unsafe { ffi::rtp_jitter_buffer_set_mode(self.to_glib_none().0, mode.to_glib()) }
    }

    #[allow(dead_code)]
    pub fn get_delay(&self) -> gst::ClockTime {
        unsafe { from_glib(ffi::rtp_jitter_buffer_get_delay(self.to_glib_none().0)) }
    }

    pub fn set_delay(&self, delay: gst::ClockTime) {
        unsafe { ffi::rtp_jitter_buffer_set_delay(self.to_glib_none().0, delay.to_glib()) }
    }

    pub fn set_clock_rate(&self, clock_rate: u32) {
        unsafe { ffi::rtp_jitter_buffer_set_clock_rate(self.to_glib_none().0, clock_rate) }
    }

    #[allow(dead_code)]
    pub fn get_clock_rate(&self) -> u32 {
        unsafe { ffi::rtp_jitter_buffer_get_clock_rate(self.to_glib_none().0) }
    }

    pub fn calculate_pts(
        &self,
        dts: gst::ClockTime,
        estimated_dts: bool,
        rtptime: u32,
        base_time: gst::ClockTime,
        gap: i32,
        is_rtx: bool,
    ) -> gst::ClockTime {
        unsafe {
            let pts = ffi::rtp_jitter_buffer_calculate_pts(
                self.to_glib_none().0,
                dts.to_glib(),
                estimated_dts.to_glib(),
                rtptime,
                base_time.to_glib(),
                gap,
                is_rtx.to_glib(),
            );

            if pts == gst_ffi::GST_CLOCK_TIME_NONE {
                gst::CLOCK_TIME_NONE
            } else {
                pts.into()
            }
        }
    }

    pub fn insert(&self, mut item: RTPJitterBufferItem) -> (bool, bool, i32) {
        unsafe {
            let mut head = mem::MaybeUninit::uninit();
            let mut percent = mem::MaybeUninit::uninit();
            let ptr = item.0.take().expect("Invalid wrapper");
            let ret: bool = from_glib(ffi::rtp_jitter_buffer_insert(
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

    pub fn find_earliest(&self) -> (gst::ClockTime, Option<u16>) {
        unsafe {
            let mut pts = mem::MaybeUninit::uninit();
            let mut seqnum = mem::MaybeUninit::uninit();

            ffi::rtp_jitter_buffer_find_earliest(
                self.to_glib_none().0,
                pts.as_mut_ptr(),
                seqnum.as_mut_ptr(),
            );
            let pts = pts.assume_init();
            let seqnum = seqnum.assume_init();

            let seqnum = if seqnum == u32::MAX {
                None
            } else {
                Some(seqnum as u16)
            };

            if pts == gst_ffi::GST_CLOCK_TIME_NONE {
                (gst::CLOCK_TIME_NONE, seqnum)
            } else {
                (pts.into(), seqnum)
            }
        }
    }

    pub fn pop(&self) -> (Option<RTPJitterBufferItem>, i32) {
        unsafe {
            let mut percent = mem::MaybeUninit::uninit();
            let item = ffi::rtp_jitter_buffer_pop(self.to_glib_none().0, percent.as_mut_ptr());

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

    pub fn peek(&self) -> (gst::ClockTime, Option<u16>) {
        unsafe {
            let item = ffi::rtp_jitter_buffer_peek(self.to_glib_none().0);
            if item.is_null() {
                (gst::CLOCK_TIME_NONE, None)
            } else {
                let seqnum = (*item).seqnum;
                let seqnum = if seqnum == u32::MAX {
                    None
                } else {
                    Some(seqnum as u16)
                };
                ((*item).pts.into(), seqnum)
            }
        }
    }

    pub fn flush(&self) {
        unsafe extern "C" fn free_item(item: glib_ffi::gpointer, _: glib_ffi::gpointer) {
            let _ =
                RTPJitterBufferItem(Some(ptr::NonNull::new(item as *mut _).expect("NULL item")));
        }

        unsafe {
            ffi::rtp_jitter_buffer_flush(self.to_glib_none().0, Some(free_item));
        }
    }

    pub fn reset_skew(&self) {
        unsafe { ffi::rtp_jitter_buffer_reset_skew(self.to_glib_none().0) }
    }
}

impl Default for RTPJitterBuffer {
    fn default() -> Self {
        RTPJitterBuffer::new()
    }
}
