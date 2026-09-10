// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, subclass::prelude::*};
use gst_analytics::prelude::*;
use gst_video::{prelude::*, subclass::prelude::*};

use byte_slice_cast::*;

use itertools::izip;
use std::sync::{LazyLock, Mutex};

use super::util::{BoundingBox, iou, iou_oriented};

const YOLOV8_OUT: &glib::GStr = glib::gstr!("yolo-v8-out");
const YOLOV8OBB_OUT: &glib::GStr = glib::gstr!("yolo-v8-obb-out");
const YOLOX_OUT: &glib::GStr = glib::gstr!("yolox-out");
const YOLO26_OUT: &glib::GStr = glib::gstr!("yolo-26-end2end-out");
const YOLO26OBB_OUT: &glib::GStr = glib::gstr!("yolo-26-obb-end2end-out");

#[derive(Clone, Copy, Debug)]
enum YoloTensorFormat {
    V8,   // Col-major, [1, 4+C, N]
    V8O,  // Col-major, [1, 4+C, N] Yolo V8 OBB one-to-many
    X,    // Row-major, [1, N, 5+C]
    Y26,  // Row-major, [1, N, 6], NMS done by the model
    Y26O, // Row-major, [1, N, 7], Yolo-OBB NMS done by the model
}

fn tensor_format_from_type(type_: glib::Type) -> YoloTensorFormat {
    if type_ == super::YoloV8TensorDec::static_type() {
        YoloTensorFormat::V8
    } else if type_ == super::YoloV8ObbTensorDec::static_type() {
        YoloTensorFormat::V8O
    } else if type_ == super::YoloXTensorDec::static_type() {
        YoloTensorFormat::X
    } else if type_ == super::Yolo26TensorDec::static_type() {
        YoloTensorFormat::Y26
    } else if type_ == super::Yolo26ObbTensorDec::static_type() {
        YoloTensorFormat::Y26O
    } else {
        unreachable!()
    }
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "yolotensordec",
        gst::DebugColorFlags::empty(),
        Some("Yolo tensor decoder element"),
    )
});

struct Settings {
    label_file: Option<String>,
    // Only YoloX
    box_confidence_threshold: f32,
    // YoloX and YoloV8
    class_confidence_threshold: f32,
    // YoloX and YoloV8
    iou_threshold: f32,
    // YoloX and YoloV8
    max_detections: u32,
    // Yolo26 and Yolo26Obb
    score_threshold: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            label_file: None,
            box_confidence_threshold: 0.4,
            class_confidence_threshold: 0.4,
            iou_threshold: 0.7,
            max_detections: 100,
            score_threshold: 0.3,
        }
    }
}

// Shared properties between YoloX, YoloV8 and YoloV8Obb
impl Settings {
    fn nms_properties() -> Vec<glib::ParamSpec> {
        vec![
        glib::ParamSpecFloat::builder("class-confidence-threshold")
            .nick("Class Confidence Threshold")
            .blurb("Boxes with a confidence level inferior to this threshold will be excluded")
            .minimum(0.0)
            .maximum(1.0)
            .default_value(Settings::default().class_confidence_threshold)
            .mutable_playing()
            .build(),
        glib::ParamSpecFloat::builder("iou-threshold")
            .nick("IOU Threshold")
            .blurb(
                "Maximum intersection-over-union between bounding boxes to consider them distinct",
            )
            .minimum(0.0)
            .maximum(1.0)
            .default_value(Settings::default().iou_threshold)
            .mutable_playing()
            .build(),
        glib::ParamSpecUInt::builder("max-detections")
            .nick("Maximum Detections")
            .blurb("Maximum number of detections")
            .default_value(Settings::default().max_detections)
            .mutable_playing()
            .build(),
    ]
    }

