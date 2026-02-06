// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! # handdetectiontensordec
//!
//! A GStreamer element that extracts hand detection metadata from hand tensors.
//!
//! This element extracts oriented bounding boxes for detected hands and creates
//! `AnalyticsRelationMeta` with oriented object detection metadata, which can be visualized by
//! `objectdetectionoverlay`.
//!
//! The element supports:
//! - `palm-detection-out`: Post-processed palm detections (`[score, cx, cy, size, kp0_x, kp0_y, kp2_x, kp2_y]`)
//!
//! ## Properties
//! - `confidence-threshold` (f32, 0.0-1.0, default: 0.15): Minimum confidence to consider a hand
//!   (recommended starting value for this model: 0.30)
//! - `max-hands` (u32, 1-8, default: 2): Maximum number of hands to process
//! - `nms-iou-threshold` (f32, 0.0-1.0, default: 0.2): IoU threshold for palm detection NMS
//!   (recommended starting value for this model: 0.08)
//!
//! ## Example Pipelines
//!
//! Single-element detection pipeline with overlay:
//! ```text
//! gst-launch-1.0 \
//!   v4l2src \
//!   ! videoconvert ! videoscale ! video/x-raw,width=224,height=224 \
//!   ! onnxinference model-file=palm_detection_model.onnx \
//!   ! handdetectiontensordec confidence-threshold=0.5 max-hands=2 \
//!   ! objectdetectionoverlay \
//!   ! videoconvert ! autovideosink
//! ```
//!
use byte_slice_cast::AsSliceOf;
use gst::glib;
use gst::prelude::*;
use gst::subclass::ElementMetadata;
use gst::subclass::prelude::*;
use gst_analytics::prelude::*;
use gst_base::subclass::base_transform::BaseTransformImpl;
use gst_video::VideoInfo;
use std::sync::{LazyLock, Mutex};

const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.15;
const DEFAULT_MAX_HANDS: u32 = 2;
const DEFAULT_NMS_IOU_THRESHOLD: f32 = 0.2;
const HAND_CLASS_LABEL: &str = "hand";
const PALM_DETECTION_OUT_ID: &str = "palm-detection-out";
const PALM_MIN_RR_SIZE_NORM: f32 = 0.06;
const PALM_MAX_RR_SIZE_NORM: f32 = 1.40;
const PALM_MIN_VISIBLE_BBOX_RATIO: f32 = 0.5;
const PALM_MIN_KEYPOINT_SPAN_TO_BOX_RATIO: f32 = 0.15;
const PALM_MAX_KEYPOINT_SPAN_TO_BOX_RATIO: f32 = 1.60;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "handdetectiontensordec",
        gst::DebugColorFlags::empty(),
        Some("Hand detection tensor decoder element"),
    )
});

#[derive(Clone)]
struct DetectedHand {
    rotation: f32,
    confidence: f32,
    bbox: (f32, f32, f32, f32),
}

#[derive(Clone)]
struct PalmDetectionCandidate {
    rotation: f32,
    score: f32,
    bbox: (f32, f32, f32, f32),
}

fn extract_f32_tensor(
    buffer: &gst::BufferRef,
    tensor_id: glib::Quark,
) -> Option<(Vec<f32>, Vec<usize>)> {
    for meta in buffer.iter_meta::<gst_analytics::TensorMeta>() {
        let tensor = meta
            .typed_tensor(
                tensor_id,
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[usize::MAX, usize::MAX, usize::MAX, usize::MAX],
            )
            .or_else(|| {
                meta.typed_tensor(
                    tensor_id,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[usize::MAX, usize::MAX, usize::MAX],
                )
            })
            .or_else(|| {
                meta.typed_tensor(
                    tensor_id,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[usize::MAX, usize::MAX],
                )
            })
            .or_else(|| {
                meta.typed_tensor(
                    tensor_id,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[usize::MAX],
                )
            });

        let Some(tensor) = tensor else { continue };

        let map = tensor.data().map_readable().ok()?;
        let data = map.as_slice_of::<f32>().ok()?;
        return Some((data.to_vec(), tensor.dims().to_vec()));
    }

    None
}

