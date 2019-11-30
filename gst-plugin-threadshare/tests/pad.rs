// Copyright (C) 2019 François Laignel <fengalin@free.fr>
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

use either::Either;

use futures::channel::mpsc;
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::lock::Mutex;
use futures::prelude::*;

use glib;
use glib::{glib_boxed_derive_traits, glib_boxed_type, glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;
use gst::{gst_debug, gst_error_msg, gst_log};

use lazy_static::lazy_static;

use std::boxed::Box;
use std::sync::Arc;

use gstthreadshare::runtime::prelude::*;
use gstthreadshare::runtime::{Context, PadContext, PadSink, PadSinkRef, PadSrc, PadSrcRef};

const DEFAULT_CONTEXT: &str = "";
const SLEEP_DURATION: u32 = 2;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare pad test");
    });
}

// Src

static SRC_PROPERTIES: [glib::subclass::Property; 1] =
    [glib::subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    })];

#[derive(Debug)]
struct Settings {
    context: String,
}

lazy_static! {
    static ref SRC_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-element-src-test",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Test Src Element"),
    );
}

#[derive(Clone, Debug)]
struct PadSrcHandlerTest;

impl PadSrcHandlerTest {
    async fn start_task(&self, pad: PadSrcRef<'_>, receiver: mpsc::Receiver<Item>) {
        let pad_weak = pad.downgrade();
        let receiver = Arc::new(Mutex::new(receiver));
        pad.start_task(move || {
            let pad_weak = pad_weak.clone();
            let receiver = Arc::clone(&receiver);
            async move {
                let item = receiver.lock().await.next().await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let item = match item {
                    Some(item) => item,
                    None => {
                        gst_debug!(SRC_CAT, obj: pad.gst_pad(), "SrcPad channel aborted");
                        pad.pause_task().await;
                        return;
                    }
                };

                Self::push_item(pad, item).await;
            }
        })
        .await;
    }

    async fn push_item(pad: PadSrcRef<'_>, item: Item) {
        match item {
            Item::Event(event) => {
                pad.push_event(event).await;
            }
            Item::Buffer(buffer) => {
                pad.push(buffer).await.unwrap();
            }
            Item::BufferList(list) => {
                pad.push_list(list).await.unwrap();
            }
        }
        gst_debug!(SRC_CAT, obj: pad.gst_pad(), "SrcPad handled an Item");
    }
}

impl PadSrcHandler for PadSrcHandlerTest {
    type ElementImpl = ElementSrcTest;

    fn src_activatemode(
        &self,
        _pad: PadSrcRef,
        _elem_src_test: &ElementSrcTest,
        _element: &gst::Element,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        gst_debug!(SRC_CAT, "SrcPad activatemode {:?}, {}", mode, active);

        Ok(())
    }

    fn src_event(
        &self,
        pad: PadSrcRef,
        elem_src_test: &ElementSrcTest,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = block_on(elem_src_test.pause(element));
                true
            }
            EventView::FlushStop(..) => {
                let (res, state, pending) = element.get_state(0.into());
                if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Playing
                    || res == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Playing
                {
                    let _ = block_on(elem_src_test.start(element));
                }
                true
            }
            _ => false,
        };

        if ret {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handled event {:?}", event);
        } else {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Didn't handle event {:?}", event);
        }

        Either::Left(ret)
    }
}

#[derive(Debug)]
struct ElementSrcState {
    sender: Option<mpsc::Sender<Item>>,
}

impl Default for ElementSrcState {
    fn default() -> Self {
        ElementSrcState { sender: None }
    }
}

#[derive(Debug)]
struct ElementSrcTest {
    src_pad: PadSrc,
    src_pad_handler: PadSrcHandlerTest,
    state: Mutex<ElementSrcState>,
    settings: Mutex<Settings>,
}

impl ElementSrcTest {
    async fn try_push(&self, item: Item) -> Result<(), Item> {
        match self.state.lock().await.sender.as_mut() {
            Some(sender) => sender
                .try_send(item)
                .map_err(mpsc::TrySendError::into_inner),
            None => Err(item),
        }
    }

    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let _state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().await;

