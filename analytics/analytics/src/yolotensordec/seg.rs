// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2026 Jan Schmidt <jan@centricular.com>
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

use super::imp::{CAT, Settings};
use super::util::{BoundingBox, iou, sigmoid};

const YOLOV8SEG_OUT_GROUP: &glib::GStr = glib::gstr!("yolo-v8-segmentation-out");
const YOLOV8SEG_DETECTIONS_OUT: &glib::GStr = glib::gstr!("yolo-v8-segmentation-out-detections");
const YOLOV8SEG_PROTOS_OUT: &glib::GStr = glib::gstr!("yolo-v8-segmentation-out-protos");

const YOLO26SEG_OUT_GROUP: &glib::GStr = glib::gstr!("yolo-26-segmentation-end2end-out");
const YOLO26SEG_DETECTIONS_OUT: &glib::GStr =
    glib::gstr!("yolo-26-segmentation-end2end-out-detections");
const YOLO26SEG_PROTOS_OUT: &glib::GStr = glib::gstr!("yolo-26-segmentation-end2end-out-protos");

#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstYoloSegDecoderScaleMode")]
pub enum ScaleMode {
    #[enum_value(
        name = "Native: Render the segmentation map at model native resolution",
        nick = "native"
    )]
    Native = 0,
    #[default]
    #[enum_value(
        name = "Bilinear Upscale: Upscale the segmentation map to video resolution with bilinear interpolation",
        nick = "bilinear-upscale"
    )]
    BilinearUpscale = 1,
}

const DEFAULT_SCALE_MODE: ScaleMode = ScaleMode::BilinearUpscale;

#[derive(Clone, Copy, Debug)]
enum YoloSegTensorGroupFormat {
    YV8SEG, // Col-major, [1, 4+C, N]
    Y26SEG, // Row-major, [1, N, 6], NMS done by the model
}

#[derive(Clone, Copy, Debug)]
struct YoloTensorInfo<'a> {
    name: &'a glib::GStr,
    dim_order: gst_analytics::TensorDimOrder,
}

#[derive(Clone)]
struct YoloSegTensors<'a> {
    meta: gst::MetaRef<'a, gst_analytics::TensorMeta>,
    detections_info: &'a YoloTensorInfo<'static>,
    protos_info: &'a YoloTensorInfo<'static>,
}

impl<'a> YoloSegTensors<'a> {
    fn detections(&'a self) -> &'a gst_analytics::Tensor {
        self.meta
            .typed_tensor(
                glib::Quark::from_static_str(self.detections_info.name),
                gst_analytics::TensorDataType::Float32,
                self.detections_info.dim_order,
                &[1, usize::MAX, usize::MAX],
            )
            .unwrap()
    }
    fn protos(&'a self) -> &'a gst_analytics::Tensor {
        self.meta
            .typed_tensor(
                glib::Quark::from_static_str(self.protos_info.name),
                gst_analytics::TensorDataType::Float32,
                self.protos_info.dim_order,
                &[1, usize::MAX, usize::MAX, usize::MAX],
            )
            .unwrap()
    }
}

const YOLOV8SEG_TENSORS_INFO: &(YoloTensorInfo, YoloTensorInfo) = &(
    YoloTensorInfo {
        name: YOLOV8SEG_DETECTIONS_OUT,
        dim_order: gst_analytics::TensorDimOrder::ColMajor,
    },
    YoloTensorInfo {
        name: YOLOV8SEG_PROTOS_OUT,
        dim_order: gst_analytics::TensorDimOrder::ColMajor,
    },
);

const YOLO26SEG_TENSORS_INFO: &(YoloTensorInfo, YoloTensorInfo) = &(
    YoloTensorInfo {
        name: YOLO26SEG_DETECTIONS_OUT,
        dim_order: gst_analytics::TensorDimOrder::RowMajor,
    },
    YoloTensorInfo {
        name: YOLO26SEG_PROTOS_OUT,
        dim_order: gst_analytics::TensorDimOrder::RowMajor,
    },
);

impl YoloTensorInfo<'_> {
    fn dim_order_to_static_str(&self) -> &'static str {
        match self.dim_order {
            gst_analytics::TensorDimOrder::RowMajor => "row-major",
            gst_analytics::TensorDimOrder::ColMajor => "col-major",
            _ => unreachable!(),
        }
    }
}

