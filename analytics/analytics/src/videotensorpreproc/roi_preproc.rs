// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::num::NonZeroI64;

use super::{
    converter::RoiConverter,
    model_info::Model,
    sampler::{Geometry, Roi},
};
use gst::glib;
use gst_analytics::{
    AnalyticsODMtd, AnalyticsRelationMeta, AnalyticsTensorProvenanceMtd, RelTypes, TensorListMeta,
    TensorProvenance, prelude::*,
};

pub(super) fn preprocess(
    buffer: &mut gst::BufferRef,
    converter: &mut RoiConverter,
    model: &Model,
    object_type: glib::Quark,
    allow_legacy_tensor_meta: bool,
) -> Result<(), String> {
    let info = converter.input_info();
    let mut has_matching_regions = false;

    // Collect ROIs from matching ODMtds
    let rois: Vec<_> = buffer
        .meta::<AnalyticsRelationMeta>()
        .map(|meta| {
            meta.iter::<AnalyticsODMtd>()
                .filter_map(|od| {
                    if od.obj_type() != Some(object_type) {
                        return None;
                    }

                    has_matching_regions = true;

                    let loc = od.oriented_location().ok()?;
                    if loc.w <= 0 || loc.h <= 0 || !loc.r.is_finite() {
                        return None;
                    }

                    let roi = if loc.r == 0.0 {
                        // Simple AABB rect
                        let [x, y, width, height] =
                            clip_roi(loc.x, loc.y, loc.w, loc.h, info.width(), info.height())?;
                        Roi {
                            x: x as f64,
                            y: y as f64,
                            width,
                            height,
                            rotation: 0.0,
                        }
                    } else {
                        // Rotated rectangles
                        use geo::Intersects;
                        let roi = Roi {
                            x: loc.x as f64,
                            y: loc.y as f64,
                            width: loc.w as u32,
                            height: loc.h as u32,
                            rotation: loc.r as f64,
                        };

                        let geometry = Geometry::new(roi, [1, 1]);
                        let corners =
                            [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)].map(|(x, y)| {
                                let [x, y] = geometry.point(x, y);
                                geo::Coord { x, y }
                            });

                        let polygon =
                            geo::Polygon::new(geo::LineString::from(corners.to_vec()), vec![]);

                        let frame = geo::Rect::new(
                            geo::Coord { x: 0.0, y: 0.0 },
                            geo::Coord {
                                x: info.width() as f64,
                                y: info.height() as f64,
                            },
                        );

                        if !polygon.intersects(&frame) {
                            return None;
                        }

                        roi
                    };
                    Some((od.id(), roi))
                })
                .collect()
        })
        .unwrap_or_default();

    // If there are no matching regions, process the entire frame as a single ROI
    if !has_matching_regions {
        let roi = Roi {
            x: 0.0,
            y: 0.0,
            width: info.width(),
            height: info.height(),
            rotation: 0.0,
        };

        let (tensor, transform) = converter.process(buffer, &[roi])?.remove(0);
        let correlation_id = {
            let mut meta = AnalyticsRelationMeta::add(buffer);
            let provenance = meta
                .add_tensor_provenance_mtd(model.group_id, &tensor)
                .map_err(|e| e.to_string())?;
            let id = provenance.id();
            let correlation_id = provenance.correlation_id();
            let mut provenance = meta.mtd_mut::<AnalyticsTensorProvenanceMtd>(id).unwrap();
            provenance
                .set_spatial_transform(&transform)
                .map_err(|e| e.to_string())?;
            correlation_id
        };

        return super::attach_single_tensor(
            buffer,
            model.group_id,
            tensor,
            correlation_id,
            allow_legacy_tensor_meta,
        );
    }

    if rois.is_empty() {
        return Ok(());
    }

    let mut regions = Vec::with_capacity(rois.len());
    let tensors = if model.config.dynamic_batch() {
        let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();

        // Group ROIs with the same width to create one batch tensor per group
        for (index, (_, roi)) in rois.iter().enumerate() {
            let width = converter.tensor_width(*roi)?;
            if let Some((_, indices)) = groups.iter_mut().find(|(w, _)| *w == width) {
                indices.push(index);
            } else {
                groups.push((width, vec![index]));
            }
        }

        let mut tensors = Vec::with_capacity(groups.len());
        for (_, indices) in groups {
            let geometry: Vec<_> = indices.iter().map(|&index| rois[index].1).collect();

            let (tensor, transforms) = converter.process_batch(buffer, &geometry)?;

            let start = regions.len();
            for (index, transform) in indices.into_iter().zip(transforms) {
                let (od, _) = rois[index];
                regions.push((od, transform));
            }

            tensors.push((tensor, start..regions.len()));
        }
        tensors
    } else {
        let geometry: Vec<_> = rois.iter().map(|(_, roi)| *roi).collect();
        let converted = converter.process(buffer, &geometry)?;
        let mut tensors = Vec::with_capacity(converted.len());
        for ((od, _), (tensor, transform)) in rois.into_iter().zip(converted) {
            let start = regions.len();
            regions.push((od, transform));
            tensors.push((tensor, start..regions.len()));
        }
        tensors
    };

    // Store correlation IDs in the same order as regions so the ranges apply to both
    let mut correlations = Vec::with_capacity(regions.len());
    for (tensor, range) in &tensors {
        for (od, transform) in &regions[range.clone()] {
            let mut meta = AnalyticsRelationMeta::add(buffer);
            let provenance = meta
                .add_tensor_provenance_mtd(model.group_id, tensor)
                .map_err(|e| e.to_string())?;

            let id = provenance.id();
            correlations.push(NonZeroI64::new(provenance.correlation_id()).unwrap());

            // Relate TensorProvenanceMtd with ODMtd
            meta.set_relation(RelTypes::RELATE_TO, id, *od)
                .map_err(|e| e.to_string())?;

            // Set tensor-to-video spatial transform
            meta.mtd_mut::<AnalyticsTensorProvenanceMtd>(id)
                .unwrap()
                .set_spatial_transform(transform)
                .map_err(|e| e.to_string())?;
        }
    }

    let mut list = TensorListMeta::add(buffer, model.group_id);
    for (tensor, range) in tensors {
        let key = tensor.id();
        list.append_item_full(
            [tensor].into(),
            &[TensorProvenance {
                key,
                correlation_ids: &correlations[range],
            }],
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn clip_roi(x: i32, y: i32, w: i32, h: i32, width: u32, height: u32) -> Option<[u32; 4]> {
    if w <= 0 || h <= 0 {
        return None;
    }

    let left = i64::from(x).clamp(0, i64::from(width));
    let top = i64::from(y).clamp(0, i64::from(height));
    let right = (i64::from(x) + i64::from(w)).clamp(0, i64::from(width));
    let bottom = (i64::from(y) + i64::from(h)).clamp(0, i64::from(height));
    (right > left && bottom > top).then_some([
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    ])
}
