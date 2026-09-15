// SPDX-License-Identifier: MPL-2.0

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline_and_validate_buffer};
use anyhow::bail;
use gst::prelude::*;
use gst_check::Harness;
use gst_video::prelude::*;
use std::str::FromStr;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpvraw test");
    });
}

#[allow(clippy::manual_div_ceil)]
fn calc_active_bytes_per_line(video_info: &gst_video::VideoInfo) -> [usize; 4] {
    use gst_video::VideoFormat::*;

    let width = video_info.width() as usize;

    match video_info.format() {
        Rgb | Rgba | Bgr | Bgra | Gray8 | Gray16Be | V308 | Uyvy => {
            let pstride = video_info.comp_pstride(0) as usize;
            [pstride * width, 0, 0, 0]
        }
        I420 | Uyvp => {
            // 4:2:x
            [
                width,
                width.next_multiple_of(2) / 2,
                width.next_multiple_of(2) / 2,
                0,
            ]
        }
        Y41b => {
            // 4:1:x
            [
                width,
                width.next_multiple_of(4) / 4,
                width.next_multiple_of(4) / 4,
                0,
            ]
        }
        fmt => todo!("implement for {fmt}"),
    }
}

fn create_test_frame(video_info: &gst_video::VideoInfo, frame_idx: u64) -> gst::Buffer {
    let size = video_info.size();
    let mut buffer = gst::Buffer::with_size(size).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::from_seconds(frame_idx));

        let mut frame =
            gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, video_info).unwrap();

        let n_active_bytes_per_line = calc_active_bytes_per_line(video_info);

        // Fill with an increasing bit pattern that can be checked again later
        let mut idx = frame_idx;
        for plane_idx in 0..frame.n_planes() {
            let stride = frame.plane_stride()[plane_idx as usize] as usize;
            let plane = frame.plane_data_mut(plane_idx).unwrap();

            for line in plane.chunks_mut(stride) {
                // Skip padding at the end of each line
                let n_active_bytes = n_active_bytes_per_line[plane_idx as usize];

                for b in line[0..n_active_bytes].iter_mut() {
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

    let n_active_bytes_per_line = calc_active_bytes_per_line(video_info);

    let mut idx = frame_idx;
    for plane_idx in 0..frame.n_planes() {
        let stride = frame.plane_stride()[plane_idx as usize] as usize;
        let plane = frame.plane_data(plane_idx).unwrap();

        for (y, line) in plane.chunks(stride).enumerate() {
            // Skip padding at the end of each line
            let n_active_bytes = n_active_bytes_per_line[plane_idx as usize];

            for (x, b) in line[0..n_active_bytes].iter().enumerate() {
                let expected_byte = (idx & 0xff) as u8;
                let actual_byte = *b;

                if actual_byte != expected_byte {
                    bail!(
                        "Plane {plane_idx}: Expected byte {expected_byte} at position ({x}, {y})\
                        but got {actual_byte}, stride={stride}, active_bytes={n_active_bytes}",
                    );
                }
                idx = idx.wrapping_add(1);
            }
        }
    }

    Ok(())
}

#[track_caller]
fn run_raw_video_test(
    format: gst_video::VideoFormat,
    width: u32,
    height: u32,
    expected_packets_per_frame: usize,
) {
    run_raw_video_test_with_descriptions(
        format,
        width,
        height,
        "rtpvrawpay2",
        "rtpvrawdepay2",
        expected_packets_per_frame,
        false,
    )
}

#[track_caller]
fn run_raw_video_test_with_descriptions(
    format: gst_video::VideoFormat,
    width: u32,
    height: u32,
    pay_descr: &str,
    depay_descr: &str,
    expected_packets_per_frame: usize,
    skip_first_frame_check: bool,
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
        pay_descr,
        depay_descr,
        expected_pay,
        expected_depay,
        move |buffer, list_idx, buffer_idx| {
            if buffer_idx != 0 {
                bail!("Got multiple output buffers per frame");
            }

            if list_idx >= 3 {
                bail!("Too many frames (got {}, expected 3)", list_idx + 1);
            }

            if list_idx > 0 || !skip_first_frame_check {
                check_test_frame(buffer, &video_info, list_idx as u64)
            } else {
                Ok(())
            }
        },
    );
}

#[test]
fn test_rtpvraw_rgb() {
    run_raw_video_test(gst_video::VideoFormat::Rgb, 320, 240, 168);
    run_raw_video_test(gst_video::VideoFormat::Rgb, 320, 241, 169);
    run_raw_video_test(gst_video::VideoFormat::Rgb, 320, 239, 168);
    run_raw_video_test(gst_video::VideoFormat::Rgb, 321, 240, 169);
    run_raw_video_test(gst_video::VideoFormat::Rgb, 319, 240, 168);
    run_raw_video_test(gst_video::VideoFormat::Rgb, 321, 241, 170);
    run_raw_video_test(gst_video::VideoFormat::Rgb, 319, 239, 167);
}

#[test]
fn test_rtpvraw_bgr() {
    run_raw_video_test(gst_video::VideoFormat::Bgr, 320, 240, 168);
    run_raw_video_test(gst_video::VideoFormat::Bgr, 320, 241, 169);
    run_raw_video_test(gst_video::VideoFormat::Bgr, 320, 239, 168);
    run_raw_video_test(gst_video::VideoFormat::Bgr, 321, 240, 169);
    run_raw_video_test(gst_video::VideoFormat::Bgr, 319, 240, 168);
    run_raw_video_test(gst_video::VideoFormat::Bgr, 321, 241, 170);
    run_raw_video_test(gst_video::VideoFormat::Bgr, 319, 239, 167);
}

#[test]
fn test_rtpvraw_rgba() {
    run_raw_video_test(gst_video::VideoFormat::Rgba, 320, 240, 224);
    run_raw_video_test(gst_video::VideoFormat::Rgba, 320, 241, 225);
    run_raw_video_test(gst_video::VideoFormat::Rgba, 320, 239, 224);
    run_raw_video_test(gst_video::VideoFormat::Rgba, 321, 240, 225);
    run_raw_video_test(gst_video::VideoFormat::Rgba, 319, 240, 224);
    run_raw_video_test(gst_video::VideoFormat::Rgba, 321, 241, 226);
    run_raw_video_test(gst_video::VideoFormat::Rgba, 319, 239, 223);
}

#[test]
fn test_rtpvraw_bgra() {
    run_raw_video_test(gst_video::VideoFormat::Bgra, 320, 240, 224);
    run_raw_video_test(gst_video::VideoFormat::Bgra, 320, 241, 225);
    run_raw_video_test(gst_video::VideoFormat::Bgra, 320, 239, 224);
    run_raw_video_test(gst_video::VideoFormat::Bgra, 321, 240, 225);
    run_raw_video_test(gst_video::VideoFormat::Bgra, 319, 240, 224);
    run_raw_video_test(gst_video::VideoFormat::Bgra, 321, 241, 226);
    run_raw_video_test(gst_video::VideoFormat::Bgra, 319, 239, 223);
}

#[test]
fn test_rtpvraw_gray8() {
    run_raw_video_test(gst_video::VideoFormat::Gray8, 320, 240, 57);
    run_raw_video_test(gst_video::VideoFormat::Gray8, 320, 241, 57);
    run_raw_video_test(gst_video::VideoFormat::Gray8, 320, 239, 57);
    run_raw_video_test(gst_video::VideoFormat::Gray8, 321, 240, 57);
    run_raw_video_test(gst_video::VideoFormat::Gray8, 319, 240, 57);
    run_raw_video_test(gst_video::VideoFormat::Gray8, 321, 241, 58);
    run_raw_video_test(gst_video::VideoFormat::Gray8, 319, 239, 57);
    run_raw_video_test(gst_video::VideoFormat::Gray8, 640, 480, 225);
}

#[test]
fn test_rtpvraw_gray16() {
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 320, 240, 113);
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 320, 241, 113);
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 320, 239, 112);
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 321, 240, 113);
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 319, 240, 112);
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 321, 241, 114);
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 319, 239, 112);
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 640, 480, 448);
    // Example mentionned in DEF STAN 00-082 § B.6.1
    run_raw_video_test(gst_video::VideoFormat::Gray16Be, 640, 512, 478);
}

