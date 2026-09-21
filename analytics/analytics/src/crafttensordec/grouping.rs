// Based on https://github.com/JaidedAI/EasyOCR/blob/master/easyocr/utils.py
//
// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// SPDX-License-Identifier: Apache-2.0

use geo::{Distance, Euclidean, Point};

use super::postprocess::Detection;

#[derive(Clone, Copy)]
struct Horizontal {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
    y_center: f64,
    height: f64,
    score: f32,
}

pub(super) fn group_text_box(
    polys: Vec<Detection>,
    mut transform: impl FnMut(Point<f64>) -> Point<f64>,
) -> Vec<Detection> {
    let slope_ths = 0.1;
    let ycenter_ths = 0.5;
    let height_ths = 0.5;
    let width_ths = 1.0;
    let add_margin = 0.05;
    let mut horizontal_list = Vec::new();
    let mut free_list = Vec::new();
    let mut combined_list = Vec::new();
    let mut merged_list = Vec::new();

    // poly top-left, top-right, low-right, low-left
    for detection in polys {
        let mut corners = detection.corners();
        let start = (0..4)
            .min_by(|&a, &b| {
                (corners[a].x() + corners[a].y()).total_cmp(&(corners[b].x() + corners[b].y()))
            })
            .unwrap();
        corners.rotate_left(start);
        let [p0, p1, p2, p3] = corners.map(&mut transform);
        let slope_up = (p1.y() - p0.y()) / (p1.x() - p0.x()).max(10.0);
        let slope_down = (p2.y() - p3.y()) / (p2.x() - p3.x()).max(10.0);
        if slope_up.abs().max(slope_down.abs()) < slope_ths {
            let x_max = p0.x().max(p1.x()).max(p2.x()).max(p3.x());
            let x_min = p0.x().min(p1.x()).min(p2.x()).min(p3.x());
            let y_max = p0.y().max(p1.y()).max(p2.y()).max(p3.y());
            let y_min = p0.y().min(p1.y()).min(p2.y()).min(p3.y());
            horizontal_list.push(Horizontal {
                x_min,
                x_max,
                y_min,
                y_max,
                y_center: 0.5 * (y_min + y_max),
                height: y_max - y_min,
                score: detection.score,
            });
        } else {
            let height = Euclidean.distance(p3, p0);
            let width = Euclidean.distance(p1, p0);
            let margin = (1.44 * add_margin * width.min(height)).trunc();
            let theta13 = ((p0.y() - p2.y()) / (p0.x() - p2.x()).max(10.0))
                .atan()
                .abs();
            let theta24 = ((p1.y() - p3.y()) / (p1.x() - p3.x()).max(10.0))
                .atan()
                .abs();
            let x1 = p0.x() - theta13.cos() * margin;
            let y1 = p0.y() - theta13.sin() * margin;
            let x2 = p1.x() + theta24.cos() * margin;
            let y2 = p1.y() - theta24.sin() * margin;
            let x3 = p2.x() + theta13.cos() * margin;
            let y3 = p2.y() + theta13.sin() * margin;
            let x4 = p3.x() - theta24.cos() * margin;
            let y4 = p3.y() + theta24.sin() * margin;

            if let Some(free_detect) = Detection::enclosing(
                &[
                    Point::new(x1, y1),
                    Point::new(x2, y2),
                    Point::new(x3, y3),
                    Point::new(x4, y4),
                ],
                detection.score,
            ) {
                free_list.push(free_detect);
            }
        }
    }
    horizontal_list.sort_by(|a, b| a.y_center.total_cmp(&b.y_center));

    // combine box
    let mut new_box = Vec::new();
    let mut height_sum = 0.0;
    let mut ycenter_sum = 0.0;
    for poly in horizontal_list {
        if new_box.is_empty() {
            height_sum = poly.height;
            ycenter_sum = poly.y_center;
            new_box.push(poly);
        } else {
            let count = new_box.len() as f64;
            let mean_height = height_sum / count;
            let mean_ycenter = ycenter_sum / count;

            // comparable height and comparable y_center level up to ths*height
            if (mean_ycenter - poly.y_center).abs() < ycenter_ths * mean_height {
                height_sum += poly.height;
                ycenter_sum += poly.y_center;
                new_box.push(poly);
            } else {
                height_sum = poly.height;
                ycenter_sum = poly.y_center;
                combined_list.push(new_box);
                new_box = vec![poly];
            }
        }
    }
    combined_list.push(new_box);

    // merge list use sort again
    for mut boxes in combined_list {
        if boxes.len() == 1 {
            // one box per line
            let box_ = boxes[0];
            let margin = (add_margin * (box_.x_max - box_.x_min).min(box_.height)).trunc();
            merged_list.push(Detection::from_rect(
                geo::Rect::new(
                    (box_.x_min - margin, box_.y_min - margin),
                    (box_.x_max + margin, box_.y_max + margin),
                ),
                box_.score,
            ));
        } else {
            // multiple boxes per line
            boxes.sort_by(|a, b| a.x_min.total_cmp(&b.x_min));
            let mut merged_box = Vec::new();
            let mut new_box = Vec::new();
            let mut height_sum = 0.0;
            let mut x_max = 0.0;

            for box_ in boxes {
                if new_box.is_empty() {
                    height_sum = box_.height;
                    x_max = box_.x_max;
                    new_box.push(box_);
                } else {
                    let mean_height = height_sum / new_box.len() as f64;

                    if (mean_height - box_.height).abs() < height_ths * mean_height
                        && (box_.x_min - x_max) < width_ths * box_.height
                    {
                        height_sum += box_.height;
                        x_max = box_.x_max;
                        new_box.push(box_);
                    } else {
                        height_sum = box_.height;
                        x_max = box_.x_max;
                        merged_box.push(new_box);
                        new_box = vec![box_];
                    }
                }
            }

            if !new_box.is_empty() {
                merged_box.push(new_box);
            }

            for mbox in merged_box {
                if mbox.len() != 1 {
                    // adjacent box in same line
                    let x_min = mbox.iter().map(|b| b.x_min).fold(f64::INFINITY, f64::min);
                    let x_max = mbox
                        .iter()
                        .map(|b| b.x_max)
                        .fold(f64::NEG_INFINITY, f64::max);
                    let y_min = mbox.iter().map(|b| b.y_min).fold(f64::INFINITY, f64::min);
                    let y_max = mbox
                        .iter()
                        .map(|b| b.y_max)
                        .fold(f64::NEG_INFINITY, f64::max);
                    let box_width = x_max - x_min;
                    let box_height = y_max - y_min;
                    let margin = (add_margin * box_width.min(box_height)).trunc();

                    // Keep the peak CRAFT score of the merged components
                    let score = mbox
                        .iter()
                        .map(|b| b.score)
                        .fold(f32::NEG_INFINITY, f32::max);
                    merged_list.push(Detection::from_rect(
                        geo::Rect::new(
                            (x_min - margin, y_min - margin),
                            (x_max + margin, y_max + margin),
                        ),
                        score,
                    ));
                } else {
                    // non adjacent box in same line
                    let box_ = mbox[0];
                    let box_width = box_.x_max - box_.x_min;
                    let margin = (add_margin * box_width.min(box_.height)).trunc();
                    merged_list.push(Detection::from_rect(
                        geo::Rect::new(
                            (box_.x_min - margin, box_.y_min - margin),
                            (box_.x_max + margin, box_.y_max + margin),
                        ),
                        box_.score,
                    ));
                }
            }
        }
    }
    merged_list.extend(free_list);
    merged_list
}
