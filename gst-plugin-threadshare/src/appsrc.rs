// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

use glib;
use glib::prelude::*;
use gst;
use gst::prelude::*;

use gst_plugin::element::*;
use gst_plugin::object::*;
use gst_plugin::properties::*;

use std::sync::Mutex;
use std::{u16, u32};

use futures::future;
use futures::sync::mpsc;
use futures::{Future, IntoFuture, Stream};

use either::Either;

use rand;

use iocontext::*;

const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_THREADS: i32 = 0;
const DEFAULT_CONTEXT_WAIT: u32 = 0;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MAX_BUFFERS: u32 = 10;
const DEFAULT_DO_TIMESTAMP: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_threads: i32,
    context_wait: u32,
    caps: Option<gst::Caps>,
    max_buffers: u32,
    do_timestamp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_threads: DEFAULT_CONTEXT_THREADS,
            context_wait: DEFAULT_CONTEXT_WAIT,
            caps: DEFAULT_CAPS,
            max_buffers: DEFAULT_MAX_BUFFERS,
            do_timestamp: DEFAULT_DO_TIMESTAMP,
        }
    }
}

static PROPERTIES: [Property; 6] = [
    Property::String(
        "context",
        "Context",
        "Context name to share threads with",
        Some(DEFAULT_CONTEXT),
        PropertyMutability::ReadWrite,
    ),
    Property::Int(
        "context-threads",
        "Context Threads",
        "Number of threads for the context thread-pool if we create it",
        (-1, u16::MAX as i32),
        DEFAULT_CONTEXT_THREADS,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "context-wait",
        "Context Wait",
        "Throttle poll loop to run at most once every this many ms",
        (0, 1000),
        DEFAULT_CONTEXT_WAIT,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "max-buffers",
        "Max Buffers",
        "Maximum number of buffers to queue up",
        (1, u32::MAX),
        DEFAULT_MAX_BUFFERS,
        PropertyMutability::ReadWrite,
    ),
    Property::Boxed(
        "caps",
        "Caps",
        "Caps to use",
        gst::Caps::static_type,
        PropertyMutability::ReadWrite,
    ),
    Property::Boolean(
        "do-timestamp",
        "Do Timestamp",
        "Timestamp buffers with the current running time on arrival",
        DEFAULT_DO_TIMESTAMP,
        PropertyMutability::ReadWrite,
    ),
];

struct State {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    channel: Option<mpsc::Sender<Either<gst::Buffer, gst::Event>>>,
    need_initial_events: bool,
    configured_caps: Option<gst::Caps>,
}

impl Default for State {
    fn default() -> State {
        State {
            io_context: None,
            pending_future_id: None,
            channel: None,
            need_initial_events: true,
            configured_caps: None,
        }
    }
}

struct AppSrc {
    cat: gst::DebugCategory,
    src_pad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl AppSrc {
    fn class_init(klass: &mut ElementClass) {
        klass.set_metadata(
            "Thread-sharing app source",
            "Source/Generic",
            "Thread-sharing app source",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);

        klass.add_action_signal(
            "push-buffer",
            &[gst::Buffer::static_type()],
            bool::static_type(),
            |args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .unwrap()
                    .downcast::<Element>()
                    .unwrap();
                let mut buffer = args[1].get::<gst::Buffer>().unwrap();
                let appsrc = element.get_impl().downcast_ref::<AppSrc>().unwrap();

                let settings = appsrc.settings.lock().unwrap().clone();

                if settings.do_timestamp {
                    if let Some(clock) = element.get_clock() {
                        let base_time = element.get_base_time();
                        let now = clock.get_time();

                        let buffer = buffer.make_mut();
                        buffer.set_dts(now - base_time);
                        buffer.set_pts(gst::CLOCK_TIME_NONE);
                    } else {
                        gst_error!(appsrc.cat, obj: &element, "Don't have a clock yet");
                        return Some(false.to_value());
                    }
                }

                let mut state = appsrc.state.lock().unwrap();
                if let Some(ref mut channel) = state.channel {
                    match channel.try_send(Either::Left(buffer)) {
                        Ok(_) => Some(true.to_value()),
                        Err(err) => {
                            gst_error!(
                                appsrc.cat,
                                obj: &element,
                                "Failed to queue buffer: {}",
                                err
                            );
                            Some(false.to_value())
                        }
                    }
                } else {
                    Some(false.to_value())
                }
            },
        );

        klass.add_action_signal("end-of-stream", &[], bool::static_type(), |args| {
            let element = args[0]
                .get::<gst::Element>()
                .unwrap()
                .downcast::<Element>()
                .unwrap();
            let appsrc = element.get_impl().downcast_ref::<AppSrc>().unwrap();

            let mut state = appsrc.state.lock().unwrap();
            if let Some(ref mut channel) = state.channel {
                match channel.try_send(Either::Right(gst::Event::new_eos().build())) {
                    Ok(_) => Some(true.to_value()),
                    Err(err) => {
                        gst_error!(appsrc.cat, obj: &element, "Failed to queue EOS: {}", err);
                        Some(false.to_value())
                    }
                }
            } else {
                Some(false.to_value())
            }
        });
    }

