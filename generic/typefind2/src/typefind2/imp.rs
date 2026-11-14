// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-typefind2
 *
 * Reimplementation of the core typefind element, in Rust. Finds the media
 * type of a stream by matching registered typefind functions against the
 * stream data, in both push and pull modes, optionally falling back on the
 * URI file extension.
 *
 * Since: plugins-rs-0.16.0
 */
use std::sync::{LazyLock, Mutex, MutexGuard};

use gst::glib;
use gst::glib::translate::IntoGlib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::scanner;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "typefind2",
        gst::DebugColorFlags::empty(),
        Some("Media type detection element"),
    )
});

// Require at least 2KiB of data before attempting typefinding in push mode.
// 128KiB is massive overkill for the maximum, but doesn't do any harm.
const TYPEFIND_MIN_SIZE: usize = 2 * 1024;
const TYPEFIND_MAX_SIZE: usize = 128 * 1024;

// Block size used when forwarding data in pull mode.
const PULL_BLOCK_SIZE: u32 = 4096;

const DEFAULT_MINIMUM: u32 = 1;

/// Numeric value of a probability, comparable with the `minimum` property.
fn probability_value(probability: gst::TypeFindProbability) -> u32 {
    probability.into_glib() as u32
}

/// The fatal half of the end-of-streaming decision: strictly below `Eos`
/// (-3) covers `Error` (-5) but not `Flushing` (-2), which must pause
/// silently. `NotLinked` (-1) is fatal by a separate check at the call
/// sites.
fn is_fatal(ret: gst::FlowError) -> bool {
    (ret as i32) < (gst::FlowError::Eos as i32)
}

/// Extension of a URI: everything after the final dot of the last path
/// segment, lowercased.
///
/// `GstUri` is not bound in gstreamer-rs and `glib::Uri::path()` keeps
/// percent-encoding, so the path extraction is hand-rolled. Observable
/// consequences, pinned by `uri_extension_cases`: encoded dots stay encoded
/// (`file.mp%34`), backslashes act as separators, and lowercased extensions
/// match case-insensitively.
fn uri_extension(uri: &str) -> Option<String> {
    // Data URI paths are opaque, they don't denote a file-like resource.
    if uri.starts_with("data:") {
        return None;
    }

    // Relative URIs are rejected, and the authority never enters the path:
    // a host-only URI has no basename and cannot contribute the domain's
    // final label as a bogus extension.
    let rest = &uri[uri.find("://")? + 3..];
    let path = rest
        .find(['/', '\\'])
        .map(|i| &rest[i..])
        .unwrap_or_default();
    let filename = path
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();

    let (_, extension) = filename.rsplit_once('.')?;
    if extension.is_empty() {
        return None;
    }

    Some(extension.to_ascii_lowercase())
}

#[derive(Debug)]
enum Mode {
    /// Determining the media type of the stream.
    TypeFind(TypeFindMode),
    /// Type known, acting as an identity element.
    Passthrough,
    /// Fatal error, streaming must stop.
    /// The failed flow return is replayed by later chain and getrange
    /// calls, which is how the failure reaches the streaming stack: the
    /// paths that detect it have no flow return of their own.
    Error(gst::FlowError),
}

impl Mode {
    fn is_typefind(&self) -> bool {
        matches!(self, Mode::TypeFind(_))
    }

    fn is_passthrough(&self) -> bool {
        matches!(self, Mode::Passthrough)
    }

    fn is_error(&self) -> bool {
        matches!(self, Mode::Error(_))
    }
}

/// Data accumulated while typefinding in push mode.
#[derive(Debug, Default)]
struct TypeFindMode {
    /// Accumulation of not-yet-pushed data.
    adapter: gst_base::UniqueAdapter,
    /// Offset of the first buffered buffer, kept for the merged output buffer.
    initial_offset: u64,
    /// Events received while typefinding that must be sent downstream once the
    /// type is known, in arrival order.
    cached_events: Vec<gst::Event>,
}

/// All mutable state of the element. Pad function implementations must keep
/// this lock held for as little as possible and never across calls that enter
/// pads or emit signals: application `have-type` handlers read the element
/// state back through this very lock.
#[derive(Debug)]
struct Inner {
    mode: Mode,
    pull: Option<PullState>,
}

/// Segment bookkeeping, present only while the sink pad is in pull mode: its
/// absence marks "the element does not drive the pipeline itself", which is
/// what seek handling branches on.
#[derive(Debug)]
struct PullState {
    segment: gst::FormattedSegment<gst::format::Bytes>,
    /// Read position of the forwarding loop.
    offset: u64,
    seqnum: Option<gst::Seqnum>,
    need_segment: bool,
    need_stream_start: bool,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            mode: Mode::TypeFind(TypeFindMode::default()),
            pull: None,
        }
    }
}

