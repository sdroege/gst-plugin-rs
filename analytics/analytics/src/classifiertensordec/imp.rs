// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::helpers::load_labels;
/**
 * SECTION:element-classifiertensordec
 * @see_also: classifiertensordec
 *
 * Tensor decoder element for image classification models using a common output format: a
 * vector of per-class confidence levels.
 *
 * The tensor id selects how the values are interpreted: `classification-generic-out` holds
 * raw scores that are softmaxed here, `classification-generic-softmaxed-out` holds
 * probabilities that are used as they are. uint8 values are scaled to `[0, 1]` first.
 *
 * The most confident class is attached to the buffer as classification metadata. Without
 * `labels-file` the classes are labelled by their index.
 *
 * Test image file, model file and labels file can be found here : https://gitlab.collabora.com/gstreamer/onnx-models
 *
 * |[
 * gst-launch-1.0 filesrc location=onnx-models/images/bus.jpg \
 *  ! jpegdec ! videoconvertscale ! video/x-raw,width=224,height=224 \
 *  ! onnxinference execution-provider=cpu model-file=onnx-models/models/mobilenetv2-10.onnx \
 *  ! classifiertensordec labels-file=onnx-models/labels/synset.txt \
 *  ! fakesink
 * ]| This takes a JPEG, classifies it via `onnxinference` and decodes the inferred tensor with
 * `classifiertensordec`.
 *
 * Tensor format must be:
 *  - Dims: [`class_count`] or [`batch-size`, `class_count`]
 *  - Datatype: float32 or uint8
 *
 * |[
 *  Tensor [M,N]
 *  |Batch 0   | Class 0 confidence level | ... | Class N-1 confidence level |
 *  |               ...                   |
 *  |Batch M-1 | Class 0 confidence level | ... | Class N-1 confidence level |
 *
 *  In-memory tensor format:
 *
 *  |Batch 0, Class 0 confidence level     |
 *  |Batch 0,           ...                |
 *  |Batch 0, Class N-1 confidence level   |
 *  |               ...                    |
 *  |Batch M-1, Class 0 confidence level   |
 *  |Batch M-1,           ...              |
 *  |Batch M-1, Class N-1 confidence level |
 * ]|
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::subclass::prelude::*;
use gst_analytics::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::prelude::*;

use std::sync::{LazyLock, Mutex};

const CLASSIFICATION_ID: &str = "classification-generic-out";
const CLASSIFICATION_SOFTMAXED_ID: &str = "classification-generic-softmaxed-out";

const DEFAULT_THRESHOLD: f32 = 0.0;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "classifiertensordec",
        gst::DebugColorFlags::empty(),
        Some("Tensor decoder for classification model with common output format"),
    )
});

#[derive(Clone)]
struct Settings {
    threshold: f32,
    labels_file: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            labels_file: None,
        }
    }
}

struct State {
    /// One label per class, either loaded from `labels-file` or generated from the class
    /// indices once the class count is known from the caps.
    labels: Option<Vec<glib::Quark>>,
    do_softmax: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            labels: None,
            do_softmax: true,
        }
    }
}

#[derive(Default)]
pub struct ClassifierTensorDec {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for ClassifierTensorDec {
    const NAME: &'static str = "GstRsClassifierTensorDec";
    type Type = super::ClassifierTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for ClassifierTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("class-confidence-threshold")
                    .nick("Class confidence threshold")
                    .blurb("Classes with a confidence level inferior to this threshold will be excluded")
                    .minimum(0.0)
                    .maximum(1.0)
                    .mutable_playing()
                    .default_value(DEFAULT_THRESHOLD)
                    .build(),
                glib::ParamSpecString::builder("labels-file")
                    .nick("Class labels file")
                    .mutable_ready()
                    .blurb("Path to a file containing class label following the COCO format")
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "class-confidence-threshold" => {
                settings.threshold = value.get().expect("type checked upstream");
            }
            "labels-file" => {
                let labels_file = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                settings.labels_file = match labels_file {
                    Some(path) if std::path::Path::new(&path).is_file() => Some(path),
                    Some(path) => {
                        gst::error!(CAT, imp = self, "Unable to load {path}");
                        None
                    }
                    None => None,
                };
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "class-confidence-threshold" => settings.threshold.to_value(),
            "labels-file" => settings.labels_file.to_value(),
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

impl GstObjectImpl for ClassifierTensorDec {}

impl ElementImpl for ClassifierTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Classification tensor decoder",
                "Tensordecoder",
                "Decode tensors output from classification model using common format.",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            fn tensor_caps(tensor_id: &str) -> gst::Caps {
                let strided = |dims: gst::Array| {
                    gst::Structure::builder("tensor/strided")
                        .field("tensor-id", tensor_id)
                        .field("dims", dims)
                        .field("dims-order", "row-major")
                        .field("type", gst::List::new(["float32", "uint8"]))
                        .build()
                };

                gst::Caps::builder_full()
                    .structure(strided(gst::Array::new([
                        gst::IntRange::<i32>::new(0, 1),
                        gst::IntRange::<i32>::new(1, i32::MAX),
                    ])))
                    .structure(strided(gst::Array::new([gst::IntRange::<i32>::new(
                        1,
                        i32::MAX,
                    )])))
                    .build()
            }

            fn video_caps(group_id: &str) -> gst::Caps {
                gst::Caps::builder("video/x-raw")
                    .any_features()
                    .field(
                        "tensors",
                        gst::Structure::builder("tensorgroups")
                            .field(group_id, gst::UniqueList::new([tensor_caps(group_id)]))
                            .build(),
                    )
                    .build()
            }

            let mut sink_caps = video_caps(CLASSIFICATION_ID);
            sink_caps
                .make_mut()
                .append(video_caps(CLASSIFICATION_SOFTMAXED_ID));
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
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::NullToReady {
            let labels_file = self.settings.lock().unwrap().labels_file.clone();
            if let Some(labels_file) = labels_file {
                let labels = load_labels(&labels_file).map_err(|err| {
                    gst::error!(CAT, imp = self, "{err}");
                    gst::StateChangeError
                })?;
                self.state.lock().unwrap().labels = Some(labels);
            }
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::ReadyToNull {
            self.state.lock().unwrap().labels = None;
        }

        Ok(ret)
    }
}

impl BaseTransformImpl for ClassifierTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let s = incaps
            .structure(0)
            .expect("sink template guarantees at least one structure");
        let tensors = s
            .get::<gst::Structure>("tensors")
            .expect("sink template guarantees 'tensors' field in structure");
        let group = tensors
            .get::<gst::UniqueList>(CLASSIFICATION_ID)
            .or_else(|_| tensors.get::<gst::UniqueList>(CLASSIFICATION_SOFTMAXED_ID))
            .expect("sink template guarantees classification group in tensors structure");
        let tensor_caps = group
            .first()
            .expect("sink template guarantees at least one element in classification group")
            .get::<gst::Caps>()
            .expect("sink template guarantees group's elements are caps");
        let ts = tensor_caps
            .structure(0)
            .expect("sink template guarantees at least one structure in tensor caps");
        let tensor_id = ts
            .get::<&str>("tensor-id")
            .expect("sink template guarantees 'tensor-id' field in tensor structure");
        let do_softmax = tensor_id == CLASSIFICATION_ID;
        let dims = ts
            .get::<gst::Array>("dims")
            .expect("sink template guarantees 'dims' field in tensor structure");
        let class_dim = dims
            .as_slice()
            .last()
            .expect("sink template guarantees non-empty dims");
        let class_count = class_dim
            .get::<i32>()
            .expect("sink template guarantees class count in dims")
            as usize;

        let has_labels_file = self.settings.lock().unwrap().labels_file.is_some();
        let mut state = self.state.lock().unwrap();
        state.do_softmax = do_softmax;

        // Labels generated for previous caps, regenerate them if the class count changed.
        if !has_labels_file
            && state
                .labels
                .as_ref()
                .is_some_and(|labels| labels.len() != class_count)
        {
            state.labels = None;
        }

        if state
            .labels
            .as_ref()
            .is_some_and(|labels| labels.len() != class_count)
        {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ("Label-file/Tensor mismatch"),
                ["Class count from tensor mismatch class count from label file"]
            );
            return Err(gst::loggable_error!(CAT, "Label-file/Tensor mismatch"));
        }

        if state.labels.is_none() {
            state.labels = Some(
                (0..class_count)
                    .map(|i| glib::Quark::from_str(i.to_string()))
                    .collect(),
            );
        }

        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let threshold = self.settings.lock().unwrap().threshold;
        let state = self.state.lock().unwrap();
        let tensor_id = if state.do_softmax {
            CLASSIFICATION_ID
        } else {
            CLASSIFICATION_SOFTMAXED_ID
        };

        let Some(result) = with_classification_tensor(buffer, tensor_id, |tensor| {
            let Some(labels) = state.labels.as_deref() else {
                return Err("No labels for the negotiated caps".to_string());
            };
            decode_classification(tensor, labels, state.do_softmax, self.obj().upcast_ref())
        }) else {
            gst::warning!(CAT, imp = self, "missing tensor meta");
            return Ok(gst::FlowSuccess::Ok);
        };
        drop(state);

        let classification = result.map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["{err}"]);
            gst::FlowError::Error
        })?;

        if let Some(Classification {
            index,
            label,
            confidence,
        }) = classification.filter(|c| c.confidence >= threshold)
        {
            let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);
            if let Err(err) = rmeta.add_one_cls_mtd(confidence, label) {
                gst::warning!(CAT, imp = self, "Failed to add classification: {err}");
            }
            gst::log!(
                CAT,
                imp = self,
                "Max class is {index}:{} with {confidence}",
                label.as_str()
            );
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

/// Call `f` with the first `tensor_id` tensor in the buffer's tensor metas, with dims
/// `[N]` or `[1, N]`, as float32 or uint8.
fn with_classification_tensor<R>(
    buffer: &gst::BufferRef,
    tensor_id: &str,
    f: impl FnOnce(&gst_analytics::Tensor) -> R,
) -> Option<R> {
    let id = glib::Quark::from_str(tensor_id);
    let candidates = [
        (gst_analytics::TensorDataType::Float32, &[usize::MAX][..]),
        (gst_analytics::TensorDataType::Float32, &[1, usize::MAX][..]),
        (gst_analytics::TensorDataType::Uint8, &[usize::MAX][..]),
        (gst_analytics::TensorDataType::Uint8, &[1, usize::MAX][..]),
    ];

    for meta in buffer.iter_meta::<gst_analytics::TensorMeta>() {
        let tensor = candidates.iter().find_map(|(data_type, dims)| {
            meta.typed_tensor(
                id,
                *data_type,
                gst_analytics::TensorDimOrder::RowMajor,
                dims,
            )
        });
        if let Some(tensor) = tensor {
            return Some(f(tensor));
        }
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Classification {
    index: usize,
    label: glib::Quark,
    confidence: f32,
}

/// Each value divided by `scale`, exponentiated and normalised so they sum to 1.
///
/// The maximum is subtracted before exponentiating, which doesn't change the result but
/// keeps large values from overflowing to infinity.
fn softmax(values: impl Iterator<Item = f32>, scale: f32) -> Vec<f32> {
    let scaled: Vec<f32> = values.map(|v| v / scale).collect();
    let max = scaled.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = scaled.into_iter().map(|v| (v - max).exp()).collect();
    let sum: f32 = exps.iter().sum();

    exps.into_iter().map(|e| e / sum).collect()
}

/// Decode `tensor` into the most confident class, if any class has a confidence above 0.
fn decode_classification(
    tensor: &gst_analytics::Tensor,
    labels: &[glib::Quark],
    do_softmax: bool,
    element: &gst::Element,
) -> Result<Option<Classification>, String> {
    let tensor_len = *tensor.dims().last().unwrap_or(&0);
    let mut len = tensor_len;

    if len != labels.len() {
        gst::warning!(
            CAT,
            obj = element,
            "Labels file or class indices has size {}, but the tensor has {tensor_len} entries, it is probably not the right labels file",
            labels.len()
        );
        len = len.min(labels.len());
    }

    let map = tensor
        .data()
        .map_readable()
        .map_err(|_| "Failed to map tensor data".to_string())?;

    gst::trace!(CAT, obj = element, "Tensor shape dims {:?}", tensor.dims());

    let confidences: Vec<f32> = match tensor.data_type() {
        gst_analytics::TensorDataType::Float32 => {
            if map.size() != len * size_of::<f32>() {
                return Err(format!(
                    "Tensor size is not as expected for float: tensor_len({tensor_len}), map.size({}) != label-file-length({}) * sizeof(float)({})",
                    map.size(),
                    labels.len(),
                    size_of::<f32>()
                ));
            }

            let values = map
                .as_chunks::<{ size_of::<f32>() }>()
                .0
                .iter()
                .map(|b| f32::from_ne_bytes(*b));
            if do_softmax {
                softmax(values, 1.0)
            } else {
                values.collect()
            }
        }
        gst_analytics::TensorDataType::Uint8 => {
            if map.size() != len {
                return Err(format!(
                    "Tensor size is not as expected for uint8: tensor_len({tensor_len}), map.size({}) != label-file-length({}))",
                    map.size(),
                    labels.len(),
                ));
            }

            if do_softmax {
                softmax(map.iter().map(|&v| v as f32), 255.0)
            } else {
                map.iter().map(|&v| v as f32 / 255.0).collect()
            }
        }
        data_type => return Err(format!("Can't handle data type {data_type:?}")),
    };

    let mut best: Option<Classification> = None;
    for (index, &confidence) in confidences.iter().enumerate() {
        if confidence > best.map_or(0.0, |b| b.confidence) {
            best = Some(Classification {
                index,
                label: labels[index],
                confidence,
            });
        }
    }

    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::test_util::{labels_file, path};
    use std::sync::Once;

    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            gst::Element::register(
                None,
                "classifiertensordec",
                gst::Rank::NONE,
                super::super::ClassifierTensorDec::static_type(),
            )
            .unwrap();
        });
    }

    fn init_with_element() -> gst::Element {
        init();
        gst::ElementFactory::make("classifiertensordec")
            .build()
            .unwrap()
    }

    fn f32_tensor(id: &str, dims: &[usize], values: &[f32]) -> gst_analytics::Tensor {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
        gst_analytics::Tensor::new_simple(
            glib::Quark::from_str(id),
            gst_analytics::TensorDataType::Float32,
            gst::Buffer::from_mut_slice(bytes),
            gst_analytics::TensorDimOrder::RowMajor,
            dims,
        )
    }

    fn u8_tensor(id: &str, dims: &[usize], values: &[u8]) -> gst_analytics::Tensor {
        gst_analytics::Tensor::new_simple(
            glib::Quark::from_str(id),
            gst_analytics::TensorDataType::Uint8,
            gst::Buffer::from_mut_slice(values.to_vec()),
            gst_analytics::TensorDimOrder::RowMajor,
            dims,
        )
    }

    fn labels(n: usize) -> Vec<glib::Quark> {
        (0..n)
            .map(|i| glib::Quark::from_str(format!("class{i}")))
            .collect()
    }

    #[test]
    fn softmaxes_raw_float_scores() {
        let element = init_with_element();
        let t = f32_tensor(CLASSIFICATION_ID, &[1, 3], &[1.0, 3.0, 2.0]);

        let c = decode_classification(&t, &labels(3), true, &element)
            .unwrap()
            .unwrap();

        let e: Vec<f64> = [1.0f64, 3.0, 2.0].iter().map(|v| v.exp()).collect();
        let expected = e[1] / e.iter().sum::<f64>();
        assert_eq!(c.index, 1);
        assert_eq!(c.label.as_str(), "class1");
        assert!((c.confidence as f64 - expected).abs() < 1e-6);
    }

    #[test]
    fn large_raw_scores_do_not_overflow() {
        let element = init_with_element();
        let t = f32_tensor(CLASSIFICATION_ID, &[1, 3], &[100.0, 0.0, 1.0]);
        let c = decode_classification(&t, &labels(3), true, &element)
            .unwrap()
            .unwrap();
        assert_eq!(c.index, 0);
        assert!((c.confidence - 1.0).abs() < 1e-6);

        let t = f32_tensor(CLASSIFICATION_ID, &[1, 3], &[89.0, 88.0, 0.0]);
        let c = decode_classification(&t, &labels(3), true, &element)
            .unwrap()
            .unwrap();
        let expected = 1.0 / (1.0 + (-1f64).exp());
        assert_eq!(c.index, 0);
        assert!((c.confidence as f64 - expected).abs() < 1e-6);
    }

    #[test]
    fn softmaxed_float_scores_are_used_as_is() {
        let element = init_with_element();
        let t = f32_tensor(CLASSIFICATION_SOFTMAXED_ID, &[3], &[0.2, 0.1, 0.7]);

        let c = decode_classification(&t, &labels(3), false, &element)
            .unwrap()
            .unwrap();

        assert_eq!((c.index, c.confidence), (2, 0.7));
    }

    #[test]
    fn uint8_scores_are_scaled() {
        let element = init_with_element();
        let t = u8_tensor(CLASSIFICATION_SOFTMAXED_ID, &[1, 3], &[10, 255, 51]);
        let c = decode_classification(&t, &labels(3), false, &element)
            .unwrap()
            .unwrap();
        assert_eq!((c.index, c.confidence), (1, 1.0));

        let t = u8_tensor(CLASSIFICATION_ID, &[3], &[0, 255, 0]);
        let c = decode_classification(&t, &labels(3), true, &element)
            .unwrap()
            .unwrap();
        let expected = 1f64.exp() / (2.0 + 1f64.exp());
        assert_eq!(c.index, 1);
        assert!((c.confidence as f64 - expected).abs() < 1e-6);
    }

    #[test]
    fn ties_pick_the_first_class() {
        let element = init_with_element();
        let t = f32_tensor(CLASSIFICATION_SOFTMAXED_ID, &[4], &[0.1, 0.4, 0.4, 0.1]);
        let c = decode_classification(&t, &labels(4), false, &element)
            .unwrap()
            .unwrap();
        assert_eq!(c.index, 1);
    }

    #[test]
    fn no_class_above_zero_gives_no_classification() {
        let element = init_with_element();
        let t = f32_tensor(CLASSIFICATION_SOFTMAXED_ID, &[3], &[0.0, -0.5, 0.0]);
        assert_eq!(
            decode_classification(&t, &labels(3), false, &element).unwrap(),
            None
        );
    }

    #[test]
    fn tensor_larger_than_labels_is_an_error() {
        let element = init_with_element();
        let t = f32_tensor(CLASSIFICATION_SOFTMAXED_ID, &[4], &[0.1, 0.2, 0.3, 0.4]);
        assert!(decode_classification(&t, &labels(3), false, &element).is_err());

        let t = u8_tensor(CLASSIFICATION_SOFTMAXED_ID, &[4], &[1, 2, 3, 4]);
        assert!(decode_classification(&t, &labels(3), false, &element).is_err());
    }

    #[test]
    fn tensor_shorter_than_labels_runs_ok() {
        let element = init_with_element();
        let t = f32_tensor(CLASSIFICATION_SOFTMAXED_ID, &[2], &[0.1, 0.9]);
        let c = decode_classification(&t, &labels(3), false, &element)
            .unwrap()
            .unwrap();

        assert_eq!((c.index, c.confidence), (1, 0.9));
        assert_eq!(c.label.as_str(), "class1");

        let t = u8_tensor(CLASSIFICATION_SOFTMAXED_ID, &[2], &[0, 255]);
        let c = decode_classification(&t, &labels(3), false, &element)
            .unwrap()
            .unwrap();

        assert_eq!((c.index, c.confidence), (1, 1.0));
        assert_eq!(c.label.as_str(), "class1");
    }

    fn sink_caps(tensor_id: &str, dims: &[i32], data_type: &str) -> gst::Caps {
        let tensor = gst::Caps::builder("tensor/strided")
            .field("tensor-id", tensor_id)
            .field("dims", gst::Array::new(dims.iter().copied()))
            .field("dims-order", "row-major")
            .field("type", data_type)
            .build();
        gst::Caps::builder("video/x-raw")
            .field("format", "RGB")
            .field("width", 224)
            .field("height", 224)
            .field("framerate", gst::Fraction::new(30, 1))
            .field(
                "tensors",
                gst::Structure::builder("tensorgroups")
                    .field(tensor_id, gst::UniqueList::new([tensor]))
                    .build(),
            )
            .build()
    }

    fn buffer_with_tensor(tensor: gst_analytics::Tensor) -> gst::Buffer {
        let mut buffer = gst::Buffer::with_size(224 * 224 * 3).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            let mut meta = gst_analytics::TensorMeta::add(buffer);
            meta.set(glib::Slice::from(vec![tensor]));
        }
        buffer
    }

    fn top_class(buffer: &gst::BufferRef) -> Option<(String, f32)> {
        let rmeta = buffer.meta::<gst_analytics::AnalyticsRelationMeta>()?;
        let cls = rmeta
            .iter::<gst_analytics::AnalyticsClassificationMtd>()
            .next()?;
        Some((cls.quark(0).as_str().to_string(), cls.level(0)))
    }

    #[test]
    fn generated_labels_are_class_indices() {
        init();
        let mut h = gst_check::Harness::new("classifiertensordec");
        h.set_src_caps(sink_caps(CLASSIFICATION_SOFTMAXED_ID, &[1, 3], "float32"));

        let out = h
            .push_and_pull(buffer_with_tensor(f32_tensor(
                CLASSIFICATION_SOFTMAXED_ID,
                &[1, 3],
                &[0.1, 0.2, 0.7],
            )))
            .unwrap();

        assert_eq!(top_class(&out), Some(("2".to_string(), 0.7)));
    }

    #[test]
    fn labels_file_names_the_class() {
        init();
        let file = labels_file("cat\ndog\nbird\n");
        // Labels are loaded going from NULL to READY, which the harness does on creation.
        let element = gst::ElementFactory::make("classifiertensordec")
            .property("labels-file", path(&file))
            .build()
            .unwrap();
        let mut h = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
        h.set_src_caps(sink_caps(CLASSIFICATION_SOFTMAXED_ID, &[3], "float32"));

        let out = h
            .push_and_pull(buffer_with_tensor(f32_tensor(
                CLASSIFICATION_SOFTMAXED_ID,
                &[3],
                &[0.1, 0.8, 0.1],
            )))
            .unwrap();

        assert_eq!(top_class(&out), Some(("dog".to_string(), 0.8)));
    }

    #[test]
    fn classes_below_threshold_are_excluded() {
        init();
        let mut h = gst_check::Harness::new("classifiertensordec");
        h.set_src_caps(sink_caps(CLASSIFICATION_SOFTMAXED_ID, &[3], "float32"));
        let tensor = || f32_tensor(CLASSIFICATION_SOFTMAXED_ID, &[3], &[0.3, 0.6, 0.1]);

        let out = h.push_and_pull(buffer_with_tensor(tensor())).unwrap();
        assert_eq!(top_class(&out), Some(("1".to_string(), 0.6)));

        h.element()
            .unwrap()
            .set_property("class-confidence-threshold", 0.7f32);
        let out = h.push_and_pull(buffer_with_tensor(tensor())).unwrap();
        assert_eq!(top_class(&out), None);
    }

    #[test]
    fn buffer_without_tensor_passes_through_without_meta() {
        init();
        let mut h = gst_check::Harness::new("classifiertensordec");
        h.set_src_caps(sink_caps(CLASSIFICATION_ID, &[1, 3], "float32"));

        let out = h
            .push_and_pull(gst::Buffer::with_size(224 * 224 * 3).unwrap())
            .unwrap();

        assert!(out.meta::<gst_analytics::AnalyticsRelationMeta>().is_none());
    }

    #[test]
    fn missing_labels_file_is_rejected() {
        init();
        let element = gst::ElementFactory::make("classifiertensordec")
            .property("labels-file", "/nonexistent/labels.txt")
            .build()
            .unwrap();
        assert_eq!(element.property::<Option<String>>("labels-file"), None);
    }
}
