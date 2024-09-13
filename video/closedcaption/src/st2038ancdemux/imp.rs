// GStreamer SMPTE ST-2038 ancillary metadata demuxer
//
// Copyright (C) 2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::UniqueFlowCombiner;

use atomic_refcell::AtomicRefCell;

use once_cell::sync::Lazy;

use std::collections::HashMap;

use crate::st2038anc_utils::AncDataHeader;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "st2038ancdemux",
        gst::DebugColorFlags::empty(),
        Some("SMPTE ST-2038 ancillary metadata demuxer"),
    )
});

pub struct St2038AncDemux {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: AtomicRefCell<State>,
}

#[derive(Default)]
struct State {
    streams: HashMap<AncDataHeader, AncStream>,
    flow_combiner: UniqueFlowCombiner,
    segment: gst::FormattedSegment<gst::ClockTime>,
    last_inactivity_check: Option<gst::ClockTime>,
}

struct AncStream {
    pad: gst::Pad,
    last_used: Option<gst::ClockTime>,
}

impl St2038AncDemux {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        let ts = buffer.dts_or_pts();
        let running_time = state.segment.to_running_time(ts);

        let anc_hdr = AncDataHeader::from_buffer(&buffer)
            .map_err(|err| {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Failed to parse ancillary data header: {err:?}"
                );
                // Just push it out on the combined pad and be done with it
                return self.srcpad.push(buffer.clone());
            })
            .unwrap();

        let stream = match state.streams.get_mut(&anc_hdr) {
            Some(stream) => stream,
            None => {
                let pad_name = format!(
                    "anc_{:02x}_{:02x}_at_{}_{}",
                    anc_hdr.did, anc_hdr.sdid, anc_hdr.line_number, anc_hdr.horizontal_offset
                );

                gst::info!(
                    CAT,
                    imp = self,
                    "New ancillary data stream {pad_name}: {anc_hdr:?}"
                );

                let anc_templ = self.obj().pad_template("anc_%02x_%02x_at_%u_%u").unwrap();
                let anc_srcpad = gst::Pad::builder_from_template(&anc_templ)
                    .name(pad_name)
                    .build();

                anc_srcpad.set_active(true).expect("set pad active");

                // Forward sticky events from sink pad to new ancillary data source pad
                // FIXME: do we want/need to modify the stream id here? caps?
                pad.sticky_events_foreach(|event| {
                    anc_srcpad.push_event(event.clone());
                    std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
                });

                self.obj().add_pad(&anc_srcpad).expect("add pad");

                state.flow_combiner.add_pad(&anc_srcpad);

                state.streams.insert(
                    anc_hdr.clone(),
                    AncStream {
                        pad: anc_srcpad,
                        last_used: running_time,
                    },
                );

                state.streams.get_mut(&anc_hdr).expect("stream")
            }
        };

        stream.last_used = running_time;

        // Clone pad, so the borrow on stream can be dropped, otherwise compiler will
        // complain that stream and state are both borrowed mutably..
        let anc_pad = stream.pad.clone();

        let anc_flow = anc_pad.push(buffer.clone());

        let _ = state.flow_combiner.update_pad_flow(&anc_pad, anc_flow);

        // Todo: Check every now and then if any ancillary streams haven't seen any data for a while
        if let Some((last_check, rt)) = Option::zip(state.last_inactivity_check, running_time) {
            if gst::ClockTime::absdiff(rt, last_check) >= gst::ClockTime::from_seconds(10) {
                // gst::fixme!(CAT, imp = self, "Check ancillary streams for inactivity");
                state.last_inactivity_check = running_time;
            }
        }

        let main_flow = self.srcpad.push(buffer);

        state.flow_combiner.update_pad_flow(&self.srcpad, main_flow)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        // Todo: clear last_seen times on ancillary src pads on stream start/flush?
        match event.view() {
            EventView::StreamStart(_) => {
                let mut state = self.state.borrow_mut();
                state.last_inactivity_check = gst::ClockTime::ZERO.into();
            }
            EventView::Segment(ev) => {
                let mut state = self.state.borrow_mut();
                state.segment = ev
                    .segment()
                    .clone()
                    .downcast::<gst::format::Time>()
                    .unwrap();
            }
            _ => {}
        }

        gst::Pad::event_default(pad, Some(&*self.obj()), event)
    }
}

impl GstObjectImpl for St2038AncDemux {}

impl ElementImpl for St2038AncDemux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "SMPTE ST-2038 ancillary metadata demuxer",
                "Metadata/Video/Demuxer",
                "Splits individual ancillary metadata streams from an SMPTE ST-2038 stream",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("meta/x-st-2038").build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            // One always pad that outputs the combined stream, and sometimes pads for each
            // ancillary data type, so people can only splice off the ones they actually need.
            let combined_src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let individual_src_pad_template = gst::PadTemplate::new(
                "anc_%02x_%02x_at_%u_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            vec![
                sink_pad_template,
                combined_src_pad_template,
                individual_src_pad_template,
            ]
        });

        PAD_TEMPLATES.as_ref()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for St2038AncDemux {
    const NAME: &'static str = "GstSt2038AncDemux";
    type Type = super::St2038AncDemux;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                St2038AncDemux::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |enc| enc.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                St2038AncDemux::catch_panic_pad_function(
                    parent,
                    || false,
                    |enc| enc.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::from_template(&templ);

        Self {
            srcpad,
            sinkpad,
            state: State::default().into(),
        }
    }
}

impl ObjectImpl for St2038AncDemux {
    fn constructed(&self) {
        self.parent_constructed();

        let mut state = self.state.borrow_mut();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();

        state.flow_combiner.add_pad(&self.srcpad);
    }
}
