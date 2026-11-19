// Copyright (C) 2026 Collabora Ltd
//  @author: Denis Shimizu <denis.shimizu@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-bytetracker
 * @see_also: objectdetectionoverlay.
 *
 * Object tracking element based on [ByteTrack](https://github.com/FoundationVision/ByteTrack).
 *
 * It tracks the object detection metadata attached to the buffers by an upstream tensor
 * decoder: a tracking metadata is added to each buffer for every tracked object, related
 * to its object detection metadata. The classes of the detections are not considered.
 *
 * ByteTrack also associates the detections with a low score, between 0.1 and
 * `track-threshold`, to the existing tracks, so the confidence threshold of the tensor
 * decoder has to be low enough to let these detections through. The detections that are
 * not tracked are left on the buffers without a tracking metadata.
 *
 * |[
 * gst-launch-1.0 filesrc location=video.webm ! decodebin3 \
 *     ! videoconvertscale add-borders=1 ! onnxinference model-file=yolov8s.onnx \
 *     ! yolov8tensordec2 class-confidence-threshold=0.1 ! bytetracker ! videoconvert \
 *     ! objectdetectionoverlay ! videoconvert ! autovideosink
 * ]| This takes a video, performs object detection via `onnxinference` on each frame, decodes
 * the inferred tensors with `yolov8tensordec2`, tracks the detected objects across frames with
 * `bytetracker` and then overlays them on the frames via `objectdetectionoverlay`.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_analytics::prelude::*;
use gst_analytics::{AnalyticsODMtd, AnalyticsRelationMeta, AnalyticsTrackingMtd};
use gst_base::prelude::*;
use gst_video::{VideoCapsBuilder, VideoInfo, subclass::prelude::*};

use std::sync::{LazyLock, Mutex};

use super::tracker::{ByteTracker as Tracker, Detection, Parameters};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "bytetracker",
        gst::DebugColorFlags::empty(),
        Some("ByteTrack object tracker element"),
    )
});

// Frame rate used to estimate the running time of the buffers without one, when the
// caps have no frame rate or a variable one (0/1)
const FALLBACK_FRAMERATE: u64 = 30;

#[derive(Debug, Clone, Copy)]
struct Settings {
    track_threshold: f32,
    low_threshold: f32,
    iou_score_threshold: f32,
    lost_track_timeout: gst::ClockTime,
    fuse_score: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            track_threshold: 0.5,
            low_threshold: 0.1,
            iou_score_threshold: 0.2,
            lost_track_timeout: gst::ClockTime::SECOND,
            fuse_score: true,
        }
    }
}

#[derive(Default)]
pub struct ByteTracker {
    settings: Mutex<Settings>,
    video_info: Mutex<Option<VideoInfo>>,
    // Running time of the previous buffer
    last_running_time: Mutex<Option<gst::ClockTime>>,
    tracker: Mutex<Tracker>,
}

#[glib::object_subclass]
impl ObjectSubclass for ByteTracker {
    const NAME: &'static str = "GstByteTracker";
    type Type = super::ByteTracker;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for ByteTracker {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("track-threshold")
                    .nick("Track threshold")
                    .blurb("Detections with a confidence above this are associated first, the ones between low-threshold and this are only associated to the remaining tracks. A detection needs a confidence of this plus 0.1 to start a track")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().track_threshold)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("low-threshold")
                    .nick("Low threshold")
                    .blurb("Detections with a confidence below this are ignored. The ones between this and track-threshold are only associated to the tracks remaining after the first association")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().low_threshold)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("iou-score-threshold")
                    .nick("IoU score threshold")
                    .blurb("Minimum IoU (multiplied by the detection confidence if fuse-score is set) to associate a detection above track-threshold to a track")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().iou_score_threshold)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder("lost-track-timeout")
                    .nick("Lost track timeout")
                    .blurb("Time, in nanoseconds, a lost track is kept to be found again, since its last detection")
                    .default_value(Settings::default().lost_track_timeout.nseconds())
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("fuse-score")
                    .nick("Fuse score")
                    .blurb("Multiply the IoU by the detection confidence when associating the detections above track-threshold. Better disabled on crowded scenes.")
                    .default_value(Settings::default().fuse_score)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "track-threshold" => settings.track_threshold = value.get().unwrap(),
            "low-threshold" => settings.low_threshold = value.get().unwrap(),
            "iou-score-threshold" => settings.iou_score_threshold = value.get().unwrap(),
            "lost-track-timeout" => {
                settings.lost_track_timeout = gst::ClockTime::from_nseconds(value.get().unwrap())
            }
            "fuse-score" => settings.fuse_score = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "track-threshold" => settings.track_threshold.to_value(),
            "low-threshold" => settings.low_threshold.to_value(),
            "iou-score-threshold" => settings.iou_score_threshold.to_value(),
            "lost-track-timeout" => settings.lost_track_timeout.nseconds().to_value(),
            "fuse-score" => settings.fuse_score.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ByteTracker {}

