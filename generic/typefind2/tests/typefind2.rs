// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::glib::translate::IntoGlib;
use gst::prelude::*;
use gsttypefind2::TypeFind2;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"TF2M";
const LOW_MAGIC: &[u8; 4] = b"LOWP";
const TEST_CAPS: &str = "test/tf2-itest";
const LOW_CAPS: &str = "test/tf2-lowprob";
// Mirrors the element's TYPEFIND_MAX_SIZE.
const TYPEFIND_MAX_SIZE: usize = 128 * 1024;

// Generous upper bound for every async wait: streaming completes in ms.
const TIMEOUT: Duration = Duration::from_secs(10);

fn max_probability() -> u32 {
    gst::TypeFindProbability::Maximum.into_glib() as u32
}

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gsttypefind2::plugin_register_static().unwrap();

        let caps = gst::Caps::builder(TEST_CAPS).build();
        let probe_caps = caps.clone();
        gst::TypeFind::register(
            None,
            "tf2_itest",
            gst::Rank::NONE + 1,
            Some("tf2ext"),
            Some(&caps),
            move |find| {
                if find.peek_var(0, MAGIC.len() as u32) == Some(&MAGIC[..]) {
                    find.suggest(gst::TypeFindProbability::Maximum, &probe_caps);
                }
            },
        )
        .unwrap();

        // Suggests a probability below the default `minimum` of 1.
        let caps = gst::Caps::builder(LOW_CAPS).build();
        let probe_caps = caps.clone();
        gst::TypeFind::register(
            None,
            "tf2_itest_lowprob",
            gst::Rank::NONE + 1,
            Some("tf2low"),
            Some(&caps),
            move |find| {
                if find.peek_var(0, LOW_MAGIC.len() as u32) == Some(&LOW_MAGIC[..]) {
                    find.suggest(gst::TypeFindProbability::Possible, &probe_caps);
                }
            },
        )
        .unwrap();
    });
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while !predicate() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

fn caps_name(caps: &gst::Caps) -> String {
    caps.structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default()
}

/// Waits for a TYPE_NOT_FOUND error. On failure returns the other bus errors
/// seen meanwhile, for the assertion message.
fn type_not_found_message(element: &impl IsA<gst::Element>) -> Result<(), Vec<String>> {
    use glib::error::ErrorDomain;

    let bus = element.bus().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let mut other = Vec::new();
    while Instant::now() < deadline {
        if let Some(message) = bus.timed_pop(Some(gst::ClockTime::from_mseconds(100)))
            && let gst::MessageView::Error(err) = message.view()
        {
            let error = err.error();
            if error.domain() == gst::StreamError::domain()
                && error.code() == gst::StreamError::TypeNotFound.code()
            {
                return Ok(());
            }
            other.push(error.to_string());
        }
    }
    Err(other)
}

fn assert_type_not_found(element: &impl IsA<gst::Element>) {
    let result = type_not_found_message(element);
    assert!(result.is_ok(), "expected TYPE_NOT_FOUND, got {result:?}");
}

fn assert_playing(pipeline: &gst::Pipeline) {
    pipeline.set_state(gst::State::Playing).unwrap();
    assert!(
        wait_until(|| pipeline.state(None).1 == gst::State::Playing),
        "pipeline did not reach PLAYING"
    );
}

fn make_typefind2() -> TypeFind2 {
    gst::ElementFactory::make("typefind2")
        .build()
        .unwrap()
        .downcast()
        .unwrap()
}

fn make_fakesink() -> gst::Element {
    gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap()
}

fn make_filesrc(path: &Path) -> gst::Element {
    gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap()
}

/// `filesrc ! typefind2 ! fakesink` with have-type captured, no state change.
fn pull_pipeline(path: &Path) -> (gst::Pipeline, TypeFind2, Found) {
    let pipeline = gst::Pipeline::builder().build();
    let filesrc = make_filesrc(path);
    let tf = make_typefind2();
    let fakesink = make_fakesink();
    let found = capture_have_type(&tf);

    pipeline
        .add_many([&filesrc, tf.as_ref(), &fakesink])
        .unwrap();
    filesrc.link(&tf).unwrap();
    tf.link(&fakesink).unwrap();
    (pipeline, tf, found)
}

