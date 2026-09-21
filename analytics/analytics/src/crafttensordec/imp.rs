// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use byte_slice_cast::*;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_analytics::prelude::*;
use gst_base::{prelude::*, subclass::prelude::*};
use std::sync::{LazyLock, Mutex};

use super::{
    grouping,
    postprocess::{Postprocessor, Thresholds},
};

const TENSOR_ID: &glib::GStr = glib::gstr!("craft-out");
const OD_META_LABEL: &glib::GStr = glib::gstr!("text");
const CRAFT_HEATMAP_SCALE: f64 = 2.0;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "crafttensordec",
        gst::DebugColorFlags::empty(),
        Some("CRAFT tensor decoder"),
    )
});

#[derive(Clone, Copy, Default)]
struct Settings {
    thresholds: Thresholds,
    group_text_box: bool,
}

#[derive(Debug, Default)]
struct State {
    postproc: Postprocessor,
}

#[derive(Default)]
pub struct CraftTensorDec {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for CraftTensorDec {
    const NAME: &'static str = "GstCraftTensorDec";
    type Type = super::CraftTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for CraftTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let defaults = Thresholds::default();
            vec![
                glib::ParamSpecFloat::builder("text-threshold")
                    .nick("Text Threshold")
                    .blurb("Minimum component peak text score")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(defaults.text)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("link-threshold")
                    .nick("Link Threshold")
                    .blurb("Threshold for link pixels")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(defaults.link)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("low-text")
                    .nick("Low Text")
                    .blurb("Threshold for text candidate pixels")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(defaults.low_text)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("group-text-box")
                    .nick("Group Text Box")
                    .blurb(
                        "Merge nearby horizontal text boxes of similar height on the same line using EasyOCR's grouping rules",
                    )
                    .default_value(false)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "text-threshold" => {
                settings.thresholds.text = value.get().expect("type checked upstream")
            }
            "link-threshold" => {
                settings.thresholds.link = value.get().expect("type checked upstream")
            }
            "low-text" => {
                settings.thresholds.low_text = value.get().expect("type checked upstream")
            }
            "group-text-box" => {
                settings.group_text_box = value.get().expect("type checked upstream")
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "text-threshold" => settings.thresholds.text.to_value(),
            "link-threshold" => settings.thresholds.link.to_value(),
            "low-text" => settings.thresholds.low_text.to_value(),
            "group-text-box" => settings.group_text_box.to_value(),
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

impl GstObjectImpl for CraftTensorDec {}

impl ElementImpl for CraftTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "CRAFT Tensor Decoder",
                "Tensordecoder/Video",
                "Decodes CRAFT text detection score maps",
                "Seungha Yang <seungha@centricular.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .field(
                    "tensors",
                    gst::Structure::builder("tensorgroups")
                        .field(
                            TENSOR_ID,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", TENSOR_ID)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        1i32.to_send_value(),
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        2i32.to_send_value(),
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

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst_video::VideoCapsBuilder::new().any_features().build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for CraftTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = Some(State::default());

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
        let mut state_guard = self.state.lock().unwrap();
        let Some(state) = state_guard.as_mut() else {
            return Err(gst::FlowError::Flushing);
        };

        let Some(tensor) = find_craft_tensor(buffer) else {
            gst::trace!(CAT, imp = self, "No CRAFT tensor meta found");
            return Ok(gst::FlowSuccess::Ok);
        };

        let map = tensor
            .data()
            .map_readable()
            .map_err(|_| gst::FlowError::Error)?;

        let data = map
            .as_slice_of::<f32>()
            .map_err(|_| gst::FlowError::Error)?;

        let dims = tensor.dims();

        let (thresholds, group_text_box) = {
            let settings = self.settings.lock().unwrap();
            (settings.thresholds, settings.group_text_box)
        };

        let all_detections = state
            .postproc
            .detect(data, dims[2], dims[1], thresholds)
            .map_err(|err| {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Format,
                    ["CRAFT decoding failed: {err}"]
                );
                gst::FlowError::Error
            })?;

        gst::debug!(
            CAT,
            imp = self,
            "Decoded {} CRAFT boxes from {}x{} heatmap",
            all_detections.len(),
            dims[2],
            dims[1]
        );

        for detection in &all_detections {
            gst::debug!(
                CAT,
                imp = self,
                "Heatmap box {:?}, peak text score {}",
                detection,
                detection.score
            );
        }
        drop(map);

        if !all_detections.is_empty() {
            let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);
            let label = glib::Quark::from_static_str(OD_META_LABEL);
            let tensor_id = glib::Quark::from_static_str(TENSOR_ID);

            let transform = |point: geo::Point<f64>| {
                let (x, y) = transform_point(
                    rmeta.as_ref(),
                    tensor_id,
                    point.x() * CRAFT_HEATMAP_SCALE,
                    point.y() * CRAFT_HEATMAP_SCALE,
                );
                geo::Point::new(x as f64, y as f64)
            };

            let detections = if group_text_box {
                grouping::group_text_box(all_detections, transform)
            } else {
                all_detections
                    .into_iter()
                    .filter_map(|detection| detection.transform(transform))
                    .collect()
            };

            for detection in detections {
                let x = detection.x.floor() as i32;
                let y = detection.y.floor() as i32;
                let width = ((detection.x + detection.width).ceil() - detection.x.floor()) as i32;
                let height = ((detection.y + detection.height).ceil() - detection.y.floor()) as i32;
                gst::debug!(CAT, imp = self, "Adding OD detection {detection:?}");

                let result = if detection.rotation == 0.0 {
                    rmeta.add_od_mtd(label, x, y, width, height, detection.score)
                } else {
                    rmeta.add_oriented_od_mtd(
                        label,
                        x,
                        y,
                        width,
                        height,
                        detection.rotation as f32,
                        detection.score,
                    )
                };
                result.map_err(|err| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Failed,
                        ["Failed to add CRAFT object detection metadata: {err}"]
                    );
                    gst::FlowError::Error
                })?;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

fn transform_point(
    meta: &gst::MetaRef<'_, gst_analytics::AnalyticsRelationMeta>,
    group_id: glib::Quark,
    x: f64,
    y: f64,
) -> (f32, f32) {
    meta.iter::<gst_analytics::AnalyticsTensorProvenanceMtd>()
        .find(|mtd| mtd.group_id() == Some(group_id))
        .and_then(|mtd| mtd.transform_point(x as f32, y as f32))
        .unwrap_or((x as f32, y as f32))
}

fn find_craft_tensor(buffer: &gst::BufferRef) -> Option<gst_analytics::Tensor> {
    buffer
        .iter_meta::<gst_analytics::TensorMeta>()
        .find_map(|meta| {
            meta.typed_tensor(
                glib::Quark::from_static_str(TENSOR_ID),
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[1usize, usize::MAX, usize::MAX, 2usize],
            )
            .cloned()
        })
}
