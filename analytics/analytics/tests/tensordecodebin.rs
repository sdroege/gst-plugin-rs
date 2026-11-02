// Copyright (C) 2026 Collabora Ltd
//  @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*, subclass::prelude::*};
use std::sync::{LazyLock, Once};

fn init() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsanalytics::plugin_register_static().unwrap();

        // Two factories wrapping the same fixture element, both declaring support for the same
        // tensor-group-id, with different ranks so selection order is deterministic.
        gst::Element::register(
            None,
            "faketensordecoderhigh",
            gst::Rank::PRIMARY,
            fake_tensor_decoder::FakeTensorDecoder::static_type(),
        )
        .unwrap();
        gst::Element::register(
            None,
            "faketensordecoderlow",
            gst::Rank::MARGINAL,
            fake_tensor_decoder::FakeTensorDecoder::static_type(),
        )
        .unwrap();
    });
}

/// Create a tensor caps valid enough for the purpose of the test
fn tensor_group_caps(group_id: &str) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field(
            "tensors",
            gst::Structure::builder("tensorgroups")
                .field(group_id, true)
                .build(),
        )
        .build()
}

/// A minimal tensor decoder
mod fake_tensor_decoder {
    use super::*;

    glib::wrapper! {
        pub struct FakeTensorDecoder(ObjectSubclass<imp::FakeTensorDecoder>) @extends gst::Element, gst::Object;
    }

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct FakeTensorDecoder;

        #[glib::object_subclass]
        impl ObjectSubclass for FakeTensorDecoder {
            const NAME: &'static str = "FakeTensorDecoder";
            type Type = super::FakeTensorDecoder;
            type ParentType = gst::Element;
        }

        impl ObjectImpl for FakeTensorDecoder {
            fn constructed(&self) {
                self.parent_constructed();

                let obj = self.obj();
                let klass = obj.class();
                obj.add_pad(&gst::Pad::from_template(
                    &klass.pad_template("sink").unwrap(),
                ))
                .unwrap();
                obj.add_pad(&gst::Pad::from_template(
                    &klass.pad_template("src").unwrap(),
                ))
                .unwrap();
            }
        }

        impl GstObjectImpl for FakeTensorDecoder {}

        impl ElementImpl for FakeTensorDecoder {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                    LazyLock::new(|| {
                        gst::subclass::ElementMetadata::new(
                            "Fake Tensor Decoder",
                            "Tensordecoder/Test",
                            "Test fixture tensor decoder",
                            "Test",
                        )
                    });

                Some(&*ELEMENT_METADATA)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    vec![
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &super::tensor_group_caps("tensor-group-id"),
                        )
                        .unwrap(),
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &gst::Caps::new_any(),
                        )
                        .unwrap(),
                    ]
                });

                PAD_TEMPLATES.as_ref()
            }
        }
    }
}

fn tensordecodebin_children(h: &gst_check::Harness) -> Vec<gst::Element> {
    h.element()
        .unwrap()
        .downcast::<gst::Bin>()
        .unwrap()
        .children()
}

fn highest_ranked_factory(names: &[&str]) -> glib::GString {
    names
        .iter()
        .filter_map(|name| gst::ElementFactory::find(name))
        .max_by_key(|f| f.upcast_ref::<gst::PluginFeature>().rank())
        .unwrap_or_else(|| panic!("none of {names:?} are available"))
        .name()
}

/// Builds one `tensor` entry for a tensor-group
fn tensor_caps(tensor_id: &str, dims: &[i32], dims_order: &str, type_: &str) -> gst::Caps {
    gst::Caps::builder("tensor/strided")
        .field("tensor-id", tensor_id)
        .field(
            "dims",
            gst::Array::from_values(dims.iter().map(|d| d.to_send_value())),
        )
        .field("dims-order", dims_order)
        .field("type", type_)
        .build()
}

/// Builds caps declaring exactly one tensor-group named `group_id`
fn video_tensor_group_caps(
    group_id: &str,
    tensors: impl IntoIterator<Item = gst::Caps>,
) -> gst::Caps {
    gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgb)
        .width(8)
        .height(4)
        .framerate(gst::Fraction::new(30, 1))
        .field(
            "tensors",
            gst::Structure::builder("tensorgroups")
                .field(group_id, gst::UniqueList::new(tensors))
                .build(),
        )
        .build()
}

/// Pushes `caps` to a fresh `tensordecodebin2` and returns the name of the single selected
/// decoder's factory, failing the test if zero or more than one decoder got selected.
fn select_single_decoder(caps: gst::Caps) -> glib::GString {
    let mut h = gst_check::Harness::new("tensordecodebin2");
    h.set_src_caps(caps);

    let children = tensordecodebin_children(&h);
    assert_eq!(
        children.len(),
        1,
        "expected exactly one decoder to be selected, got: {children:?}"
    );
    children[0].factory().unwrap().name()
}

#[test]
fn test_yolo_v8_out() {
    init();

    let caps = video_tensor_group_caps(
        "yolo-v8-out",
        [tensor_caps(
            "yolo-v8-out",
            &[1, 84, 8400],
            "col-major",
            "float32",
        )],
    );

    assert_eq!(
        select_single_decoder(caps),
        highest_ranked_factory(&["yolov8tensordec", "yolov8tensordec2"])
    );
}