        let context = Context::acquire(&settings.context, SLEEP_DURATION).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to acquire Context: {}", err]
            )
        })?;

        self.src_pad
            .prepare(context, &self.src_pad_handler)
            .await
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error joining Context: {:?}", err]
                )
            })?;

        gst_debug!(SRC_CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let _state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Unpreparing");

        self.src_pad.stop_task().await;
        let _ = self.src_pad.unprepare().await;

        gst_debug!(SRC_CAT, obj: element, "Unprepared");

        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Starting");

        let (sender, receiver) = mpsc::channel(1);
        state.sender = Some(sender);
        self.src_pad_handler
            .start_task(self.src_pad.as_ref(), receiver)
            .await;

        gst_debug!(SRC_CAT, obj: element, "Started");

        Ok(())
    }

    async fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let pause_completion = {
            let mut state = self.state.lock().await;
            gst_debug!(SRC_CAT, obj: element, "Pausing");

            let pause_completion = self.src_pad.pause_task().await;
            // Prevent subsequent items from being enqueued
            state.sender = None;

            pause_completion
        };

        gst_debug!(SRC_CAT, obj: element, "Waiting for Task Pause to complete");
        pause_completion.await;

        gst_debug!(SRC_CAT, obj: element, "Paused");

        Ok(())
    }
}

impl ObjectSubclass for ElementSrcTest {
    const NAME: &'static str = "RsTsElementSrcTest";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = glib::subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut glib::subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing Test Src Element",
            "Generic",
            "Src Element for Pad Src Test",
            "François Laignel <fengalin@free.fr>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&SRC_PROPERTIES);
    }

    fn new_with_class(klass: &glib::subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        let settings = Settings {
            context: String::new(),
        };

        ElementSrcTest {
            src_pad,
            src_pad_handler: PadSrcHandlerTest {},
            state: Mutex::new(ElementSrcState::default()),
            settings: Mutex::new(settings),
        }
    }
}

impl ObjectImpl for ElementSrcTest {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &SRC_PROPERTIES[id];

        match *prop {
            glib::subclass::Property("context", ..) => {
                let context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());

                block_on(self.settings.lock()).context = context;
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();
    }
}

impl ElementImpl for ElementSrcTest {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_log!(SRC_CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                block_on(self.pause(element)).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                block_on(self.unprepare(element)).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToPlaying => {
                block_on(self.start(element)).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }
}

// Sink

#[derive(Debug)]
enum Item {
    Buffer(gst::Buffer),
    BufferList(gst::BufferList),
    Event(gst::Event),
}

#[derive(Clone, Debug)]
struct ItemSender {
    sender: mpsc::Sender<Item>,
}
impl glib::subclass::boxed::BoxedType for ItemSender {
    const NAME: &'static str = "TsTestItemSender";

    glib_boxed_type!();
}
glib_boxed_derive_traits!(ItemSender);

static SINK_PROPERTIES: [glib::subclass::Property; 1] =
    [glib::subclass::Property("sender", |name| {
        glib::ParamSpec::boxed(
            name,
            "Sender",
            "Channel sender to forward the incoming items to",
            ItemSender::get_type(),
            glib::ParamFlags::WRITABLE,
        )
    })];

#[derive(Clone, Debug)]
struct PadSinkHandlerTest;

impl PadSinkHandler for PadSinkHandlerTest {
    type ElementImpl = ElementSinkTest;

