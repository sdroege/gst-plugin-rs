// GStreamer APE tag demuxer
//
// Copyright (C) 2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-apetagdemux
 * @title: apetagdemux
 *
 * apetagdemux accepts data streams with APE tags at the start or at the end
 * (or both). The media type of the data between the tag blocks is detected
 * using typefind functions, and the appropriate output media type will be set
 * on the source pad before buffers are output. The tag data will be stripped off
 * from the data stream and output in form of tag events and messages.
 *
 * The element is only able to read APE tags at the end of a stream from
 * a seekable stream, ie. when get_range mode is supported by the upstream
 * elements. If get_range operation is available, apetagdemux makes it available
 * downstream. This means that elements which require get_range mode, such as
 * wavparse or musepackdec, can operate on files containing APE tag
 * information.
 *
 * ## Example launch line
 * |[
 * gst-launch-1.0 -t filesrc location=file.mpc ! apetagdemux ! fakesink -t -v
 * ]| This pipeline should read any available APE tag information and output it.
 * The contents of the file inside the APE tag regions should be detected, and
 * the appropriate mime type set on buffers produced from apetagdemux.
 *
 */
use gst::LoggableError;
use gst::{glib, subclass::prelude::*};
use gst_tag::{prelude::*, subclass::prelude::*};

use anyhow::Result;
use std::sync::LazyLock;

use crate::apedemux::apetag;
use crate::apedemux::utils;

use apetag::HEADER_FOOTER_SIZE as HEADER_SIZE;
use apetag::HEADER_FOOTER_SIZE as FOOTER_SIZE;

// Wouldn't expect more than 1-8 MB usually even if there's cover art.
const APE_TAG_MAX_ALLOWED_SIZE: usize = 32 * 1024 * 1024;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "apetagdemux",
        gst::DebugColorFlags::empty(),
        Some("APE audio tag demuxer"),
    )
});

#[derive(Default)]
pub struct ApeTagDemux;

#[glib::object_subclass]
impl ObjectSubclass for ApeTagDemux {
    const NAME: &'static str = "GstApeTagDemux";
    type Type = super::ApeTagDemux;
    type ParentType = gst_tag::TagDemux;
}

impl ObjectImpl for ApeTagDemux {}

impl GstObjectImpl for ApeTagDemux {}