#[test]
fn test_rtpvraw_v308() {
    run_raw_video_test(gst_video::VideoFormat::V308, 320, 240, 168);
    run_raw_video_test(gst_video::VideoFormat::V308, 320, 241, 169);
    run_raw_video_test(gst_video::VideoFormat::V308, 320, 239, 168);
    run_raw_video_test(gst_video::VideoFormat::V308, 321, 240, 169);
    run_raw_video_test(gst_video::VideoFormat::V308, 319, 240, 168);
    run_raw_video_test(gst_video::VideoFormat::V308, 321, 241, 170);
    run_raw_video_test(gst_video::VideoFormat::V308, 319, 239, 167);
}

#[test]
fn test_rtpvraw_uyvy() {
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 320, 240, 113);
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 320, 241, 113);
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 320, 239, 112);
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 321, 240, 114);
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 319, 240, 113);
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 321, 241, 114);
    run_raw_video_test(gst_video::VideoFormat::Uyvy, 319, 239, 112);
}

#[test]
fn test_rtpvraw_i420() {
    run_raw_video_test(gst_video::VideoFormat::I420, 320, 240, 84);
    run_raw_video_test(gst_video::VideoFormat::I420, 320, 241, 85);
    run_raw_video_test(gst_video::VideoFormat::I420, 320, 239, 84);
    run_raw_video_test(gst_video::VideoFormat::I420, 321, 240, 85);
    run_raw_video_test(gst_video::VideoFormat::I420, 319, 240, 84);
    run_raw_video_test(gst_video::VideoFormat::I420, 321, 241, 86);
    run_raw_video_test(gst_video::VideoFormat::I420, 319, 239, 84);
}

