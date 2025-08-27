// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

use std::{fs::File, path::Path};
#[cfg(feature = "v1_28")]
use std::{io::Seek as _, sync::LazyLock};

#[cfg(feature = "v1_28")]
use gst::{ClockTime, ReferenceTimestampMeta};
use gst_pbutils::prelude::*;
use mp4_atom::{Atom, ReadAtom as _, ReadFrom as _};
use tempfile::tempdir;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstmp4::plugin_register_static().unwrap();
    });
}

#[cfg(feature = "v1_28")]
static TAI1958_CAPS: LazyLock<gst::Caps> =
    LazyLock::new(|| gst::Caps::builder("timestamp/x-tai1958").build());

struct Pipeline(gst::Pipeline);
impl std::ops::Deref for Pipeline {
    type Target = gst::Pipeline;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

impl Pipeline {
    fn into_completion(self) {
        self.set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");

        for msg in self.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => (),
            }
        }

        self.set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");
    }
}

struct ExpectedConfiguration {
    is_audio: bool,
    width: u32,
    height: u32,
    has_ctts: bool,
    has_stss: bool,
    has_taic: bool,
    taic_time_uncertainty: u64,
    taic_clock_type: u8,
    num_tai_timestamps: i32,
}

fn test_basic_with(video_enc: &str, audio_enc: &str, cb: impl FnOnce(&Path)) {
    let Ok(pipeline) = gst::parse::launch(&format!(
        "videotestsrc num-buffers=99 ! {video_enc} ! mux. \
         audiotestsrc num-buffers=140 ! {audio_enc} ! mux. \
         isomp4mux name=mux ! filesink name=sink"
    )) else {
        println!("could not build encoding pipeline");
        return;
    };
    run_pipeline(pipeline, cb);
}

fn test_short_basic_with(video_enc: &str, audio_enc: &str, cb: impl FnOnce(&Path)) {
    let Ok(pipeline) = gst::parse::launch(&format!(
        "videotestsrc num-buffers=9 ! {video_enc} ! mux. \
         audiotestsrc num-buffers=13 ! {audio_enc} ! mux. \
         isomp4mux name=mux ! filesink name=sink"
    )) else {
        println!("could not build encoding pipeline");
        return;
    };
    run_pipeline(pipeline, cb);
}

fn run_pipeline(pipeline: gst::Element, cb: impl FnOnce(&Path)) {
    let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());

    let dir = tempfile::TempDir::new().unwrap();
    let mut location = dir.path().to_owned();
    location.push("test.mp4");

    let sink = pipeline.by_name("sink").unwrap();
    sink.set_property("location", location.to_str().expect("Non-UTF8 filename"));
    pipeline.into_completion();

    cb(&location)
}

#[test]
fn test_basic_x264_aac() {
    init();
    test_basic_with("x264enc", "fdkaacenc", |location| {
        let discoverer = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
            .expect("Failed to create discoverer");
        let info = discoverer
            .discover_uri(
                url::Url::from_file_path(location)
                    .expect("Failed to convert filename to URL")
                    .as_str(),
            )
            .expect("Failed to discover MP4 file");

        assert_eq!(info.duration(), Some(gst::ClockTime::from_mseconds(3_300)));

        let audio_streams = info.audio_streams();
        assert_eq!(audio_streams.len(), 1);
        let audio_stream = &audio_streams[0];
        assert_eq!(audio_stream.channels(), 1);
        assert_eq!(audio_stream.sample_rate(), 44_100);
        let caps = audio_stream.caps().unwrap();
        assert!(
            caps.can_intersect(
                &gst::Caps::builder("audio/mpeg")
                    .any_features()
                    .field("mpegversion", 4i32)
                    .build()
            ),
            "Unexpected audio caps {caps:?}"
        );

        let video_streams = info.video_streams();
        assert_eq!(video_streams.len(), 1);
        let video_stream = &video_streams[0];
        assert_eq!(video_stream.width(), 320);
        assert_eq!(video_stream.height(), 240);
        assert_eq!(video_stream.framerate(), gst::Fraction::new(30, 1));
        assert_eq!(video_stream.par(), gst::Fraction::new(1, 1));
        assert!(!video_stream.is_interlaced());
        let caps = video_stream.caps().unwrap();
        assert!(
            caps.can_intersect(&gst::Caps::builder("video/x-h264").any_features().build()),
            "Unexpected video caps {caps:?}"
        );
    })
}

#[test]
fn test_roundtrip_vp9_flac() {
    init();
    test_short_basic_with("vp9enc ! vp9parse", "flacenc ! flacparse", |location| {
        let Ok(pipeline) = gst::parse::launch(
            "filesrc name=src ! qtdemux name=demux \
             demux.audio_0 ! queue ! flacdec ! fakesink \
             demux.video_0 ! queue ! vp9dec ! fakesink",
        ) else {
            panic!("could not build decoding pipeline")
        };
        let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());
        pipeline
            .by_name("src")
            .unwrap()
            .set_property("location", location.display().to_string());
        pipeline.into_completion();
    })
}

