// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-burn-yoloxinference
 * @see_also: objectdetectionoverlay, yoloxtensordec.
 *
 * [burn](https://burn.dev)-based inference element that performs
 * [YOLOX](https://github.com/Megvii-BaseDetection/YOLOX) object detection.
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
use gst_video::{prelude::*, subclass::prelude::*};

use byte_slice_cast::*;

use std::{
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use super::yolox_burn::model::yolox;
use super::ModelType;
use crate::BackendType;

use burn::{
    backend,
    module::{ConstantRecord, Module},
    record::{FullPrecisionSettings, Recorder, RecorderError},
    tensor::{backend::Backend, Device, Tensor, TensorData},
};

const YOLOX_OUT: &glib::GStr = glib::gstr!("yolox-out");

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "burn-yoloxinference",
        gst::DebugColorFlags::empty(),
        Some("Burn YOLOX Inference Element"),
    )
});

struct VecWrapper<T>(Vec<T>);

impl<T: ToByteSlice> AsRef<[u8]> for VecWrapper<T> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_byte_slice()
    }
}
impl<T: ToMutByteSlice> AsMut<[u8]> for VecWrapper<T> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.0.as_mut_byte_slice()
    }
}

struct State {
    model: Model,
    info: Option<gst_video::VideoInfo>,
}

struct Settings {
    backend_type: BackendType,
    model_type: ModelType,
    num_classes: u32,
    weights_path: Option<PathBuf>,
    cubecl_type_id: u32,
    cubecl_index_id: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            backend_type: Default::default(),
            model_type: Default::default(),
            num_classes: Default::default(),
            weights_path: Default::default(),
            cubecl_type_id: u32::MAX,
            cubecl_index_id: u32::MAX,
        }
    }
}

#[derive(Default)]
pub struct YoloxInference {
    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for YoloxInference {
    const NAME: &'static str = "GstBurnYoloxInference";
    type Type = super::YoloxInference;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for YoloxInference {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder("backend-type")
                    .nick("Backend Type")
                    .blurb("Burn backend to use")
                    .default_value(Settings::default().backend_type)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder("model-type")
                    .nick("Model Type")
                    .blurb("YOLOX model type to use")
                    .default_value(Settings::default().model_type)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("num-classes")
                    .nick("Number of classes")
                    .blurb("Number of output classes of the model. This must match the weights. Keep at 0 for pretrained models.")
                    .default_value(Settings::default().num_classes)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("weights-path")
                    .nick("Weights Path")
                    .blurb("Path to a PyTorch weights file for the model. This must match the model type and number of weights. Keep empty for pretrained models.")
                    .default_value(None)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("cubecl-type-id")
                    .nick("CubeCL Type ID")
                    .blurb("Type ID that identifies the type of the device. For CubeCL-based backends only, -1 for default.")
                    .default_value(Settings::default().cubecl_type_id)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("cubecl-index-id")
                    .nick("CubeCL Index ID")
                    .blurb("Index ID that identifies the device number. For CubeCL-based backends only.")
                    .default_value(Settings::default().cubecl_index_id)
                    .mutable_ready()
                    .build(),

            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "backend-type" => {
                let mut settings = self.settings.lock().unwrap();
                settings.backend_type = value.get().unwrap();
            }
            "model-type" => {
                let mut settings = self.settings.lock().unwrap();
                settings.model_type = value.get().unwrap();
            }
            "num-classes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.num_classes = value.get().unwrap();
            }
            "weights-path" => {
                let mut settings = self.settings.lock().unwrap();
                settings.weights_path = value.get().unwrap();
            }
            "cubecl-type-id" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cubecl_type_id = value.get().unwrap();
            }
            "cubecl-index-id" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cubecl_index_id = value.get().unwrap();
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "backend-type" => {
                let settings = self.settings.lock().unwrap();
                settings.backend_type.to_value()
            }
            "model-type" => {
                let settings = self.settings.lock().unwrap();
                settings.model_type.to_value()
            }
            "num-classes" => {
                let settings = self.settings.lock().unwrap();
                settings.num_classes.to_value()
            }
            "weights-path" => {
                let settings = self.settings.lock().unwrap();
                settings.weights_path.to_value()
            }
            "cubecl-type-id" => {
                let settings = self.settings.lock().unwrap();
                settings.cubecl_type_id.to_value()
            }
            "cubecl-index-id" => {
                let settings = self.settings.lock().unwrap();
                settings.cubecl_index_id.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for YoloxInference {}

impl ElementImpl for YoloxInference {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Burn YOLOX Inference Element",
                "Inference/Classification/Video",
                "Runs inference on video frames via a YOLOX model",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // FIXME: Unclear which resolutions are actually supported but all these are working
            let widths_heights = gst::List::new([480i32, 640, 800, 1024]);

            let sink_caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgb)
                .field("width", &widths_heights)
                .field("height", &widths_heights)
                .pixel_aspect_ratio(gst::Fraction::new(1, 1))
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgb)
                .pixel_aspect_ratio(gst::Fraction::new(1, 1))
                .field("width", &widths_heights)
                .field("height", &widths_heights)
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

impl BaseTransformImpl for YoloxInference {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        let settings = self.settings.lock().unwrap();

