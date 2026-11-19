// Copyright (C) 2026 Collabora Ltd
//  @author: Denis Shimizu  <denis.shimizu@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Port of the tracker of [ByteTrack](https://github.com/FoundationVision/ByteTrack)
// (`yolox/tracker/`): `kalman_filter.py`, `matching.py` and `byte_tracker.py`.

// Added to the track threshold to get the minimum score a detection needs to start a track
const NEW_TRACK_THRESHOLD_MARGIN: f64 = 0.1;
// Maximum IoU distance for the second association (low score detections)
const SECOND_MATCH_THRESHOLD: f64 = 0.5;
// Maximum distance for the association of the unconfirmed tracks
const UNCONFIRMED_MATCH_THRESHOLD: f64 = 0.7;
// A tracked and a lost track closer than this IoU distance are duplicates
const DUPLICATE_THRESHOLD: f64 = 0.15;

// Kalman filter with the state `(x, y, a, h, vx, vy, va, vh)`: center of the box, aspect
// ratio (width / height), height, and their velocities, on a constant velocity model.
// The uncertainties are relative to the height of the box.
const STD_WEIGHT_POSITION: f64 = 1.0 / 20.0;
const STD_WEIGHT_VELOCITY: f64 = 1.0 / 160.0;

type Mean = [f64; 8];
type Covariance = [[f64; 8]; 8];

fn kalman_initiate(xyah: [f64; 4]) -> (Mean, Covariance) {
    let h = xyah[3];
    let mean = [xyah[0], xyah[1], xyah[2], xyah[3], 0.0, 0.0, 0.0, 0.0];
    let std = [
        2.0 * STD_WEIGHT_POSITION * h,
        2.0 * STD_WEIGHT_POSITION * h,
        1e-2,
        2.0 * STD_WEIGHT_POSITION * h,
        10.0 * STD_WEIGHT_VELOCITY * h,
        10.0 * STD_WEIGHT_VELOCITY * h,
        1e-5,
        10.0 * STD_WEIGHT_VELOCITY * h,
    ];
    let mut covariance = [[0.0; 8]; 8];
    for i in 0..8 {
        covariance[i][i] = std[i] * std[i];
    }

    (mean, covariance)
}

fn kalman_predict(mean: &mut Mean, covariance: &mut Covariance) {
    let h = mean[3];
    let std = [
        STD_WEIGHT_POSITION * h,
        STD_WEIGHT_POSITION * h,
        1e-2,
        STD_WEIGHT_POSITION * h,
        STD_WEIGHT_VELOCITY * h,
        STD_WEIGHT_VELOCITY * h,
        1e-5,
        STD_WEIGHT_VELOCITY * h,
    ];

    // The motion matrix F is the identity plus ones at (i, i + 4): mean = F * mean
    for i in 0..4 {
        mean[i] += mean[i + 4];
    }

    // covariance = F * covariance * F^T + motion_cov
    let (position_rows, velocity_rows) = covariance.split_at_mut(4);
    for (position_row, velocity_row) in position_rows.iter_mut().zip(velocity_rows.iter()) {
        for (position, velocity) in position_row.iter_mut().zip(velocity_row) {
            *position += velocity;
        }
    }
    for row in covariance.iter_mut() {
        for j in 0..4 {
            row[j] += row[j + 4];
        }
    }
    for i in 0..8 {
        covariance[i][i] += std[i] * std[i];
    }
}

