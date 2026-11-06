// Copyright (C) 2026 Collabora Ltd
// Author: Mehdi Hassan <mehdi.hassan@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! # facemeshtensordec
//!
//! A tensor decoder element for the output tensors of the MediaPipe FaceMesh
//! [V1](https://drive.google.com/file/d/1QvwWNfFoweGVjsXF3DXzcrCnz-mx-Lha/preview) and
//! [V2](https://storage.googleapis.com/mediapipe-assets/Model%20Card%20MediaPipe%20Face%20Mesh%20V2.pdf) models.
//!
//! For FaceMesh V1, the two output tensors are:
//! 1. a tensor of 468 3D facial landmarks (Z scaled proportionally to X)
//! 2. a tensor containing a single scalar denoting the logit of the face presence probability ("face flag")
//!
//! For FaceMesh V2, the three output tensors are:
//! 1. a tensor of 478 3D facial landmarks (Z scaled proportionally to X)
//! 2. a tensor containing a single scalar denoting the logit of the face presence probability
//! 3. a tensor containing a single scalar in the interval (0,1) denoting the blendshape coefficient "tongueOut".
//!
//! These models expect that the input image is suitably cropped to the face of the subject. Thus, in practice,
//! inference is usually performed on the output ROI given by a face detection model earlier in the pipeline.
//!
//! ## Properties
//!
//! - `add-tessellation`
//!   Whether to add the skeleton pairs corresponding to "tessellation connections",
//!   which are required to construct a detailed face mesh.
//!   (bool, default: true)
//!
//! - `presence-threshold`
//!   The minimum required face presence probability in order to attach results
//!   from the inference to the buffer
//!   (float, default: 0.5)
//!
//! ## Examples
//!
//! FaceMesh V1 example pipeline:
//!
//! ```console
//! gst-launch-1.0 filesrc location=/path/to/some-face-close-up.jpg ! jpegdec ! videoconvertscale \
//!     ! onnxinference model-file=/path/to/ml-models/models/facemesh_v1.onnx \
//!     ! facemeshtensordec add-tessellation=true presence-threshold=0.7 \
//!     ! keypointoverlay draw-labels=false draw-skeleton=true keypoint-radius=1 skeleton-line-width=1 \
//!         semantic-tag='face-468-kp' \
//!     ! videoconvertscale ! imagefreeze ! autovideosink
//! ```
//!
//! FaceMesh V2 example pipeline:
//!
//! ```console
//! gst-launch-1.0 filesrc location=/path/to/some-face-close-up.jpg ! jpegdec ! videoconvertscale \
//!     ! onnxinference model-file=/path/to/ml-models/models/facemesh_v2.onnx \
//!     ! facemeshtensordec add-tessellation=true presence-threshold=0.7 \
//!     ! keypointoverlay draw-labels=false draw-skeleton=true keypoint-radius=1 skeleton-line-width=1 \
//!         semantic-tag='face-478-kp' \
//!     ! videoconvertscale ! imagefreeze ! autovideosink
//! ```
//!

use super::connections::*;

use std::sync::{LazyLock, Mutex};

use gst::{
    glib::{self, value::ToSendValue},
    subclass::prelude::*,
};
use gst_analytics::{
    AnalyticsKeypointDimensions, AnalyticsRelationMetaGroupExt, AnalyticsRelationMetaTextExt,
    RelTypes, Tensor, TensorMeta,
};
use gst_base::{prelude::*, subclass::prelude::*};

use byte_slice_cast::AsSliceOf;
use itertools::Itertools;

const DEFAULT_PRESENCE_THRESHOLD: f32 = 0.5;
const DEFAULT_ADD_TESSELLATION: bool = true;

// This is the only blendshape output given by FaceMesh V2.
const BLENDSHAPE_NAME: &str = "tongueOut";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "facemeshtensordec",
        gst::DebugColorFlags::empty(),
        Some("Face mesh tensor decoder element"),
    )
});

#[derive(Clone, Copy)]
enum FaceMeshVariant {
    V1,
    V2,
}

trait FaceMeshImpl {
    const ID_OUT_GROUP: &'static str;
    const ID_OUT_LANDMARKS: &'static str;
    const ID_OUT_PRESENCE: &'static str;
    const ID_OUT_BLENDSHAPES: Option<&'static str>;
    const SEMANTIC_TAG: &'static str;
    const LANDMARK_POINTS: usize;
    const LANDMARK_SCALARS: usize = Self::LANDMARK_POINTS * 3;
    const BATCH_SIZE: usize; // 0 represents dynamic batch size
    const MODEL_INPUT_SIZE: (usize, usize); // (width in pixels, height in pixels)