        let model = match Model::load_model(&settings) {
            Ok(model) => model,
            Err(err) => {
                gst::error!(CAT, imp = self, "Failed to load model: {err}");
                return Err(gst::error_msg!(
                    gst::LibraryError::Settings,
                    ["Failed to load model: {err}"]
                ));
            }
        };
        *state = Some(State { model, info: None });

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;
        gst::info!(CAT, imp = self, "Stopped");
        Ok(())
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "Received caps {incaps:?}");

        let mut state_guard = self.state.lock().unwrap();
        let Some(state) = &mut *state_guard else {
            return Err(gst::loggable_error!(CAT, "Invalid state"));
        };

        let Ok(info) = gst_video::VideoInfo::from_caps(incaps) else {
            return Err(gst::loggable_error!(CAT, "Invalid caps {incaps:?}"));
        };
        state.info = Some(info);

        drop(state_guard);

        self.parent_set_caps(incaps, outcaps)
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let res = if direction == gst::PadDirection::Src {
            let mut res = caps.copy();
            for s in res.get_mut().unwrap().iter_mut() {
                if let Ok(mut tensors) = s.get::<gst::Structure>("tensors") {
                    tensors.remove_field(YOLOX_OUT);
                    s.set("tensors", tensors);
                }
            }

            res
        } else {
            let state = self.state.lock().unwrap();

            let mut res = caps.copy();
            for s in res.get_mut().unwrap().iter_mut() {
                let mut tensors = s
                    .get::<gst::Structure>("tensors")
                    .ok()
                    .unwrap_or_else(|| gst::Structure::new_empty("tensorgroups"));

                tensors.set(
                    YOLOX_OUT,
                    gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                        .field(
                            "dims",
                            gst::Array::from_values([
                                1i32.to_send_value(),
                                0i32.to_send_value(),
                                if let Some(state) = &*state {
                                    (5 + state.model.num_classes() as i32).to_send_value()
                                } else {
                                    gst::IntRange::<i32>::new(5, i32::MAX).to_send_value()
                                },
                            ]),
                        )
                        .field("dims-order", "row-major")
                        .field("type", "float32")
                        .build()]),
                );

                s.set("tensors", tensors);
            }

            res
        };

        let res = filter
            .map(|filter| filter.intersect_with_mode(&res, gst::CapsIntersectMode::First))
            .unwrap_or(res);

        Some(res)
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

        let Some(ref info) = state.info else {
            gst::error!(CAT, imp = self, "No caps");
            return Err(gst::FlowError::NotNegotiated);
        };

        let Ok(frame) = gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, info) else {
            gst::error!(CAT, imp = self, "Failed to map video frame");
            return Err(gst::FlowError::Error);
        };

        let width = frame.width() as usize;
        let height = frame.height() as usize;
        let stride = frame.plane_stride()[0] as usize;

        // Copy to a Vec
        let mut input = vec![0u8; width * height * 3];
        let in_data = frame.plane_data(0).unwrap();
        for (out_line, in_line) in Iterator::zip(
            input.chunks_exact_mut(width * 3),
            in_data.chunks_exact(stride),
        ) {
            out_line.copy_from_slice(&in_line[..width * 3]);
        }
        drop(frame);

        let (output, num_boxes, num_columns) = state.model.forward(input, width, height);
        gst::log!(
            CAT,
            imp = self,
            "Inferred {num_boxes} detections with {} classes",
            num_columns - 4 - 1
        );

        let tensor_data = gst::Buffer::from_slice(VecWrapper(output));

        let tensor = gst_analytics::Tensor::new_simple(
            glib::Quark::from_static_str(YOLOX_OUT),
            gst_analytics::TensorDataType::Float32,
            tensor_data,
            gst_analytics::TensorDimOrder::RowMajor,
            &[1, num_boxes, num_columns],
        );

        let mut meta = gst_analytics::TensorMeta::add(buffer);
        meta.set(glib::Slice::from([tensor]));

        Ok(gst::FlowSuccess::Ok)
    }
}