#[test]
fn test_roundtrip_av1_aac() {
    init();
    test_short_basic_with("av1enc ! av1parse", "avenc_aac ! aacparse", |location| {
        let Ok(pipeline) = gst::parse::launch(
            "filesrc name=src ! qtdemux name=demux \
             demux.audio_0 ! queue ! avdec_aac ! fakesink \
             demux.video_0 ! queue ! av1dec ! fakesink",
        ) else {
            panic!("could not build decoding pipeline")
        };
        let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());
        pipeline
            .by_name("src")
            .unwrap()
            .set_property("location", location.display().to_string());
        pipeline.into_completion();
    })
}

fn test_uncompressed_with(format: &str, width: u32, height: u32, cb: impl FnOnce(&Path)) {
    let Ok(pipeline) = gst::parse::launch(&format!(
        "videotestsrc num-buffers=34 ! video/x-raw,format={format},width={width},height={height} ! mux. \
         isomp4mux name=mux ! filesink name=sink"
    )) else {
        println!("could not build encoding pipeline");
        return;
    };
    let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());

    let dir = tempdir().unwrap();
    let mut location = dir.path().to_owned();
    location.push("test.mp4");

    let sink = pipeline.by_name("sink").unwrap();
    sink.set_property("location", location.to_str().expect("Non-UTF8 filename"));
    pipeline.into_completion();

    cb(&location)
}

fn test_roundtrip_uncompressed(video_format: &str, width: u32, height: u32) {
    test_uncompressed_with(video_format, width, height, |location| {
        let Ok(pipeline) = gst::parse::launch(
            "filesrc name=src ! qtdemux name=demux \
             demux.video_0 ! queue ! fakesink",
        ) else {
            panic!("could not build decoding pipeline")
        };
        let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());
        pipeline
            .by_name("src")
            .unwrap()
            .set_property("location", location.display().to_string());
        pipeline.into_completion();
    })
}

fn test_encode_uncompressed(video_format: &str, width: u32, height: u32) {
    let filename = format!("{video_format}_{width}x{height}.mp4");
    let temp_dir = tempdir().unwrap();
    let temp_file_path = temp_dir.path().join(filename);
    let location = temp_file_path.as_path();
    let pipeline_text = format!("videotestsrc num-buffers=34 ! video/x-raw,format={video_format},width={width},height={height} ! isomp4mux ! filesink location={location:?}");

    let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
        panic!("could not build encoding pipeline")
    };
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");
    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    test_expected_uncompressed_output(location, width, height);
}

fn test_expected_uncompressed_output(location: &Path, width: u32, height: u32) {
    check_generic_single_trak_file_structure(
        location,
        b"iso4".into(),
        0,
        vec![b"isom".into(), b"mp41".into(), b"mp42".into()],
        ExpectedConfiguration {
            is_audio: false,
            width,
            height,
            has_ctts: false,
            has_stss: false,
            has_taic: false,
            taic_clock_type: 0,       // only if has_taic
            taic_time_uncertainty: 0, // only if has_taic
            num_tai_timestamps: 0,
        },
    );
}

fn check_generic_single_trak_file_structure(
    location: &Path,
    expected_major_brand: mp4_atom::FourCC,
    expected_minor_version: u32,
    expected_compatible_brands: Vec<mp4_atom::FourCC>,
    expected_config: ExpectedConfiguration,
) {
    let mut required_top_level_boxes: Vec<mp4_atom::FourCC> = vec![
        b"ftyp".into(),
        b"free".into(),
        b"mdat".into(),
        b"moov".into(),
    ];

    let mut input = File::open(location).unwrap();
    while let Ok(header) = mp4_atom::Header::read_from(&mut input) {
        assert!(required_top_level_boxes.contains(&header.kind));
        let pos = required_top_level_boxes
            .iter()
            .position(|&fourcc| fourcc == header.kind)
            .unwrap_or_else(|| panic!("expected to find a matching fourcc {:?}", header.kind));
        required_top_level_boxes.remove(pos);
        match header.kind {
            mp4_atom::Ftyp::KIND => {
                let ftyp = mp4_atom::Ftyp::read_atom(&header, &mut input).unwrap();
                check_ftyp_output(
                    expected_major_brand,
                    expected_minor_version,
                    &expected_compatible_brands,
                    ftyp,
                );
            }
            mp4_atom::Moov::KIND => {
                let moov = mp4_atom::Moov::read_atom(&header, &mut input).unwrap();
                assert!(moov.meta.is_none());
                assert!(moov.mvex.is_none());
                assert!(moov.udta.is_none());
                check_mvhd_sanity(&moov.mvhd, &expected_config);
                check_trak_sanity(&moov.trak, &expected_config);
            }
            mp4_atom::Free::KIND => {
                let free = mp4_atom::Free::read_atom(&header, &mut input).unwrap();
                assert_eq!(free.zeroed.size, 0);
            }
            mp4_atom::Mdat::KIND => {
                let mdat = mp4_atom::Mdat::read_atom(&header, &mut input).unwrap();
                assert!(!mdat.data.is_empty());
            }
            _ => {
                panic!("Unexpected top level box: {:?}", header.kind);
            }
        }
    }
    assert!(
        required_top_level_boxes.is_empty(),
        "expected all top level boxes to be found, but these were missed: {required_top_level_boxes:?}"
    );
}

