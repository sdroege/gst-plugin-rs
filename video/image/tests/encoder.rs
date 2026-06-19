// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

#[allow(dead_code)]
fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstimagers::plugin_register_static().expect("image-rs test");
    });
}

#[cfg(all(feature = "bmp", any(feature = "png", feature = "ico")))]
#[test]
fn test_check_imagers_similarity() {
    let gray_image_one = image::load_from_memory_with_format(
        include_bytes!("files/smpte-rp-219-gray.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_luma8();

    let gray_image_two = image::load_from_memory_with_format(
        include_bytes!("files/smpte-rp-219-gray.bmp"),
        image::ImageFormat::Bmp,
    )
    .unwrap()
    .into_luma8();

    let result = image_compare::gray_similarity_structure(
        &image_compare::Algorithm::MSSIMSimple,
        &gray_image_one,
        &gray_image_two,
    )
    .expect("Images had different dimensions");
    assert!(result.score >= 0.95);
}

#[cfg(any(feature = "png", feature = "ico"))]
#[test]
fn test_encoder() {
    init();

    let rgba_image_one = image::load_from_memory_with_format(
        include_bytes!("files/smpte-rp-219.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_rgba8();

    let gray_image_one = image::load_from_memory_with_format(
        include_bytes!("files/smpte-rp-219-gray.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_luma8();

    for (f, formats) in gstimagers::format::Format::all_encoding_formats() {
        for s in f.iter() {
            let mediatype = s.name();
            let container_format: image::ImageFormat = gstimagers::format::Format::try_from(s)
                .and_then(|v| v.try_into())
                .unwrap();

            for f in formats.iter() {
                let format = f.to_str();
                let mut h = gst_check::Harness::new("imagersenc");
                let spec = format!(
                    "videotestsrc pattern=smpte-rp-219 is-live=1 ! capsfilter caps=video/x-raw,width=160,height=120,format={}",
                    format
                );
                h.add_src_parse(&spec, true);
                h.set_sink_caps(gst::Caps::builder(mediatype).build());
                h.push_from_src().unwrap();

                let new_image = h.pull().unwrap();

                let test = format!("when decoding {} {}", mediatype, format);
                let image_two = image::load_from_memory_with_format(
                    &new_image.into_mapped_buffer_readable().unwrap(),
                    container_format,
                )
                .expect(&test);
                if image_two.color().has_color() {
                    let result = image_compare::rgba_hybrid_compare(
                        &rgba_image_one,
                        &image_two.into_rgba8(),
                    )
                    .expect("Images had different dimensions");
                    assert!(
                        result.score >= 0.95,
                        "Failed validation: {} {} -> {} ",
                        mediatype,
                        format,
                        result.score
                    );
                } else {
                    let result = image_compare::gray_similarity_structure(
                        &image_compare::Algorithm::MSSIMSimple,
                        &gray_image_one,
                        &image_two.into_luma8(),
                    )
                    .expect("Images had different dimensions");
                    assert!(
                        result.score >= 0.95,
                        "Failed validation: {} {} -> {} ",
                        mediatype,
                        format,
                        result.score
                    );
                }
            }
        }
    }
}
