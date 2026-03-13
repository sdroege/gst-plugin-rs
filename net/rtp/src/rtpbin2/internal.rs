// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    task::Waker,
    time::Duration,
};

use gst::{glib, prelude::*};
use std::sync::{LazyLock, OnceLock};

use super::config::Rtp2Session;
use super::session::{RtpProfile, Session};
use super::source::ReceivedRb;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpinternalsession",
        gst::DebugColorFlags::empty(),
        Some("RTP Session (internal)"),
    )
});

/// Global Shared RTP state directory
///
/// Each entry corresponds to a named RTP State which can be shared by a sender and a receiver element.
/// A new entry is added, if not already present, upon a call to `SharedRtpState::get_or_init` and
/// a strong reference to the value of this entry is returned upon subsequent calls.
///
/// When no more elements hold a strong reference to the `SharedRtpState`, the `Drop` impl of
/// `SharedRtpStateInner` removes the entry from the global Shared RTP state directory.
/// This is why the `HashMap` value is a `Weak` pointer.
static SHARED_RTP_STATE: OnceLock<Mutex<HashMap<String, Weak<Mutex<SharedRtpStateInner>>>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub struct SharedRtpState(Arc<Mutex<SharedRtpStateInner>>);

#[derive(Debug)]
struct SharedRtpStateInner {
    name: String,
    sessions: HashMap<usize, SharedSession>,
}

impl SharedRtpState {
    pub fn get_or_init(name: String) -> Self {
        use std::collections::hash_map::Entry;

        match SHARED_RTP_STATE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .entry(name)
        {
            Entry::Occupied(entry) => SharedRtpState(entry.get().upgrade().unwrap()),
            Entry::Vacant(entry) => {
                let shared_state = Arc::new(Mutex::new(SharedRtpStateInner {
                    name: entry.key().to_owned(),
                    sessions: HashMap::new(),
                }));
                entry.insert(Arc::downgrade(&shared_state));

                SharedRtpState(shared_state)
            }
        }
    }

    // Currently this is only used in NULL -> Ready to make sure current name matches the rtp_id
    // Should we need to avoid returning an owned String, we could return a wrapper around
    // `MutexGuard` and impl `Deref::Target = &str`, or use `MutexGuard::map` once it's stable.
    pub fn name(&self) -> String {
        self.0.lock().unwrap().name.clone()
    }

    pub fn session_get_or_init<F>(&self, id: usize, f: F) -> SharedSession
    where
        F: FnOnce() -> SharedSession,
    {
        self.0
            .lock()
            .unwrap()
            .sessions
            .entry(id)
            .or_insert_with(f)
            .clone()
    }
}

impl Drop for SharedRtpStateInner {
    fn drop(&mut self) {
        SHARED_RTP_STATE
            .get()
            .expect("get_or_init when SharedRtpState was acquired")
            .lock()
            .expect("none of the code paths can panic with this mutex locked")
            .remove(&self.name);
    }
}

#[derive(Debug, Clone)]
pub struct SharedSession {
    pub(crate) id: usize,
    pub(crate) inner: Arc<Mutex<SharedSessionInner>>,
    pub(crate) config: Rtp2Session,
}