fn kalman_update(mean: &mut Mean, covariance: &mut Covariance, xyah: [f64; 4]) {
    // Project the state to the measurement space, which is its first 4 dimensions
    let h = mean[3];
    let std = [
        STD_WEIGHT_POSITION * h,
        STD_WEIGHT_POSITION * h,
        1e-1,
        STD_WEIGHT_POSITION * h,
    ];
    let mut projected_cov = [[0.0; 4]; 4];
    for i in 0..4 {
        projected_cov[i][..4].copy_from_slice(&covariance[i][..4]);
        projected_cov[i][i] += std[i] * std[i];
    }

    // Cholesky factorization: projected_cov = l * l^T
    let mut l = [[0.0; 4]; 4];
    for i in 0..4 {
        for j in 0..=i {
            let sum: f64 = (0..j).map(|k| l[i][k] * l[j][k]).sum();
            l[i][j] = if i == j {
                (projected_cov[i][i] - sum).sqrt()
            } else {
                (projected_cov[i][j] - sum) / l[j][j]
            };
        }
    }

    // kalman_gain = covariance * H^T * projected_cov^-1, solved row by row
    let mut kalman_gain = [[0.0; 4]; 8];
    for (r, gain) in kalman_gain.iter_mut().enumerate() {
        let mut y = [0.0; 4];
        for i in 0..4 {
            let sum: f64 = (0..i).map(|k| l[i][k] * y[k]).sum();
            y[i] = (covariance[r][i] - sum) / l[i][i];
        }
        for i in (0..4).rev() {
            let sum: f64 = (i + 1..4).map(|k| l[k][i] * gain[k]).sum();
            gain[i] = (y[i] - sum) / l[i][i];
        }
    }

    let innovation: [f64; 4] = std::array::from_fn(|i| xyah[i] - mean[i]);
    for i in 0..8 {
        mean[i] += (0..4)
            .map(|k| kalman_gain[i][k] * innovation[k])
            .sum::<f64>();
    }

    // covariance -= kalman_gain * projected_cov * kalman_gain^T
    let mut gain_cov = [[0.0; 4]; 8];
    for i in 0..8 {
        for j in 0..4 {
            gain_cov[i][j] = (0..4)
                .map(|k| kalman_gain[i][k] * projected_cov[k][j])
                .sum();
        }
    }
    for i in 0..8 {
        for j in 0..8 {
            covariance[i][j] -= (0..4)
                .map(|k| gain_cov[i][k] * kalman_gain[j][k])
                .sum::<f64>();
        }
    }
}

fn tlwh_to_xyah(tlwh: [f64; 4]) -> [f64; 4] {
    [
        tlwh[0] + tlwh[2] / 2.0,
        tlwh[1] + tlwh[3] / 2.0,
        tlwh[2] / tlwh[3],
        tlwh[3],
    ]
}

fn tlwh_to_tlbr(tlwh: [f64; 4]) -> [f64; 4] {
    [tlwh[0], tlwh[1], tlwh[0] + tlwh[2], tlwh[1] + tlwh[3]]
}

// Returns the `1 - IoU` of each pair of boxes, as a row-major `a.len() x b.len()` matrix.
// The boxes are `(min x, min y, max x, max y)`. As in the `cython_bbox` used by ByteTrack,
// the coordinates are taken as inclusive pixel indices, hence the `+ 1`.
fn iou_distance(a: &[[f64; 4]], b: &[[f64; 4]]) -> Vec<f64> {
    let mut dists = Vec::with_capacity(a.len() * b.len());
    for a in a {
        let a_area = (a[2] - a[0] + 1.0) * (a[3] - a[1] + 1.0);
        for b in b {
            let b_area = (b[2] - b[0] + 1.0) * (b[3] - b[1] + 1.0);
            let iw = a[2].min(b[2]) - a[0].max(b[0]) + 1.0;
            let ih = a[3].min(b[3]) - a[1].max(b[1]) + 1.0;
            let iou = if iw > 0.0 && ih > 0.0 {
                iw * ih / (a_area + b_area - iw * ih)
            } else {
                0.0
            };
            dists.push(1.0 - iou);
        }
    }

    dists
}

// Multiplies the IoU similarities by the score of the detections
fn fuse_score(dists: &mut [f64], scores: &[f64]) {
    if scores.is_empty() {
        return;
    }
    for row in dists.chunks_mut(scores.len()) {
        for (dist, score) in row.iter_mut().zip(scores) {
            *dist = 1.0 - (1.0 - *dist) * score;
        }
    }
}