    fn init(element: &Element) -> Box<ElementImpl<Element>> {
        let templ = element.get_pad_template("src").unwrap();
        let src_pad = gst::Pad::new_from_template(&templ, "src");

        src_pad.set_event_function(|pad, parent, event| {
            AppSrc::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_event(pad, element, event),
            )
        });
        src_pad.set_query_function(|pad, parent, query| {
            AppSrc::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_query(pad, element, query),
            )
        });
        element.add_pad(&src_pad).unwrap();

        Box::new(Self {
            cat: gst::DebugCategory::new(
                "ts-appsrc",
                gst::DebugColorFlags::empty(),
                "Thread-sharing app source",
            ),
            src_pad: src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        })
    }

    fn catch_panic_pad_function<T, F: FnOnce(&Self, &Element) -> T, G: FnOnce() -> T>(
        parent: &Option<gst::Object>,
        fallback: G,
        f: F,
    ) -> T {
        let element = parent
            .as_ref()
            .cloned()
            .unwrap()
            .downcast::<Element>()
            .unwrap();
        let src = element.get_impl().downcast_ref::<AppSrc>().unwrap();
        element.catch_panic(fallback, |element| f(src, element))
    }

    fn create_io_context_event(state: &State) -> Option<gst::Event> {
        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            let s = gst::Structure::new(
                "ts-io-context",
                &[
                    ("io-context", &glib::AnySendValue::new(io_context.clone())),
                    (
                        "pending-future-id",
                        &glib::AnySendValue::new(*pending_future_id),
                    ),
                ],
            );
            Some(gst::Event::new_custom_downstream_sticky(s).build())
        } else {
            None
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
                true
            }
            EventView::FlushStop(..) => {
                let (ret, state, pending) = element.get_state(0.into());
                if ret == gst::StateChangeReturn::Success && state == gst::State::Playing
                    || ret == gst::StateChangeReturn::Async && pending == gst::State::Playing
                {
                    let _ = self.start(element);
                }
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(self.cat, obj: pad, "Handled event {:?}", event);
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle event {:?}", event);
        }

        ret
    }

    fn src_query(&self, pad: &gst::Pad, _element: &Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
        let ret = match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                q.set(true, 0.into(), 0.into());
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let state = self.state.lock().unwrap();
                let caps = if let Some(ref caps) = state.configured_caps {
                    q.get_filter()
                        .map(|f| f.intersect_with_mode(caps, gst::CapsIntersectMode::First))
                        .unwrap_or(caps.clone())
                } else {
                    q.get_filter()
                        .map(|f| f.to_owned())
                        .unwrap_or(gst::Caps::new_any())
                };

                q.set_result(&caps);

                true
            }
            _ => false,
        };

        if ret {
            gst_log!(self.cat, obj: pad, "Handled query {:?}", query);
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle query {:?}", query);
        }
        ret
    }

    fn push_item(
        &self,
        element: &Element,
        item: Either<gst::Buffer, gst::Event>,
    ) -> future::Either<
        Box<Future<Item = (), Error = ()> + Send + 'static>,
        future::FutureResult<(), ()>,
    > {
        let mut events = Vec::new();
        let mut state = self.state.lock().unwrap();
        if state.need_initial_events {
            gst_debug!(self.cat, obj: element, "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            events.push(gst::Event::new_stream_start(&stream_id).build());
            if let Some(ref caps) = self.settings.lock().unwrap().caps {
                events.push(gst::Event::new_caps(&caps).build());
                state.configured_caps = Some(caps.clone());
            }
            events.push(
                gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
            );

            if let Some(event) = Self::create_io_context_event(&state) {
                events.push(event);

                // Get rid of reconfigure flag
                self.src_pad.check_reconfigure();
            }
            state.need_initial_events = false;
        } else if self.src_pad.check_reconfigure() {
            if let Some(event) = Self::create_io_context_event(&state) {
                events.push(event);
            }
        }
        drop(state);

        for event in events {
            self.src_pad.push_event(event);
        }

        let res = match item {
            Either::Left(buffer) => {
                gst_log!(self.cat, obj: element, "Forwarding buffer {:?}", buffer);
                self.src_pad.push(buffer).into_result().map(|_| ())
            }
            Either::Right(event) => {
                gst_log!(self.cat, obj: element, "Forwarding event {:?}", event);
                self.src_pad.push_event(event);
                Ok(())
            }
        };

        let res = match res {
            Ok(_) => {
                gst_log!(self.cat, obj: element, "Successfully pushed item");
                Ok(())
            }
            Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => {
                gst_debug!(self.cat, obj: element, "EOS");
                Err(())
            }
            Err(err) => {
                gst_error!(self.cat, obj: element, "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
                Err(())
            }
        };

        match res {
            Ok(()) => {
                let state = self.state.lock().unwrap();

                let State {
                    ref pending_future_id,
                    ref io_context,
                    ..
                } = *state;

                if let (&Some(ref pending_future_id), &Some(ref io_context)) =
                    (pending_future_id, io_context)
                {
                    let pending_futures = io_context.drain_pending_futures(*pending_future_id);

                    if !pending_futures.is_empty() {
                        gst_log!(
                            self.cat,
                            obj: element,
                            "Scheduling {} pending futures",
                            pending_futures.len()
                        );

                        let future = pending_futures.for_each(|_| Ok(()));

                        future::Either::A(Box::new(future))
                    } else {
                        future::Either::B(Ok(()).into_future())
                    }
                } else {
                    future::Either::B(Ok(()).into_future())
                }
            }
            Err(_) => future::Either::B(Err(()).into_future()),
        }
    }

    fn prepare(&self, element: &Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context = IOContext::new(
            &settings.context,
            settings.context_threads as isize,
            settings.context_wait,
        ).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to create IO context: {}", err]
            )
        })?;

        let pending_future_id = io_context.acquire_pending_future_id();
        gst_debug!(
            self.cat,
            obj: element,
            "Got pending future id {:?}",
            pending_future_id
        );

        state.io_context = Some(io_context);
        state.pending_future_id = Some(pending_future_id);

        gst_debug!(self.cat, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Unpreparing");

        let mut state = self.state.lock().unwrap();

        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            io_context.release_pending_future_id(*pending_future_id);
        }

        *state = State::default();

        gst_debug!(self.cat, obj: element, "Unprepared");

        Ok(())
    }

    fn start(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Starting");
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();

        let State {
            ref io_context,
            ref mut channel,
            ..
        } = *state;

        let io_context = io_context.as_ref().unwrap();

        let (channel_sender, channel_receiver) = mpsc::channel(settings.max_buffers as usize);

        let element_clone = element.clone();
        let future = channel_receiver.for_each(move |item| {
            let appsrc = element_clone.get_impl().downcast_ref::<AppSrc>().unwrap();
            appsrc.push_item(&element_clone, item)
        });
        io_context.spawn(future);

        *channel = Some(channel_sender);
        gst_debug!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Stopping");
        let mut state = self.state.lock().unwrap();

        let _ = state.channel.take();

        gst_debug!(self.cat, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectImpl<Element> for AppSrc {
    fn set_property(&self, _obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value.get().unwrap_or_else(|| "".into());
            }
            Property::Int("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_threads = value.get().unwrap();
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get().unwrap();
            }
            Property::Boxed("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.caps = value.get();
            }
            Property::UInt("max-buffers", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_buffers = value.get().unwrap();
            }
            Property::Boolean("do-timestamp", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_timestamp = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            Property::Int("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_threads.to_value())
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            Property::Boxed("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.caps.to_value())
            }
            Property::UInt("max-buffers", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.max_buffers.to_value())
            }
            Property::Boolean("do-timestamp", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.do_timestamp.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl<Element> for AppSrc {
    fn change_state(
        &self,
        element: &Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => match self.prepare(element) {
                Err(err) => {
                    element.post_error_message(&err);
                    return gst::StateChangeReturn::Failure;
                }
                Ok(_) => (),
            },
            gst::StateChange::PlayingToPaused => match self.stop(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            gst::StateChange::ReadyToNull => match self.unprepare(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            _ => (),
        }

        let mut ret = element.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::ReadyToPaused => {
                ret = gst::StateChangeReturn::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => match self.start(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                state.need_initial_events = true;
            }
            _ => (),
        }

        ret
    }
}

struct AppSrcStatic;

impl ImplTypeStatic<Element> for AppSrcStatic {
    fn get_name(&self) -> &str {
        "AppSrc"
    }

    fn new(&self, element: &Element) -> Box<ElementImpl<Element>> {
        AppSrc::init(element)
    }

    fn class_init(&self, klass: &mut ElementClass) {
        AppSrc::class_init(klass);
    }
}

pub fn register(plugin: &gst::Plugin) {
    let type_ = register_type(AppSrcStatic);
    gst::Element::register(plugin, "ts-appsrc", 0, type_);
}
