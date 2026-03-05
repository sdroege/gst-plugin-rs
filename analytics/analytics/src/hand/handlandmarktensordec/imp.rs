// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! # handlandmarktensordec
//!
//! A GStreamer element that attaches hand keypoint metadata (semantic-tag=hand-21-kp) to video buffers.
//!
//! This element extracts hand landmark keypoints from hand landmark tensors (produced by an
//! inference element) and attaches them as tensor metadata to the buffer. This allows
//! downstream elements to perform gesture recognition, hand pose analysis, or other ML tasks that
//! require access to the raw keypoint coordinates.
//!
//! The element is designed to work with hand landmark models that output tensors with:
//! - `hand_landmarks`: Keypoints for each hand (commonly 21 points with x/y/(optional z)); this decoder currently uses x/y and ignores z/depth.
//! - `hand_score`: Confidence score per hand. When this tensor is present it is used; otherwise the decoder falls back to 1.0 confidence.
//!
//! ## Properties
//! - `confidence-threshold` (f32, 0.0-1.0, default: 0.5): Minimum confidence to consider a hand
//! - `nms-iou-threshold` (f32, 0.0-1.0, default: 0.2): IoU threshold for non-maximum suppression on hand detections
//! - `max-hands` (u32, 1-10, default: 2): Maximum number of hands to process
//! - `attach-bounding-box` (bool, default: true): Whether to attach oriented bounding box metadata for each hand
//!
//! ## Example Pipelines
//!
//! Basic hand landmark decoding pipeline:
//! ```text
//! gst-launch-1.0 \
//!   v4l2src \
//!   ! videoconvertscale add-borders=true \
//!   ! onnxinference model-file=hand_landmark_model.onnx \
//!   ! handlandmarktensordec confidence-threshold=0.5 max-hands=2 \
//!   ! fakesink
//! ```
//!
//! Combined detection and landmark analysis:
//! ```text
//! gst-launch-1.0 \
//!   v4l2src \
//!   ! videoconvertscale add-borders=true \
//!   ! onnxinference model-file=palm_detection_full_inf_post_192x192.onnx \
//!   ! handdetectiontensordec confidence-threshold=0.7 \
//!   ! onnxinference model-file=hand_landmark_model.onnx \
//!   ! handlandmarktensordec confidence-threshold=0.5 \
//!   ! objectdetectionoverlay \
//!   ! videoconvert ! autovideosink
//! ```

use gst::glib;
use gst::prelude::*;
use gst::subclass::ElementMetadata;
use gst::subclass::prelude::*;
use gst_analytics::prelude::*;
use gst_base::subclass::base_transform::BaseTransformImpl;
use gst_video::VideoInfo;
use std::sync::{LazyLock, Mutex};

use super::super::helper::{
    bbox_iou, extract_f32_tensor, oriented_od_params_from_bbox_and_rotation,
};

const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.5;
const DEFAULT_MAX_HANDS: u32 = 2;
const DEFAULT_NMS_IOU_THRESHOLD: f32 = 0.2;
const DEFAULT_ATTACH_BOUNDING_BOX: bool = true;
const HAND_CLASS_LABEL: &str = "hand";
const HAND_KEYPOINT_GROUP_SEMANTIC_TAG: &str = "hand-21-kp";
const HAND_LANDMARKS_TENSOR_ID: &str = "hand_landmarks";
const HAND_SCORE_TENSOR_ID: &str = "hand_score";
const HAND_KEYPOINT_COUNT: usize = 21;
const HAND_BBOX_PADDING: f32 = 0.15;
/// Offset applied to the hand-axis rotation to reach the oriented-OD +X baseline.
/// This landmark model puts 0 rad on the +Y axis, so shift by -PI/2.
const HAND_AXIS_ROTATION_OFFSET: f32 = -std::f32::consts::FRAC_PI_2;
const HAND_KEYPOINT_SKELETON_PAIRS: [i32; 42] = [
    0, 1, 1, 2, 2, 3, 3, 4, 0, 5, 5, 6, 6, 7, 7, 8, 5, 9, 9, 10, 10, 11, 11, 12, 9, 13, 13, 14, 14,
    15, 15, 16, 13, 17, 17, 18, 18, 19, 19, 20, 0, 17,
];

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "handlandmarktensordec",
        gst::DebugColorFlags::empty(),
        Some("Hand landmark tensor decoder element"),
    )
});

