use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gst_base::AGGREGATOR_FLOW_NEED_DATA;
use minidom::Element;
use once_cell::sync::Lazy;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::sync::Mutex;

// Offset in nanoseconds from midnight 01-01-1900 (prime epoch) to
// midnight 01-01-1970 (UNIX epoch)
const PRIME_EPOCH_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(2_208_988_800);

// Incoming metadata is split up frame-wise, and stored in a FIFO.
#[derive(Eq, Clone)]
struct MetaFrame {
    // From UtcTime attribute, in nanoseconds since prime epoch
    timestamp: gst::ClockTime,
    // The frame element, dumped to XML
    buffer: gst::Buffer,
}

impl Ord for MetaFrame {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.timestamp.cmp(&other.timestamp)
    }
}

impl PartialOrd for MetaFrame {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for MetaFrame {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp
    }
}

#[derive(Default)]
struct State {
    // FIFO of MetaFrames
    meta_frames: BTreeSet<MetaFrame>,
    // We may store the next buffer we output here while waiting
    // for a future buffer, when we need one to calculate its duration
    current_media_buffer: Option<gst::Buffer>,
}

pub struct OnvifAggregator {
    // Input media stream, can be anything with a reference timestamp meta
    media_sink_pad: gst_base::AggregatorPad,
    // Input metadata stream, must be complete VideoAnalytics XML documents
    // as output by onvifdepay
    meta_sink_pad: gst_base::AggregatorPad,
    state: Mutex<State>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "onvifaggregator",
        gst::DebugColorFlags::empty(),
        Some("ONVIF metadata / video aggregator"),
    )
});

static NTP_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::builder("timestamp/x-ntp").build());

#[glib::object_subclass]
impl ObjectSubclass for OnvifAggregator {
    const NAME: &'static str = "GstOnvifAggregator";
    type Type = super::OnvifAggregator;
    type ParentType = gst_base::Aggregator;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("media").unwrap();
        let media_sink_pad =
            gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ, Some("media"))
                .build();

        let templ = klass.pad_template("meta").unwrap();
        let meta_sink_pad =
            gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ, Some("meta")).build();

        Self {
            media_sink_pad,
            meta_sink_pad,
            state: Mutex::default(),
        }
    }
}

impl ObjectImpl for OnvifAggregator {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.media_sink_pad).unwrap();
        obj.add_pad(&self.meta_sink_pad).unwrap();
    }
}

impl GstObjectImpl for OnvifAggregator {}

impl ElementImpl for OnvifAggregator {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ONVIF metadata aggregator",
                "Aggregator",
                "ONVIF metadata aggregator",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let media_caps = gst::Caps::new_any();
            let media_sink_pad_template = gst::PadTemplate::with_gtype(
                "media",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &media_caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let meta_caps = gst::Caps::builder("application/x-onvif-metadata")
                .field("encoding", "utf8")
                .build();

            let meta_sink_pad_template = gst::PadTemplate::with_gtype(
                "meta",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &meta_caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &media_caps,
            )
            .unwrap();

            vec![
                media_sink_pad_template,
                meta_sink_pad_template,
                src_pad_template,
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        _templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        gst::error!(
            CAT,
            obj: element,
            "onvifaggregator doesn't expose request pads"
        );

        None
    }

    fn release_pad(&self, element: &Self::Type, _pad: &gst::Pad) {
        gst::error!(
            CAT,
            obj: element,
            "onvifaggregator doesn't expose request pads"
        );
    }
}

impl OnvifAggregator {
    // We simply consume all the incoming meta buffers and store them in a FIFO
    // as they arrive
    fn consume_meta(
        &self,
        state: &mut State,
        element: &super::OnvifAggregator,
    ) -> Result<(), gst::FlowError> {
        while let Some(buffer) = self.meta_sink_pad.pop_buffer() {
            let buffer = buffer.into_mapped_buffer_readable().map_err(|_| {
                gst::element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Failed to map buffer readable"]
                );

                gst::FlowError::Error
            })?;

            let utf8 = std::str::from_utf8(buffer.as_ref()).map_err(|err| {
                gst::element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Failed to decode buffer as UTF-8: {}", err]
                );

                gst::FlowError::Error
            })?;

