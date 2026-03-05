// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gst_base::prelude::BaseTransformExtManual;

/// Compress element caps transformation.
/// Sink -> Src: wraps sink caps in `original-caps` field on each src template structure.
/// Src -> Sink: recovers `original-caps` from each src structure, returns `ANY` if none found.
pub fn compress_transform_caps(
    element: &gst_base::BaseTransform,
    direction: gst::PadDirection,
    caps: &gst::Caps,
    filter: Option<&gst::Caps>,
    cat: &gst::DebugCategory,
) -> Option<gst::Caps> {
    let other_caps = match direction {
        gst::PadDirection::Sink => {
            let tmpl_caps = element.src_pad().pad_template_caps();
            tmpl_caps
                .iter()
                .fold(
                    gst::Caps::builder_full(),
                    |builder, s: &gst::StructureRef| {
                        builder.structure(
                            gst::Structure::builder(s.name())
                                .field("original-caps", caps)
                                .build(),
                        )
                    },
                )
                .build()
        }
        gst::PadDirection::Src => {
            let recovered = caps
                .iter()
                .filter_map(|s| s.get::<gst::Caps>("original-caps").ok())
                .fold(gst::Caps::new_empty(), |mut acc, c| {
                    acc.get_mut().unwrap().append(c);
                    acc
                });
            if recovered.is_empty() {
                gst::Caps::new_any()
            } else {
                recovered
            }
        }
        _ => return None,
    };
    let obj_ref = element.upcast_ref::<gst::Element>();
    gst::debug!(
        cat,
        obj = obj_ref,
        "Transformed caps {caps} -> {other_caps} ({direction:?})"
    );
    Some(if let Some(f) = filter {
        f.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First)
    } else {
        other_caps
    })
}

/// Decompress element caps transformation.
/// Sink -> Src: extracts `original-caps` from the first sink structure; returns `ANY` if absent.
/// Src -> Sink: wraps src caps in `original-caps` field on each sink template structure (or empty structure if caps is ANY).
pub fn decompress_transform_caps(
    element: &gst_base::BaseTransform,
    direction: gst::PadDirection,
    caps: &gst::Caps,
    filter: Option<&gst::Caps>,
    cat: &gst::DebugCategory,
) -> Option<gst::Caps> {
    let other_caps = match direction {
        gst::PadDirection::Sink => caps
            .structure(0)
            .and_then(|s| s.get::<gst::Caps>("original-caps").ok())
            .unwrap_or_else(gst::Caps::new_any),
        gst::PadDirection::Src => {
            let tmpl_caps = element.sink_pad().pad_template_caps();
            tmpl_caps
                .iter()
                .fold(
                    gst::Caps::builder_full(),
                    |builder, s: &gst::StructureRef| {
                        if caps.is_any() {
                            builder.structure(gst::Structure::new_empty(s.name()))
                        } else {
                            builder.structure(
                                gst::Structure::builder(s.name())
                                    .field("original-caps", caps)
                                    .build(),
                            )
                        }
                    },
                )
                .build()
        }
        _ => return None,
    };
    let obj_ref = element.upcast_ref::<gst::Element>();
    gst::debug!(
        cat,
        obj = obj_ref,
        "Transformed caps {caps} -> {other_caps} ({direction:?})"
    );
    Some(if let Some(f) = filter {
        f.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First)
    } else {
        other_caps
    })
}