#[derive(Clone, Debug)]
struct HandData {
    confidence: f32,
    rotation: f32,
    bbox: (f32, f32, f32, f32),
    landmarks: Vec<f32>,
    kps_dim: usize,
}

fn decode_landmark_hands(data: &[f32], dims: &[usize]) -> Option<(Vec<Vec<f32>>, usize)> {
    match dims {
        // The models we target (e.g. hand_landmark_sparse_Nx3x224x224.onnx) use a dense
        // output head, so the landmarks tensor is [N, 63]: N hands, each a flat vector of
        // 21 keypoints x 3 values. Other layouts aren't produced by any model we use yet.
        [hands_count, flattened] if *flattened % HAND_KEYPOINT_COUNT == 0 => {
            let kps_dim = *flattened / HAND_KEYPOINT_COUNT;
            if kps_dim < 2 {
                gst::debug!(
                    CAT,
                    "Hand-landmark tensor has too few values per keypoint: dims {:?} give {}, need >= 2",
                    dims,
                    kps_dim
                );
                return None;
            }
            if data.len() < *hands_count * *flattened {
                gst::debug!(
                    CAT,
                    "Hand-landmark tensor too short: dims {:?} need {} values, got {}",
                    dims,
                    *hands_count * *flattened,
                    data.len()
                );
                return None;
            }

            Some((
                data.chunks_exact(*flattened)
                    .take(*hands_count)
                    .map(|chunk| chunk.to_vec())
                    .collect(),
                kps_dim,
            ))
        }
        _ => {
            gst::debug!(
                CAT,
                "Unrecognized hand-landmark tensor shape {:?}; expected flattened [N, {}*D] with D >= 2",
                dims,
                HAND_KEYPOINT_COUNT
            );
            None
        }
    }
}

fn compute_rotation_from_landmarks(landmarks: &[f32], kps_dim: usize) -> Option<f32> {
    if kps_dim < 2 || landmarks.len() < HAND_KEYPOINT_COUNT * kps_dim {
        gst::debug!(
            CAT,
            "Unexpected landmark data for rotation: kps_dim {}, got {} values, need >= {}",
            kps_dim,
            landmarks.len(),
            HAND_KEYPOINT_COUNT * kps_dim
        );
        return None;
    }

    // Hand orientation is the wrist -> middle-finger MCP (base) axis. Both are rigid palm
    // points, so this vector stays in the plane of the hand regardless of finger curl
    // (unlike wrist -> middle tip). Matches how handdetectiontensordec derives rotation.
    let wrist_x = landmarks[0];
    let wrist_y = landmarks[1];
    let middle_base_x = landmarks[9 * kps_dim];
    let middle_base_y = landmarks[9 * kps_dim + 1];

    Some(std::f32::consts::FRAC_PI_2 + (middle_base_y - wrist_y).atan2(middle_base_x - wrist_x))
}