#[derive(Debug, glib::Properties)]
#[properties(wrapper_type = super::TypeFind2)]
pub struct TypeFind2 {
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    inner: Mutex<Inner>,
    #[property(get, set, nullable)]
    force_caps: Mutex<Option<gst::Caps>>,
    #[property(get, set, minimum = DEFAULT_MINIMUM, maximum = 100u32, default = DEFAULT_MINIMUM)]
    minimum: Mutex<u32>,
    /// The caps found so far, if any.
    #[property(get, nullable)]
    caps: Mutex<Option<gst::Caps>>,
}

impl TypeFind2 {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn min_probability(&self) -> u32 {
        *self.minimum.lock().unwrap()
    }

    pub(crate) fn caps(&self) -> Option<gst::Caps> {
        self.caps.lock().unwrap().clone()
    }

    // ---- type finding ------------------------------------------------

    /// Emit the have-type signal: publish the found caps first (so that caps
    /// queries and property reads inside application handlers see them), then
    /// make the caps event sticky (so handlers can inspect it before it goes
    /// downstream), then emit. The default class handler forwards the event.
    fn emit_have_type(&self, probability: gst::TypeFindProbability, caps: &gst::Caps) {
        gst::info!(CAT, "Found caps {caps}, probability {probability:?}");

        *self.caps.lock().unwrap() = Some(caps.clone());

        let event = gst::event::Caps::new(caps);
        if let Err(err) = self.srcpad.store_sticky_event(&event) {
            gst::info!(CAT, "Could not store sticky caps event: {err}");
        }

        self.obj()
            .emit_by_name::<()>("have-type", &[&probability_value(probability), caps]);
    }

    fn start_typefinding(&self) {
        let mut inner = self.inner();
        *self.caps.lock().unwrap() = None;
        inner.mode = Mode::TypeFind(TypeFindMode::default());
    }

    /// Leaves typefind mode (done by the caller, which captures the payload)
    /// and sends the cached events and accumulated data downstream.
    fn stop_typefinding(&self, state: Option<TypeFindMode>) {
        let element = self.obj();
        let current_state = element.current_state();

        // Data and cached events only make sense to push for a pipeline that
        // is actually playing and for which a type was found.
        let collected = current_state >= gst::State::Paused && self.caps.lock().unwrap().is_some();

        let (cached_events, buffer) = match state.filter(|_| collected) {
            Some(mut state) => {
                let avail = state.adapter.available();
                let buffer = if avail > 0 {
                    let (pts, _) = state.adapter.prev_pts();
                    let (dts, _) = state.adapter.prev_dts();
                    let initial_offset = state.initial_offset;
                    state.adapter.take_buffer(avail).ok().map(|mut buffer| {
                        let bufref = buffer.make_mut();
                        bufref.set_pts(pts);
                        bufref.set_dts(dts);
                        bufref.set_offset(initial_offset);
                        buffer
                    })
                } else {
                    None
                };

                (std::mem::take(&mut state.cached_events), buffer)
            }
            _ => (Vec::new(), None),
        };

        for event in cached_events {
            self.srcpad.push_event(event);
        }

        let Some(buffer) = buffer else {
            gst::debug!(CAT, "No data to push downstream");
            return;
        };

        // The push is the only available check for a peer without a chain
        // function (`gst_pad_check_chain_function` is unbound): it fails
        // with NotSupported, alongside a critical log from the pad layer.
        // The element error below replaces that with a meaningful message.
        // Any other failure is kept as-is: the flow return has nowhere to
        // propagate from here, so it is replayed by later chain and
        // getrange calls.
        if let Err(err) = self.srcpad.push(buffer) {
            if err == gst::FlowError::NotSupported
                && let Some(peer) = self.srcpad.peer()
            {
                let parent_name = peer
                    .parent_element()
                    .map(|parent| parent.name().to_string())
                    .unwrap_or_default();
                gst::debug!(
                    CAT,
                    "Downstream element {} cannot work in push mode",
                    peer.name()
                );
                gst::element_error!(
                    &element,
                    gst::LibraryError::Failed,
                    (
                        "{} cannot work in push mode. The operation is not supported with this source element or protocol.",
                        parent_name
                    ),
                    [
                        "Downstream pad {}:{} has no chainfunction, and the upstream element does not support pull mode",
                        parent_name,
                        peer.name()
                    ]
                );
            }
            self.inner().mode = Mode::Error(err);
        }
    }

