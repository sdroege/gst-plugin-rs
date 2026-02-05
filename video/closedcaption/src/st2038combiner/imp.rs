// Copyright (C) 2025 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-st2038combiner
 * @title: st2038combiner
 * @short_description: Add GstAncillaryMeta to video stream given a ST-2038 stream.
 *
 * Since: plugins-rs-0.15.0
 */
use crate::st2038anc_utils::{add_ancillary_meta_to_buffer, AncData};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use std::sync::{LazyLock, Mutex, MutexGuard};

const FALLBACK_FRAME_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(50);

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "st2038combiner",
        gst::DebugColorFlags::empty(),
        Some("ST-2038 stream to GstAncillaryMeta element"),
    )
});

#[derive(Default)]
struct State {
    st2038_sinkpad: Option<gst_base::AggregatorPad>,
    current_frame_st2038: Vec<AncData>,
    last_st2038_ts: Option<gst::ClockTime>,

    current_video_buffer: Option<gst::Buffer>,
    current_video_caps: Option<gst::Caps>,
    pending_video_caps: Option<gst::Caps>,
    framerate: Option<gst::Fraction>,
    previous_video_running_time_end: Option<gst::ClockTime>,
    current_video_running_time_end: Option<gst::ClockTime>,
    current_video_running_time: Option<gst::ClockTime>,
}

pub struct St2038Combiner {
    video_sinkpad: gst_base::AggregatorPad,
    state: Mutex<State>,
}

impl ObjectImpl for St2038Combiner {
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.video_sinkpad).unwrap();
    }
}

impl GstObjectImpl for St2038Combiner {}

impl ElementImpl for St2038Combiner {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "ST-2038 Combiner",
                "Filter",
                "Combines video input stream and ST2038 stream in GstAncillaryMeta",
                "Sanchayan Maity <sanchayan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst::Caps::builder("meta/x-st-2038")
                .field("alignment", gst::List::new(["packet", "line", "frame"]))
                .build();

