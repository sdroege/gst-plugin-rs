// SPDX-License-Identifier: MPL-2.0

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline};
use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpvorbispay test");
    });
}

#[test]
fn test_vorbis() {
    init();

    let src = "audiotestsrc num-buffers=100 samplesperbuffer=1024 wave=silence ! audio/x-raw,rate=48000,channels=2 ! vorbisenc";
    // 45ms = a bit more than two vorbis frames of 1024 samples
    let pay = "rtpvorbispay2 max-ptime=45000000";
    let depay = "rtpvorbisdepay2";

    let mut expected_pay = Vec::with_capacity(51);
    for i in 0..51 {
        let position = match i {
            0 => 0u64, // 0 + 576 + 1024 = 1600 samples duration
            _ if i < 50 => 1600 + (i - 1) * 1024 * 2,
            50 => 1600 + 49 * 1024 * 2,
            _ => unreachable!(),
        };

        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_nseconds(
                    position
                        .mul_div_floor(*gst::ClockTime::SECOND, 48_000)
                        .unwrap(),
                ))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT
                } else {
                    gst::BufferFlags::empty()
                })
                .rtp_time((position & 0xffff_ffff) as u32)
                .marker_bit(false)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(51);
    for i in 0..51 {
        match i {
            0 => {
                // 3 headers and 3 Vorbis frames
                let mut list = Vec::with_capacity(6);
                for j in 0..6 {
                    list.push(
                        ExpectedBuffer::builder()
                            .maybe_pts(if j < 4 {
                                Some(gst::ClockTime::ZERO)
                            } else {
                                None
                            })
                            .flags(if j == 0 {
                                gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER
                            } else if j < 3 {
                                gst::BufferFlags::HEADER
                            } else {
                                gst::BufferFlags::empty()
                            })
                            .build(),
                    );
                }

                expected_depay.push(list);
            }
            _ if i < 50 => {
                // 2 Vorbis frames per list
                let position = 1600 + (i - 1) * 1024 * 2;

                let mut list = Vec::with_capacity(2);
                for j in 0..2 {
                    list.push(
                        ExpectedBuffer::builder()
                            .maybe_pts(if j == 0 {
                                Some(gst::ClockTime::from_nseconds(
                                    position
                                        .mul_div_floor(*gst::ClockTime::SECOND, 48_000)
                                        .unwrap(),
                                ))
                            } else {
                                None
                            })
                            .build(),
                    );
                }

                expected_depay.push(list);
            }
            50 => {
                let position = 1600 + (i - 1) * 1024 * 2;
                expected_depay.push(vec![
                    ExpectedBuffer::builder()
                        .pts(gst::ClockTime::from_nseconds(
                            position
                                .mul_div_floor(*gst::ClockTime::SECOND, 48_000)
                                .unwrap(),
                        ))
                        .build(),
                ]);
            }
            _ => unreachable!(),
        }
    }

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_vorbis_inband_headers() {
    init();

    let src = "audiotestsrc num-buffers=100 samplesperbuffer=1024 wave=silence ! audio/x-raw,rate=48000,channels=2 ! vorbisenc";
    // 45ms = a bit more than two vorbis frames of 1024 samples
    let pay = "rtpvorbispay2 max-ptime=45000000 config-interval=1";
    // use a capssetter to drop any additional out of band signalling and only rely on in-band data
    let depay = "capssetter replace=true join=false caps=application/x-rtp,media=audio,encoding-name=VORBIS,clock-rate=48000 ! rtpvorbisdepay2";

    let mut expected_pay = Vec::with_capacity(51);
    for i in 0..51 {
        let position = match i {
            0 => {
                // 4 packets with inline headers (3960 bytes of headers in total),
                // plus first packet with 0 + 576 + 1024 = 1600 samples duration
                let mut list = Vec::with_capacity(5);
                for j in 0..5 {
                    list.push(
                        ExpectedPacket::builder()
                            .pts(gst::ClockTime::ZERO)
                            .flags(if j == 0 {
                                gst::BufferFlags::DISCONT
                            } else {
                                gst::BufferFlags::empty()
                            })
                            .rtp_time(0)
                            .marker_bit(false)
                            .build(),
                    );
                }
                expected_pay.push(list);
                continue;
            }
            _ if i < 50 => 1600 + (i - 1) * 1024 * 2,
            50 => 1600 + 49 * 1024 * 2,
            _ => unreachable!(),
        };

        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_nseconds(
                    position
                        .mul_div_floor(*gst::ClockTime::SECOND, 48_000)
                        .unwrap(),
                ))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT
                } else {
                    gst::BufferFlags::empty()
                })
                .rtp_time((position & 0xffff_ffff) as u32)
                .marker_bit(false)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(51);
    for i in 0..51 {
        match i {
            0 => {
                // 3 headers and 3 Vorbis frames
                let mut list = Vec::with_capacity(6);
                for j in 0..6 {
                    list.push(
                        ExpectedBuffer::builder()
                            .maybe_pts(if j < 4 {
                                Some(gst::ClockTime::ZERO)
                            } else {
                                None
                            })
                            .flags(if j == 0 {
                                gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER
                            } else if j < 3 {
                                gst::BufferFlags::HEADER
                            } else {
                                gst::BufferFlags::empty()
                            })
                            .build(),
                    );
                }

                expected_depay.push(list);
            }
            _ if i < 50 => {
                // 2 Vorbis frames per list
                let position = 1600 + (i - 1) * 1024 * 2;

                let mut list = Vec::with_capacity(2);
                for j in 0..2 {
                    list.push(
                        ExpectedBuffer::builder()
                            .maybe_pts(if j == 0 {
                                Some(gst::ClockTime::from_nseconds(
                                    position
                                        .mul_div_floor(*gst::ClockTime::SECOND, 48_000)
                                        .unwrap(),
                                ))
                            } else {
                                None
                            })
                            .build(),
                    );
                }

                expected_depay.push(list);
            }
            50 => {
                let position = 1600 + (i - 1) * 1024 * 2;
                expected_depay.push(vec![
                    ExpectedBuffer::builder()
                        .pts(gst::ClockTime::from_nseconds(
                            position
                                .mul_div_floor(*gst::ClockTime::SECOND, 48_000)
                                .unwrap(),
                        ))
                        .build(),
                ]);
            }
            _ => unreachable!(),
        }
    }

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}