    fn skeleton_pairs(add_tessellation: bool) -> &'static [i32];

    #[inline]
    fn batch_size() -> i32 {
        if Self::BATCH_SIZE == 0 {
            i32::MAX
        } else {
            Self::BATCH_SIZE.clamp(0, i32::MAX as usize) as i32
        }
    }

    fn sink_caps() -> gst::Caps {
        gst_video::VideoCapsBuilder::new()
            .any_features()
            .width(Self::MODEL_INPUT_SIZE.0 as i32)
            .height(Self::MODEL_INPUT_SIZE.1 as i32)
            .field(
                "tensors",
                gst::Structure::builder("tensorgroups")
                    .field(
                        Self::ID_OUT_GROUP,
                        gst::UniqueList::new(Self::tensor_caps()),
                    )
                    .build(),
            )
            .build()
    }

    fn tensor_caps() -> Vec<gst::Caps> {
        let mut tensor_caps = vec![
            gst::Caps::builder("tensor/strided")
                .field("tensor-id", Self::ID_OUT_LANDMARKS)
                .field(
                    "dims",
                    gst::Array::from_values([
                        gst::IntRange::<i32>::new(0, Self::batch_size()).to_send_value(),
                        1_i32.to_send_value(),
                        1_i32.to_send_value(),
                        (Self::LANDMARK_SCALARS as i32).to_send_value(),
                    ]),
                )
                .field("dims-order", "row-major")
                .field("type", "float32")
                .build(),
            gst::Caps::builder("tensor/strided")
                .field("tensor-id", Self::ID_OUT_PRESENCE)
                .field(
                    "dims",
                    gst::Array::from_values([
                        gst::IntRange::<i32>::new(0, Self::batch_size()).to_send_value(),
                        1_i32.to_send_value(),
                        1_i32.to_send_value(),
                        1_i32.to_send_value(),
                    ]),
                )
                .field("dims-order", "row-major")
                .field("type", "float32")
                .build(),
        ];

        if let Some(id_out_blendshapes) = Self::ID_OUT_BLENDSHAPES {
            tensor_caps.push(
                gst::Caps::builder("tensor/strided")
                    .field("tensor-id", id_out_blendshapes)
                    .field(
                        "dims",
                        gst::Array::from_values([
                            gst::IntRange::<i32>::new(0, Self::batch_size()).to_send_value(),
                            1_i32.to_send_value(),
                        ]),
                    )
                    .field("dims-order", "row-major")
                    .field("type", "float32")
                    .build(),
            );
        }

        tensor_caps
    }
}

struct FaceMeshV1;

impl FaceMeshImpl for FaceMeshV1 {
    const ID_OUT_GROUP: &'static str = "facemesh-v1-out";
    const ID_OUT_LANDMARKS: &'static str = "facemesh-v1-out-landmarks";
    const ID_OUT_PRESENCE: &'static str = "facemesh-v1-out-presence";
    const ID_OUT_BLENDSHAPES: Option<&'static str> = None;
    const SEMANTIC_TAG: &'static str = "face-468-kp";
    const LANDMARK_POINTS: usize = 468;
    const BATCH_SIZE: usize = 1;
    const MODEL_INPUT_SIZE: (usize, usize) = (192, 192);

    fn skeleton_pairs(add_tessellation: bool) -> &'static [i32] {
        if add_tessellation {
            *V1_SKELETON_PAIRS_WITH_TESSELLATION
        } else {
            *V1_SKELETON_PAIRS
        }
    }
}

struct FaceMeshV2;

impl FaceMeshImpl for FaceMeshV2 {
    const ID_OUT_GROUP: &'static str = "facemesh-v2-out";
    const ID_OUT_LANDMARKS: &'static str = "facemesh-v2-out-landmarks";
    const ID_OUT_PRESENCE: &'static str = "facemesh-v2-out-presence";
    const ID_OUT_BLENDSHAPES: Option<&'static str> = Some("facemesh-v2-out-blendshapes");
    const SEMANTIC_TAG: &'static str = "face-478-kp";
    const LANDMARK_POINTS: usize = 478;
    // TODO: Set back to 0 (dynamic) once we have a good way of associating each collection
    // of facial landmarks with the respective face detection.
    // const BATCH_SIZE: usize = 0;
    const BATCH_SIZE: usize = 1;
    const MODEL_INPUT_SIZE: (usize, usize) = (256, 256);