/// Waits for the first have-type in pull mode; returns (probability, caps name).
fn first_found(found: &Found, why: &str) -> (u32, String) {
    assert!(
        wait_until(|| !found.lock().unwrap().is_empty()),
        "have-type was not emitted ({why})"
    );
    found.lock().unwrap()[0].clone()
}

fn magic_buf(magic: &[u8], len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    data[..magic.len()].copy_from_slice(magic);
    data
}

fn seek_to_zero() -> gst::Event {
    use gst::format::BytesFormatConstructor;

    gst::event::Seek::new(
        1.0,
        gst::SeekFlags::FLUSH,
        gst::SeekType::Set,
        0u64.bytes(),
        gst::SeekType::None,
        0u64.bytes(),
    )
}

// Zero-filled file with the maximum-probability magic at the start: detection
// can only come from the `tf2_itest` factory.
fn magic_file(name: &str) -> (PathBuf, Vec<u8>) {
    let mut data = vec![0u8; 12 * 1024];
    data[..MAGIC.len()].copy_from_slice(MAGIC);
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, &data).unwrap();
    (path, data)
}

// Recorded have-type emissions: (probability, caps name).
type Found = Arc<Mutex<Vec<(u32, String)>>>;

fn capture_have_type(tf: &TypeFind2) -> Found {
    let found = Arc::new(Mutex::new(Vec::new()));
    tf.connect_have_type({
        let found = found.clone();
        move |_, probability, caps| {
            found.lock().unwrap().push((probability, caps_name(caps)));
        }
    });
    found
}

// Push-mode tests drive the element with a harness: sources that send a
// caps event themselves (appsrc) would short-circuit typefinding by design,
// the interesting case is a bare pad pushing data with no caps.
struct PushTest {
    harness: gst_check::Harness,
    tf: TypeFind2,
    found: Found,
    segment_sent: bool,
}

impl PushTest {
    fn new() -> Self {
        init();

        let mut harness = gst_check::Harness::new("typefind2");
        let tf = harness.element().unwrap().downcast::<TypeFind2>().unwrap();
        // A standalone element has no bus; give it one to watch for errors.
        tf.set_bus(Some(&gst::Bus::new()));

        let found = capture_have_type(&tf);

        harness.play();

        Self {
            harness,
            tf,
            found,
            segment_sent: false,
        }
    }

    fn push_data(&mut self, data: &[u8]) -> Result<gst::FlowSuccess, gst::FlowError> {
        // The harness injects a stream-start event but no segment (that only
        // comes with set_src_caps()); data without one warns, and CI runs
        // tests with G_DEBUG=fatal_warnings. Sent lazily so that upstream
        // caps, when a test sends them, still precede it as in real streams.
        if !self.segment_sent {
            let segment = gst::FormattedSegment::<gst::format::Bytes>::new();
            assert!(self.harness.push_event(gst::event::Segment::new(&segment)));
            self.segment_sent = true;
        }
        self.harness.push(gst::Buffer::from_slice(data.to_vec()))
    }

    fn push_event(&mut self, event: gst::Event) -> bool {
        self.harness.push_event(event)
    }

    fn push_ok(&mut self, data: &[u8]) {
        assert_eq!(
            self.push_data(data),
            Ok(gst::FlowSuccess::Ok),
            "push should not break flow"
        );
    }

    /// Waits for have-type and checks the first suggestion.
    fn assert_detected(&self, probability: u32, caps: &str) {
        let found = self.wait_found(1);
        assert_eq!(found[0], (probability, caps.to_string()));
    }

    /// Checks the caps event reaches downstream, then the buffered data.
    fn assert_forwarded(&mut self, size: usize) {
        self.next_event(gst::EventType::Caps);
        let buffer = self.harness.pull().unwrap();
        assert_eq!(buffer.size(), size);
    }

    // The harness injects its own stream-start event into the stream; skip
    // over such passthrough events and return the first event of the wanted
    // type.
    fn next_event(&mut self, wanted: gst::EventType) -> gst::Event {
        loop {
            let event = self.harness.pull_event().unwrap();
            if event.type_() == wanted {
                return event;
            }
            assert!(
                matches!(
                    event.type_(),
                    gst::EventType::StreamStart | gst::EventType::Segment
                ),
                "unexpected event {event:?} while waiting for {wanted:?}"
            );
        }
    }