fn extract_hands_from_palm_detection(
    buffer: &gst::BufferRef,
    max_hands: usize,
    confidence_threshold: f32,
    nms_iou_threshold: f32,
    video_size: Option<(i32, i32)>,
) -> Result<Vec<DetectedHand>, gst::FlowError> {
    let palm_detection_out_id = glib::Quark::from_str(PALM_DETECTION_OUT_ID);

    let Some((detections_data, detections_dims)) =
        extract_f32_tensor(buffer, palm_detection_out_id)
    else {
        gst::debug!(CAT, "No palm-detection-out tensor found");
        return Ok(Vec::new());
    };

    gst::debug!(
        CAT,
        "Palm detection tensor: dims={:?}, data_len={}, first 8 values={:?}",
        detections_dims,
        detections_data.len(),
        detections_data.get(..8).unwrap_or(&detections_data)
    );

    if detections_dims.len() < 2 || detections_dims[1] != 8 {
        gst::debug!(CAT, "Invalid palm detection tensor dimensions");
        return Ok(Vec::new());
    }

    let num_detections = detections_dims[0];
    let mut palm_candidates = Vec::new();

    gst::debug!(
        CAT,
        "Processing {} detections with confidence threshold {}",
        num_detections,
        confidence_threshold
    );

    for (i, detection) in detections_data
        .chunks_exact(8)
        .take(num_detections)
        .enumerate()
    {
        let score = detection[0];

        gst::debug!(
            CAT,
            "Detection {}: score={}, detection={:?}",
            i,
            score,
            detection
        );

        if score < confidence_threshold {
            continue;
        }

        let box_center_x = detection[1];
        let box_center_y = detection[2];
        let box_size = detection[3];
        let kp0_x = detection[4];
        let kp0_y = detection[5];
        let kp2_x = detection[6];
        let kp2_y = detection[7];

        if box_size <= 0.0 {
            continue;
        }

        let rotation = palm_rotation_from_keypoints((kp0_x, kp0_y), (kp2_x, kp2_y));
        let rr_size = 2.9 * box_size;
        let center_x = box_center_x + 0.5 * box_size * rotation.sin();
        let center_y = box_center_y - 0.5 * box_size * rotation.cos();

        if !is_valid_palm_candidate_normalized(PalmCandidateNormalized {
            center_x,
            center_y,
            rr_size,
            box_size,
            kp0_x,
            kp0_y,
            kp2_x,
            kp2_y,
        }) {
            continue;
        }

        let (center_x, center_y, rr_size) = if let Some((width, height)) = video_size {
            let w = width as f32;
            let h = height as f32;
            (center_x * w, center_y * h, rr_size * w.max(h))
        } else {
            (center_x, center_y, rr_size)
        };

        let half_size = rr_size / 2.0;
        let bbox_min_x = center_x - half_size;
        let bbox_min_y = center_y - half_size;
        let bbox_max_x = center_x + half_size;
        let bbox_max_y = center_y + half_size;

        palm_candidates.push(PalmDetectionCandidate {
            rotation,
            score,
            bbox: (bbox_min_x, bbox_min_y, bbox_max_x, bbox_max_y),
        });
    }

    let selected = apply_nms_to_palm_candidates(palm_candidates, max_hands, nms_iou_threshold);

    Ok(selected
        .into_iter()
        .map(|candidate| DetectedHand {
            rotation: candidate.rotation,
            confidence: candidate.score,
            bbox: candidate.bbox,
        })
        .collect())
}

fn angle_from_vector(dx: f32, dy: f32) -> f32 {
    dy.atan2(dx)
}

fn palm_rotation_from_keypoints(kp0: (f32, f32), kp2: (f32, f32)) -> f32 {
    let dx = kp2.0 - kp0.0;
    let dy = kp2.1 - kp0.1;
    std::f32::consts::FRAC_PI_2 + angle_from_vector(dx, dy)
}

struct PalmCandidateNormalized {
    center_x: f32,
    center_y: f32,
    rr_size: f32,
    box_size: f32,
    kp0_x: f32,
    kp0_y: f32,
    kp2_x: f32,
    kp2_y: f32,
}