impl ElementImpl for ApeTagDemux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "APE audio tag parser",
                "Codec/Demuxer/Metadata",
                "Extracts APE tags from audio streams and files",
                "Tim-Philipp Müller <tim@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_empty_simple("application/x-apetag"),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl TagDemuxImpl for ApeTagDemux {
    const MIN_START_SIZE: usize = HEADER_SIZE;
    const MIN_END_SIZE: usize = FOOTER_SIZE;

    // default merge_tags function prefers start tags over end tags which is acceptable

    // Identify tag and determine the size required to parse the tag
    fn identify_tag(&self, buffer: &gst::Buffer, start_tag: bool) -> Option<usize> {
        let position_str = if start_tag { "start" } else { "end" };

        let map = buffer.map_readable().ok().or_else(|| {
            gst::error!(CAT, imp = self, "Failed to map buffer as readable");
            None
        })?;

        let data = map.as_slice();

        gst::memdump!(CAT, imp = self, "{position_str} tag data: {data:02x?}");

        // Note: This likely means that the tag is at the other end of the file
        if !data.starts_with(b"APETAGEX") {
            gst::log!(CAT, imp = self, "No APETAGEX marker at {position_str}");
            return None;
        }

        // This would be a base class bug
        assert!(data.len() >= apetag::HEADER_FOOTER_SIZE);

        let (marker_chunk, hdr_ftr_str) = if start_tag {
            (data.first_chunk::<HEADER_SIZE>().unwrap(), "header")
        } else {
            (data.last_chunk::<FOOTER_SIZE>().unwrap(), "footer")
        };

        let marker = apetag::Marker::from_bytes(marker_chunk)
            .map_err(|e| {
                gst::warning!(CAT, imp = self, "Failed to parse APE {hdr_ftr_str}: {e:?}");
                e
            })
            .ok()?;

        // Tag size is including footer but excluding any header (for max compatibility with APEv1)
        let min_tag_size = if start_tag && !marker.global_flags.has_footer() {
            0
        } else {
            apetag::HEADER_FOOTER_SIZE
        };

        let mut tag_size = marker.size;

        if tag_size > APE_TAG_MAX_ALLOWED_SIZE || tag_size < min_tag_size {
            gst::warning!(
                CAT,
                imp = self,
                "APE tag {hdr_ftr_str} with excessive or bogus size {tag_size}, ignoring"
            );

            // Just strip the header or footer for now, and hope typefinding succeeds so the
            // broken tag can just be ignored.
            tag_size = apetag::HEADER_FOOTER_SIZE;
        }

        // Tag size is including footer but excluding any header (for max compatibility with APEv1).
        // If this is a start tag, we know the size of the header is not included and add it.
        // If this is an end tag, we will add the size of an additional header if the footer
        // indicates that there is one, to make sure the entire tag gets stripped off. Otherwise
        // we assume there isn't a header (and what else can we do without excessive probing).
        if start_tag || marker.global_flags.has_header() {
            tag_size += HEADER_SIZE;
        }

        gst::info!(
            CAT,
            imp = self,
            "Found APE {hdr_ftr_str} of {tag_size} bytes at {position_str}: {marker:?}"
        );

        Some(tag_size)
    }

    // Parse the tag once it is identified and its size is known
    fn parse_tag(
        &self,
        buffer: &gst::Buffer,
        start_tag: bool,
    ) -> Result<(usize, Option<gst::TagList>), LoggableError> {
        let position_str = if start_tag { "start" } else { "end" };

        let end_tag = !start_tag;

        let map = buffer
            .map_readable()
            .map_err(|_| gst::loggable_error!(CAT, "Failed to map buffer as readable"))?;

        let data = map.as_slice();

        gst::memdump!(CAT, imp = self, "Full {position_str} tag data: {data:02x?}");

        let len = data.len();

        gst::log!(CAT, imp = self, "Parsing {position_str} tag of {len} bytes");

        // Minimum amount of data requested from base class (and returned from identify function)
        assert!(len >= apetag::HEADER_FOOTER_SIZE);

        let header = {
            let marker_chunk = data.first_chunk::<32>().unwrap();

            match apetag::Marker::from_bytes(marker_chunk) {
                Ok(header) => {
                    gst::log!(CAT, imp = self, "{position_str} header: {header:?}");

                    Some(header)
                }

                // Error parsing the header at beginning of the stream is fatal. Having said that,
                // we already parsed the header in the identify function, so this shouldn't happen.
                Err(e) if start_tag => {
                    return Err(gst::loggable_error!(
                        CAT,
                        "Failed to parse APE tag header of a start tag: {e:?}"
                    ));
                }

                // Any errors parsing the optional header of an end tag we can just ignore
                Err(e) => {
                    if marker_chunk.starts_with(b"APETAGEX") {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Failed to parse APE tag header of an end tag: {e:?}"
                        );
                    }
                    None
                }
            }
        };

        let footer = {
            let marker_chunk = data.last_chunk::<32>().unwrap();

            match apetag::Marker::from_bytes(marker_chunk) {
                Ok(footer) => {
                    gst::log!(CAT, imp = self, "{position_str} footer: {footer:?}");

                    Some(footer)
                }

                // Error parsing the footer at end of the stream is fatal. Having said that, we
                // already parsed the footer in the identify function, so this shouldn't happen.
                Err(e) if end_tag => {
                    return Err(gst::loggable_error!(
                        CAT,
                        "Failed to parse APE tag footer of an end tag: {e:?}"
                    ));
                }

                // Any errors parsing the optional footer of a start tag we can just ignore
                Err(e) => {
                    if marker_chunk.starts_with(b"APETAGEX") {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Failed to parse APE tag footer of a start tag: {e:?}"
                        );
                    }
                    None
                }
            }
        };

        // Consistency check, though not sure how valuable this is in practice
        if let Some((hdr, ftr)) = Option::zip(header.as_ref(), footer.as_ref())
            && hdr != ftr
        {
            gst::warning!(
                CAT,
                imp = self,
                "APE tag header and footer details differ: {hdr:?} vs. {ftr:?}"
            );
        }

        let tag = if start_tag {
            header.as_ref()
        } else {
            footer.as_ref()
        }
        .expect("tag");

        // Also remove footer from data if it's signalled to exist but we failed to parse it
        let footer_size = if footer.is_some() || (start_tag && tag.has_footer()) {
            FOOTER_SIZE
        } else {
            0
        };

        let data = {
            // Also skip header if it's signalled to exist but we failed to parse it
            let start_offset = if header.is_some() || (end_tag && tag.has_header()) {
                HEADER_SIZE
            } else {
                0
            };

            // If header == footer (i.e. data length is 32) this could underflow
            let tag_data_len = len.saturating_sub(start_offset + footer_size);

            &data[start_offset..][..tag_data_len]
        };

        let mut tag_list = gst::TagList::new();
        let tags = tag_list.make_mut();

        for apetag::Item { key, value } in tag.items(data) {
            use gst::TagMergeMode;
            use gst_tag::{acoustid, cdda, musicbrainz};

            gst::log!(CAT, imp = self, "{key} = {value:?}");

            let lc_key = key.to_lowercase();

            // We use MergeMode::Replace for unique IDs and tags where multiple
            // values don't make sense, otherwise we use MergeMode::Append.
            match value {
                apetag::ItemValue::Text("") => {
                    gst::debug!(CAT, imp = self, "Empty string value for text tag {lc_key}");
                }

                apetag::ItemValue::Text(string) => match lc_key.as_str() {
                    "acoustid_fingerprint" => {
                        tags.add::<acoustid::Fingerprint>(&string, TagMergeMode::Replace);
                    }

                    "acoustid_id" => {
                        tags.add::<acoustid::Id>(&string, TagMergeMode::Replace);
                    }

                    "artist" => {
                        tags.add::<gst::tags::Artist>(&string, TagMergeMode::Append);
                    }

                    "artistsort" => {
                        tags.add::<gst::tags::ArtistSortname>(&string, TagMergeMode::Append);
                    }

                    "album" => {
                        tags.add::<gst::tags::Album>(&string, TagMergeMode::Append);
                    }

                    "album artist" | "albumartist" | "album_artist" => {
                        tags.add::<gst::tags::AlbumArtist>(&string, TagMergeMode::Append);
                    }

                    "albumartistsort" => {
                        tags.add::<gst::tags::AlbumArtistSortname>(&string, TagMergeMode::Append);
                    }

                    "bpm" => {
                        if let Ok(utils::FloatFromString { value: bpm }) =
                            string.parse::<utils::FloatFromString>()
                            && bpm > 0.0
                        {
                            tags.add::<gst::tags::BeatsPerMinute>(&bpm, TagMergeMode::Append);
                        } else {
                            gst::debug!(CAT, "Couldn't parse bpm string '{string}' into number");
                        }
                    }

                    "comment" | "comments" => {
                        tags.add::<gst::tags::Comment>(&string, TagMergeMode::Append);
                    }

                    "composer" => {
                        tags.add::<gst::tags::Composer>(&string, TagMergeMode::Append);
                    }

                    "copyright" => {
                        tags.add::<gst::tags::Copyright>(&string, TagMergeMode::Append);
                    }

                    "disc" | "disk" | "discnumber" | "disknumber" | "media" => {
                        // media tag might be e.g. "CD 1/2", need to strip off the "CD" prefix
                        let (_prefix, string) = match string.rsplit_once(' ') {
                            None => ("", string),
                            Some((p, s)) => (p, s),
                        };

                        if let Ok(utils::NumberWithCount { number, count }) =
                            string.parse::<utils::NumberWithCount>()
                        {
                            if let Some(number) = number {
                                tags.add::<gst::tags::AlbumVolumeNumber>(
                                    &number.get(),
                                    TagMergeMode::Replace,
                                );
                            }
                            if let Some(count) = count {
                                tags.add::<gst::tags::AlbumVolumeCount>(
                                    &count.get(),
                                    TagMergeMode::Replace,
                                );
                            }
                        } else {
                            gst::debug!(CAT, imp = self, "Failed to parse number/count '{string}'");
                        }
                    }

                    "discc" | "disctotal" | "totaldiscs" => {
                        if let Ok(count) = string.parse::<std::num::NonZeroU32>() {
                            tags.add::<gst::tags::AlbumVolumeCount>(
                                &count.get(),
                                TagMergeMode::Replace,
                            );
                        }
                    }

                    "encoder" | "encodedby" => {
                        tags.add::<gst::tags::Encoder>(&string, TagMergeMode::Append);
                    }

                    "track" | "tracknumber" => {
                        // Strip off any prefix, like "Track 5/10" (pure precaution, not seen)
                        let (_prefix, string) = match string.rsplit_once(' ') {
                            None => ("", string),
                            Some((p, s)) => (p, s),
                        };

                        if let Ok(utils::NumberWithCount { number, count }) =
                            string.parse::<utils::NumberWithCount>()
                        {
                            if let Some(number) = number {
                                tags.add::<gst::tags::TrackNumber>(
                                    &number.get(),
                                    TagMergeMode::Replace,
                                );
                            }
                            if let Some(count) = count {
                                tags.add::<gst::tags::TrackCount>(
                                    &count.get(),
                                    TagMergeMode::Replace,
                                );
                            }
                        } else {
                            gst::debug!(CAT, imp = self, "Failed to parse number/count '{string}'");
                        }
                    }

                    "trackc" | "tracktotal" | "totaltracks" => {
                        if let Ok(count) = string.parse::<std::num::NonZeroU32>() {
                            tags.add::<gst::tags::TrackCount>(&count.get(), TagMergeMode::Replace);
                        }
                    }

                    "genre" => {
                        tags.add::<gst::tags::Genre>(&string, TagMergeMode::Replace);
                    }

                    "isrc" => {
                        tags.add::<gst::tags::ISRC>(&string, TagMergeMode::Replace);
                    }

                    "lyrics" => {
                        tags.add::<gst::tags::Lyrics>(&string, TagMergeMode::Replace);
                    }

                    "replaygain_track_gain"
                    | "replaygain_track_peak"
                    | "replaygain_album_gain"
                    | "replaygain_album_peak" => {
                        if let Ok(utils::FloatFromString { value: num }) =
                            string.replace(" dB", "").parse::<utils::FloatFromString>()
                        {
                            match lc_key.as_str() {
                                "replaygain_track_gain" => {
                                    tags.add::<gst::tags::TrackGain>(&num, TagMergeMode::Replace);
                                }
                                "replaygain_track_peak" => {
                                    tags.add::<gst::tags::TrackPeak>(&num, TagMergeMode::Replace);
                                }
                                "replaygain_album_gain" => {
                                    tags.add::<gst::tags::AlbumGain>(&num, TagMergeMode::Replace);
                                }
                                "replaygain_album_peak" => {
                                    tags.add::<gst::tags::AlbumPeak>(&num, TagMergeMode::Replace);
                                }
                                _ => unreachable!(),
                            }
                        } else {
                            gst::debug!(CAT, imp = self, "Failed to parse '{string}' as float");
                        }
                    }

                    "musicbrainz_trackid" => {
                        tags.add::<musicbrainz::TrackId>(&string, TagMergeMode::Replace);
                    }
                    "musicbrainz_artistid" => {
                        tags.add::<musicbrainz::ArtistId>(&string, TagMergeMode::Replace);
                    }
                    "musicbrainz_albumid" => {
                        tags.add::<musicbrainz::AlbumId>(&string, TagMergeMode::Replace);
                    }
                    "musicbrainz_albumartistid" => {
                        tags.add::<musicbrainz::AlbumArtistId>(&string, TagMergeMode::Replace);
                    }
                    "musicbrainz_releasegroupid" => {
                        tags.add::<musicbrainz::ReleaseGroupId>(&string, TagMergeMode::Replace);
                    }
                    "musicbrainz_releasetrackid" => {
                        tags.add::<musicbrainz::ReleaseTrackId>(&string, TagMergeMode::Replace);
                    }
                    "musicbrainz_trmid" => {
                        tags.add::<musicbrainz::TrmId>(&string, TagMergeMode::Replace);
                    }
                    "musicbrainz_discid" => {
                        tags.add::<cdda::MusicbrainzDiscid>(&string, TagMergeMode::Replace);
                    }

                    "title" => {
                        tags.add::<gst::tags::Title>(&string, TagMergeMode::Append);
                    }

                    "year" => {
                        if let Ok(datetime) = gst::DateTime::from_iso8601_string(string) {
                            tags.add::<gst::tags::DateTime>(&datetime, TagMergeMode::Replace);
                        } else {
                            gst::debug!(CAT, imp = self, "Failed to parse year '{string}'");
                        }
                    }

                    _ => {
                        gst::debug!(CAT, imp = self, "No mapping for APE text tag '{key}'");
                        continue;
                    }
                },

                apetag::ItemValue::Binary(data) => match lc_key.as_str() {
                    "cover art (front)" | "cover art (back)" => {
                        let image_type = match lc_key.as_str() {
                            "cover art (front)" => gst_tag::TagImageType::FrontCover,
                            "cover art (back)" => gst_tag::TagImageType::BackCover,
                            _ => unreachable!(),
                        };

                        // Extract zero-terminated filename string before image data
                        let Some(terminator_pos) = data.iter().position(|&b| b == b'\0') else {
                            gst::warning!(CAT, "Missing {lc_key} filename terminator");
                            continue;
                        };

                        // split_once() would probably be nicer, but it's still nightly API as of October 2026
                        let (fn_data, image_data) = data.split_at(terminator_pos);

                        // Skip filename nul terminator
                        let image_data = &image_data[1..];

                        gst::memdump!(CAT, imp = self, "image_data: {image_data:02x?}");

                        match gst::Sample::from_image_data(image_data, image_type) {
                            Ok(mut sample) => {
                                // Attach filename to image sample
                                if let Ok(filename) = str::from_utf8(fn_data)
                                    && !filename.is_empty()
                                {
                                    gst::log!(CAT, "Image filename: {filename:?}");

                                    let mut info = match sample.info() {
                                        None => gst::Structure::new_empty("GstTagImageInfo"),
                                        Some(info_ref) => info_ref.to_owned(),
                                    };
                                    info.set("filename", filename);
                                    sample.get_mut().unwrap().set_info(Some(info));
                                }

                                // Add image sample to taglist
                                tags.add::<gst::tags::Image>(&sample, TagMergeMode::Append);
                            }
                            Err(e) => {
                                gst::debug!(
                                    CAT,
                                    imp = self,
                                    "Couldn't create image from data: {e}"
                                );
                            }
                        }
                    }

                    _ => {
                        gst::debug!(CAT, imp = self, "No mapping for APE binary tag '{key}'");
                        continue;
                    }
                },

                apetag::ItemValue::Locator(string) => match lc_key.as_str() {
                    "file" => {
                        tags.add::<gst::tags::Location>(&string, TagMergeMode::Append);
                    }

                    _ => {
                        gst::debug!(CAT, imp = self, "No mapping for APE location tag '{key}'");
                        continue;
                    }
                },

                _ => {}
            }
        }

        tags.add::<gst::tags::ContainerFormat>(&"APE tag", gst::TagMergeMode::Append);

        gst::info!(
            CAT,
            imp = self,
            "Tags found at {position_str}: {tag_list:?}"
        );

        Ok((buffer.size(), Some(tag_list)))
    }
}