    fn wait_found(&self, count: usize) -> Vec<(u32, String)> {
        assert!(
            wait_until(|| self.found.lock().unwrap().len() >= count),
            "have-type was not emitted {count} time(s), got {:?}",
            self.found.lock().unwrap()
        );
        self.found.lock().unwrap().clone()
    }
}

#[test]
fn push_detect_and_pass_through() {
    let mut test = PushTest::new();

    // The buffered data and everything after it flows through unchanged.
    test.push_ok(&magic_buf(MAGIC, 4096));

    // The found caps are visible through the property and downstream.
    let caps = test.tf.caps().expect("caps property");
    assert_eq!(caps_name(&caps), TEST_CAPS);

    test.assert_detected(max_probability(), TEST_CAPS);
    test.assert_forwarded(4096);

    test.push_ok(&[0u8; 1024]);
    let buffer = test.harness.pull().unwrap();
    assert_eq!(buffer.size(), 1024);

    assert!(test.push_event(gst::event::Eos::new()));
    test.next_event(gst::EventType::Eos);
}

#[test]
fn push_detect_at_eos() {
    let mut test = PushTest::new();

    // Below the minimum typefinding size: only the final scan at EOS runs.
    test.push_ok(&magic_buf(MAGIC, 8));
    assert!(test.push_event(gst::event::Eos::new()));

    test.assert_detected(max_probability(), TEST_CAPS);
    test.assert_forwarded(8);
    test.next_event(gst::EventType::Eos);
}

#[test]
fn push_no_type_found() {
    let mut test = PushTest::new();

    // Past the maximum scan window with data no factory claims.
    let data = vec![0u8; TYPEFIND_MAX_SIZE + 4096];
    assert_eq!(test.push_data(&data), Err(gst::FlowError::Error));
    assert_type_not_found(&test.tf);
    assert!(test.tf.caps().is_none());
}

#[test]
fn push_low_probability_waits_for_max_data() {
    let mut test = PushTest::new();
    test.tf.set_property("minimum", max_probability());

    test.push_ok(&magic_buf(LOW_MAGIC, 4096));

    // Below the maximum window: wait for more data, no signal, no error.
    assert!(test.found.lock().unwrap().is_empty());
    assert!(test.tf.caps().is_none());

    // Past the maximum window: the low probability is final and fails.
    // (Total pushed: 4096 + TYPEFIND_MAX_SIZE, above the window.)
    assert_eq!(
        test.push_data(&vec![0u8; TYPEFIND_MAX_SIZE]),
        Err(gst::FlowError::Error)
    );
    assert_type_not_found(&test.tf);
    assert!(test.found.lock().unwrap().is_empty());
}

#[test]
fn push_low_probability_accepted_when_minimum_allows() {
    let mut test = PushTest::new();

    // Default minimum is 1, below the Possible suggestion.
    test.push_ok(&magic_buf(LOW_MAGIC, 4096));

    test.assert_detected(
        gst::TypeFindProbability::Possible.into_glib() as u32,
        LOW_CAPS,
    );
    test.assert_forwarded(4096);
}

#[test]
fn push_upstream_caps_skip_typefinding() {
    let mut test = PushTest::new();

    let upstream = gst::Caps::builder("test/tf2-upstream").build();
    assert!(test.push_event(gst::event::Caps::new(&upstream)));

    // No data needed: the caps event itself determines the type.
    test.assert_detected(max_probability(), "test/tf2-upstream");

    // Data is passed through without being scanned.
    test.push_ok(&[0u8; 4096]);
    let buffer = test.harness.pull().unwrap();
    assert_eq!(buffer.size(), 4096);
}

#[test]
fn push_force_caps() {
    let mut test = PushTest::new();
    let forced = gst::Caps::builder("test/tf2-forced").build();
    test.tf.set_property("force-caps", &forced);

    test.push_ok(&[0u8; 4096]);

    test.assert_detected(max_probability(), "test/tf2-forced");
    test.assert_forwarded(4096);
}

