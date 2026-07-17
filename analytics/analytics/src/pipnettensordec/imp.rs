// Copyright (C) 2026 Collabora Ltd
//  @author: Olivier Crête <olivier.crete@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-pipnettensordec
 * @see_also: objectdetectionoverlay.
 *
 * Tensor decoder element for [PIPNet](https://github.com/jhb86253817/PIPNet)-based facial
 * landmark detection. Supports the 68-landmark (300W/300W+CelebA) and 98-landmark (WFLW)
 * variants; the variant is auto-detected from the `cls_map` tensor's landmark-count
 * dimension. Only a batch size of 1 is supported.
 *
 * The models expects its input to already be a (cropped) face image occupying the
 * full video frame; decoded landmarks are scaled by the frame width/height.
 *
 * |[
 * gst-launch-1.0 filesrc location=face.jpg ! jpegdec ! videoconvertscale \
 *     ! onnxinference model-file=pipnet_r18_300w_celeba_68.onnx \
 *     ! pipnettensordec ! keypointsoverlay \
 *     ! videoconvertscale ! imagefreeze ! autovideosink -v
 * ]| This takes a JPEG, performs facial landmark detection via `onnxinference` on it, decodes
 * the inferred tensors with `pipnettensordec` and then overlays the landmarks on the frame via
 * `objectdetectionoverlay`.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, subclass::prelude::*};
use gst_analytics::prelude::*;
use gst_video::{VideoCapsBuilder, VideoInfo, prelude::*, subclass::prelude::*};

use byte_slice_cast::*;

use std::sync::{Arc, LazyLock, Mutex};

use super::meanface::{self, NUM_NB, ReverseIndex, VariantInfo};

const PIPNET_ID: &str = "pipnet-out";
const CLS_MAP_ID: &str = "pipnet-out-cls-map";
const OFFSET_X_ID: &str = "pipnet-out-offset-x";
const OFFSET_Y_ID: &str = "pipnet-out-offset-y";
const NB_X_ID: &str = "pipnet-out-neighbor-offset-x";
const NB_Y_ID: &str = "pipnet-out-neighbor-offset-y";

const DEFAULT_VISIBILITY_THRESHOLD: f32 = 0.2;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "pipnettensordec",
        gst::DebugColorFlags::empty(),
        Some("PIPNet tensor decoder element"),
    )
});

#[derive(Clone)]
struct Settings {
    visibility_threshold: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            visibility_threshold: DEFAULT_VISIBILITY_THRESHOLD,
        }
    }
}

/// Tensor shape information fixated by caps negotiation and cached in `set_caps`, so that
/// per-buffer tensor lookups don't need to re-derive the variant or spatial dimensions, and
/// so the variant's neighbor reverse-index and skeleton pairs are computed once per stream
/// rather than per buffer. `Arc` makes cloning out of the `Mutex` cheap.
#[derive(Clone)]
struct TensorInfo {
    variant: &'static VariantInfo,
    feat_h: usize,
    feat_w: usize,
    reverse_index: Arc<ReverseIndex>,
    skeleton_pairs: Arc<Vec<i32>>,
}

#[derive(Default)]
pub struct PipnetTensorDec {
    settings: Mutex<Settings>,
    video_info: Mutex<Option<VideoInfo>>,
    tensor_info: Mutex<Option<TensorInfo>>,
}

#[glib::object_subclass]
impl ObjectSubclass for PipnetTensorDec {
    const NAME: &'static str = "GstPipnetTensorDec";
    type Type = super::PipnetTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for PipnetTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("visibility-threshold")
                    .nick("Visibility Threshold")
                    .blurb("Minimum per-landmark confidence to mark a keypoint as visible (otherwise occluded)")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(DEFAULT_VISIBILITY_THRESHOLD)
                    .mutable_playing()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "visibility-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.visibility_threshold = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "visibility-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.visibility_threshold.to_value()
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

impl GstObjectImpl for PipnetTensorDec {}

impl ElementImpl for PipnetTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "PIPNet Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes facial landmark tensors from a PIPNet model",
                "Olivier Crête <olivier.crete@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // The 2nd tensor dimension is the landmark count (times NUM_NB for nb_x/nb_y),
            // which is offered as a list of the values supported by the known PIPNet
            // variants so that caps negotiation fixates it to a single value; feat_h/feat_w
            // stay open ranges since they depend on the model's input resolution.
            fn strided_tensor(tensor_id: &str, lms_dim_values: &[i32]) -> gst::Caps {
                gst::Caps::builder("tensor/strided")
                    .field("tensor-id", tensor_id)
                    .field(
                        "dims",
                        gst::Array::from_values([
                            1i32.to_send_value(),
                            gst::List::new(lms_dim_values.iter().copied()).to_send_value(),
                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                        ]),
                    )
                    .field("dims-order", "row-major")
                    .field("type", "float32")
                    .build()
            }