impl SharedSession {
    pub fn new(
        id: usize,
        profile: RtpProfile,
        min_rtcp_interval: Duration,
        reduced_size_rtcp: bool,
    ) -> Self {
        let mut inner = SharedSessionInner::new(id);
        inner.session.set_min_rtcp_interval(min_rtcp_interval);
        inner.session.set_profile(profile);
        inner.session.set_reduced_size_rtcp(reduced_size_rtcp);
        let inner = Arc::new(Mutex::new(inner));
        let weak_inner = Arc::downgrade(&inner);
        Self {
            id,
            inner,
            config: Rtp2Session::new(weak_inner),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SharedSessionInner {
    id: usize,

    pub(crate) session: Session,

    pub(crate) pt_map: HashMap<u8, gst::Caps>,

    pub(crate) rtcp_waker: Option<Waker>,
    pub(crate) rtp_send_sinkpad: Option<gst::Pad>,
}

impl SharedSessionInner {
    fn new(id: usize) -> Self {
        Self {
            id,

            session: Session::new(),

            pt_map: HashMap::default(),
            rtcp_waker: None,
            rtp_send_sinkpad: None,
        }
    }

    pub fn clear_pt_map(&mut self) {
        self.pt_map.clear();
    }

    pub fn add_caps(&mut self, caps: gst::Caps) {
        let Some((pt, clock_rate)) = pt_clock_rate_from_caps(&caps) else {
            return;
        };
        let caps_clone = caps.clone();
        self.pt_map
            .entry(pt)
            .and_modify(move |entry| *entry = caps)
            .or_insert_with(move || caps_clone);
        self.session.set_pt_clock_rate(pt, clock_rate);
    }

    pub(crate) fn caps_from_pt(&self, pt: u8) -> gst::Caps {
        self.pt_map.get(&pt).cloned().unwrap_or(
            gst::Caps::builder("application/x-rtp")
                .field("payload", pt as i32)
                .build(),
        )
    }

    pub fn pt_map(&self) -> impl Iterator<Item = (u8, &gst::Caps)> + '_ {
        self.pt_map.iter().map(|(&k, v)| (k, v))
    }

    pub fn stats(&self) -> gst::Structure {
        let mut session_stats = gst::Structure::builder("application/x-rtpbin2-session-stats")
            .field("id", self.id as u64);
        for ssrc in self.session.ssrcs() {
            if let Some(ls) = self.session.local_send_source_by_ssrc(ssrc) {
                let mut source_stats =
                    gst::Structure::builder("application/x-rtpbin2-source-stats")
                        .field("ssrc", ls.ssrc())
                        .field("sender", true)
                        .field("local", true)
                        .field("packets-sent", ls.packet_count())
                        .field("octets-sent", ls.octet_count())
                        .field("bitrate", ls.bitrate() as u64);
                if let Some(pt) = ls.payload_type()
                    && let Some(clock_rate) = self.session.clock_rate_from_pt(pt)
                {
                    source_stats = source_stats.field("clock-rate", clock_rate);
                }
                if let Some(sr) = ls.last_sent_sr() {
                    source_stats = source_stats
                        .field("sr-ntptime", sr.ntp_timestamp().as_u64())
                        .field("sr-rtptime", sr.rtp_timestamp())
                        .field("sr-octet-count", sr.octet_count())
                        .field("sr-packet-count", sr.packet_count());
                }
                let rbs = gst::List::new(ls.received_report_blocks().map(
                    |(sender_ssrc, ReceivedRb { rb, .. })| {
                        gst::Structure::builder("application/x-rtcp-report-block")
                            .field("sender-ssrc", sender_ssrc)
                            .field("rb-fraction-lost", rb.fraction_lost())
                            .field("rb-packets-lost", rb.cumulative_lost())
                            .field("rb-extended_sequence_number", rb.extended_sequence_number())
                            .field("rb-jitter", rb.jitter())
                            .field("rb-last-sr-ntp-time", rb.last_sr_ntp_time())
                            .field("rb-delay_since_last-sr-ntp-time", rb.delay_since_last_sr())
                            .build()
                    },
                ));
                match rbs.len() {
                    0 => (),
                    1 => {
                        source_stats =
                            source_stats.field("report-blocks", rbs.first().unwrap().clone());
                    }
                    _ => {
                        source_stats = source_stats.field("report-blocks", rbs);
                    }
                }

                // TODO: add jitter, packets-lost
                session_stats = session_stats.field(ls.ssrc().to_string(), source_stats.build());
            } else if let Some(lr) = self.session.local_receive_source_by_ssrc(ssrc) {
                let mut source_stats =
                    gst::Structure::builder("application/x-rtpbin2-source-stats")
                        .field("ssrc", lr.ssrc())
                        .field("sender", false)
                        .field("local", true);
                if let Some(pt) = lr.payload_type()
                    && let Some(clock_rate) = self.session.clock_rate_from_pt(pt)
                {
                    source_stats = source_stats.field("clock-rate", clock_rate);
                }
                // TODO: add rb stats
                session_stats = session_stats.field(lr.ssrc().to_string(), source_stats.build());
            } else if let Some(rs) = self.session.remote_send_source_by_ssrc(ssrc) {
                let mut source_stats =
                    gst::Structure::builder("application/x-rtpbin2-source-stats")
                        .field("ssrc", rs.ssrc())
                        .field("sender", true)
                        .field("local", false)
                        .field("octets-received", rs.octet_count())
                        .field("packets-received", rs.packet_count())
                        .field("bitrate", rs.bitrate() as u64)
                        .field("jitter", rs.jitter())
                        .field("packets-lost", rs.packets_lost());
                if let Some(pt) = rs.payload_type()
                    && let Some(clock_rate) = self.session.clock_rate_from_pt(pt)
                {
                    source_stats = source_stats.field("clock-rate", clock_rate);
                }
                if let Some(rtp_from) = rs.rtp_from() {
                    source_stats = source_stats.field("rtp-from", rtp_from.to_string());
                }
                if let Some(rtcp_from) = rs.rtcp_from() {
                    source_stats = source_stats.field("rtcp-from", rtcp_from.to_string());
                }
                if let Some(sr) = rs.last_received_sr() {
                    source_stats = source_stats
                        .field("sr-ntptime", sr.ntp_timestamp().as_u64())
                        .field("sr-rtptime", sr.rtp_timestamp())
                        .field("sr-octet-count", sr.octet_count())
                        .field("sr-packet-count", sr.packet_count());
                }
                if let Some(rb) = rs.last_sent_rb() {
                    source_stats = source_stats
                        .field("sent-rb-fraction-lost", rb.fraction_lost())
                        .field("sent-rb-packets-lost", rb.cumulative_lost())
                        .field(
                            "sent-rb-extended-sequence-number",
                            rb.extended_sequence_number(),
                        )
                        .field("sent-rb-jitter", rb.jitter())
                        .field("sent-rb-last-sr-ntp-time", rb.last_sr_ntp_time())
                        .field(
                            "sent-rb-delay-since-last-sr-ntp-time",
                            rb.delay_since_last_sr(),
                        );
                }
                let rbs = gst::List::new(rs.received_report_blocks().map(
                    |(sender_ssrc, ReceivedRb { rb, .. })| {
                        gst::Structure::builder("application/x-rtcp-report-block")
                            .field("sender-ssrc", sender_ssrc)
                            .field("rb-fraction-lost", rb.fraction_lost())
                            .field("rb-packets-lost", rb.cumulative_lost())
                            .field("rb-extended_sequence_number", rb.extended_sequence_number())
                            .field("rb-jitter", rb.jitter())
                            .field("rb-last-sr-ntp-time", rb.last_sr_ntp_time())
                            .field("rb-delay_since_last-sr-ntp-time", rb.delay_since_last_sr())
                            .build()
                    },
                ));
                match rbs.len() {
                    0 => (),
                    1 => {
                        source_stats =
                            source_stats.field("report-blocks", rbs.first().unwrap().clone());
                    }
                    _ => {
                        source_stats = source_stats.field("report-blocks", rbs);
                    }
                }
                session_stats = session_stats.field(rs.ssrc().to_string(), source_stats.build());
            } else if let Some(rr) = self.session.remote_receive_source_by_ssrc(ssrc) {
                let source_stats = gst::Structure::builder("application/x-rtpbin2-source-stats")
                    .field("ssrc", rr.ssrc())
                    .field("sender", false)
                    .field("local", false)
                    .build();
                session_stats = session_stats.field(rr.ssrc().to_string(), source_stats);
            }
        }

        session_stats.build()
    }
}

pub fn pt_clock_rate_from_caps(caps: &gst::CapsRef) -> Option<(u8, u32)> {
    let Some(s) = caps.structure(0) else {
        gst::debug!(CAT, "no structure!");
        return None;
    };
    let Some((clock_rate, pt)) = Option::zip(
        s.get::<i32>("clock-rate").ok(),
        s.get::<i32>("payload").ok(),
    ) else {
        gst::debug!(
            CAT,
            "could not retrieve clock-rate and/or payload from structure"
        );
        return None;
    };
    if (0..=127).contains(&pt) && clock_rate > 0 {
        Some((pt as u8, clock_rate as u32))
    } else {
        gst::debug!(
            CAT,
            "payload value {pt} out of bounds or clock-rate {clock_rate} out of bounds"
        );
        None
    }
}

static RUST_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rust-log",
        gst::DebugColorFlags::empty(),
        Some("Logs from rust crates"),
    )
});

