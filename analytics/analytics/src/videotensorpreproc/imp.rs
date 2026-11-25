// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*, subclass::prelude::*};
use gst_analytics::{AnalyticsTensorProvenanceMtd, prelude::*};
use gst_base::{prelude::*, subclass::prelude::*};

use std::sync::{LazyLock, Mutex};

use super::converter::{Converter, HwDeviceState, RoiConverter};
use super::model_info;

pub(super) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "videotensorpreproc",
        gst::DebugColorFlags::empty(),
        Some("video tensor preprocessing"),
    )
});

#[derive(Default)]
struct Settings {
    model_info: Option<String>,
    allow_legacy_tensor_meta: bool,
}

struct State {
    model: model_info::Model,
    allow_legacy_tensor_meta: bool,
    conv: Option<Converter>,
    roi_conv: Option<RoiConverter>,
}

#[derive(Default)]
pub struct VideoTensorPreproc {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
    hw_device: HwDeviceState,
}

#[glib::object_subclass]
impl ObjectSubclass for VideoTensorPreproc {
    const NAME: &'static str = "GstVideoTensorPreproc";
    type Type = super::VideoTensorPreproc;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for VideoTensorPreproc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("model-info")
                    .nick("Model Info")
                    .blurb("Path to the .modelinfo file describing the input tensor")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("allow-legacy-tensor-meta")
                    .nick("Allow Legacy Tensor Meta")
                    .blurb("Use legacy tensor metadata when preprocessing the entire frame")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "model-info" => settings.model_info = value.get().expect("type checked upstream"),
            "allow-legacy-tensor-meta" => {
                settings.allow_legacy_tensor_meta = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "model-info" => settings.model_info.to_value(),
            "allow-legacy-tensor-meta" => settings.allow_legacy_tensor_meta.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        self.obj().set_prefer_passthrough(false);
    }
}

impl GstObjectImpl for VideoTensorPreproc {}