// Events that must come after caps are cached while typefinding and
// forwarded between the caps event and the merged data once the type is
// found.
#[test]
fn cached_event_forwarded_after_type() {
    let mut test = PushTest::new();

    let mut tags = gst::TagList::new();
    tags.make_mut()
        .add::<gst::tags::Title>(&"cached", gst::TagMergeMode::Replace);
    assert!(test.push_event(gst::event::Tag::new(tags)));

    test.push_ok(&magic_buf(MAGIC, 4096));
    test.assert_detected(max_probability(), TEST_CAPS);

    test.next_event(gst::EventType::Caps);
    let event = test.harness.pull_event().unwrap();
    assert_eq!(event.type_(), gst::EventType::Tag);
    let buffer = test.harness.pull().unwrap();
    assert_eq!(buffer.size(), 4096);
}

// Deactivating push mode with buffered, unclassified data: the typefind
// payload is dropped, nothing errors and nothing leaks downstream.
#[test]
fn push_deactivation_drops_pending_data() {
    let mut test = PushTest::new();

    test.push_ok(&vec![0u8; 4096]);

    // The stream-start passes through during typefinding; the segment is
    // cached until typefinding ends (caps must precede the segment).
    test.next_event(gst::EventType::StreamStart);

    let sinkpad = test.tf.static_pad("sink").unwrap();
    assert!(sinkpad.activate_mode(gst::PadMode::Push, false).is_ok());
    assert!(test.tf.caps().is_none());
    // Nothing is expected and nothing can still arrive: deactivation
    // finished synchronously on this thread.
    assert!(test.harness.try_pull_event().is_none());
    assert!(test.harness.try_pull().is_none());
}

// A pull-mode downstream (parsebin, discoverer) reads our src pad through
// the getrange class function; it must forward to the peer source.
#[test]
fn getrange_forwards_to_peer() {
    init();

    let (path, _data) = magic_file("getrange.tf2ext");

    let tf = make_typefind2();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    // pull_range forwards to the peer's getrange, which needs the file open.
    filesrc.set_state(gst::State::Ready).unwrap();
    let sinkpad = tf.static_pad("sink").unwrap().to_owned();
    filesrc.static_pad("src").unwrap().link(&sinkpad).unwrap();
    sinkpad
        .activate_mode(gst::PadMode::Pull, true)
        .expect("sink pull activation");

    let srcpad = tf.static_pad("src").unwrap().to_owned();
    srcpad
        .activate_mode(gst::PadMode::Pull, true)
        .expect("pull activation");
    let buffer = srcpad
        .range(0, 1024)
        .expect("getrange on pull-active src pad");
    assert_eq!(buffer.size(), 1024);
    assert_eq!(
        &buffer.map_readable().unwrap().as_slice()[..MAGIC.len()],
        MAGIC
    );

    srcpad.activate_mode(gst::PadMode::Pull, false).ok();
    filesrc.set_state(gst::State::Null).ok();
}

// A caller handing over a pre-allocated buffer (as internal typefind
// helpers do) gets its contents back in that buffer: the fill request is
// forwarded upstream and the peer fills the memory in place.
#[test]
fn getrange_fills_provided_buffer() {
    init();

    let (path, _data) = magic_file("getrange_fill.tf2ext");

    let tf = make_typefind2();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    filesrc.set_state(gst::State::Ready).unwrap();
    let sinkpad = tf.static_pad("sink").unwrap().to_owned();
    filesrc.static_pad("src").unwrap().link(&sinkpad).unwrap();
    sinkpad
        .activate_mode(gst::PadMode::Pull, true)
        .expect("sink pull activation");

    let srcpad = tf.static_pad("src").unwrap().to_owned();
    srcpad
        .activate_mode(gst::PadMode::Pull, true)
        .expect("pull activation");

    // Freshly sliced buffer: private memory, writable in place.
    let mut buffer = gst::Buffer::from_slice(vec![0u8; 1024]);
    srcpad
        .range_fill(0, buffer.make_mut(), 1024)
        .expect("getrange into provided buffer");
    assert_eq!(
        &buffer.map_readable().unwrap().as_slice()[..MAGIC.len()],
        MAGIC
    );

    srcpad.activate_mode(gst::PadMode::Pull, false).ok();
    filesrc.set_state(gst::State::Null).ok();
}