fn is_valid_palm_candidate_normalized(candidate: PalmCandidateNormalized) -> bool {
    let PalmCandidateNormalized {
        center_x,
        center_y,
        rr_size,
        box_size,
        kp0_x,
        kp0_y,
        kp2_x,
        kp2_y,
    } = candidate;

    if !center_x.is_finite()
        || !center_y.is_finite()
        || !rr_size.is_finite()
        || !box_size.is_finite()
        || !kp0_x.is_finite()
        || !kp0_y.is_finite()
        || !kp2_x.is_finite()
        || !kp2_y.is_finite()
    {
        return false;
    }

    if !(PALM_MIN_RR_SIZE_NORM..=PALM_MAX_RR_SIZE_NORM).contains(&rr_size) {
        return false;
    }

    if !(0.0..=1.0).contains(&center_x) || !(0.0..=1.0).contains(&center_y) {
        return false;
    }

    if box_size <= 0.0 {
        return false;
    }

    let kp_dx = kp2_x - kp0_x;
    let kp_dy = kp2_y - kp0_y;
    let kp_span = (kp_dx * kp_dx + kp_dy * kp_dy).sqrt();
    let kp_span_ratio = kp_span / box_size;
    if !(PALM_MIN_KEYPOINT_SPAN_TO_BOX_RATIO..=PALM_MAX_KEYPOINT_SPAN_TO_BOX_RATIO)
        .contains(&kp_span_ratio)
    {
        return false;
    }

    let half_size = rr_size * 0.5;
    let x0 = center_x - half_size;
    let y0 = center_y - half_size;
    let x1 = center_x + half_size;
    let y1 = center_y + half_size;

    let bbox_area = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    if bbox_area <= 0.0 {
        return false;
    }

    let inter_x0 = x0.max(0.0);
    let inter_y0 = y0.max(0.0);
    let inter_x1 = x1.min(1.0);
    let inter_y1 = y1.min(1.0);
    let inter_area = (inter_x1 - inter_x0).max(0.0) * (inter_y1 - inter_y0).max(0.0);
    let visible_ratio = inter_area / bbox_area;

    visible_ratio >= PALM_MIN_VISIBLE_BBOX_RATIO
}

fn bbox_iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    gst_analytics::image_util::iou_f32(
        gst_analytics::image_util::Rect::<f32> {
            x: a.0,
            y: a.1,
            w: a.2 - a.0,
            h: a.3 - a.1,
        },
        gst_analytics::image_util::Rect::<f32> {
            x: b.0,
            y: b.1,
            w: b.2 - b.0,
            h: b.3 - b.1,
        },
    )
}

fn hand_bbox_to_oriented_od_params(
    bbox: (f32, f32, f32, f32),
    rotation: f32,
    video_size: Option<(i32, i32)>,
) -> Option<(i32, i32, i32, i32, f32)> {
    let (min_x, min_y, max_x, max_y) = bbox;
    if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return None;
    }

    let (x0, y0, x1, y1) = (min_x.floor(), min_y.floor(), max_x.ceil(), max_y.ceil());

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    if let Some((frame_width, frame_height)) = video_size
        && frame_width > 0
        && frame_height > 0
    {
        let fw = frame_width as f32;
        let fh = frame_height as f32;

        // Keep boxes that are partially outside the frame. Only drop boxes that are
        // completely outside (no overlap with the visible frame area).
        if x1 <= 0.0 || y1 <= 0.0 || x0 >= fw || y0 >= fh {
            return None;
        }
    }

    let x = x0 as i32;
    let y = y0 as i32;
    let width = (x1 - x0) as i32;
    let height = (y1 - y0) as i32;

    if width <= 0 || height <= 0 {
        return None;
    }

    let rotation_for_od = rotation - std::f32::consts::FRAC_PI_2;

    Some((x, y, width, height, rotation_for_od))
}

fn apply_nms_to_palm_candidates(
    mut candidates: Vec<PalmDetectionCandidate>,
    max_hands: usize,
    iou_threshold: f32,
) -> Vec<PalmDetectionCandidate> {
    if candidates.is_empty() || max_hands == 0 {
        return Vec::new();
    }

    let iou_threshold = iou_threshold.clamp(0.0, 1.0);
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));

    let mut selected: Vec<PalmDetectionCandidate> =
        Vec::with_capacity(max_hands.min(candidates.len()));

    'candidate: for candidate in candidates {
        for kept in &selected {
            if bbox_iou(candidate.bbox, kept.bbox) > iou_threshold {
                continue 'candidate;
            }
        }

        selected.push(candidate);
        if selected.len() >= max_hands {
            break;
        }
    }

    selected
}