#[test]
fn encode_uncompressed_iyu2() {
    init();
    test_encode_uncompressed("IYU2", 1275, 713);
}

#[test]
fn encode_uncompressed_rgb() {
    init();
    test_encode_uncompressed("RGB", 1275, 713);
}

#[test]
fn encode_uncompressed_rgb_row_align_0() {
    init();
    test_encode_uncompressed("RGB", 1280, 720);
    test_roundtrip_uncompressed("RGB", 1280, 720);
}

#[test]
fn encode_uncompressed_bgr() {
    init();
    test_encode_uncompressed("BGR", 1275, 713);
}

#[test]
fn encode_uncompressed_bgr_row_align_0() {
    init();
    test_encode_uncompressed("BGR", 1280, 720);
    test_roundtrip_uncompressed("BGR", 1280, 720);
}

#[test]
fn encode_uncompressed_nv12() {
    init();
    test_encode_uncompressed("NV12", 1275, 714);
}

#[test]
fn encode_uncompressed_nv21() {
    init();
    test_encode_uncompressed("NV21", 1275, 714);
}

#[test]
fn encode_uncompressed_rgba() {
    init();
    test_encode_uncompressed("RGBA", 1275, 713);
}

#[test]
fn encode_uncompressed_rgba_row_align_0() {
    init();
    test_encode_uncompressed("RGBA", 1280, 720);
    test_roundtrip_uncompressed("RGBA", 1280, 720);
}

#[test]
fn encode_uncompressed_argb() {
    init();
    test_encode_uncompressed("ARGB", 1275, 713);
}

#[test]
fn encode_uncompressed_abgr() {
    init();
    test_encode_uncompressed("ABGR", 1275, 713);
}

#[test]
fn encode_uncompressed_abgr_row_align_0() {
    init();
    test_encode_uncompressed("ABGR", 1280, 720);
    test_roundtrip_uncompressed("ABGR", 1280, 720);
}

#[test]
fn encode_uncompressed_bgra() {
    init();
    test_encode_uncompressed("BGRA", 1275, 713);
}

#[test]
fn encode_uncompressed_rgbx() {
    init();
    test_encode_uncompressed("RGBx", 1275, 713);
}

#[test]
fn encode_uncompressed_bgrx() {
    init();
    test_encode_uncompressed("BGRx", 1275, 713);
}

#[test]
fn encode_uncompressed_y444() {
    init();
    test_encode_uncompressed("Y444", 1275, 713);
}

#[test]
fn encode_uncompressed_i420() {
    init();
    test_encode_uncompressed("I420", 1280, 720);
}

#[test]
fn encode_uncompressed_yv12() {
    init();
    test_encode_uncompressed("YV12", 1280, 720);
}

#[test]
fn encode_uncompressed_yuy2() {
    init();
    test_encode_uncompressed("YUY2", 320, 120);
}

#[test]
fn encode_uncompressed_yvyu() {
    init();
    test_encode_uncompressed("YVYU", 320, 120);
}

#[test]
fn encode_uncompressed_vyuy() {
    init();
    test_encode_uncompressed("VYUY", 320, 120);
}

#[test]
fn encode_uncompressed_uyvy() {
    init();
    test_encode_uncompressed("UYVY", 320, 120);
}

/*
TODO: report YA4p unknown pixel format to GPAC
*/
#[test]
fn encode_uncompressed_ayuv() {
    init();
    test_encode_uncompressed("AYUV", 1275, 713);
}

#[test]
fn encode_uncompressed_y41b() {
    init();
    test_encode_uncompressed("Y41B", 1280, 713);
}

#[test]
fn encode_uncompressed_y42b() {
    init();
    test_encode_uncompressed("Y42B", 1280, 713);
}

#[test]
fn encode_uncompressed_v308() {
    init();
    test_encode_uncompressed("v308", 1275, 713);
}

#[test]
fn encode_uncompressed_gray8() {
    init();
    test_encode_uncompressed("GRAY8", 1275, 713);
}

#[test]
fn encode_uncompressed_gray8_row_align_0() {
    init();
    test_encode_uncompressed("GRAY8", 1280, 720);
    test_roundtrip_uncompressed("GRAY8", 1280, 720);
}

#[test]
fn encode_uncompressed_gray16_be() {
    init();
    test_encode_uncompressed("GRAY16_BE", 1275, 713);
}

#[test]
fn encode_uncompressed_r210() {
    init();
    test_encode_uncompressed("r210", 1275, 713);
}

#[test]
fn encode_uncompressed_nv16() {
    init();
    test_encode_uncompressed("NV16", 1280, 713);
}

#[test]
fn encode_uncompressed_nv61() {
    init();
    test_encode_uncompressed("NV61", 1280, 713);
}

#[test]
fn encode_uncompressed_gbr() {
    init();
    test_encode_uncompressed("GBR", 1275, 713);
}

