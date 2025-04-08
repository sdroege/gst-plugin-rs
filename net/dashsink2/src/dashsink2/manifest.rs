// Copyright (C) 2025 Roberto Viola <rviola@vicomtech.org>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use chrono::Timelike;
use dash_mpd::*;
use serde::ser::Serialize;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct MediaRepresentation {
    pub id: String,
    pub is_video: bool,
    pub codec: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub framerate: Option<String>,
    pub bandwidth: Option<u64>,
    pub init_location: String,
    pub segment_template: String,
    pub segment_duration: u32,
}

#[derive(Debug, Default, PartialEq)]
pub enum ManifestType {
    #[default]
    Static,
    Dynamic,
}

#[derive(Debug, Default)]
pub struct Manifest {
    inner: MPD,
    mpd_type: ManifestType,
    minimum_update_period: Option<Duration>,
    utc_timing: Option<UTCTiming>,
}

impl Manifest {
    pub fn new() -> Self {
        let period = Period {
            id: Some("P0".into()),
            start: Some(Duration::from_secs(0)),
            ..Default::default()
        };
        Self {
            inner: MPD {
                xmlns: Some("urn:mpeg:dash:schema:mpd:2011".into()),
                schemaLocation: Some("urn:mpeg:dash:schema:mpd:2011 DASH-MPD.xsd".into()),
                mpdtype: Some("static".into()),
                profiles: Some("urn:mpeg:dash:profile:isoff-on-demand:2011".into()),
                minBufferTime: Some(Duration::from_secs(10)),
                minimumUpdatePeriod: None,
                periods: vec![period],
                ..Default::default()
            },
            mpd_type: ManifestType::default(),
            minimum_update_period: None,
            utc_timing: None,
        }
    }

    pub fn set_mpd_type(&mut self, mpdtype: ManifestType) {
        match mpdtype {
            ManifestType::Static => {
                self.inner.mpdtype = Some("static".into());
                self.inner.profiles = Some("urn:mpeg:dash:profile:isoff-on-demand:2011".into());
                self.inner.minimumUpdatePeriod = None;
                // If availabilityStartTime, it means that the Live DASH is turning VOD
                if let Some(start_time) = self.inner.availabilityStartTime {
                    let now = chrono::Utc::now();
                    let elapsed = now - start_time;
                    let elapsed_secs = elapsed.num_seconds().max(0) as u64;
                    self.inner.mediaPresentationDuration =
                        Some(std::time::Duration::from_secs(elapsed_secs));
                }
                self.inner.availabilityStartTime = None;
                self.inner.publishTime = None;
                if !self.inner.UTCTiming.is_empty() {
                    self.inner.UTCTiming.clear();
                }
            }
            ManifestType::Dynamic => {
                self.inner.mpdtype = Some("dynamic".into());
                self.inner.profiles = Some("urn:mpeg:dash:profile:isoff-live:2011".into());
                self.inner.mediaPresentationDuration = None;
                if self.inner.minimumUpdatePeriod.is_none() {
                    self.inner.minimumUpdatePeriod =
                        self.minimum_update_period.or(Some(Duration::from_secs(10)));
                }
                if self.inner.UTCTiming.is_empty()
                    && let Some(timing) = &self.utc_timing
                {
                    self.inner.UTCTiming = vec![timing.clone()];
                }
            }
        }
        self.mpd_type = mpdtype;
    }

    pub fn set_min_buffer_time(&mut self, time: u32) {
        self.inner.minBufferTime = Some(Duration::from_secs(time as u64));
    }

    pub fn set_minimum_update_period(&mut self, time: u32) {
        self.minimum_update_period = Some(Duration::from_secs(time as u64));
        if self.mpd_type == ManifestType::Dynamic {
            self.inner.minimumUpdatePeriod = Some(Duration::from_secs(time as u64));
        }
    }

    pub fn set_utc_timing_url(&mut self, url: String) {
        self.utc_timing = Some(UTCTiming {
            id: Some("1".to_string()),
            schemeIdUri: "urn:mpeg:dash:utc:http-xsdate:2014".to_string(),
            value: Some(url),
        });
        if self.mpd_type == ManifestType::Dynamic
            && let Some(timing) = &self.utc_timing
        {
            self.inner.UTCTiming = vec![timing.clone()];
        }
    }