fn tensor_group_format_from_type(type_: glib::Type) -> YoloSegTensorGroupFormat {
    if type_ == super::YoloV8SegTensorDec::static_type() {
        YoloSegTensorGroupFormat::YV8SEG
    } else if type_ == super::Yolo26SegTensorDec::static_type() {
        YoloSegTensorGroupFormat::Y26SEG
    } else {
        unreachable!()
    }
}

#[derive(Clone, Debug)]
struct BoundingBoxWithMask {
    id: u32,
    bbox: BoundingBox,
    mask_weights: Vec<f32>,
    mask_buf: Option<gst::Buffer>,
}

impl BoundingBoxWithMask {
    fn from_box_and_mask_weights(bbox: BoundingBox, mask_weights: Vec<f32>) -> Self {
        Self {
            id: 0,
            bbox,
            mask_weights,
            mask_buf: None,
        }
    }
}

struct State {
    labels: Vec<glib::Quark>,
    video_info: Option<gst_video::VideoInfo>,
}

impl State {
    fn from_labels(labels: Vec<glib::Quark>) -> Self {
        Self {
            labels,
            video_info: None,
        }
    }
}

pub struct SegSettings {
    s: Settings,
    scale_mode: ScaleMode,
}

impl Default for SegSettings {
    fn default() -> Self {
        Self {
            s: Settings::default(),
            scale_mode: DEFAULT_SCALE_MODE,
        }
    }
}

#[derive(Default)]
pub struct YoloSegTensorDec {
    state: Mutex<Option<State>>,
    settings: Mutex<SegSettings>,
}