    fn sink_chain(
        &self,
        pad: PadSinkRef,
        elem_sink_test: &ElementSinkTest,
        _element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let sender = Arc::clone(&elem_sink_test.sender);
        async move {
            let pad = pad_weak
                .upgrade()
                .expect("PadSink no longer exists in sink_chain");

            gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Fowarding {:?}", buffer);
            sender
                .lock()
                .await
                .as_mut()
                .expect("ItemSender not set")
                .send(Item::Buffer(buffer))
                .await
                .map(|_| gst::FlowSuccess::Ok)
                .map_err(|_| gst::FlowError::CustomError)
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        pad: PadSinkRef,
        elem_sink_test: &ElementSinkTest,
        _element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let sender = Arc::clone(&elem_sink_test.sender);
        async move {
            let pad = pad_weak
                .upgrade()
                .expect("PadSink no longer exists in sink_chain_list");

            gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Fowarding {:?}", list);
            sender
                .lock()
                .await
                .as_mut()
                .expect("ItemSender not set")
                .send(Item::BufferList(list))
                .await
                .map(|_| gst::FlowSuccess::Ok)
                .map_err(|_| gst::FlowError::CustomError)
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: PadSinkRef,
        elem_sink_test: &ElementSinkTest,
        _element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        if event.is_serialized() {
            let pad_weak = pad.downgrade();
            let sender = Arc::clone(&elem_sink_test.sender);

            Either::Right(async move {
                let pad = pad_weak
                    .upgrade()
                    .expect("PadSink no longer exists in sink_event");

                    gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Fowarding serialized event {:?}", event);
                    sender
                        .lock()
                        .await
                        .as_mut()
                        .expect("ItemSender not set")
                        .send(Item::Event(event))
                        .await
                        .is_ok()
            }.boxed())
        } else {
            gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Fowarding non-serialized event {:?}", event);
            Either::Left(
                block_on(elem_sink_test.sender.lock())
                    .as_mut()
                    .expect("ItemSender not set")
                    .try_send(Item::Event(event))
                    .is_ok(),
            )
        }
    }
}

#[derive(Debug)]
struct ElementSinkTest {
    sink_pad: PadSink,
    sender: Arc<Mutex<Option<mpsc::Sender<Item>>>>,
}

lazy_static! {
    static ref SINK_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-element-sink-test",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Test Sink Element"),
    );
}

impl ObjectSubclass for ElementSinkTest {
    const NAME: &'static str = "RsTsElementSinkTest";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = glib::subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut glib::subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing Test Sink Element",
            "Generic",
            "Sink Element for Pad Test",
            "François Laignel <fengalin@free.fr>",
        );

        let caps = gst::Caps::new_any();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&SINK_PROPERTIES);

        klass.add_signal_with_class_handler(
            "flush-start",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[],
            bool::static_type(),
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let this = Self::from_instance(&element);
                Some(
                    this.sink_pad
                        .gst_pad()
                        .push_event(gst::Event::new_flush_start().build())
                        .to_value(),
                )
            },
        );

        klass.add_signal_with_class_handler(
            "flush-stop",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[],
            bool::static_type(),
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let this = Self::from_instance(&element);
                Some(
                    this.sink_pad
                        .gst_pad()
                        .push_event(gst::Event::new_flush_stop(true).build())
                        .to_value(),
                )
            },
        );
    }

    fn new_with_class(klass: &glib::subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sink_pad = PadSink::new_from_template(&templ, Some("sink"));

        ElementSinkTest {
            sink_pad,
            sender: Arc::new(Mutex::new(None)),
        }
    }
}

impl ObjectImpl for ElementSinkTest {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &SINK_PROPERTIES[id];

        match *prop {
            glib::subclass::Property("sender", ..) => {
                let ItemSender { sender } = value
                    .get::<&ItemSender>()
                    .expect("type checked upstream")
                    .expect("ItemSender not found")
                    .clone();
                *block_on(self.sender.lock()) = Some(sender);
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.sink_pad.gst_pad()).unwrap();
    }
}