    /// Push-mode typefinding over the data accumulated in the adapter.
    fn chain_do_typefinding(&self, check_avail: bool, at_eos: bool) -> Result<(), gst::FlowError> {
        let (probability, caps, have_max) = match self.force_caps.lock().unwrap().clone() {
            // Forced caps are always Maximum, the low-probability path below
            // cannot trigger for them.
            Some(caps) => (gst::TypeFindProbability::Maximum, caps, true),
            None => {
                let extension = self.extension(&self.sinkpad);

                let mut inner = self.inner();
                let Mode::TypeFind(state) = &mut inner.mode else {
                    // The mode raced away (have-type handler): nothing to
                    // typefind anymore.
                    return Ok(());
                };
                let avail = state.adapter.available();
                let (have_min, have_max) = if check_avail {
                    (avail >= TYPEFIND_MIN_SIZE, avail >= TYPEFIND_MAX_SIZE)
                } else {
                    (avail > 0, true)
                };

                if !have_min {
                    drop(inner);
                    return self.not_enough_data(avail, at_eos);
                }

                let outcome = {
                    let map = state.adapter.map(avail).map_err(|_| {
                        gst::error!(CAT, "Could not map {avail} bytes of data");
                        gst::FlowError::Error
                    })?;

                    scanner::run(
                        scanner::Scan::new(scanner::SliceSource::new(&map[..])),
                        extension.as_deref(),
                    )
                };
                drop(inner);

                match outcome.best {
                    Some((probability, caps)) => (probability, caps, have_max),
                    None => {
                        return if have_max {
                            self.no_type_found()
                        } else if at_eos {
                            self.not_enough_data(avail, true)
                        } else {
                            gst::debug!(
                                CAT,
                                "No caps found with {avail} bytes of data, waiting for more data"
                            );
                            Ok(())
                        };
                    }
                }
            }
        };

        if probability_value(probability) < self.min_probability() {
            gst::debug!(
                CAT,
                "Found caps {caps}, but probability {} is lower than the required minimum {}",
                probability_value(probability),
                self.min_probability()
            );
            // Below the maximum window, more data may still raise the
            // probability.
            return if have_max {
                self.no_type_found()
            } else {
                Ok(())
            };
        }

        self.type_found(probability, &caps);
        Ok(())
    }

    fn type_found(&self, probability: gst::TypeFindProbability, caps: &gst::Caps) {
        // Leave typefind mode before emitting, in case a signal handler
        // seeks; the payload travels to stop_typefinding with us.
        let state = self.take_typefind_mode();
        self.emit_have_type(probability, caps);
        self.stop_typefinding(state);
    }

    /// Switches to passthrough mode, handing the typefind payload (if any)
    /// to the caller.
    fn take_typefind_mode(&self) -> Option<TypeFindMode> {
        let mut inner = self.inner();
        match std::mem::replace(&mut inner.mode, Mode::Passthrough) {
            Mode::TypeFind(state) => Some(state),
            _ => None,
        }
    }

    fn not_enough_data(&self, avail: usize, at_eos: bool) -> Result<(), gst::FlowError> {
        if at_eos {
            gst::element_error!(
                &self.obj(),
                gst::StreamError::TypeNotFound,
                ("Stream doesn't contain enough data."),
                ["Can't typefind stream"]
            );
            Err(gst::FlowError::Error)
        } else {
            gst::debug!(CAT, "Not enough data for typefinding yet ({avail} bytes)");
            Ok(())
        }
    }

    /// Posts the type-not-found error and returns the flow error it implies.
    fn report_type_not_found(&self) -> gst::FlowError {
        gst::element_error!(
            &self.obj(),
            gst::StreamError::TypeNotFound,
            ("Failed to determine type of stream"),
            ["Can't typefind stream"]
        );
        gst::FlowError::Error
    }

    fn no_type_found(&self) -> Result<(), gst::FlowError> {
        let err = self.report_type_not_found();
        // Nothing is collected without found caps, but the payload must not
        // outlive the typefind mode.
        let state = self.take_typefind_mode();
        self.stop_typefinding(state);
        Err(err)
    }

