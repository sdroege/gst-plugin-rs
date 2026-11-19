// Copyright (C) 2026 Netflix Inc.
//   Author: Xavier Claessens <xclaessens@netflix.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![cfg(target_os = "linux")]

use gst::prelude::*;
use gst_video::prelude::*;
use std::str::FromStr;

const WIDTH: u32 = 321;
const HEIGHT: u32 = 241;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstdebugutils::plugin_register_static().expect("debugutils plugin");
    });
}

/// Tests that allocate udmabuf memory need access to /dev/udmabuf.
fn have_udmabuf() -> bool {
    init();
    let available = gst_allocators::UdmabufAllocator::get().is_some();
    if !available {
        println!("udmabuf not available, skipping test");
    }
    available
}

fn create_harness(format: &str, width: u32, height: u32) -> gst_check::Harness {
    let mut h = gst_check::Harness::new("udmabufupload");
    h.add_propose_allocation_meta(gst_video::VideoMeta::meta_api(), None);
    h.set_src_caps_str(&format!(
        "video/x-raw,format={format},width={width},height={height},framerate=30/1"
    ));
    h
}

fn create_input_buffer(info: &gst_video::VideoInfo) -> gst::Buffer {
    let data = (0..info.size())
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let mut buffer = gst::Buffer::from_mut_slice(data);
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::ZERO);
        buffer.set_duration(gst::ClockTime::SECOND / 30);
    }
    buffer
}

fn buffer_is_udmabuf(buffer: &gst::BufferRef) -> bool {
    buffer.n_memory() > 0
        && buffer.iter_memories().all(|mem| {
            mem.allocator()
                .is_some_and(|a| a.is::<gst_allocators::UdmabufAllocator>())
        })
}

/// Compares planes line by line, since strides differ.
fn check_frames_equal(
    in_info: &gst_video::VideoInfo,
    inbuf: &gst::Buffer,
    out_info: &gst_video::VideoInfo,
    outbuf: &gst::Buffer,
) {
    let in_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(inbuf, in_info).unwrap();
    let out_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(outbuf, out_info).unwrap();

    for plane in 0..in_frame.n_planes() {
        let comp = in_info.format_info().component(plane)[0] as u32;
        let width = (in_frame.comp_width(comp) as i32 * in_frame.comp_pstride(comp)) as usize;
        let height = in_frame.comp_height(comp) as usize;
        let src_stride = in_frame.plane_stride()[plane as usize] as usize;
        let dst_stride = out_frame.plane_stride()[plane as usize] as usize;
        let src = in_frame.plane_data(plane).unwrap();
        let dst = out_frame.plane_data(plane).unwrap();

        for y in 0..height {
            assert_eq!(
                &src[y * src_stride..y * src_stride + width],
                &dst[y * dst_stride..y * dst_stride + width],
                "plane {plane} line {y} differs"
            );
        }
    }
}

/// Tiles can't be compared line by line, copy the output back into the input
/// layout and compare the whole buffer instead.
fn check_tiled_frames_equal(
    in_info: &gst_video::VideoInfo,
    inbuf: &gst::Buffer,
    out_info: &gst_video::VideoInfo,
    outbuf: &gst::Buffer,
) {
    let mut copy = gst::Buffer::with_size(in_info.size()).unwrap();
    {
        let out_frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(outbuf, out_info).unwrap();
        let mut copy_frame =
            gst_video::VideoFrameRef::from_buffer_ref_writable(copy.get_mut().unwrap(), in_info)
                .unwrap();
        out_frame.copy(&mut copy_frame).unwrap();
    }

    let in_map = inbuf.map_readable().unwrap();
    let copy_map = copy.map_readable().unwrap();
    assert!(in_map.as_slice() == copy_map.as_slice());
}

fn check_copy(format: &str, width: u32, height: u32, drm_format: &str) {
    if !have_udmabuf() {
        return;
    }

    let mut h = create_harness(format, width, height);

    let caps = h.srcpad().unwrap().current_caps().unwrap();
    let in_info = gst_video::VideoInfo::from_caps(&caps).unwrap();

    let inbuf = create_input_buffer(&in_info);
    h.push(inbuf.clone()).unwrap();

    let caps = h.sinkpad().unwrap().current_caps().unwrap();
    assert!(gst_video::is_dma_drm_caps(&caps));
    assert_eq!(
        caps.structure(0)
            .unwrap()
            .get::<&str>("drm-format")
            .unwrap(),
        drm_format
    );
    let out_info = gst_video::VideoInfoDmaDrm::from_caps(&caps).unwrap();

    let outbuf = h.pull().unwrap();
    assert_ne!(outbuf.as_ptr(), inbuf.as_ptr());
    assert!(buffer_is_udmabuf(&outbuf));
    assert_eq!(outbuf.pts(), inbuf.pts());

    let meta = outbuf.meta::<gst_video::VideoMeta>().unwrap();
    assert_eq!(meta.width(), width);
    assert_eq!(meta.height(), height);

    if in_info.format_info().is_tiled() {
        check_tiled_frames_equal(&in_info, &inbuf, &out_info, &outbuf);
    } else {
        check_frames_equal(&in_info, &inbuf, &out_info, &outbuf);
    }
}

