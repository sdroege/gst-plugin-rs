// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{element_error, error_msg, gst_error, gst_log, gst_trace};
use serde_derive::Deserialize;

use once_cell::sync::Lazy;

use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "awstranscribeparse",
        gst::DebugColorFlags::empty(),
        Some("AWS transcript parser"),
    )
});

struct State {
    adapter: gst_base::UniqueAdapter,
}

impl Default for State {
    fn default() -> Self {
        Self {
            adapter: gst_base::UniqueAdapter::new(),
        }
    }
}

unsafe impl Send for State {}

pub struct TranscribeParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
struct Alternative {
    confidence: serde_json::Value,
    content: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
struct Item {
    start_time: Option<String>,
    end_time: Option<String>,
    alternatives: Vec<Alternative>,
    #[serde(rename = "type")]
    type_: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Results {
    transcripts: serde_json::Value,
    items: Vec<Item>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Transcript {
    job_name: String,
    account_id: String,
    results: Results,
}

impl TranscribeParse {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        _element: &super::TranscribeParse,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();

        state.adapter.push(buffer);

        Ok(gst::FlowSuccess::Ok)
    }

    fn drain(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        let available = state.adapter.available();
        let buffer = state
            .adapter
            .take_buffer(available)
            .unwrap()
            .into_mapped_buffer_readable()
            .unwrap();
        drop(state);

        self.srcpad.push_event(gst::event::Caps::new(
            &gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build(),
        ));
        self.srcpad
            .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::format::Time,
            >::new()));
        let json = std::str::from_utf8(buffer.as_slice()).map_err(|err| {
            error_msg!(
                gst::StreamError::Failed,
                ["Couldn't parse input as utf8: {}", err]
            )
        })?;
        let mut transcript: Transcript = serde_json::from_str(json).map_err(|err| {
            error_msg!(
                gst::StreamError::Failed,
                ["Unexpected transcription format: {}", err]
            )
        })?;

        let mut last_pts: gst::ClockTime = gst::ClockTime::from_nseconds(0);

        for mut item in transcript.results.items.drain(..) {
            match item.type_.as_str() {
                "punctuation" => {
                    if !item.alternatives.is_empty() {
                        let alternative = item.alternatives.remove(0);
                        let mut outbuf =
                            gst::Buffer::from_mut_slice(alternative.content.into_bytes());

                        {
                            let outbuf = outbuf.get_mut().unwrap();

                            outbuf.set_pts(last_pts);
                            outbuf.set_duration(gst::ClockTime::from_nseconds(0));
                        }

                        self.srcpad.push(outbuf).map_err(|err| {
                            error_msg!(
                                gst::StreamError::Failed,
                                ["Failed to push transcript item: {}", err]
                            )
                        })?;
                    }
                }
                "pronunciation" => {
                    let start_time: f64 = match item.start_time.as_ref().unwrap().parse() {
                        Ok(start_time) => start_time,
                        Err(err) => {
                            return Err(error_msg!(
                                gst::StreamError::Failed,
                                ["Failed to parse start_time as float ({})", err]
                            ));
                        }
                    };
                    let end_time: f64 = match item.end_time.as_ref().unwrap().parse() {
                        Ok(end_time) => end_time,
                        Err(err) => {
                            return Err(error_msg!(
                                gst::StreamError::Failed,
                                ["Failed to parse end_time as float ({})", err]
                            ));
                        }
                    };

                    let start_pts =
                        gst::ClockTime::from_nseconds((start_time as f64 * 1_000_000_000.0) as u64);
                    let end_pts =
                        gst::ClockTime::from_nseconds((end_time as f64 * 1_000_000_000.0) as u64);
                    let duration = end_pts.saturating_sub(start_pts);

                    if start_pts > last_pts {
                        let gap_event = gst::event::Gap::builder(last_pts)
                            .duration(start_pts - last_pts)
                            .build();
                        if !self.srcpad.push_event(gap_event) {
                            return Err(error_msg!(
                                gst::StreamError::Failed,
                                ["Failed to push gap"]
                            ));
                        }
                    }

                    if !(item.alternatives.is_empty()) {
                        let alternative = item.alternatives.remove(0);
                        let mut outbuf =
                            gst::Buffer::from_mut_slice(alternative.content.into_bytes());

                        {
                            let outbuf = outbuf.get_mut().unwrap();

                            outbuf.set_pts(start_pts);
                            outbuf.set_duration(duration);
                        }

                        self.srcpad.push(outbuf).map_err(|err| {
                            error_msg!(
                                gst::StreamError::Failed,
                                ["Failed to push transcript item: {}", err]
                            )
                        })?;

                        last_pts = end_pts;
                    }
                }
                _ => unreachable!(),
            }
        }

        Ok(())
    }

    fn sink_event(
        &self,
        pad: &gst::Pad,
        element: &super::TranscribeParse,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                pad.event_default(Some(element), event)
            }
            EventView::Eos(..) => match self.drain() {
                Ok(()) => pad.event_default(Some(element), event),
                Err(err) => {
                    gst_error!(CAT, obj: element, "failed to drain on EOS: {}", err);
                    element_error!(
                        element,
                        gst::StreamError::Failed,
                        ["Streaming failed: {}", err]
                    );

                    false
                }
            },
            EventView::Segment(..) | EventView::Caps(..) => true,
            _ => pad.event_default(Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TranscribeParse {
    const NAME: &'static str = "RsAWSTranscribeParse";
    type Type = super::TranscribeParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                TranscribeParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |parse, element| parse.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TranscribeParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src")).build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for TranscribeParse {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for TranscribeParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "AWS transcript parser",
                "Text/Subtitle",
                "Parses AWS transcripts into timed text buffers",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/x-json").build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady => {
                // Reset the whole state
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}