#[test]
fn encode_uncompressed_rgbp() {
    init();
    test_encode_uncompressed("RGBP", 1275, 713);
}

#[test]
fn encode_uncompressed_bgrp() {
    init();
    test_encode_uncompressed("BGRP", 1275, 713);
}

fn test_encode_uncompressed_image_sequence(video_format: &str, width: u32, height: u32) {
    let filename = format!("{video_format}_{width}x{height}.heifs");
    let temp_dir = tempdir().unwrap();
    let temp_file_path = temp_dir.path().join(filename);
    let location = temp_file_path.as_path();
    let pipeline_text = format!("videotestsrc num-buffers=10 ! video/x-raw,format={video_format},width={width},height={height} ! isomp4mux name=mux ! filesink location={location:?}");

    let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
        panic!("could not build encoding pipeline")
    };
    let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());
    let mux = pipeline.by_name("mux").unwrap();
    let sink_pad = &mux.sink_pads()[0];
    sink_pad.set_property("image-sequence", true);
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");
    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    test_expected_image_sequence_output(location, width, height);
}

fn test_expected_image_sequence_output(location: &Path, width: u32, height: u32) {
    check_generic_single_trak_file_structure(
        location,
        b"msf1".into(),
        0,
        vec![b"iso8".into(), b"msf1".into(), b"unif".into()],
        ExpectedConfiguration {
            is_audio: false,
            width,
            height,
            has_ctts: false,
            has_stss: false,
            has_taic: false,
            taic_time_uncertainty: 0, // only if has_taic
            taic_clock_type: 0,       // only if has_taic
            num_tai_timestamps: 0,
        },
    );
}

#[test]
fn encode_uncompressed_image_sequence_rgb() {
    init();
    test_encode_uncompressed_image_sequence("RGB", 1275, 713);
}

#[test]
fn encode_uncompressed_image_sequence_nv12() {
    init();
    test_encode_uncompressed_image_sequence("NV12", 1275, 714);
}

#[test]
fn test_encode_audio_trak() {
    init();
    let filename = "audio_only.mp4";
    let temp_dir = tempdir().unwrap();
    let temp_file_path = temp_dir.path().join(filename);
    let location = temp_file_path.as_path();
    let pipeline_text = format!("audiotestsrc num-buffers=100 ! audioconvert ! opusenc ! isomp4mux ! filesink location={location:?}");

    let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
        panic!("could not build encoding pipeline")
    };
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");
    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    test_audio_only_output(location);
}

fn test_audio_only_output(location: &Path) {
    check_generic_single_trak_file_structure(
        location,
        b"iso4".into(),
        0,
        vec![b"isom".into(), b"mp41".into(), b"mp42".into()],
        ExpectedConfiguration {
            is_audio: true,
            width: 0,  // ignored for audio
            height: 0, // ignored for audio
            has_ctts: false,
            has_stss: false,
            has_taic: false,
            taic_time_uncertainty: 0, // only if has_taic
            taic_clock_type: 0,       // only if has_taic
            num_tai_timestamps: 0,
        },
    );
}

fn check_ftyp_output(
    expected_major_brand: mp4_atom::FourCC,
    expected_minor_version: u32,
    expected_compatible_brands: &Vec<mp4_atom::FourCC>,
    ftyp: mp4_atom::Ftyp,
) {
    assert_eq!(ftyp.major_brand, expected_major_brand);
    assert_eq!(ftyp.minor_version, expected_minor_version);
    assert_eq!(
        ftyp.compatible_brands.len(),
        expected_compatible_brands.len()
    );
    for fourcc in expected_compatible_brands {
        assert!(ftyp.compatible_brands.contains(fourcc));
    }
}

fn check_mvhd_sanity(mvhd: &mp4_atom::Mvhd, expected_config: &ExpectedConfiguration) {
    assert!(mvhd.creation_time > 0);
    assert!(mvhd.modification_time > 0);
    assert!(mvhd.next_track_id > 1);
    assert!(mvhd.duration > 0);
    if expected_config.is_audio {
        assert!(mvhd.volume.integer() > 0);
    } else {
        assert_eq!(mvhd.volume.integer(), 1);
        assert_eq!(mvhd.volume.decimal(), 0);
    }
    // TODO: assess remaining values for potential invariant
}

fn check_trak_sanity(trak: &[mp4_atom::Trak], expected_config: &ExpectedConfiguration) {
    assert_eq!(trak.len(), 1);
    assert!(trak[0].meta.is_none());
    check_tkhd_sanity(&trak[0].tkhd, expected_config);
    check_edts_sanity(&trak[0].edts);
    check_mdia_sanity(&trak[0].mdia, expected_config);
}

fn check_edts_sanity(maybe_edts: &Option<mp4_atom::Edts>) {
    assert!(maybe_edts.as_ref().is_some_and(|edts| {
        assert!(edts.elst.is_some());
        let elst = edts.elst.as_ref().unwrap();
        assert!(!elst.entries.is_empty());
        true
    }));
}