    /// Typefind the pull-mode data source, falling back to guessing from the
    /// stream extension when content detection fails or is not confident
    /// enough.
    fn typefind_pull(&self) -> Result<(), gst::FlowError> {
        let extension = self.extension(&self.sinkpad);

        let mut found = self
            .force_caps
            .lock()
            .unwrap()
            .clone()
            .map(|caps| (gst::TypeFindProbability::Maximum, caps));

        if found.is_none() {
            let Some(duration) = self.sinkpad.peer_query_duration::<gst::format::Bytes>() else {
                gst::warning!(CAT, "Could not query upstream length!");
                // Deliberately report TYPE_NOT_FOUND so the bus carries
                // the reason, not only the generic stream error that
                // pausing the task produces.
                return Err(self.report_type_not_found());
            };
            let size = *duration;
            gst::debug!(CAT, "Typefinding {size} bytes in pull mode");

            if size == 0 {
                // Wording kept byte-identical for bus compatibility with
                // the element this one replaces.
                gst::element_error!(
                    &self.obj(),
                    gst::StreamError::TypeNotFound,
                    ("Stream contains no data."),
                    ["Can't typefind empty stream"]
                );
                return Err(gst::FlowError::Error);
            }

            let outcome = scanner::run(
                scanner::Scan::new(scanner::PullSource::new(&self.sinkpad, size)),
                extension.as_deref(),
            );
            // Flushing is not a read failure: propagate it so the task
            // pauses silently (no error, no extension guessing), whether or
            // not a best result exists.
            if outcome.flow_error == Err(gst::FlowError::Flushing) {
                return Err(gst::FlowError::Flushing);
            }
            // A fatal read error inside the scan already discarded the
            // partial results, so an error reported alongside a result is
            // only the end-of-data marker converted at the end of the round
            // (the original scan aborts here even with caps found, losing
            // detections whenever a typefinder reads past the end of a
            // small file; deliberately not mirrored). Without a result, extension guessing and the
            // type-not-found report below still run, so the error does not
            // need to abort here either.
            if let (None, Err(err)) = (&outcome.best, outcome.flow_error) {
                gst::debug!(CAT, "Read error while typefinding: {err}");
            }
            found = outcome.best;
        }

        if !found
            .as_ref()
            .is_some_and(|(probability, _)| self.confident(*probability))
        {
            gst::debug!(CAT, "Trying to guess using extension");
            found = extension
                .as_deref()
                .and_then(scanner::caps_for_extension)
                .map(|caps| (gst::TypeFindProbability::Maximum, caps));
        }

        match found.filter(|(probability, _)| self.confident(*probability)) {
            Some((probability, caps)) => {
                self.type_found(probability, &caps);
                Ok(())
            }
            // Mode stays TypeFind: the caller's pause_task_for pauses the
            // task.
            None => Err(self.report_type_not_found()),
        }
    }

    fn confident(&self, probability: gst::TypeFindProbability) -> bool {
        probability_value(probability) >= self.min_probability()
    }

    // ---- URI extension ------------------------------------------------

    /// Extension of the stream URI, used to prioritize (or guess) the media
    /// type (see [`uri_extension`]).
    fn extension(&self, pad: &gst::Pad) -> Option<String> {
        let mut query = gst::query::Uri::new();
        if !pad.peer_query(&mut query) {
            gst::debug!(CAT, "Failed to query peer URI");
            return None;
        }
        let uri = query.uri()?;
        if let Some(redirect) = query.uri_redirection() {
            gst::debug!(CAT, "Stream was redirected to {redirect}");
        }

        let extension = uri_extension(&uri)?;
        gst::debug!(CAT, "Found extension {extension} for {uri}");
        Some(extension)
    }

    // ---- task loop ----------------------------------------------------

    fn loop_task(&self) {
        let element = self.obj();
        let src = &self.srcpad;
        let sink = &self.sinkpad;

        // Snapshot what this iteration pushes and release the lock before
        // pushing: entering downstream pads may re-enter this element.
        let mut inner = self.inner();
        let stream_start = inner.pull.as_mut().and_then(|pull| {
            pull.need_stream_start.then(|| {
                pull.need_stream_start = false;
                (pull.seqnum, src.create_stream_id(&*element, Some("")))
            })
        });
        // Only consume the pending segment once the data can flow.
        let segment = match &mut inner.mode {
            Mode::Passthrough => inner.pull.as_mut().and_then(|pull| {
                pull.need_segment.then(|| {
                    pull.need_segment = false;
                    (pull.seqnum, pull.segment.clone())
                })
            }),
            _ => None,
        };
        let typefinding = inner.mode.is_typefind();
        drop(inner);

        if let Some((seqnum, stream_id)) = stream_start {
            let event = gst::event::StreamStart::builder(&stream_id)
                .group_id(gst::GroupId::next())
                .seqnum_if_some(seqnum)
                .build();
            gst::debug!(CAT, "Pushing STREAM_START");
            src.push_event(event);
        }

        if typefinding {
            if let Err(err) = self.typefind_pull() {
                self.pause_task_for(err);
            }
        } else if self.inner().mode.is_error() {
            self.pause_task_for(gst::FlowError::Error);
        } else {
            if let Some((seqnum, segment)) = segment {
                let event = gst::event::Segment::builder(&segment)
                    .seqnum_if_some(seqnum)
                    .build();
                src.push_event(event);
            }

            // The pull state is gone when the sink pad was deactivated
            // while the task was still running.
            let Some(offset) = self.inner().pull.as_ref().map(|pull| pull.offset) else {
                sink.pause_task().ok();
                return;
            };

            match sink.pull_range(offset, PULL_BLOCK_SIZE) {
                Ok(buffer) => {
                    let size = buffer.size() as u64;
                    let mut inner = self.inner();
                    let Some(pull) = inner.pull.as_mut() else {
                        drop(inner);
                        sink.pause_task().ok();
                        return;
                    };
                    pull.offset += size;
                    drop(inner);

                    if let Err(err) = src.push(buffer) {
                        self.pause_task_for(err);
                    }
                }
                Err(err) => self.pause_task_for(err),
            }
        }
    }

