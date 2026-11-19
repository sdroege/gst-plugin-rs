// Copyright (C) 2026 Collabora Ltd
//  @author: Denis Shimizu  <denis.shimizu@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

use gst::prelude::*;
use gst_analytics::prelude::*;
use gst_analytics::{AnalyticsODMtd, AnalyticsRelationMeta, AnalyticsTrackingMtd};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsanalytics::plugin_register_static().unwrap();
    });
}

// Pushes a buffer with the given `(x, y, confidence)` detections, and returns the
// `(tracking id, first seen, last seen)` related to each one of them on the output buffer
fn push_detections(
    h: &mut gst_check::Harness,
    frame: u64,
    detections: &[(i32, i32, f32)],
) -> Vec<Option<(u64, gst::ClockTime, gst::ClockTime)>> {
    let mut buffer = gst::Buffer::with_size(4 * 4).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(frame * 40.mseconds());
        if !detections.is_empty() {
            let mut rmeta = AnalyticsRelationMeta::add(buffer);
            for &(x, y, confidence) in detections {
                rmeta
                    .add_od_mtd(glib::Quark::from_str("person"), x, y, 40, 80, confidence)
                    .unwrap();
            }
        }
    }

    let buffer = h.push_and_pull(buffer).unwrap();
    assert_eq!(buffer.pts(), Some(frame * 40.mseconds()));

    let Some(rmeta) = buffer.meta::<AnalyticsRelationMeta>() else {
        return vec![];
    };
    rmeta
        .iter::<AnalyticsODMtd>()
        .map(|od| {
            let mut tracking = rmeta.iter_direct_related::<AnalyticsTrackingMtd>(
                od.id(),
                gst_analytics::RelTypes::RELATE_TO,
            );
            let info = tracking.next().map(|mtd| {
                let (id, first_seen, last_seen, lost) = mtd.info();
                assert!(!lost);
                (id, first_seen, last_seen)
            });
            assert!(tracking.next().is_none());
            info
        })
        .collect()
}

#[test]
fn test_tracking() {
    init();

    let mut h = gst_check::Harness::new("bytetracker");
    h.element()
        .unwrap()
        .set_property("lost-track-timeout", 80.mseconds().nseconds());
    h.set_src_caps_str("video/x-raw,format=GRAY8,width=4,height=4,framerate=30/1");

    let ts = |frame: u64| frame * 40.mseconds();

    // Buffers without detections pass through
    assert_eq!(push_detections(&mut h, 0, &[]), []);

    // Two objects moving in opposite directions
    assert_eq!(
        push_detections(&mut h, 1, &[(100, 100, 0.9), (400, 300, 0.8)]),
        [None, None]
    );
    for frame in 2..6 {
        let offset = 4 * (frame as i32 - 1);
        assert_eq!(
            push_detections(
                &mut h,
                frame,
                &[(400 - offset, 300, 0.8), (100 + offset, 100, 0.9)]
            ),
            [Some((2, ts(1), ts(frame))), Some((1, ts(1), ts(frame)))]
        );
    }

    // The low confidence detection of an object keeps its track, and the one of a new
    // object is not tracked
    assert_eq!(
        push_detections(
            &mut h,
            6,
            &[(120, 100, 0.3), (380, 300, 0.9), (200, 500, 0.3)]
        ),
        [Some((1, ts(1), ts(6))), Some((2, ts(1), ts(6))), None]
    );

    // An object is found again after being lost for up to lost-track-timeout
    assert_eq!(
        push_detections(&mut h, 7, &[(124, 100, 0.9)]),
        [Some((1, ts(1), ts(7)))]
    );
    assert_eq!(push_detections(&mut h, 8, &[(128, 100, 0.9)]).len(), 1);
    assert_eq!(
        push_detections(&mut h, 9, &[(132, 100, 0.9), (368, 300, 0.9)]),
        [Some((1, ts(1), ts(9))), Some((2, ts(1), ts(9)))]
    );

    // But a new track is started after that
    for frame in 10..13 {
        assert_eq!(
            push_detections(&mut h, frame, &[(132, 100, 0.9)]),
            [Some((1, ts(1), ts(frame)))]
        );
    }
    assert_eq!(
        push_detections(&mut h, 13, &[(132, 100, 0.9), (368, 300, 0.9)]),
        [Some((1, ts(1), ts(13))), None]
    );
    assert_eq!(
        push_detections(&mut h, 14, &[(132, 100, 0.9), (368, 300, 0.9)]),
        [Some((1, ts(1), ts(14))), Some((3, ts(13), ts(14)))]
    );
}
