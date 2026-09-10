// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2026 Jan Schmidt <jan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
use std::ops::{Add, Deref, DerefMut, Mul, Sub};

#[derive(Clone, Copy, Debug)]
pub struct BoundingBox {
    pub xmin: f32,
    pub xmax: f32,
    pub ymin: f32,
    pub ymax: f32,
    pub rotation: Option<f32>, // In radians from -pi/4 to 3pi/4, for OBB models
    pub class: u32,
    pub confidence: f32,
}

impl BoundingBox {
    pub fn from_corners(
        xmin: f32,
        ymin: f32,
        xmax: f32,
        ymax: f32,
        rotation: Option<f32>,
        class: u32,
        confidence: f32,
    ) -> Self {
        BoundingBox {
            xmin,
            ymin,
            xmax,
            ymax,
            rotation,
            class,
            confidence,
        }
    }
    pub fn from_center_extents(
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        rotation: Option<f32>,
        class: u32,
        confidence: f32,
    ) -> Self {
        let xmin = x - width / 2.;
        let ymin = y - height / 2.;

        BoundingBox {
            xmin,
            ymin,
            xmax: xmin + width,
            ymax: ymin + height,
            rotation,
            class,
            confidence,
        }
    }
    fn area(&self) -> f32 {
        self.width() * self.height()
    }
    fn width(&self) -> f32 {
        self.xmax - self.xmin
    }
    fn height(&self) -> f32 {
        self.ymax - self.ymin
    }
    fn center(&self) -> Point {
        Point::from_xy((self.xmin + self.xmax) / 2.0, (self.ymin + self.ymax) / 2.0)
    }
}

// Intersection over union of two non-rotated bounding boxes
pub fn iou(b1: &BoundingBox, b2: &BoundingBox) -> f32 {
    let b1_area = (b1.xmax - b1.xmin + 1.0) * (b1.ymax - b1.ymin + 1.0);
    let b2_area = (b2.xmax - b2.xmin + 1.0) * (b2.ymax - b2.ymin + 1.0);
    let i_xmin = f32::max(b1.xmin, b2.xmin);
    let i_xmax = f32::min(b1.xmax, b2.xmax);
    let i_ymin = f32::max(b1.ymin, b2.ymin);
    let i_ymax = f32::min(b1.ymax, b2.ymax);
    let i_area = f32::max(i_xmax - i_xmin + 1.0, 0.0) * f32::max(i_ymax - i_ymin + 1.0, 0.0);
    i_area / (b1_area + b2_area - i_area)
}

#[derive(Clone, Copy, Debug)]
struct Point {
    x: f32,
    y: f32,
}