impl ElementImpl for VideoTensorPreproc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Video Tensor Preprocessor",
                "Filter/Video",
                "Preprocesses video for tensor inference",
                "Seungha Yang <seungha@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new().build();
            let mut doc_caps = caps.copy();
            for feature in ["memory:HIPMemory", "memory:D3D12Memory"] {
                doc_caps.make_mut().append(
                    gst_video::VideoCapsBuilder::new()
                        .features([feature])
                        .build(),
                );
            }

            #[cfg(hip_enabled)]
            let caps = {
                let mut hip_caps = gst_video::VideoCapsBuilder::new().build();
                hip_caps
                    .make_mut()
                    .set_features_simple(Some(gst::CapsFeatures::new([
                        gst_hip::CAPS_FEATURE_MEMORY_HIP_MEMORY,
                    ])));
                let mut caps = caps;
                caps.make_mut().append(hip_caps);
                caps
            };

            #[cfg(windows)]
            let caps = {
                let mut d3d12_caps = gst_video::VideoCapsBuilder::new().build();
                d3d12_caps
                    .make_mut()
                    .set_features_simple(Some(gst::CapsFeatures::new([
                        gst_d3d12::CAPS_FEATURE_MEMORY_D3D12_MEMORY,
                    ])));
                let mut caps = caps;
                caps.make_mut().append(d3d12_caps);
                caps
            };

            let sink_pad_template = gst::PadTemplate::builder(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .documentation_caps(&doc_caps)
            .build()
            .unwrap();

            let src_pad_template = gst::PadTemplate::builder(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .documentation_caps(&doc_caps)
            .build()
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn set_context(&self, context: &gst::Context) {
        self.hw_device.handle_set_context(&*self.obj(), context);
        self.parent_set_context(context);
    }
}

impl BaseTransformImpl for VideoTensorPreproc {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let (path, allow_legacy_tensor_meta) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.model_info.clone(),
                settings.allow_legacy_tensor_meta,
            )
        };
        let path = path.filter(|path| !path.is_empty()).ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["No model-info file configured"]
            )
        })?;

        let model = model_info::Model::load(&path).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["Failed to load model-info '{}': {}", path, err]
            )
        })?;

        *self.state.lock().unwrap() = Some(State {
            model,
            allow_legacy_tensor_meta,
            conv: None,
            roi_conv: None,
        });

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;

        self.hw_device.clear();

        Ok(())
    }

    fn query(&self, direction: gst::PadDirection, query: &mut gst::QueryRef) -> bool {
        if self.hw_device.handle_context_query(&*self.obj(), query) {
            return true;
        }

        BaseTransformImplExt::parent_query(self, direction, query)
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut caps = caps.copy();

        match direction {
            gst::PadDirection::Src => {
                for structure in caps.make_mut().iter_mut() {
                    structure.remove_field("tensors");
                }
            }
            gst::PadDirection::Sink => {
                if let Some(state) = self.state.lock().unwrap().as_ref() {
                    let group_id = state.model.group_id.as_str();
                    for structure in caps.make_mut().iter_mut() {
                        let mut groups = structure
                            .get::<gst::Structure>("tensors")
                            .unwrap_or_else(|_| gst::Structure::new_empty("tensorgroups"));
                        groups.set_value(
                            group_id,
                            state.model.tensor_groups.value(group_id).unwrap().clone(),
                        );
                        structure.set("tensors", groups);
                    }
                }
            }
            _ => unreachable!(),
        }

        if let Some(filter) = filter {
            caps = filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First);
        }

        Some(caps)
    }

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "Input caps {incaps}");

        let info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|err| gst::loggable_error!(CAT, "Invalid video caps: {err}"))?;

        let device = self
            .hw_device
            .select_device(&*self.obj(), incaps)
            .map_err(|err| gst::loggable_error!(CAT, "{err}"))?;

        let mut state = self.state.lock().unwrap();
        let state = state
            .as_mut()
            .ok_or_else(|| gst::loggable_error!(CAT, "Model info not loaded"))?;

        if state.model.ctc {
            state.roi_conv = Some(
                RoiConverter::new(&info, state.model.config.clone(), device).map_err(|e| {
                    gst::loggable_error!(CAT, "Couldn't prepare ROI preprocessing: {e}")
                })?,
            );
            state.conv = None;

            return Ok(());
        }

        let conv = Converter::new(&info, state.model.config.clone(), device)
            .map_err(|e| gst::loggable_error!(CAT, "Couldn't prepare preprocessing: {e}"))?;

        state.conv = Some(conv);

        Ok(())
    }

    fn before_transform(&self, buffer: &gst::BufferRef) {
        let mut state = self.state.lock().unwrap();
        let Some(state) = state.as_mut() else {
            return;
        };

        let result = if state.model.ctc {
            state
                .roi_conv
                .as_mut()
                .map(|conv| conv.update_device(buffer, &self.hw_device))
        } else {
            state
                .conv
                .as_mut()
                .map(|conv| conv.update_device(buffer, &self.hw_device))
        };

        if let Some(Err(err)) = result {
            gst::element_error!(
                self.obj(),
                gst::StreamError::Failed,
                ["Couldn't update preprocessing device: {err}"]
            );
        }
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let state = state.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        if state.model.ctc {
            let conv = state
                .roi_conv
                .as_mut()
                .ok_or(gst::FlowError::NotNegotiated)?;

            if !conv.is_available() {
                return Err(gst::FlowError::Error);
            }

            let text = glib::Quark::from_static_str(glib::gstr!("text"));
            super::roi_preproc::preprocess(
                buffer,
                conv,
                &state.model,
                text,
                state.allow_legacy_tensor_meta,
            )
            .map_err(|err| {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ["CTC preprocessing failed: {err}"]
                );
                gst::FlowError::Error
            })?;

            return Ok(gst::FlowSuccess::Ok);
        }

        let group_id = state.model.group_id;
        let conv = state.conv.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        if !conv.is_available() {
            return Err(gst::FlowError::Error);
        }

        let tensor = conv.process(buffer).map_err(|err| {
            gst::element_error!(
                self.obj(),
                gst::StreamError::Failed,
                ["Preprocessing failed: {err}"]
            );
            gst::FlowError::Error
        })?;

        let (tensor_rect, video_rect) = conv.tensor_to_video_rectangles();

        let correlation_id = {
            let mut meta = gst_analytics::AnalyticsRelationMeta::add(buffer);
            let provenance = meta
                .add_tensor_provenance_mtd(group_id, &tensor)
                .map_err(|err| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Failed,
                        ["Failed to add tensor provenance: {err}"]
                    );
                    gst::FlowError::Error
                })?;
            let id = provenance.id();
            let correlation_id = provenance.correlation_id();

            let mut provenance = meta.mtd_mut::<AnalyticsTensorProvenanceMtd>(id).unwrap();
            provenance
                .set_spatial_transform_from_rectangles(tensor_rect, video_rect)
                .map_err(|err| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Failed,
                        ["Failed to set tensor spatial transform: {err}"]
                    );
                    gst::FlowError::Error
                })?;
            correlation_id
        };

        super::attach_single_tensor(
            buffer,
            group_id,
            tensor,
            correlation_id,
            state.allow_legacy_tensor_meta,
        )
        .map_err(|err| {
            gst::element_error!(
                self.obj(),
                gst::StreamError::Failed,
                ["Failed to attach preprocessed tensor: {err}"]
            );
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}