    fn pause_task_for(&self, ret: gst::FlowError) {
        let sink = &self.sinkpad;
        let src = &self.srcpad;
        gst::log!(CAT, "Pausing task, reason {ret}");
        sink.pause_task().ok();

        let mut push_eos = false;

        if ret == gst::FlowError::Eos {
            let inner = self.inner();
            let segment_done = inner.pull.as_ref().and_then(|pull| {
                if pull.segment.flags().contains(gst::SegmentFlags::SEGMENT) {
                    // For segment playback report where we stopped: the
                    // segment stop if set, our read position otherwise.
                    let stop = pull
                        .segment
                        .stop()
                        .map(|stop| *stop as i64)
                        .unwrap_or(pull.offset as i64);
                    Some(stop)
                } else {
                    None
                }
            });
            drop(inner);

            if let Some(stop) = segment_done {
                gst::log!(CAT, "Sending segment done, at end of segment");
                let position = gst::GenericFormattedValue::new(gst::Format::Bytes, stop);
                let _ = self
                    .obj()
                    .post_message(gst::message::SegmentDone::builder(position).build());
                src.push_event(gst::event::SegmentDone::new(position));
            } else {
                push_eos = true;
            }
        } else if ret == gst::FlowError::NotLinked || is_fatal(ret) {
            // Fatal flow errors must be reported and end the stream.
            gst::element_error!(
                &self.obj(),
                gst::StreamError::Failed,
                ("Internal data stream error"),
                ["streaming stopped, reason {ret}"]
            );
            push_eos = true;
        }

        if push_eos {
            gst::log!(CAT, "Sending EOS, at end of stream");
            src.push_event(gst::event::Eos::new());
        }
    }

    // ---- pad function bodies ------------------------------------------

