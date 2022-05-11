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

use glib::ffi::{gboolean, gpointer, GList, GType};
use gst::glib;

use gst::ffi::GstClockTime;
use std::ffi::{c_int, c_uint, c_ulonglong, c_ushort, c_void};

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
    pub fn ts_rtp_jitter_buffer_new() -> *mut RTPJitterBuffer;
    pub fn ts_rtp_jitter_buffer_get_type() -> GType;
    #[allow(dead_code)]
    pub fn ts_rtp_jitter_buffer_get_mode(jbuf: *mut RTPJitterBuffer) -> RTPJitterBufferMode;
    #[allow(dead_code)]
    pub fn ts_rtp_jitter_buffer_set_mode(jbuf: *mut RTPJitterBuffer, mode: RTPJitterBufferMode);
    #[allow(dead_code)]
    pub fn ts_rtp_jitter_buffer_get_delay(jbuf: *mut RTPJitterBuffer) -> GstClockTime;
    pub fn ts_rtp_jitter_buffer_set_delay(jbuf: *mut RTPJitterBuffer, delay: GstClockTime);
    pub fn ts_rtp_jitter_buffer_set_clock_rate(jbuf: *mut RTPJitterBuffer, clock_rate: c_uint);
    #[allow(dead_code)]
    pub fn ts_rtp_jitter_buffer_get_clock_rate(jbuf: *mut RTPJitterBuffer) -> c_uint;
    pub fn ts_rtp_jitter_buffer_reset_skew(jbuf: *mut RTPJitterBuffer);

    pub fn ts_rtp_jitter_buffer_flush(jbuf: *mut RTPJitterBuffer, free_func: glib::ffi::GFunc);
    pub fn ts_rtp_jitter_buffer_find_earliest(
        jbuf: *mut RTPJitterBuffer,
        pts: *mut GstClockTime,
        seqnum: *mut c_uint,
    );
    pub fn ts_rtp_jitter_buffer_calculate_pts(
        jbuf: *mut RTPJitterBuffer,
        dts: GstClockTime,
        estimated_dts: gboolean,
        rtptime: c_uint,
        base_time: GstClockTime,
        gap: c_int,
        is_rtx: gboolean,
    ) -> GstClockTime;
    pub fn ts_rtp_jitter_buffer_num_packets(jbuf: *mut RTPJitterBuffer) -> c_uint;
    pub fn ts_rtp_jitter_buffer_insert(
        jbuf: *mut RTPJitterBuffer,
        item: *mut RTPJitterBufferItem,
        head: *mut gboolean,
        percent: *mut c_int,
    ) -> gboolean;
    pub fn ts_rtp_jitter_buffer_pop(
        jbuf: *mut RTPJitterBuffer,
        percent: *mut c_int,
    ) -> *mut RTPJitterBufferItem;
    pub fn ts_rtp_jitter_buffer_peek(jbuf: *mut RTPJitterBuffer) -> *mut RTPJitterBufferItem;

    pub fn ts_gst_rtp_packet_rate_ctx_reset(ctx: *mut RTPPacketRateCtx, clock_rate: c_int);
    pub fn ts_gst_rtp_packet_rate_ctx_update(
        ctx: *mut RTPPacketRateCtx,
        seqnum: c_ushort,
        ts: c_uint,
    ) -> c_uint;
    pub fn ts_gst_rtp_packet_rate_ctx_get_max_dropout(
        ctx: *mut RTPPacketRateCtx,
        time_ms: c_int,
    ) -> c_uint;
    #[allow(dead_code)]
    pub fn ts_gst_rtp_packet_rate_ctx_get_max_misorder(
        ctx: *mut RTPPacketRateCtx,
        time_ms: c_int,
    ) -> c_uint;
}