#[test]
fn test_yolo_v8_obb_out() {
    init();

    let caps = video_tensor_group_caps(
        "yolo-v8-obb-out",
        [tensor_caps(
            "yolo-v8-obb-out",
            &[1, 20, 8400],
            "col-major",
            "float32",
        )],
    );

    assert_eq!(
        select_single_decoder(caps),
        highest_ranked_factory(&["yoloobbv8tensordec", "yolov8obbtensordec"])
    );
}

#[test]
fn test_yolo_26_end2end_out() {
    init();

    let caps = video_tensor_group_caps(
        "yolo-26-end2end-out",
        [tensor_caps(
            "yolo-26-end2end-out",
            &[1, 300, 6],
            "row-major",
            "float32",
        )],
    );

    assert_eq!(
        select_single_decoder(caps),
        highest_ranked_factory(&["yolo26tensordec", "yolo26tensordec2"])
    );
}

#[test]
fn test_yolo_v8_segmentation_out() {
    init();

    let caps = video_tensor_group_caps(
        "yolo-v8-segmentation-out",
        [
            tensor_caps(
                "yolo-v8-segmentation-out-detections",
                &[1, 116, 8400],
                "col-major",
                "float32",
            ),
            tensor_caps(
                "yolo-v8-segmentation-out-protos",
                &[1, 32, 160, 160],
                "col-major",
                "float32",
            ),
        ],
    );

    assert_eq!(
        select_single_decoder(caps),
        highest_ranked_factory(&["yolov8segtensordec", "yolosegv8tensordec"])
    );
}

#[test]
fn test_yolo_v8_segmentation_out_normalized() {
    init();

    let caps = video_tensor_group_caps(
        "yolo-v8-segmentation-out-normalized",
        [
            tensor_caps(
                "yolo-v8-segmentation-out-detections-normalized",
                &[1, 116, 8400],
                "col-major",
                "float32",
            ),
            tensor_caps(
                "yolo-v8-segmentation-out-protos",
                &[1, 32, 160, 160],
                "col-major",
                "float32",
            ),
        ],
    );

    assert_eq!(select_single_decoder(caps), "yolosegv8tensordec");
}

#[test]
fn test_ssd_mobilenet_v1_variant_1_out() {
    init();

    let caps = video_tensor_group_caps(
        "ssd-mobilenet-v1-variant-1-out",
        [
            tensor_caps("generic-variant-1-out-count", &[1], "row-major", "float32"),
            tensor_caps(
                "ssd-mobilenet-v1-variant-1-out-scores",
                &[1, 100],
                "row-major",
                "float32",
            ),
            tensor_caps(
                "ssd-mobilenet-v1-variant-1-out-boxes",
                &[1, 100, 4],
                "row-major",
                "float32",
            ),
            tensor_caps(
                "ssd-mobilenet-v1-variant-1-out-classes",
                &[1, 100],
                "row-major",
                "float32",
            ),
        ],
    );

    assert_eq!(select_single_decoder(caps), "ssdtensordec");
}

#[test]
fn test_ultra_lightweight_face_detection_out() {
    init();

    let caps = video_tensor_group_caps(
        "ultra-lightweight-face-detection-rfb-320-v1-variant-1-out",
        [
            tensor_caps(
                "ssd-mobilenet-v1-variant-1-out-boxes",
                &[1, 4420, 4],
                "row-major",
                "float32",
            ),
            tensor_caps(
                "ultra-lightweight-face-detection-rfb-320-v1-variant-1-out-scores",
                &[1, 4420, 2],
                "row-major",
                "float32",
            ),
        ],
    );

    assert_eq!(
        select_single_decoder(caps),
        "ultralightfacedetectortensordec"
    );
}

#[test]
fn test_ultra_lightweight_face_detection_without_postproc_out() {
    init();

    let caps = video_tensor_group_caps(
        "ultra-lightweight-face-detection-rfb-320-v1-without-postproc-out",
        [
            tensor_caps(
                "ultra-lightweight-face-detection-rfb-320-v1-variant-1-out-boxes-without-postproc",
                &[1, 4420, 4],
                "row-major",
                "float32",
            ),
            tensor_caps(
                "ultra-lightweight-face-detection-rfb-320-v1-variant-1-out-scores",
                &[1, 4420, 2],
                "row-major",
                "float32",
            ),
        ],
    );

    assert_eq!(
        select_single_decoder(caps),
        "ultralightfacedetectortensordec"
    );
}

#[test]
fn test_classification_generic_out() {
    init();

    let caps = video_tensor_group_caps(
        "classification-generic-out",
        [tensor_caps(
            "classification-generic-out",
            &[1, 1000],
            "row-major",
            "float32",
        )],
    );

    assert_eq!(select_single_decoder(caps), "classifiertensordecoder");
}

#[test]
fn test_classification_generic_softmaxed_out() {
    init();

    let caps = video_tensor_group_caps(
        "classification-generic-softmaxed-out",
        [tensor_caps(
            "classification-generic-softmaxed-out",
            &[1, 1000],
            "row-major",
            "float32",
        )],
    );

    assert_eq!(select_single_decoder(caps), "classifiertensordecoder");
}

#[test]
fn test_avoid_dup_tensordec() {
    init();

    let mut h = gst_check::Harness::new("tensordecodebin2");
    h.set_src_caps(tensor_group_caps("tensor-group-id"));

    let children = tensordecodebin_children(&h);
    assert_eq!(
        children.len(),
        1,
        "expected exactly one decoder for a single tensor-group, got: {children:?}"
    );
    assert_eq!(
        children[0].factory().unwrap().name(),
        "faketensordecoderhigh",
        "the higher-ranked decoder should be the one selected"
    );
}
