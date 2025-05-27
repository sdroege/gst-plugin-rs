// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

use std::{fs::File, path::Path};

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
    let pipeline_text = format!("videotestsrc num-buffers=34 ! video/x-raw,format={video_format},width={width},height={height} ! isomp4mux ! filesink location={:?}", location);

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

    test_expected_uncompressed_output(location);
}

fn test_expected_uncompressed_output(location: &Path) {
    check_generic_single_trak_file_structure(
        location,
        b"iso4".into(),
        0,
        vec![b"isom".into(), b"mp41".into(), b"mp42".into()],
    );
}

fn check_generic_single_trak_file_structure(
    location: &Path,
    expected_major_brand: mp4_atom::FourCC,
    expected_minor_version: u32,
    expected_compatible_brands: Vec<mp4_atom::FourCC>,
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
                check_mvhd_sanity(&moov.mvhd);
                check_trak_sanity(&moov.trak);
            }
            _ => {
                let _ = mp4_atom::Any::read_atom(&header, &mut input).unwrap();
            }
        }
    }
    assert!(
        required_top_level_boxes.is_empty(),
        "expected all top level boxes to be found, but these were missed: {:?}",
        required_top_level_boxes
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
    let pipeline_text = format!("videotestsrc num-buffers=10 ! video/x-raw,format={video_format},width={width},height={height} ! isomp4mux name=mux ! filesink location={:?}", location);

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

    test_expected_image_sequence_output(location);
}

fn test_expected_image_sequence_output(location: &Path) {
    check_generic_single_trak_file_structure(
        location,
        b"msf1".into(),
        0,
        vec![b"iso8".into(), b"msf1".into(), b"unif".into()],
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
    let pipeline_text = format!("audiotestsrc num-buffers=100 ! audioconvert ! opusenc ! isomp4mux ! filesink location={:?}", location);

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

fn check_mvhd_sanity(mvhd: &mp4_atom::Mvhd) {
    assert!(mvhd.creation_time > 0);
    assert!(mvhd.modification_time > 0);
    assert!(mvhd.next_track_id > 1);
    assert!(mvhd.duration > 0);
    // tkhd.volume might or might not be valid depending on track type
    // TODO: assess remaining values for potential invariant
}

fn check_trak_sanity(trak: &[mp4_atom::Trak]) {
    assert_eq!(trak.len(), 1);
    assert!(trak[0].meta.is_none());
    check_tkhd_sanity(&trak[0].tkhd);
    check_edts_sanity(&trak[0].edts);
    check_mdia_sanity(&trak[0].mdia);
}

fn check_edts_sanity(maybe_edts: &Option<mp4_atom::Edts>) {
    assert!(maybe_edts.is_some());
    let edts = maybe_edts.as_ref().unwrap();
    assert!(edts.elst.is_some());
    let elst = edts.elst.as_ref().unwrap();
    assert!(!elst.entries.is_empty());
}

fn check_tkhd_sanity(tkhd: &mp4_atom::Tkhd) {
    assert!(tkhd.creation_time > 0);
    assert!(tkhd.modification_time > 0);
    assert!(tkhd.enabled);
    // tkhd.height might or might not be valid depending on track type
    // tkhd.width might or might not be valid depending on track type
    // tkhd.volume might or might not be valid depending on track type
    // TODO: assess remaining values for potential invariant
}

fn check_mdia_sanity(mdia: &mp4_atom::Mdia) {
    check_hdlr_sanity(&mdia.hdlr);
    check_mdhd_sanity(&mdia.mdhd);
    check_minf_sanity(&mdia.minf);
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

fn check_minf_sanity(minf: &mp4_atom::Minf) {
    check_dinf_sanity(&minf.dinf);
    assert!(
        (minf.smhd.is_some() && (minf.vmhd.is_none())
            || (minf.smhd.is_none() || minf.vmhd.is_some()))
    );
    check_stbl_sanity(&minf.stbl);
}

fn check_dinf_sanity(dinf: &mp4_atom::Dinf) {
    let dref = &dinf.dref;
    assert_eq!(dref.urls.len(), 1);
    let url = &dref.urls[0];
    assert!(url.location.is_empty());
}

fn check_stbl_sanity(stbl: &mp4_atom::Stbl) {
    assert!(stbl.co64.is_none());
    assert!(stbl.ctts.is_none());
    assert!(stbl.saio.is_none());
    assert!(stbl.saiz.is_none());
    check_stco_sanity(&stbl.stco);
    check_stsc_sanity(&stbl.stsc);
    check_stsd_sanity(&stbl.stsd);
    assert!(stbl.stss.is_none());
    check_stsz_sanity(&stbl.stsz);
    check_stts_sanity(&stbl.stts);
    // TODO: check consistency between sample sizes and chunk / sample offsets
}

fn check_stco_sanity(maybe_stco: &Option<mp4_atom::Stco>) {
    assert!(maybe_stco.is_some());
    let stco = maybe_stco.as_ref().unwrap();
    assert!(!stco.entries.is_empty());
    // TODO: see if there is anything generic about the stco entries we could check
}

fn check_stsc_sanity(stsc: &mp4_atom::Stsc) {
    assert!(!stsc.entries.is_empty());
    // TODO: see if there is anything generic about the stsc entries we could check
}

fn check_stsd_sanity(stsd: &mp4_atom::Stsd) {
    assert_eq!(stsd.codecs.len(), 1);
    let codec = &stsd.codecs[0];
    match codec {
        mp4_atom::Codec::Avc1(_avc1) => {
            // TODO: check H.264 codec
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
            check_uncv_codec_sanity(uncv);
        }
        mp4_atom::Codec::Unknown(four_cc) => {
            todo!("Unsupported codec type: {:?}", four_cc);
        }
    }
}

fn check_uncv_codec_sanity(uncv: &mp4_atom::Uncv) {
    check_visual_sample_entry_sanity(&uncv.visual);
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
            println!("profile: {:?}", profile);
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

fn check_visual_sample_entry_sanity(visual: &mp4_atom::Visual) {
    assert!(visual.depth > 0);
    assert!(visual.height > 0);
    assert!(visual.width > 0);
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