#[glib::object_subclass]
impl ObjectSubclass for YoloSegTensorDec {
    const ABSTRACT: bool = true;
    const NAME: &'static str = "GstYoloSegTensorDec";
    type Type = super::YoloSegTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for YoloSegTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("label-file")
                    .nick("Label File")
                    .blurb("Label file with one label per line")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder("scale-mode")
                    .nick("Scale Mode")
                    .blurb("Segmentation Map scaling mode")
                    .default_value(DEFAULT_SCALE_MODE)
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
                settings.s.label_file = value.get().unwrap();
            }
            "scale-mode" => {
                let mut settings = self.settings.lock().unwrap();
                settings.scale_mode = value.get().unwrap();
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "label-file" => {
                let settings = self.settings.lock().unwrap();
                settings.s.label_file.to_value()
            }
            "scale-mode" => {
                let settings = self.settings.lock().unwrap();
                settings.scale_mode.to_value()
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

impl GstObjectImpl for YoloSegTensorDec {}

impl ElementImpl for YoloSegTensorDec {}

impl BaseTransformImpl for YoloSegTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        let settings = self.settings.lock().unwrap();
        let labels = settings
            .s
            .load_labels()
            .inspect_err(|err| {
                gst::error!(CAT, imp = self, "{}", err);
            })?
            .unwrap_or_else(|| {
                gst::debug!(CAT, imp = self, "No labels file provided");
                Vec::new()
            });

        *state = Some(State::from_labels(labels));

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }
    fn set_caps(&self, input: &gst::Caps, _output: &gst::Caps) -> Result<(), gst::LoggableError> {
        let mut state_guard = self.state.lock().unwrap();
        let Some(state) = &mut *state_guard else {
            return Err(gst::loggable_error!(CAT, "Wrong State"));
        };

        let in_info = gst_video::VideoInfo::from_caps(input)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse input caps"))?;
        state.video_info = Some(in_info);

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

        let tensor_format = tensor_group_format_from_type(self.obj().type_());
        let Some(tensors) = find_yolo_seg_tensors(buffer, tensor_format) else {
            gst::trace!(CAT, imp = self, "Yolo segmentation tensor meta not found");
            return Ok(gst::FlowSuccess::Ok);
        };

        let (detections, protos) = (tensors.detections(), tensors.protos());

        if protos.dims()[0] != 1 {
            gst::error!(
                CAT,
                imp = self,
                "Invalid number of batches {}",
                protos.dims()[0]
            );
            return Err(gst::FlowError::Error);
        }

        let [num_masks, mask_w, mask_h] = protos.dims()[1..4] else {
            gst::error!(
                CAT,
                imp = self,
                "Invalid protos tensor dims {:?}",
                protos.dims()
            );
            return Err(gst::FlowError::Error);
        };

        if detections.dims()[0] != 1 {
            gst::error!(
                CAT,
                imp = self,
                "Invalid number of batches {}",
                detections.dims()[0]
            );
            return Err(gst::FlowError::Error);
        }

        // The v8 one-to-many and "v26" end-to-end tensors use different memory layouts.
        let (num_candidates, num_fields) = match tensor_format {
            YoloSegTensorGroupFormat::YV8SEG => {
                // YoloV8: dims[1] = num_fields, dims[2] = num_candidates
                // Planar / column-major: field f of candidate c is at data[c + f * stride]
                (detections.dims()[2], detections.dims()[1])
            }
            YoloSegTensorGroupFormat::Y26SEG => {
                // Yolo26: dims[1] = num_candidates, dims[2] = 6 (fixed)
                (detections.dims()[1], detections.dims()[2])
            }
        };
        // YoloV8Seg has no box confidence field, but has rotation, fields 5.. are class scores.
        // Yolo26Seg has a single score on a bounding box and class index, no per-class scores.
        let num_classes = match tensor_format {
            YoloSegTensorGroupFormat::YV8SEG => num_fields - 4 - num_masks,
            YoloSegTensorGroupFormat::Y26SEG => 1, // Actually, this value isn't used for yolo v26
                                                   // except for the debug statement below, but set it to 1 for that
        };

        gst::log!(
            CAT,
            imp = self,
            "Received {num_candidates} boxes with {num_classes} classes",
        );

        let settings = self.settings.lock().unwrap();

        let Ok(detections_map) = detections.data().map_readable() else {
            return Err(gst::FlowError::Error);
        };
        let Ok(detections_data) = detections_map.as_slice_of::<f32>() else {
            return Err(gst::FlowError::Error);
        };
        let Ok(protos_map) = protos.data().map_readable() else {
            return Err(gst::FlowError::Error);
        };
        let Ok(protos_data) = protos_map.as_slice_of::<f32>() else {
            return Err(gst::FlowError::Error);
        };

        // Non-maximum suppression (NMS) filter conceptually based on the one in
        // https://github.com/tracel-ai/models/blob/main/yolox-burn/src/model/boxes.rs
        let mut candidate_boxes = vec![];
        match tensor_format {
            YoloSegTensorGroupFormat::YV8SEG => {
                // YoloV8 planar layout: field f of candidate c is at data[c + f * stride]
                // stride = dims[2] = num_candidates
                let stride = num_candidates;
                let (boxes, extras) = detections_data.split_at(4 * stride);
                let (xs, rest) = boxes.split_at(stride);
                let (ys, rest) = rest.split_at(stride);
                let (widths, heights) = rest.split_at(stride);
                let (classes, all_mask_weights) = extras.split_at(num_classes * stride);

                for (c, (&x, &y, &width, &height)) in izip!(xs, ys, widths, heights).enumerate() {
                    // Find max confidence across all class confidence slices
                    let (class, max_confidence) = classes
                        .chunks_exact(stride)
                        .enumerate()
                        .map(|(i, chunk)| (i as u32, chunk[c]))
                        .max_by(|(_, a), (_, b)| a.total_cmp(b))
                        .unwrap();
                    if max_confidence < settings.s.class_confidence_threshold {
                        continue;
                    }

                    // Collect mask weights - the c'th entry from each row
                    let mask_weights = all_mask_weights
                        .chunks_exact(stride)
                        .map(|m| m[c])
                        .collect::<Vec<_>>();

                    candidate_boxes.push(BoundingBoxWithMask::from_box_and_mask_weights(
                        BoundingBox::from_center_extents(
                            x,
                            y,
                            width,
                            height,
                            None,
                            class,
                            max_confidence,
                        ),
                        mask_weights,
                    ));
                }
            }
            YoloSegTensorGroupFormat::Y26SEG => {
                // Yolo26Seg (end2end) interleaved layout: each row is a finalized detection
                for det in detections_data.chunks_exact(num_fields) {
                    let score = det[4];
                    if score < settings.s.score_threshold {
                        continue;
                    }

                    let (x1, y1, x2, y2) = (det[0], det[1], det[2], det[3]);
                    let class = det[5] as u32;
                    let mask_weights = &det[6..];

                    gst::trace!(
                        CAT,
                        imp = self,
                        "candidate x {x1}, y {y1} to x2 {x2}, y2 {y2}, class {class} confidence {score} {0} mask_weights",
                        mask_weights.len()
                    );

                    candidate_boxes.push(BoundingBoxWithMask::from_box_and_mask_weights(
                        BoundingBox::from_corners(x1, y1, x2, y2, None, class, score),
                        mask_weights.to_vec(),
                    ));
                }
            }
        }

        // Calculate which boxes we're keeping, and generate mask buffers
        let mut candidate_boxes = if matches!(tensor_format, YoloSegTensorGroupFormat::Y26SEG) {
            // Yolo26 and Yolo26Obb: NMS is already done by the model, so emit all detections
            // that passed the score threshold.
            for (next_id, b) in (1..).zip(candidate_boxes.iter_mut()) {
                b.id = next_id;

                let mask_buf = self.prepare_mask_buffer(
                    state,
                    b,
                    settings.scale_mode,
                    mask_w,
                    mask_h,
                    protos_data,
                )?;
                b.mask_buf = Some(mask_buf);
            }

            candidate_boxes
        } else {
            // YoloV8: perform non-maximum suppression per class,
            // processing the boxes in globally decreasing confidence order, so
            // that the max-detections limit keeps the highest confidence
            // detections.
            let mut kept: Vec<Vec<BoundingBoxWithMask>> = vec![Vec::new(); num_classes];
            let mut num_detections = 0;

            // Sort full candidate list by confidence, then discard overlaps
            candidate_boxes
                .sort_unstable_by(|a, b| b.bbox.confidence.total_cmp(&a.bbox.confidence));
            for b in &mut candidate_boxes {
                if kept[b.bbox.class as usize]
                    .iter()
                    .any(|k| iou(&b.bbox, &k.bbox) > settings.s.iou_threshold)
                {
                    continue;
                }

                b.id = num_detections + 1;
                let mask_buf = self.prepare_mask_buffer(
                    state,
                    b,
                    settings.scale_mode,
                    mask_w,
                    mask_h,
                    protos_data,
                )?;
                let mut out_b = b.clone();
                out_b.mask_buf = Some(mask_buf);

                kept[b.bbox.class as usize].push(out_b);

                num_detections += 1;
                if num_detections >= settings.s.max_detections {
                    break;
                }
            }

            kept.into_iter().flatten().collect()
        };

        // Sort final boxes by decreasing confidence
        candidate_boxes.sort_unstable_by(|a, b| b.bbox.confidence.total_cmp(&a.bbox.confidence));

        drop(detections_map);
        drop(protos_map);

        // Add meta to buffer and add masks and detection boxes
        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);
        for b in &mut candidate_boxes {
            self.add_detection(&mut rmeta, &state.labels, b);
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

impl YoloSegTensorDec {
    fn prepare_mask_buffer(
        &self,
        state: &State,
        b: &BoundingBoxWithMask,
        scale_mode: ScaleMode,
        mask_protos_w: usize,
        mask_protos_h: usize,
        protos_data: &[f32],
    ) -> Result<gst::Buffer, gst::FlowError> {
        // Size of a single plane of mask prototypes
        let mask_proto_size = mask_protos_w * mask_protos_h;

        let (video_w, video_h) = if let Some(video_info) = &state.video_info {
            (video_info.width(), video_info.height())
        } else {
            gst::error!(CAT, imp = self, "Caps weren't configured before dataflow");
            return Err(gst::FlowError::NotNegotiated);
        };

        // Calculate the scaling between video and segmentation prototype tensors
        let (mask_scale_w, mask_scale_h) = (
            mask_protos_w as f32 / video_w as f32,
            mask_protos_h as f32 / video_h as f32,
        );

        // Bounding box region in the prototypes mask as pixel extents
        let bb_mask_pixels = b
            .bbox
            .scale_and_clamp(
                mask_scale_w,
                mask_scale_h,
                mask_protos_w as f32,
                mask_protos_h as f32,
            )
            .pixel_extents();

        // output resolution to generate is based on scaling mode
        let (output_width, output_height) = match scale_mode {
            ScaleMode::Native => {
                // Render at mask prototype resolution
                (bb_mask_pixels.width, bb_mask_pixels.height)
            }
            ScaleMode::BilinearUpscale => {
                // Render at video resolution
                (b.bbox.pixel_width(), b.bbox.pixel_height())
            }
        };

        let mask_info = gst_video::VideoInfo::builder(
            gst_video::VideoFormat::Gray8,
            output_width as u32,
            output_height as u32,
        )
        .build()
        .unwrap();

        let out_stride = mask_info.stride()[0] as usize;

        gst::trace![
            CAT,
            imp = self,
            "Allocating mask buffer of size {0} for mask width {1} height {2} for region x {3} y {4} width {5} height {6}",
            mask_info.size(),
            mask_info.width(),
            mask_info.height(),
            bb_mask_pixels.x,
            bb_mask_pixels.y,
            bb_mask_pixels.width,
            bb_mask_pixels.height,
        ];
        let mut mask_buf =
            gst::Buffer::with_size(mask_info.size()).map_err(|_| gst::FlowError::Error)?;

        {
            let buf = mask_buf.get_mut().unwrap();

            gst_video::VideoMeta::add_from_info(
                buf,
                gst_video::VideoFrameFlags::empty(),
                &mask_info,
            )
            .map_err(|_| gst::FlowError::Error)?;

            gst::debug![
                CAT,
                imp = self,
                "Rendering mask region x {0} y {1} w {2} h{3} to output size w {4} h {5}",
                bb_mask_pixels.x,
                bb_mask_pixels.y,
                bb_mask_pixels.width,
                bb_mask_pixels.height,
                output_width,
                output_height,
            ];

            // num_mask x mask_protos_w x mask_protos_h
            let mut out_mask_data = buf.map_writable().map_err(|_| gst::FlowError::Error)?;
            match scale_mode {
                ScaleMode::Native => {
                    for my in 0..bb_mask_pixels.height {
                        let in_row = (bb_mask_pixels.y + my) * mask_protos_w;
                        let out_row = my * out_stride;

                        for mx in 0..bb_mask_pixels.width {
                            let mut sum = 0.0;
                            let offset = in_row + bb_mask_pixels.x + mx;

                            for (k, w) in b.mask_weights.iter().enumerate() {
                                let (_, cur_mask_proto) = protos_data.split_at(k * mask_proto_size);
                                sum += w * cur_mask_proto[offset];
                            }

                            let out_offset = out_row + mx;
                            // FIXME: How to handle more than 255 segmentation instances cleanly?
                            // The bounding box id gets truncated here
                            out_mask_data[out_offset] =
                                if sigmoid(sum) > 0.5 { b.id as u8 } else { 0 };
                        }
                    }
                }
                ScaleMode::BilinearUpscale => {
                    // Calculate the map logits we're going to sample
                    let mut logits_map = Vec::with_capacity(bb_mask_pixels.size());
                    for my in 0..bb_mask_pixels.height {
                        let in_row = (bb_mask_pixels.y + my) * mask_protos_w;
                        for mx in 0..bb_mask_pixels.width {
                            let mut sum = 0.0;
                            let offset = in_row + bb_mask_pixels.x + mx;

                            for (k, w) in b.mask_weights.iter().enumerate() {
                                let (_, cur_mask_proto) = protos_data.split_at(k * mask_proto_size);
                                sum += w * cur_mask_proto[offset];
                            }

                            logits_map.push(sum);
                        }
                    }
                    assert_eq![logits_map.len(), bb_mask_pixels.size()];

                    // Now upscale and sample the logits
                    for y in 0..output_height {
                        // Calculate y0 and y1 anchor lines in the map prototypes
                        let my = (y as f32) * mask_scale_h;
                        let mask_y0 = my.floor() as usize;
                        let mask_y0 = mask_y0.clamp(0, bb_mask_pixels.height - 1);
                        let mask_y1 = (mask_y0 + 1).min(bb_mask_pixels.height - 1);

                        let ty = my - (mask_y0 as f32);

                        let in_row0 = mask_y0 * bb_mask_pixels.width;
                        let in_row1 = mask_y1 * bb_mask_pixels.width;

                        let out_row = y * out_stride;

                        for x in 0..output_width {
                            let mx = (x as f32) * mask_scale_w;
                            let mask_x0 = mx.floor() as usize;
                            let mask_x0 = mask_x0.clamp(0, bb_mask_pixels.width - 1);
                            let mask_x1 = (mask_x0 + 1).min(bb_mask_pixels.width - 1);

                            let sum00 = logits_map[in_row0 + mask_x0];
                            let sum01 = logits_map[in_row0 + mask_x1];
                            let sum10 = logits_map[in_row1 + mask_x0];
                            let sum11 = logits_map[in_row1 + mask_x1];

                            let tx = mx - (mask_x0 as f32);
                            let w00 = (1.0 - tx) * (1.0 - ty);
                            let w01 = tx * (1.0 - ty);
                            let w10 = (1.0 - tx) * ty;
                            let w11 = tx * ty;

                            let sum = sum00 * w00 + sum01 * w01 + sum10 * w10 + sum11 * w11;

                            // FIXME: How to handle more than 255 segmentation instances cleanly?
                            // The bounding box id gets truncated here
                            out_mask_data[out_row + x] =
                                if sigmoid(sum) > 0.5 { b.id as u8 } else { 0 };
                        }
                    }
                }
            }
        }

        Ok(mask_buf)
    }

    fn add_detection(
        &self,
        rmeta: &mut gst::MetaRefMut<
            '_,
            gst_analytics::AnalyticsRelationMeta,
            gst::meta::Standalone,
        >,
        labels: &[glib::Quark],
        b: &mut BoundingBoxWithMask,
    ) {
        // Calculate top-left corner and width/height from top-left and bottom-right corner
        let bbox = &b.bbox;
        let mask_buf = b.mask_buf.take().unwrap();

        let x = bbox.xmin.round() as i32;
        let y = bbox.ymin.round() as i32;
        let width = (bbox.xmax - bbox.xmin).round() as u32;
        let height = (bbox.ymax - bbox.ymin).round() as u32;

        let class = labels
            .get(bbox.class as usize)
            .copied()
            .unwrap_or_else(|| glib::Quark::from_str(glib::gformat!("CLASS-{}", bbox.class)));

        gst::log!(
            CAT,
            imp = self,
            "Adding object {} with confidence {} at ({x}, {y}) with size {width}x{height}",
            class.as_str(),
            bbox.confidence,
        );

        let od_meta = rmeta
            .add_od_mtd(class, x, y, width as i32, height as i32, bbox.confidence)
            .unwrap()
            .id();

        let region_ids = [b.id];
        let seg_meta = rmeta
            .add_segmentation_mtd(
                mask_buf,
                gst_analytics::SegmentationType::Instance,
                &region_ids,
                x,
                y,
                width,
                height,
            )
            .unwrap()
            .id();
        rmeta
            .set_relation(gst_analytics::RelTypes::RELATE_TO, od_meta, seg_meta)
            .unwrap();
        rmeta
            .set_relation(gst_analytics::RelTypes::RELATE_TO, seg_meta, od_meta)
            .unwrap();

        let cls_meta = rmeta.add_one_cls_mtd(bbox.confidence, class).unwrap().id();
        rmeta
            .set_relation(gst_analytics::RelTypes::RELATE_TO, od_meta, cls_meta)
            .unwrap();
    }
}

fn find_yolo_seg_tensors(
    buffer: &gst::BufferRef,
    format: YoloSegTensorGroupFormat,
) -> Option<YoloSegTensors<'_>> {
    let (detections_info, protos_info) = match format {
        YoloSegTensorGroupFormat::YV8SEG => &YOLOV8SEG_TENSORS_INFO,
        YoloSegTensorGroupFormat::Y26SEG => &YOLO26SEG_TENSORS_INFO,
    };

    buffer
        .iter_meta::<gst_analytics::TensorMeta>()
        .find_map(|meta| {
            let detections = meta.typed_tensor(
                glib::Quark::from_static_str(detections_info.name),
                gst_analytics::TensorDataType::Float32,
                detections_info.dim_order,
                &[1, usize::MAX, usize::MAX],
            )?;
            if detections.dims()[1] < 6 && detections.dims().len() != 3 {
                return None;
            }

            if let Some(protos) = meta.typed_tensor(
                glib::Quark::from_static_str(protos_info.name),
                gst_analytics::TensorDataType::Float32,
                protos_info.dim_order,
                &[1, usize::MAX, usize::MAX, usize::MAX],
            ) {
                if protos.dims().len() != 4 {
                    return None;
                }
            } else {
                gst::warning![CAT, "found valid detections tensor, but no protos tensor"];
                return None;
            };

            Some(YoloSegTensors {
                meta: meta.clone(),
                detections_info,
                protos_info,
            })
        })
}

impl super::YoloSegTensorDecImpl for YoloSegTensorDec {}

#[derive(Default)]
pub struct YoloV8SegTensorDec {}

#[glib::object_subclass]
impl ObjectSubclass for YoloV8SegTensorDec {
    const NAME: &'static str = "GstYoloV8SegTensorDec";
    type Type = super::YoloV8SegTensorDec;
    type ParentType = super::YoloSegTensorDec;
}

impl ObjectImpl for YoloV8SegTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(Settings::nms_properties);
        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloSegTensorDec>().imp();
        imp.settings
            .lock()
            .unwrap()
            .s
            .nms_set_property(value, pspec);
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloSegTensorDec>().imp();
        imp.settings.lock().unwrap().s.nms_get_property(pspec)
    }
}