#[derive(Clone)]
struct Settings {
    confidence_threshold: f32,
    max_hands: u32,
    nms_iou_threshold: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            max_hands: DEFAULT_MAX_HANDS,
            nms_iou_threshold: DEFAULT_NMS_IOU_THRESHOLD,
        }
    }
}

#[derive(Default)]
pub struct HandDetectionTensorDec {
    settings: Mutex<Settings>,
    video_info: Mutex<Option<VideoInfo>>,
}

#[glib::object_subclass]
impl ObjectSubclass for HandDetectionTensorDec {
    const NAME: &'static str = "GstHandDetectionTensorDec";
    type Type = super::HandDetectionTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for HandDetectionTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("confidence-threshold")
                    .nick("Confidence Threshold")
                    .blurb("Confidence threshold for hand detection")
                    .default_value(DEFAULT_CONFIDENCE_THRESHOLD)
                    .minimum(0.0)
                    .maximum(1.0)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("max-hands")
                    .nick("Max Hands")
                    .blurb("Maximum number of hands to track")
                    .default_value(DEFAULT_MAX_HANDS)
                    .minimum(1)
                    .maximum(8)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("nms-iou-threshold")
                    .nick("NMS IoU Threshold")
                    .blurb("IoU threshold for non-maximum suppression on palm detections")
                    .default_value(DEFAULT_NMS_IOU_THRESHOLD)
                    .minimum(0.0)
                    .maximum(1.0)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "confidence-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.confidence_threshold = value.get().expect("type checked upstream");
            }
            "max-hands" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_hands = value.get().expect("type checked upstream");
            }
            "nms-iou-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.nms_iou_threshold = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "confidence-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.confidence_threshold.to_value()
            }
            "max-hands" => {
                let settings = self.settings.lock().unwrap();
                settings.max_hands.to_value()
            }
            "nms-iou-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.nms_iou_threshold.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for HandDetectionTensorDec {}

impl ElementImpl for HandDetectionTensorDec {
    fn metadata() -> Option<&'static ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<ElementMetadata> = LazyLock::new(|| {
            ElementMetadata::new(
                "Hand Detection Tensor Decoder",
                "Tensordecoder/Video",
                "Decodes hand tensors and attaches object detection metadata",
                "Jeremy Whiting<jeremy.whiting@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .field(
                    "tensors",
                    gst::Structure::builder("tensorgroups")
                        .field(
                            PALM_DETECTION_OUT_ID,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", PALM_DETECTION_OUT_ID)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        8i32.to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "row-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst_video::VideoCapsBuilder::new().build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for HandDetectionTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {incaps:?}"))?;
        *self.video_info.lock().unwrap() = Some(info);
        Ok(())
    }

    fn transform_ip(&self, buf: &mut gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (max_hands, confidence_threshold, nms_iou_threshold) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.max_hands as usize,
                settings.confidence_threshold,
                settings.nms_iou_threshold,
            )
        };

