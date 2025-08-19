// SPDX-License-Identifier: LGPL-2.1-or-later

/**
 * SECTION:element-ts-blocking-adapter
 * @title: ts-blocking-adapter
 *
 * Thread-sharing downstream blocking branch async adapter.
 *
 * When a threadshare element streams buffers to an element which synchronizes on
 * the clock or which uses a limited blocking queue, the threadshare context is
 * perdiodically blocked, preventing other async tasks from progressing.
 *
 * One workaround is to use a regular `queue` with enough buffering to cope with
 * early buffers. This is suboptimal though because this `queue` doesn't propagate
 * backpressure upstream.
 *
 * The `ts-blocking-adapter` applies an async backpressure when downstream blocks,
 * allowing the threadshare context to process other tasks. This is achieved by
 * having the 'sink' Pad forward serialized items (buffers, ...) to the 'src' Pad
 * Task (thread) via a rendezvous channel.
 *
 * Since: plugins-rs-0.14.2
 */
use std::sync::{
    atomic::{AtomicBool, Ordering},
    LazyLock,
};

use flume::{Receiver, Sender};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::runtime::prelude::*;
use crate::runtime::PadSink;

#[derive(Debug)]
enum Item {
    Buffer(gst::Buffer),
    List(gst::BufferList),
    Event(gst::Event),
    Stop,
}

#[derive(Clone)]
struct BlockingAdapterPadSinkHandler;

impl PadSinkHandler for BlockingAdapterPadSinkHandler {
    type ElementImpl = BlockingAdapter;

    async fn sink_chain(
        self,
        pad: gst::Pad,
        elem: super::BlockingAdapter,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling {buffer:?}");
        elem.imp().handle_item(Item::Buffer(buffer)).await
    }

    async fn sink_chain_list(
        self,
        pad: gst::Pad,
        elem: super::BlockingAdapter,
        list: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling {list:?}");
        elem.imp().handle_item(Item::List(list)).await
    }

    fn sink_event(self, pad: &gst::Pad, imp: &BlockingAdapter, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling non-serialized {event:?}");

        if let gst::EventView::FlushStart(..) = event.view() {
            imp.flush_start();
        }

        imp.srcpad.push_event(event)
    }

    async fn sink_event_serialized(
        self,
        pad: gst::Pad,
        elem: super::BlockingAdapter,
        event: gst::Event,
    ) -> bool {
        gst::log!(CAT, obj = pad, "Handling serialized {event:?}");

        let imp = elem.imp();

        if let gst::EventView::FlushStop(..) = event.view() {
            imp.flush_stop();
        }

        imp.handle_item(Item::Event(event)).await.is_ok()
    }

    fn sink_query(self, pad: &gst::Pad, imp: &BlockingAdapter, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling {query:?}");

        // Note about serialized queries:
        // if upstream is able to pass a serialized query,
        // then buffer backpressure is not active,
        // which means downstream is not blocked and is ready to handle the query.

        imp.srcpad.peer_query(query)
    }
}

#[derive(Debug)]
pub struct BlockingAdapter {
    sinkpad: PadSink,
    srcpad: gst::Pad,
    flushing: AtomicBool,
    item_tx: Sender<Item>,
    item_rx: Receiver<Item>,
    res_tx: Sender<Result<gst::FlowSuccess, gst::FlowError>>,
    res_rx: Receiver<Result<gst::FlowSuccess, gst::FlowError>>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-blocking-adapter",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing blocking adapter"),
    )
});

impl BlockingAdapter {
    async fn handle_item(&self, item: Item) -> Result<gst::FlowSuccess, gst::FlowError> {
        if self.flushing.load(Ordering::SeqCst) {
            return Err(gst::FlowError::Flushing);
        }

        self.item_tx.send_async(item).await.map_err(|_| {
            self.flush_start();
            gst::FlowError::Flushing
        })?;
        self.res_rx.recv_async().await.unwrap_or_else(|_| {
            self.flush_start();
            Err(gst::FlowError::Flushing)
        })
    }

    fn flush_start(&self) {
        self.flushing.store(true, Ordering::SeqCst);
    }

    fn flush_stop(&self) {
        self.flushing.store(false, Ordering::SeqCst);
    }