impl GstObjectImpl for YoloV8SegTensorDec {}

impl ElementImpl for YoloV8SegTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YoloV8-V10, Yolo11, Yolo12 and Yolo26 Segmentation Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YoloV8-v10, Yolo11, Yolo12 and Yolo26 segmentation model \
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
                            YOLOV8SEG_OUT_GROUP,
                            gst::UniqueList::new([
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", YOLOV8SEG_TENSORS_INFO.0.name)
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(5, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        ]),
                                    )
                                    .field(
                                        "dims-order",
                                        YOLOV8SEG_TENSORS_INFO.0.dim_order_to_static_str(),
                                    )
                                    .field("type", "float32")
                                    .build(),
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", YOLOV8SEG_TENSORS_INFO.1.name)
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        ]),
                                    )
                                    .field(
                                        "dims-order",
                                        YOLOV8SEG_TENSORS_INFO.1.dim_order_to_static_str(),
                                    )
                                    .field("type", "float32")
                                    .build(),
                            ]),
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

impl BaseTransformImpl for YoloV8SegTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
}

impl super::YoloSegTensorDecImpl for YoloV8SegTensorDec {}

#[derive(Default)]
pub struct Yolo26SegTensorDec;

#[glib::object_subclass]
impl ObjectSubclass for Yolo26SegTensorDec {
    const NAME: &'static str = "GstYolo26SegTensorDec";
    type Type = super::Yolo26SegTensorDec;
    type ParentType = super::YoloSegTensorDec;
}