    fn chain(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut inner = self.inner();
        if let Mode::Error(err) = &inner.mode {
            return Err(*err);
        }
        // The caps cannot change anymore, and pushing downstream enters
        // other pads: release the guard first.
        if inner.mode.is_passthrough() {
            drop(inner);
            return self.srcpad.push(buffer);
        }
        let Mode::TypeFind(state) = &mut inner.mode else {
            // Unreachable: the mode cannot change while the guard is held.
            return Err(gst::FlowError::Error);
        };
        if state.initial_offset == gst::format::Buffers::OFFSET_NONE {
            state.initial_offset = buffer.offset();
        }
        state.adapter.push(buffer);
        drop(inner);

        self.chain_do_typefinding(true, false)?;

        // A handler of the have-type signal may have changed the mode.
        let inner = self.inner();
        if let Mode::Error(err) = &inner.mode {
            return Err(*err);
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        gst::debug!(CAT, "Got {event:?} in {:?}", self.inner().mode);

        // Every arm re-enters pads or re-locks: release the guard first.
        let (typefinding, error) = {
            let inner = self.inner();
            (inner.mode.is_typefind(), inner.mode.is_error())
        };
        if typefinding {
            self.sink_event_typefind(event)
        } else if error {
            false
        } else {
            self.srcpad.push_event(event)
        }
    }

    fn sink_event_typefind(&self, event: gst::Event) -> bool {
        let src = &self.srcpad;

        match event.type_() {
            gst::EventType::Caps => {
                let caps = match event.view() {
                    gst::EventView::Caps(caps_event) => caps_event.caps().to_owned(),
                    _ => return true,
                };

                // Don't operate on ANY caps.
                if !caps.is_any() {
                    // Leave typefind mode before emitting, in case a
                    // handler seeks.
                    let state = self.take_typefind_mode();
                    self.emit_have_type(gst::TypeFindProbability::Maximum, &caps);

                    // Upstream already knows the type, skip typefinding.
                    gst::debug!(
                        CAT,
                        "Skipping typefinding, using caps from upstream: {caps}"
                    );
                    self.stop_typefinding(state);
                }
                true
            }
            gst::EventType::Gap => {
                // TODO: GAP events received during typefinding should be
                // inserted in the stream at the right position between
                // buffers, but buffers are merged in the adapter. Dropping
                // the event only causes an implicit gap.
                gst::fixme!(CAT, "GAP events during typefinding not handled properly");
                true
            }
            gst::EventType::Eos => {
                gst::info!(CAT, "Got EOS and no type found yet");
                self.chain_do_typefinding(false, true).ok();
                src.push_event(event)
            }
            gst::EventType::FlushStop => {
                let recovered = {
                    let mut inner = self.inner();
                    let Mode::TypeFind(state) = &mut inner.mode else {
                        // The mode raced away: nothing to recover.
                        return src.push_event(event);
                    };
                    let recovered: Vec<gst::Event> = state
                        .cached_events
                        .iter()
                        .filter(|cached| {
                            cached.is_sticky()
                                && cached.type_() != gst::EventType::Segment
                                && cached.type_() != gst::EventType::Eos
                        })
                        .cloned()
                        .collect();
                    state.cached_events.clear();
                    state.adapter.clear();
                    recovered
                };
                for cached in recovered {
                    src.store_sticky_event(&cached).ok();
                }
                src.push_event(event)
            }
            _ => {
                // Events that would happen before the caps event are
                // forwarded directly; only events that must come after caps
                // are cached.
                if event.type_().into_glib() < gst::EventType::Caps.into_glib() {
                    src.push_event(event)
                } else {
                    gst::debug!(CAT, "Saving {:?} event to send later", event.type_());
                    let mut inner = self.inner();
                    let Mode::TypeFind(state) = &mut inner.mode else {
                        // The mode raced away (have-type handler): forward
                        // the event rather than drop it.
                        return src.push_event(event);
                    };
                    state.cached_events.push(event);
                    true
                }
            }
        }
    }

    fn getrange(
        &self,
        offset: u64,
        params: Option<&mut gst::BufferRef>,
        length: u32,
    ) -> Result<gst::PadGetRangeSuccess, gst::FlowError> {
        // A pull against an element that already failed reports that
        // failure instead of opening a proxy to upstream.
        if let Mode::Error(err) = &self.inner().mode {
            return Err(*err);
        }

        // A caller-provided buffer must be filled in place: forward it to
        // the peer so the whole upstream chain can fill it without a copy.
        // If it is smaller than the requested length (which the fill call
        // aborts on), pull a fresh buffer and let the copy path handle it.
        if let Some(buffer) = params.filter(|b| b.size() >= length as usize) {
            self.sinkpad.pull_range_fill(offset, buffer, length)?;
            return Ok(gst::PadGetRangeSuccess::FilledBuffer);
        }

        Ok(gst::PadGetRangeSuccess::NewBuffer(
            self.sinkpad.pull_range(offset, length)?,
        ))
    }

    fn src_event(&self, event: gst::Event) -> bool {
        // Always forward RECONFIGURE events upstream.
        if event.type_() == gst::EventType::Reconfigure {
            return self.sinkpad.push_event(event);
        }

        if !self.inner().mode.is_passthrough() {
            gst::log!(CAT, "Still typefinding. Not passing event upstream");
            return false;
        }

        // Only handle seeks here if we drive the pipeline (pull mode).
        if event.type_() == gst::EventType::Seek && self.inner().pull.is_some() {
            return self.handle_seek(event);
        }

        self.sinkpad.push_event(event)
    }

    fn handle_seek(&self, event: gst::Event) -> bool {
        let sink = &self.sinkpad;
        let src = &self.srcpad;

        let (rate, flags, start_type, start, stop_type, stop) = match event.view() {
            gst::EventView::Seek(seek) => seek.get(),
            _ => return false,
        };

        // We can only seek on bytes.
        if start.format() != gst::Format::Bytes || stop.format() != gst::Format::Bytes {
            gst::debug!(CAT, "Can only seek on BYTES");
            return false;
        }

        let flush = flags.contains(gst::SeekFlags::FLUSH);

        // Configure the seek on a copy of the segment: the old one is still
        // needed to close the current segment.
        let mut seeksegment = {
            let inner = self.inner();
            let Some(pull) = inner.pull.as_ref() else {
                return false;
            };
            pull.segment.clone()
        };

        gst::debug!(CAT, "Configuring seek");
        let _ = seeksegment.do_seek(rate, flags, start_type, start, stop_type, stop);
        gst::debug!(CAT, "New segment {seeksegment:?}");

        if flush {
            gst::debug!(CAT, "Starting flush");
            sink.push_event(gst::event::FlushStart::new());
            src.push_event(gst::event::FlushStart::new());
        } else {
            gst::debug!(CAT, "Non-flushing seek, pausing task");
            sink.pause_task().ok();
        }

        // Grab the stream lock so that streaming cannot continue. For
        // non-flushing seeks while the element is paused this can block
        // forever.
        gst::debug!(CAT, "Waiting for streaming to stop");
        let _stream_lock = sink.stream_lock();

        if flush {
            gst::debug!(CAT, "Stopping flush");
            sink.push_event(gst::event::FlushStop::new(true));
            src.push_event(gst::event::FlushStop::new(true));
        }

        // Commit the new seek segment.
        gst::debug!(CAT, "Committing new seek segment");
        let mut inner = self.inner();
        let Some(pull) = inner.pull.as_mut() else {
            return false;
        };
        pull.offset = seeksegment.start().map(|start| *start).unwrap_or(u64::MAX);
        pull.segment = seeksegment;
        pull.need_segment = true;
        pull.seqnum = Some(event.seqnum());
        let segment_start = pull.segment.flags().contains(gst::SegmentFlags::SEGMENT);
        let segment_start_offset = pull.offset as i64;
        drop(inner);

        if segment_start {
            let _ = self.obj().post_message(
                gst::message::SegmentStart::builder(gst::GenericFormattedValue::new(
                    gst::Format::Bytes,
                    segment_start_offset,
                ))
                .build(),
            );
        }

        // Restart our task, it might have been stopped by the flush.
        let element = (*self.obj()).clone();
        sink.start_task(move || element.imp().loop_task()).is_ok()
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        gst::debug!(CAT, obj = pad, "Handling src query {query:?}");

        match query.type_() {
            // Mirror the C src_query: forward the upstream's answer. The
            // default handler forwards Scheduling only on pads carrying the
            // proxy-scheduling flag, which only ghost pads ever set.
            gst::QueryType::Scheduling => self.sinkpad.peer_query(query),
            gst::QueryType::Caps => {
                // We can hijack the caps query if we already typefound.
                let Some(caps) = self.caps() else {
                    return self.sinkpad.peer_query(query);
                };

                gst::debug!(CAT, "Got caps query, responding with {caps}");
                if let QueryViewMut::Caps(q) = query.view_mut() {
                    q.set_result(&caps);
                }
                true
            }
            gst::QueryType::Position => {
                if !self.sinkpad.peer_query(query) {
                    return false;
                }

                if let QueryViewMut::Position(q) = query.view_mut() {
                    let position = q.result();
                    if position.format() == gst::Format::Bytes {
                        // FIXME: assumes there is no discontinuity in the queue.
                        let buffered = match &self.inner().mode {
                            Mode::TypeFind(state) => state.adapter.available() as i64,
                            _ => 0,
                        };
                        q.set(gst::GenericFormattedValue::new(
                            gst::Format::Bytes,
                            (position.value() - buffered).max(0),
                        ));
                    }
                }
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TypeFind2 {
    const NAME: &'static str = "GstRsTypeFind2";
    type Type = super::TypeFind2;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let sinkpad = gst::Pad::builder_from_template(&klass.pad_template("sink").unwrap())
            .activate_function(|pad, parent| {
                TypeFind2::catch_panic_pad_function(
                    parent,
                    || Err(gst::loggable_error!(CAT, "Panic activating sink pad")),
                    |imp| imp.activate_sink(pad),
                )
            })
            .activatemode_function(|_pad, parent, mode, active| {
                TypeFind2::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating sink pad with mode"
                        ))
                    },
                    |imp| imp.activate_sink_mode(mode, active),
                )
            })
            .chain_function(|_pad, parent, buffer| {
                TypeFind2::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.chain(buffer),
                )
            })
            .event_function(|_pad, parent, event| {
                TypeFind2::catch_panic_pad_function(parent, || false, |imp| imp.sink_event(event))
            })
            .build();