#[test]
fn test_rtpvraw_y41b() {
    run_raw_video_test(gst_video::VideoFormat::Y41b, 320, 240, 85);
    run_raw_video_test(gst_video::VideoFormat::Y41b, 320, 241, 85);
    run_raw_video_test(gst_video::VideoFormat::Y41b, 320, 239, 85);
    run_raw_video_test(gst_video::VideoFormat::Y41b, 321, 240, 86);
    run_raw_video_test(gst_video::VideoFormat::Y41b, 319, 240, 85);
    run_raw_video_test(gst_video::VideoFormat::Y41b, 321, 241, 86);
    run_raw_video_test(gst_video::VideoFormat::Y41b, 319, 239, 85);
}

#[test]
fn test_rtpvraw_uyvp() {
    run_raw_video_test(gst_video::VideoFormat::Uyvp, 320, 240, 141);
    run_raw_video_test(gst_video::VideoFormat::Uyvp, 320, 241, 142);
    run_raw_video_test(gst_video::VideoFormat::Uyvp, 320, 239, 140);

    // Some versions of GStreamer (< 1.28.2) have a too-small default stride for odd widths
    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Uyvp, 321, 240)
        .build()
        .unwrap();

    if video_info.stride()[0] >= 805 {
        run_raw_video_test(gst_video::VideoFormat::Uyvp, 321, 240, 142);
        run_raw_video_test(gst_video::VideoFormat::Uyvp, 319, 240, 141);
        run_raw_video_test(gst_video::VideoFormat::Uyvp, 321, 241, 142);
        run_raw_video_test(gst_video::VideoFormat::Uyvp, 319, 239, 140);
    } else {
        eprintln!("Skipping test, libgstvideo has too small strides for odd widths for UYVP");
    }
}

#[test]
fn test_rtpvraw_bt2100_reads_tcs() {
    init();

    let mut h = Harness::new("rtpvrawdepay2");
    h.play();
    h.set_src_caps(
        gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90000i32)
            .field("encoding-name", "RAW")
            .field("payload", 96i32)
            .field("sampling", "YCbCr-4:2:2")
            .field("depth", "10")
            .field("width", "1920")
            .field("height", "1080")
            .field("colorimetry", "BT2100")
            .field("tcs", "HLG")
            .build(),
    );

    let element = h.element().unwrap();
    let caps = element.static_pad("src").unwrap().current_caps().unwrap();
    let s = caps.structure(0).unwrap();
    assert_eq!(s.get::<&str>("colorimetry"), Ok("bt2100-hlg"));

    drop(h);
    let _ = element.set_state(gst::State::Null);
}

#[test]
fn test_rtpvraw_bt2100_defaults_tcs_to_sdr() {
    init();

    let mut h = Harness::new("rtpvrawdepay2");
    h.play();
    h.set_src_caps(
        gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90000i32)
            .field("encoding-name", "RAW")
            .field("payload", 96i32)
            .field("sampling", "YCbCr-4:2:2")
            .field("depth", "10")
            .field("width", "1920")
            .field("height", "1080")
            .field("colorimetry", "BT2100")
            // No TCS: ST 2110-20 omission default is SDR (not PQ).
            .build(),
    );

    let element = h.element().unwrap();
    let caps = element.static_pad("src").unwrap().current_caps().unwrap();
    let s = caps.structure(0).unwrap();
    // BT2100 + SDR has no named GStreamer preset; stringifies as bt2020-10 at depth 10.
    assert_eq!(s.get::<&str>("colorimetry"), Ok("bt2020-10"));

    drop(h);
    let _ = element.set_state(gst::State::Null);
}

