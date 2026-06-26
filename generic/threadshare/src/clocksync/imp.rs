// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-ts-clocksync
 * @title: ts-clocksync
 *
 * Thread-sharing clocksync.
 *
 * Asynchronously synchronizes buffers to the clock.
 *
 * Since: plugins-rs-0.15.3
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::runtime::{self, PadSink, PadSrc, prelude::*};

const DEFAULT_SYNC: bool = true;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-clocksync",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing clocksync"),
    )
});

#[derive(Debug, Default, Clone)]
struct ClockSyncPadSinkHandler;
impl ClockSyncPadSinkHandler {
    async fn handle_buffer(
        &self,
        pad: &gst::Pad,
        imp: &ClockSync,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if imp.is_flushing.load(Ordering::SeqCst) {
            gst::debug!(CAT, obj = pad, "flushing => not forwarding {buffer:?}");

            return Err(gst::FlowError::Flushing);
        }

        if let Some(buffer_time) = buffer.dts_or_pts()
            && imp.sync.load(Ordering::SeqCst)
        {
            let running_time = {
                let mut sync_state = imp.sync_state.lock().unwrap();
                if sync_state.upstream_latency.is_none() {
                    drop(sync_state);
                    imp.update_upstream_latency();
                    sync_state = imp.sync_state.lock().unwrap();
                }
                sync_state.segment.as_ref().and_then(|segment| {
                    segment
                        .to_running_time(buffer_time)
                        .opt_add(sync_state.upstream_latency.unwrap_or(gst::ClockTime::ZERO))
                })
            };

            if let Some(running_time) = running_time {
                let now = imp.obj().current_running_time();

                gst::trace!(
                    CAT,
                    obj = pad,
                    "running time {running_time} now {}",
                    now.display(),
                );

                if let Ok(Some(delay)) = running_time.opt_checked_sub(now) {
                    // Flushing status might have changed
                    if imp.is_flushing.load(Ordering::SeqCst) {
                        gst::debug!(CAT, obj = pad, "flushing => not forwarding {buffer:?}");

                        return Err(gst::FlowError::Flushing);
                    }

                    gst::trace!(CAT, obj = pad, "sync: waiting {delay}");
                    // Up to 1/2 context-wait delay. See also `ClockSync::update_upstream_latency()`.
                    // TODO add option for 'no sooner than'
                    runtime::timer::delay_for(delay.into()).await;
                }
            } else {
                gst::log!(CAT, obj = pad, "couldn't compute running time");
            }

            // Flushing status might have changed
            if imp.is_flushing.load(Ordering::SeqCst) {
                gst::debug!(CAT, obj = pad, "flushing => not forwarding {buffer:?}");

                return Err(gst::FlowError::Flushing);
            }
        }

        gst::log!(CAT, obj = pad, "Forwarding {buffer:?}");

        imp.srcpad.push(buffer).await
    }
}

impl PadSinkHandler for ClockSyncPadSinkHandler {
    type ElementImpl = ClockSync;

    async fn sink_chain(
        self,
        pad: gst::Pad,
        elem: super::ClockSync,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, obj = pad, "Got {buffer:?}");

        self.handle_buffer(&pad, elem.imp(), buffer).await
    }

    async fn sink_chain_list(
        self,
        pad: gst::Pad,
        elem: super::ClockSync,
        list: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, obj = pad, "Got {list:?}");

        let imp = elem.imp();
        for buffer in list.iter_owned() {
            self.handle_buffer(&pad, imp, buffer).await?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(self, pad: &gst::Pad, imp: &ClockSync, event: gst::Event) -> bool {
        gst::debug!(CAT, obj = pad, "Handling non-serialized {event:?}");

        if let gst::EventView::FlushStart(..) = event.view() {
            imp.is_flushing.store(true, Ordering::SeqCst);
            return imp.srcpad.gst_pad().push_event(event);
        }

        gst::Pad::event_default(pad, Some(&*imp.obj()), event)
    }

    async fn sink_event_serialized(
        self,
        pad: gst::Pad,
        elem: super::ClockSync,
        event: gst::Event,
    ) -> bool {
        gst::debug!(CAT, obj = elem, "Handling {event:?}");

        use gst::EventView::*;
        match event.view() {
            Segment(e) => {
                let imp = elem.imp();
                let segment = e.segment().clone().downcast::<gst::format::Time>().ok();
                if segment.is_none() && imp.sync.load(Ordering::SeqCst) {
                    gst::warning!(CAT, obj = pad, "got non-time segment => will NOT sync")
                }
                imp.sync_state.lock().unwrap().segment = segment;
            }
            FlushStart(_) => {
                elem.imp().is_flushing.store(true, Ordering::SeqCst);
            }
            FlushStop(_) => elem.imp().stop_flush(),
            _ => (),
        }

        elem.imp().srcpad.push_event(event).await
    }

    fn sink_query(self, pad: &gst::Pad, imp: &ClockSync, query: &mut gst::QueryRef) -> bool {
        gst::debug!(CAT, obj = pad, "Handling {query:?}");

        gst::Pad::query_default(pad, Some(&*imp.obj()), query)
    }
}