        let srcpad = gst::Pad::builder_from_template(&klass.pad_template("src").unwrap())
            .activatemode_function(|_pad, parent, mode, active| {
                TypeFind2::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating src pad with mode"
                        ))
                    },
                    |imp| imp.activate_src_mode(mode, active),
                )
            })
            .getrange_function(|_pad, parent, offset, params, length| {
                TypeFind2::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.getrange(offset, params, length),
                )
            })
            .event_function(|_pad, parent, event| {
                TypeFind2::catch_panic_pad_function(parent, || false, |imp| imp.src_event(event))
            })
            .query_function(|pad, parent, query| {
                TypeFind2::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.src_query(pad, query),
                )
            })
            .build();

        // The sink pad answers allocation queries on behalf of upstream: the
        // data is only consumed once typefinding is over, so upstream's own
        // allocator settings must not be hidden behind this element.
        sinkpad.set_pad_flags(gst::PadFlags::PROXY_ALLOCATION);
        srcpad.use_fixed_caps();

        Self {
            sinkpad,
            srcpad,
            inner: Mutex::new(Inner::default()),
            force_caps: Mutex::new(None),
            minimum: Mutex::new(DEFAULT_MINIMUM),
            caps: Mutex::new(None),
        }
    }
}

impl TypeFind2 {
    fn activate_sink(&self, pad: &gst::Pad) -> Result<(), gst::LoggableError> {
        let mut query = gst::query::Scheduling::new();
        let pull_mode = pad.peer_query(&mut query)
            && query
                .has_scheduling_mode_with_flags(gst::PadMode::Pull, gst::SchedulingFlags::SEEKABLE);

        if pull_mode && pad.activate_mode(gst::PadMode::Pull, true).is_ok() {
            // Only start our task if we ourselves decided to start in pull mode.
            let element = (*self.obj()).clone();
            return pad
                .start_task(move || element.imp().loop_task())
                .map_err(gst::LoggableError::from);
        }

        pad.activate_mode(gst::PadMode::Push, true)
            .map_err(gst::LoggableError::from)
    }