            let st2038_pad_template = gst::PadTemplate::with_gtype(
                "st2038",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &sink_caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::with_gtype(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            vec![st2038_pad_template, sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn release_pad(&self, pad: &gst::Pad) {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(st2038_pad) = &state.st2038_sinkpad {
                if st2038_pad == pad.downcast_ref::<gst_base::AggregatorPad>().unwrap() {
                    state.st2038_sinkpad.take();
                }
            }
        }

        self.obj().child_removed(pad, &pad.name());
        self.parent_release_pad(pad);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for St2038Combiner {
    const NAME: &'static str = "GstSt2038Combiner";
    type Type = super::St2038Combiner;
    type ParentType = gst_base::Aggregator;
    type Interfaces = (gst::ChildProxy,);

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let video_sinkpad =
            gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ).build();

        Self {
            video_sinkpad,
            state: Mutex::<State>::default(),
        }
    }
}

impl ChildProxyImpl for St2038Combiner {
    fn child_by_index(&self, index: u32) -> Option<glib::Object> {
        self.obj()
            .sink_pads()
            .get(index as usize)
            .cloned()
            .map(|pad| pad.upcast())
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        self.obj()
            .sink_pads()
            .into_iter()
            .find(|pad| pad.name() == name)
            .map(|pad| pad.upcast())
    }

    fn children_count(&self) -> u32 {
        self.obj().num_sink_pads() as u32
    }
}

impl AggregatorImpl for St2038Combiner {
    fn aggregate(&self, timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        // If we have no current video buffer, queue one. If we have one
        // but its end running time is not known yet, try to determine it
        // from the next video buffer.
        if state.current_video_buffer.is_none() || state.current_video_running_time_end.is_none() {
            let video_sinkpad = self
                .video_sinkpad
                .downcast_ref::<gst_base::AggregatorPad>()
                .unwrap();
            let video_buffer = video_sinkpad.peek_buffer();

            if video_buffer.is_none() {
                let flow_ret = if video_sinkpad.is_eos() {
                    gst::log!(CAT, imp = self, "Video pad is EOS, we're done");

                    if state.current_video_buffer.is_some() {
                        // Assume that this buffer ends where it started
                        // +50ms (20fps) and handle it.
                        state.current_video_running_time_end = state
                            .current_video_running_time_end
                            .map(|t| t + FALLBACK_FRAME_DURATION);

                        match self.collect_st2038(state, timeout) {
                            // If we collected all ST-2038 for the remaining video frame we're
                            // done, otherwise get called another time and go directly into the
                            // outer branch for finishing the current video frame.
                            Ok(true) => Ok(gst::FlowSuccess::Ok),
                            Ok(false) => Err(gst::FlowError::Eos),
                            Err(e) => Err(e),
                        }
                    } else {
                        Err(gst::FlowError::Eos)
                    }
                } else {
                    Ok(gst::FlowSuccess::Ok)
                };

                return flow_ret;
            }

            let video_buffer = video_buffer.unwrap();
            let Some(video_start_pts) = video_buffer.pts() else {
                gst::error!(CAT, imp = self, "Video buffer without PTS");
                return Err(gst::FlowError::Error);
            };

            gst::debug!(CAT, imp = self, "Video buffer with PTS: {video_start_pts}");

            let Ok(video_segment) = video_sinkpad.segment().downcast::<gst::ClockTime>() else {
                gst::error!(
                    CAT,
                    imp = self,
                    "Segment in non-TIME format, dropping video buffer"
                );
                video_sinkpad.drop_buffer();
                return Ok(gst::FlowSuccess::Ok);
            };

            let video_start = video_segment
                .to_running_time_full(video_start_pts)
                .ok_or_else(|| {
                    gst::error!(CAT, obj = video_sinkpad, "Couldn't convert to running time");
                    gst::FlowError::Error
                })?
                .positive()
                .ok_or_else(|| {
                    gst::error!(
                        CAT,
                        obj = video_sinkpad,
                        "Video buffer with negative PTS running time"
                    );
                    gst::FlowError::Error
                })?;

            if state.current_video_buffer.is_some() {
                // If we already have a video buffer just update the
                // current end running time accordingly. That's what
                // was missing and why we got here.
                state.current_video_running_time_end.replace(video_start);
                gst::log!(
                    CAT,
                    imp = self,
                    "Determined end timestamp for video buffer {:?} {:?}",
                    state.current_video_running_time,
                    state.current_video_running_time_end
                );
            } else {
                // Otherwise we had no buffer queued currently. Let's
                // do that now so that we can collect ST-2038 for it.
                state.current_video_running_time.replace(video_start);
                video_sinkpad.drop_buffer();

                let running_time_end = video_buffer
                    .duration()
                    .or_else(|| {
                        state
                            .framerate
                            .as_ref()
                            .filter(|framerate| framerate.numer() != 0)
                            .and_then(|framerate| {
                                gst::ClockTime::SECOND.mul_div_round(
                                    framerate.denom() as u64,
                                    framerate.numer() as u64,
                                )
                            })
                    })
                    .and_then(|duration| {
                        let end_time = video_start_pts + duration;
                        self.get_running_time_end(video_sinkpad, video_segment, end_time)
                            .ok()
                    });

                state.current_video_running_time_end = running_time_end;
                state.current_video_buffer.replace(video_buffer);
            }

            gst::log!(
                CAT,
                imp = self,
                "Queued new video buffer, running_time: {:?}, running_time_end: {:?}",
                state.current_video_running_time,
                state.current_video_running_time_end
            );
        }

        if state.current_video_running_time_end.is_none() {
            return Ok(gst::FlowSuccess::Ok);
        }

        // At this point we have a video buffer queued and can start
        // collecting ST-2038 buffers for it.
        assert!(state.current_video_buffer.is_some());
        assert!(state.current_video_running_time.is_some());

        self.collect_st2038(state, timeout)?;

        Ok(gst::FlowSuccess::Ok)
    }

    fn create_new_pad(
        &self,
        templ: &gst::PadTemplate,
        req_name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst_base::AggregatorPad> {
        let st2038_pad = self.parent_create_new_pad(templ, req_name, caps);
        if let Some(pad) = &st2038_pad {
            self.obj().child_added(pad, &pad.name());

            let mut state = self.state.lock().unwrap();
            state.st2038_sinkpad = Some(pad.clone());
        }

        st2038_pad
    }

    fn flush(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.reset(true);
        self.parent_flush()
    }

    fn sink_event(&self, pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Caps(ev) => {
                if self.is_video_pad(pad) {
                    let mut state = self.state.lock().unwrap();

                    let caps = ev.caps_owned();
                    let s = caps.structure(0).unwrap();
                    let framerate = s.get::<gst::Fraction>("framerate").ok();

                    let frame_duration = framerate
                        .filter(|fr| fr.numer() > 0 && fr.denom() > 0)
                        .and_then(|fr| {
                            gst::ClockTime::SECOND
                                .mul_div_round(fr.denom() as u64, fr.numer() as u64)
                        })
                        .unwrap_or(FALLBACK_FRAME_DURATION);
                    self.obj().set_latency(frame_duration, frame_duration);

                    state.framerate = framerate;

                    if state.current_video_buffer.is_some() {
                        gst::debug!(CAT, imp = self, "Storing new caps {caps:?}");
                        state.pending_video_caps = Some(caps);
                    } else {
                        state.current_video_caps = Some(caps.clone());
                        drop(state);
                        self.obj().set_src_caps(&caps);
                    }
                }
            }
            gst::EventView::Segment(e) => match e.segment().downcast_ref::<gst::ClockTime>() {
                Some(s) => self.obj().update_segment(s),
                None => {
                    gst::error!(CAT, obj = pad, "Segment in non-TIME format");
                    return false;
                }
            },
            _ => (),
        }

        self.parent_sink_event(pad, event)
    }

    fn sink_query(&self, pad: &gst_base::AggregatorPad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Position(_)
            | QueryViewMut::Duration(_)
            | QueryViewMut::Uri(_)
            | QueryViewMut::Allocation(_) => {
                if self.is_video_pad(pad) {
                    self.obj().src_pad().peer_query(query)
                } else {
                    self.parent_sink_query(pad.upcast_ref(), query)
                }
            }
            QueryViewMut::Caps(query_caps) => {
                if self.is_video_pad(pad) {
                    self.obj().src_pad().peer_query(query)
                } else {
                    let st2038_tmpl_caps = pad.pad_template_caps();
                    if let Some(filter) = query_caps.filter() {
                        let caps = st2038_tmpl_caps
                            .intersect_with_mode(filter, gst::CapsIntersectMode::First);
                        query_caps.set_result(&caps);
                    } else {
                        query_caps.set_result(&st2038_tmpl_caps);
                    }

                    true
                }
            }
            QueryViewMut::AcceptCaps(query_accept_caps) => {
                let pad_tmpl_caps = pad.pad_template_caps();
                let accept_caps = query_accept_caps.caps().is_subset(&pad_tmpl_caps);
                query_accept_caps.set_result(accept_caps);

                true
            }
            _ => self.parent_sink_query(pad.upcast_ref(), query),
        }
    }

    fn src_query(&self, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Position(_)
            | QueryViewMut::Duration(_)
            | QueryViewMut::Uri(_)
            | QueryViewMut::Allocation(_) => self.video_sinkpad.peer_query(query),
            QueryViewMut::AcceptCaps(query_accept_caps) => {
                let video_templ_caps = self.video_sinkpad.pad_template_caps();
                let accept_caps = query_accept_caps.caps().is_subset(&video_templ_caps);
                query_accept_caps.set_result(accept_caps);

                true
            }
            _ => self.parent_src_query(query),
        }
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.reset(false);
        Ok(())
    }