fn compute_bbox_from_landmarks(landmarks: &[f32], kps_dim: usize) -> Option<(f32, f32, f32, f32)> {
    if kps_dim < 2 || landmarks.len() < HAND_KEYPOINT_COUNT * kps_dim {
        gst::debug!(
            CAT,
            "Unexpected landmark data for bbox: kps_dim {}, got {} values, need >= {}",
            kps_dim,
            landmarks.len(),
            HAND_KEYPOINT_COUNT * kps_dim
        );
        return None;
    }

    let mut xs = Vec::with_capacity(HAND_KEYPOINT_COUNT);
    let mut ys = Vec::with_capacity(HAND_KEYPOINT_COUNT);

    // Landmark coordinates are already absolute input-frame pixels (see decode_landmark_hands),
    // so they are used as-is.
    for point in landmarks.chunks_exact(kps_dim).take(HAND_KEYPOINT_COUNT) {
        let x = point[0];
        let y = point[1];
        if !x.is_finite() || !y.is_finite() {
            gst::debug!(
                CAT,
                "Skipping non-finite landmark coordinate ({}, {})",
                x,
                y
            );
            continue;
        }

        xs.push(x);
        ys.push(y);
    }

    if xs.is_empty() || ys.is_empty() {
        gst::debug!(
            CAT,
            "No finite landmark coordinates; cannot compute hand bbox"
        );
        return None;
    }

    let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
    let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
    let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    let width = max_x - min_x;
    let height = max_y - min_y;

    if width <= 0.0 || height <= 0.0 {
        gst::debug!(
            CAT,
            "Degenerate hand bbox (width {}, height {}); skipping",
            width,
            height
        );
        return None;
    }

    Some((
        min_x - width * HAND_BBOX_PADDING,
        min_y - height * HAND_BBOX_PADDING,
        max_x + width * HAND_BBOX_PADDING,
        max_y + height * HAND_BBOX_PADDING,
    ))
}

fn flatten_optional_hand_values(
    buffer: &gst::BufferRef,
    tensor_id: &'static str,
) -> Option<Vec<f32>> {
    let tensor_id = glib::Quark::from_str(tensor_id);
    extract_f32_tensor(buffer, tensor_id).map(|(data, _dims)| data)
}

fn extract_hands(
    buffer: &gst::BufferRef,
    max_hands: usize,
    confidence_threshold: f32,
    nms_iou_threshold: f32,
) -> Result<Vec<HandData>, gst::FlowError> {
    let landmarks_id = glib::Quark::from_str(HAND_LANDMARKS_TENSOR_ID);
    let Some((landmark_data, landmark_dims)) = extract_f32_tensor(buffer, landmarks_id) else {
        gst::debug!(CAT, "No hand landmarks tensor found");
        return Ok(Vec::new());
    };

    let Some((hands_landmarks, kps_dim)) = decode_landmark_hands(&landmark_data, &landmark_dims)
    else {
        gst::warning!(
            CAT,
            "Unsupported landmarks tensor dims: {:?}",
            landmark_dims
        );
        return Ok(Vec::new());
    };

    let hand_scores = flatten_optional_hand_values(buffer, HAND_SCORE_TENSOR_ID);

    let mut hands = Vec::new();

    for (index, landmarks) in hands_landmarks.into_iter().enumerate() {
        let confidence = hand_scores
            .as_ref()
            .and_then(|scores| scores.get(index).copied())
            .unwrap_or(1.0);
        if confidence < confidence_threshold {
            continue;
        }

        let Some(bbox) = compute_bbox_from_landmarks(&landmarks, kps_dim) else {
            continue;
        };

        let rotation = compute_rotation_from_landmarks(&landmarks, kps_dim).unwrap_or(0.0);

        hands.push(HandData {
            confidence,
            rotation,
            bbox,
            landmarks: landmarks.clone(),
            kps_dim,
        });
    }

    hands.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));

    let mut selected: Vec<HandData> = Vec::with_capacity(max_hands.min(hands.len()));

    'candidate: for hand in hands {
        for kept in &selected {
            if bbox_iou(hand.bbox, kept.bbox) > nms_iou_threshold {
                continue 'candidate;
            }
        }

        selected.push(hand);
        if selected.len() >= max_hands {
            break;
        }
    }

    Ok(selected)
}

fn extract_keypoint_confidence(hand: &HandData, keypoint_idx: usize) -> Option<f32> {
    if hand.kps_dim < 3 || keypoint_idx >= HAND_KEYPOINT_COUNT {
        return None;
    }
    let confidence_idx = keypoint_idx * hand.kps_dim + 2;
    hand.landmarks.get(confidence_idx).copied()
}

