// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstimagers::plugin_register_static().expect("image-rs test");
    });
}

fn create_overlay() -> (gst::Element, gst_check::Harness) {
    let overlay = gst::ElementFactory::make("imagersoverlay")
        .property("location", "tests/files/red-box.png")
        .build()
        .unwrap();

    let mut h = gst_check::Harness::with_element(&overlay, Some("sink"), Some("src"));
    h.add_src_parse("videotestsrc pattern=blue is-live=1 ! capsfilter caps=\"video/x-raw,width=720,height=480,format=RGB\"", true);

    (overlay, h)
}

#[test]
fn test_meta_composition() {
    init();

    let (_o, mut h) = create_overlay();
    h.add_propose_allocation_meta(gst_video::VideoOverlayCompositionMeta::meta_api(), None);
    h.push_from_src().unwrap();
    let framebuffer = h.pull().unwrap();
    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgb);
    assert_eq!(video_info.width(), 720);
    assert_eq!(video_info.height(), 480);

    assert!(
        framebuffer
            .meta::<gst_video::VideoOverlayCompositionMeta>()
            .is_some()
    );
    let composition = framebuffer
        .meta::<gst_video::VideoOverlayCompositionMeta>()
        .unwrap();
    assert_eq!(composition.overlay().n_rectangles(), 1);
    assert_eq!(
        composition
            .overlay()
            .rectangle(0)
            .unwrap()
            .render_rectangle(),
        (0, 0, 100, 100)
    );
}

#[test]
fn test_blending() {
    init();

    let blended_buffer = image::load_from_memory_with_format(
        include_bytes!("files/red-on-blue.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_rgb8();

    let (_o, mut h) = create_overlay();
    h.push_from_src().unwrap();
    let framebuffer = h
        .pull()
        .unwrap()
        .into_mapped_buffer_readable()
        .unwrap()
        .to_vec();
    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgb);
    assert_eq!(video_info.width(), 720);
    assert_eq!(video_info.height(), 480);

    let image_one = image::RgbImage::from_raw(720, 480, framebuffer).unwrap();

    let result = image_compare::rgb_hybrid_compare(&image_one, &blended_buffer)
        .expect("Images had different dimensions");
    assert!(result.score >= 0.95);
}