    fn next_time(&self) -> Option<gst::ClockTime> {
        let state = self.state.lock().unwrap();
        if state.st2038_sinkpad.is_none() {
            // No point timing out if we can't combine AncillaryMeta
            return gst::ClockTime::NONE;
        }

        let video_pad = self
            .video_sinkpad
            .downcast_ref::<gst_base::AggregatorPad>()
            .unwrap();
        let video_pad_has_buffer = video_pad.has_buffer();

        if state.current_video_buffer.is_none() && !video_pad_has_buffer {
            // No point timing out if we don't have a video buffer
            return gst::ClockTime::NONE;
        }
        drop(state);

        self.obj().simple_get_next_time()
    }

    fn negotiate(&self) -> bool {
        true
    }

    fn peek_next_sample(&self, pad: &gst_base::AggregatorPad) -> Option<gst::Sample> {
        let state = self.state.lock().unwrap();

        if self.is_st2038_pad(pad) {
            if !state.current_frame_st2038.is_empty() {
                let mut anc_data_buffer = gst::Buffer::new();
                let buffer_mut = anc_data_buffer.make_mut();

                add_ancillary_meta_to_buffer(buffer_mut, &state.current_frame_st2038);

                return Some(
                    gst::Sample::builder()
                        .buffer(&anc_data_buffer)
                        .segment(&pad.segment())
                        .caps(&pad.pad_template_caps())
                        .build(),
                );
            }
        } else if let (Some(video_buffer), Some(video_caps)) =
            (&state.current_video_buffer, &state.current_video_caps)
        {
            return Some(
                gst::Sample::builder()
                    .buffer(video_buffer)
                    .segment(&pad.segment())
                    .caps(video_caps)
                    .build(),
            );
        }

        None
    }
}