fn check_tkhd_sanity(tkhd: &mp4_atom::Tkhd, expected_config: &ExpectedConfiguration) {
    assert!(tkhd.creation_time > 0);
    assert!(tkhd.modification_time > 0);
    assert!(tkhd.enabled);
    if expected_config.is_audio {
        assert_eq!(tkhd.width, 0.into());
        assert_eq!(tkhd.height, 0.into());
        assert!(tkhd.volume.integer() > 0);
    } else {
        assert_eq!(tkhd.width.integer(), expected_config.width as u16);
        assert_eq!(tkhd.height.integer(), expected_config.height as u16);
        assert_eq!(tkhd.volume.integer(), 0);
        assert_eq!(tkhd.volume.decimal(), 0);
    }
    // TODO: assess remaining values for potential invariant
}

fn check_mdia_sanity(mdia: &mp4_atom::Mdia, expected_config: &ExpectedConfiguration) {
    check_hdlr_sanity(&mdia.hdlr);
    check_mdhd_sanity(&mdia.mdhd);
    check_minf_sanity(&mdia.minf, expected_config);
}

fn check_hdlr_sanity(hdlr: &mp4_atom::Hdlr) {
    assert!(
        (hdlr.handler == b"soun".into())
            || (hdlr.handler == b"vide".into())
            || (hdlr.handler == b"pict".into())
    );
    assert!(!hdlr.name.is_empty());
}

fn check_mdhd_sanity(mdhd: &mp4_atom::Mdhd) {
    assert!(mdhd.creation_time > 0);
    assert!(mdhd.modification_time > 0);
    assert!(mdhd.timescale > 0);
    assert!(mdhd.language.len() == 3);
}

fn check_minf_sanity(minf: &mp4_atom::Minf, expected_config: &ExpectedConfiguration) {
    check_dinf_sanity(&minf.dinf);
    assert!(
        (minf.smhd.is_some() && (minf.vmhd.is_none())
            || (minf.smhd.is_none() || minf.vmhd.is_some()))
    );
    check_stbl_sanity(&minf.stbl, expected_config);
}

fn check_dinf_sanity(dinf: &mp4_atom::Dinf) {
    let dref = &dinf.dref;
    assert_eq!(dref.urls.len(), 1);
    let url = &dref.urls[0];
    assert!(url.location.is_empty());
}

fn check_stbl_sanity(stbl: &mp4_atom::Stbl, expected_config: &ExpectedConfiguration) {
    assert!(stbl.co64.is_none());
    if expected_config.has_ctts {
        assert!(stbl.ctts.is_some());
        // TODO:
        // check_ctts_sanity(&stbl.ctts);
    } else {
        assert!(stbl.ctts.is_none());
    }
    check_saio_sanity(&stbl.saio, expected_config);
    check_saiz_sanity(&stbl.saiz, expected_config);
    check_stco_sanity(&stbl.stco);
    check_stsc_sanity(&stbl.stsc);
    check_stsd_sanity(&stbl.stsd, expected_config);
    if expected_config.has_stss {
        assert!(stbl.stss.is_some());
        // TODO:
        // check_stss_sanity(&stbl.ctts);
    } else {
        assert!(stbl.stss.is_none());
    }
    check_stsz_sanity(&stbl.stsz);
    check_stts_sanity(&stbl.stts);
    // TODO: check consistency between sample sizes and chunk / sample offsets
}

fn check_saio_sanity(maybe_saio: &Option<mp4_atom::Saio>, expected_config: &ExpectedConfiguration) {
    if expected_config.num_tai_timestamps == 0 {
        assert!(maybe_saio.is_none());
    } else {
        assert!(maybe_saio.is_some());
        let saio = maybe_saio.as_ref().unwrap();
        assert!(saio.aux_info.is_some());
        assert_eq!(
            saio.aux_info.as_ref().unwrap().aux_info_type,
            b"stai".into()
        );
        assert_eq!(saio.aux_info.as_ref().unwrap().aux_info_type_parameter, 0);
        assert_eq!(
            saio.offsets.len(),
            expected_config.num_tai_timestamps as usize
        );
        let mut previous_offset = 0u64;
        for offset in &saio.offsets {
            // We check that the byte offsets are increasing
            // This is different to checking that the timestamps are increasing
            assert!(*offset > previous_offset);
            previous_offset = *offset;
        }
    }
}

fn check_saiz_sanity(maybe_saiz: &Option<mp4_atom::Saiz>, expected_config: &ExpectedConfiguration) {
    if expected_config.num_tai_timestamps == 0 {
        assert!(maybe_saiz.is_none());
    } else {
        assert!(maybe_saiz.is_some());
        let saiz = maybe_saiz.as_ref().unwrap();
        assert!(saiz.aux_info.is_some());
        assert_eq!(
            saiz.aux_info.as_ref().unwrap().aux_info_type,
            b"stai".into()
        );
        assert_eq!(saiz.aux_info.as_ref().unwrap().aux_info_type_parameter, 0);
        assert_eq!(saiz.default_sample_info_size, 9);
        assert_eq!(saiz.sample_count, expected_config.num_tai_timestamps as u32);
    }
}