    fn activate_sink_mode(
        &self,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        match mode {
            gst::PadMode::Pull => {
                if active {
                    let mut inner = self.inner();
                    inner.pull = Some(PullState {
                        segment: gst::FormattedSegment::new(),
                        offset: 0,
                        seqnum: None,
                        need_segment: true,
                        need_stream_start: true,
                    });
                } else {
                    self.sinkpad.stop_task().ok();
                    self.inner().pull = None;
                }
                Ok(())
            }
            gst::PadMode::Push => {
                if active {
                    self.start_typefinding();
                } else {
                    let state = self.take_typefind_mode();
                    self.stop_typefinding(state);
                }
                Ok(())
            }
            _ => Err(gst::loggable_error!(CAT, "Unsupported pad mode {mode:?}")),
        }
    }

    fn activate_src_mode(
        &self,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        match mode {
            gst::PadMode::Pull => {
                // Make sure our task stops pushing: activation might happen
                // from the streaming thread, so the task can't be stopped
                // synchronously.
                self.sinkpad.pause_task().ok();
                self.sinkpad
                    .activate_mode(mode, active)
                    .map_err(gst::LoggableError::from)
            }
            _ => Ok(()),
        }
    }
}

#[glib::derived_properties]
impl ObjectImpl for TypeFind2 {
    fn constructed(&self) {
        self.parent_constructed();

        let element = self.obj();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                glib::subclass::Signal::builder("have-type")
                    .param_types([u32::static_type(), gst::Caps::static_type()])
                    .return_type::<()>()
                    .class_handler(|args| {
                        let element = args[0].get::<super::TypeFind2>().expect("signal arg");
                        let caps = args[2].get::<gst::Caps>().expect("signal arg");

                        let imp = element.imp();

                        // Downstream pulls from us, it gets the type through
                        // caps queries.
                        if imp.srcpad.mode() != gst::PadMode::Pull {
                            // Reuse the sticky caps event stored before the
                            // signal was emitted if it still matches, instead of
                            // allocating an equivalent one and possibly causing
                            // renegotiation downstream.
                            let caps_ref: &gst::CapsRef = caps.as_ref();
                            let event = imp
                                .srcpad
                                .sticky_event::<gst::event::Caps>(0)
                                .filter(|event| event.caps() == caps_ref)
                                .map(gst::Event::from)
                                .unwrap_or_else(|| gst::event::Caps::new(&caps));

                            imp.srcpad.push_event(event);
                        }

                        None
                    })
                    .build(),
            ]
        });

        &SIGNALS
    }
}

impl GstObjectImpl for TypeFind2 {}

impl ElementImpl for TypeFind2 {
    // TODO: call mark_as_untrusted_aware() in class_init; the setter is
    // available in the 0.26 bindings behind the v1_30 feature, not in the
    // pinned 0.25.
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "TypeFind2",
                "Generic",
                "Finds the media type of a stream",
                "Edward Hervey <edward@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &gst::Caps::new_any(),
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &gst::Caps::new_any(),
                )
                .unwrap(),
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let ret = self.parent_change_state(transition);

        if matches!(
            transition,
            gst::StateChange::PausedToReady | gst::StateChange::ReadyToNull
        ) {
            let mut inner = self.inner();
            *self.caps.lock().unwrap() = None;
            // The fresh payload also drops buffered data; the element this
            // one replaces kept it.
            inner.mode = Mode::TypeFind(TypeFindMode::default());
        }

        ret
    }
}

#[cfg(test)]
mod tests {
    use super::uri_extension;

    #[test]
    fn uri_extension_cases() {
        // Mirrors the extension extraction of the element this one
        // replaces; DIVERGENCE marks deliberate differences.
        const CASES: &[(&str, Option<&str>)] = &[
            ("file:///path/x.wav", Some("wav")),
            ("file:///path/.hidden.mp4", Some("mp4")),
            ("file:///path/archive.tar.gz", Some("gz")),
            ("http://host/dir/a.mkv?x=1#frag", Some("mkv")),
            ("https://cdn.example.org/movie.TS", Some("ts")),
            // Relative URIs are rejected; basename of the path component
            // only (authority excluded).
            ("relative/path/file.mp3", None),
            ("http://example.com", None),
            ("http://example.com/README", None),
            ("file:///dir/", None),
            ("file:///file.", None),
            ("file:///no-extension", None),
            ("data:audio/mpeg;base64,AAAA", None),
            // DIVERGENCE: the replaced element compares extensions
            // case-sensitively (lowercasing applies to every case above).
            // DIVERGENCE: percent-decoding the path first would yield
            // "mp4".
            ("file:///file.mp%34", Some("mp%34")),
            // DIVERGENCE: a basename that treats '\' as an ordinary
            // character would yield the extension "a\b" below.
            ("file:///dir\\file.a\\b", None),
        ];

        for (uri, expected) in CASES {
            assert_eq!(uri_extension(uri).as_deref(), *expected, "uri: {uri}");
        }
    }
}