impl St2038Combiner {
    fn buffer_to_ancdata(
        &self,
        anc_data: &mut Vec<AncData>,
        buffer: gst::Buffer,
    ) -> Result<(), gst::FlowError> {
        let Ok(map) = buffer.map_readable() else {
            gst::error!(CAT, imp = self, "Failed to map buffer");
            return Err(gst::FlowError::Error);
        };

        let mut slice = map.as_slice();

        while !slice.is_empty() {
            // Stop on stuffing bytes
            if slice[0] == 0b1111_1111 {
                break;
            }

            let anc = match AncData::from_slice(slice) {
                Ok(anc) => anc,
                Err(err) => {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Dropping buffer with invalid ST-2038 data ({err})"
                    );
                    continue;
                }
            };

            slice = &slice[anc.header.len..];

            anc_data.push(anc);
        }

        Ok(())
    }

    // Only if we collected all ST-2038 we replace the current video
    // buffer with None and continue with the next one on the next call.
    fn collect_st2038<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        timeout: bool,
    ) -> Result<bool, gst::FlowError> {
        assert!(state.current_video_buffer.is_some());

        if state.st2038_sinkpad.is_none() {
            // No ST-2038 pad, forward buffer directly
            let video_buf = state.current_video_buffer.take().unwrap();
            self.collect_st2038_done(state, &video_buf);

            // Do not hold the lock when calling selected_samples and
            // finish_buffer. selected_samples can result in handlers
            // calling peek_next_sample which also needs to take the
            // lock. finish_buffer will result in a pad push.
            self.obj().selected_samples(
                video_buf.pts(),
                video_buf.dts(),
                video_buf.duration(),
                None,
            );
            self.finish_buffer(video_buf)?;

            return Ok(false);
        }

        let st2038_sinkpad = state.st2038_sinkpad.as_ref().unwrap().clone();

        loop {
            let Some(buffer) = st2038_sinkpad.peek_buffer() else {
                if st2038_sinkpad.is_eos() {
                    gst::debug!(CAT, imp = self, "ST-2038 pad is EOS, we're done");
                    break;
                } else if !state.current_frame_st2038.is_empty() {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "No more ST-2038 data for current_video_running_time_end: {:?}, collected {}",
                        state.current_video_running_time_end,
                        state.current_frame_st2038.len()
                    );
                    break;
                } else if !timeout {
                    gst::debug!(CAT, imp = self, "Need more ST-2038 data");
                    return Ok(true);
                } else {
                    gst::debug!(CAT, imp = self, "No ST-2038 data on timeout");
                    break;
                }
            };

            let Some(st2038_pts) = buffer.pts().or(state.last_st2038_ts) else {
                gst::error!(CAT, imp = self, "ST-2038 buffer without PTS");
                return Err(gst::FlowError::Error);
            };

            gst::debug!(CAT, imp = self, "ST-2038 buffer with PTS: {st2038_pts}");

            let Ok(st2038_segment) = st2038_sinkpad.segment().downcast::<gst::ClockTime>() else {
                gst::error!(
                    CAT,
                    imp = self,
                    "Segment in non-TIME format, dropping ST-2038 buffer"
                );
                st2038_sinkpad.drop_buffer();
                continue;
            };

            let st2038_time = st2038_segment
                .to_running_time_full(st2038_pts)
                .ok_or_else(|| {
                    gst::error!(
                        CAT,
                        obj = st2038_sinkpad,
                        "Couldn't convert to running time"
                    );
                    gst::FlowError::Error
                })?
                .positive()
                .ok_or_else(|| {
                    gst::error!(
                        CAT,
                        obj = st2038_sinkpad,
                        "ST-2038 buffer with negative PTS running time"
                    );
                    gst::FlowError::Error
                })?;

            gst::debug!(CAT,
                imp = self,
                "ST-2038 buffer with PTS: {st2038_time}, current_video_running_time: {:?}, current_video_running_time_end: {:?}, previous_video_running_time_end: {:?}",
                state.current_video_running_time, state.current_video_running_time_end, state.previous_video_running_time_end);

            if let Some(current_video_running_time_end) = state.current_video_running_time_end {
                if st2038_time >= current_video_running_time_end {
                    gst::debug!(
                                CAT,
                                imp = self,
                                "Collected all ST-2038 data for this video buffer, {st2038_time} >= {current_video_running_time_end}"
                            );
                    break;
                }
            } else {
                if let Some(previous_video_running_time_end) = state.previous_video_running_time_end
                {
                    if st2038_time < previous_video_running_time_end {
                        gst::debug!(
                                    CAT,
                                    imp = self,
                                    "ST-2038 buffer before end of last video frame, dropping {st2038_time} < {previous_video_running_time_end}"
                                );
                        st2038_sinkpad.drop_buffer();
                        continue;
                    }
                }

                if let Some(current_video_running_time) = state.current_video_running_time {
                    if st2038_time < current_video_running_time {
                        gst::debug!(
                                    CAT,
                                    imp = self,
                                    "ST-2038 buffer before current video frame, dropping {st2038_time} < {current_video_running_time}"
                                );
                        st2038_sinkpad.drop_buffer();
                        continue;
                    }
                }
            }

            // This ST-2038 buffer has to be collected
            st2038_sinkpad.drop_buffer();

            state.last_st2038_ts = buffer.pts();
            self.buffer_to_ancdata(&mut state.current_frame_st2038, buffer)?;

            gst::debug!(
                CAT,
                imp = self,
                "Collected {} ST-2038 buffer with PTS: {st2038_time:?} for current_video_running_time_end: {:?}",
                state.current_frame_st2038.len(),
                state.current_video_running_time_end
            );
        }

        gst::log!(
            CAT,
            imp = self,
            "Collected {} ST-2038 buffers for current_video_running_time_end: {:?}",
            state.current_frame_st2038.len(),
            state.current_video_running_time_end
        );

        // We validated the presence of a video buffer at the start
        let mut video_buf = state.current_video_buffer.take().unwrap();

        if !state.current_frame_st2038.is_empty() {
            add_ancillary_meta_to_buffer(video_buf.make_mut(), &state.current_frame_st2038);
            state.current_frame_st2038.clear();
        } else {
            gst::log!(CAT, imp = self, "No ST-2038 for video buffer");
        }

        self.collect_st2038_done(state, &video_buf);

        // Do not hold the lock when calling selected_samples and
        // finish_buffer. selected_samples can result in handlers
        // calling peek_next_sample which also needs to take the
        // lock. finish_buffer will result in a pad push.
        self.obj()
            .selected_samples(video_buf.pts(), video_buf.dts(), video_buf.duration(), None);
        self.finish_buffer(video_buf)?;

        Ok(false)
    }

    fn collect_st2038_done<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        video_buffer: &gst::Buffer,
    ) {
        self.obj().src_pad().segment().set_position(
            video_buffer
                .pts()
                .zip(video_buffer.duration())
                .map(|(pts, duration)| pts + duration),
        );

        if let Some(caps) = state.pending_video_caps.take() {
            drop(state);
            gst::debug!(CAT, imp = self, "Setting pending video caps {caps:?}");
            self.obj().set_src_caps(&caps);
            state = self.state.lock().unwrap();
        }

        let _ = state.current_video_buffer.take();
        state.previous_video_running_time_end = state.current_video_running_time_end;
        state.current_video_running_time = gst::ClockTime::NONE;
        state.current_video_running_time_end = gst::ClockTime::NONE;

        drop(state);
    }

    fn get_running_time_end(
        &self,
        sinkpad: &gst_base::AggregatorPad,
        segment: gst::FormattedSegment<gst::ClockTime>,
        end_time: gst::ClockTime,
    ) -> Result<gst::ClockTime, gst::FlowError> {
        let time_end = if let Some(stop) = segment.stop() {
            if end_time > stop {
                stop
            } else {
                end_time
            }
        } else {
            end_time
        };

        let running_time_end = segment
            .to_running_time_full(time_end)
            .ok_or_else(|| {
                gst::error!(CAT, obj = sinkpad, "Couldn't convert to running time");
                gst::FlowError::Error
            })?
            .positive()
            .ok_or_else(|| {
                gst::error!(CAT, obj = sinkpad, "Buffer with negative PTS running time");
                gst::FlowError::Error
            })?;

        Ok(running_time_end)
    }

    fn is_video_pad(&self, pad: &gst_base::AggregatorPad) -> bool {
        pad == &self.video_sinkpad
    }

    fn is_st2038_pad(&self, pad: &gst_base::AggregatorPad) -> bool {
        pad != &self.video_sinkpad
    }

    fn reset(&self, is_flush: bool) {
        if is_flush {
            self.obj()
                .src_pad()
                .segment()
                .set_position(None::<gst::ClockTime>);
        }

        let mut state = self.state.lock().unwrap();
        if !is_flush {
            let _ = state.framerate.take();
        }
        let _ = state.pending_video_caps.take();
        let _ = state.current_video_buffer.take();
        state.current_frame_st2038.clear();

        state.previous_video_running_time_end = gst::ClockTime::NONE;
        state.current_video_running_time_end = gst::ClockTime::NONE;
        state.current_video_running_time = gst::ClockTime::NONE;
        state.last_st2038_ts = gst::ClockTime::NONE;
    }
}
