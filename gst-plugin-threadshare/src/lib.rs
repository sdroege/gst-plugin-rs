// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

#![crate_type = "cdylib"]

mod iocontext;

mod socket;
mod tcpclientsrc;
mod udpsrc;

mod appsrc;
mod dataqueue;
mod jitterbuffer;
mod proxy;
mod queue;

use glib_sys as glib_ffi;

use gst;
use gst::gst_plugin_define;
use gstreamer_sys as gst_ffi;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    udpsrc::register(plugin)?;
    tcpclientsrc::register(plugin)?;
    queue::register(plugin)?;
    proxy::register(plugin)?;
    appsrc::register(plugin)?;
    jitterbuffer::register(plugin)?;

    Ok(())
}

gst_plugin_define!(
    threadshare,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

pub fn set_element_flags<T: glib::IsA<gst::Object> + glib::IsA<gst::Element>>(
    element: &T,
    flags: gst::ElementFlags,
) {
    unsafe {
        let ptr: *mut gst_ffi::GstObject = element.as_ptr() as *mut _;
        let _guard = MutexGuard::lock(&(*ptr).lock);
        (*ptr).flags |= flags.to_glib();
    }
}

struct MutexGuard<'a>(&'a glib_ffi::GMutex);

impl<'a> MutexGuard<'a> {
    pub fn lock(mutex: &'a glib_ffi::GMutex) -> Self {
        unsafe {
            glib_ffi::g_mutex_lock(mut_override(mutex));
        }
        MutexGuard(mutex)
    }
}

impl<'a> Drop for MutexGuard<'a> {
    fn drop(&mut self) {
        unsafe {
            glib_ffi::g_mutex_unlock(mut_override(self.0));
        }
    }
}

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
        pub fn rtp_jitter_buffer_get_mode(jbuf: *mut RTPJitterBuffer) -> RTPJitterBufferMode;
        pub fn rtp_jitter_buffer_set_mode(jbuf: *mut RTPJitterBuffer, mode: RTPJitterBufferMode);
        pub fn rtp_jitter_buffer_get_delay(jbuf: *mut RTPJitterBuffer) -> GstClockTime;
        pub fn rtp_jitter_buffer_set_delay(jbuf: *mut RTPJitterBuffer, delay: GstClockTime);
        pub fn rtp_jitter_buffer_set_clock_rate(jbuf: *mut RTPJitterBuffer, clock_rate: c_uint);
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
        pub fn gst_rtp_packet_rate_ctx_get_max_disorder(
            ctx: *mut RTPPacketRateCtx,
            time_ms: c_int,
        ) -> c_uint;
    }
}

use glib::prelude::*;
use glib::translate::*;
use glib::{glib_object_wrapper, glib_wrapper};

use std::mem;
use std::ptr;