            let root = utf8.parse::<Element>().map_err(|err| {
                gst::element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Failed to parse buffer as XML: {}", err]
                );

                gst::FlowError::Error
            })?;

            if let Some(analytics) =
                root.get_child("VideoAnalytics", "http://www.onvif.org/ver10/schema")
            {
                for el in analytics.children() {
                    // We are only interested in associating Frame metadata with video frames
                    if el.is("Frame", "http://www.onvif.org/ver10/schema") {
                        let timestamp = el.attr("UtcTime").ok_or_else(|| {
                            gst::element_error!(
                                element,
                                gst::ResourceError::Read,
                                ["Frame element has no UtcTime attribute"]
                            );

                            gst::FlowError::Error
                        })?;

                        let dt =
                            chrono::DateTime::parse_from_rfc3339(timestamp).map_err(|err| {
                                gst::element_error!(
                                    element,
                                    gst::ResourceError::Read,
                                    ["Failed to parse UtcTime {}: {}", timestamp, err]
                                );

                                gst::FlowError::Error
                            })?;

                        let prime_dt_ns = PRIME_EPOCH_OFFSET
                            + gst::ClockTime::from_nseconds(dt.timestamp_nanos() as u64);

                        let mut writer = Cursor::new(Vec::new());
                        el.write_to(&mut writer).map_err(|err| {
                            gst::element_error!(
                                element,
                                gst::ResourceError::Write,
                                ["Failed to write back frame as XML: {}", err]
                            );

                            gst::FlowError::Error
                        })?;

                        gst::trace!(CAT, "Consuming metadata buffer {}", prime_dt_ns);

                        state.meta_frames.insert(MetaFrame {
                            timestamp: prime_dt_ns,
                            buffer: gst::Buffer::from_slice(writer.into_inner()),
                        });
                    }
                }
            }
        }

        Ok(())
    }

    fn lookup_reference_timestamp(&self, buffer: &gst::Buffer) -> Option<gst::ClockTime> {
        for meta in buffer.iter_meta::<gst::ReferenceTimestampMeta>() {
            if meta.reference().is_subset(&NTP_CAPS) {
                return Some(meta.timestamp());
            }
        }

        None
    }

    fn media_buffer_duration(
        &self,
        element: &super::OnvifAggregator,
        current_media_buffer: &gst::Buffer,
        timeout: bool,
    ) -> Option<gst::ClockTime> {
        match current_media_buffer.duration() {
            Some(duration) => {
                gst::log!(
                    CAT,
                    obj: element,
                    "Current media buffer has a duration, using it: {}",
                    duration
                );
                Some(duration)
            }
            None => {
                if let Some(next_buffer) = self.media_sink_pad.peek_buffer() {
                    match next_buffer.pts().zip(current_media_buffer.pts()) {
                        Some((next_pts, current_pts)) => {
                            let duration = next_pts.saturating_sub(current_pts);

                            gst::log!(
                                CAT,
                                obj: element,
                                "calculated duration for current media buffer from next buffer: {}",
                                duration
                            );

                            Some(duration)
                        }
                        None => {
                            gst::log!(
                                CAT,
                                obj: element,
                                "could not calculate duration for current media buffer"
                            );
                            Some(gst::ClockTime::from_nseconds(0))
                        }
                    }
                } else if timeout {
                    gst::log!(
                        CAT,
                        obj: element,
                        "could not calculate duration for current media buffer"
                    );
                    Some(gst::ClockTime::from_nseconds(0))
                } else {
                    gst::trace!(
                        CAT,
                        obj: element,
                        "No next buffer to peek at yet to calculate duration"
                    );
                    None
                }
            }
        }
    }

    // Called after consuming metadata buffers, we consume the current media buffer
    // and output it when:
    //
    // * it does not have a reference timestamp meta
    // * we have timed out
    // * we have consumed a metadata buffer for a future frame
    fn consume_media(
        &self,
        state: &mut State,
        element: &super::OnvifAggregator,
        timeout: bool,
    ) -> Result<Option<(gst::Buffer, Option<gst::ClockTime>)>, gst::FlowError> {
        if let Some(mut current_media_buffer) = state
            .current_media_buffer
            .take()
            .or_else(|| self.media_sink_pad.pop_buffer())
        {
            if let Some(current_media_start) =
                self.lookup_reference_timestamp(&current_media_buffer)
            {
                let duration =
                    match self.media_buffer_duration(element, &current_media_buffer, timeout) {
                        Some(duration) => {
                            // Update the buffer duration for good measure, in order to
                            // set a fully-accurate position later on in aggregate()
                            {
                                let buf_mut = current_media_buffer.make_mut();
                                buf_mut.set_duration(duration);
                            }

                            duration
                        }
                        None => {
                            state.current_media_buffer = Some(current_media_buffer);
                            return Ok(None);
                        }
                    };

                let end = current_media_start + duration;

                if timeout {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Media buffer spanning {} -> {} is ready (timeout)",
                        current_media_start,
                        end
                    );
                    Ok(Some((current_media_buffer, Some(end))))
                } else if self.meta_sink_pad.is_eos() {
                    gst::debug!(
                        CAT,
                        obj: element,
                        "Media buffer spanning {} -> {} is ready (meta pad is EOS)",
                        current_media_start,
                        end
                    );
                    Ok(Some((current_media_buffer, Some(end))))
                } else if let Some(latest_frame) = state.meta_frames.iter().next_back() {
                    if latest_frame.timestamp > end {
                        gst::debug!(
                            CAT,
                            obj: element,
                            "Media buffer spanning {} -> {} is ready",
                            current_media_start,
                            end
                        );
                        Ok(Some((current_media_buffer, Some(end))))
                    } else {
                        gst::trace!(
                            CAT,
                            obj: element,
                            "Media buffer spanning {} -> {} isn't ready yet",
                            current_media_start,
                            end
                        );
                        state.current_media_buffer = Some(current_media_buffer);
                        Ok(None)
                    }
                } else {
                    gst::trace!(
                        CAT,
                        obj: element,
                        "Media buffer spanning {} -> {} isn't ready yet",
                        current_media_start,
                        end
                    );
                    state.current_media_buffer = Some(current_media_buffer);
                    Ok(None)
                }
            } else {
                gst::debug!(
                    CAT,
                    obj: element,
                    "Consuming media buffer with no reference NTP timestamp"
                );

                Ok(Some((current_media_buffer, gst::ClockTime::NONE)))
            }
        } else {
            gst::trace!(CAT, obj: element, "No media buffer queued");

            Ok(None)
        }
    }
}