#[test]
fn pull_seek_replays_data() {
    init();

    let (path, data) = magic_file("seek.tf2ext");

    let (pipeline, tf, _found) = pull_pipeline(&path);

    let forwarded = Arc::new(AtomicU64::new(0));
    let eos_count = Arc::new(AtomicU64::new(0));
    tf.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
        {
            let forwarded = forwarded.clone();
            let eos_count = eos_count.clone();
            move |_, info| {
                if let Some(buffer) = info.buffer() {
                    forwarded.fetch_add(buffer.size() as u64, Ordering::Relaxed);
                } else if let Some(event) = info.event()
                    && event.type_() == gst::EventType::Eos
                {
                    eos_count.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            }
        },
    );

    let total = data.len() as u64;

    // First pass: the whole file is forwarded and EOS ends it.
    assert_playing(&pipeline);
    assert!(
        wait_until(
            || eos_count.load(Ordering::Relaxed) >= 1 && forwarded.load(Ordering::Relaxed) >= total
        ),
        "first pass stalled at {} of {total} bytes, {} EOS",
        forwarded.load(Ordering::Relaxed),
        eos_count.load(Ordering::Relaxed),
    );
    assert_eq!(forwarded.load(Ordering::Relaxed), total);

    // A byte seek back to 0 must be handled and the data replayed. Sending it
    // on the src pad runs the element's src_event handler, as a sink would.
    assert!(tf.static_pad("src").unwrap().send_event(seek_to_zero()));

    // EOS trails the last buffer on the same pad, so the second EOS also
    // closes the replay pass.
    assert!(
        wait_until(|| eos_count.load(Ordering::Relaxed) >= 2
            && forwarded.load(Ordering::Relaxed) >= 2 * total),
        "replay stalled at {} of {} bytes, {} EOS",
        forwarded.load(Ordering::Relaxed),
        2 * total,
        eos_count.load(Ordering::Relaxed),
    );
    assert_eq!(forwarded.load(Ordering::Relaxed), 2 * total);

    pipeline.set_state(gst::State::Null).ok();
}

