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
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::Mutex;
use std::u32;

use futures::future;
use futures::sync::{mpsc, oneshot};
use futures::{Future, Stream};

use either::Either;

use rand;

use iocontext::*;

const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MAX_BUFFERS: u32 = 10;
const DEFAULT_DO_TIMESTAMP: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: u32,
    caps: Option<gst::Caps>,
    max_buffers: u32,
    do_timestamp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            caps: DEFAULT_CAPS,
            max_buffers: DEFAULT_MAX_BUFFERS,
            do_timestamp: DEFAULT_DO_TIMESTAMP,
        }
    }
}

static PROPERTIES: [subclass::Property; 5] = [
    subclass::Property("context", || {
        glib::ParamSpec::string(
            "context",
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context-wait", || {
        glib::ParamSpec::uint(
            "context-wait",
            "Context Wait",
            "Throttle poll loop to run at most once every this many ms",
            0,
            1000,
            DEFAULT_CONTEXT_WAIT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-buffers", || {
        glib::ParamSpec::uint(
            "max-buffers",
            "Max Buffers",
            "Maximum number of buffers to queue up",
            1,
            u32::MAX,
            DEFAULT_MAX_BUFFERS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("caps", || {
        glib::ParamSpec::boxed(
            "caps",
            "Caps",
            "Caps to use",
            gst::Caps::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("do-timestamp", || {
        glib::ParamSpec::boolean(
            "do-timestamp",
            "Do Timestamp",
            "Timestamp buffers with the current running time on arrival",
            DEFAULT_DO_TIMESTAMP,
            glib::ParamFlags::READWRITE,
        )
    }),
];

struct State {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    channel: Option<mpsc::Sender<Either<gst::Buffer, gst::Event>>>,
    pending_future_cancel: Option<oneshot::Sender<()>>,
    need_initial_events: bool,
    configured_caps: Option<gst::Caps>,
}

impl Default for State {
    fn default() -> State {
        State {
            io_context: None,
            pending_future_id: None,
            channel: None,
            pending_future_cancel: None,
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
    fn create_io_context_event(state: &State) -> Option<gst::Event> {
        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            let s = gst::Structure::new(
                "ts-io-context",
                &[
                    ("io-context", &io_context),
                    ("pending-future-id", &*pending_future_id),
                ],
            );
            Some(gst::Event::new_custom_downstream_sticky(s).build())
        } else {
            None
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
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

    fn src_query(
        &self,
        pad: &gst::Pad,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
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

    fn push_buffer(&self, element: &gst::Element, mut buffer: gst::Buffer) -> bool {
        let settings = self.settings.lock().unwrap().clone();

        if settings.do_timestamp {
            if let Some(clock) = element.get_clock() {
                let base_time = element.get_base_time();
                let now = clock.get_time();

                let buffer = buffer.make_mut();
                buffer.set_dts(now - base_time);
                buffer.set_pts(gst::CLOCK_TIME_NONE);
            } else {
                gst_error!(self.cat, obj: element, "Don't have a clock yet");
                return false;
            }
        }

        let mut state = self.state.lock().unwrap();
        if let Some(ref mut channel) = state.channel {
            match channel.try_send(Either::Left(buffer)) {
                Ok(_) => true,
                Err(err) => {
                    gst_error!(self.cat, obj: element, "Failed to queue buffer: {}", err);
                    false
                }
            }
        } else {
            false
        }
    }

    fn end_of_stream(&self, element: &gst::Element) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(ref mut channel) = state.channel {
            match channel.try_send(Either::Right(gst::Event::new_eos().build())) {
                Ok(_) => true,
                Err(err) => {
                    gst_error!(self.cat, obj: element, "Failed to queue EOS: {}", err);
                    false
                }
            }
        } else {
            false
        }
    }

    fn push_item(
        &self,
        element: &gst::Element,
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
                let mut state = self.state.lock().unwrap();

                if let State {
                    io_context: Some(ref io_context),
                    pending_future_id: Some(ref pending_future_id),
                    ref mut pending_future_cancel,
                    ..
                } = *state
                {
                    let (cancel, future) = io_context.drain_pending_futures(*pending_future_id);
                    *pending_future_cancel = cancel;

                    future
                } else {
                    future::Either::B(future::ok(()))
                }
            }
            Err(_) => future::Either::B(future::err(())),
        }
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context =
            IOContext::new(&settings.context, settings.context_wait).map_err(|err| {
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

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
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

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
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
            let appsrc = Self::from_instance(&element_clone);
            appsrc.push_item(&element_clone, item)
        });
        io_context.spawn(future);

        *channel = Some(channel_sender);
        gst_debug!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Stopping");
        let mut state = self.state.lock().unwrap();

        let _ = state.channel.take();
        let _ = state.pending_future_cancel.take();

        gst_debug!(self.cat, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectSubclass for AppSrc {
    const NAME: &'static str = "RsTsAppSrc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
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
                let element = args[0].get::<gst::Element>().unwrap();
                let buffer = args[1].get::<gst::Buffer>().unwrap();
                let appsrc = Self::from_instance(&element);

                Some(appsrc.push_buffer(&element, buffer).to_value())
            },
        );

        klass.add_action_signal("end-of-stream", &[], bool::static_type(), |args| {
            let element = args[0].get::<gst::Element>().unwrap();
            let appsrc = Self::from_instance(&element);
            Some(appsrc.end_of_stream(&element).to_value())
        });
    }

    fn new() -> Self {
        unreachable!()
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
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

        Self {
            cat: gst::DebugCategory::new(
                "ts-appsrc",
                gst::DebugColorFlags::empty(),
                "Thread-sharing app source",
            ),
            src_pad: src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for AppSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value.get().unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get().unwrap();
            }
            subclass::Property("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.caps = value.get();
            }
            subclass::Property("max-buffers", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_buffers = value.get().unwrap();
            }
            subclass::Property("do-timestamp", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_timestamp = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            subclass::Property("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.caps.to_value())
            }
            subclass::Property("max-buffers", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.max_buffers.to_value())
            }
            subclass::Property("do-timestamp", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.do_timestamp.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.src_pad).unwrap();

        ::set_element_flags(element, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for AppSrc {
    fn change_state(
        &self,
        element: &gst::Element,
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

        let mut ret = self.parent_change_state(element, transition);
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

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(plugin, "ts-appsrc", 0, AppSrc::get_type())
}