    fn skeleton_pairs(add_tessellation: bool) -> &'static [i32] {
        if add_tessellation {
            *V2_SKELETON_PAIRS_WITH_TESSELLATION
        } else {
            *V2_SKELETON_PAIRS
        }
    }
}

#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

struct Settings {
    presence_threshold: f32,
    add_tessellation: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            presence_threshold: DEFAULT_PRESENCE_THRESHOLD,
            add_tessellation: DEFAULT_ADD_TESSELLATION,
        }
    }
}

#[derive(Default)]
struct State {
    variant: Option<FaceMeshVariant>,
}

#[derive(Default)]
pub struct FaceMeshTensorDec {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl FaceMeshTensorDec {
    fn decode_landmarks_tensor<V: FaceMeshImpl>(
        &self,
        landmarks_tensor: &Tensor,
    ) -> Result<Vec<Vec<f32>>, gst::FlowError> {
        let Ok(map) = landmarks_tensor.data().map_readable() else {
            gst::error!(
                CAT,
                imp = self,
                "Could not map landmarks tensor as readable"
            );
            return Err(gst::FlowError::Error);
        };

        let Ok(slice) = map.as_slice_of::<f32>() else {
            gst::error!(
                CAT,
                imp = self,
                "Could not cast landmarks tensor to f32 slice",
            );
            return Err(gst::FlowError::Error);
        };

        let landmarks_batch_iter = slice.chunks_exact(V::LANDMARK_SCALARS);
        if !landmarks_batch_iter.remainder().is_empty() {
            gst::error!(CAT, imp = self, "Batch is not divisible by landmark count");
            return Err(gst::FlowError::Error);
        };

        if landmarks_tensor.dims()[0] != landmarks_batch_iter.len()
            || landmarks_batch_iter.len() != V::batch_size() as usize
        {
            gst::error!(
                CAT,
                imp = self,
                "Unexpected tensor dimensions, landmarks tensor batch sizes do not agree",
            );
            return Err(gst::FlowError::Error);
        }

        Ok(landmarks_batch_iter
            .map(|b| b.iter().copied().collect_vec())
            .collect_vec())
    }

    fn decode_presence_tensor<V: FaceMeshImpl>(
        &self,
        presence_tensor: &Tensor,
    ) -> Result<Vec<f32>, gst::FlowError> {
        let Ok(map) = presence_tensor.data().map_readable() else {
            gst::error!(CAT, imp = self, "Could not map presence tensor as readable");
            return Err(gst::FlowError::Error);
        };

        let Ok(slice) = map.as_slice_of::<f32>() else {
            gst::error!(
                CAT,
                imp = self,
                "Could not cast presence tensor to f32 slice",
            );
            return Err(gst::FlowError::Error);
        };

        if presence_tensor.dims()[0] != slice.len() || slice.len() != V::batch_size() as usize {
            gst::error!(
                CAT,
                imp = self,
                "Unexpected tensor dimensions, presence tensor batch sizes do not agree",
            );
            return Err(gst::FlowError::Error);
        }

        Ok(slice.iter().copied().map(sigmoid).collect_vec())
    }

    fn decode_blendshapes_tensor<V: FaceMeshImpl>(
        &self,
        blendshapes_tensor: &Tensor,
    ) -> Result<Vec<f32>, gst::FlowError> {
        let Ok(map) = blendshapes_tensor.data().map_readable() else {
            gst::error!(
                CAT,
                imp = self,
                "Could not map blendshapes tensor as readable"
            );
            return Err(gst::FlowError::Error);
        };

        let Ok(slice) = map.as_slice_of::<f32>() else {
            gst::error!(
                CAT,
                imp = self,
                "Could not cast blendshapes tensor to f32 slice",
            );
            return Err(gst::FlowError::Error);
        };

        if blendshapes_tensor.dims()[0] != slice.len() || slice.len() != V::batch_size() as usize {
            gst::error!(
                CAT,
                imp = self,
                "Unexpected tensor dimensions, blendshapes tensor batch sizes do not agree",
            );
            return Err(gst::FlowError::Error);
        }

        Ok(slice.iter().copied().collect_vec())
    }

