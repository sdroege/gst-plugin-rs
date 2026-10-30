// Copyright (C) 2026 Collabora Ltd.
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Ported from the GStreamer C yolotensordecoder check test.

use gst::prelude::*;
use gst_analytics::prelude::*;

const EPS: f32 = 1e-4;

const V8_OUT: &str = "yolo-v8-out";
const V8_OUT_NORMALIZED: &str = "yolo-v8-out-normalized";
const OBB_OUT: &str = "yolo-v8-obb-out";
const OBB_OUT_NORMALIZED: &str = "yolo-v8-obb-out-normalized";
const SEG_DETECTIONS: &str = "yolo-v8-segmentation-out-detections";
const SEG_DETECTIONS_NORMALIZED: &str = "yolo-v8-segmentation-out-detections-normalized";
const SEG_PROTOS: &str = "yolo-v8-segmentation-out-protos";

const DIMS2: &[i32] = &[1, i32::MAX, i32::MAX];
const DIMS3: &[i32] = &[1, i32::MAX, i32::MAX, i32::MAX];

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsanalytics::plugin_register_static().unwrap();
    });
}

fn labels_file() -> std::path::PathBuf {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    let path = std::env::temp_dir().join("gstrs-yolo-test-labels.txt");
    ONCE.call_once(|| {
        std::fs::write(&path, "person\ncar\ndog\ncat\n").unwrap();
    });
    path
}

#[derive(Clone)]
struct Candidate {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    classes: Vec<f32>,
    extras: Vec<f32>,
}

fn candidate(x: f32, y: f32, w: f32, h: f32, classes: &[f32]) -> Candidate {
    candidate_extras(x, y, w, h, classes, &[])
}

fn candidate_extras(x: f32, y: f32, w: f32, h: f32, classes: &[f32], extras: &[f32]) -> Candidate {
    Candidate {
        x,
        y,
        w,
        h,
        classes: classes.to_vec(),
        extras: extras.to_vec(),
    }
}

// Column-major [1, fields, n_candidates] float32 tensor, fields being
// 4 (x, y, w, h) + number of class confidences + number of extras.
fn detections_tensor(id: &str, candidates: &[Candidate]) -> gst_analytics::Tensor {
    let n = candidates.len();
    let num_classes = candidates
        .iter()
        .map(|c| c.classes.len())
        .max()
        .unwrap_or(0);
    let num_extras = candidates.iter().map(|c| c.extras.len()).max().unwrap_or(0);
    let fields = 4 + num_classes + num_extras;

    let mut data = vec![0.0f32; fields * n];
    for (c, cand) in candidates.iter().enumerate() {
        data[c] = cand.x;
        data[n + c] = cand.y;
        data[2 * n + c] = cand.w;
        data[3 * n + c] = cand.h;
        for (i, v) in cand.classes.iter().enumerate() {
            data[(4 + i) * n + c] = *v;
        }
        for (i, v) in cand.extras.iter().enumerate() {
            data[(4 + num_classes + i) * n + c] = *v;
        }
    }

    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_ne_bytes()).collect();
    gst_analytics::Tensor::new_simple(
        glib::Quark::from_str(id),
        gst_analytics::TensorDataType::Float32,
        gst::Buffer::from_slice(bytes),
        gst_analytics::TensorDimOrder::ColMajor,
        &[1, fields, n],
    )
}

// Column-major [1, num_masks, w, h] float32 protos/logits tensor, values in
// row-major (y * w + x) order per mask.
fn logits_tensor(num_masks: usize, w: usize, h: usize, values: &[f32]) -> gst_analytics::Tensor {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
    gst_analytics::Tensor::new_simple(
        glib::Quark::from_str(SEG_PROTOS),
        gst_analytics::TensorDataType::Float32,
        gst::Buffer::from_slice(bytes),
        gst_analytics::TensorDimOrder::ColMajor,
        &[1, num_masks, w, h],
    )
}

fn dets(id: &str, candidates: &[Candidate]) -> Vec<gst_analytics::Tensor> {
    vec![detections_tensor(id, candidates)]
}

fn input_buffer(tensors: Vec<gst_analytics::Tensor>) -> gst::Buffer {
    let mut buffer = gst::Buffer::new();
    {
        let buffer = buffer.get_mut().unwrap();
        gst_analytics::TensorMeta::add(buffer).set(glib::Slice::from(tensors));
    }
    buffer
}