struct Model {
    model: Box<dyn DynModel>,
    num_classes: u32,
}

trait DynModel: Send + 'static {
    fn forward(&self, input: Vec<u8>, width: usize, height: usize) -> (Vec<f32>, usize, usize);
}

impl Model {
    fn num_classes(&self) -> u32 {
        self.num_classes
    }

    fn forward(&self, input: Vec<u8>, width: usize, height: usize) -> (Vec<f32>, usize, usize) {
        self.model.forward(input, width, height)
    }

    fn load_model(settings: &Settings) -> Result<Self, anyhow::Error> {
        let (model, num_classes) = match settings.backend_type {
            BackendType::NdArray => {
                let device = Device::<backend::NdArray>::default();
                Self::load_model_internal::<backend::NdArray>(device, settings)?
            }
            #[cfg(feature = "vulkan")]
            BackendType::Vulkan => {
                let device = match (settings.cubecl_type_id, settings.cubecl_index_id) {
                    (u32::MAX, _) => Device::<backend::Vulkan>::default(),
                    (type_id, index_id) => {
                        use burn::tensor::backend::{Device as _, DeviceId};

                        Device::<backend::Vulkan>::from_id(DeviceId {
                            type_id: type_id as u16,
                            index_id,
                        })
                    }
                };
                Self::load_model_internal::<backend::Vulkan>(device, settings)?
            }
        };

        Ok(Model { model, num_classes })
    }

    fn forward_internal<B: Backend>(
        device: &Device<B>,
        model: &yolox::Yolox<B>,
        input: Vec<u8>,
        width: usize,
        height: usize,
    ) -> (Vec<f32>, usize, usize) {
        // Create tensor from image data
        let img_tensor = Tensor::<B, 3>::from_data(
            TensorData::new(input, [height, width, 3]).convert::<<B as Backend>::FloatElem>(),
            device,
        )
        // [H, W, C] -> [C, H, W]
        .permute([2, 0, 1])
        .unsqueeze::<4>(); // [B, C, H, W]

        // Forward pass
        let out = model.forward(img_tensor);

        // Post-processing
        let [num_batches, num_boxes, num_columns] = out.dims();
        assert_eq!(num_batches, 1); // we only passed a single batch in
        assert!(num_columns > 4 + 1); // Bounding box (4) and box confidence (1)

        let output = out.into_data().into_vec::<f32>().unwrap();

        (output, num_boxes, num_columns)
    }