fn attach_keypoint_metadata(
    rmeta: &mut gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
    hand: &HandData,
) -> Result<Option<u32>, String> {
    if hand.kps_dim < 2 || hand.landmarks.len() < HAND_KEYPOINT_COUNT * hand.kps_dim {
        return Err("Invalid landmarks data".to_string());
    }

    let mut positions = Vec::with_capacity(HAND_KEYPOINT_COUNT * 2);
    let mut confidences = Vec::with_capacity(HAND_KEYPOINT_COUNT);
    let mut visibilities = Vec::with_capacity(HAND_KEYPOINT_COUNT);

    for (keypoint_idx, point) in hand
        .landmarks
        .chunks_exact(hand.kps_dim)
        .take(HAND_KEYPOINT_COUNT)
        .enumerate()
    {
        let x = point[0];
        let y = point[1];

        if !x.is_finite() || !y.is_finite() {
            continue;
        }

        // Coordinates are already absolute input-frame pixels; no normalization needed.
        let px = x as i32;
        let py = y as i32;
        let keypoint_confidence = extract_keypoint_confidence(hand, keypoint_idx);

        // Determine visibility based on per-keypoint confidence if available
        let visibility = if let Some(kp_conf) = keypoint_confidence {
            if kp_conf > 0.5 {
                gst_analytics::AnalyticsKeypointVisibility::VISIBLE
            } else {
                gst_analytics::AnalyticsKeypointVisibility::OCCLUDED
            }
        } else {
            gst_analytics::AnalyticsKeypointVisibility::UNKNOWN
        };

        // Keep keypoint confidence when available; otherwise fall back to hand-level confidence.
        let confidence = keypoint_confidence.unwrap_or(hand.confidence);

        positions.push(px);
        positions.push(py);
        confidences.push(confidence);
        visibilities.push(visibility.bits() as u8);
    }

    if positions.is_empty() {
        return Ok(None);
    }

    let keypoint_group = rmeta
        .add_keypoints_group(
            HAND_KEYPOINT_GROUP_SEMANTIC_TAG,
            gst_analytics::AnalyticsKeypointDimensions::_2d,
            &positions,
            Some(&confidences),
            Some(&visibilities),
            &HAND_KEYPOINT_SKELETON_PAIRS,
        )
        .map_err(|e| format!("Failed to add keypoint group metadata: {}", e))?;

    Ok(Some(keypoint_group.id()))
}

fn hand_bbox_to_oriented_od_params(
    hand: &HandData,
    video_size: Option<(i32, i32)>,
) -> Option<(i32, i32, i32, i32, f32)> {
    oriented_od_params_from_bbox_and_rotation(
        hand.bbox,
        hand.rotation,
        HAND_AXIS_ROTATION_OFFSET,
        video_size,
    )
}

#[derive(Clone)]
struct Settings {
    confidence_threshold: f32,
    max_hands: u32,
    nms_iou_threshold: f32,
    attach_bounding_box: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            max_hands: DEFAULT_MAX_HANDS,
            nms_iou_threshold: DEFAULT_NMS_IOU_THRESHOLD,
            attach_bounding_box: DEFAULT_ATTACH_BOUNDING_BOX,
        }
    }
}

#[derive(Default)]
pub struct HandLandmarkTensorDec {
    settings: Mutex<Settings>,
    video_info: Mutex<Option<VideoInfo>>,
}