fn strided_tensor(id: &str, dims: &[i32]) -> gst::Caps {
    gst::Caps::builder("tensor/strided")
        .field("tensor-id", id)
        .field(
            "dims",
            gst::Array::from_values(dims.iter().map(|d| d.to_send_value())),
        )
        .field("dims-order", "col-major")
        .field("type", "float32")
        .build()
}

// The tensor group is required in the sink caps, but the tensor in the buffer
// meta is looked up by id, so permissive dims suffice.
fn caps(width: u32, height: u32, group: &str, tensors: &[(&str, &[i32])]) -> gst::Caps {
    init();
    gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgba)
        .width(width as i32)
        .height(height as i32)
        .framerate(gst::Fraction::new(30, 1))
        .field(
            "tensors",
            gst::Structure::builder("tensorgroups")
                .field(
                    group,
                    gst::UniqueList::new(tensors.iter().map(|(id, dims)| strided_tensor(id, dims))),
                )
                .build(),
        )
        .build()
}

fn v8_caps(width: u32, height: u32) -> gst::Caps {
    caps(width, height, V8_OUT, &[(V8_OUT, DIMS2)])
}

fn obb_caps(width: u32, height: u32) -> gst::Caps {
    caps(width, height, OBB_OUT, &[(OBB_OUT, DIMS2)])
}

fn seg_caps(width: u32, height: u32) -> gst::Caps {
    caps(
        width,
        height,
        "yolo-v8-segmentation-out",
        &[(SEG_DETECTIONS, DIMS2), (SEG_PROTOS, DIMS3)],
    )
}

fn decoder(name: &str) -> gst::Element {
    init();
    gst::ElementFactory::make(name).build().unwrap()
}

// Decoder under test: the element is configured before the harness plays it on
// the first push.
struct Dec {
    element: gst::Element,
    caps: gst::Caps,
}

impl Dec {
    fn new(name: &str, caps: &gst::Caps) -> Self {
        Self {
            element: decoder(name),
            caps: caps.clone(),
        }
    }

    fn set<T: ToValue + ?Sized>(self, property: &str, value: &T) -> Self {
        self.element.set_property(property, value.to_value());
        self
    }

    #[track_caller]
    fn push(self, tensors: Vec<gst_analytics::Tensor>) -> gst::Buffer {
        self.run(input_buffer(tensors))
    }

    #[track_caller]
    fn push_raw(self, buffer: gst::Buffer) -> gst::Buffer {
        self.run(buffer)
    }

    #[track_caller]
    fn push_ods(self, tensors: Vec<gst_analytics::Tensor>) -> Vec<Od> {
        let outbuf = self.push(tensors);
        ods(&relation(&outbuf).expect("relation meta"))
    }

    #[track_caller]
    fn run(self, buffer: gst::Buffer) -> gst::Buffer {
        let mut h = gst_check::Harness::with_element(&self.element, Some("sink"), Some("src"));
        h.set_src_caps(self.caps);
        h.play();
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        h.pull().unwrap()
    }
}

fn relation(
    buffer: &gst::Buffer,
) -> Option<gst::MetaRef<'_, gst_analytics::AnalyticsRelationMeta>> {
    buffer.meta::<gst_analytics::AnalyticsRelationMeta>()
}

struct Od {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    conf: f32,
    obj_type: Option<glib::Quark>,
    o: Option<(i32, i32, i32, i32, f32)>,
}

fn ods(rmeta: &gst::MetaRef<'_, gst_analytics::AnalyticsRelationMeta>) -> Vec<Od> {
    rmeta
        .iter::<gst_analytics::AnalyticsODMtd>()
        .map(|od| {
            let loc = od.location().unwrap();
            let o = od
                .oriented_location()
                .ok()
                .map(|o| (o.x, o.y, o.w, o.h, o.r));
            Od {
                x: loc.x,
                y: loc.y,
                w: loc.w,
                h: loc.h,
                conf: loc.loc_conf_lvl,
                obj_type: od.obj_type(),
                o,
            }
        })
        .collect()
}

#[track_caller]
fn assert_od(od: &Od, (x, y, w, h, conf): (i32, i32, i32, i32, f32)) {
    assert_eq!((od.x, od.y, od.w, od.h), (x, y, w, h));
    assert!((od.conf - conf).abs() <= EPS, "{} vs {}", od.conf, conf);
}

