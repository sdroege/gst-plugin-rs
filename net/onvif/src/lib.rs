// Copyright (C) 2022 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-rsonvif:
 *
 * Since: plugins-rs-0.9.0
 */
use gst::glib;
use once_cell::sync::Lazy;

mod onvifmetadatacombiner;
mod onvifmetadatadepay;
mod onvifmetadataoverlay;
mod onvifmetadataparse;
mod onvifmetadatapay;

// Offset in nanoseconds from midnight 01-01-1900 (prime epoch) to
// midnight 01-01-1970 (UNIX epoch)
pub(crate) const PRIME_EPOCH_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(2_208_988_800);

pub(crate) static NTP_CAPS: Lazy<gst::Caps> =
    Lazy::new(|| gst::Caps::builder("timestamp/x-ntp").build());
pub(crate) static UNIX_CAPS: Lazy<gst::Caps> =
    Lazy::new(|| gst::Caps::builder("timestamp/x-unix").build());

pub(crate) fn lookup_reference_timestamp(buffer: &gst::Buffer) -> Option<gst::ClockTime> {
    for meta in buffer.iter_meta::<gst::ReferenceTimestampMeta>() {
        if meta.reference().is_subset(&NTP_CAPS) {
            return Some(meta.timestamp());
        }
        if meta.reference().is_subset(&UNIX_CAPS) {
            return Some(meta.timestamp() + PRIME_EPOCH_OFFSET);
        }
    }

    None
}

pub(crate) fn xml_from_buffer(buffer: &gst::Buffer) -> Result<minidom::Element, gst::ErrorMessage> {
    let map = buffer.map_readable().map_err(|_| {
        gst::error_msg!(gst::ResourceError::Read, ["Failed to map buffer readable"])
    })?;

    let utf8 = std::str::from_utf8(&map).map_err(|err| {
        gst::error_msg!(
            gst::StreamError::Format,
            ["Failed to decode buffer as UTF-8: {}", err]
        )
    })?;

    let root = utf8.parse::<minidom::Element>().map_err(|err| {
        gst::error_msg!(
            gst::ResourceError::Read,
            ["Failed to parse buffer as XML: {}", err]
        )
    })?;

    Ok(root)
}

pub(crate) fn iterate_video_analytics_frames(
    root: &minidom::Element,
) -> impl Iterator<
    Item = Result<(chrono::DateTime<chrono::FixedOffset>, &minidom::Element), gst::ErrorMessage>,
> {
    root.get_child("VideoAnalytics", "http://www.onvif.org/ver10/schema")
        .map(|analytics| {
            analytics.children().filter_map(|el| {
                // We are only interested in associating Frame metadata with video frames
                if el.is("Frame", "http://www.onvif.org/ver10/schema") {
                    let timestamp = match el.attr("UtcTime") {
                        Some(timestamp) => timestamp,
                        None => {
                            return Some(Err(gst::error_msg!(
                                gst::ResourceError::Read,
                                ["Frame element has no UtcTime attribute"]
                            )));
                        }
                    };

                    let dt = match chrono::DateTime::parse_from_rfc3339(timestamp) {
                        Ok(dt) => dt,
                        Err(err) => {
                            return Some(Err(gst::error_msg!(
                                gst::ResourceError::Read,
                                ["Failed to parse UtcTime {}: {}", timestamp, err]
                            )));
                        }
                    };

                    Some(Ok((dt, el)))
                } else {
                    None
                }
            })
        })
        .into_iter()
        .flatten()
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    onvifmetadatapay::register(plugin)?;
    onvifmetadatadepay::register(plugin)?;
    onvifmetadatacombiner::register(plugin)?;
    onvifmetadataoverlay::register(plugin)?;
    onvifmetadataparse::register(plugin)?;

    gst::meta::CustomMeta::register("OnvifXMLFrameMeta", &[]);

    Ok(())
}

gst::plugin_define!(
    rsonvif,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