// Minimum cost assignment of the rows to the columns of the row-major `cost` matrix, not
// matching the pairs costing more than `thresh`. Returns the `(row, column)` matches
// sorted by row, the unmatched rows and the unmatched columns.
//
// As `lap.lapjv(extend_cost=True, cost_limit=thresh)`, used by ByteTrack, this solves the
// square problem where each row and each column can also be matched with a dummy one, at
// the cost of `thresh / 2`. It is solved by the Hungarian algorithm.
fn linear_assignment(
    cost: &[f64],
    rows: usize,
    cols: usize,
    thresh: f64,
) -> (Vec<(usize, usize)>, Vec<usize>, Vec<usize>) {
    if rows == 0 || cols == 0 {
        return (vec![], (0..rows).collect(), (0..cols).collect());
    }

    let n = rows + cols;
    let extended_cost = |i: usize, j: usize| {
        if i < rows && j < cols {
            cost[i * cols + j]
        } else if i >= rows && j >= cols {
            0.0
        } else {
            thresh / 2.0
        }
    };

    // Rows and columns are 1-based here, 0 is a sentinel. `row_of[j]` is the row matched
    // to the column `j`, `u` and `v` are the potentials of the rows and columns.
    let mut u = vec![0.0; n + 1];
    let mut v = vec![0.0; n + 1];
    let mut row_of = vec![0; n + 1];
    let mut way = vec![0; n + 1];
    // Allocated once, and reset for each row
    let mut min_v = vec![f64::INFINITY; n + 1];
    let mut used = vec![false; n + 1];
    for i in 1..=n {
        row_of[0] = i;
        let mut j0 = 0;
        min_v.fill(f64::INFINITY);
        used.fill(false);
        loop {
            used[j0] = true;
            let i0 = row_of[j0];
            let mut delta = f64::INFINITY;
            let mut j1 = 0;
            for j in 1..=n {
                if used[j] {
                    continue;
                }
                let cur = extended_cost(i0 - 1, j - 1) - u[i0] - v[j];
                if cur < min_v[j] {
                    min_v[j] = cur;
                    way[j] = j0;
                }
                if min_v[j] < delta {
                    delta = min_v[j];
                    j1 = j;
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[row_of[j]] += delta;
                    v[j] -= delta;
                } else {
                    min_v[j] -= delta;
                }
            }
            j0 = j1;
            if row_of[j0] == 0 {
                break;
            }
        }
        while j0 != 0 {
            let j1 = way[j0];
            row_of[j0] = row_of[j1];
            j0 = j1;
        }
    }

    let mut matches = (1..=cols)
        .filter(|&j| row_of[j] <= rows)
        .map(|j| (row_of[j] - 1, j - 1))
        .collect::<Vec<_>>();
    matches.sort_unstable();
    let unmatched_rows = (0..rows)
        .filter(|i| !matches.iter().any(|(row, _)| row == i))
        .collect();
    let unmatched_cols = (1..=cols)
        .filter(|&j| row_of[j] > rows)
        .map(|j| j - 1)
        .collect();

    (matches, unmatched_rows, unmatched_cols)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackState {
    Tracked,
    Lost,
    Removed,
}

#[derive(Debug)]
struct STrack {
    mean: Mean,
    covariance: Covariance,
    track_id: u64,
    state: TrackState,
    // Tracks are only output once activated, which needs a detection in two consecutive
    // frames, except for the tracks started at the first frame
    is_activated: bool,
    // Frame of the last matched detection
    frame_id: u64,
    start_frame: u64,
    first_seen: gst::ClockTime,
    // Running time of the last matched detection
    last_seen: gst::ClockTime,
    // Index of the last matched detection
    detection: usize,
}

impl STrack {
    fn tlbr(&self) -> [f64; 4] {
        let w = self.mean[2] * self.mean[3];
        let h = self.mean[3];
        tlwh_to_tlbr([self.mean[0] - w / 2.0, self.mean[1] - h / 2.0, w, h])
    }

    fn update(
        &mut self,
        tlwh: [f64; 4],
        detection: usize,
        frame_id: u64,
        timestamp: gst::ClockTime,
    ) {
        kalman_update(&mut self.mean, &mut self.covariance, tlwh_to_xyah(tlwh));
        self.state = TrackState::Tracked;
        self.is_activated = true;
        self.frame_id = frame_id;
        self.last_seen = timestamp;
        self.detection = detection;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Detection {
    // `(top left x, top left y, width, height)`
    pub tlwh: [f64; 4],
    pub score: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Parameters {
    // Detections scoring more than this are used by the first association
    pub track_threshold: f64,
    // Detections scoring less than this are never used, not even by the second association
    pub low_threshold: f64,
    // Maximum distance for the first association
    pub match_threshold: f64,
    // Time a lost track is kept, since its last matched detection
    pub lost_track_timeout: gst::ClockTime,
    // Whether the first association uses the IoU fused with the detection score
    pub fuse_score: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Track {
    pub track_id: u64,
    pub first_seen: gst::ClockTime,
    // Index, in the detections of the frame, of the detection of this track
    pub detection: usize,
}

#[derive(Debug, Default)]
pub struct ByteTracker {
    tracked: Vec<STrack>,
    lost: Vec<STrack>,
    frame_id: u64,
    track_count: u64,
}

impl ByteTracker {
    // Tracks the detections of a new frame, returning the tracks found on it
    pub fn update(
        &mut self,
        params: &Parameters,
        detections: &[Detection],
        timestamp: gst::ClockTime,
    ) -> Vec<Track> {
        self.frame_id += 1;
        let frame_id = self.frame_id;

        // Indexes of the high and low score detections. Empty boxes have no aspect ratio.
        let (mut high, mut low) = (vec![], vec![]);
        for (idx, det) in detections.iter().enumerate() {
            if !(det.tlwh[2] > 0.0 && det.tlwh[3] > 0.0) {
                continue;
            }
            if det.score > params.track_threshold {
                high.push(idx);
            } else if det.score > params.low_threshold && det.score < params.track_threshold {
                low.push(idx);
            }
        }
        let tlbrs = |indexes: &[usize]| {
            indexes
                .iter()
                .map(|&idx| tlwh_to_tlbr(detections[idx].tlwh))
                .collect::<Vec<_>>()
        };
        let scores = |indexes: &[usize]| {
            indexes
                .iter()
                .map(|&idx| detections[idx].score)
                .collect::<Vec<_>>()
        };

        let mut tracked = std::mem::take(&mut self.tracked);
        let mut lost = std::mem::take(&mut self.lost);
        let mut new_tracks = vec![];

        {
            let (mut pool, mut unconfirmed): (Vec<&mut STrack>, Vec<&mut STrack>) =
                tracked.iter_mut().partition(|track| track.is_activated);
            pool.extend(lost.iter_mut());

            // Predict the current location with the Kalman filter
            for track in pool.iter_mut() {
                if track.state != TrackState::Tracked {
                    track.mean[7] = 0.0;
                }
                kalman_predict(&mut track.mean, &mut track.covariance);
            }

            // First association, with the high score detections
            let pool_tlbrs = pool.iter().map(|track| track.tlbr()).collect::<Vec<_>>();
            let mut dists = iou_distance(&pool_tlbrs, &tlbrs(&high));
            if params.fuse_score {
                fuse_score(&mut dists, &scores(&high));
            }
            let (matches, unmatched_tracks, unmatched_high) =
                linear_assignment(&dists, pool.len(), high.len(), params.match_threshold);
            for (itrack, idet) in matches {
                pool[itrack].update(detections[high[idet]].tlwh, high[idet], frame_id, timestamp);
            }

            // Second association, of the remaining tracked tracks with the low score
            // detections
            let mut remaining = unmatched_tracks
                .into_iter()
                .filter(|&itrack| pool[itrack].state == TrackState::Tracked)
                .collect::<Vec<_>>();
            let remaining_tlbrs = remaining
                .iter()
                .map(|&itrack| pool[itrack].tlbr())
                .collect::<Vec<_>>();
            let dists = iou_distance(&remaining_tlbrs, &tlbrs(&low));
            let (matches, unmatched_tracks, _) =
                linear_assignment(&dists, remaining.len(), low.len(), SECOND_MATCH_THRESHOLD);
            for (itrack, idet) in matches {
                pool[remaining[itrack]].update(
                    detections[low[idet]].tlwh,
                    low[idet],
                    frame_id,
                    timestamp,
                );
            }
            remaining = unmatched_tracks
                .into_iter()
                .map(|itrack| remaining[itrack])
                .collect();
            for itrack in remaining {
                pool[itrack].state = TrackState::Lost;
            }

            // Association of the unconfirmed tracks, usually the ones seen in a single
            // frame, with the remaining high score detections
            let high = unmatched_high
                .into_iter()
                .map(|idet| high[idet])
                .collect::<Vec<_>>();
            let unconfirmed_tlbrs = unconfirmed
                .iter()
                .map(|track| track.tlbr())
                .collect::<Vec<_>>();
            let mut dists = iou_distance(&unconfirmed_tlbrs, &tlbrs(&high));
            if params.fuse_score {
                fuse_score(&mut dists, &scores(&high));
            }
            let (matches, unmatched_tracks, unmatched_high) = linear_assignment(
                &dists,
                unconfirmed.len(),
                high.len(),
                UNCONFIRMED_MATCH_THRESHOLD,
            );
            for (itrack, idet) in matches {
                unconfirmed[itrack].update(
                    detections[high[idet]].tlwh,
                    high[idet],
                    frame_id,
                    timestamp,
                );
            }
            for itrack in unmatched_tracks {
                unconfirmed[itrack].state = TrackState::Removed;
            }

            // Start new tracks with the remaining high score detections
            for idet in unmatched_high {
                let det = &detections[high[idet]];
                if det.score < params.track_threshold + NEW_TRACK_THRESHOLD_MARGIN {
                    continue;
                }
                let (mean, covariance) = kalman_initiate(tlwh_to_xyah(det.tlwh));
                self.track_count += 1;
                new_tracks.push(STrack {
                    mean,
                    covariance,
                    track_id: self.track_count,
                    state: TrackState::Tracked,
                    is_activated: frame_id == 1,
                    frame_id,
                    start_frame: frame_id,
                    first_seen: timestamp,
                    last_seen: timestamp,
                    detection: high[idet],
                });
            }
        }

        // Remove the tracks lost for too long
        for track in lost.iter_mut() {
            if track.state == TrackState::Lost
                && timestamp.saturating_sub(track.last_seen) > params.lost_track_timeout
            {
                track.state = TrackState::Removed;
            }
        }

        // The tracked tracks are the ones still tracked, the new ones and the lost ones
        // found again, and the lost tracks are the ones still lost and the newly lost ones
        let (found, still_lost): (Vec<_>, Vec<_>) = lost
            .into_iter()
            .filter(|track| track.state != TrackState::Removed)
            .partition(|track| track.state == TrackState::Tracked);
        let (still_tracked, newly_lost): (Vec<_>, Vec<_>) = tracked
            .into_iter()
            .filter(|track| track.state != TrackState::Removed)
            .partition(|track| track.state == TrackState::Tracked);
        let mut tracked = still_tracked;
        tracked.extend(new_tracks);
        tracked.extend(found);
        let mut lost = still_lost;
        lost.extend(newly_lost);

        // Of a tracked and a lost track on the same place, keep the oldest one
        let tracked_tlbrs = tracked.iter().map(|track| track.tlbr()).collect::<Vec<_>>();
        let lost_tlbrs = lost.iter().map(|track| track.tlbr()).collect::<Vec<_>>();
        let dists = iou_distance(&tracked_tlbrs, &lost_tlbrs);
        for (itracked, track) in tracked.iter_mut().enumerate() {
            for (ilost, lost_track) in lost.iter_mut().enumerate() {
                if dists[itracked * lost_tlbrs.len() + ilost] >= DUPLICATE_THRESHOLD {
                    continue;
                }
                if track.frame_id - track.start_frame > lost_track.frame_id - lost_track.start_frame
                {
                    lost_track.state = TrackState::Removed;
                } else {
                    track.state = TrackState::Removed;
                }
            }
        }
        tracked.retain(|track| track.state != TrackState::Removed);
        lost.retain(|track| track.state != TrackState::Removed);

        self.tracked = tracked;
        self.lost = lost;

        self.tracked
            .iter()
            .filter(|track| track.is_activated)
            .map(|track| Track {
                track_id: track.track_id,
                first_seen: track.first_seen,
                detection: track.detection,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: Parameters = Parameters {
        track_threshold: 0.5,
        low_threshold: 0.1,
        match_threshold: 0.8,
        lost_track_timeout: gst::ClockTime::from_seconds(3),
        fuse_score: true,
    };

    fn det(x: f64, y: f64, score: f64) -> Detection {
        Detection {
            tlwh: [x, y, 40.0, 80.0],
            score,
        }
    }

    fn ids(tracks: &[Track]) -> Vec<(u64, usize)> {
        tracks.iter().map(|t| (t.track_id, t.detection)).collect()
    }

    #[test]
    fn linear_assignment_minimizes_cost() {
        // The greedy choice (0, 0) would force (1, 1), for a total cost of 1.0
        let cost = [0.1, 0.2, 0.2, 0.9];
        let (matches, unmatched_rows, unmatched_cols) = linear_assignment(&cost, 2, 2, 1.0);
        assert_eq!(matches, [(0, 1), (1, 0)]);
        assert!(unmatched_rows.is_empty() && unmatched_cols.is_empty());

        // Pairs costing more than the threshold are not matched
        let cost = [0.1, 0.9, 0.9, 0.7, 0.9, 0.9];
        let (matches, unmatched_rows, unmatched_cols) = linear_assignment(&cost, 2, 3, 0.5);
        assert_eq!(matches, [(0, 0)]);
        assert_eq!(unmatched_rows, [1]);
        assert_eq!(unmatched_cols, [1, 2]);
    }

    #[test]
    fn kalman_follows_constant_velocity() {
        let (mut mean, mut covariance) = kalman_initiate([100.0, 100.0, 0.5, 80.0]);
        for i in 1..=20 {
            kalman_predict(&mut mean, &mut covariance);
            kalman_update(
                &mut mean,
                &mut covariance,
                [100.0 + 5.0 * i as f64, 100.0, 0.5, 80.0],
            );
        }
        assert!((mean[0] - 200.0).abs() < 1.0, "x is {}", mean[0]);
        assert!((mean[4] - 5.0).abs() < 0.5, "vx is {}", mean[4]);

        kalman_predict(&mut mean, &mut covariance);
        assert!((mean[0] - 205.0).abs() < 1.5, "x is {}", mean[0]);
    }

    #[test]
    fn keeps_ids_of_moving_objects() {
        let mut tracker = ByteTracker::default();
        for frame in 0..10 {
            let offset = 4.0 * frame as f64;
            // The order of the detections is swapped on every frame
            let mut dets = vec![
                det(100.0 + offset, 100.0, 0.9),
                det(400.0 - offset, 300.0, 0.8),
            ];
            if frame % 2 == 1 {
                dets.reverse();
            }
            let tracks = tracker.update(&PARAMS, &dets, gst::ClockTime::from_seconds(frame));
            let expected = if frame % 2 == 1 {
                [(1, 1), (2, 0)]
            } else {
                [(1, 0), (2, 1)]
            };
            assert_eq!(ids(&tracks), expected);
            assert!(tracks.iter().all(|t| t.first_seen == gst::ClockTime::ZERO));
        }
    }

    #[test]
    fn new_track_needs_two_frames() {
        let mut tracker = ByteTracker::default();
        let first = det(100.0, 100.0, 0.9);
        assert_eq!(
            ids(&tracker.update(&PARAMS, &[first], gst::ClockTime::ZERO)),
            [(1, 0)]
        );

        // A detection appearing after the first frame is only output on its second frame
        let dets = [first, det(400.0, 300.0, 0.9)];
        let tracks = tracker.update(&PARAMS, &dets, gst::ClockTime::from_seconds(1));
        assert_eq!(ids(&tracks), [(1, 0)]);
        let tracks = tracker.update(&PARAMS, &dets, gst::ClockTime::from_seconds(2));
        assert_eq!(ids(&tracks), [(1, 0), (2, 1)]);
        assert_eq!(tracks[1].first_seen, gst::ClockTime::from_seconds(1));

        // And it is dropped if it is not detected on the second frame
        let dets = [first, det(400.0, 300.0, 0.9), det(200.0, 500.0, 0.9)];
        tracker.update(&PARAMS, &dets, gst::ClockTime::from_seconds(3));
        tracker.update(&PARAMS, &dets[..2], gst::ClockTime::from_seconds(4));
        let tracks = tracker.update(&PARAMS, &dets, gst::ClockTime::from_seconds(5));
        assert_eq!(ids(&tracks), [(1, 0), (2, 1)]);
    }

    #[test]
    fn low_score_detections() {
        let mut tracker = ByteTracker::default();
        let ts = gst::ClockTime::ZERO;
        assert_eq!(
            ids(&tracker.update(&PARAMS, &[det(100.0, 100.0, 0.9)], ts)),
            [(1, 0)]
        );

        // A low score detection keeps an existing track, but does not start a new one
        let dets = [det(400.0, 300.0, 0.3), det(102.0, 100.0, 0.3)];
        for _ in 0..3 {
            assert_eq!(ids(&tracker.update(&PARAMS, &dets, ts)), [(1, 1)]);
        }

        // Neither does a detection scoring less than the track threshold plus 0.1
        let dets = [det(400.0, 300.0, 0.55), det(102.0, 100.0, 0.9)];
        for _ in 0..3 {
            assert_eq!(ids(&tracker.update(&PARAMS, &dets, ts)), [(1, 1)]);
        }

        // The detections scoring less than 0.1 are ignored
        assert_eq!(
            ids(&tracker.update(&PARAMS, &[det(102.0, 100.0, 0.05)], ts)),
            []
        );
    }

    #[test]
    fn lost_track_is_found_again_until_removed() {
        let dets = [det(100.0, 100.0, 0.9)];
        let ts = gst::ClockTime::from_seconds;

        // One frame per second, so a track is lost for one more second on each missing frame
        let timeout = PARAMS.lost_track_timeout.seconds();
        for (missing_frames, expected_id) in [(timeout, 1), (timeout + 1, 2)] {
            let mut tracker = ByteTracker::default();
            assert_eq!(ids(&tracker.update(&PARAMS, &dets, ts(0))), [(1, 0)]);
            for frame in 1..=missing_frames {
                assert_eq!(ids(&tracker.update(&PARAMS, &[], ts(frame))), []);
            }
            // A removed track is replaced by a new one, output on its second frame
            tracker.update(&PARAMS, &dets, ts(missing_frames + 1));
            assert_eq!(
                ids(&tracker.update(&PARAMS, &dets, ts(missing_frames + 2))),
                [(expected_id, 0)]
            );
        }
    }

    #[test]
    fn ignores_empty_boxes() {
        let mut tracker = ByteTracker::default();
        let dets = [
            Detection {
                tlwh: [10.0, 10.0, 0.0, 0.0],
                score: 0.9,
            },
            det(100.0, 100.0, 0.9),
        ];
        let tracks = tracker.update(&PARAMS, &dets, gst::ClockTime::ZERO);
        assert_eq!(ids(&tracks), [(1, 1)]);
    }
}