struct Mask {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    pixels: Vec<u8>,
}

// The single GRAY8 mask of a decoded buffer. Its rows are padded to the stride
// from the video meta, which is stripped here.
#[track_caller]
fn seg_mask(buffer: &gst::Buffer) -> Mask {
    let rmeta = relation(buffer).expect("relation meta");
    let segs: Vec<_> = rmeta
        .iter::<gst_analytics::AnalyticsSegmentationMtd>()
        .collect();
    assert_eq!(segs.len(), 1);

    let mask = segs[0].mask();
    let vmeta = mask
        .buffer
        .meta::<gst_video::VideoMeta>()
        .expect("video meta on the mask buffer");
    assert_eq!(vmeta.format(), gst_video::VideoFormat::Gray8);
    assert_eq!((vmeta.width(), vmeta.height()), (mask.w, mask.h));

    let stride = vmeta.stride()[0] as usize;
    let data = mask.buffer.map_readable().unwrap().to_vec();
    let pixels = (0..mask.h as usize)
        .flat_map(|row| data[row * stride..row * stride + mask.w as usize].to_vec())
        .collect();

    Mask {
        x: mask.x,
        y: mask.y,
        w: mask.w,
        h: mask.h,
        pixels,
    }
}

fn quark(name: &str) -> glib::Quark {
    glib::Quark::from_str(name)
}

#[track_caller]
fn assert_pad_caps(name: &str, group_caps: &gst::Caps, plain_video_caps: &str) {
    let element = decoder(name);
    let pad = element.static_pad("sink").unwrap();

    let mut template_caps = pad.pad_template().unwrap().caps().to_owned();
    template_caps.fixate();
    assert!(template_caps.is_fixed());
    assert!(pad.query_accept_caps(&template_caps));

    assert!(pad.query_accept_caps(group_caps));
    let plain_video_caps: gst::Caps = plain_video_caps.parse().unwrap();
    assert!(!pad.query_accept_caps(&plain_video_caps));
    let audio_caps: gst::Caps = "audio/x-raw,format=S16LE,rate=48000".parse().unwrap();
    assert!(!pad.query_accept_caps(&audio_caps));
}

// ---------------------------------------------------------------------------
// yolov8tensordec2
// ---------------------------------------------------------------------------

#[test]
fn basic_single_detection() {
    for (id, cand, exp) in [
        (
            V8_OUT,
            candidate(320.0, 240.0, 100.0, 50.0, &[0.9]),
            (270, 215, 100, 50, 0.9),
        ),
        (
            V8_OUT_NORMALIZED,
            candidate(0.5, 0.5, 0.25, 0.25, &[0.9]),
            (240, 180, 160, 120, 0.9),
        ),
    ] {
        let ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480)).push_ods(dets(id, &[cand]));
        assert_eq!(ods.len(), 1);
        assert_od(&ods[0], exp);
        assert_eq!(ods[0].obj_type, Some(quark("CLASS-0")));
    }
}

#[test]
fn below_confidence_threshold_dropped() {
    let outbuf = Dec::new("yolov8tensordec2", &v8_caps(640, 480))
        .set("class-confidence-threshold", &0.8f32)
        .push(dets(V8_OUT, &[candidate(200.0, 100.0, 40.0, 20.0, &[0.6])]));

    let rmeta = relation(&outbuf).expect("relation meta");
    assert_eq!(rmeta.len(), 0);
    assert_eq!(ods(&rmeta).len(), 0);
}

#[test]
fn multiple_classes_pick_max() {
    let ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480))
        .set("label-file", labels_file().to_str().unwrap())
        .push_ods(dets(
            V8_OUT,
            &[candidate(120.0, 80.0, 40.0, 20.0, &[0.1, 0.8, 0.2, 0.3])],
        ));

    assert_eq!(ods.len(), 1);
    assert_od(&ods[0], (100, 70, 40, 20, 0.8));
    assert_eq!(ods[0].obj_type, Some(quark("car")));
}

#[test]
fn nms_drops_overlapping_lower_confidence() {
    let ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480))
        .set("iou-threshold", &0.3f32)
        .push_ods(dets(
            V8_OUT,
            &[
                candidate(100.0, 100.0, 40.0, 40.0, &[0.95]),
                candidate(105.0, 105.0, 40.0, 40.0, &[0.8]),
            ],
        ));

    assert_eq!(ods.len(), 1);
    assert_od(&ods[0], (80, 80, 40, 40, 0.95));
}