    fn nms_set_property(&mut self, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "class-confidence-threshold" => {
                self.class_confidence_threshold = value.get().unwrap();
            }
            "iou-threshold" => {
                self.iou_threshold = value.get().unwrap();
            }
            "max-detections" => {
                self.max_detections = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn nms_get_property(&self, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "class-confidence-threshold" => self.class_confidence_threshold.to_value(),
            "iou-threshold" => self.iou_threshold.to_value(),
            "max-detections" => self.max_detections.to_value(),
            _ => unimplemented!(),
        }
    }
}

struct State {
    labels: Vec<glib::Quark>,
}

#[derive(Default)]
pub struct YoloTensorDec {
    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for YoloTensorDec {
    const ABSTRACT: bool = true;
    const NAME: &'static str = "GstYoloTensorDec";
    type Type = super::YoloTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for YoloTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("label-file")
                    .nick("Label File")
                    .blurb("Label file with one label per line")
                    .mutable_ready()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "label-file" => {
                let mut settings = self.settings.lock().unwrap();
                settings.label_file = value.get().unwrap();
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "label-file" => {
                let settings = self.settings.lock().unwrap();
                settings.label_file.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        self.obj()
            .sink_pad()
            .unset_pad_flags(gst::PadFlags::ACCEPT_INTERSECT);
    }
}

impl GstObjectImpl for YoloTensorDec {}

impl ElementImpl for YoloTensorDec {}

impl BaseTransformImpl for YoloTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        use std::fs;

        let mut state = self.state.lock().unwrap();

        let settings = self.settings.lock().unwrap();
        let labels = if let Some(ref label_file) = settings.label_file {
            let label_file = match fs::read_to_string(label_file) {
                Ok(s) => s,
                Err(err) => {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed to open labels file '{label_file}': {err}"
                    );
                    return Err(gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to open labels file '{label_file}': {err}"]
                    ));
                }
            };
            label_file
                .lines()
                .map(glib::Quark::from_str)
                .collect::<Vec<_>>()
        } else {
            gst::debug!(CAT, imp = self, "No labels file provided");
            Vec::new()
        };

        *state = Some(State { labels });

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;
        gst::info!(CAT, imp = self, "Stopped");
        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state_guard = self.state.lock().unwrap();
        let Some(state) = &*state_guard else {
            gst::debug!(CAT, imp = self, "Wrong state");
            return Err(gst::FlowError::Flushing);
        };

        let Some(meta) = find_yolo_tensor_meta(buffer, self.obj().type_()) else {
            gst::trace!(CAT, imp = self, "No Yolo tensor meta found");
            return Ok(gst::FlowSuccess::Ok);
        };

        let tensor = &meta.as_slice()[0];
        let Ok(map) = tensor.data().map_readable() else {
            return Err(gst::FlowError::Error);
        };

        let Ok(data) = map.as_slice_of::<f32>() else {
            return Err(gst::FlowError::Error);
        };

        if tensor.dims()[0] != 1 {
            gst::error!(
                CAT,
                imp = self,
                "Invalid number of batches {}",
                tensor.dims()[0]
            );
            return Err(gst::FlowError::Error);
        }

        let tensor_format = tensor_format_from_type(self.obj().type_());

        // YoloV8, YoloX, Yolo26, Yolo26Obb tensors all use different memory layouts.
        let (num_candidates, num_fields) = match tensor_format {
            YoloTensorFormat::V8O | YoloTensorFormat::V8 => {
                // YoloV8: dims[1] = num_fields, dims[2] = num_candidates
                // Planar / column-major: field f of candidate c is at data[c + f * stride]
                (tensor.dims()[2], tensor.dims()[1])
            }
            YoloTensorFormat::X => {
                // YoloX: dims[1] = num_candidates, dims[2] = num_fields
                // Interleaved / row-major: each chunk of num_fields is one candidate
                (tensor.dims()[1], tensor.dims()[2])
            }
            YoloTensorFormat::Y26 => {
                // Yolo26: dims[1] = num_candidates, dims[2] = 6 (fixed)
                (tensor.dims()[1], 6)
            }
            YoloTensorFormat::Y26O => {
                // Yolo26: dims[1] = num_candidates, dims[2] = 7 (fixed)
                (tensor.dims()[1], 7)
            }
        };
        // YoloV8 has no box confidence field, fields 4.. are class scores.
        // YoloX has a box confidence field at index 4, fields 5.. are class scores.
        // Yolo26 has a single score and class index, no per-class scores.
        // YoloV8Obb has no box confidence field, but has rotation, fields 5.. are class scores.
        // Yolo26Obb has a single score on an oriented box and class index, no per-class scores.
        let num_classes = match tensor_format {
            YoloTensorFormat::V8 => num_fields - 4,
            YoloTensorFormat::X => num_fields - 5,
            YoloTensorFormat::Y26 => 0,
            YoloTensorFormat::V8O => num_fields - 5,
            YoloTensorFormat::Y26O => 0,
        };
        match tensor_format {
            YoloTensorFormat::Y26 | YoloTensorFormat::Y26O => {
                gst::log!(CAT, imp = self, "Received {num_candidates} boxes",);
            }
            _ => {
                gst::log!(
                    CAT,
                    imp = self,
                    "Received {num_candidates} boxes with {num_classes} classes",
                );
            }
        }