#[test]
fn test_rtpvraw_bt2100_writes_tcs_and_range() {
    init();

    // Caps-only: no buffers needed to negotiate RTP colorimetry/TCS/RANGE.
    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Uyvp, 1920, 1080)
        .fps(gst::Fraction::new(25, 1))
        .colorimetry(&gst_video::VideoColorimetry::from_str("bt2100-hlg").unwrap())
        .build()
        .unwrap();

    let mut h = Harness::new("rtpvrawpay2");
    h.play();
    h.set_src_caps(video_info.to_caps().unwrap());

    let element = h.element().unwrap();
    let caps = element.static_pad("src").unwrap().current_caps().unwrap();
    let s = caps.structure(0).unwrap();
    assert_eq!(s.get::<&str>("colorimetry"), Ok("BT2100"));
    assert_eq!(s.get::<&str>("tcs"), Ok("HLG"));
    assert_eq!(s.get::<&str>("range"), Ok("NARROW"));

    drop(h);
    let _ = element.set_state(gst::State::Null);
}

#[test]
fn test_rtpvraw_reads_range_full() {
    init();

    let mut h = Harness::new("rtpvrawdepay2");
    h.play();
    h.set_src_caps(
        gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90000i32)
            .field("encoding-name", "RAW")
            .field("payload", 96i32)
            .field("sampling", "YCbCr-4:2:2")
            .field("depth", "10")
            .field("width", "1920")
            .field("height", "1080")
            .field("colorimetry", "BT709")
            .field("tcs", "SDR")
            .field("range", "FULL")
            .build(),
    );

    let element = h.element().unwrap();
    let caps = element.static_pad("src").unwrap().current_caps().unwrap();
    let colorimetry = caps
        .structure(0)
        .unwrap()
        .get::<&str>("colorimetry")
        .unwrap();
    assert_eq!(
        gst_video::VideoColorimetry::from_str(colorimetry)
            .unwrap()
            .range(),
        gst_video::VideoColorRange::Range0_255
    );

    drop(h);
    let _ = element.set_state(gst::State::Null);
}

