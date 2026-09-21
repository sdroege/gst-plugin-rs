// CRAFT poly=false postprocessing, based on EasyOCR easyocr/craft_utils.py
// getDetBoxes() and getDetBoxes_core().
//
// Copyright (c) 2019-present NAVER Corp.
// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// SPDX-License-Identifier: MIT

use geo::{
    BoundingRect, Distance, Euclidean, MinimumRotatedRect, Point, Rotate, winding_order::Winding,
};

#[derive(Clone, Copy, Debug)]
pub(super) struct Thresholds {
    pub text: f32,
    pub link: f32,
    pub low_text: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        // Defaults used by EasyOCR
        Self {
            text: 0.7,
            link: 0.4,
            low_text: 0.4,
        }
    }
}

#[derive(Debug)]
pub(super) struct Detection {
    // Pre-rotation rectangle, clockwise rotation around its center in image coordinates
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub rotation: f64,
    // EasyOCR uses the peak text score only for component filtering
    pub score: f32,
}

impl Detection {
    pub(super) fn from_rect(rect: geo::Rect<f64>, score: f32) -> Self {
        Self {
            x: rect.min().x,
            y: rect.min().y,
            width: rect.width(),
            height: rect.height(),
            rotation: 0.0,
            score,
        }
    }

    pub(super) fn enclosing(points: &[Point<f64>], score: f32) -> Option<Self> {
        let geometry = geo::MultiPoint(points.to_vec());
        let mut rectangle = geometry.minimum_rotated_rect()?;

        rectangle.exterior_mut(|ring| ring.make_ccw_winding());
        let ring = &rectangle.exterior().0;

        // Use CRAFT's top-left starting corner, not the minimum rectangle's arbitrary start.
        let start = (0..4)
            .min_by(|&a, &b| (ring[a].x + ring[a].y).total_cmp(&(ring[b].x + ring[b].y)))
            .unwrap();
        let a = ring[start];
        let b = ring[(start + 1) % 4];
        let c = ring[(start + 2) % 4];

        let width = Euclidean.distance(a, b);
        let height = Euclidean.distance(b, c);
        let center = (a + c) * 0.5;
        let rotation = (b.y - a.y).atan2(b.x - a.x);

        Some(Self {
            x: center.x - width * 0.5,
            y: center.y - height * 0.5,
            width,
            height,
            rotation,
            score,
        })
    }

    pub(super) fn corners(&self) -> [Point<f64>; 4] {
        let center = Point::new(self.x + self.width * 0.5, self.y + self.height * 0.5);
        [
            Point::new(self.x, self.y),
            Point::new(self.x + self.width, self.y),
            Point::new(self.x + self.width, self.y + self.height),
            Point::new(self.x, self.y + self.height),
        ]
        .map(|p| p.rotate_around_point(self.rotation.to_degrees(), center))
    }

    pub(super) fn transform<F>(&self, transform: F) -> Option<Self>
    where
        F: FnMut(Point<f64>) -> Point<f64>,
    {
        Self::enclosing(&self.corners().map(transform), self.score)
    }
}

#[derive(Debug, Default)]
pub(super) struct Postprocessor {
    visited: Vec<bool>,
    component: Vec<usize>,
}

impl Postprocessor {
    // Rust port of CRAFT getDetBoxes_core()
    pub fn detect(
        &mut self,
        scores: &[f32],
        width: usize,
        height: usize,
        thresholds: Thresholds,
    ) -> Result<Vec<Detection>, &'static str> {
        let size = width
            .checked_mul(height)
            .ok_or("Heatmap dimensions overflow")?;

        if size.checked_mul(2) != Some(scores.len()) {
            return Err("Heatmap data does not match its dimensions");
        }

        let scores = scores.as_chunks::<2>().0;

        let text: Vec<_> = scores.iter().map(|p| p[0] > thresholds.low_text).collect();

        let link: Vec<_> = scores.iter().map(|p| p[1] > thresholds.link).collect();

        self.visited.clear();
        self.visited.resize(size, false);

        let mut detections = Vec::new();