#[glib::object_subclass]
impl ObjectSubclass for HandLandmarkTensorDec {
    const NAME: &'static str = "GstHandLandmarkTensorDec";
    type Type = super::HandLandmarkTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for HandLandmarkTensorDec {
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
                    .maximum(10)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("nms-iou-threshold")
                    .nick("NMS IoU Threshold")
                    .blurb("IoU threshold for non-maximum suppression on hand detections")
                    .default_value(DEFAULT_NMS_IOU_THRESHOLD)
                    .minimum(0.0)
                    .maximum(1.0)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("attach-bounding-box")
                    .nick("Attach Bounding Box")
                    .blurb("Whether to attach oriented bounding box metadata for each hand")
                    .default_value(DEFAULT_ATTACH_BOUNDING_BOX)
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
            "attach-bounding-box" => {
                let mut settings = self.settings.lock().unwrap();
                settings.attach_bounding_box = value.get().expect("type checked upstream");
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
            "attach-bounding-box" => {
                let settings = self.settings.lock().unwrap();
                settings.attach_bounding_box.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for HandLandmarkTensorDec {}

impl ElementImpl for HandLandmarkTensorDec {
    fn metadata() -> Option<&'static ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<ElementMetadata> = LazyLock::new(|| {
            ElementMetadata::new(
                "Hand Landmark Tensor Decoder",
                "Tensordecoder/Video",
                "Decodes hand landmark tensors and attaches keypoint metadata",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
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
                            HAND_LANDMARKS_TENSOR_ID,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", HAND_LANDMARKS_TENSOR_ID)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        (HAND_KEYPOINT_COUNT as i32 * 3).to_send_value(),
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

impl BaseTransformImpl for HandLandmarkTensorDec {
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
        let (max_hands, confidence_threshold, nms_iou_threshold, attach_bounding_box) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.max_hands as usize,
                settings.confidence_threshold,
                settings.nms_iou_threshold,
                settings.attach_bounding_box,
            )
        };

        let video_size = self
            .video_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|info| (info.width() as i32, info.height() as i32));

        let Some(video_size) = video_size else {
            gst::warning!(
                CAT,
                "Missing frame resolution in caps; cannot decode hand landmarks"
            );
            return Err(gst::FlowError::Error);
        };

        let hands = extract_hands(buf, max_hands, confidence_threshold, nms_iou_threshold)?;

        gst::debug!(CAT, "Extracted {} hands", hands.len());

        if hands.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buf);
        let class = glib::Quark::from_str(HAND_CLASS_LABEL);

        for hand in &hands {
            let mut hand_od_id = None;

            // Attach bounding box as oriented object detection metadata if enabled
            if attach_bounding_box {
                let Some((x, y, width, height, rotation_for_od)) =
                    hand_bbox_to_oriented_od_params(hand, Some(video_size))
                else {
                    gst::debug!(CAT, "Skipping invalid/out-of-frame hand bbox");
                    continue;
                };

                match rmeta.add_oriented_od_mtd(
                    class,
                    x,
                    y,
                    width,
                    height,
                    rotation_for_od,
                    hand.confidence,
                ) {
                    Ok(od) => {
                        hand_od_id = Some(od.id());
                    }
                    Err(err) => {
                        gst::warning!(CAT, "Failed to add oriented OD metadata: {}", err);
                    }
                }
            }

            // Attach individual keypoint metadata with visibility flags
            let keypoint_group_id = match attach_keypoint_metadata(&mut rmeta, hand) {
                Ok(id) => id,
                Err(err) => {
                    gst::debug!(CAT, "Failed to attach keypoint metadata: {}", err);
                    None
                }
            };

            if let (Some(od_id), Some(kp_group_id)) = (hand_od_id, keypoint_group_id)
                && let Err(err) =
                    rmeta.set_relation(gst_analytics::RelTypes::RELATE_TO, od_id, kp_group_id)
            {
                gst::debug!(
                    CAT,
                    "Failed to set relation between hand OD and keypoint group: {}",
                    err
                );
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landmark_rotation_aligns_with_hand_axis() {
        let mut landmarks = vec![0.0f32; HAND_KEYPOINT_COUNT * 2];

        // Wrist at origin (all-zero init), middle-finger base one unit along +X:
        // the hand axis points along +X.
        landmarks[9 * 2] = 1.0;
        landmarks[9 * 2 + 1] = 0.0;

        let rotation = compute_rotation_from_landmarks(&landmarks, 2).unwrap();
        let rotation_for_od = rotation - std::f32::consts::FRAC_PI_2;

        assert!(rotation_for_od.abs() < 1e-6);
    }
}
