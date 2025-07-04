// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-yoloxtensordec
 * @see_also: objectdetectionoverlay, burn-yoloxinference.
 *
 * Tensor decoder element for [YOLOX](https://github.com/Megvii-BaseDetection/YOLOX)-based object
 * detection.
 *
 * |[
 * gst-launch-1.0 souphttpsrc location=https://raw.githubusercontent.com/tracel-ai/models/ab8c64bd7e1f45e99cc321ce900a5b5e6b97910c/yolox-burn/samples/dog_bike_man.jpg \
 *     ! jpegdec ! videoconvertscale ! "video/x-raw,width=800,height=640" \
 *     ! burn-yoloxinference ! yoloxtensordec label-file=COCO_classes.txt \
 *     ! videoconvertscale ! objectdetectionoverlay \
 *     ! videoconvertscale ! imagefreeze ! autovideosink -v
 * |] This takes a JPEG, performs object detection via `burn-yoloxinference` on it, decodes the
 * inferred tensors with `yoloxtensordec` and then overlays the detected objects on the frame via
 * `objectdetectionoverlay`.
 *
 * Since: plugins-rs-0.15.0
 */
use gst::{glib, subclass::prelude::*};
use gst_analytics::prelude::*;
use gst_video::{prelude::*, subclass::prelude::*};

use byte_slice_cast::*;

use std::sync::{LazyLock, Mutex};

const YOLOX_OUT: &glib::GStr = glib::gstr!("yolox-out");

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "yoloxtensordec",
        gst::DebugColorFlags::empty(),
        Some("YOLOX tensor decoder element"),
    )
});

struct Settings {
    box_confidence_threshold: f32,
    class_confidence_threshold: f32,
    iou_threshold: f32,
    max_detections: u32,
    label_file: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            box_confidence_threshold: 0.4,
            class_confidence_threshold: 0.4,
            iou_threshold: 0.7,
            max_detections: 100,
            label_file: None,
        }
    }
}

struct State {
    labels: Vec<glib::Quark>,
}