        let settings = self.settings.lock().unwrap();

        // Non-maximum suppression (NMS) filter conceptually based on the one in
        // https://github.com/tracel-ai/models/blob/main/yolox-burn/src/model/boxes.rs

        let mut candidate_boxes = vec![];
        match tensor_format {
            YoloTensorFormat::V8 => {
                // YoloV8 planar layout: field f of candidate c is at data[c + f * stride]
                // stride = dims[2] = num_candidates
                let stride = num_candidates;
                let (boxes, classes) = data.split_at(4 * stride);
                let (xs, rest) = boxes.split_at(stride);
                let (ys, rest) = rest.split_at(stride);
                let (widths, heights) = rest.split_at(stride);

                for (c, (&x, &y, &width, &height)) in izip!(xs, ys, widths, heights).enumerate() {
                    // Find max confidence across all class confidence slices
                    let (class, max_confidence) = classes
                        .chunks_exact(stride)
                        .enumerate()
                        .map(|(i, chunk)| (i as u32, chunk[c]))
                        .max_by(|(_, a), (_, b)| a.total_cmp(b))
                        .unwrap();
                    if max_confidence < settings.class_confidence_threshold {
                        continue;
                    }

                    candidate_boxes.push(BoundingBox::from_center_extents(
                        x,
                        y,
                        width,
                        height,
                        None,
                        class,
                        max_confidence,
                    ));
                }
            }
            YoloTensorFormat::X => {
                // YoloX interleaved layout: contiguous chunk per candidate
                for b in data.chunks_exact(num_fields) {
                    // Skip boxes that have a too low confidence
                    if b[4] < settings.box_confidence_threshold {
                        continue;
                    }

                    // For each box, search for the class with maximum confidence
                    let (class, confidence) = b[5..]
                        .iter()
                        .enumerate()
                        .max_by(|(_, a), (_, b)| a.total_cmp(b))
                        .unwrap();
                    if *confidence < settings.class_confidence_threshold {
                        continue;
                    }

                    let combined_confidence = b[4] * confidence;
                    candidate_boxes.push(BoundingBox::from_center_extents(
                        b[0],
                        b[1],
                        b[2],
                        b[3],
                        None,
                        class as u32,
                        combined_confidence,
                    ));
                }
            }
            YoloTensorFormat::Y26 => {
                // Yolo26 (end2end) interleaved layout: each row is a finalized detection
                for det in data.chunks_exact(num_fields) {
                    let score = det[4];
                    if score < settings.score_threshold {
                        continue;
                    }

                    let width = det[2] - det[0];
                    let height = det[3] - det[1];
                    if width <= 0. || height <= 0. {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Skipping box with negative dimensions: \
                             ({}, {}, {}, {})",
                            det[0],
                            det[1],
                            det[2],
                            det[3],
                        );
                        continue;
                    }

                    // Unlike other Yolo variants the box coordinates are the four corners already.
                    candidate_boxes.push(BoundingBox::from_corners(
                        det[0],
                        det[1],
                        det[2],
                        det[3],
                        None,
                        det[5] as u32,
                        score,
                    ));
                }
            }
            YoloTensorFormat::V8O => {
                // YoloV8 Obb planar layout: field f of candidate c is at data[c + f * stride]
                // stride = dims[2] = num_candidates
                // Boxes are x,y,w,h,class0...n,r
                let stride = num_candidates;
                let (boxes, extras) = data.split_at(4 * stride);
                let (xs, rest) = boxes.split_at(stride);
                let (ys, rest) = rest.split_at(stride);
                let (widths, heights) = rest.split_at(stride);
                let (classes, rotations) = extras.split_at(num_classes * stride);

                for (c, (&x, &y, &width, &height, &rotation)) in
                    izip!(xs, ys, widths, heights, rotations).enumerate()
                {
                    // Find max confidence across all class confidence slices
                    let (class, max_confidence) = classes
                        .chunks_exact(stride)
                        .enumerate()
                        .map(|(i, chunk)| (i as u32, chunk[c]))
                        .max_by(|(_, a), (_, b)| a.total_cmp(b))
                        .unwrap();
                    if max_confidence < settings.class_confidence_threshold {
                        continue;
                    }

                    gst::trace!(
                        CAT,
                        imp = self,
                        "candidate x {x}, y {y}, w {width}, h {height}, rotation {rotation} class {class} confidence {max_confidence}"
                    );

                    candidate_boxes.push(BoundingBox::from_center_extents(
                        x,
                        y,
                        width,
                        height,
                        Some(rotation),
                        class,
                        max_confidence,
                    ));
                }
            }
            YoloTensorFormat::Y26O => {
                // Yolo26Obb (end2end) interleaved layout: each row is a finalized detection
                for det in data.chunks_exact(num_fields) {
                    let score = det[4];
                    if score < settings.score_threshold {
                        continue;
                    }

                    // The output box coordinates for the OBB model differ from the detect end2end model
                    // in that they are xywh instead of xyxy corners
                    let x = det[0];
                    let y = det[1];
                    let width = det[2];
                    let height = det[3];
                    let class = det[5] as u32;
                    let rot = det[6];

                    gst::trace!(
                        CAT,
                        imp = self,
                        "candidate x {x}, y {y}, w {width}, h {height}, rotation {rot} class {class} confidence {score}"
                    );

                    candidate_boxes.push(BoundingBox::from_center_extents(
                        x,
                        y,
                        width,
                        height,
                        Some(rot),
                        class,
                        score,
                    ));
                }
            }
        }

        // Sort boxes by decreasing confidence
        candidate_boxes.sort_unstable_by(|a, b| b.confidence.total_cmp(&a.confidence));

        drop(map);
        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);

        if matches!(
            tensor_format,
            YoloTensorFormat::Y26 | YoloTensorFormat::Y26O
        ) {
            // Yolo26 and Yolo26Obb: NMS is already done by the model, so emit all detections
            // that passed the score threshold.
            for b in &candidate_boxes {
                self.add_detection(&mut rmeta, &state.labels, b);
            }
        } else if matches!(tensor_format, YoloTensorFormat::V8O) {
            // YoloV8Obb: perform non-maximum suppression per class,
            // processing the boxes in globally decreasing confidence order, so
            // that the max-detections limit keeps the highest confidence
            // detections, using the full oriented/rotated iou
            let mut kept: Vec<Vec<BoundingBox>> = vec![Vec::new(); num_classes];
            let mut num_detections = 0;
            for b in &candidate_boxes {
                if kept[b.class as usize]
                    .iter()
                    .any(|k| iou_oriented(b, k) > settings.iou_threshold)
                {
                    continue;
                }
                kept[b.class as usize].push(*b);

                self.add_detection(&mut rmeta, &state.labels, b);

                num_detections += 1;
                if num_detections >= settings.max_detections {
                    break;
                }
            }
        } else {
            // YoloV8 and YoloX: perform non-maximum suppression per class,
            // processing the boxes in globally decreasing confidence order, so
            // that the max-detections limit keeps the highest confidence
            // detections.
            let mut kept: Vec<Vec<BoundingBox>> = vec![Vec::new(); num_classes];
            let mut num_detections = 0;
            for b in &candidate_boxes {
                if kept[b.class as usize]
                    .iter()
                    .any(|k| iou(b, k) > settings.iou_threshold)
                {
                    continue;
                }
                kept[b.class as usize].push(*b);

                self.add_detection(&mut rmeta, &state.labels, b);

                num_detections += 1;
                if num_detections >= settings.max_detections {
                    break;
                }
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

impl YoloTensorDec {
    fn add_detection(
        &self,
        rmeta: &mut gst::MetaRefMut<
            '_,
            gst_analytics::AnalyticsRelationMeta,
            gst::meta::Standalone,
        >,
        labels: &[glib::Quark],
        b: &BoundingBox,
    ) {
        // Calculate top-left corner and width/height from top-left and bottom-right corner
        let x = b.xmin.round() as i32;
        let y = b.ymin.round() as i32;
        let width = (b.xmax - b.xmin).round() as i32;
        let height = (b.ymax - b.ymin).round() as i32;

        let class = labels
            .get(b.class as usize)
            .copied()
            .unwrap_or_else(|| glib::Quark::from_str(glib::gformat!("CLASS-{}", b.class)));

        let od_meta = if let Some(rotation) = b.rotation {
            gst::log!(
                CAT,
                imp = self,
                "Adding object {} with confidence {} at ({x}, {y}) with size {width}x{height} rotation {rotation}",
                class.as_str(),
                b.confidence,
            );

            rmeta
                .add_oriented_od_mtd(class, x, y, width, height, rotation, b.confidence)
                .unwrap()
                .id()
        } else {
            gst::log!(
                CAT,
                imp = self,
                "Adding object {} with confidence {} at ({x}, {y}) with size {width}x{height}",
                class.as_str(),
                b.confidence,
            );

            rmeta
                .add_od_mtd(class, x, y, width, height, b.confidence)
                .unwrap()
                .id()
        };
        let cls_meta = rmeta.add_one_cls_mtd(b.confidence, class).unwrap().id();
        rmeta
            .set_relation(gst_analytics::RelTypes::RELATE_TO, od_meta, cls_meta)
            .unwrap();
    }
}

fn find_yolo_tensor_meta(
    buffer: &gst::BufferRef,
    type_: glib::Type,
) -> Option<gst::MetaRef<'_, gst_analytics::TensorMeta>> {
    buffer
        .iter_meta::<gst_analytics::TensorMeta>()
        .find(|meta| {
            let format = tensor_format_from_type(type_);
            let (model, order) = match format {
                YoloTensorFormat::V8 => (YOLOV8_OUT, gst_analytics::TensorDimOrder::ColMajor),
                YoloTensorFormat::X => (YOLOX_OUT, gst_analytics::TensorDimOrder::RowMajor),
                YoloTensorFormat::Y26 => (YOLO26_OUT, gst_analytics::TensorDimOrder::RowMajor),
                YoloTensorFormat::V8O => (YOLOV8OBB_OUT, gst_analytics::TensorDimOrder::ColMajor),
                YoloTensorFormat::Y26O => (YOLO26OBB_OUT, gst_analytics::TensorDimOrder::RowMajor),
            };

            let Some(tensor) = meta.typed_tensor(
                glib::Quark::from_static_str(model),
                gst_analytics::TensorDataType::Float32,
                order,
                &[1, usize::MAX, usize::MAX],
            ) else {
                return false;
            };

            if tensor.dims().len() != 3 {
                return false;
            }

            // YoloV8: at least 5 fields (4 bounding box + 1 class) in dims[1].
            // YoloX: at least 6 fields (4 bounding box + 1 box conf + 1 class) in dims[2].
            // Yolo26: exactly 6 fields (4 bounding box + 1 score + 1 class) in dims[2].
            // YoloV8Obb: at least 6 fields (5 bounding box + 1 class) in dims[1].
            // Yolo26Obb: exactly 7 fields (4 oriented bounding box + 1 score + 1 class + 1 rotation) in dims[2].
            match format {
                YoloTensorFormat::V8 => tensor.dims()[1] >= 5,
                YoloTensorFormat::X => tensor.dims()[2] >= 6,
                YoloTensorFormat::Y26 => tensor.dims()[2] == 6,
                YoloTensorFormat::V8O => tensor.dims()[1] >= 6,
                YoloTensorFormat::Y26O => tensor.dims()[2] == 7,
            }
        })
}

impl super::YoloTensorDecImpl for YoloTensorDec {}

#[derive(Default)]
pub struct YoloV8TensorDec {}

#[glib::object_subclass]
impl ObjectSubclass for YoloV8TensorDec {
    const NAME: &'static str = "GstYoloV8TensorDec";
    type Type = super::YoloV8TensorDec;
    type ParentType = super::YoloTensorDec;
}

impl ObjectImpl for YoloV8TensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(Settings::nms_properties);
        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        imp.settings.lock().unwrap().nms_set_property(value, pspec);
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        imp.settings.lock().unwrap().nms_get_property(pspec)
    }
}

