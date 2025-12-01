// Copyright (C) 2025 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-st2038extractor
 * @title: st2038extractor
 * @short_description: Extracts GstAncillaryMeta from video stream and gives ST-2038 stream.
 *
 * Since: plugins-rs-0.15.0
 */
use crate::st2038anc_utils::to_st2038_with_10bit;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::UniqueFlowCombiner;
use gst_video::video_meta::AncillaryMeta;
use std::sync::LazyLock;
use std::sync::{Mutex, MutexGuard};

const DEFAULT_REMOVE_ANCILLARY_META: bool = false;
const DEFAULT_ALWAYS_ADD_ST2038_PAD: bool = false;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "st2038extractor",
        gst::DebugColorFlags::empty(),
        Some("GstAncillaryMeta to ST-2038 stream element"),
    )
});

#[derive(Debug, Default, Clone)]
struct Settings {
    remove_ancillary_meta: bool,
    always_add_st2038_pad: bool,
}

pub struct St2038Extractor {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

#[derive(Default)]
struct State {
    st2038_srcpad: Option<gst::Pad>,
    flow_combiner: UniqueFlowCombiner,
}

fn create_st2038_stream_start_event(ev: &gst::event::StreamStart) -> gst::Event {
    gst::event::StreamStart::builder(format!("{}/st2038", ev.stream_id()).as_str())
        .seqnum(ev.seqnum())
        .build()
}

fn create_st2038_caps_event(ev: &gst::event::Caps, srcpad: &gst::Pad) -> gst::Event {
    let caps = ev.caps();
    let s = caps.structure(0).unwrap();
    let framerate = s.get::<gst::Fraction>("framerate").ok();

    let mut src_caps = srcpad.pad_template_caps();
    if let Some(framerate) = framerate {
        src_caps.make_mut().set("framerate", framerate);
    }

    gst::event::Caps::builder(&src_caps)
        .seqnum(ev.seqnum())
        .build()
}

impl St2038Extractor {
    fn create_st2038_srcpad<'a>(&'a self, mut state: MutexGuard<'a, State>) -> gst::Pad {
        let (pad, state) = match &state.st2038_srcpad {
            Some(pad) => (pad.clone(), state),
            None => {
                let st2038_templ = self.obj().pad_template("st2038").unwrap();
                let st2038_srcpad = gst::Pad::builder_from_template(&st2038_templ)
                    .name("st2038")
                    .query_function(move |pad, parent, query| {
                        St2038Extractor::catch_panic_pad_function(
                            parent,
                            || false,
                            |this| this.st2038_src_query(pad, query),
                        )
                    })
                    .build();
                st2038_srcpad.set_active(true).expect("set pad active");

                self.sinkpad.sticky_events_foreach(|event| {
                    use gst::EventView;

                    match event.view() {
                        EventView::Caps(ev) => {
                            let cev = create_st2038_caps_event(ev, &st2038_srcpad);
                            let _ = st2038_srcpad.store_sticky_event(&cev);
                        }
                        EventView::StreamStart(ev) => {
                            let sev = create_st2038_stream_start_event(ev);
                            let _ = st2038_srcpad.store_sticky_event(&sev);
                        }
                        _ => {
                            let _ = st2038_srcpad.store_sticky_event(event);
                        }
                    }

                    std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
                });

                drop(state);

                self.obj().add_pad(&st2038_srcpad).expect("add pad");

                state = self.state.lock().unwrap();

                state.st2038_srcpad = Some(st2038_srcpad.clone());
                state.flow_combiner.add_pad(&st2038_srcpad);

                (st2038_srcpad, state)
            }
        };

        drop(state);

        pad
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, imp = self, "Handling buffer {buffer:?}");

        // Sort them by line and offset so that line alignment can also
        // be properly preserved.
        let mut ancillary_metas = Vec::new();
        for meta in buffer.iter_meta::<AncillaryMeta>() {
            gst::trace!(CAT, imp = self, "Have ancillary meta {:?}", meta);
            ancillary_metas.push(meta);
        }
        ancillary_metas.sort_by_key(|meta| (meta.line(), meta.offset()));

        gst::debug!(
            CAT,
            imp = self,
            "Have {:?} Ancillary metas",
            ancillary_metas.len()
        );