static GST_RUST_LOGGER_ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
static GST_RUST_LOGGER: GstRustLogger = GstRustLogger {};

pub(crate) struct GstRustLogger {}

impl GstRustLogger {
    pub fn install() {
        GST_RUST_LOGGER_ONCE.get_or_init(|| {
            if log::set_logger(&GST_RUST_LOGGER).is_err() {
                gst::warning!(
                    RUST_CAT,
                    "Cannot install log->gst logger, already installed?"
                );
            } else {
                log::set_max_level(GstRustLogger::debug_level_to_log_level_filter(
                    RUST_CAT.threshold(),
                ));
                gst::info!(RUST_CAT, "installed log->gst logger");
            }
        });
    }

    fn debug_level_to_log_level_filter(level: gst::DebugLevel) -> log::LevelFilter {
        match level {
            gst::DebugLevel::None => log::LevelFilter::Off,
            gst::DebugLevel::Error => log::LevelFilter::Error,
            gst::DebugLevel::Warning => log::LevelFilter::Warn,
            gst::DebugLevel::Fixme | gst::DebugLevel::Info => log::LevelFilter::Info,
            gst::DebugLevel::Debug => log::LevelFilter::Debug,
            gst::DebugLevel::Log | gst::DebugLevel::Trace | gst::DebugLevel::Memdump => {
                log::LevelFilter::Trace
            }
            _ => log::LevelFilter::Trace,
        }
    }

    fn log_level_to_debug_level(level: log::Level) -> gst::DebugLevel {
        match level {
            log::Level::Error => gst::DebugLevel::Error,
            log::Level::Warn => gst::DebugLevel::Warning,
            log::Level::Info => gst::DebugLevel::Info,
            log::Level::Debug => gst::DebugLevel::Debug,
            log::Level::Trace => gst::DebugLevel::Trace,
        }
    }
}

impl log::Log for GstRustLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        RUST_CAT.above_threshold(GstRustLogger::log_level_to_debug_level(metadata.level()))
    }

    fn log(&self, record: &log::Record) {
        let gst_level = GstRustLogger::log_level_to_debug_level(record.metadata().level());
        let file = record
            .file()
            .map(glib::GString::from)
            .unwrap_or_else(|| glib::GString::from("rust-log"));
        let function = record.target();
        let line = record.line().unwrap_or(0);
        RUST_CAT.log(
            None::<&glib::Object>,
            gst_level,
            file.as_gstr(),
            function,
            line,
            *record.args(),
        );
    }

    fn flush(&self) {}
}