impl GstObjectImpl for YoloV8TensorDec {}

impl ElementImpl for YoloV8TensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YoloV8-V10, Yolo11, Yolo12 and Yolo26 Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YoloV8-v10, Yolo11, Yolo12 and Yolo26 model \
                 from the one-to-many head",
                "Sebastian Dröge <sebastian@centricular.com>",
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
                            YOLOV8_OUT,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", YOLOV8_OUT)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        1i32.to_send_value(),
                                        gst::IntRange::<i32>::new(5, i32::MAX).to_send_value(),
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "col-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .any_features()
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new().any_features().build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for YoloV8TensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
}

impl super::YoloTensorDecImpl for YoloV8TensorDec {}

#[derive(Default)]
pub struct YoloXTensorDec {}

#[glib::object_subclass]
impl ObjectSubclass for YoloXTensorDec {
    const NAME: &'static str = "GstYoloXTensorDec";
    type Type = super::YoloXTensorDec;
    type ParentType = super::YoloTensorDec;
}

impl ObjectImpl for YoloXTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let mut properties = Settings::nms_properties();
            properties.push(
                glib::ParamSpecFloat::builder("box-confidence-threshold")
                    .nick("Box Confidence Threshold")
                    .blurb("Boxes with a location confidence level inferior to this threshold will be excluded")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().box_confidence_threshold)
                    .mutable_playing()
                    .build(),
            );
            properties
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        match pspec.name() {
            "box-confidence-threshold" => {
                let mut settings = imp.settings.lock().unwrap();
                settings.box_confidence_threshold = value.get().unwrap();
            }
            _ => imp.settings.lock().unwrap().nms_set_property(value, pspec),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        match pspec.name() {
            "box-confidence-threshold" => {
                let settings = imp.settings.lock().unwrap();
                settings.box_confidence_threshold.to_value()
            }
            _ => imp.settings.lock().unwrap().nms_get_property(pspec),
        }
    }
}