impl ElementImpl for ByteTracker {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "ByteTrack Object Tracker Element",
                "Analyzer/Video",
                "Tracks detected objects across frames with ByteTrack",
                "Denis Shimizu <denis.shimizu@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = VideoCapsBuilder::new().any_features().build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for ByteTracker {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {incaps:?}"))?;

        *self.video_info.lock().unwrap() = Some(info);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.video_info.lock().unwrap() = None;
        *self.last_running_time.lock().unwrap() = None;
        *self.tracker.lock().unwrap() = Tracker::default();
        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, imp = self, "Handling buffer {buffer:?}");

        // Copy the properties, so a change during this frame does not affect it
        let settings = *self.settings.lock().unwrap();

        // Build the tracker parameters
        let params = Parameters {
            track_threshold: settings.track_threshold as f64,
            low_threshold: settings.low_threshold as f64,
            // The tracker works with a distance (1 - IoU) instead of an IoU score
            match_threshold: 1.0 - settings.iou_score_threshold as f64,
            lost_track_timeout: settings.lost_track_timeout,
            fuse_score: settings.fuse_score,
        };

        // Get the running time of the buffer to timestamp the tracks, and to know how long
        // the lost tracks have been lost. If the segment is not in time format or the buffer
        // has no PTS, it is estimated as one frame after the previous buffer
        let running_time = self
            .obj()
            .segment()
            .downcast_ref::<gst::ClockTime>()
            .and_then(|segment| segment.to_running_time(buffer.pts()));
        let running_time = {
            let mut last_running_time = self.last_running_time.lock().unwrap();
            let running_time = running_time.unwrap_or_else(|| {
                let (numer, denom) = self
                    .video_info
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|info| info.fps())
                    .filter(|fps| fps.numer() > 0)
                    .map_or((FALLBACK_FRAMERATE, 1), |fps| {
                        (fps.numer() as u64, fps.denom() as u64)
                    });
                let frame_duration = gst::ClockTime::SECOND
                    .mul_div_floor(denom, numer)
                    .unwrap_or(gst::ClockTime::ZERO);
                last_running_time.map_or(gst::ClockTime::ZERO, |last| last + frame_duration)
            });
            *last_running_time = Some(running_time);
            running_time
        };

        // Collect the detections attached by the tensor decoder. Their metadata ids are
        // kept in a separate list, at the same index, to relate the tracks to them later.
        // Detections without a location are skipped, and the oriented boxes are tracked
        // by their axis-aligned bounding box
        let (od_ids, detections): (Vec<_>, Vec<_>) = buffer
            .meta::<AnalyticsRelationMeta>()
            .iter()
            .flat_map(|rmeta| rmeta.iter::<AnalyticsODMtd>())
            .filter_map(|od| {
                let loc = od.location().ok()?;
                Some((
                    od.id(),
                    Detection {
                        tlwh: [loc.x as f64, loc.y as f64, loc.w as f64, loc.h as f64],
                        score: loc.loc_conf_lvl as f64,
                    },
                ))
            })
            .unzip();

        // Run one ByteTrack step. It returns the active tracks of this frame, each one
        // with the index of its detection in `detections`
        let tracks = self
            .tracker
            .lock()
            .unwrap()
            .update(&params, &detections, running_time);

        // Add the tracking information of each track to the buffer
        for track in tracks {
            // Get the metadata id of the track detection
            let od_id = od_ids[track.detection];
            gst::log!(
                CAT,
                imp = self,
                "Object {od_id} has the tracking id {}",
                track.track_id
            );

            // Add a tracking metadata with the track id and the time it was first
            // seen. The tracks are always related to a detection of this buffer, so the
            // buffer has a relation meta
            let mut rmeta = buffer.meta_mut::<AnalyticsRelationMeta>().unwrap();
            let Ok(tracking_id) = rmeta
                .add_tracking_mtd(track.track_id, track.first_seen)
                .map(|mtd| mtd.id())
            else {
                gst::warning!(CAT, imp = self, "Failed to add the tracking metadata");
                continue;
            };

            // Relate the detection to its tracking metadata, so downstream elements
            // can find the track of each detection
            if rmeta
                .set_relation(gst_analytics::RelTypes::RELATE_TO, od_id, tracking_id)
                .is_err()
            {
                gst::warning!(CAT, imp = self, "Failed to relate the tracking metadata");
            }

            // Set this frame as the last time the track was seen
            if let Some(mut mtd) = rmeta.mtd_mut::<AnalyticsTrackingMtd>(tracking_id) {
                let _ = mtd.update_last_seen(running_time);
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