#[derive(Default)]
pub struct YoloxTensorDec {
    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for YoloxTensorDec {
    const NAME: &'static str = "GstYoloxTensorDec";
    type Type = super::YoloxTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for YoloxTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("box-confidence-threshold")
                    .nick("Box Confidence Threshold")
                    .blurb("Boxes with a location confidence level inferior to this threshold will be excluded")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().box_confidence_threshold)
                    .mutable_playing()
                    .build(),
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
                    .blurb("Maximum intersection-over-union between bounding boxes to consider them distinct")
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
            "box-confidence-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.box_confidence_threshold = value.get().unwrap();
            }
            "class-confidence-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.class_confidence_threshold = value.get().unwrap();
            }
            "iou-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.iou_threshold = value.get().unwrap();
            }
            "max-detections" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_detections = value.get().unwrap();
            }
            "label-file" => {
                let mut settings = self.settings.lock().unwrap();
                settings.label_file = value.get().unwrap();
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "box-confidence-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.box_confidence_threshold.to_value()
            }
            "class-confidence-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.class_confidence_threshold.to_value()
            }
            "iou-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.iou_threshold.to_value()
            }
            "max-detections" => {
                let settings = self.settings.lock().unwrap();
                settings.max_detections.to_value()
            }
            "label-file" => {
                let settings = self.settings.lock().unwrap();
                settings.label_file.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for YoloxTensorDec {}

impl ElementImpl for YoloxTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YOLOX Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YOLOX model",
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
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        1i32.to_send_value(),
                                        0i32.to_send_value(),
                                        gst::IntRange::<i32>::new(5, i32::MAX).to_send_value(),
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

            let src_caps = gst_video::VideoCapsBuilder::new().build();
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

impl BaseTransformImpl for YoloxTensorDec {
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

        let Some(meta) = find_yolox_tensor_meta(buffer) else {
            gst::trace!(CAT, imp = self, "No YOLOX tensor meta found");
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

        let num_columns = tensor.dims()[2];
        gst::log!(
            CAT,
            imp = self,
            "Received {} boxes with {} classes",
            data.len() / num_columns,
            num_columns - 4 - 1,
        );

        let settings = self.settings.lock().unwrap();

        // Non-maximum suppression (NMS) filter conceptually based on the one in
        // https://github.com/tracel-ai/models/blob/main/yolox-burn/src/model/boxes.rs

        let mut candidate_boxes = vec![];
        for b in data.chunks_exact(tensor.dims()[2]) {
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
            candidate_boxes.push(BoundingBox {
                xmin: b[0] - b[2] / 2.,
                ymin: b[1] - b[3] / 2.,
                xmax: b[0] + b[2] / 2.,
                ymax: b[1] + b[3] / 2.,
                class: class as u32,
                confidence: combined_confidence,
            });
        }

        // Sort boxes by class and then by decreasing confidence
        candidate_boxes.sort_unstable_by(|a, b| {
            a.class
                .cmp(&b.class)
                .then_with(|| a.confidence.total_cmp(&b.confidence).reverse())
        });

        drop(map);
        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);

        // For all boxes of the same class, perform non-maximum suppression
        for b in candidate_boxes.chunk_by_mut(|a, b| a.class == b.class) {
            let mut current_index = 0;
            for index in 0..b.len() {
                let mut drop = false;
                for prev_index in 0..current_index {
                    let iou = iou(&b[prev_index], &b[index]);
                    if iou > settings.iou_threshold {
                        drop = true;
                        break;
                    }
                }
                if !drop {
                    b.swap(current_index, index);
                    current_index += 1;
                }
            }

            // Add relation meta for the remaining boxes
            for b in &b[0..current_index] {
                // Calculate top-left corner and width/height from top-left and bottom-right corner
                let x = b.xmin as i32;
                let y = b.ymin as i32;
                let width = (b.xmax - b.xmin) as i32;
                let height = (b.ymax - b.ymin) as i32;

                let class = state
                    .labels
                    .get(b.class as usize)
                    .copied()
                    .unwrap_or_else(|| glib::Quark::from_str(glib::gformat!("CLASS-{}", b.class)));

                gst::log!(
                    CAT,
                    imp = self,
                    "Adding object {} with confidence {} at ({x}, {y}) with size {width}x{height}",
                    class.as_str(),
                    b.confidence,
                );

                let od_meta = rmeta
                    .add_od_mtd(class, x, y, width, height, b.confidence)
                    .unwrap()
                    .id();
                let cls_meta = rmeta.add_one_cls_mtd(b.confidence, class).unwrap().id();
                rmeta
                    .set_relation(gst_analytics::RelTypes::RELATE_TO, od_meta, cls_meta)
                    .unwrap();
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

fn find_yolox_tensor_meta(
    buffer: &gst::BufferRef,
) -> Option<gst::MetaRef<'_, gst_analytics::TensorMeta>> {
    buffer
        .iter_meta::<gst_analytics::TensorMeta>()
        .find(|meta| {
            let Some(tensor) = meta.typed_tensor(
                glib::Quark::from_static_str(YOLOX_OUT),
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[1, usize::MAX, usize::MAX],
            ) else {
                return false;
            };

            if tensor.dims().len() != 3 {
                return false;
            }

            // Need at least the bounding box (4) and the box confidence (1)
            // and at least the confidence for a single class (1)
            if tensor.dims()[2] < 4 + 1 + 1 {
                return false;
            }

            true
        })
}

#[derive(Debug)]
struct BoundingBox {
    xmin: f32,
    xmax: f32,
    ymin: f32,
    ymax: f32,
    class: u32,
    confidence: f32,
}

// Intersection over union of two bounding boxes
fn iou(b1: &BoundingBox, b2: &BoundingBox) -> f32 {
    let b1_area = (b1.xmax - b1.xmin + 1.0) * (b1.ymax - b1.ymin + 1.0);
    let b2_area = (b2.xmax - b2.xmin + 1.0) * (b2.ymax - b2.ymin + 1.0);
    let i_xmin = f32::max(b1.xmin, b2.xmin);
    let i_xmax = f32::min(b1.xmax, b2.xmax);
    let i_ymin = f32::max(b1.ymin, b2.ymin);
    let i_ymax = f32::min(b1.ymax, b2.ymax);
    let i_area = f32::max(i_xmax - i_xmin + 1.0, 0.0) * f32::max(i_ymax - i_ymin + 1.0, 0.0);
    i_area / (b1_area + b2_area - i_area)
}