#[test]
fn nms_keeps_distant_boxes() {
    let ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480))
        .set("iou-threshold", &0.3f32)
        .push_ods(dets(
            V8_OUT,
            &[
                candidate(100.0, 100.0, 30.0, 30.0, &[0.9]),
                candidate(240.0, 240.0, 30.0, 30.0, &[0.88]),
            ],
        ));

    assert_eq!(ods.len(), 2);
    assert_od(&ods[0], (85, 85, 30, 30, 0.9));
    assert_od(&ods[1], (225, 225, 30, 30, 0.88));
}

#[test]
fn max_detections_limit() {
    let ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480))
        .set("max-detections", &2u32)
        .push_ods(dets(
            V8_OUT,
            &[
                candidate(100.0, 100.0, 30.0, 30.0, &[0.95]),
                candidate(200.0, 100.0, 30.0, 30.0, &[0.85]),
                candidate(300.0, 100.0, 30.0, 30.0, &[0.75]),
            ],
        ));

    assert_eq!(ods.len(), 2);
    assert_od(&ods[0], (85, 85, 30, 30, 0.95));
    assert_od(&ods[1], (185, 85, 30, 30, 0.85));
}

#[test]
fn label_file_applied() {
    let mut ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480))
        .set("label-file", labels_file().to_str().unwrap())
        .push_ods(dets(
            V8_OUT,
            &[
                candidate(80.0, 80.0, 20.0, 20.0, &[0.95, 0.1, 0.05, 0.01]),
                candidate(160.0, 160.0, 20.0, 20.0, &[0.1, 0.2, 0.91, 0.3]),
            ],
        ));

    assert_eq!(ods.len(), 2);
    ods.sort_by_key(|od| od.x);
    assert_od(&ods[0], (70, 70, 20, 20, 0.95));
    assert_eq!(ods[0].obj_type, Some(quark("person")));
    assert_od(&ods[1], (150, 150, 20, 20, 0.91));
    assert_eq!(ods[1].obj_type, Some(quark("dog")));
}

#[test]
fn invalid_bbox_rejected() {
    let ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480)).push_ods(dets(
        V8_OUT,
        &[
            candidate(120.0, 120.0, 0.0, 40.0, &[0.95]),
            candidate(1200.0, 120.0, 30.0, 30.0, &[0.9]),
            candidate(320.0, 200.0, 50.0, 60.0, &[0.92]),
        ],
    ));

    assert_eq!(ods.len(), 1);
    assert_od(&ods[0], (295, 170, 50, 60, 0.92));
}

#[test]
fn missing_tensor_meta() {
    let outbuf = Dec::new("yolov8tensordec2", &v8_caps(640, 480)).push_raw(gst::Buffer::new());
    assert!(relation(&outbuf).is_none());
}

#[test]
fn tensor_dims_too_small() {
    let outbuf = Dec::new("yolov8tensordec2", &v8_caps(640, 480))
        .push(dets(V8_OUT, &[candidate(100.0, 100.0, 40.0, 20.0, &[])]));
    assert!(relation(&outbuf).is_none());
}

#[test]
fn properties_roundtrip() {
    let element = decoder("yolov8tensordec2");
    element.set_property("class-confidence-threshold", 0.62f32);
    element.set_property("iou-threshold", 0.33f32);
    element.set_property("max-detections", 17u32);
    element.set_property("label-file", labels_file().to_str().unwrap());

    let get = |p: &str| element.property::<f32>(p);
    assert!((get("class-confidence-threshold") - 0.62).abs() < 1e-6);
    assert!((get("iou-threshold") - 0.33).abs() < 1e-6);
    assert_eq!(element.property::<u32>("max-detections"), 17);
    assert_eq!(
        element
            .property::<Option<glib::GString>>("label-file")
            .unwrap()
            .as_str(),
        labels_file().to_str().unwrap()
    );
}

#[test]
fn accept_caps() {
    assert_pad_caps(
        "yolov8tensordec2",
        &v8_caps(640, 480),
        "video/x-raw,format=RGBA,width=640,height=480,framerate=30/1",
    );
}