    fn handle_buffer<V: FaceMeshImpl>(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(tensor_meta) = buffer
            .iter_meta::<TensorMeta>()
            .find(|meta| meta.index_from_id(glib::Quark::from_str(V::ID_OUT_LANDMARKS)) != -1)
        else {
            gst::debug!(CAT, imp = self, "No tensor meta found");
            return Ok(gst::FlowSuccess::Ok);
        };

        let Some(presence_tensor) = tensor_meta.typed_tensor(
            glib::Quark::from_str(V::ID_OUT_PRESENCE),
            gst_analytics::TensorDataType::Float32,
            gst_analytics::TensorDimOrder::RowMajor,
            &[V::batch_size() as usize, 1, 1, 1],
        ) else {
            gst::debug!(CAT, imp = self, "No presence tensor found");
            return Ok(gst::FlowSuccess::Ok);
        };

        let Some(landmarks_tensor) = tensor_meta.typed_tensor(
            glib::Quark::from_str(V::ID_OUT_LANDMARKS),
            gst_analytics::TensorDataType::Float32,
            gst_analytics::TensorDimOrder::RowMajor,
            &[V::batch_size() as usize, 1, 1, V::LANDMARK_SCALARS],
        ) else {
            gst::debug!(CAT, imp = self, "No landmarks tensor found");
            return Ok(gst::FlowSuccess::Ok);
        };

        let blendshapes_tensor = V::ID_OUT_BLENDSHAPES.and_then(|id| {
            tensor_meta.typed_tensor(
                glib::Quark::from_str(id),
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[V::batch_size() as usize, 1],
            )
        });

        if V::ID_OUT_BLENDSHAPES.is_some() && blendshapes_tensor.is_none() {
            gst::debug!(CAT, imp = self, "No blendshapes tensor found");
            return Ok(gst::FlowSuccess::Ok);
        }

        let presence_batch = self.decode_presence_tensor::<V>(presence_tensor)?;
        let landmarks_batch = self.decode_landmarks_tensor::<V>(landmarks_tensor)?;

        let blendshapes_tensor = match blendshapes_tensor {
            Some(blendshapes_tensor) => {
                let res = self.decode_blendshapes_tensor::<V>(blendshapes_tensor)?;
                Some(res)
            }
            None => None,
        };

        let Settings {
            presence_threshold,
            add_tessellation,
        } = *self.settings.lock().unwrap();

        let skeleton_pairs = V::skeleton_pairs(add_tessellation);

        let mut rel_meta = gst_analytics::AnalyticsRelationMeta::add(buffer);

        for (i, (presence_prob, landmarks)) in
            presence_batch.into_iter().zip(landmarks_batch).enumerate()
        {
            if presence_prob < presence_threshold {
                continue;
            }

            let landmarks = landmarks
                .into_iter()
                .map(|x| x.round() as i32)
                .collect_vec();

            // TODO: According to analytics meta conventions not sure if I should put this under a ODMtd or keep it full frame.
            // Generally speaking this model is meant to be run on the ROI given by a separate face detection model.
            // Also if the batch size > 1, how do we know which ODMtd corresponds to which set of landmarks?

            let group_id = rel_meta
                .add_keypoints_group(
                    V::SEMANTIC_TAG,
                    AnalyticsKeypointDimensions::_3d,
                    &landmarks,
                    Some(&[presence_prob].repeat(V::LANDMARK_POINTS)),
                    None,
                    skeleton_pairs,
                )
                .unwrap()
                .id();

            if let Some(ref blendshapes_tensor) = blendshapes_tensor {
                let text_id = rel_meta
                    .add_text_mtd(BLENDSHAPE_NAME, blendshapes_tensor[i])
                    .unwrap()
                    .id();

                rel_meta
                    .set_relation(RelTypes::RELATE_TO, text_id, group_id)
                    .unwrap();
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FaceMeshTensorDec {
    const NAME: &'static str = "GstFaceMeshTensorDec";
    type Type = super::FaceMeshTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for FaceMeshTensorDec {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj()
            .sink_pad()
            .unset_pad_flags(gst::PadFlags::ACCEPT_INTERSECT);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("presence-threshold")
                    .nick("Face Presence Threshold")
                    .blurb("The minimum required face presence probability in order to add facial landmarks")
                    .default_value(DEFAULT_PRESENCE_THRESHOLD)
                    .minimum(0.0)
                    .maximum(1.0)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("add-tessellation")
                    .nick("Add Tessellation")
                    .blurb("Add the skeleton pairs required to construct a detailed face mesh")
                    .default_value(DEFAULT_ADD_TESSELLATION)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "presence-threshold" => settings.presence_threshold = value.get().unwrap(),
            "add-tessellation" => settings.add_tessellation = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }
    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "presence-threshold" => settings.presence_threshold.to_value(),
            "add-tessellation" => settings.add_tessellation.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for FaceMeshTensorDec {}

impl ElementImpl for FaceMeshTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Face Mesh Tensor Decoder",
                "Tensordecoder/Video",
                "Decodes tensors from the MediaPipe FaceMesh V1 and V2 models",
                "Mehdi Hassan <mehdi.hassan@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst_video::VideoCapsBuilder::new().any_features().build(),
            )
            .unwrap();

            let mut sink_caps = gst::Caps::new_empty();
            sink_caps.get_mut().unwrap().append(FaceMeshV1::sink_caps());
            sink_caps.get_mut().unwrap().append(FaceMeshV2::sink_caps());

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for FaceMeshTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let tensors = incaps
            .structure(0)
            .and_then(|s| s.get::<gst::Structure>("tensors").ok())
            .ok_or_else(|| gst::loggable_error!(CAT, "Could not retrieve tensors from caps"))?;

        let variant = if tensors.has_field(FaceMeshV1::ID_OUT_GROUP) {
            FaceMeshVariant::V1
        } else if tensors.has_field(FaceMeshV2::ID_OUT_GROUP) {
            FaceMeshVariant::V2
        } else {
            return Err(gst::loggable_error!(
                CAT,
                "Could not find any supported tensor group in caps"
            ));
        };

        let mut state = self.state.lock().unwrap();
        state.variant = Some(variant);

        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let variant = self.state.lock().unwrap().variant;

        match variant {
            Some(FaceMeshVariant::V1) => self.handle_buffer::<FaceMeshV1>(buffer),
            Some(FaceMeshVariant::V2) => self.handle_buffer::<FaceMeshV2>(buffer),
            None => {
                gst::error!(CAT, imp = self, "FaceMesh variant not yet determined");
                Err(gst::FlowError::NotNegotiated)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_pairs_len_is_even() {
        // Since the slice of skeleton *pairs* is flat, it should obviously be even
        assert!(V1_SKELETON_PAIRS.len().is_multiple_of(2));
        assert!(V1_SKELETON_PAIRS_WITH_TESSELLATION.len().is_multiple_of(2));

        assert!(V2_SKELETON_PAIRS.len().is_multiple_of(2));
        assert!(V2_SKELETON_PAIRS_WITH_TESSELLATION.len().is_multiple_of(2));
    }

    #[test]
    fn skeleton_pair_has_inverse_and_correct_multiplicity() {
        // Since atm we need to set RELATE_TO in both directions, for each skeleton pair (a, b) we should have (b, a)
        // Also, when adding the keypoint group, we don't want to pass unnecessary extra skeleton pairs, only one
        // occurence of a pair and its inverse is required
        // However, some tessellation connections are also contour connections, so we only check that the multiplicity
        // is either 2 or 4 if the skeleton pairs include tessellation

        fn check(contains_tessellation: bool, skeleton_pairs: &[i32]) {
            use std::collections::HashMap;

            let mut equiv_class_upto_inverse: HashMap<[i32; 2], Vec<[i32; 2]>> = HashMap::new();
            let (pairs, []) = skeleton_pairs.as_chunks::<2>() else {
                panic!("Skeleton pairs not of even length");
            };

            for [a, b] in pairs.iter().copied() {
                let representative = [i32::min(a, b), i32::max(a, b)];
                let equiv_pairs = equiv_class_upto_inverse.get_mut(&representative);

                if let Some(equiv_pairs) = equiv_pairs {
                    equiv_pairs.push([a, b]);
                } else {
                    equiv_class_upto_inverse.insert(representative, vec![[a, b]]);
                }
            }

            assert!(equiv_class_upto_inverse.iter().all(|(_, pairs)| {
                if !contains_tessellation {
                    pairs.len() == 2
                } else {
                    pairs.len() == 2 || pairs.len() == 4
                }
            }));

            assert!(
                equiv_class_upto_inverse
                    .iter()
                    .all(|([a, b], pairs)| { pairs.contains(&[*a, *b]) })
            );

            assert!(
                equiv_class_upto_inverse
                    .iter()
                    .all(|([a, b], pairs)| { pairs.contains(&[*b, *a]) })
            );
        }

        check(false, *V1_SKELETON_PAIRS);
        check(true, *V1_SKELETON_PAIRS_WITH_TESSELLATION);

        check(false, *V2_SKELETON_PAIRS);
        check(true, *V2_SKELETON_PAIRS_WITH_TESSELLATION);
    }
}
