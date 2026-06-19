// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

#[cfg(any(feature = "gif", feature = "ico", feature = "png", feature = "webp"))]
use gst::prelude::*;

#[cfg(any(feature = "gif", feature = "ico", feature = "png", feature = "webp"))]
fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstimagers::plugin_register_static().expect("image-rs test");
    });
}

#[cfg(any(feature = "png", feature = "ico"))]
#[test]
fn test_aligned() {
    init();

    let rgb_image_one = include_bytes!("files/ball-aligned.png");
    let rgb_image_two = image::load_from_memory_with_format(
        include_bytes!("files/ball-single-frame.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_rgba8();

    let mut h = gst_check::Harness::new("animatedimagersdec");
    h.element()
        .unwrap()
        .set_property_from_str("loop", "force-no");
    h.set_src_caps(
        gst::Caps::builder(image::ImageFormat::Png.to_mime_type())
            .field("animated", true)
            .build(),
    );
    {
        let buf = gst::Buffer::from_slice(rgb_image_one);
        h.push(buf).unwrap();
        h.push_event(gst::event::Eos::new());
    }

    let mut expected_timestamp: Option<gst::ClockTime> = Some(gst::ClockTime::ZERO);
    let mut count = 0;
    let expected_duration: Option<gst::ClockTime> = Some(gst::ClockTime::SECOND / 15);

    while let Some(buf) = h.try_pull() {
        println!("{count}");
        assert_eq!(buf.pts(), expected_timestamp);
        assert_eq!(buf.duration(), expected_duration);

        if count == 0 {
            let framebuffer = buf.into_mapped_buffer_readable().unwrap().to_vec();

            let image_one = image::RgbaImage::from_raw(160, 120, framebuffer).unwrap();

            let result = image_compare::rgba_hybrid_compare(&image_one, &rgb_image_two)
                .expect("Images had different dimensions");
            assert!(result.score >= 0.95);
        }

        expected_timestamp = expected_timestamp.opt_add(expected_duration);
        count += 1;
    }

    assert_eq!(count, 75);

    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgba);
    assert_eq!(video_info.width(), 160);
    assert_eq!(video_info.height(), 120);
}

#[cfg(any(feature = "png", feature = "ico"))]
#[test]
fn test_misaligned() {
    init();

    let rgb_image_one = include_bytes!("files/smpte-misaligned-limited.png");
    let rgb_image_two = image::load_from_memory_with_format(
        include_bytes!("files/smpte-single-frame.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_rgba8();

    let mut h = gst_check::Harness::new("animatedimagersdec");
    h.element()
        .unwrap()
        .set_property_from_str("loop", "force-no");
    h.set_src_caps(
        gst::Caps::builder(image::ImageFormat::Png.to_mime_type())
            .field("animated", true)
            .build(),
    );
    {
        let buf = gst::Buffer::from_slice(rgb_image_one);
        h.push(buf).unwrap();
        h.push_event(gst::event::Eos::new());
    }

    let mut expected_timestamp: Option<gst::ClockTime> = Some(gst::ClockTime::ZERO);
    let mut count = 0;

    let expected_duration: Option<gst::ClockTime> = Some(gst::ClockTime::MSECOND * 250);

    while let Some(buf) = h.try_pull() {
        println!("{count}");
        assert_eq!(buf.pts(), expected_timestamp);
        assert_eq!(buf.duration(), expected_duration);

        if count == 0 {
            let framebuffer = buf.into_mapped_buffer_readable().unwrap().to_vec();

            let image_one = image::RgbaImage::from_raw(163, 121, framebuffer).unwrap();

            let result = image_compare::rgba_hybrid_compare(&image_one, &rgb_image_two)
                .expect("Images had different dimensions");
            assert!(result.score >= 0.95);
        }

        expected_timestamp = expected_timestamp.opt_add(expected_duration);
        count += 1;
    }

    assert_eq!(count, 20);

    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgba);
    assert_eq!(video_info.width(), 163);
    assert_eq!(video_info.height(), 121);
}

#[cfg(any(feature = "png", feature = "ico"))]
#[test]
fn test_infinite_loop() {
    use std::sync::{Arc, atomic::AtomicU16, atomic::Ordering};

    init();

    let rgb_image_one = include_bytes!("files/smpte-misaligned-limited.png");

    let mut h = gst_check::Harness::new("animatedimagersdec");
    h.element()
        .unwrap()
        .set_property_from_str("loop", "force-yes");
    h.set_src_caps(
        gst::Caps::builder(image::ImageFormat::Png.to_mime_type())
            .field("animated", true)
            .build(),
    );

    // Probe will EOS as soon as more than one loop is detected
    let posted_frame = Arc::new(AtomicU16::new(0));
    let posted_frame_clone = posted_frame.clone();
    h.add_probe(
        "animatedimagersdec",
        "src",
        gst::PadProbeType::BUFFER,
        move |p, _| {
            posted_frame_clone.fetch_add(1, Ordering::SeqCst);
            if posted_frame_clone.load(Ordering::SeqCst) >= 20 {
                let _ = p.push_event(gst::event::Eos::new());
            }
            gst::PadProbeReturn::Ok
        },
    );

    {
        let buf = gst::Buffer::from_slice(rgb_image_one);
        h.push(buf).unwrap();
        h.push_event(gst::event::Eos::new());
    }

    assert_eq!(posted_frame.load(Ordering::SeqCst), 20);

    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgba);
    assert_eq!(video_info.width(), 163);
    assert_eq!(video_info.height(), 121);
}

#[cfg(any(feature = "png", feature = "ico"))]
#[test]
fn test_limited_loop_apng() {
    init();

    let rgb_image_one = include_bytes!("files/smpte-misaligned-limited.png");

    let mut h = gst_check::Harness::new("animatedimagersdec");
    h.element().unwrap().set_property_from_str("loop", "image");
    h.set_src_caps(
        gst::Caps::builder(image::ImageFormat::Png.to_mime_type())
            .field("animated", true)
            .build(),
    );

    {
        let buf = gst::Buffer::from_slice(rgb_image_one);
        h.push(buf).unwrap();
        h.push_event(gst::event::Eos::new());
    }

    let mut count = 0;

    let expected_duration: Option<gst::ClockTime> = Some(gst::ClockTime::MSECOND * 250);
    let mut expected_timestamp: Option<gst::ClockTime> = Some(gst::ClockTime::ZERO);
    while let Some(buf) = h.try_pull() {
        assert_eq!(buf.pts(), expected_timestamp);
        assert_eq!(buf.duration(), expected_duration);

        expected_timestamp = expected_timestamp.opt_add(expected_duration);
        count += 1;
    }

    assert_eq!(count, 40);

    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgba);
    assert_eq!(video_info.width(), 163);
    assert_eq!(video_info.height(), 121);
}

