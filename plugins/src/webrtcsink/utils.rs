use std::collections::HashMap;
use std::mem;
use std::sync::{atomic, Arc, Mutex};

use anyhow::{Context, Error};
use once_cell::sync::Lazy;

use gst::prelude::*;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "app-stream-producer",
        gst::DebugColorFlags::empty(),
        Some("gst_app Stream Producer interface"),
    )
});

/// Wrapper around `gst::ElementFactory::make` with a better error
/// message
pub fn make_element(element: &str, name: Option<&str>) -> Result<gst::Element, Error> {
    gst::ElementFactory::make(element, name)
        .with_context(|| format!("Failed to make element {}", element))
}

/// The interface for transporting media data from one node
/// to another.
///
/// A producer is essentially a GStreamer `appsink` whose output
/// is sent to a set of consumers, who are essentially `appsrc` wrappers
#[derive(Debug, Clone)]
pub struct StreamProducer {
    /// The appsink to dispatch data for
    appsink: gst_app::AppSink,
    /// The consumers to dispatch data to
    consumers: Arc<Mutex<StreamConsumers>>,
}

impl PartialEq for StreamProducer {
    fn eq(&self, other: &Self) -> bool {
        self.appsink.eq(&other.appsink)
    }
}

impl Eq for StreamProducer {}

impl StreamProducer {
    /// Add an appsrc to dispatch data to
    pub fn add_consumer(&self, consumer: &gst_app::AppSrc, consumer_id: &str) {
        let mut consumers = self.consumers.lock().unwrap();
        if consumers.consumers.get(consumer_id).is_some() {
            gst::error!(CAT, "Consumer already added");
            return;
        }

        gst::debug!(CAT, "Adding consumer");

        consumer.set_property("max-buffers", 0u64);
        consumer.set_property("max-bytes", 0u64);
        consumer.set_property("max-time", 500 * gst::ClockTime::MSECOND);
        consumer.set_property_from_str("leaky-type", "downstream");

        // Forward force-keyunit events upstream to the appsink
        let srcpad = consumer.static_pad("src").unwrap();
        let appsink_clone = self.appsink.clone();
        let fku_probe_id = srcpad
            .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(ref ev)) = info.data {
                    if gst_video::UpstreamForceKeyUnitEvent::parse(ev).is_ok() {
                        gst::debug!(CAT, "Requesting keyframe");
                        let _ = appsink_clone.send_event(ev.clone());
                    }
                }

                gst::PadProbeReturn::Ok
            })
            .unwrap();

        consumers.consumers.insert(
            consumer_id.to_string(),
            StreamConsumer::new(consumer, fku_probe_id, consumer_id),
        );
    }

    /// Remove a consumer appsrc by id
    pub fn remove_consumer(&self, consumer_id: &str) {
        if let Some(consumer) = self.consumers.lock().unwrap().consumers.remove(consumer_id) {
            gst::debug!(CAT, "Removed consumer {}", consumer.appsrc.name());
        } else {
            gst::debug!(CAT, "Consumer {} not found", consumer_id);
        }
    }

    /// Stop discarding data samples and start forwarding them to the consumers.
    ///
    /// This is useful for example for prerolling live sources.
    pub fn forward(&self) {
        self.consumers.lock().unwrap().discard = false;
    }

    /// Get the GStreamer `appsink` wrapped by this producer
    pub fn appsink(&self) -> &gst_app::AppSink {
        &self.appsink
    }
}