impl Point {
    pub fn from_xy(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn rotate_radians(self, rot: f32) -> Point {
        Point::from_xy(
            self.x * rot.cos() - self.y * rot.sin(),
            self.x * rot.sin() + self.y * rot.cos(),
        )
    }
    // True if point p is to the left of vector ab
    fn is_inside(&self, a: &Point, b: &Point) -> bool {
        (b.x - a.x) * (self.y - a.y) > (b.y - a.y) * (self.x - a.x)
    }
}

impl Sub for &Point {
    type Output = Point;
    fn sub(self, other: Self) -> Self::Output {
        Point {
            x: self.x - other.x,
            y: self.y - other.y,
        }
    }
}

impl Add for &Point {
    type Output = Point;
    fn add(self, other: Self) -> Self::Output {
        Point {
            x: self.x + other.x,
            y: self.y + other.y,
        }
    }
}
impl Add<&Point> for Point {
    type Output = Point;
    fn add(self, other: &Point) -> Self::Output {
        <&Point as Add<&Point>>::add(&self, other)
    }
}
impl Add<Point> for &Point {
    type Output = Point;
    fn add(self, other: Point) -> Self::Output {
        <&Point as Add<&Point>>::add(self, &other)
    }
}
impl Add<Point> for Point {
    type Output = Point;
    fn add(self, other: Point) -> Self::Output {
        <&Point as Add<&Point>>::add(&self, &other)
    }
}

impl Mul<f32> for &Point {
    type Output = Point;
    fn mul(self, other: f32) -> Self::Output {
        Point {
            x: self.x * other,
            y: self.y * other,
        }
    }
}
impl Mul<&Point> for f32 {
    type Output = Point;
    fn mul(self, other: &Point) -> Self::Output {
        Point {
            x: self * other.x,
            y: self * other.y,
        }
    }
}
impl Mul<Point> for f32 {
    type Output = Point;
    fn mul(self, other: Point) -> Self::Output {
        <f32 as Mul<&Point>>::mul(self, &other)
    }
}

#[derive(Clone, Debug)]
struct Poly(Vec<Point>);

impl Deref for Poly {
    type Target = Vec<Point>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for Poly {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<&BoundingBox> for Poly {
    fn from(b: &BoundingBox) -> Self {
        let w2 = b.width() * 0.5;
        let h2 = b.height() * 0.5;
        let c = b.center();
        let rot = b.rotation.unwrap_or(0f32);

        Poly(vec![
            Point::from_xy(-w2, -h2).rotate_radians(rot) + c,
            Point::from_xy(w2, -h2).rotate_radians(rot) + c,
            Point::from_xy(w2, h2).rotate_radians(rot) + c,
            Point::from_xy(-w2, h2).rotate_radians(rot) + c,
        ])
    }
}

impl Poly {
    fn area(self: &Poly) -> f32 {
        let mut area = 0f32;

        if self.len() > 2 {
            let mut p1 = self.last().unwrap();
            for p2 in &self.0 {
                area += p2.x * p1.y - p1.x * p2.y;
                p1 = p2;
            }
        }

        area.abs() / 2.0f32
    }

    fn sutherland_hodgman_clip(self: &Poly, clip: &Poly) -> Poly {
        let mut result = self.clone();

        if clip.is_empty() {
            return result;
        }

        let mut clip_p1 = clip.last().unwrap();
        for clip_p2 in &clip.0 {
            let input = result;
            result = Poly(vec![]);

            let mut s = input.last().unwrap();
            for e in &input.0 {
                let s_is_inside = s.is_inside(clip_p1, clip_p2);

                if e.is_inside(clip_p1, clip_p2) {
                    if !s_is_inside && let Some(i) = get_line_intersection(clip_p1, clip_p2, s, e) {
                        result.push(i);
                    }
                    result.push(*e);
                } else if s_is_inside && let Some(i) = get_line_intersection(clip_p1, clip_p2, s, e)
                {
                    result.push(i);
                }
                s = e;
            }
            clip_p1 = clip_p2;

            if result.is_empty() {
                break;
            }
        }

        result
    }
}

// Find the interesction point of 2 vectors AB and CD, if there is one
fn get_line_intersection(a: &Point, b: &Point, c: &Point, d: &Point) -> Option<Point> {
    let delta_ab = b - a;
    let delta_cd = d - c;
    let delta_ac = c - a;
    let det = delta_ab.x * delta_cd.y - delta_ab.y * delta_cd.x;

    if det.abs() > 1e-6 {
        let t = (delta_ac.x * delta_cd.y - delta_ac.y * delta_cd.x) / det;

        Some(a + t * delta_ab)
    } else {
        // No solution - lines are parallel or intersect outside the segments
        None
    }
}

// Intersection over union of two oriented (rotated) bounding boxes
pub fn iou_oriented(b1: &BoundingBox, b2: &BoundingBox) -> f32 {
    let poly1 = Poly::from(b1);
    let poly2 = Poly::from(b2);

    let intersection = poly1.sutherland_hodgman_clip(&poly2);

    let union_area = b1.area() + b2.area() - intersection.area();
    if union_area > 0.0 {
        intersection.area() / union_area
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_poly_intersect() {
        let p = |x: f32, y: f32| Point::from_xy(x, y);

        // Overlapping squares
        let subject = Poly(vec![p(0.0, 0.0), p(50.0, 0.0), p(50.0, 50.0), p(0.0, 50.0)]);
        let clip_poly = Poly(vec![
            p(25.0, 25.0),
            p(75.0, 25.0),
            p(75.0, 75.0),
            p(25.0, 75.0),
        ]);
        let result = subject.sutherland_hodgman_clip(&clip_poly);
        assert![result.len() == 4];
        assert![result.area() > 0.0];

        /* Non intersecting square */
        let clip_poly = Poly(vec![
            p(75.0, 75.0),
            p(125.0, 75.0),
            p(125.0, 125.0),
            p(75.0, 125.0),
        ]);
        let result = subject.sutherland_hodgman_clip(&clip_poly);
        assert![result.is_empty()];
        assert![result.area() == 0.0];
    }
}