#[cfg(feature = "gif")]
#[test]
fn test_limited_loop_gif() {
    init();

    let rgb_image_one = include_bytes!("files/smpte-misaligned-limited.gif");

    let mut h = gst_check::Harness::new("animatedimagersdec");
    h.element().unwrap().set_property_from_str("loop", "image");
    h.set_src_caps(
        gst::Caps::builder(image::ImageFormat::Gif.to_mime_type())
            .field("animated", true)
            .build(),
    );

    {
        let buf = gst::Buffer::from_slice(rgb_image_one);
        h.push(buf).unwrap();
        h.push_event(gst::event::Eos::new());
    }

    let mut count = 0;

    let expected_duration: Option<gst::ClockTime> = Some(gst::ClockTime::MSECOND * 250);
    let mut expected_timestamp: Option<gst::ClockTime> = Some(gst::ClockTime::ZERO);
    while let Some(buf) = h.try_pull() {
        assert_eq!(buf.pts(), expected_timestamp);
        assert_eq!(buf.duration(), expected_duration);

        expected_timestamp = expected_timestamp.opt_add(expected_duration);
        count += 1;
    }

    assert_eq!(count, 40);

    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgba);
    assert_eq!(video_info.width(), 163);
    assert_eq!(video_info.height(), 121);
}

#[cfg(feature = "webp")]
#[test]
fn test_limited_loop_webp() {
    init();

    let rgb_image_one = include_bytes!("files/smpte-misaligned-limited.webp");

    let mut h = gst_check::Harness::new("animatedimagersdec");
    h.element().unwrap().set_property_from_str("loop", "image");
    h.set_src_caps(
        gst::Caps::builder(image::ImageFormat::WebP.to_mime_type())
            .field("animated", true)
            .build(),
    );

    {
        let buf = gst::Buffer::from_slice(rgb_image_one);
        h.push(buf).unwrap();
        h.push_event(gst::event::Eos::new());
    }

    let mut count = 0;

    let expected_duration: Option<gst::ClockTime> = Some(gst::ClockTime::MSECOND * 250);
    let mut expected_timestamp: Option<gst::ClockTime> = Some(gst::ClockTime::ZERO);
    while let Some(buf) = h.try_pull() {
        assert_eq!(buf.pts(), expected_timestamp);
        assert_eq!(buf.duration(), expected_duration);

        expected_timestamp = expected_timestamp.opt_add(expected_duration);
        count += 1;
    }

    assert_eq!(count, 40);

    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgba);
    assert_eq!(video_info.width(), 163);
    assert_eq!(video_info.height(), 121);
}