        for seed in 0..size {
            if self.visited[seed] || !(text[seed] || link[seed]) {
                continue;
            }

            self.component.clear();
            self.component.push(seed);
            self.visited[seed] = true;

            let mut head = 0;
            let mut left = width;
            let mut right = 0;
            let mut top = height;
            let mut bottom = 0;
            let mut peak = f32::NEG_INFINITY;

            // Four-connected component labeling. The reference Python implementation
            // uses cv2.connectedComponentsWithStats(), but we do it directly using BFS
            while head < self.component.len() {
                let index = self.component[head];
                head += 1;

                let x = index % width;
                let y = index / width;

                left = left.min(x);
                right = right.max(x);
                top = top.min(y);
                bottom = bottom.max(y);
                peak = peak.max(scores[index][0]);

                let mut add_neighbor = |neighbor: usize| {
                    if !self.visited[neighbor] && (text[neighbor] || link[neighbor]) {
                        self.visited[neighbor] = true;
                        self.component.push(neighbor);
                    }
                };

                if x > 0 {
                    add_neighbor(index - 1);
                }

                if x + 1 < width {
                    add_neighbor(index + 1);
                }

                if y > 0 {
                    add_neighbor(index - width);
                }

                if y + 1 < height {
                    add_neighbor(index + width);
                }
            }

            // size filtering
            if self.component.len() < 10 {
                continue;
            }

            // thresholding
            if peak < thresholds.text {
                continue;
            }

            let w = right - left + 1;
            let h = bottom - top + 1;
            let niter = ((self.component.len() as f64 * w.min(h) as f64 / (w as f64 * h as f64))
                .sqrt()
                * 2.0) as usize;

            let sx = left.saturating_sub(niter);
            let sy = top.saturating_sub(niter);
            let ex = right.saturating_add(niter).saturating_add(2).min(width);
            let ey = bottom.saturating_add(niter).saturating_add(2).min(height);

            let rw = ex - sx;
            let rh = ey - sy;

            // make segmentation map
            let mut mask = vec![false; rw * rh];
            for &index in &self.component {
                // Remove pixels that only belong to the link map, after computing
                // the component size, bounds and score as in the reference.
                if link[index] && !text[index] {
                    continue;
                }
                mask[(index / width - sy) * rw + index % width - sx] = true;
            }

            let dilated = dilate(&mask, rw, rh, 1 + niter);

            // make box
            let mut points = Vec::new();
            for (y, row) in dilated.chunks_exact(rw).enumerate() {
                for (x, &value) in row.iter().enumerate() {
                    if value {
                        points.push(Point::new((sx + x) as f64, (sy + y) as f64));
                    }
                }
            }

            if let Some(mut detection) = Detection::enclosing(&points, peak) {
                // align diamond-shape
                let w = detection.width;
                let h = detection.height;
                let box_ratio = w.max(h) / (w.min(h) + 1e-5);

                if (1.0 - box_ratio).abs() <= 0.1 {
                    let bounds = geo::MultiPoint(points).bounding_rect().unwrap();
                    detection = Detection::from_rect(bounds, peak);
                }

                detections.push(detection);
            }
        }

        Ok(detections)
    }
}

fn dilate(mask: &[bool], width: usize, height: usize, kernel: usize) -> Vec<bool> {
    let anchor = kernel / 2;
    let after = kernel - 1 - anchor;

    let mut horizontal = vec![false; mask.len()];
    let mut output = vec![false; mask.len()];
    let mut prefix = vec![0usize; width.max(height) + 1];

    for y in 0..height {
        prefix[0] = 0;
        for x in 0..width {
            prefix[x + 1] = prefix[x] + usize::from(mask[y * width + x]);
        }
        for x in 0..width {
            horizontal[y * width + x] =
                prefix[(x + after + 1).min(width)] > prefix[x.saturating_sub(anchor)];
        }
    }

    for x in 0..width {
        prefix[0] = 0;
        for y in 0..height {
            prefix[y + 1] = prefix[y] + usize::from(horizontal[y * width + x]);
        }
        for y in 0..height {
            output[y * width + x] =
                prefix[(y + after + 1).min(height)] > prefix[y.saturating_sub(anchor)];
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_rectangle(angle: f64, expected_angle: f64) {
        let center = Point::new(200.0, 150.0);
        let points = [
            Point::new(100.0, 130.0),
            Point::new(300.0, 130.0),
            Point::new(300.0, 170.0),
            Point::new(100.0, 170.0),
        ]
        .map(|p| p.rotate_around_point(angle, center));

        for start in 0..4 {
            let mut input = points;
            input.rotate_left(start);
            for reverse in [false, true] {
                if reverse {
                    input.reverse();
                }
                let detection = Detection::enclosing(&input, 0.9).unwrap();
                assert!((detection.rotation.to_degrees() - expected_angle).abs() < 1e-8);
                assert!((detection.width - 200.0).abs() < 1e-8);
                assert!((detection.height - 40.0).abs() < 1e-8);
                assert!((detection.x - 100.0).abs() < 1e-8);
                assert!((detection.y - 130.0).abs() < 1e-8);
                assert_eq!(detection.score, 0.9);
                let corners = detection.corners();
                for point in points {
                    assert!(
                        corners
                            .iter()
                            .any(|&corner| Euclidean.distance(point, corner) < 1e-8)
                    );
                }
            }
        }
    }

    #[test]
    fn enclosing_axis_aligned() {
        check_rectangle(0.0, 0.0);
    }

    #[test]
    fn enclosing_positive_rotation() {
        check_rectangle(30.0, 30.0);
    }

    #[test]
    fn enclosing_negative_rotation() {
        check_rectangle(-30.0, -30.0);
    }

    #[test]
    fn enclosing_opposite_direction() {
        check_rectangle(153.1680303262898, -26.8319696737102);
    }
}