    fn src_activatemode(
        &self,
        pad: &gst::Pad,
        imp: &BlockingAdapter,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if mode != gst::PadMode::Push {
            return Err(gst::LoggableError::new(
                *CAT,
                glib::bool_error!("Unsupported pad mode {mode:?}"),
            ));
        }

        if !active {
            gst::trace!(CAT, obj = pad, "Stopping task");
            imp.flush_start();
            // Unlock the task loop
            let _ = imp.item_tx.try_send(Item::Stop);
            let _ = imp.srcpad.send_event(gst::event::FlushStart::new());
            pad.stop_task()?;
            gst::trace!(CAT, obj = pad, "Task stopped");

            return Ok(());
        }

        gst::trace!(CAT, obj = pad, "Starting task");
        pad.start_task({
            let weak_obj = imp.obj().downgrade();
            let weak_pad = imp.srcpad.downgrade();
            let item_rx = imp.item_rx.clone();
            let res_tx = imp.res_tx.clone();
            move || {
                let pause = || {
                    if let Some(obj) = weak_obj.upgrade() {
                        obj.imp().flush_start();
                    }
                    if let Some(pad) = weak_pad.upgrade() {
                        let _ = pad.pause_task();
                    }
                };

                let res = item_rx.recv();

                let Some(srcpad) = weak_pad.upgrade() else {
                    return;
                };
                gst::trace!(CAT, obj = srcpad, "Recv returned: {res:?}");

                match res {
                    Ok(Item::Buffer(buffer)) => {
                        gst::log!(CAT, obj = srcpad, "Handling {buffer:?}");
                        let res = srcpad.push(buffer);

                        if let Err(err) = res {
                            gst::info!(CAT, obj = srcpad, "Error forwarding buffer: {err}");
                        }
                        if res_tx.send(res).is_err() || res.is_err() {
                            pause()
                        }
                    }
                    Ok(Item::List(list)) => {
                        gst::log!(CAT, obj = srcpad, "Handling {list:?}");
                        let res = srcpad.push_list(list);

                        if let Err(err) = res {
                            gst::info!(CAT, obj = srcpad, "Error forwarding list: {err}");
                        }
                        if res_tx.send(res).is_err() || res.is_err() {
                            pause()
                        }
                    }
                    Ok(Item::Event(event)) => {
                        gst::log!(CAT, obj = srcpad, "Handling {event:?}");
                        let res = if srcpad.push_event(event) {
                            Ok(gst::FlowSuccess::Ok)
                        } else {
                            gst::info!(CAT, obj = srcpad, "Error forwarding event");
                            Err(gst::FlowError::Error)
                        };

                        if res_tx.send(res).is_err() || res.is_err() {
                            pause()
                        }
                    }
                    Ok(Item::Stop) => {
                        gst::log!(CAT, obj = srcpad, "Stopping task");
                        // Make sure the task doesn't iterate again before it's actually stopped
                        if let Some(pad) = weak_pad.upgrade() {
                            let _ = pad.pause_task();
                        }
                    }
                    Err(_) => pause(),
                }
            }
        })
        .map_err(|err| gst::loggable_error!(CAT, "Failed to start pad task: {err}"))?;

        imp.flush_stop();

        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for BlockingAdapter {
    const NAME: &'static str = "GstTsBlockingAdapter";
    type Type = super::BlockingAdapter;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let (item_tx, item_rx) = flume::bounded(0);
        let (res_tx, res_rx) = flume::bounded(0);

        let srcpad = gst::Pad::builder_from_template(&klass.pad_template("src").unwrap())
            .event_function(|pad, parent, event| {
                BlockingAdapter::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| {
                        gst::log!(CAT, obj = pad, "Handling upstrem {event:?}");

                        use gst::EventView;
                        match event.view() {
                            EventView::FlushStart(..) => this.flush_start(),
                            EventView::FlushStop(..) => this.flush_stop(),
                            _ => (),
                        }

                        this.sinkpad.gst_pad().push_event(event)
                    },
                )
            })
            .query_function(|pad, parent, query| {
                BlockingAdapter::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| {
                        gst::log!(CAT, obj = pad, "Forwarding {query:?}");
                        this.sinkpad.gst_pad().peer_query(query)
                    },
                )
            })
            .activatemode_function(|pad, parent, mode, active| {
                BlockingAdapter::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating src pad with mode"
                        ))
                    },
                    |this| this.src_activatemode(pad, this, mode, active),
                )
            })
            .build();

        Self {
            sinkpad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap()),
                BlockingAdapterPadSinkHandler,
            ),
            srcpad,
            flushing: AtomicBool::new(true),
            item_tx,
            item_rx,
            res_tx,
            res_rx,
        }
    }
}

impl ObjectImpl for BlockingAdapter {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(self.sinkpad.gst_pad()).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for BlockingAdapter {}

impl ElementImpl for BlockingAdapter {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing blocking adapter",
                "Generic",
                "Converts a blocking downstream branch into an async backpressure",
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
