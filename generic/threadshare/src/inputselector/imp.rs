// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
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

use futures::future::BoxFuture;
use futures::future::{abortable, AbortHandle};
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_log, gst_trace};

use once_cell::sync::Lazy;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::u32;

use crate::runtime::prelude::*;
use crate::runtime::{self, PadSink, PadSinkRef, PadSrc, PadSrcRef};

const DEFAULT_CONTEXT: &str = "";
// FIXME use Duration::ZERO when MSVC >= 1.53.2
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_nanos(0);

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

#[derive(Debug)]
struct InputSelectorPadSinkHandlerInner {
    segment: Option<gst::Segment>,
    send_sticky: bool,
    abort_handle: Option<AbortHandle>,
}

impl Default for InputSelectorPadSinkHandlerInner {
    fn default() -> Self {
        InputSelectorPadSinkHandlerInner {
            segment: None,
            send_sticky: true,
            abort_handle: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct InputSelectorPadSinkHandler(Arc<Mutex<InputSelectorPadSinkHandlerInner>>);

impl InputSelectorPadSinkHandler {
    /* Wait until specified time */
    async fn sync(
        &self,
        element: &super::InputSelector,
        running_time: impl Into<Option<gst::ClockTime>>,
    ) {
        let now = element.current_running_time();

        match running_time
            .into()
            .zip(now)
            .and_then(|(running_time, now)| running_time.checked_sub(now))
        {
            Some(delay) => runtime::time::delay_for(delay.into()).await,
            None => runtime::executor::yield_now().await,
        }
    }

    async fn handle_item(
        &self,
        pad: &PadSinkRef<'_>,
        element: &super::InputSelector,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let inputselector = InputSelector::from_instance(element);

        let (stickies, is_active, sync_future, switched_pad) = {
            let mut state = inputselector.state.lock().unwrap();
            let mut inner = self.0.lock().unwrap();
            let mut stickies = vec![];
            let mut sync_future = None;
            let switched_pad = state.switched_pad;

            if let Some(segment) = &inner.segment {
                if let Some(segment) = segment.downcast_ref::<gst::format::Time>() {
                    let rtime = segment.to_running_time(buffer.pts());
                    let (sync_fut, abort_handle) = abortable(self.sync(&element, rtime));
                    inner.abort_handle = Some(abort_handle);
                    sync_future = Some(sync_fut.map_err(|_| gst::FlowError::Flushing));
                }
            }

            let is_active = {
                if state.active_sinkpad.as_ref() == Some(pad.gst_pad()) {
                    if inner.send_sticky || state.switched_pad {
                        pad.gst_pad().sticky_events_foreach(|event| {
                            stickies.push(event.clone());
                            Ok(Some(event))
                        });

                        inner.send_sticky = false;
                        state.switched_pad = false;
                    }
                    true
                } else {
                    false
                }
            };

            (stickies, is_active, sync_future, switched_pad)
        };

        if let Some(sync_fut) = sync_future {
            sync_fut.await?;
        }

        for event in stickies {
            inputselector.src_pad.push_event(event).await;
        }

        if is_active {
            gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", buffer);

            if switched_pad && !buffer.flags().contains(gst::BufferFlags::DISCONT) {
                let buffer = buffer.make_mut();
                buffer.set_flags(gst::BufferFlags::DISCONT);
            }

            inputselector.src_pad.push(buffer).await
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

impl PadSinkHandler for InputSelectorPadSinkHandler {
    type ElementImpl = InputSelector;

    fn sink_chain(
        &self,
        pad: &PadSinkRef,
        _inputselector: &InputSelector,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let this = self.clone();
        let element = element.clone().downcast::<super::InputSelector>().unwrap();
        let pad_weak = pad.downgrade();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            this.handle_item(&pad, &element, buffer).await
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        pad: &PadSinkRef,
        _inputselector: &InputSelector,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let this = self.clone();
        let element = element.clone().downcast::<super::InputSelector>().unwrap();
        let pad_weak = pad.downgrade();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(CAT, obj: pad.gst_pad(), "Handling buffer list {:?}", list);
            // TODO: Ideally we would keep the list intact and forward it in one go
            for buffer in list.iter_owned() {
                this.handle_item(&pad, &element, buffer).await?;
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_event_serialized(
        &self,
        _pad: &PadSinkRef,
        _inputselector: &InputSelector,
        _element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        let this = self.clone();

        async move {
            let mut inner = this.0.lock().unwrap();

            // Remember the segment for later use
            if let gst::EventView::Segment(e) = event.view() {
                inner.segment = Some(e.segment().clone());
            }

            // We sent sticky events together with the next buffer once it becomes
            // the active pad.
            //
            // TODO: Other serialized events for the active pad can also be forwarded
            // here, and sticky events could be forwarded directly. Needs forwarding of
            // all other sticky events first!
            if event.is_sticky() {
                inner.send_sticky = true;
                true
            } else {
                true
            }
        }
        .boxed()
    }

    fn sink_event(
        &self,
        _pad: &PadSinkRef,
        inputselector: &InputSelector,
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        /* Drop all events for now */
        if let gst::EventView::FlushStart(..) = event.view() {
            /* Unblock downstream */
            inputselector.src_pad.gst_pad().push_event(event.clone());

            let mut inner = self.0.lock().unwrap();

            if let Some(abort_handle) = inner.abort_handle.take() {
                abort_handle.abort();
            }
        }
        true
    }

    fn sink_query(
        &self,
        pad: &PadSinkRef,
        inputselector: &InputSelector,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling query {:?}", query);

        if query.is_serialized() {
            // FIXME: How can we do this (drops ALLOCATION and DRAIN)?
            gst_log!(CAT, obj: pad.gst_pad(), "Dropping serialized query {:?}", query);
            false
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Forwarding query {:?}", query);
            inputselector.src_pad.gst_pad().peer_query(query)
        }
    }
}

#[derive(Clone, Debug)]
struct InputSelectorPadSrcHandler;

impl PadSrcHandler for InputSelectorPadSrcHandler {
    type ElementImpl = InputSelector;

    fn src_query(
        &self,
        pad: &PadSrcRef,
        inputselector: &InputSelector,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut ret = true;
                let mut min_latency = gst::ClockTime::ZERO;
                let mut max_latency = gst::ClockTime::NONE;
                let pads = {
                    let pads = inputselector.pads.lock().unwrap();
                    pads.sink_pads
                        .iter()
                        .map(|p| p.0.clone())
                        .collect::<Vec<_>>()
                };

                for pad in pads {
                    let mut peer_query = gst::query::Latency::new();

                    ret = pad.peer_query(&mut peer_query);

                    if ret {
                        let (live, min, max) = peer_query.result();
                        if live {
                            min_latency = min.max(min_latency);
                            max_latency = max
                                .zip(max_latency)
                                .map(|(max, max_latency)| max.min(max_latency))
                                .or(max);
                        }
                    }
                }

                q.set(true, min_latency, max_latency);

                ret
            }
            _ => {
                let sinkpad = {
                    let state = inputselector.state.lock().unwrap();
                    state.active_sinkpad.clone()
                };

                if let Some(sinkpad) = sinkpad {
                    sinkpad.peer_query(query)
                } else {
                    true
                }
            }
        }
    }
}

#[derive(Debug)]
struct State {
    active_sinkpad: Option<gst::Pad>,
    switched_pad: bool,
}

impl Default for State {
    fn default() -> State {
        State {
            active_sinkpad: None,
            switched_pad: true,
        }
    }
}

#[derive(Debug)]
struct Pads {
    pad_serial: u32,
    sink_pads: HashMap<gst::Pad, PadSink>,
}

impl Default for Pads {
    fn default() -> Pads {
        Pads {
            pad_serial: 0,
            sink_pads: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct InputSelector {
    src_pad: PadSrc,
    state: Mutex<State>,
    settings: Mutex<Settings>,
    pads: Mutex<Pads>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-input-selector",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing input selector"),
    )
});

impl InputSelector {
    fn unprepare(&self, element: &super::InputSelector) {
        let mut state = self.state.lock().unwrap();
        gst_debug!(CAT, obj: element, "Unpreparing");
        *state = State::default();
        gst_debug!(CAT, obj: element, "Unprepared");
    }
}

#[glib::object_subclass]
impl ObjectSubclass for InputSelector {
    const NAME: &'static str = "RsTsInputSelector";
    type Type = super::InputSelector;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                InputSelectorPadSrcHandler,
            ),
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
            pads: Mutex::new(Pads::default()),
        }
    }
}

impl ObjectImpl for InputSelector {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_string(
                    "context",
                    "Context",
                    "Context name to share threads with",
                    Some(DEFAULT_CONTEXT),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "context-wait",
                    "Context Wait",
                    "Throttle poll loop to run at most once every this many ms",
                    0,
                    1000,
                    DEFAULT_CONTEXT_WAIT.as_millis() as u32,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_object(
                    "active-pad",
                    "Active Pad",
                    "Currently active pad",
                    gst::Pad::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "context" => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            "context-wait" => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = Duration::from_millis(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "active-pad" => {
                let pad = value
                    .get::<Option<gst::Pad>>()
                    .expect("type checked upstream");
                let mut state = self.state.lock().unwrap();
                let pads = self.pads.lock().unwrap();
                let mut old_pad = None;
                if let Some(ref pad) = pad {
                    if pads.sink_pads.get(&pad).is_some() {
                        old_pad = state.active_sinkpad.clone();
                        state.active_sinkpad = Some(pad.clone());
                        state.switched_pad = true;
                    }
                } else {
                    state.active_sinkpad = None;
                }

                drop(pads);
                drop(state);

                if let Some(old_pad) = old_pad {
                    if Some(&old_pad) != pad.as_ref() {
                        let _ = old_pad.push_event(gst::event::Reconfigure::new());
                    }
                }

                if let Some(pad) = pad {
                    let _ = pad.push_event(gst::event::Reconfigure::new());
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "context" => {
                let settings = self.settings.lock().unwrap();
                settings.context.to_value()
            }
            "context-wait" => {
                let settings = self.settings.lock().unwrap();
                (settings.context_wait.as_millis() as u32).to_value()
            }
            "active-pad" => {
                let state = self.state.lock().unwrap();
                let active_pad = state.active_sinkpad.clone();
                active_pad.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.src_pad.gst_pad()).unwrap();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }
}

impl ElementImpl for InputSelector {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing input selector",
                "Generic",
                "Simple input selector element",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::new(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
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

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        if let gst::StateChange::ReadyToNull = transition {
            self.unprepare(element);
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut state = self.state.lock().unwrap();
        let mut pads = self.pads.lock().unwrap();
        let sink_pad =
            gst::Pad::from_template(&templ, Some(format!("sink_{}", pads.pad_serial).as_str()));
        pads.pad_serial += 1;
        sink_pad.set_active(true).unwrap();
        element.add_pad(&sink_pad).unwrap();
        let sink_pad = PadSink::new(sink_pad, InputSelectorPadSinkHandler::default());
        let ret = sink_pad.gst_pad().clone();

        if state.active_sinkpad.is_none() {
            state.active_sinkpad = Some(ret.clone());
            state.switched_pad = true;
        }

        pads.sink_pads.insert(ret.clone(), sink_pad);
        drop(pads);
        drop(state);

        let _ = element.post_message(gst::message::Latency::builder().src(element).build());

        Some(ret)
    }

    fn release_pad(&self, element: &Self::Type, pad: &gst::Pad) {
        let mut pads = self.pads.lock().unwrap();
        let sink_pad = pads.sink_pads.remove(pad).unwrap();
        drop(sink_pad);
        element.remove_pad(pad).unwrap();
        drop(pads);

        let _ = element.post_message(gst::message::Latency::builder().src(element).build());
    }

    fn provide_clock(&self, _element: &Self::Type) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
