// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

#[cfg(any(feature = "jpeg", feature = "tga"))]
use gst::prelude::*;
#[cfg(any(feature = "jpeg", feature = "tga"))]
use gst_video::prelude::*;

#[cfg(any(feature = "jpeg", feature = "tga"))]
fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstimagers::plugin_register_static().expect("image-rs test");
    });
}

#[cfg(feature = "jpeg")]
#[test]
fn test_aligned() {
    init();

    let rgba_image_one = include_bytes!("files/smpte-rp-219.jpg");
    let rgba_image_two = image::load_from_memory_with_format(
        include_bytes!("files/smpte-rp-219.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_rgb8();

    let mut h = gst_check::Harness::new("imagersdec");
    h.set_src_caps(gst::Caps::builder(image::ImageFormat::Jpeg.to_mime_type()).build());
    {
        let buf = gst::Buffer::from_slice(rgba_image_one);
        h.push(buf).unwrap();
    }
    let framebuffer = h
        .pull()
        .unwrap()
        .into_mapped_buffer_readable()
        .unwrap()
        .to_vec();
    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();

    assert_eq!(video_info.format(), gst_video::VideoFormat::Rgb);
    assert_eq!(video_info.width(), 160);
    assert_eq!(video_info.height(), 120);
    assert_eq!(video_info.fps(), gst::Fraction::new(0, 1));

    let image_one = image::RgbImage::from_raw(160, 120, framebuffer).unwrap();

    let result = image_compare::rgb_hybrid_compare(&image_one, &rgba_image_two)
        .expect("Images had different dimensions");
    assert!(result.score >= 0.95);
}

#[cfg(feature = "tga")]
#[test]
fn test_misaligned() {
    use image::GenericImage;

    init();

    let rgba_image_one = include_bytes!("files/smpte-rp-219-misaligned.tga");
    let rgba_image_two = image::load_from_memory_with_format(
        include_bytes!("files/smpte-rp-219-misaligned.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_rgb8();

    let mut h = gst_check::Harness::new("imagersdec");
    h.set_src_caps(gst::Caps::builder(image::ImageFormat::Tga.to_mime_type()).build());
    {
        let buf = gst::Buffer::from_slice(rgba_image_one);
        h.push(buf).unwrap();
    }
    let buffer = h.pull().unwrap();
    let caps = h.sinkpad().and_then(|v| v.current_caps()).unwrap();
    let video_info = gst_video::VideoInfo::from_caps(caps.as_ref()).unwrap();
    let input_frame = gst_video::VideoFrame::from_buffer_readable(buffer, &video_info).unwrap();

    assert_eq!(input_frame.format(), gst_video::VideoFormat::Rgb);
    assert_eq!(input_frame.width(), 163);
    assert_eq!(input_frame.height(), 121);
    assert_eq!(video_info.fps(), gst::Fraction::new(0, 1));

    let layout = image::flat::SampleLayout {
        channels: input_frame.n_components().try_into().unwrap(),
        // Planar format (contiguous channels)
        channel_stride: 1,
        width: input_frame.width(),
        width_stride: input_frame.comp_pstride(0).try_into().unwrap(),
        height: input_frame.height(),
        height_stride: input_frame.comp_stride(0).try_into().unwrap(),
    };

    let image_one = if layout.is_normal(image::flat::NormalForm::RowMajorPacked) {
        image::RgbImage::from_raw(
            input_frame.width(),
            input_frame.height(),
            input_frame.buffer().map_readable().unwrap().to_vec(),
        )
        .unwrap()
    } else {
        let container = image::FlatSamples {
            samples: input_frame.buffer().map_readable().unwrap(),
            layout,
            // Do not initialize color type, this is stride governed
            color_hint: None,
        };

        let view = container.as_view::<image::Rgb<u8>>().unwrap();

        let mut image = image::GenericImageView::buffer_like(&view);

        image
            .copy_from(&view, 0, 0)
            .expect("Image buffer too small");

        image
    };

    assert_eq!(image_one.width(), rgba_image_two.width());
    assert_eq!(image_one.height(), rgba_image_two.height());

    let result = image_compare::rgb_hybrid_compare(&image_one, &rgba_image_two).unwrap();
    assert!(result.score >= 0.95);
}