fn check_stco_sanity(maybe_stco: &Option<mp4_atom::Stco>) {
    assert!(maybe_stco
        .as_ref()
        .is_some_and(|stco| { !stco.entries.is_empty() }));
    // TODO: see if there is anything generic about the stco entries we could check
}

fn check_stsc_sanity(stsc: &mp4_atom::Stsc) {
    assert!(!stsc.entries.is_empty());
    // TODO: see if there is anything generic about the stsc entries we could check
}

fn check_stsd_sanity(stsd: &mp4_atom::Stsd, expected_config: &ExpectedConfiguration) {
    assert_eq!(stsd.codecs.len(), 1);
    let codec = &stsd.codecs[0];
    match codec {
        mp4_atom::Codec::Avc1(avc1) => {
            assert_eq!(avc1.visual.width, expected_config.width as u16);
            assert_eq!(avc1.visual.height, expected_config.height as u16);
            assert_eq!(avc1.visual.depth, 24);
            if expected_config.has_taic {
                assert!(avc1.taic.as_ref().is_some_and(|taic| {
                    assert_eq!(taic.clock_type, expected_config.taic_clock_type.into());
                    assert_eq!(taic.time_uncertainty, expected_config.taic_time_uncertainty);
                    assert_eq!(taic.clock_drift_rate, 2147483647);
                    assert_eq!(taic.clock_resolution, 1000);
                    true
                }));
            } else {
                assert!(avc1.taic.is_none());
            }

            assert!(avc1
                .pasp
                .as_ref()
                .is_some_and(|pasp| { pasp.h_spacing == 1 && pasp.v_spacing == 1 }));
            assert!(avc1.colr.as_ref().is_some_and(|colr| {
                match colr {
                    mp4_atom::Colr::Nclx {
                        colour_primaries,
                        transfer_characteristics,
                        matrix_coefficients: _,
                        full_range_flag,
                    } => {
                        assert_eq!(*colour_primaries, 6);
                        assert_eq!(*transfer_characteristics, 6);
                        assert!(!(*full_range_flag));
                        true
                    }
                    mp4_atom::Colr::Ricc { profile: _ } => {
                        panic!("Incorrect colr type: ricc")
                    }
                    mp4_atom::Colr::Prof { profile: _ } => {
                        panic!("Incorrect colr type: prof")
                    }
                }
            }));
        }
        mp4_atom::Codec::Hev1(_hev1) => {
            // TODO: check HEVC codec (maybe shared?)
        }
        mp4_atom::Codec::Hvc1(_hvc1) => {
            // TODO: check HEVC codec (maybe shared?)
        }
        mp4_atom::Codec::Vp08(_vp08) => {
            // TODO: check VP8 codec
        }
        mp4_atom::Codec::Vp09(_vp09) => {
            // TODO: check VP9 codec
        }
        mp4_atom::Codec::Av01(_av01) => {
            // TODO: check AV1 codec
        }
        mp4_atom::Codec::Mp4a(_mp4a) => {
            // TODO: check MPEG Audio codec
        }
        mp4_atom::Codec::Tx3g(_tx3g) => {
            // TODO: check subtitles / text codec
        }
        mp4_atom::Codec::Opus(_opus) => {
            // TODO: check OPUS codec
        }
        mp4_atom::Codec::Uncv(uncv) => {
            check_uncv_codec_sanity(uncv, expected_config);
        }
        mp4_atom::Codec::Unknown(four_cc) => {
            todo!("Unsupported codec type: {:?}", four_cc);
        }
    }
}

fn check_uncv_codec_sanity(uncv: &mp4_atom::Uncv, expected_config: &ExpectedConfiguration) {
    check_visual_sample_entry_sanity(&uncv.visual, expected_config);
    // See ISO/IEC 23001-17 Table 5 for the profiles
    let valid_v0_profiles: Vec<mp4_atom::FourCC> = vec![
        b"2vuy".into(),
        b"yuv2".into(),
        b"yvyu".into(),
        b"vyuy".into(),
        b"yuv1".into(),
        b"v308".into(),
        b"v408".into(),
        b"y210".into(),
        b"v410".into(),
        b"v210".into(),
        b"rgb3".into(),
        b"i420".into(),
        b"nv12".into(),
        b"nv21".into(),
        b"rgba".into(),
        b"abgr".into(),
        b"yu22".into(),
        b"yv22".into(),
        b"yv20".into(),
        b"\0\0\0\0".into(),
    ];
    let valid_v1_profiles: Vec<mp4_atom::FourCC> =
        vec![b"rgb3".into(), b"rgba".into(), b"abgr".into()];
    let uncc = &uncv.uncc;
    match uncc {
        mp4_atom::UncC::V1 { profile } => {
            assert!(uncv.cmpd.is_none());
            assert!(valid_v1_profiles.contains(profile));
        }
        mp4_atom::UncC::V0 {
            profile,
            components,
            sampling_type,
            interleave_type,
            block_size: _,
            components_little_endian: _,
            block_pad_lsb: _,
            block_little_endian: _,
            block_reversed: _,
            pad_unknown: _,
            pixel_size: _,
            row_align_size: _,
            tile_align_size: _,
            num_tile_cols_minus_one: _,
            num_tile_rows_minus_one: _,
        } => {
            assert!(uncv.cmpd.is_some());
            let cmpd = uncv.cmpd.as_ref().unwrap();
            println!("profile: {profile:?}");
            assert!(valid_v0_profiles.contains(profile));
            assert!(!components.is_empty());
            for component in components {
                // Not clear if component.component_align_size could be tested
                // Not clear if component.component_bit_depth_minus_one coudl be tested
                assert!(component.component_index < (cmpd.components.len() as u16));
                assert!(component.component_format < 4); // 3 for signed int is in Amd 2.
            }
            assert!(*sampling_type < 4);
            assert!(*interleave_type < 6);
            // TODO: there are some special cases we could cross check here.
        }
    }
}