impl ObjectImpl for Yolo26SegTensorDec {
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
        let imp = obj.upcast_ref::<super::YoloSegTensorDec>().imp();
        match pspec.name() {
            "score-threshold" => {
                let mut settings = imp.settings.lock().unwrap();
                settings.s.score_threshold = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        let imp = obj.upcast_ref::<super::YoloSegTensorDec>().imp();
        match pspec.name() {
            "score-threshold" => {
                let settings = imp.settings.lock().unwrap();
                settings.s.score_threshold.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Yolo26SegTensorDec {}

impl ElementImpl for Yolo26SegTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "YoloV10, Yolo11, Yolo12 and Yolo26 end2end Segmentation Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes tensors from a YoloV10, Yolo11, Yolo12 and Yolo26 Segmentation model \
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
                            YOLO26SEG_OUT_GROUP,
                            gst::UniqueList::new([
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", YOLO26SEG_TENSORS_INFO.0.name)
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(6, i32::MAX).to_send_value(),
                                        ]),
                                    )
                                    .field(
                                        "dims-order",
                                        YOLO26SEG_TENSORS_INFO.0.dim_order_to_static_str(),
                                    )
                                    .field("type", "float32")
                                    .build(),
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", YOLO26SEG_TENSORS_INFO.1.name)
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        ]),
                                    )
                                    .field(
                                        "dims-order",
                                        YOLO26SEG_TENSORS_INFO.1.dim_order_to_static_str(),
                                    )
                                    .field("type", "float32")
                                    .build(),
                            ]),
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

impl BaseTransformImpl for Yolo26SegTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;
}

impl super::YoloSegTensorDecImpl for Yolo26SegTensorDec {}