        let video_size = self
            .video_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|info| (info.width() as i32, info.height() as i32));

        gst::debug!(
            CAT,
            "Processing buffer with max_hands={}, confidence_threshold={}, nms_iou_threshold={}, video_size={:?}",
            max_hands,
            confidence_threshold,
            nms_iou_threshold,
            video_size
        );

        let hands = extract_hands_from_palm_detection(
            buf,
            max_hands,
            confidence_threshold,
            nms_iou_threshold,
            video_size,
        )?;

        gst::debug!(CAT, "Extracted {} hands", hands.len());

        if hands.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buf);
        let class = glib::Quark::from_str(HAND_CLASS_LABEL);

        for hand in &hands {
            gst::debug!(
                CAT,
                "Hand: bbox=({}, {}, {}, {}), rotation={}, confidence={}",
                hand.bbox.0,
                hand.bbox.1,
                hand.bbox.2,
                hand.bbox.3,
                hand.rotation,
                hand.confidence,
            );

            let Some((x, y, width, height, rotation_for_od)) =
                hand_bbox_to_oriented_od_params(hand.bbox, hand.rotation, video_size)
            else {
                gst::debug!(CAT, "Skipping invalid/out-of-frame hand bbox");
                continue;
            };

            if let Err(err) = rmeta.add_oriented_od_mtd(
                class,
                x,
                y,
                width,
                height,
                rotation_for_od,
                hand.confidence,
            ) {
                gst::warning!(CAT, "Failed to add oriented OD metadata: {}", err);
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    fn ensure_gstreamer_initialized() {
        static GST_INIT: Once = Once::new();
        GST_INIT.call_once(|| {
            gst::init().expect("Failed to initialize GStreamer for tests");
        });
    }

    #[test]
    fn oriented_od_params_keep_negative_coords_for_partial_overlap() {
        let hand = DetectedHand {
            rotation: 0.0,
            confidence: 0.8,
            bbox: (-5.2, 10.1, 20.4, 30.9),
        };

        let params = hand_bbox_to_oriented_od_params(hand.bbox, hand.rotation, Some((100, 100)))
            .expect("partially overlapping box should be accepted");

        assert_eq!(params.0, -6);
        assert_eq!(params.1, 10);
        assert_eq!(params.2, 27);
        assert_eq!(params.3, 21);
    }

    #[test]
    fn oriented_od_params_keep_partial_overlap_on_right_edge() {
        let hand = DetectedHand {
            rotation: 0.0,
            confidence: 0.8,
            bbox: (90.1, 20.2, 105.9, 40.4),
        };

        let params = hand_bbox_to_oriented_od_params(hand.bbox, hand.rotation, Some((100, 100)))
            .expect("partially overlapping box should be accepted");

        assert_eq!(params.0, 90);
        assert_eq!(params.1, 20);
        assert_eq!(params.2, 16);
        assert_eq!(params.3, 21);
    }

    #[test]
    fn oriented_od_params_keep_partial_overlap_on_top_edge() {
        let hand = DetectedHand {
            rotation: 0.0,
            confidence: 0.8,
            bbox: (15.5, -8.6, 35.2, 10.1),
        };

        let params = hand_bbox_to_oriented_od_params(hand.bbox, hand.rotation, Some((100, 100)))
            .expect("partially overlapping box should be accepted");

        assert_eq!(params.0, 15);
        assert_eq!(params.1, -9);
        assert_eq!(params.2, 21);
        assert_eq!(params.3, 20);
    }

    #[test]
    fn oriented_od_params_drop_fully_outside_box() {
        let hand = DetectedHand {
            rotation: 0.0,
            confidence: 0.8,
            bbox: (-30.0, 10.0, -5.0, 40.0),
        };

        let params = hand_bbox_to_oriented_od_params(hand.bbox, hand.rotation, Some((100, 100)));
        assert!(params.is_none());
    }

    #[test]
    fn oriented_od_params_drop_degenerate_box() {
        let hand = DetectedHand {
            rotation: 0.0,
            confidence: 0.8,
            bbox: (10.0, 20.0, 10.0, 30.0),
        };

        let params = hand_bbox_to_oriented_od_params(hand.bbox, hand.rotation, Some((100, 100)));
        assert!(params.is_none());
    }

    #[test]
    fn oriented_od_params_rotation_mapping_preserves_direction() {
        let params =
            hand_bbox_to_oriented_od_params((10.0, 10.0, 30.0, 30.0), 0.0, Some((100, 100)))
                .expect("valid box should map");

        assert!((params.4 + std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }

    #[test]
    fn palm_detection_caps_profile_generates_expected_schema() {
        ensure_gstreamer_initialized();
        let sink_template = <HandDetectionTensorDec as ElementImpl>::pad_templates()
            .iter()
            .find(|template| template.direction() == gst::PadDirection::Sink)
            .expect("sink template must exist");
        let caps = sink_template.caps();

        let structure = caps.structure(0).expect("caps must contain one structure");
        assert_eq!(structure.name(), "video/x-raw");
        assert!(structure.has_field("tensors"));
        assert!(!caps.is_empty());
    }

    #[test]
    fn angle_from_vector_computes_expected_radians() {
        let angle = angle_from_vector(1.0, 0.0);
        assert!((angle - 0.0).abs() < 1e-6);

        let angle = angle_from_vector(0.0, 1.0);
        assert!((angle - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }

    #[test]
    fn palm_rotation_from_keypoints_applies_hand_alignment_offset() {
        let rotation = palm_rotation_from_keypoints((0.0, 0.0), (1.0, 0.0));
        assert!((rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }
}