#[test]
fn test_copy_nv12() {
    check_copy("NV12", WIDTH, HEIGHT, "NV12");
}

#[test]
fn test_copy_i420() {
    check_copy("I420", WIDTH, HEIGHT, "YU12");
}

#[test]
fn test_copy_rgba() {
    check_copy("RGBA", WIDTH, HEIGHT, "AB24");
}

// Tiled formats keep their layout and get the matching DRM modifier
#[test]
fn test_copy_nv12_16l32s() {
    check_copy("NV12_16L32S", 320, 256, "NV12:0x0b00000000000001");
}

#[test]
fn test_copy_nv12_4l4() {
    check_copy("NV12_4L4", 320, 240, "NV12:0x0600000000000001");
}

#[test]
fn test_zero_copy() {
    if !have_udmabuf() {
        return;
    }

    let mut h = create_harness("NV12", WIDTH, HEIGHT);
    let caps = h.srcpad().unwrap().current_caps().unwrap();

    // The element proposes a udmabuf pool to upstream
    let mut query = gst::query::Allocation::new(Some(&caps), true);
    assert!(h.srcpad().unwrap().peer_query(&mut query));
    let (pool, _, _, _) = query.allocation_pools().next().unwrap();
    let pool = pool.unwrap();
    assert!(pool.is::<gst_video::VideoDmabufPool>());
    assert!(
        query
            .find_allocation_meta::<gst_video::VideoMeta>()
            .is_some()
    );

    pool.set_active(true).unwrap();
    let inbuf = pool.acquire_buffer(None).unwrap();
    assert!(buffer_is_udmabuf(&inbuf));
    let inbuf_ptr = inbuf.as_ptr();

    // Buffers from that pool are forwarded as-is
    h.push(inbuf).unwrap();
    let outbuf = h.pull().unwrap();
    assert_eq!(outbuf.as_ptr(), inbuf_ptr);
    drop(outbuf);

    pool.set_active(false).unwrap();
}

#[test]
fn test_reverse_caps() {
    init();

    let mut h = gst_check::Harness::new("udmabufupload");
    h.set_sink_caps_str(
        "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=AR24,\
         width=320,height=240,framerate=30/1",
    );

    let caps = h.srcpad().unwrap().peer_query_caps(None);
    let expected = gst::Caps::from_str(
        "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=AR24,\
         width=320,height=240,framerate=30/1; \
         video/x-raw,format=BGRA,width=320,height=240,framerate=30/1",
    )
    .unwrap();
    assert_eq!(caps, expected);
}

#[test]
fn test_passthrough() {
    init();

    let caps_str = "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=NV12,\
                    width=320,height=240,framerate=30/1";

    let mut h = gst_check::Harness::new("udmabufupload");
    h.add_propose_allocation_meta(gst_video::VideoMeta::meta_api(), None);
    h.set_src_caps_str(caps_str);

    let inbuf = gst::Buffer::with_size(320 * 240 * 3 / 2).unwrap();
    let inbuf_ptr = inbuf.as_ptr();
    h.push(inbuf).unwrap();

    let element = h
        .element()
        .unwrap()
        .downcast::<gst_base::BaseTransform>()
        .unwrap();
    assert!(element.is_passthrough());

    let caps = h.sinkpad().unwrap().current_caps().unwrap();
    assert_eq!(caps, gst::Caps::from_str(caps_str).unwrap());

    let outbuf = h.pull().unwrap();
    assert_eq!(outbuf.as_ptr(), inbuf_ptr);

    // Allocation query is forwarded downstream: the harness proposes no pool
    let mut query = gst::query::Allocation::new(Some(&caps), true);
    assert!(h.srcpad().unwrap().peer_query(&mut query));
    assert_eq!(query.allocation_pools().count(), 0);
    assert!(
        query
            .find_allocation_meta::<gst_video::VideoMeta>()
            .is_some()
    );
}

#[test]
fn test_unsupported_format() {
    init();

    // v210 has no DRM fourcc equivalent
    let mut h = create_harness("v210", WIDTH, HEIGHT);

    let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::V210, WIDTH, HEIGHT)
        .build()
        .unwrap();
    let buffer = create_input_buffer(&info);
    assert_eq!(h.push(buffer), Err(gst::FlowError::NotNegotiated));
}