impl<'a> From<&'a gst_app::AppSink> for StreamProducer {
    fn from(appsink: &'a gst_app::AppSink) -> Self {
        let consumers = Arc::new(Mutex::new(StreamConsumers {
            current_latency: None,
            latency_updated: false,
            consumers: HashMap::new(),
            discard: true,
        }));

        let consumers_clone = consumers.clone();
        let consumers_clone2 = consumers.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let mut consumers = consumers_clone.lock().unwrap();

                    let sample = match appsink.pull_sample() {
                        Ok(sample) => sample,
                        Err(_err) => {
                            gst::debug!(CAT, "Failed to pull sample");
                            return Err(gst::FlowError::Flushing);
                        }
                    };

                    if consumers.discard {
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    gst::trace!(CAT, "processing sample");

                    let latency = consumers.current_latency;
                    let latency_updated = mem::replace(&mut consumers.latency_updated, false);
                    let mut requested_keyframe = false;

                    let current_consumers = consumers
                        .consumers
                        .values()
                        .map(|c| {
                            if let Some(latency) = latency {
                                if c.forwarded_latency
                                    .compare_exchange(
                                        false,
                                        true,
                                        atomic::Ordering::SeqCst,
                                        atomic::Ordering::SeqCst,
                                    )
                                    .is_ok()
                                    || latency_updated
                                {
                                    c.appsrc.set_latency(latency, gst::ClockTime::NONE);
                                }
                            }

                            if c.first_buffer
                                .compare_exchange(
                                    true,
                                    false,
                                    atomic::Ordering::SeqCst,
                                    atomic::Ordering::SeqCst,
                                )
                                .is_ok()
                                && !requested_keyframe
                            {
                                gst::debug!(CAT, "Requesting keyframe for first buffer");
                                appsink.send_event(
                                    gst_video::UpstreamForceKeyUnitEvent::builder()
                                        .all_headers(true)
                                        .build(),
                                );
                                requested_keyframe = true;
                            }

                            c.appsrc.clone()
                        })
                        .collect::<smallvec::SmallVec<[_; 16]>>();
                    drop(consumers);

                    for consumer in current_consumers {
                        if let Err(err) = consumer.push_sample(&sample) {
                            gst::warning!(CAT, "Failed to push sample: {}", err);
                        }
                    }

                    Ok(gst::FlowSuccess::Ok)
                })
                .eos(move |_| {
                    let current_consumers = consumers_clone2
                        .lock()
                        .unwrap()
                        .consumers
                        .values()
                        .map(|c| c.appsrc.clone())
                        .collect::<smallvec::SmallVec<[_; 16]>>();

                    for consumer in current_consumers {
                        let _ = consumer.end_of_stream();
                    }
                })
                .build(),
        );

        let consumers_clone = consumers.clone();
        let sinkpad = appsink.static_pad("sink").unwrap();
        sinkpad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |pad, info| {
            if let Some(gst::PadProbeData::Event(ref ev)) = info.data {
                use gst::EventView;

                if let EventView::Latency(ev) = ev.view() {
                    if pad.parent().is_some() {
                        let latency = ev.latency();
                        let mut consumers = consumers_clone.lock().unwrap();
                        consumers.current_latency = Some(latency);
                        consumers.latency_updated = true;
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        StreamProducer {
            appsink: appsink.clone(),
            consumers,
        }
    }
}

/// Wrapper around a HashMap of consumers, exists for thread safety
/// and also protects some of the producer state
#[derive(Debug)]
struct StreamConsumers {
    /// The currently-observed latency
    current_latency: Option<gst::ClockTime>,
    /// Whether the consumers' appsrc latency needs updating
    latency_updated: bool,
    /// The consumers, link id -> consumer
    consumers: HashMap<String, StreamConsumer>,
    /// Whether appsrc samples should be forwarded to consumers yet
    discard: bool,
}

/// Wrapper around a consumer's `appsrc`
#[derive(Debug)]
struct StreamConsumer {
    /// The GStreamer `appsrc` of the consumer
    appsrc: gst_app::AppSrc,
    /// The id of a pad probe that intercepts force-key-unit events
    fku_probe_id: Option<gst::PadProbeId>,
    /// Whether an initial latency was forwarded to the `appsrc`
    forwarded_latency: atomic::AtomicBool,
    /// Whether a first buffer has made it through, used to determine
    /// whether a new key unit should be requested. Only useful for encoded
    /// streams.
    first_buffer: atomic::AtomicBool,
}

impl StreamConsumer {
    /// Create a new consumer
    fn new(appsrc: &gst_app::AppSrc, fku_probe_id: gst::PadProbeId, consumer_id: &str) -> Self {
        let consumer_id = consumer_id.to_string();
        appsrc.set_callbacks(
            gst_app::AppSrcCallbacks::builder()
                .enough_data(move |_appsrc| {
                    gst::debug!(
                        CAT,
                        "consumer {} is not consuming fast enough, old samples are getting dropped",
                        consumer_id
                    );
                })
                .build(),
        );

        StreamConsumer {
            appsrc: appsrc.clone(),
            fku_probe_id: Some(fku_probe_id),
            forwarded_latency: atomic::AtomicBool::new(false),
            first_buffer: atomic::AtomicBool::new(true),
        }
    }
}

impl Drop for StreamConsumer {
    fn drop(&mut self) {
        if let Some(fku_probe_id) = self.fku_probe_id.take() {
            let srcpad = self.appsrc.static_pad("src").unwrap();
            srcpad.remove_probe(fku_probe_id);
        }
    }
}

impl PartialEq for StreamConsumer {
    fn eq(&self, other: &Self) -> bool {
        self.appsrc.eq(&other.appsrc)
    }
}

impl Eq for StreamConsumer {}

impl std::hash::Hash for StreamConsumer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.appsrc, state);
    }
}

impl std::borrow::Borrow<gst_app::AppSrc> for StreamConsumer {
    fn borrow(&self) -> &gst_app::AppSrc {
        &self.appsrc
    }
}