#[test]
fn test_depay_ancillary_and_active_lines() {
    use super::line_numbering::{
        HD_720P_ACTIVE_HEIGHT, HD_720P_ACTIVE_WIDTH, HD_720P_FIRST_ACTIVE, HD_720P_LAST_ACTIVE,
    };
    use super::pay::packing_template::{VRAW_CHUNK_HDR_LEN, VRAW_EXT_SEQNUM_LEN};
    use super::pixel_group::PixelGroup;
    use smallvec::SmallVec;
    use std::io::Write;

    const HD_720P_FIRST_ANC: u32 = 7;
    const MTU: usize = 1400;
    const MAX_CHUNK_PER_PACKET: usize = 2;
    const MAX_VRAW_HEADER_LEN: usize =
        VRAW_EXT_SEQNUM_LEN + MAX_CHUNK_PER_PACKET * VRAW_CHUNK_HDR_LEN;
    const MAX_PAYLOAD_LEN: usize =
        MTU - rtp_types::RtpPacket::MIN_RTP_PACKET_LEN - MAX_VRAW_HEADER_LEN;

    const CLOCK_RATE: u32 = 90_000;

    init();

    let video_info = gst_video::VideoInfo::builder(
        gst_video::VideoFormat::Rgb,
        HD_720P_ACTIVE_WIDTH,
        HD_720P_ACTIVE_HEIGHT,
    )
    .build()
    .unwrap();
    let pgroup = PixelGroup::from_video_info(&video_info).unwrap();

    // The rtpvrawpay2 doesn't support this configuration
    // => manually build packets for 3 frames containing ancillary & active lines
    let mut ext_seqnum = 0u32;
    let mut packets = vec![];
    for frame_idx in 0..3 {
        struct Packet {
            ext_seqnum: u32,
            vraw_header: SmallVec<[u8; MAX_VRAW_HEADER_LEN]>,
            payload: Vec<u8>,
            payload_offset: usize,
        }

        impl Packet {
            fn new(ext_seqnum: u32) -> Self {
                let mut vraw_header = SmallVec::<[u8; MAX_VRAW_HEADER_LEN]>::new();
                vraw_header.extend(((ext_seqnum >> 16) as u16).to_be_bytes());
                Packet {
                    ext_seqnum,
                    vraw_header,
                    payload: vec![0; MAX_PAYLOAD_LEN],
                    payload_offset: 0,
                }
            }

            fn rem_payload_mut(&mut self) -> &mut [u8] {
                &mut self.payload[self.payload_offset..]
            }

            fn payload(&self) -> &[u8] {
                &self.payload[..self.payload_offset]
            }
        }

        let mut x = 0;
        let mut y = HD_720P_FIRST_ANC as usize;

        let mut packet_opt = None;

        while y <= HD_720P_LAST_ACTIVE as usize {
            let packet = packet_opt.get_or_insert_with(|| Packet::new(ext_seqnum));

            let rem_payload = packet.rem_payload_mut();
            let rem_payload_len = rem_payload.len();

            assert!(rem_payload_len >= pgroup.size());

            let take_packet;
            let is_last;
            if x == 0 {
                // For each new line:
                // * byte 0: frame nb
                // * bytes 1 & 2 (BE u16): scan line number
                rem_payload[0] = frame_idx;
                let _ = (&mut rem_payload[1..=2])
                    .write(&(y as u16).to_be_bytes())
                    .unwrap();
            }

            let rem_pgroups_capacity = rem_payload_len / pgroup.size();
            let rem_pgroups_for_line = (HD_720P_ACTIVE_WIDTH as usize - x) / pgroup.x_inc();
            if rem_pgroups_for_line > rem_pgroups_capacity {
                // rem of the line doesn't fit in remaining payload
                let len = rem_pgroups_capacity * pgroup.size();
                packet.vraw_header.extend((len as u16).to_be_bytes());
                packet.payload_offset += len;

                take_packet = true;
                is_last = false;

                packet.vraw_header.extend((y as u16).to_be_bytes());
                // no Continuation bit in this case
                packet.vraw_header.extend((x as u16).to_be_bytes());

                x += rem_pgroups_capacity * pgroup.x_inc();
            } else {
                // terminate current line
                let len = rem_pgroups_for_line * pgroup.size();
                packet.vraw_header.extend((len as u16).to_be_bytes());
                packet.payload_offset += len;

                is_last = y == HD_720P_LAST_ACTIVE as usize;
                take_packet = packet.rem_payload_mut().len() < pgroup.size() || is_last;

                // ensure we get the end of an ancillary line chunk and
                // the first active line chunk in the same packet
                // so as to test this edge case
                if y + pgroup.y_inc() == HD_720P_FIRST_ACTIVE as usize {
                    assert!(!take_packet);
                }

                packet.vraw_header.extend((y as u16).to_be_bytes());
                let continuation_flag = if take_packet { 0 } else { 1 << 15 };
                packet
                    .vraw_header
                    .extend(((continuation_flag + x) as u16).to_be_bytes());

                y += pgroup.y_inc();
                x = 0;
            }

            if take_packet && let Some(packet) = packet_opt.take() {
                let rtp_packet_builder = rtp_types::RtpPacketBuilder::new()
                    .payload_type(96)
                    .sequence_number((packet.ext_seqnum & 0xffff) as u16)
                    .timestamp(CLOCK_RATE * frame_idx as u32)
                    .ssrc(1234)
                    .marker_bit(is_last)
                    .payload(packet.vraw_header.as_slice())
                    .payload(packet.payload());

                let packet_len = rtp_packet_builder.calculate_size().unwrap();
                let mut buf = gst::Buffer::with_size(packet_len).unwrap();
                {
                    let buf_mut = buf.make_mut();
                    buf_mut.set_pts((frame_idx as u64).seconds());

                    let mut buf_mapped = buf_mut.map_writable().unwrap();
                    rtp_packet_builder
                        .write_into(buf_mapped.as_mut_slice())
                        .unwrap();
                }
                packets.push(buf);

                ext_seqnum += 1;
            }
        }
    }

    let mut h = gst_check::Harness::new("rtpvrawdepay2");
    h.play();

    h.push_event(gst::event::StreamStart::new("test"));
    h.push_event(gst::event::Caps::new(
        &gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", CLOCK_RATE as i32)
            .field("encoding-name", "RAW")
            .field(
                "sampling",
                match video_info.format() {
                    gst_video::VideoFormat::Rgb => "RGB",
                    gst_video::VideoFormat::I420 => "YCbCr-4:2:0",
                    _ => unreachable!(),
                },
            )
            .field("width", HD_720P_ACTIVE_WIDTH.to_string())
            .field("height", HD_720P_ACTIVE_HEIGHT.to_string())
            .field("depth", "8")
            .build(),
    ));
    h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
        gst::format::Time,
    >::new()));

    let mut frame_idx = 0;
    for packet in packets.drain(..) {
        h.push(packet).unwrap();

        if let Some(buf) = h.try_pull() {
            let frame =
                gst_video::VideoFrameRef::from_buffer_ref_readable(&buf, &video_info).unwrap();

            for plane_idx in 0..frame.n_planes() {
                let stride = frame.plane_stride()[plane_idx as usize] as usize;
                let plane = frame.plane_data(plane_idx).unwrap();

                for (y, line) in plane.chunks(stride).enumerate() {
                    assert_eq!(frame_idx, line[0]);

                    let scan_line = u16::from_be_bytes(line[1..=2].try_into().unwrap()) as usize;

                    if frame_idx > 0 {
                        // numbering scheme identified
                        assert_eq!(y, scan_line - HD_720P_FIRST_ACTIVE as usize);
                    } else {
                        // numbering scheme not identified yet
                        match y as u32 {
                            0..HD_720P_FIRST_ACTIVE => {
                                // blank line
                                assert_eq!(0, scan_line);
                            }
                            HD_720P_FIRST_ACTIVE..HD_720P_ACTIVE_HEIGHT => {
                                // safe to display, but offset
                                assert_eq!(y, scan_line);
                            }
                            _ => unreachable!("out of range"),
                        }
                    }
                }
            }

            frame_idx += 1;
        }
    }
}