glib_wrapper! {
    pub struct RTPJitterBuffer(Object<ffi::RTPJitterBuffer, RTPJitterBufferClass>);

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

pub struct RTPJitterBufferItem(Option<Box<ffi::RTPJitterBufferItem>>);

unsafe impl Send for RTPJitterBufferItem {}

impl RTPJitterBufferItem {
    pub fn new(
        buffer: gst::Buffer,
        dts: gst::ClockTime,
        pts: gst::ClockTime,
        seqnum: u32,
        rtptime: u32,
    ) -> RTPJitterBufferItem {
        unsafe {
            RTPJitterBufferItem(Some(Box::new(ffi::RTPJitterBufferItem {
                data: buffer.into_ptr() as *mut _,
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
                r#type: 0,
                dts: dts.to_glib(),
                pts: pts.to_glib(),
                seqnum,
                count: 1,
                rtptime,
            })))
        }
    }

    pub fn get_buffer(&self) -> gst::Buffer {
        unsafe {
            let item = self.0.as_ref().expect("Invalid wrapper");
            let buf = item.data as *mut gst_ffi::GstBuffer;
            from_glib_none(buf)
        }
    }

    pub fn get_dts(&self) -> gst::ClockTime {
        let item = self.0.as_ref().expect("Invalid wrapper");
        if item.dts == gst_ffi::GST_CLOCK_TIME_NONE {
            gst::CLOCK_TIME_NONE
        } else {
            gst::ClockTime(Some(item.dts))
        }
    }

    pub fn get_pts(&self) -> gst::ClockTime {
        let item = self.0.as_ref().expect("Invalid wrapper");
        if item.pts == gst_ffi::GST_CLOCK_TIME_NONE {
            gst::CLOCK_TIME_NONE
        } else {
            gst::ClockTime(Some(item.pts))
        }
    }

    pub fn get_seqnum(&self) -> u32 {
        let item = self.0.as_ref().expect("Invalid wrapper");
        item.seqnum
    }

    pub fn get_rtptime(&self) -> u32 {
        let item = self.0.as_ref().expect("Invalid wrapper");
        item.rtptime
    }
}

impl Drop for RTPJitterBufferItem {
    fn drop(&mut self) {
        unsafe {
            if let Some(ref item) = self.0 {
                gst_ffi::gst_mini_object_unref(item.data as *mut _)
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

    pub fn get_mode(&self) -> RTPJitterBufferMode {
        unsafe { from_glib(ffi::rtp_jitter_buffer_get_mode(self.to_glib_none().0)) }
    }

    pub fn set_mode(&self, mode: RTPJitterBufferMode) {
        unsafe { ffi::rtp_jitter_buffer_set_mode(self.to_glib_none().0, mode.to_glib()) }
    }

    pub fn get_delay(&self) -> gst::ClockTime {
        unsafe { from_glib(ffi::rtp_jitter_buffer_get_delay(self.to_glib_none().0)) }
    }

    pub fn set_delay(&self, delay: gst::ClockTime) {
        unsafe { ffi::rtp_jitter_buffer_set_delay(self.to_glib_none().0, delay.to_glib()) }
    }

    pub fn set_clock_rate(&self, clock_rate: u32) {
        unsafe { ffi::rtp_jitter_buffer_set_clock_rate(self.to_glib_none().0, clock_rate) }
    }

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
            let box_ = item.0.take().expect("Invalid wrapper");
            let ptr = Box::into_raw(box_);
            let ret: bool = from_glib(ffi::rtp_jitter_buffer_insert(
                self.to_glib_none().0,
                ptr,
                head.as_mut_ptr(),
                percent.as_mut_ptr(),
            ));
            if !ret {
                item.0 = Some(Box::from_raw(ptr));
            }
            (ret, from_glib(head.assume_init()), percent.assume_init())
        }
    }

    pub fn find_earliest(&self) -> (gst::ClockTime, u32) {
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

            if pts == gst_ffi::GST_CLOCK_TIME_NONE {
                (gst::CLOCK_TIME_NONE, seqnum)
            } else {
                (pts.into(), seqnum)
            }
        }
    }

    pub fn pop(&self) -> (RTPJitterBufferItem, i32) {
        unsafe {
            let mut percent = mem::MaybeUninit::uninit();
            let item = ffi::rtp_jitter_buffer_pop(self.to_glib_none().0, percent.as_mut_ptr());

            (
                RTPJitterBufferItem(Some(Box::from_raw(item))),
                percent.assume_init(),
            )
        }
    }

    pub fn peek(&self) -> (gst::ClockTime, u32) {
        unsafe {
            let item = ffi::rtp_jitter_buffer_peek(self.to_glib_none().0);
            if item.is_null() {
                (gst::CLOCK_TIME_NONE, std::u32::MAX)
            } else {
                ((*item).pts.into(), (*item).seqnum)
            }
        }
    }

    pub fn flush(&self) {
        unsafe extern "C" fn free_item(item: glib_ffi::gpointer, _: glib_ffi::gpointer) {
            let _ = RTPJitterBufferItem(Some(Box::from_raw(item as *mut _)));
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