impl AggregatorImpl for OnvifAggregator {
    fn aggregate(
        &self,
        element: &Self::Type,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj: element, "aggregate, timeout: {}", timeout);

        let mut state = self.state.lock().unwrap();

        self.consume_meta(&mut state, element)?;

        // When the current media buffer is ready, we attach all matching metadata buffers
        // and push it out
        if let Some((mut buffer, end)) = self.consume_media(&mut state, element, timeout)? {
            let mut buflist = gst::BufferList::new();

            if let Some(end) = end {
                let mut split_at: Option<MetaFrame> = None;
                let buflist_mut = buflist.get_mut().unwrap();

                for frame in state.meta_frames.iter() {
                    if frame.timestamp > end {
                        gst::trace!(
                            CAT,
                            obj: element,
                            "keeping metadata buffer at {} for next media buffer",
                            frame.timestamp
                        );
                        split_at = Some(frame.clone());
                        break;
                    } else {
                        gst::debug!(
                            CAT,
                            obj: element,
                            "Attaching meta buffer {}",
                            frame.timestamp
                        );
                        buflist_mut.add(frame.buffer.clone());
                    }
                }

                if let Some(split_at) = split_at {
                    state.meta_frames = state.meta_frames.split_off(&split_at);
                } else {
                    state.meta_frames.clear();
                }
            }

            drop(state);

            {
                let buf = buffer.make_mut();
                let mut meta = gst::meta::CustomMeta::add(buf, "OnvifXMLFrameMeta").unwrap();

                let s = meta.mut_structure();
                s.set("frames", buflist);
            }

            let position = buffer.pts().opt_add(
                buffer
                    .duration()
                    .unwrap_or_else(|| gst::ClockTime::from_nseconds(0)),
            );

            gst::log!(CAT, obj: element, "Updating position: {:?}", position);

            element.set_position(position);

            self.finish_buffer(element, buffer)
        } else if self.media_sink_pad.is_eos() {
            Err(gst::FlowError::Eos)
        } else {
            Err(AGGREGATOR_FLOW_NEED_DATA)
        }
    }

    fn src_query(&self, aggregator: &Self::Type, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Position(..)
            | QueryViewMut::Duration(..)
            | QueryViewMut::Uri(..)
            | QueryViewMut::Caps(..)
            | QueryViewMut::Allocation(..) => self.media_sink_pad.peer_query(query),
            QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps_owned();
                let class = aggregator.class();
                let templ = class.pad_template("media").unwrap();
                let templ_caps = templ.caps();

                q.set_result(caps.is_subset(templ_caps));

                true
            }
            _ => self.parent_src_query(aggregator, query),
        }
    }

    fn sink_event(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Caps(e) => {
                if aggregator_pad.upcast_ref::<gst::Pad>() == &self.media_sink_pad {
                    gst::info!(CAT, obj: aggregator, "Pushing caps {}", e.caps());
                    aggregator.set_src_caps(&e.caps_owned());
                }

                true
            }
            EventView::Segment(e) => {
                if aggregator_pad.upcast_ref::<gst::Pad>() == &self.media_sink_pad {
                    aggregator.update_segment(e.segment());
                }
                self.parent_sink_event(aggregator, aggregator_pad, event)
            }
            _ => self.parent_sink_event(aggregator, aggregator_pad, event),
        }
    }

    fn sink_query(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Position(..)
            | QueryViewMut::Duration(..)
            | QueryViewMut::Uri(..)
            | QueryViewMut::Allocation(..) => {
                if aggregator_pad == &self.media_sink_pad {
                    let srcpad = aggregator.src_pad();
                    srcpad.peer_query(query)
                } else {
                    self.parent_sink_query(aggregator, aggregator_pad, query)
                }
            }
            QueryViewMut::Caps(q) => {
                if aggregator_pad == &self.media_sink_pad {
                    let srcpad = aggregator.src_pad();
                    srcpad.peer_query(query)
                } else {
                    let filter = q.filter_owned();
                    let class = aggregator.class();
                    let templ = class.pad_template("meta").unwrap();
                    let templ_caps = templ.caps();

                    if let Some(filter) = filter {
                        q.set_result(
                            &filter.intersect_with_mode(templ_caps, gst::CapsIntersectMode::First),
                        );
                    } else {
                        q.set_result(templ_caps);
                    }

                    true
                }
            }
            QueryViewMut::AcceptCaps(q) => {
                if aggregator_pad.upcast_ref::<gst::Pad>() == &self.media_sink_pad {
                    let srcpad = aggregator.src_pad();
                    srcpad.peer_query(query);
                } else {
                    let caps = q.caps_owned();
                    let class = aggregator.class();
                    let templ = class.pad_template("meta").unwrap();
                    let templ_caps = templ.caps();

                    q.set_result(caps.is_subset(templ_caps));
                }

                true
            }
            _ => self.parent_src_query(aggregator, query),
        }
    }

    fn next_time(&self, aggregator: &Self::Type) -> Option<gst::ClockTime> {
        aggregator.simple_get_next_time()
    }

    fn negotiate(&self, _aggregator: &Self::Type) -> bool {
        true
    }
}