#[test]
fn normalized_invalid_bbox_rejected() {
    let mut ods = Dec::new("yolov8tensordec2", &v8_caps(640, 480)).push_ods(dets(
        V8_OUT_NORMALIZED,
        &[
            candidate(0.5, 0.5, 0.0, 0.2, &[0.95]),
            candidate(1.2, 0.5, 0.1, 0.1, &[0.9]),
            // Wider than the frame but overlapping it, so kept unclamped.
            candidate(0.5, 0.5, 1.5, 0.1, &[0.92]),
            candidate(0.5, 0.5, 0.2, 0.2, &[0.93]),
        ],
    ));

    assert_eq!(ods.len(), 2);
    ods.sort_by_key(|od| od.x);
    assert_od(&ods[0], (-160, 216, 960, 48, 0.92));
    assert_od(&ods[1], (256, 192, 128, 96, 0.93));
}

#[test]
fn normalized_nms() {
    let ods = Dec::new("yolov8tensordec2", &v8_caps(400, 400))
        .set("iou-threshold", &0.3f32)
        .push_ods(dets(
            V8_OUT_NORMALIZED,
            &[
                candidate(0.25, 0.25, 0.1, 0.1, &[0.95]),
                candidate(0.2625, 0.2625, 0.1, 0.1, &[0.8]),
            ],
        ));

    assert_eq!(ods.len(), 1);
    assert_od(&ods[0], (80, 80, 40, 40, 0.95));
}

// ---------------------------------------------------------------------------
// yolov8obbtensordec
// ---------------------------------------------------------------------------

#[test]
fn obb_basic_decode() {
    for (id, cand, exp) in [
        (
            OBB_OUT,
            candidate_extras(150.0, 120.0, 40.0, 20.0, &[0.9], &[0.5]),
            (130, 110, 40, 20),
        ),
        (
            OBB_OUT_NORMALIZED,
            candidate_extras(0.5, 0.4, 0.2, 0.1, &[0.9], &[0.5]),
            (120, 105, 60, 30),
        ),
    ] {
        let ods = Dec::new("yolov8obbtensordec", &obb_caps(300, 300)).push_ods(dets(id, &[cand]));
        assert_eq!(ods.len(), 1);

        let (x, y, w, h, rot) = ods[0].o.expect("oriented location");
        assert_eq!((x, y, w, h), exp);
        assert!((rot - 0.5).abs() <= EPS);
        assert!((ods[0].conf - 0.9).abs() <= EPS);
    }
}

#[test]
fn obb_dims_too_small() {
    let outbuf = Dec::new("yolov8obbtensordec", &obb_caps(300, 300)).push(dets(
        OBB_OUT,
        &[candidate(100.0, 100.0, 40.0, 20.0, &[0.9])],
    ));
    assert!(relation(&outbuf).is_none());
}

#[test]
fn obb_polygon_iou_nms() {
    let ods = Dec::new("yolov8obbtensordec", &obb_caps(300, 300))
        .set("iou-threshold", &0.3f32)
        .push_ods(dets(
            OBB_OUT,
            &[
                candidate_extras(100.0, 100.0, 60.0, 10.0, &[0.9], &[0.0]),
                candidate_extras(
                    100.0,
                    100.0,
                    60.0,
                    10.0,
                    &[0.8],
                    &[std::f32::consts::FRAC_PI_2],
                ),
            ],
        ));

    assert_eq!(ods.len(), 2);
    let mut seen_r0 = 0;
    let mut seen_r90 = 0;
    for od in &ods {
        let (x, y, w, h, rot) = od.o.expect("oriented location");
        assert_eq!((x, y, w, h), (70, 95, 60, 10));
        if rot.abs() <= 1e-3 {
            seen_r0 += 1;
        } else if (rot - std::f32::consts::FRAC_PI_2).abs() <= 1e-3 {
            seen_r90 += 1;
        }
    }
    assert_eq!(seen_r0, 1);
    assert_eq!(seen_r90, 1);
}

#[test]
fn obb_accept_caps() {
    assert_pad_caps(
        "yolov8obbtensordec",
        &obb_caps(300, 300),
        "video/x-raw,format=RGBA,width=300,height=300,framerate=30/1",
    );
}

// ---------------------------------------------------------------------------
// yolov8segtensordec
// ---------------------------------------------------------------------------