#[test]
fn pull_detect_with_filesrc() {
    init();

    let (path, data) = magic_file("pull.tf2ext");

    let (pipeline, tf, found) = pull_pipeline(&path);

    let forwarded = Arc::new(AtomicU64::new(0));
    tf.static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, {
            let forwarded = forwarded.clone();
            move |_, info| {
                if let Some(buffer) = info.buffer() {
                    forwarded.fetch_add(buffer.size() as u64, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            }
        });

    assert_playing(&pipeline);

    assert_eq!(
        first_found(&found, "in pull mode"),
        (max_probability(), TEST_CAPS.to_string())
    );

    // The whole file is eventually forwarded.
    assert!(
        wait_until(|| forwarded.load(Ordering::Relaxed) >= data.len() as u64),
        "only {} bytes forwarded",
        forwarded.load(Ordering::Relaxed)
    );

    pipeline.set_state(gst::State::Null).ok();
    std::fs::remove_file(&path).ok();
}

#[test]
fn pull_no_type_found_rejects_seek() {
    init();

    // Zeroes match no typefinder and "bin" maps to no extension guess.
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fail.bin");
    std::fs::write(&path, vec![0u8; 12 * 1024]).unwrap();

    let (pipeline, tf, found) = pull_pipeline(&path);

    // The pull task starts at pad activation (READY->PAUSED), so the
    // failure is posted before the pipeline can commit PLAYING and aborts the
    // async state change. Drive it anyway; the error is what the test checks.
    pipeline.set_state(gst::State::Playing).unwrap();

    // The element's own bus is the bin's internal child bus, created without
    // async polling; the pipeline bus is the one that can be waited on.
    assert_type_not_found(&pipeline);
    assert!(
        found.lock().unwrap().is_empty(),
        "have-type must not be emitted when typefinding fails"
    );

    // The mode stays TypeFind after a pull failure, so a seek is rejected
    // while still typefinding.
    assert!(
        !tf.static_pad("src").unwrap().send_event(seek_to_zero()),
        "seek must be rejected while still typefinding"
    );

    pipeline.set_state(gst::State::Null).ok();
    std::fs::remove_file(&path).ok();
}

// PCM s16le mono 8 kHz: header plus a block of silence, enough for the
// registered typefindfunctions sniffer to report maximum probability.
fn wav_bytes() -> Vec<u8> {
    let pcm = [0u8; 512];
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&8000u32.to_le_bytes());
    data.extend_from_slice(&16000u32.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    data.extend_from_slice(&pcm);
    data
}

#[test]
fn pull_detect_real_wav() {
    init();

    // Unlike the other pull tests this detects a real-world format, running
    // against the factories registered by gst-plugins-base.
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("real.wav");
    std::fs::write(&path, wav_bytes()).unwrap();

    let (pipeline, _tf, found) = pull_pipeline(&path);
    assert_playing(&pipeline);

    let (probability, name) = first_found(&found, "for the WAV file");
    assert_eq!(name, "audio/x-wav");
    assert!(probability >= gst::TypeFindProbability::Likely.into_glib() as u32);

    pipeline.set_state(gst::State::Null).ok();
    std::fs::remove_file(&path).ok();
}

// The registry resolves element names by last registration, so aliasing
// TypeFind2 as "typefind" takes over every lookup by name (gst-launch,
// decodebin, parsebin, urisourcebin, ...). The registry is process-global:
// this test is the only one relying on the alias, the other tests build
// "typefind2" explicitly and are unaffected by it.
#[test]
fn registry_takeover() {
    init();

    let reassert = || -> Result<(), glib::BoolError> {
        let plugin = gst::Registry::get().find_plugin("typefind2");
        gst::Element::register(
            plugin.as_ref(),
            "typefind",
            gst::Rank::MARGINAL,
            TypeFind2::static_type(),
        )
    };
    // Loading a plugin re-runs its plugin_init() and registers its features
    // again: coreelements lazily replaces the alias when the first core
    // element is created. Re-assert on every such registration.
    gst::Registry::get().connect_feature_added(move |_registry, feature| {
        if feature.name() == "typefind"
            && !feature
                .downcast_ref::<gst::ElementFactory>()
                .is_some_and(|f| f.element_type() == TypeFind2::static_type())
        {
            let _ = reassert();
        }
    });
    reassert().unwrap();

    assert_eq!(
        gst::ElementFactory::find("typefind")
            .expect("typefind factory")
            .element_type(),
        TypeFind2::static_type(),
        "the registry does not resolve \"typefind\" to TypeFind2"
    );

    // A real WAV with an uninformative extension, so detection must come from
    // the sniffers, not from guessing.
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("takeover.wav");
    std::fs::write(&path, wav_bytes()).unwrap();

    let pipeline = gst::Pipeline::new();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    let typefind = gst::ElementFactory::make("typefind").build().unwrap();
    assert_eq!(typefind.type_(), TypeFind2::static_type());
    let fakesink = make_fakesink();

    let found = capture_have_type(typefind.downcast_ref::<TypeFind2>().unwrap());

    pipeline.add_many([&filesrc, &typefind, &fakesink]).unwrap();
    filesrc.link(&typefind).unwrap();
    typefind.link(&fakesink).unwrap();
    assert_playing(&pipeline);

    assert_eq!(
        first_found(&found, "through the aliased factory").1,
        "audio/x-wav"
    );
    pipeline.set_state(gst::State::Null).ok();

    // urisourcebin (gst-plugins-base) builds its graphs by name lookup too:
    // the typefinder parsebin instantiates inside it must be ours.
    let pipeline = gst::Pipeline::new();
    let urisourcebin = gst::ElementFactory::make("urisourcebin")
        .property_from_str("uri", &glib::filename_to_uri(&path, None).unwrap())
        .property("parse-streams", true)
        .build()
        .unwrap();
    let sink = make_fakesink();
    pipeline.add_many([&urisourcebin, &sink]).unwrap();

    let added = Arc::new(AtomicU64::new(0));
    urisourcebin.connect_pad_added({
        let added = Arc::clone(&added);
        let sink = sink.clone();
        move |_urisourcebin, pad| {
            if pad.link(&sink.static_pad("sink").unwrap()).is_ok() {
                sink.sync_state_with_parent().ok();
                added.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    pipeline.set_state(gst::State::Playing).unwrap();

    assert!(
        wait_until(|| added.load(Ordering::Relaxed) > 0),
        "urisourcebin exposed no stream with the taken-over typefind"
    );
    assert!(
        pipeline
            .iterate_recurse()
            .into_iter()
            .filter_map(Result::ok)
            .any(|child| child.name() == *"typefind" && child.type_() == TypeFind2::static_type()),
        "parsebin's typefinder is not our element"
    );
    pipeline.set_state(gst::State::Null).ok();
    std::fs::remove_file(&path).ok();
}