#[test]
fn test_rtpvraw_vesa_numbering() {
    // skip first frame checks as it takes 1 frame to infer line numbering scheme
    run_raw_video_test_with_descriptions(
        gst_video::VideoFormat::Rgb,
        320,
        240,
        "rtpvrawpay2 line-numbering-scheme=vesa",
        "rtpvrawdepay2 line-numbering-identification-method=infer",
        168,
        true,
    );
    run_raw_video_test_with_descriptions(
        gst_video::VideoFormat::Rgb,
        640,
        480,
        "rtpvrawpay2 line-numbering-scheme=vesa",
        "rtpvrawdepay2 line-numbering-identification-method=infer",
        670,
        true,
    );
    run_raw_video_test_with_descriptions(
        gst_video::VideoFormat::I420,
        640,
        480,
        "rtpvrawpay2 line-numbering-scheme=vesa",
        "rtpvrawdepay2 line-numbering-identification-method=infer",
        335,
        true,
    );
}

#[test]
fn test_rtpvraw_smpte_numbering() {
    use super::line_numbering::{HD_720P_ACTIVE_HEIGHT, HD_720P_ACTIVE_WIDTH};
    // elligble resolution for ancillary offset
    run_raw_video_test_with_descriptions(
        gst_video::VideoFormat::Rgb,
        HD_720P_ACTIVE_WIDTH,
        HD_720P_ACTIVE_HEIGHT,
        "rtpvrawpay2 line-numbering-scheme=smpte",
        "rtpvrawdepay2 line-numbering-identification-method=infer",
        2007,
        true,
    );
    run_raw_video_test_with_descriptions(
        gst_video::VideoFormat::I420,
        HD_720P_ACTIVE_WIDTH,
        HD_720P_ACTIVE_HEIGHT,
        "rtpvrawpay2 line-numbering-scheme=smpte",
        "rtpvrawdepay2 line-numbering-identification-method=infer",
        1004,
        true,
    );
}

#[test]
fn test_rtpvraw_smpte_numbering_resolution_mismatch() {
    // skip first frame checks as it takes 1 frame to infer line numbering scheme
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    init();

    let mut h = gst_check::Harness::new("rtpvrawpay2");
    h.element()
        .unwrap()
        .set_property_from_str("line-numbering-scheme", "smpte");
    h.play();

    assert!(h.push_event(gst::event::StreamStart::new("test")));
    assert!(
        h.push_event(gst::event::Caps::new(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGB")
                .field("width", WIDTH as i32)
                .field("height", HEIGHT as i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .field("interlace-mode", "progressive")
                .build(),
        ))
    );
    assert!(
        h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::format::Time,
        >::new()))
    );
    assert_eq!(
        Err(gst::FlowError::NotNegotiated),
        h.push(gst::Buffer::new())
    );
}