        // Maximum size of one ST-2038 packet can be 328.
        let mut st2038_buffer = Vec::<u8>::with_capacity(ancillary_metas.len() * 328);
        for meta in ancillary_metas {
            if let Err(err) = to_st2038_with_10bit(&mut st2038_buffer, &meta) {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed to convert AncillaryMeta to ST2038, {err:?}"
                );
                continue;
            };
        }

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap().clone();

        if !st2038_buffer.is_empty() {
            let mut st2038_outbuf = gst::Buffer::from_mut_slice(st2038_buffer);

            {
                let outbuf = st2038_outbuf.make_mut();
                buffer
                    .copy_into(outbuf, gst::BUFFER_COPY_METADATA, ..)
                    .unwrap();
            }

            let st2038_srcpad = self.create_st2038_srcpad(state);

            let flow_ret = st2038_srcpad.push(st2038_outbuf);

            state = self.state.lock().unwrap();
            state
                .flow_combiner
                .update_pad_flow(&st2038_srcpad, flow_ret)?;
            drop(state);
        } else if let Some(pts) = buffer.pts() {
            let st2038_srcpad = if settings.always_add_st2038_pad {
                Some(self.create_st2038_srcpad(state))
            } else {
                let st2038_srcpad = state.st2038_srcpad.clone();
                drop(state);
                st2038_srcpad
            };

            if let Some(st2038_srcpad) = st2038_srcpad {
                let gap = gst::event::Gap::builder(pts)
                    .duration(buffer.duration())
                    .build();

                let _ = st2038_srcpad.push_event(gap);
            }
        }

        if settings.remove_ancillary_meta {
            let buffer = buffer.make_mut();
            buffer.foreach_meta_mut(|meta| {
                use std::ops::ControlFlow::*;
                if meta.api() == AncillaryMeta::meta_api() {
                    return Continue(gst::buffer::BufferMetaForeachAction::Remove);
                }
                Continue(gst::buffer::BufferMetaForeachAction::Keep)
            });
        }

        let flow_ret = self.srcpad.push(buffer);

        state = self.state.lock().unwrap();
        state.flow_combiner.update_pad_flow(&self.srcpad, flow_ret)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        let state = self.state.lock().unwrap();
        let st2038_srcpad = state.st2038_srcpad.clone();
        drop(state);

        if let Some(srcpad) = st2038_srcpad {
            match event.view() {
                EventView::Caps(ev) => {
                    let _ = srcpad.push_event(create_st2038_caps_event(ev, &srcpad));
                }
                EventView::StreamStart(ev) => {
                    let _ = srcpad.push_event(create_st2038_stream_start_event(ev));
                }
                _ => {
                    // Forward all other events to the ST2038 pad if present
                    srcpad.push_event(event.clone());
                }
            }
        }

        // This only forwards to the non-ST2038 source pad
        self.srcpad.push_event(event)
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        gst::log!(CAT, obj = pad, "Handling query {:?}", query);

        if let QueryViewMut::AcceptCaps(q) = query.view_mut() {
            q.set_result(true);
            return true;
        }

        self.srcpad.peer_query(query)
    }

    fn st2038_src_query(&self, srcpad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Caps(query_caps) => {
                let sink_tmpl_caps = self.sinkpad.pad_template_caps();
                let peer_caps = self.sinkpad.peer_query_caps(Some(&sink_tmpl_caps));

                if peer_caps.is_empty() {
                    query_caps.set_result(&peer_caps);
                    return true;
                }

                let mut ret_caps = gst::Caps::new_empty();
                let caps = ret_caps.get_mut().unwrap();

                for s in peer_caps.iter() {
                    if let Ok(framerate) = s.value("framerate") {
                        caps.append(
                            gst::Caps::builder("meta/x-st-2038")
                                .field("framerate", framerate.to_owned())
                                .build(),
                        );
                    }
                }

                // If the peer caps contain no frame rate
                if caps.is_empty() {
                    ret_caps = srcpad.pad_template_caps();
                }

                if let Some(filter) = query_caps.filter() {
                    ret_caps = ret_caps.intersect_with_mode(filter, gst::CapsIntersectMode::First);
                }

                query_caps.set_result(&ret_caps);

                true
            }
            _ => gst::Pad::query_default(&self.sinkpad, Some(&*self.obj()), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for St2038Extractor {
    const NAME: &'static str = "GstSt2038Extractor";
    type Type = super::St2038Extractor;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                St2038Extractor::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this| this.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                St2038Extractor::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| this.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                St2038Extractor::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| this.sink_query(pad, query),
                )
            })
            .flags(
                gst::PadFlags::PROXY_CAPS
                    | gst::PadFlags::PROXY_ALLOCATION
                    | gst::PadFlags::PROXY_SCHEDULING
                    | gst::PadFlags::ACCEPT_TEMPLATE,
            )
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::PadBuilder::<gst::Pad>::from_template(&templ)
            .flags(
                gst::PadFlags::PROXY_CAPS
                    | gst::PadFlags::PROXY_ALLOCATION
                    | gst::PadFlags::PROXY_SCHEDULING,
            )
            .build();

        let state = Mutex::<State>::default();
        {
            let mut state = state.lock().unwrap();
            state.flow_combiner.add_pad(&srcpad);
        }

        Self {
            sinkpad,
            srcpad,
            state,
            settings: Mutex::default(),
        }
    }
}

impl ObjectImpl for St2038Extractor {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("remove-ancillary-meta")
                    .nick("Remove Ancillary Meta")
                    .blurb("Remove ancillary meta from outgoing video buffers")
                    .default_value(DEFAULT_REMOVE_ANCILLARY_META)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("always-add-st2038-pad")
                    .nick("Always add ST2038 pad")
                    .blurb("Always add the ST2038 pad even if not ancillary data was received yet")
                    .default_value(DEFAULT_ALWAYS_ADD_ST2038_PAD)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "remove-ancillary-meta" => {
                let mut settings = self.settings.lock().unwrap();
                settings.remove_ancillary_meta = value.get().expect("type checked upstream");
            }
            "always-add-st2038-pad" => {
                let mut settings = self.settings.lock().unwrap();
                settings.always_add_st2038_pad = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "remove-ancillary-meta" => settings.remove_ancillary_meta.to_value(),
            "always-add-st2038-pad" => settings.always_add_st2038_pad.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for St2038Extractor {}

impl ElementImpl for St2038Extractor {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GstAncillaryMeta to ST-2038",
                "Generic",
                "Extracts ST2038 stream in GstAncillaryMeta from video input stream",
                "Sanchayan Maity <sanchayan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps_aligned = gst::Caps::builder("meta/x-st-2038")
                .field("alignment", "frame")
                .build();
            let anc_src_pad_template = gst::PadTemplate::new(
                "st2038",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps_aligned,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![anc_src_pad_template, src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();
                state.flow_combiner.reset();
            }
            _ => (),
        }

        let ret = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                let st2038_srcpad = state.st2038_srcpad.take();

                if let Some(pad) = st2038_srcpad {
                    state.flow_combiner.remove_pad(&pad);
                    drop(state);
                    let _ = self.obj().remove_pad(&pad);
                }
            }
            _ => (),
        }

        Ok(ret)
    }
}