    pub fn add_representation(&mut self, rep: MediaRepresentation) {
        let adaptations = &mut self.inner.periods.get_mut(0).unwrap().adaptations;
        let segment_template = SegmentTemplate {
            timescale: Some(1000),
            duration: Some(rep.segment_duration as f64),
            startNumber: Some(0),
            initialization: Some(rep.init_location.clone()),
            media: Some(rep.segment_template.clone()),
            ..Default::default()
        };

        let mut representation = Representation {
            id: Some(rep.id.clone()),
            codecs: Some(rep.codec.clone()),
            bandwidth: rep.bandwidth,
            SegmentTemplate: Some(segment_template),
            ..Default::default()
        };

        if rep.is_video {
            representation.width = rep.width;
            representation.height = rep.height;
            representation.frameRate = rep.framerate.clone();
            if !adaptations
                .iter()
                .any(|adapt| adapt.id.as_deref() == Some("video"))
            {
                adaptations.push(AdaptationSet {
                    id: Some("video".into()),
                    contentType: Some("video".into()),
                    mimeType: Some("video/mp4".into()),
                    segmentAlignment: Some(true),
                    subsegmentStartsWithSAP: Some(1),
                    ..Default::default()
                });
            }
            if let Some(adapt) = adaptations
                .iter_mut()
                .find(|adapt| adapt.id.as_deref() == Some("video"))
            {
                adapt.representations.push(representation);
            }
        } else {
            if !adaptations
                .iter()
                .any(|adapt| adapt.id.as_deref() == Some("audio"))
            {
                adaptations.push(AdaptationSet {
                    id: Some("audio".into()),
                    contentType: Some("audio".into()),
                    mimeType: Some("audio/mp4".into()),
                    segmentAlignment: Some(true),
                    subsegmentStartsWithSAP: Some(1),
                    ..Default::default()
                });
            }
            if let Some(adapt) = adaptations
                .iter_mut()
                .find(|adapt| adapt.id.as_deref() == Some("audio"))
            {
                adapt.representations.push(representation);
            }
        }
    }

    pub fn add_segment(&mut self, rep_name: String, index: u32, bandwidth: u64) {
        let adaptations = &mut self.inner.periods.get_mut(0).unwrap().adaptations;
        let reps_opt: Option<&mut Vec<Representation>> = if rep_name.contains("video") {
            adaptations
                .iter_mut()
                .find(|adapt| adapt.id.as_deref() == Some("video"))
                .map(|adapt| &mut adapt.representations)
        } else {
            adaptations
                .iter_mut()
                .find(|adapt| adapt.id.as_deref() == Some("audio"))
                .map(|adapt| &mut adapt.representations)
        };

        if let Some(reps) = reps_opt
            && let Some(rep) = reps
                .iter_mut()
                .find(|rep| rep.id.as_deref() == Some(&rep_name))
        {
            // Always update the bandwidth, regardless of mpd_type
            rep.bandwidth = Some(bandwidth);

            match &mut self.mpd_type {
                ManifestType::Static => {
                    if let Some(duration) = rep
                        .SegmentTemplate
                        .as_ref()
                        .and_then(|t| t.duration)
                        .map(|d| d as u64 * index as u64)
                    {
                        self.inner.mediaPresentationDuration = Some(Duration::from_millis(
                            self.inner
                                .mediaPresentationDuration
                                .map(|d| d.as_millis() as u64)
                                .map_or(duration, |existing| existing.max(duration)),
                        ));
                    }
                }
                ManifestType::Dynamic => {
                    let now = chrono::Utc::now().with_nanosecond(0).unwrap();
                    if self.inner.availabilityStartTime.is_none() {
                        self.inner.availabilityStartTime = Some(now);
                    }
                    self.inner.publishTime = Some(now);
                }
            }
        }
    }

    pub fn to_string(&self) -> Result<String, Box<dyn std::error::Error>> {
        let mut xml = String::new();
        let mut ser = quick_xml::se::Serializer::new(&mut xml);
        ser.indent(' ', 4);
        self.inner.serialize(ser)?;
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
{xml}
"#
        ))
    }
}