fn check_visual_sample_entry_sanity(
    visual: &mp4_atom::Visual,
    expected_config: &ExpectedConfiguration,
) {
    assert!(visual.depth > 0);
    assert_eq!(visual.height, expected_config.height as u16);
    assert_eq!(visual.width, expected_config.width as u16);
    // TODO: assess remaining values for potential invariant
}

fn check_stsz_sanity(stsz: &mp4_atom::Stsz) {
    let samples = &stsz.samples;
    match samples {
        mp4_atom::StszSamples::Identical { count, size } => {
            assert!(*count > 0);
            assert!(*size > 0);
        }
        mp4_atom::StszSamples::Different { sizes } => {
            assert!(!sizes.is_empty())
        }
    }
}

fn check_stts_sanity(stts: &mp4_atom::Stts) {
    assert!(!stts.entries.is_empty());
    // TODO: see if there is anything generic about the stts entries we could check
}

fn test_taic_encode(video_enc: &str) {
    let filename = format!("taic_{video_enc}.mp4").to_string();
    let temp_dir = tempdir().unwrap();
    let temp_file_path = temp_dir.path().join(filename);
    let location = temp_file_path.as_path();
    let pipeline_text = format!("videotestsrc num-buffers=250 ! {video_enc} ! taginject tags=\"precision-clock-type=can-sync-to-TAI,precision-clock-time-uncertainty-nanoseconds=100000\" scope=stream ! isomp4mux ! filesink location={location:?}");

    let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
        println!("could not build encoding pipeline");
        return;
    };
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");
    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    check_generic_single_trak_file_structure(
        location,
        b"iso4".into(),
        0,
        vec![b"isom".into(), b"mp41".into(), b"mp42".into()],
        ExpectedConfiguration {
            is_audio: false,
            width: 320,
            height: 240,
            has_ctts: true,
            has_stss: true,
            has_taic: true,
            taic_time_uncertainty: 100_000,
            taic_clock_type: 2,
            num_tai_timestamps: 0,
        },
    );
}