    fn load_model_internal<B: Backend>(
        device: Device<B>,
        settings: &Settings,
    ) -> Result<(Box<dyn DynModel>, u32), anyhow::Error> {
        use anyhow::Context;

        struct M<B: Backend> {
            model: yolox::Yolox<B>,
            device: Device<B>,
        }

        impl<B: Backend> DynModel for M<B> {
            fn forward(
                &self,
                input: Vec<u8>,
                width: usize,
                height: usize,
            ) -> (Vec<f32>, usize, usize) {
                Model::forward_internal(&self.device, &self.model, input, width, height)
            }
        }

        let (model, num_classes) = match settings.weights_path {
            None => {
                #[cfg(feature = "pretrained")]
                {
                    use super::yolox_burn::model::weights;

                    let model = match settings.model_type {
                        ModelType::Nano => {
                            yolox::Yolox::yolox_nano_pretrained(weights::YoloxNano::Coco, &device)
                        }
                        ModelType::Tiny => {
                            yolox::Yolox::yolox_tiny_pretrained(weights::YoloxTiny::Coco, &device)
                        }
                        ModelType::Small => {
                            yolox::Yolox::yolox_s_pretrained(weights::YoloxS::Coco, &device)
                        }
                        ModelType::Medium => {
                            yolox::Yolox::yolox_m_pretrained(weights::YoloxM::Coco, &device)
                        }
                        ModelType::Large => {
                            yolox::Yolox::yolox_l_pretrained(weights::YoloxL::Coco, &device)
                        }
                        ModelType::ExtraLarge => {
                            yolox::Yolox::yolox_x_pretrained(weights::YoloxX::Coco, &device)
                        }
                    }
                    .context("Failed loading pre-trained weights")?;
                    (model, 80)
                }
                #[cfg(not(feature = "pretrained"))]
                {
                    anyhow::bail!("Compiled without support for pretrained models");
                }
            }
            Some(ref path) => {
                fn load_weights<B: Backend>(
                    path: &Path,
                    device: &Device<B>,
                ) -> Result<yolox::YoloxRecord<B>, RecorderError> {
                    use burn_import::pytorch::{LoadArgs, PyTorchFileRecorder};

                    // Load weights from torch state_dict
                    let load_args = LoadArgs::new(path.into())
                        // State dict contains "model", "amp", "optimizer", "start_epoch"
                        .with_top_level_key("model")
                        // Map backbone.C3_* -> backbone.c3_*
                        .with_key_remap("backbone\\.C3_(.+)", "backbone.c3_$1")
                        // Map backbone.backbone.dark[i].0.* -> backbone.backbone.dark[i].conv.*
                        .with_key_remap(
                            "(backbone\\.backbone\\.dark[2-5])\\.0\\.(.+)",
                            "$1.conv.$2",
                        )
                        // Map backbone.backbone.dark[i].1.* -> backbone.backbone.dark[i].c3.*
                        .with_key_remap("(backbone\\.backbone\\.dark[2-4])\\.1\\.(.+)", "$1.c3.$2")
                        // Map backbone.backbone.dark5.1.* -> backbone.backbone.dark5.spp.*
                        .with_key_remap("(backbone\\.backbone\\.dark5)\\.1\\.(.+)", "$1.spp.$2")
                        // Map backbone.backbone.dark5.2.* -> backbone.backbone.dark5.c3.*
                        .with_key_remap("(backbone\\.backbone\\.dark5)\\.2\\.(.+)", "$1.c3.$2")
                        // Map head.{cls | reg}_convs.x.[i].* -> head.{cls | reg}_convs.x.conv[i].*
                        .with_key_remap(
                            "(head\\.(cls|reg)_convs\\.[0-9]+)\\.([0-9]+)\\.(.+)",
                            "$1.conv$3.$4",
                        );

                    let mut record: yolox::YoloxRecord<B> =
                        PyTorchFileRecorder::<FullPrecisionSettings>::new()
                            .load(load_args, device)?;

                    if let Some(ref mut spp) = record.backbone.backbone.dark5.spp {
                        // Handle the initialization for Vec<MaxPool2d>, which has no parameters.
                        // Without this, the vector would be initialized as empty and thus no MaxPool2d
                        // layers would be applied, which is incorrect.
                        if spp.m.is_empty() {
                            spp.m = vec![ConstantRecord; crate::yoloxinference::yolox_burn::model::bottleneck::SPP_POOLING.len()];
                        }
                    }

                    Ok(record)
                }

                let record = load_weights::<B>(path, &device).context("Failed to load weights")?;

                let model = match settings.model_type {
                    ModelType::Nano => {
                        yolox::Yolox::yolox_nano(settings.num_classes as usize, &device)
                            .load_record(record)
                    }
                    ModelType::Tiny => {
                        yolox::Yolox::yolox_tiny(settings.num_classes as usize, &device)
                            .load_record(record)
                    }
                    ModelType::Small => {
                        yolox::Yolox::yolox_s(settings.num_classes as usize, &device)
                            .load_record(record)
                    }
                    ModelType::Medium => {
                        yolox::Yolox::yolox_m(settings.num_classes as usize, &device)
                            .load_record(record)
                    }
                    ModelType::Large => {
                        yolox::Yolox::yolox_l(settings.num_classes as usize, &device)
                            .load_record(record)
                    }
                    ModelType::ExtraLarge => {
                        yolox::Yolox::yolox_x(settings.num_classes as usize, &device)
                            .load_record(record)
                    }
                };

                (model, settings.num_classes)
            }
        };

        Ok((Box::new(M { model, device }), num_classes))
    }
}