impl GstObjectImpl for YoloXTensorDec {}

impl ElementImpl for YoloXTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YoloX Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YoloX model",
                "Sebastian Dröge <sebastian@centricular.com>",
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
                            YOLOX_OUT,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", YOLOX_OUT)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        1i32.to_send_value(),
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        gst::IntRange::<i32>::new(6, i32::MAX).to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "row-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .any_features()
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new().any_features().build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for YoloXTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
}

impl super::YoloTensorDecImpl for YoloXTensorDec {}

#[derive(Default)]
pub struct Yolo26TensorDec;

#[glib::object_subclass]
impl ObjectSubclass for Yolo26TensorDec {
    const NAME: &'static str = "GstYolo26TensorDec";
    type Type = super::Yolo26TensorDec;
    type ParentType = super::YoloTensorDec;
}

impl ObjectImpl for Yolo26TensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("score-threshold")
                    .nick("Score Threshold")
                    .blurb("Detections with a score inferior to this threshold will be excluded")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().score_threshold)
                    .mutable_playing()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        match pspec.name() {
            "score-threshold" => {
                let mut settings = imp.settings.lock().unwrap();
                settings.score_threshold = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        match pspec.name() {
            "score-threshold" => {
                let settings = imp.settings.lock().unwrap();
                settings.score_threshold.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Yolo26TensorDec {}

impl ElementImpl for Yolo26TensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YoloV10, Yolo11, Yolo12 and Yolo26 end2end Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YoloV10, Yolo11, Yolo12 and Yolo26 model \
                 from the end2end (one-to-one) head",
                "Sebastian Dröge <sebastian@centricular.com>",
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
                            YOLO26_OUT,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", YOLO26_OUT)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        1i32.to_send_value(),
                                        gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        6i32.to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "row-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .any_features()
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new().any_features().build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for Yolo26TensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
}

impl super::YoloTensorDecImpl for Yolo26TensorDec {}

#[derive(Default)]
pub struct YoloV8ObbTensorDec {}

#[glib::object_subclass]
impl ObjectSubclass for YoloV8ObbTensorDec {
    const NAME: &'static str = "GstYoloV8ObbTensorDec";
    type Type = super::YoloV8ObbTensorDec;
    type ParentType = super::YoloTensorDec;
}

impl ObjectImpl for YoloV8ObbTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(Settings::nms_properties);
        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        imp.settings.lock().unwrap().nms_set_property(value, pspec);
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        imp.settings.lock().unwrap().nms_get_property(pspec)
    }
}

impl GstObjectImpl for YoloV8ObbTensorDec {}

impl ElementImpl for YoloV8ObbTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YoloV8-V10-obb, Yolo11-obb, Yolo12-obb and Yolo26-obb Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YoloV8-v10-obb, Yolo11-obb, Yolo12-obb and Yolo26-obb model \
                 from the one-to-many head",
                "Jan Schmidt <jan@centricular.com>",
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
                            YOLOV8OBB_OUT,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", YOLOV8OBB_OUT)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        1i32.to_send_value(),
                                        gst::IntRange::<i32>::new(6, i32::MAX).to_send_value(),
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "col-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .any_features()
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new().any_features().build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for YoloV8ObbTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
}