#[cfg(feature = "v1_28")]
fn test_taic_stai_encode(video_enc: &str, enabled: bool) {
    let filename = format!("taic_{video_enc}.mp4").to_string();
    let temp_dir = tempdir().unwrap();
    let temp_file_path = temp_dir.path().join(filename);
    let location = temp_file_path.as_path();
    let number_of_frames = 12;
    let pipeline = gst::Pipeline::builder()
        .name(format!("stai-{video_enc}"))
        .build();
    let videotestsrc = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", number_of_frames)
        .property("is-live", true)
        .build()
        .unwrap();
    let Ok(encoder) = gst::ElementFactory::make(video_enc)
        .name("video encoder")
        .property("bframes", 0u32)
        .build()
    else {
        println!("could not build encoding pipeline");
        return;
    };
    let taginject = gst::ElementFactory::make("taginject")
        .property_from_str("tags", "precision-clock-type=can-sync-to-TAI,precision-clock-time-uncertainty-nanoseconds=100000")
        .property_from_str("scope", "stream")
        .build().unwrap();
    let mux = gst::ElementFactory::make("isomp4mux")
        .property("tai-precision-timestamps", enabled)
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", location)
        .build()
        .unwrap();
    pipeline
        .add_many([&videotestsrc, &encoder, &taginject, &mux, &sink])
        .unwrap();

    gst::Element::link_many([&videotestsrc, &encoder, &taginject, &mux, &sink]).unwrap();

    let tai_nanos_initial_offset: u64 = 100_000_000_000;
    let tai_nanos_per_frame_step = 20_000_000; // 20 milliseconds.
    let tai_nanos = std::sync::atomic::AtomicU64::new(tai_nanos_initial_offset);
    videotestsrc.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::BUFFER,
        move |_pad, info| {
            if let Some(buffer) = info.buffer_mut() {
                let timestamp: ClockTime =
                    ClockTime::from_nseconds(tai_nanos.load(std::sync::atomic::Ordering::Acquire));
                let mut meta =
                    ReferenceTimestampMeta::add(buffer.make_mut(), &TAI1958_CAPS, timestamp, None);
                let s = gst::Structure::builder("iso23001-17-timestamp")
                    .field("synchronization-state", true)
                    .field("timestamp-generation-failure", false)
                    .field("timestamp-is-modified", false)
                    .build();
                meta.set_info(s);
                tai_nanos.fetch_add(
                    tai_nanos_per_frame_step,
                    std::sync::atomic::Ordering::AcqRel,
                );
            }
            gst::PadProbeReturn::Ok
        },
    );

    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");
    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    check_generic_single_trak_file_structure(
        location,
        b"iso4".into(),
        0,
        if enabled {
            vec![
                b"isom".into(),
                b"mp41".into(),
                b"mp42".into(),
                b"iso6".into(),
            ]
        } else {
            vec![b"isom".into(), b"mp41".into(), b"mp42".into()]
        },
        ExpectedConfiguration {
            is_audio: false,
            width: 320,
            height: 240,
            has_ctts: false,
            has_stss: true,
            has_taic: true,
            taic_time_uncertainty: 100_000,
            taic_clock_type: 2,
            num_tai_timestamps: if enabled { number_of_frames } else { 0 },
        },
    );
    if enabled {
        let mut input = File::open(location).unwrap();
        let mut mdat_data: Option<Vec<u8>> = None;
        let mut mdat_offset: u64 = 0;
        while let Ok(header) = mp4_atom::Header::read_from(&mut input) {
            match header.kind {
                mp4_atom::Moov::KIND => {
                    let moov = mp4_atom::Moov::read_atom(&header, &mut input).unwrap();
                    let stbl = &moov.trak.first().unwrap().mdia.minf.stbl;
                    let saio = stbl.saio.as_ref().unwrap();
                    let saiz = stbl.saiz.as_ref().unwrap();
                    if let Some(ref mdat_data) = mdat_data {
                        assert_eq!(
                            saio.aux_info.as_ref().unwrap().aux_info_type,
                            b"stai".into()
                        );
                        assert_eq!(saio.aux_info.as_ref().unwrap().aux_info_type_parameter, 0);
                        assert_eq!(
                            saiz.aux_info.as_ref().unwrap().aux_info_type,
                            b"stai".into()
                        );
                        assert_eq!(saiz.aux_info.as_ref().unwrap().aux_info_type_parameter, 0);
                        for i in 0..saio.offsets.len() {
                            let offset = saio.offsets[i];
                            let len = saiz.default_sample_info_size as u64;
                            let offset_into_mdat_start = (offset - mdat_offset) as usize;
                            let offset_into_mdat_end = offset_into_mdat_start + len as usize;
                            let slice = &mdat_data[offset_into_mdat_start..offset_into_mdat_end];
                            assert_eq!(slice.len(), 9);
                            let mut timestamp_bytes: [u8; 8] = [0; 8];
                            timestamp_bytes.copy_from_slice(&slice[0..8]);
                            let timestamp = u64::from_be_bytes(timestamp_bytes);
                            assert_eq!(
                                timestamp,
                                tai_nanos_initial_offset + (i as u64) * tai_nanos_per_frame_step
                            );
                            assert_eq!(slice[8], 0x80);
                        }
                    } else {
                        panic!("mdat should not be none");
                    }
                }
                mp4_atom::Mdat::KIND => {
                    mdat_offset = input.stream_position().unwrap();
                    let mdat = mp4_atom::Mdat::read_atom(&header, &mut input).unwrap();
                    mdat_data = Some(mdat.data);
                }
                _ => {}
            }
        }
    }
}

fn test_taic_encode_cannot_sync(video_enc: &str) {
    let filename = format!("taic_{video_enc}_cannot_sync.mp4").to_string();
    let temp_dir = tempdir().unwrap();
    let temp_file_path = temp_dir.path().join(filename);
    let location = temp_file_path.as_path();
    let pipeline_text = format!("videotestsrc num-buffers=250 ! {video_enc} ! taginject tags=\"precision-clock-type=cannot-sync-to-TAI\" scope=stream ! isomp4mux ! filesink location={location:?}");
    let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
        println!("could not build encoding pipeline");
        return;
    };
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");
    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    check_generic_single_trak_file_structure(
        location,
        b"iso4".into(),
        0,
        vec![b"isom".into(), b"mp41".into(), b"mp42".into()],
        ExpectedConfiguration {
            is_audio: false,
            width: 320,
            height: 240,
            has_ctts: true,
            has_stss: true,
            has_taic: true,
            taic_time_uncertainty: 0xFFFF_FFFF_FFFF_FFFF,
            taic_clock_type: 1,
            num_tai_timestamps: 0,
        },
    );
}

#[test]
fn test_taic_x264() {
    init();
    test_taic_encode("x264enc");
}

#[test]
#[cfg(feature = "v1_28")]
fn test_taic_stai_x264() {
    init();
    test_taic_stai_encode("x264enc", true);
}

#[test]
#[cfg(feature = "v1_28")]
fn test_taic_stai_x264_not_enabled() {
    init();
    test_taic_stai_encode("x264enc", false);
}

#[test]
fn test_taic_x264_no_sync() {
    init();
    test_taic_encode_cannot_sync("x264enc");
}