#[derive(Clone, Debug)]
struct ClockSyncPadSrcHandler;
impl PadSrcHandler for ClockSyncPadSrcHandler {
    type ElementImpl = ClockSync;

    fn src_event(self, pad: &gst::Pad, imp: &ClockSync, event: gst::Event) -> bool {
        gst::debug!(CAT, obj = pad, "Handling {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(_) => {
                imp.is_flushing.store(true, Ordering::SeqCst);
            }
            FlushStop(_) => imp.stop_flush(),
            _ => (),
        }

        gst::Pad::event_default(pad, Some(&*imp.obj()), event)
    }

    fn src_query(self, pad: &gst::Pad, imp: &ClockSync, query: &mut gst::QueryRef) -> bool {
        gst::debug!(CAT, obj = pad, "Handling {query:?}");

        use gst::QueryViewMut::*;
        match query.view_mut() {
            Latency(q) => {
                let Some(latency_res) = imp.update_upstream_latency() else {
                    gst::debug!(CAT, obj = pad, "Failed to query upstream latency");
                    return false;
                };

                gst::debug!(CAT, obj = pad, "Returning latency: {latency_res:?}");
                q.set(latency_res.is_live, latency_res.min, latency_res.max);

                return true;
            }
            Scheduling(q) => {
                let mut upstream_query = gst::query::Scheduling::new();
                let res = gst::Pad::query_default(pad, Some(&*imp.obj()), &mut upstream_query);
                if !res {
                    return res;
                }

                gst::log!(CAT, obj = pad, "Upstream returned {upstream_query:?}");

                let (flags, min, max, align) = upstream_query.result();
                q.set(flags, min, max, align);
                q.add_scheduling_modes(
                    upstream_query
                        .scheduling_modes()
                        .filter(|m| m != &gst::PadMode::Pull),
                );
                gst::log!(CAT, obj = pad, "Returning {query:?}");
                return true;
            }
            _ => (),
        }

        gst::Pad::query_default(pad, Some(&*imp.obj()), query)
    }
}

#[derive(Debug, Default)]
struct SyncState {
    upstream_latency: Option<gst::ClockTime>,
    segment: Option<gst::FormattedSegment<gst::format::Time>>,
}

#[derive(Debug)]
pub struct ClockSync {
    sinkpad: PadSink,
    srcpad: PadSrc,
    is_flushing: AtomicBool,
    sync: AtomicBool,
    sync_state: Mutex<SyncState>,
}

impl ClockSync {
    fn update_upstream_latency(&self) -> Option<gst::query::LatencyResult> {
        let mut upstream_query = gst::query::Latency::new();
        let res = self.sinkpad.gst_pad().peer_query(&mut upstream_query);
        if !res {
            return None;
        }

        let mut latency_res = upstream_query.result_struct();

        gst::log!(CAT, imp = self, "Upstream returned {latency_res:?}");

        let has_changed = {
            let mut sync_state = self.sync_state.lock().unwrap();
            let new_latency = if latency_res.is_live {
                latency_res.min
            } else {
                gst::ClockTime::ZERO
            };

            let prev_latency = sync_state.upstream_latency.replace(new_latency);

            prev_latency != Some(new_latency)
        };

        if self.sync.load(Ordering::SeqCst)
            && let Some(ts_ctx) = runtime::Context::current()
        {
            // Element uses `delay_for` for sync (see `UdpSinkPadHandlerInner::sync`)
            // which adds up to 1/2 context-wait delay
            latency_res.min +=
                gst::ClockTime::from_nseconds(ts_ctx.wait_duration().as_nanos() as u64 / 2);
        }

        if has_changed {
            gst::log!(CAT, imp = self, "Upstream latency has changed");
            let _ = self.post_message(gst::message::Latency::builder().src(&*self.obj()).build());
        }

        Some(latency_res)
    }

    fn stop_flush(&self) {
        let mut sync_state = self.sync_state.lock().unwrap();
        sync_state.segment = None;
        self.is_flushing.store(false, Ordering::SeqCst);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ClockSync {
    const NAME: &'static str = "GstTsClockSync";
    type Type = super::ClockSync;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            sinkpad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap()),
                ClockSyncPadSinkHandler,
            ),
            srcpad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap()),
                ClockSyncPadSrcHandler,
            ),
            is_flushing: AtomicBool::new(false),
            sync: AtomicBool::new(DEFAULT_SYNC),
            sync_state: Default::default(),
        }
    }
}

impl ObjectImpl for ClockSync {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(self.sinkpad.gst_pad()).unwrap();
        obj.add_pad(self.srcpad.gst_pad()).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("sync")
                    .nick("Sync")
                    .blurb("Sync on the clock")
                    .default_value(DEFAULT_SYNC)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "sync" => {
                self.sync.store(
                    value.get().expect("type checked upstream"),
                    Ordering::SeqCst,
                );
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "sync" => self.sync.load(Ordering::SeqCst).to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ClockSync {}

impl ElementImpl for ClockSync {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing clocksync",
                "Generic",
                "Asynchronously synchronizes buffers to the clock",
                "François Laignel <francois@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();

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