            let num_lms_values: Vec<i32> = meanface::VARIANTS
                .iter()
                .map(|v| v.num_lms() as i32)
                .collect();
            let num_nb_values: Vec<i32> =
                num_lms_values.iter().map(|n| n * NUM_NB as i32).collect();

            let sink_caps = VideoCapsBuilder::new()
                .any_features()
                .field(
                    "tensors",
                    gst::Structure::builder("tensorgroups")
                        .field(
                            PIPNET_ID,
                            gst::UniqueList::new([
                                strided_tensor(CLS_MAP_ID, &num_lms_values),
                                strided_tensor(OFFSET_X_ID, &num_lms_values),
                                strided_tensor(OFFSET_Y_ID, &num_lms_values),
                                strided_tensor(NB_X_ID, &num_nb_values),
                                strided_tensor(NB_Y_ID, &num_nb_values),
                            ]),
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

            let src_caps = VideoCapsBuilder::new().any_features().build();
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

impl BaseTransformImpl for PipnetTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {incaps:?}"))?;

        let tensor_info = parse_tensor_info(incaps).ok_or_else(|| {
            gst::loggable_error!(CAT, "Invalid or unfixed tensor caps {incaps:?}")
        })?;

        *self.video_info.lock().unwrap() = Some(info);
        *self.tensor_info.lock().unwrap() = Some(tensor_info);
        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let visibility_threshold = self.settings.lock().unwrap().visibility_threshold;

        let video_size = self
            .video_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|info| (info.width(), info.height()));

        let Some((frame_width, frame_height)) = video_size else {
            gst::warning!(CAT, imp = self, "No video info");
            return Err(gst::FlowError::NotNegotiated);
        };

        let Some((landmarks, variant, skeleton_pairs)) =
            self.with_pipnet_tensors(buffer, |tensors| {
                let landmarks = decode_pipnet(&tensors);

                gst::log!(
                    CAT,
                    imp = self,
                    "Decoded {} landmarks for {} variant",
                    landmarks.len(),
                    tensors.variant.semantic_tag
                );
                (landmarks, tensors.variant, tensors.skeleton_pairs.clone())
            })
        else {
            gst::trace!(CAT, imp = self, "No complete PIPNet tensor set found");
            return Ok(gst::FlowSuccess::Ok);
        };

        if landmarks.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut positions = Vec::with_capacity(landmarks.len() * 2);
        let mut confidences = Vec::with_capacity(landmarks.len());
        let mut visibilities = Vec::with_capacity(landmarks.len());

        for lm in &landmarks {
            let x = (lm.x_norm * frame_width as f32).round() as i32;
            let y = (lm.y_norm * frame_height as f32).round() as i32;
            positions.push(x);
            positions.push(y);
            confidences.push(lm.confidence);
            let visibility = if lm.confidence >= visibility_threshold {
                gst_analytics::AnalyticsKeypointVisibility::VISIBLE
            } else {
                gst_analytics::AnalyticsKeypointVisibility::OCCLUDED
            };
            visibilities.push(visibility.bits() as u8);
        }

        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);
        if let Err(err) = rmeta.add_keypoints_group(
            variant.semantic_tag,
            gst_analytics::AnalyticsKeypointDimensions::_2d,
            &positions,
            Some(&confidences),
            Some(&visibilities),
            skeleton_pairs.as_slice(),
        ) {
            gst::warning!(CAT, imp = self, "Failed to add keypoints group: {}", err);
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

struct PipnetTensors<'a> {
    variant: &'static VariantInfo,
    feat_h: usize,
    feat_w: usize,
    reverse_index: Arc<ReverseIndex>,
    skeleton_pairs: Arc<Vec<i32>>,
    cls_map: &'a [f32],
    offset_x: &'a [f32],
    offset_y: &'a [f32],
    nb_x: &'a [f32],
    nb_y: &'a [f32],
}

/// A decoded landmark, normalized to `[0, 1]` in the (already cropped) input frame.
struct Landmark {
    x_norm: f32,
    y_norm: f32,
    confidence: f32,
}

/// Looks up an f32 tensor by its tensor-id, requiring the tensor's dimensions to match
/// `dims` exactly.
fn with_f32_tensor<F, R>(
    buffer: &gst::BufferRef,
    tensor_id: &str,
    dims: &[usize],
    f: F,
) -> Option<R>
where
    F: FnOnce(&[f32]) -> Option<R>,
{
    for meta in buffer.iter_meta::<gst_analytics::TensorMeta>() {
        let Some(tensor) = meta.typed_tensor(
            glib::Quark::from_str(tensor_id),
            gst_analytics::TensorDataType::Float32,
            gst_analytics::TensorDimOrder::RowMajor,
            dims,
        ) else {
            continue;
        };

        let Ok(map) = tensor.data().map_readable() else {
            continue;
        };
        let Ok(data) = map.as_slice_of::<f32>() else {
            continue;
        };

        return f(data);
    }

    None
}

impl PipnetTensorDec {
    /// Locates and reads the full set of 5 PIPNet output tensors in `buffer`, using the
    /// variant and spatial dimensions cached from `set_caps` (they are fixed once caps have
    /// been negotiated, so there is no need to re-derive them per buffer).
    fn with_pipnet_tensors<R>(
        &self,
        buffer: &gst::BufferRef,
        f: impl FnOnce(PipnetTensors<'_>) -> R,
    ) -> Option<R> {
        let TensorInfo {
            variant,
            feat_h,
            feat_w,
            reverse_index,
            skeleton_pairs,
        } = self.tensor_info.lock().unwrap().clone()?;

        let num_lms = variant.num_lms();
        let cls_dims = [1, num_lms, feat_h, feat_w];
        let nb_dims = [1, num_lms * NUM_NB, feat_h, feat_w];

        with_f32_tensor(buffer, CLS_MAP_ID, &cls_dims, |cls_map| {
            with_f32_tensor(buffer, OFFSET_X_ID, &cls_dims, |offset_x| {
                with_f32_tensor(buffer, OFFSET_Y_ID, &cls_dims, |offset_y| {
                    with_f32_tensor(buffer, NB_X_ID, &nb_dims, |nb_x| {
                        with_f32_tensor(buffer, NB_Y_ID, &nb_dims, |nb_y| {
                            let tensors = PipnetTensors {
                                variant,
                                feat_h,
                                feat_w,
                                reverse_index,
                                skeleton_pairs,
                                cls_map,
                                offset_x,
                                offset_y,
                                nb_x,
                                nb_y,
                            };
                            Some(f(tensors))
                        })
                    })
                })
            })
        })
    }
}

/// Parses the negotiated (fixed) sink caps to recover the `cls_map` tensor's landmark-count
/// and spatial dimensions, from which the PIPNet variant and feature-map size are derived.
fn parse_tensor_info(caps: &gst::Caps) -> Option<TensorInfo> {
    let s = caps.structure(0)?;
    let tensors = s.get::<gst::Structure>("tensors").ok()?;
    let pipnet_list = tensors.get::<gst::UniqueList>(PIPNET_ID).ok()?;
    let cls_map_caps = pipnet_list.iter().find_map(|v| {
        let caps = v.get::<gst::Caps>().ok()?;
        let s = caps.structure(0)?;
        (s.get::<&str>("tensor-id").ok()? == CLS_MAP_ID).then_some(caps)
    })?;
    let cls_map_s = cls_map_caps.structure(0)?;
    let dims = cls_map_s.get::<gst::Array>("dims").ok()?;
    let dims: Vec<i32> = dims
        .as_slice()
        .iter()
        .map(|v| v.get::<i32>().ok())
        .collect::<Option<_>>()?;

    let num_lms = dims[1] as usize;
    let feat_h = dims[2] as usize;
    let feat_w = dims[3] as usize;

    let variant = meanface::find_variant(num_lms)?;

    Some(TensorInfo {
        variant,
        feat_h,
        feat_w,
        reverse_index: Arc::new(variant.reverse_index()),
        skeleton_pairs: Arc::new(variant.skeleton_pairs()),
    })
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Decodes the PIPNet tensor set into normalized `[0, 1]` landmark positions, following
/// `yakhyo/pipnet-onnx`'s decoding algorithm: coarse cell selection via `cls_map` argmax,
/// refinement via `offset_x`/`offset_y`, and a second refinement via neighbor-assisted
/// `nb_x`/`nb_y` predictions merged through the meanface reverse-index table.
fn decode_pipnet(tensors: &PipnetTensors) -> Vec<Landmark> {
    let PipnetTensors {
        variant,
        feat_h,
        feat_w,
        reverse_index,
        cls_map,
        offset_x,
        offset_y,
        nb_x,
        nb_y,
        ..
    } = tensors;

    let num_lms = variant.num_lms();
    let spatial_size = feat_h * feat_w;

    // Own prediction + selected cell, per landmark.
    let mut own_x = vec![0.0f32; num_lms];
    let mut own_y = vec![0.0f32; num_lms];
    let mut confidence = vec![0.0f32; num_lms];
    // Neighbor predictions emitted by landmark l as its k-th neighbor, flattened [l * NUM_NB + k].
    let mut nb_pred_x = vec![0.0f32; num_lms * NUM_NB];
    let mut nb_pred_y = vec![0.0f32; num_lms * NUM_NB];

    for (l, plane) in cls_map.chunks_exact(spatial_size).take(num_lms).enumerate() {
        let (max_idx, &max_score) = plane
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .expect("spatial_size > 0");

        let row = max_idx / feat_w;
        let col = max_idx % feat_w;

        let cell = l * spatial_size + max_idx;
        own_x[l] = (col as f32 + offset_x[cell]) / *feat_w as f32;
        own_y[l] = (row as f32 + offset_y[cell]) / *feat_h as f32;
        confidence[l] = sigmoid(max_score);

        for k in 0..NUM_NB {
            let ch = l * NUM_NB + k;
            let nb_cell = ch * spatial_size + max_idx;
            nb_pred_x[ch] = (col as f32 + nb_x[nb_cell]) / *feat_w as f32;
            nb_pred_y[ch] = (row as f32 + nb_y[nb_cell]) / *feat_h as f32;
        }
    }

    let max_len = reverse_index.max_len;

    let mut landmarks = Vec::with_capacity(num_lms);
    for (i, (x, y)) in own_x.iter().zip(own_y.iter()).enumerate() {
        let mut sum_x = *x;
        let mut sum_y = *y;
        let mut count = 1usize;

        for m in 0..max_len {
            let entry = i * max_len + m;
            let j = reverse_index.index1[entry] as usize;
            let k = reverse_index.index2[entry] as usize;
            let ch = j * NUM_NB + k;
            sum_x += nb_pred_x[ch];
            sum_y += nb_pred_y[ch];
            count += 1;
        }

        landmarks.push(Landmark {
            x_norm: sum_x / count as f32,
            y_norm: sum_y / count as f32,
            confidence: confidence[i],
        });
    }

    landmarks
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

    /// Builds a synthetic tensor set for a variant where every landmark's `cls_map` argmax is
    /// at a fixed `(row, col)` cell, with zero offsets, so the decoded landmark should land
    /// exactly at `(col / feat_w, row / feat_h)`.
    fn with_synthetic_tensors<R>(
        variant: &'static VariantInfo,
        feat_h: usize,
        feat_w: usize,
        row: usize,
        col: usize,
        f: impl FnOnce(PipnetTensors<'_>) -> R,
    ) -> R {
        let num_lms = variant.num_lms();
        let spatial_size = feat_h * feat_w;
        let mut cls_map = vec![0.0f32; num_lms * spatial_size];
        let offset_x = vec![0.0f32; num_lms * spatial_size];
        let offset_y = vec![0.0f32; num_lms * spatial_size];
        let nb_x = vec![0.0f32; num_lms * NUM_NB * spatial_size];
        let nb_y = vec![0.0f32; num_lms * NUM_NB * spatial_size];

        for l in 0..num_lms {
            cls_map[l * spatial_size + row * feat_w + col] = 10.0;
        }

        let tensors = PipnetTensors {
            variant,
            feat_h,
            feat_w,
            reverse_index: Arc::new(variant.reverse_index()),
            skeleton_pairs: Arc::new(variant.skeleton_pairs()),
            cls_map: &cls_map,
            offset_x: &offset_x,
            offset_y: &offset_y,
            nb_x: &nb_x,
            nb_y: &nb_y,
        };
        f(tensors)
    }

    #[test]
    fn decode_zero_offsets_lands_on_cell_center_68() {
        ensure_gstreamer_initialized();
        let landmarks =
            with_synthetic_tensors(meanface::find_variant(68).unwrap(), 8, 8, 3, 5, |tensors| {
                decode_pipnet(&tensors)
            });

        assert_eq!(landmarks.len(), 68);
        for lm in &landmarks {
            assert!((lm.x_norm - 5.0 / 8.0).abs() < 1e-5);
            assert!((lm.y_norm - 3.0 / 8.0).abs() < 1e-5);
            assert!(lm.confidence > 0.9);
        }
    }

    #[test]
    fn decode_zero_offsets_lands_on_cell_center_98() {
        ensure_gstreamer_initialized();
        let landmarks =
            with_synthetic_tensors(meanface::find_variant(98).unwrap(), 8, 8, 2, 6, |tensors| {
                decode_pipnet(&tensors)
            });
        assert_eq!(landmarks.len(), 98);
        for lm in &landmarks {
            assert!((lm.x_norm - 6.0 / 8.0).abs() < 1e-5);
            assert!((lm.y_norm - 2.0 / 8.0).abs() < 1e-5);
        }
    }

    #[test]
    fn sigmoid_bounds() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
        assert!(sigmoid(10.0) > 0.99);
        assert!(sigmoid(-10.0) < 0.01);
    }
}