impl super::YoloTensorDecImpl for YoloV8ObbTensorDec {}

#[derive(Default)]
pub struct Yolo26ObbTensorDec;

#[glib::object_subclass]
impl ObjectSubclass for Yolo26ObbTensorDec {
    const NAME: &'static str = "GstYolo26ObbTensorDec";
    type Type = super::Yolo26ObbTensorDec;
    type ParentType = super::YoloTensorDec;
}

impl ObjectImpl for Yolo26ObbTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("score-threshold")
                    .nick("Score Threshold")
                    .blurb("Detections with a score inferior to this threshold will be excluded")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().score_threshold)
                    .mutable_playing()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        match pspec.name() {
            "score-threshold" => {
                let mut settings = imp.settings.lock().unwrap();
                settings.score_threshold = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloTensorDec>().imp();
        match pspec.name() {
            "score-threshold" => {
                let settings = imp.settings.lock().unwrap();
                settings.score_threshold.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Yolo26ObbTensorDec {}

impl ElementImpl for Yolo26ObbTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YoloV10-obb, Yolo11-obb, Yolo12-obb and Yolo26-obb end2end Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YoloV10-obb, Yolo11-obb, Yolo12-obb and Yolo26-obb model \
                 from the end2end (one-to-one) head",
                "Jan Schmidt <jan@centricular.com>",
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
                            YOLO26OBB_OUT,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", YOLO26OBB_OUT)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        1i32.to_send_value(),
                                        gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        7i32.to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "row-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .any_features()
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new().any_features().build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for Yolo26ObbTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
}

impl super::YoloTensorDecImpl for Yolo26ObbTensorDec {}
