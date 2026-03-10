// SPDX-License-Identifier: MPL-2.0

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline_and_validate_buffer};
use anyhow::bail;
use gst_video::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpvraw test");
    });
}

fn create_test_frame(video_info: &gst_video::VideoInfo, frame_idx: u64) -> gst::Buffer {
    let size = video_info.size();
    let mut buffer = gst::Buffer::with_size(size).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::from_seconds(frame_idx));

        let mut frame =
            gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, video_info).unwrap();

        // Fill with an increasing bit pattern that can be checked again later
        let mut idx = frame_idx;
        for plane_idx in 0..frame.n_planes() {
            let stride = frame.plane_stride()[plane_idx as usize] as usize;
            let plane = frame.plane_data_mut(plane_idx).unwrap();

            for line in plane.chunks_mut(stride) {
                // FIXME: This fills padding at the end of each line which will get dropped
                // but as we use 320 as width there is actually no padding
                for b in line.iter_mut() {
                    *b = (idx & 0xff) as u8;
                    idx = idx.wrapping_add(1);
                }
            }
        }
    }

    buffer
}

fn check_test_frame(
    buffer: &gst::Buffer,
    video_info: &gst_video::VideoInfo,
    frame_idx: u64,
) -> anyhow::Result<()> {
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, video_info).unwrap();

    let mut idx = frame_idx;
    for plane_idx in 0..frame.n_planes() {
        let stride = frame.plane_stride()[plane_idx as usize] as usize;
        let plane = frame.plane_data(plane_idx).unwrap();

        for (y, line) in plane.chunks(stride).enumerate() {
            for (x, b) in line.iter().enumerate() {
                let expected_byte = (idx & 0xff) as u8;
                let actual_byte = *b;

                if actual_byte != expected_byte {
                    bail!(
                        "Plane {plane_idx}: Expected byte {expected_byte} at position ({x}, {y}) but got {actual_byte}",
                    );
                }
                idx = idx.wrapping_add(1);
            }
        }
    }

    Ok(())
}

fn run_raw_video_test(
    format: gst_video::VideoFormat,
    width: u32,
    height: u32,
    expected_packets_per_frame: usize,
) {
    init();

    let video_info = gst_video::VideoInfo::builder(format, width, height)
        .build()
        .unwrap();
    let caps = video_info.to_caps().unwrap();

    let buffers = (0..3)
        .map(|i| create_test_frame(&video_info, i))
        .collect::<Vec<_>>();

    let expected_pay = (0..3)
        .map(|i| {
            (0..expected_packets_per_frame)
                .map(|j| {
                    ExpectedPacket::builder()
                        .pts(gst::ClockTime::from_seconds(i))
                        .flags(if j == expected_packets_per_frame - 1 {
                            gst::BufferFlags::MARKER
                        } else if i == 0 && j == 0 {
                            gst::BufferFlags::DISCONT
                        } else {
                            gst::BufferFlags::empty()
                        })
                        .pt(96)
                        .rtp_time(i as u32 * 90_000)
                        .marker_bit(j == expected_packets_per_frame - 1)
                        // FIXME: Should also check sizes but the pattern is not simple
                        .build()
                })
                .collect()
        })
        .collect();

    let expected_depay = (0..3)
        .map(|i| {
            vec![
                ExpectedBuffer::builder()
                    .pts(gst::ClockTime::from_seconds(i))
                    .size(video_info.size())
                    .flags(if i == 0 {
                        gst::BufferFlags::DISCONT
                    } else {
                        gst::BufferFlags::empty()
                    })
                    .build(),
            ]
        })
        .collect();

    run_test_pipeline_and_validate_buffer(
        Source::Buffers(caps, buffers),
        "rtpvrawpay2",
        "rtpvrawdepay2",
        expected_pay,
        expected_depay,
        move |buffer, list_idx, buffer_idx| {
            if buffer_idx != 0 {
                bail!("Got multiple output buffers per frame");
            }

            if list_idx >= 3 {
                bail!("Too many frames (got {}, expected 3)", list_idx + 1);
            }

            check_test_frame(buffer, &video_info, list_idx as u64)
        },
    );
}

#[test]
fn test_rtpvraw_rgb() {
    run_raw_video_test(gst_video::VideoFormat::Rgb, 320, 240, 167);
}

#[test]
fn test_rtpvraw_bgr() {
    run_raw_video_test(gst_video::VideoFormat::Bgr, 320, 240, 167);
}

#[test]
fn test_rtpvraw_rgba() {
    run_raw_video_test(gst_video::VideoFormat::Rgba, 320, 240, 223);
}

#[test]
fn test_rtpvraw_bgra() {
    run_raw_video_test(gst_video::VideoFormat::Bgra, 320, 240, 223);
}

#[test]
fn test_rtpvraw_v308() {
    run_raw_video_test(gst_video::VideoFormat::V308, 320, 240, 167);
}

#[test]
fn test_rtpvraw_uyvy() {
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 320, 240, 112);
}

#[test]
fn test_rtpvraw_i420() {
    run_raw_video_test(gst_video::VideoFormat::I420, 320, 240, 84);
}

#[test]
fn test_rtpvraw_y41b() {
    run_raw_video_test(gst_video::VideoFormat::Y41b, 320, 240, 84);
}

#[test]
fn test_rtpvraw_uyvp() {
    run_raw_video_test(gst_video::VideoFormat::Uyvp, 320, 240, 140);
}