impl ElementImpl for ElementSinkTest {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_log!(SINK_CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                block_on(self.sink_pad.prepare(&PadSinkHandlerTest {}));
            }
            gst::StateChange::ReadyToNull => {
                block_on(self.sink_pad.unprepare());
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}

fn setup(
    context_name: &str,
) -> (
    gst::Pipeline,
    gst::Element,
    gst::Element,
    mpsc::Receiver<Item>,
) {
    init();

    // Src
    let src_element = glib::Object::new(ElementSrcTest::get_type(), &[])
        .unwrap()
        .downcast::<gst::Element>()
        .unwrap();
    src_element.set_property("context", &context_name).unwrap();

    // Sink
    let sink_element = glib::Object::new(ElementSinkTest::get_type(), &[])
        .unwrap()
        .downcast::<gst::Element>()
        .unwrap();

    let (sender, receiver) = mpsc::channel::<Item>(10);
    sink_element
        .set_property("sender", &ItemSender { sender })
        .unwrap();

    let pipeline = gst::Pipeline::new(None);
    pipeline.add_many(&[&src_element, &sink_element]).unwrap();

    src_element.link(&sink_element).unwrap();

    (pipeline, src_element, sink_element, receiver)
}

#[test]
fn task() {
    let (pipeline, src_element, sink_element, mut receiver) = setup("task");

    let elem_src_test = ElementSrcTest::from_instance(&src_element);

    pipeline.set_state(gst::State::Playing).unwrap();

    // Initial events
    block_on(
        elem_src_test.try_push(Item::Event(
            gst::Event::new_stream_start("stream_id_task_test")
                .group_id(gst::util_group_id_next())
                .build(),
        )),
    )
    .unwrap();

    match block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            gst::EventView::CustomDownstreamSticky(e) => {
                assert!(PadContext::is_pad_context_sticky_event(&e))
            }
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    match block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            gst::EventView::StreamStart(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    block_on(elem_src_test.try_push(Item::Event(
        gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
    )))
    .unwrap();

    match block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            gst::EventView::Segment(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    // Buffer
    block_on(elem_src_test.try_push(Item::Buffer(gst::Buffer::from_slice(vec![1, 2, 3, 4]))))
        .unwrap();

    match block_on(receiver.next()).unwrap() {
        Item::Buffer(buffer) => {
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.as_slice(), vec![1, 2, 3, 4].as_slice());
        }
        other => panic!("Unexpected item {:?}", other),
    }

    // BufferList
    let mut list = gst::BufferList::new();
    list.get_mut()
        .unwrap()
        .add(gst::Buffer::from_slice(vec![1, 2, 3, 4]));
    block_on(elem_src_test.try_push(Item::BufferList(list))).unwrap();

    match block_on(receiver.next()).unwrap() {
        Item::BufferList(_) => (),
        other => panic!("Unexpected item {:?}", other),
    }

    // Pause the Pad task
    pipeline.set_state(gst::State::Paused).unwrap();

    // Items not longer accepted
    block_on(elem_src_test.try_push(Item::Buffer(gst::Buffer::from_slice(vec![1, 2, 3, 4]))))
        .unwrap_err();

    // Nothing forwarded
    receiver.try_next().unwrap_err();

    // Switch back the Pad task to Started
    pipeline.set_state(gst::State::Playing).unwrap();

    // Still nothing forwarded
    receiver.try_next().unwrap_err();

    // Flush
    assert!(sink_element
        .emit("flush-start", &[])
        .unwrap()
        .unwrap()
        .get_some::<bool>()
        .unwrap());

    assert!(sink_element
        .emit("flush-stop", &[])
        .unwrap()
        .unwrap()
        .get_some::<bool>()
        .unwrap());

    // EOS
    block_on(elem_src_test.try_push(Item::Event(gst::Event::new_eos().build()))).unwrap();
    match block_on(receiver.next()).unwrap() {
        Item::Event(event) => match event.view() {
            gst::EventView::Eos(_) => (),
            other => panic!("Unexpected event {:?}", other),
        },
        other => panic!("Unexpected item {:?}", other),
    }

    // Stop the Pad task
    pipeline.set_state(gst::State::Ready).unwrap();

    // Receiver was dropped when stopping => can't send anymore
    block_on(
        elem_src_test.try_push(Item::Event(
            gst::Event::new_stream_start("stream_id_task_test_past_stop")
                .group_id(gst::util_group_id_next())
                .build(),
        )),
    )
    .unwrap_err();
}
