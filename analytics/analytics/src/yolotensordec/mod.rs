// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst_base::subclass::prelude::*;

pub mod imp;

glib::wrapper! {
    pub struct YoloTensorDec(ObjectSubclass<imp::YoloTensorDec>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

/**
 * SECTION:element-yolov8tensordec2
 * @see_also: objectdetectionoverlay, onnxinference.
 *
 * Tensor decoder element for [Yolo object detection](https://docs.ultralytics.com/models).
 * This supports YoloV8-V10, Yolo11, Yolo12 and Yolo26 but only the one-to-many (non-NMS) heads of
 * YoloV10, Yolo11, Yolo12 and Yolo26.
 *
 * Test image file, model file and labels file can be found here : https://gitlab.collabora.com/gstreamer/onnx-models
 *
 * |[
 * gst-launch-1.0 filesrc location=onnx-models/images/bus.jpg \
 *     ! jpegdec ! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 \
 *     !  onnxinference model-file=onnx-models/models/yolov8s.onnx \
 *     ! yolov8tensordec label-file=onnx-models/labels/COCO_classes.txt \
 *     ! bjectdetectionoverlay ! videoconvert ! imagefreeze ! autovideosink
 * ]| This takes a JPEG, performs object detection via `onnxinference` on it, decodes the
 * inferred tensors with `yolov8tensordec` and then overlays the detected objects on the frame via
 * `objectdetectionoverlay`.
 *
 * Since: plugins-rs-0.16.0
 */
glib::wrapper! {
    pub struct YoloV8TensorDec(ObjectSubclass<imp::YoloV8TensorDec>) @extends YoloTensorDec, gst_base::BaseTransform, gst::Element, gst::Object;
}

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
 * ]| This takes a JPEG, performs object detection via `burn-yoloxinference` on it, decodes the
 * inferred tensors with `yoloxtensordec` and then overlays the detected objects on the frame via
 * `objectdetectionoverlay`.
 *
 * Since: plugins-rs-0.15.0
 */
glib::wrapper! {
    pub struct YoloXTensorDec(ObjectSubclass<imp::YoloXTensorDec>) @extends YoloTensorDec, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub trait YoloTensorDecImpl: BaseTransformImpl + ObjectSubclass<Type: IsA<YoloTensorDec>> {}

unsafe impl<T: YoloTensorDecImpl> IsSubclassable<T> for YoloTensorDec {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    YoloTensorDec::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    gst::Element::register(
        Some(plugin),
        "yolov8tensordec2",
        gst::Rank::PRIMARY + 1,
        YoloV8TensorDec::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "yoloxtensordec",
        gst::Rank::PRIMARY,
        YoloXTensorDec::static_type(),
    )?;

    Ok(())
}
