//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

macro_rules! err_flow {
    ($element:ident, read, $msg:literal) => {
        |err| {
            gst::element_error!($element, gst::ResourceError::Read, [$msg, err]);
            gst::FlowError::Error
        }
    };
    ($element:ident, write, $msg:literal) => {
        |err| {
            gst::element_error!($element, gst::ResourceError::Write, [$msg, err]);
            gst::FlowError::Error
        }
    };

    ($element:ident, buf_read) => {
        err_flow!($element, read, "Failed to read buffer: {}")
    };
    ($element:ident, aggr_header_write) => {
        err_flow!(
            $element,
            write,
            "Failed to write aggregation header to the payload: {}"
        )
    };
    ($element:ident, leb_write) => {
        err_flow!(
            $element,
            write,
            "Failed to write leb128 size field to the payload: {}"
        )
    };
    ($element:ident, obu_write) => {
        err_flow!(
            $element,
            write,
            "Failed to write OBU bytes to the payload: {}"
        )
    };
    ($element:ident, outbuf_alloc) => {
        err_flow!($element, write, "Failed to allocate output buffer: {}")
    };
}

macro_rules! err_opt {
    ($element:ident, read, $msg:literal) => {
        |err| {
            gst::element_error!($element, gst::ResourceError::Read, [$msg, err]);
            Option::<()>::None
        }
    };
    ($element:ident, write, $msg:literal) => {
        |err| {
            gst::element_error!($element, gst::ResourceError::Write, [$msg, err]);
            Option::<()>::None
        }
    };

    ($element:ident, buf_alloc) => {
        err_opt!($element, write, "Failed to allocate new buffer: {}")
    };

    ($element:ident, payload_buf) => {
        err_opt!($element, read, "Failed to get RTP payload buffer: {}")
    };
    ($element:ident, payload_map) => {
        err_opt!($element, read, "Failed to map payload as readable: {}")
    };
    ($element:ident, buf_take) => {
        err_opt!($element, read, "Failed to take buffer from adapter: {}")
    };
    ($element:ident, aggr_header_read) => {
        err_opt!($element, read, "Failed to read aggregation header: {}")
    };
    ($element:ident, leb_read) => {
        err_opt!($element, read, "Failed to read leb128 size field: {}")
    };
    ($element:ident, leb_write) => {
        err_opt!($element, read, "Failed to write leb128 size field: {}")
    };
    ($element:ident, obu_read) => {
        err_opt!($element, read, "Failed to read OBU header: {}")
    };
    ($element:ident, buf_read) => {
        err_opt!($element, read, "Failed to read RTP buffer: {}")
    };
}

pub(crate) use err_flow;
pub(crate) use err_opt;