#[test]
fn seg_basic_decode() {
    for (id, cand) in [
        (
            SEG_DETECTIONS,
            candidate_extras(2.0, 2.0, 2.0, 2.0, &[0.95], &[2.0]),
        ),
        (
            SEG_DETECTIONS_NORMALIZED,
            candidate_extras(0.5, 0.5, 0.5, 0.5, &[0.95], &[2.0]),
        ),
    ] {
        let outbuf = Dec::new("yolov8segtensordec", &seg_caps(4, 4)).push(vec![
            detections_tensor(id, &[cand]),
            logits_tensor(1, 4, 4, &[1.0; 16]),
        ]);

        let rmeta = relation(&outbuf).expect("relation meta");
        let od_ids: Vec<u32> = rmeta
            .iter::<gst_analytics::AnalyticsODMtd>()
            .map(|od| od.id())
            .collect();
        let seg_ids: Vec<u32> = rmeta
            .iter::<gst_analytics::AnalyticsSegmentationMtd>()
            .map(|seg| seg.id())
            .collect();
        assert_eq!(od_ids.len(), 1);
        assert_eq!(seg_ids.len(), 1);
        assert!(rmeta.exist(od_ids[0], seg_ids[0], 1, gst_analytics::RelTypes::RELATE_TO));
        assert!(rmeta.exist(seg_ids[0], od_ids[0], 1, gst_analytics::RelTypes::RELATE_TO));

        let mask = seg_mask(&outbuf);
        assert_eq!((mask.x, mask.y, mask.w, mask.h), (1, 1, 2, 2));
    }
}

#[test]
fn seg_mask_pixel_values() {
    let mut logits = vec![-1.0f32; 16];
    // Only positions (0,0) and (1,1) in the 4x4 logits grid are positive.
    logits[0] = 1.0;
    logits[5] = 1.0;

    let outbuf = Dec::new("yolov8segtensordec", &seg_caps(4, 4)).push(vec![
        detections_tensor(
            SEG_DETECTIONS,
            &[candidate_extras(1.0, 1.0, 2.0, 2.0, &[0.99], &[2.0])],
        ),
        logits_tensor(1, 4, 4, &logits),
    ]);

    let mask = seg_mask(&outbuf);
    assert_eq!((mask.x, mask.y, mask.w, mask.h), (0, 0, 2, 2));
    assert_eq!(mask.pixels, vec![1, 0, 0, 1]);
}

#[test]
fn seg_mask_roi() {
    // The protos are sampled at bbox-relative coordinates.
    type Case = (u32, u32, Candidate, fn(usize, usize) -> bool, Vec<u8>);
    let cases: [Case; 2] = [
        (
            8,
            4,
            candidate_extras(3.0, 2.0, 2.0, 2.0, &[0.99], &[5.0]),
            |_x, y| y >= 3,
            vec![0, 0, 1, 1],
        ),
        (
            4,
            8,
            candidate_extras(2.0, 3.0, 2.0, 2.0, &[0.99], &[5.0]),
            |x, _y| x >= 3,
            vec![0, 1, 0, 1],
        ),
    ];

    for (width, height, cand, positive, expected) in cases {
        let logits: Vec<f32> = (0..8)
            .flat_map(|y| (0..8).map(move |x| if positive(x, y) { 1.0 } else { -1.0 }))
            .collect();

        let outbuf = Dec::new("yolov8segtensordec", &seg_caps(width, height)).push(vec![
            detections_tensor(SEG_DETECTIONS, &[cand]),
            logits_tensor(1, 8, 8, &logits),
        ]);

        let mask = seg_mask(&outbuf);
        assert_eq!(mask.pixels, expected);
    }
}

#[test]
fn seg_missing_logits_tensor() {
    let outbuf = Dec::new("yolov8segtensordec", &seg_caps(4, 4)).push(dets(
        SEG_DETECTIONS,
        &[candidate_extras(2.0, 2.0, 2.0, 2.0, &[0.95], &[2.0])],
    ));
    assert!(relation(&outbuf).is_none());
}

#[test]
fn seg_dims_too_small() {
    let outbuf = Dec::new("yolov8segtensordec", &seg_caps(4, 4)).push(vec![
        detections_tensor(
            SEG_DETECTIONS,
            &[candidate_extras(2.0, 2.0, 2.0, 2.0, &[0.95], &[2.0])],
        ),
        logits_tensor(2, 2, 2, &[1.0; 8]),
    ]);
    assert!(relation(&outbuf).is_none());
}
