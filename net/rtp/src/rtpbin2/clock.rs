use gst::prelude::*;
use itertools::Itertools as _;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use super::time::*;
use sdp_types::{MediaClock, MediaClockSource, ReferenceClock};

const DEFAULT_NTP_PORT: u16 = 123;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpbin2clock",
        gst::DebugColorFlags::empty(),
        Some("RTP clock"),
    )
});

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClockError {
    #[error("Unsupported reference clock {}", .0)]
    UnsupportedReferenceClock(ReferenceClock),

    #[error("Unsupported media clock source {}", .0)]
    UnsupportedMediaClockSource(MediaClockSource),

    #[error("Invalid clock signalling: {}", .0)]
    InvalidClockSignalling(#[from] sdp_types::AttributeError),

    #[error("Non real-time system clock, set the 'clock-type' property to RealTime")]
    NonRealTimeSystemClock,

    #[error("clock with type {} doesn't match refclk: {}", .clock_type, .refclk)]
    ClockMismatch {
        clock_type: glib::Type,
        refclk: ReferenceClock,
    },
}

impl ClockError {
    fn new_clock_mismatch(clock: &gst::Clock, refclk: ReferenceClock) -> ClockError {
        ClockError::ClockMismatch {
            clock_type: clock.type_(),
            refclk,
        }
    }
}

#[derive(Debug)]
pub struct SignalledClockInner {
    ref_ts_meta: gst::Caps,
    pub(crate) gst_clock: gst::Clock,
    /// Offset to add to the system clock time to convert to the target reference clock time
    system_time_offset: gst::ClockTime,
    /// Offset to subtract to an NTP time to convert to the target reference clock time
    ntp_offset: gst::ClockTime,
}

impl SignalledClockInner {
    pub fn is_same_as(&self, other: &gst::Clock) -> bool {
        let clock_type = self.gst_clock.type_();
        if clock_type != other.type_() {
            return false;
        }

        if gst_net::NtpClock::static_type() == clock_type {
            self.gst_clock.property::<glib::GString>("address")
                == other.property::<glib::GString>("address")
                && self.gst_clock.property::<i32>("port") == other.property::<i32>("port")
        } else if gst_net::PtpClock::static_type() == clock_type {
            self.gst_clock.property::<glib::GString>("domain")
                == other.property::<glib::GString>("domain")
        } else if gst::SystemClock::static_type() == clock_type {
            self.gst_clock.property::<gst::ClockType>("clock-type")
                == other.property::<gst::ClockType>("clock-type")
        } else {
            gst::fixme!(CAT, "unsupported clock type: {clock_type:?}");
            false
        }
    }

    fn try_new(refclk_str: &glib::GStr, gst_clock: gst::Clock) -> Result<Self, ClockError> {
        use sdp_types::{Ntp, NtpServerAddr, Ptp, PtpDomain, PtpServer};

        let refclk = match refclk_str.parse::<ReferenceClock>() {
            Ok(refclk) => refclk,
            Err(err) => {
                return Err(err.into());
            }
        };

        match &refclk {
            ReferenceClock::Local => {
                if gst_clock.type_() != gst::SystemClock::static_type() {
                    return Err(ClockError::new_clock_mismatch(&gst_clock, refclk));
                }
                if gst_clock.property::<gst::ClockType>("clock-type") != gst::ClockType::Realtime {
                    return Err(ClockError::NonRealTimeSystemClock);
                }

                // sender declared using its local clock, there's nothing else we can assume
                Ok(SignalledClockInner {
                    ref_ts_meta: gst::Caps::new_empty_simple("timestamp/x-sender"),
                    gst_clock,
                    system_time_offset: gst::ClockTime::ZERO,
                    ntp_offset: gst::clock_time::UNIX_TO_NTP_TIME_OFFSET,
                })
            }
            ReferenceClock::Ntp(Ntp { server }) => {
                let system_time_offset = if gst_clock.type_() == gst::SystemClock::static_type() {
                    if gst_clock.property::<gst::ClockType>("clock-type")
                        != gst::ClockType::Realtime
                    {
                        return Err(ClockError::NonRealTimeSystemClock);
                    }

                    // user decided to use system clock to represent an NTP clock
                    // => apply the UNIX time to NTP time offset
                    gst::clock_time::UNIX_TO_NTP_TIME_OFFSET
                } else if gst_clock.type_() != gst_net::NtpClock::static_type() {
                    return Err(ClockError::new_clock_mismatch(&gst_clock, refclk));
                } else {
                    gst::ClockTime::ZERO
                };

                let mut ref_ts_meta_builder = gst::Caps::builder("timestamp/x-ntp");

                match server {
                    NtpServerAddr::HostPort { hostname, port } => {
                        if gst_clock.type_() == gst_net::NtpClock::static_type()
                            && (gst_clock.property::<glib::GString>("address").as_str() != hostname
                                || gst_clock.property::<i32>("port") as u16
                                    != port.unwrap_or(DEFAULT_NTP_PORT))
                        {
                            return Err(ClockError::new_clock_mismatch(&gst_clock, refclk));
                        }

                        ref_ts_meta_builder = ref_ts_meta_builder
                            .field("host", hostname)
                            .field_if_some("port", port.map(|p| p as i32));
                    }
                    NtpServerAddr::Traceable => (),
                }

                let ntp_offset = gst::ClockTime::ZERO;

                Ok(SignalledClockInner {
                    ref_ts_meta: ref_ts_meta_builder.build(),
                    gst_clock,
                    system_time_offset,
                    ntp_offset,
                })
            }
            ReferenceClock::Ptp(Ptp { version, server }) => {
                let system_time_offset = if gst_clock.type_() == gst::SystemClock::static_type() {
                    if gst_clock.property::<gst::ClockType>("clock-type")
                        != gst::ClockType::Realtime
                    {
                        return Err(ClockError::NonRealTimeSystemClock);
                    }

                    // user decided to use system clock to represent a PTP clock
                    // => adjust with leap seconds
                    *gst::clock_time::UNIX_TO_PTP_TIME_OFFSET
                } else if gst_clock.type_() != gst_net::PtpClock::static_type() {
                    return Err(ClockError::new_clock_mismatch(&gst_clock, refclk));
                } else {
                    gst::ClockTime::ZERO
                };

                let mut ref_ts_meta_builder =
                    gst::Caps::builder("timestamp/x-ptp").field("version", version.to_string());

                match server {
                    PtpServer::GmidDomain { gmid, domain } => {
                        ref_ts_meta_builder = ref_ts_meta_builder.field("gmid", gmid.as_u64());

                        ref_ts_meta_builder = match domain {
                            Some(PtpDomain::DomainName { name }) => {
                                if gst_clock.type_() == gst_net::PtpClock::static_type()
                                    && gst_clock.property::<glib::GString>("domain-name").as_str()
                                        != name
                                {
                                    return Err(ClockError::new_clock_mismatch(&gst_clock, refclk));
                                }

                                ref_ts_meta_builder
                                    .field("domain-name", name)
                                    .field("domain", gst_clock.property::<u32>("domain"))
                            }
                            Some(PtpDomain::DomainNumber(number)) => {
                                if gst_clock.type_() == gst_net::PtpClock::static_type()
                                    && gst_clock.property::<u32>("domain") != (*number as u32)
                                {
                                    return Err(ClockError::new_clock_mismatch(&gst_clock, refclk));
                                }

                                ref_ts_meta_builder.field("domain", *number as u32)
                            }
                            None => ref_ts_meta_builder
                                .field("domain", gst_clock.property::<u8>("domain")),
                        };
                    }
                    PtpServer::Traceable => (),
                }

                Ok(SignalledClockInner {
                    ref_ts_meta: ref_ts_meta_builder.build(),
                    gst_clock,
                    system_time_offset,
                    ntp_offset: *gst::clock_time::NTP_TO_PTP_TIME_OFFSET,
                })
            }
            _ => Err(ClockError::UnsupportedReferenceClock(refclk)),
        }
    }

    pub fn to_reference_time(&self, ntp_time: NtpTime) -> gst::ClockTime {
        gst::ClockTime::from_nseconds(ntp_time.as_nanos()).saturating_sub(self.ntp_offset)
    }
}

#[derive(Clone, Debug)]
pub struct SignalledClock(Arc<SignalledClockInner>);

impl SignalledClock {
    fn try_new(refclk_str: &glib::GStr, gst_clock: gst::Clock) -> Result<Self, ClockError> {
        Ok(SignalledClock(Arc::new(SignalledClockInner::try_new(
            refclk_str, gst_clock,
        )?)))
    }
}

impl std::ops::Deref for SignalledClock {
    type Target = SignalledClockInner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn get_media_clock_offset(media_clock_source: &MediaClockSource) -> Result<u32, ClockError> {
    // TODO handle other variants
    match &media_clock_source.clock {
        MediaClock::Direct(sdp_types::Direct { offset, rate }) => {
            if rate.is_some_and(|r| !r.is_one()) {
                gst::fixme!(CAT, "unsupported non-1/1 rate: {media_clock_source}");
                return Err(ClockError::UnsupportedMediaClockSource(
                    media_clock_source.clone(),
                ));
            }
            Ok(offset.unwrap_or(0))
        }
        MediaClock::Sender => Ok(0),
        other => {
            gst::fixme!(CAT, "unsupported {other}");
            Err(ClockError::UnsupportedMediaClockSource(
                media_clock_source.clone(),
            ))
        }
    }
}

#[derive(Debug)]
struct MediaLevelClock {
    clock: SignalledClock,
    mediaclk: Option<MediaClockSource>,
    offset: u32,
}

impl MediaLevelClock {
    fn new(clock: &SignalledClock) -> Self {
        MediaLevelClock {
            clock: clock.clone(),
            mediaclk: None,
            offset: 0,
        }
    }
}

impl MediaLevelClock {
    fn add_mediaclk(&mut self, mediaclk: &str) -> Result<(), ClockError> {
        let mediaclk = mediaclk.parse::<MediaClockSource>()?;
        self.offset = get_media_clock_offset(&mediaclk)?;
        self.mediaclk = Some(mediaclk);

        Ok(())
    }
}

#[derive(Debug)]
pub struct SourceLevelClock {
    ssrc: u32,
    pub clock: SignalledClock,
    pub mediaclk: Option<MediaClockSource>,
    pub offset: u32,
}

impl SourceLevelClock {
    fn new(ssrc: u32, clock: &SignalledClock) -> Self {
        SourceLevelClock {
            ssrc,
            clock: clock.clone(),
            mediaclk: None,
            offset: 0,
        }
    }

    fn from(ssrc: u32, pt_clock: &MediaLevelClock) -> Self {
        SourceLevelClock {
            ssrc,
            clock: pt_clock.clock.clone(),
            mediaclk: pt_clock.mediaclk.clone(),
            offset: pt_clock.offset,
        }
    }

    pub fn is_same_as(&self, other: &gst::Clock) -> bool {
        self.clock.is_same_as(other)
    }

    fn add_mediaclk(&mut self, mediaclk: &str) -> Result<(), ClockError> {
        let mediaclk = mediaclk.parse::<MediaClockSource>()?;
        self.offset = get_media_clock_offset(&mediaclk)?;
        self.mediaclk = Some(mediaclk);

        Ok(())
    }

    pub fn ts_meta_ref(&self) -> &gst::Caps {
        &self.clock.ref_ts_meta
    }

    pub fn get_reference_time(&self, rtptime: u32, clock_rate: u32) -> gst::ClockTime {
        let now = self.clock.gst_clock.internal_time();
        self.get_reference_time_priv(now, rtptime, clock_rate)
    }

    fn get_reference_time_priv(
        &self,
        now: gst::ClockTime,
        rtptime: u32,
        clock_rate: u32,
    ) -> gst::ClockTime {
        // This is a port of the relevant parts of rtp_jitter_buffer_calculate_pts
        // except that it returns the reference time relative to its actual epoch, not UTC (NTP time)

        let rtptime = rtptime as u64;

        // Get current time relative to the target reference clock epoch
        // (offset applies if it's a system clock targeting another reference clock)
        let now_ref_clk = now.wrapping_add(self.clock.system_time_offset);

        // Current RTP time based on the estimated reference clock and the corresponding
        // RTP time period start
        let mut rtptime_period_start = now_ref_clk
            .nseconds()
            .mul_div_floor(clock_rate as _, *gst::ClockTime::SECOND)
            .unwrap();

        // offset here is the RTP timestamp reference clock epoch
        let mut rtptime_ext = (rtptime_period_start + self.offset as u64) & 0xffff_ffff;

        // If we're in the first period then the start of the period might be
        // before the clock epoch
        let mut negative_rtptime_period_start = if rtptime_period_start >= rtptime_ext {
            rtptime_period_start -= rtptime_ext;
            false
        } else {
            rtptime_period_start = rtptime_ext - rtptime_period_start;
            true
        };

        let ssrc = self.ssrc;
        let get_sign = |is_negative| -> char { if is_negative { '-' } else { '+' } };
        gst::trace!(
            CAT,
            "{ssrc:#08x} ({ssrc}): packet rtptime {rtptime}, clock offset {}",
            self.offset,
        );
        gst::trace!(
            CAT,
            "{ssrc:#08x} ({ssrc}): cur reference time {now_ref_clk}"
        );
        gst::trace!(
            CAT,
            "{ssrc:#08x} ({ssrc}): cur RTP time period start {sign}{} (RTP {sign}{rtptime_period_start})",
            gst::ClockTime::from_nseconds(
                rtptime_period_start
                    .mul_div_floor(*gst::ClockTime::SECOND, clock_rate as _)
                    .unwrap()
            ),
            sign = get_sign(negative_rtptime_period_start),
        );
        gst::trace!(
            CAT,
            "{ssrc:#08x} ({ssrc}): cur RTP time related to period start {} (RTP {rtptime_ext})",
            gst::ClockTime::from_nseconds(
                rtptime_ext
                    .mul_div_floor(*gst::ClockTime::SECOND, clock_rate as _)
                    .unwrap()
            ),
        );

        // Check for wraparounds, we assume that the diff between current RTP
        // timestamp and current reference clock time can't be bigger than 2**31 clock
        // rate units. If it is bigger then get closer to it by moving one RTP
        // timestamp period into the future or into the past.
        //
        // E.g.
        //    current EXT: 0x_______5 fffffffe
        //    packet  RTP: 0x         00000001
        // => packet  EXT: 0x_______6 00000001
        //
        //    current EXT: 0x_______5 00000001
        //    packet  RTP: 0x         fffffffe
        // => packet  EXT: 0x_______4 fffffffe
        //

        if rtptime_ext > rtptime && rtptime_ext - rtptime >= 0x8000_0000 {
            if negative_rtptime_period_start {
                negative_rtptime_period_start = false;
                // we're in the first period
                assert!(rtptime_period_start <= 0x1_0000_0000);
                rtptime_period_start = 0x1_0000_0000 - rtptime_period_start;
            } else {
                rtptime_period_start += 0x1_0000_0000;
            }
        } else if rtptime > rtptime_ext && rtptime - rtptime_ext >= 0x8000_0000 {
            if negative_rtptime_period_start {
                rtptime_period_start += 0x1_0000_0000;
            } else if rtptime_period_start < 0x1_0000_0000 {
                negative_rtptime_period_start = true;
                rtptime_period_start = 0x1_0000_0000 - rtptime_period_start;
            } else {
                rtptime_period_start -= 0x1_0000_0000;
            }
        }

        gst::trace!(
            CAT,
            "{ssrc:#08x} ({ssrc}): wraparound adjusted RTP time period start {sign}{} (RTP {sign}{rtptime_period_start})",
            gst::ClockTime::from_nseconds(
                rtptime_period_start
                    .mul_div_floor(*gst::ClockTime::SECOND, clock_rate as _)
                    .unwrap()
            ),
            sign = get_sign(negative_rtptime_period_start),
        );

        // Packet timestamp according to the reference clock in RTP time units.
        // Note that this does not include any inaccuracy caused by the estimation
        // of the reference clock unless it is more than 2**31 RTP time units off.
        // This is only relevant if the system clock is used as a proxy for a NTP
        // or PTP reference clock. As long as it's not that much off, we can use
        // literally any clock here for getting the correct result.
        if negative_rtptime_period_start {
            rtptime_ext = rtptime.saturating_sub(rtptime_period_start);
        } else {
            rtptime_ext = rtptime_period_start.wrapping_add(rtptime);
        }

        // Packet timestamp in nanoseconds according to the reference clock
        let ref_time = gst::ClockTime::from_nseconds(
            rtptime_ext
                .mul_div_floor(*gst::ClockTime::SECOND, clock_rate as _)
                .unwrap(),
        );

        gst::debug!(
            CAT,
            "{ssrc:#08x} ({ssrc}): RFC7273 packet reference time {ref_time} (RTP ext {rtptime_ext})",
        );

        ref_time
    }

    pub fn to_reference_time(&self, ntp_time: NtpTime) -> gst::ClockTime {
        self.clock.to_reference_time(ntp_time)
    }
}

#[derive(Debug, Default)]
pub struct SignalledClocks {
    clock_map: HashMap<glib::GString, SignalledClock>,
    pt_clock_map: HashMap<u8, MediaLevelClock>,
    ssrc_clock_map: HashMap<u32, SourceLevelClock>,
}

impl SignalledClocks {
    pub fn clear(&mut self) {
        self.clock_map.clear();
        self.pt_clock_map.clear();
        self.ssrc_clock_map.clear();
    }

    pub fn add_or_update_from_caps(&mut self, pt: u8, caps: &gst::Caps) {
        if let Some(s) = caps.structure(0) {
            for field in s.iter() {
                match field.0.as_str() {
                    "a-ts-refclk" => {
                        let Ok(refclk) = field.1.get::<&glib::GStr>() else {
                            gst::warning!(CAT, "pt {pt}: invalid refclk: {:?} ({caps})", field.1);
                            continue;
                        };
                        if let Some(clock) = self.clock_from_refclk(refclk) {
                            gst::debug!(CAT, "pt {pt}: setting media-level clock: {refclk}");
                            self.pt_clock_map.insert(pt, MediaLevelClock::new(clock));
                        } else {
                            gst::debug!(
                                CAT,
                                "pt {pt}: no clock provided at this stage for refclk: {refclk} ({caps})"
                            );
                        }
                    }
                    "a-mediaclk" => {
                        let Ok(mediaclk) = field.1.get::<&glib::GStr>() else {
                            gst::warning!(CAT, "pt {pt}: invalid mediaclk: {:?} ({caps})", field.1);
                            continue;
                        };
                        if let Some(clock) = self.pt_clock_map.get_mut(&pt) {
                            if let Err(err) = clock.add_mediaclk(mediaclk) {
                                gst::warning!(
                                    CAT,
                                    "pt {pt}: failed to parse mediaclk: {mediaclk}: {err} ({caps})"
                                );
                                continue;
                            }
                            gst::debug!(CAT, "pt {pt}: adding media-level: {mediaclk}");
                        } else {
                            gst::debug!(
                                CAT,
                                "pt {pt}: no known clock at this stage for payload type {pt}, processing mediaclk: {mediaclk} ({caps})"
                            );
                        }
                    }
                    other if other.starts_with("a-ssrc-") => {
                        let Some((_a_prefix, _ssrc_prefix, ssrc, value)) =
                            other.splitn(4, '-').collect_tuple()
                        else {
                            continue;
                        };
                        let Ok(ssrc) = ssrc.parse::<u32>() else {
                            continue;
                        };
                        match value {
                            "ts-refclk" => {
                                let Ok(refclk) = field.1.get::<&glib::GStr>() else {
                                    gst::warning!(
                                        CAT,
                                        "{ssrc:#08x} ({ssrc}) invalid refclk: {:?} ({caps})",
                                        field.1
                                    );
                                    continue;
                                };
                                if let Some(clock) = self.clock_from_refclk(refclk) {
                                    gst::debug!(
                                        CAT,
                                        "{ssrc:#08x} ({ssrc}): setting source-level clock: {refclk}"
                                    );
                                    self.ssrc_clock_map
                                        .insert(ssrc, SourceLevelClock::new(ssrc, clock));
                                } else {
                                    gst::debug!(
                                        CAT,
                                        "{ssrc:#08x} ({ssrc}): no clock provided at this stage for refclk: {refclk} ({caps})"
                                    );
                                }
                            }
                            "mediaclk" => {
                                let Ok(mediaclk) = field.1.get::<&glib::GStr>() else {
                                    gst::warning!(
                                        CAT,
                                        "{ssrc:#08x} ({ssrc}): invalid mediaclk: {:?} ({caps})",
                                        field.1
                                    );
                                    continue;
                                };
                                if let Some(clock) = self.ssrc_clock_map.get_mut(&ssrc) {
                                    if let Err(err) = clock.add_mediaclk(mediaclk) {
                                        gst::warning!(
                                            CAT,
                                            "{ssrc:#08x} ({ssrc}): failed to parse mediaclk: {mediaclk}: {err} ({caps})"
                                        );
                                        continue;
                                    }
                                    gst::debug!(
                                        CAT,
                                        "{ssrc:#08x} ({ssrc}): adding session-level: {mediaclk}"
                                    );
                                } else {
                                    gst::debug!(
                                        CAT,
                                        "{ssrc:#08x} ({ssrc}): no known clock at this stage for SSRC {ssrc}, processing mediaclk: {mediaclk} ({caps})"
                                    );
                                }
                            }
                            _ => (),
                        }
                    }
                    _ => (),
                }
            }
        }
    }

    pub fn add(&mut self, refclk_str: &glib::GStr, clock: gst::Clock) -> Result<(), ClockError> {
        if self.clock_map.contains_key(refclk_str) {
            // this clock is already known => no need to add it again
            // note: Rtp2Session::set_clock_map clears the clocks
            //       when a new clock-map is set.
            return Ok(());
        }

        let signalled_clock = SignalledClock::try_new(refclk_str, clock)?;
        gst::debug!(CAT, "inserting: {refclk_str}, {signalled_clock:?}");
        self.clock_map
            .insert(refclk_str.to_owned(), signalled_clock);

        Ok(())
    }

    pub fn clock_from_refclk(&self, refclk: &glib::GStr) -> Option<&SignalledClock> {
        self.clock_map.get(refclk)
    }

    /// Init the reference clock for this SSRC if it has been signalled
    ///
    /// Priority: source-level, media-level and finally session-level
    /// (the latter being taken care of by the application when building the pt caps)
    pub fn maybe_init_reference_clock(&mut self, ssrc: u32, pt: u8) {
        if !self.ssrc_clock_map.contains_key(&ssrc) {
            // no source-level clock yet => attempt to init it from the media-level clock
            let Some(pt_clock) = self.pt_clock_map.get(&pt) else {
                gst::debug!(CAT, "{ssrc:#08x} ({ssrc}), pt {pt}: no clocks signalled");

                return;
            };

            gst::info!(
                CAT,
                "{ssrc:#08x} ({ssrc}): will use media-level clock {pt_clock:?}"
            );
            self.ssrc_clock_map
                .insert(ssrc, SourceLevelClock::from(ssrc, pt_clock));
        }
    }

    pub fn get_reference_clock(&self, ssrc: u32) -> Option<&SourceLevelClock> {
        self.ssrc_clock_map.get(&ssrc)
    }

    pub fn clock_map(&self) -> impl Iterator<Item = (&glib::GStr, &SignalledClock)> + '_ {
        self.clock_map.iter().map(|(k, v)| (k.as_gstr(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    const PT: u8 = 96;
    const CLOCK_RATE: u32 = 48_000;
    const SSRC: u32 = 1234;
    const LEAP_YEARS_1900_TO_1970: u64 = 17;
    const LEAP_YEARS_1970_TO_2026: u64 = 14;
    const FIRST_JANUARY_2026_NTP_TIME: gst::ClockTime = gst::ClockTime::from_seconds(
        ((2026 - 1900) * 365 + LEAP_YEARS_1900_TO_1970 + LEAP_YEARS_1970_TO_2026) * 24 * 60 * 60,
    );
    const FIRST_JANUARY_2026_UNIX_TIME_SECONDS: u64 =
        ((2026 - 1970) * 365 + LEAP_YEARS_1970_TO_2026) * 24 * 60 * 60;
    const FIRST_JANUARY_2026_UNIX_TIME: gst::ClockTime =
        gst::ClockTime::from_seconds(FIRST_JANUARY_2026_UNIX_TIME_SECONDS);
    static FIRST_JANUARY_2026_PTP_TIME: LazyLock<gst::ClockTime> =
        LazyLock::new(|| FIRST_JANUARY_2026_UNIX_TIME + *gst::clock_time::UNIX_TO_PTP_TIME_OFFSET);

    const PTP_VERSION: &str = "IEEE1588-2008";
    const PTP_GMID: u64 = 42;
    const PTP_GMID_STR: &str = "00-00-00-00-00-00-00-2A";
    const PTP_DOMAIN_NUMBER: u8 = 1;

    #[test]
    fn get_reference_time_system_clock() {
        init();

        let mut signalled_clocks = SignalledClocks::default();

        let system_clock = glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build()
            .upcast::<gst::Clock>();
        signalled_clocks
            .add(
                glib::GString::from(sdp_types::ReferenceClock::Local.to_string()).as_gstr(),
                system_clock,
            )
            .unwrap();

        let media_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("payload", PT as i32)
            .field("clock-rate", CLOCK_RATE as i32)
            .field("encoding-name", "L24")
            .field("a-ts-refclk", "local")
            .build();
        signalled_clocks.add_or_update_from_caps(PT, &media_caps);
        signalled_clocks.maybe_init_reference_clock(SSRC, PT);

        let source_clock = signalled_clocks.get_reference_clock(SSRC).unwrap();
        assert_eq!(
            source_clock.ts_meta_ref(),
            &gst::Caps::new_empty_simple("timestamp/x-sender"),
        );

        let packet_sys_time = 5.seconds() + FIRST_JANUARY_2026_UNIX_TIME;
        let rtptime = (packet_sys_time
            .nseconds()
            .mul_div_round(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        let sys_clock_now = 6.seconds() + FIRST_JANUARY_2026_UNIX_TIME;
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // packet clock in NTP time: add UNIX time epoch
        let packet_ntp_time_ns =
            (packet_sys_time + gst::clock_time::UNIX_TO_NTP_TIME_OFFSET).nseconds();
        let ntp_time = NtpTime::from_duration(Duration::from_nanos(packet_ntp_time_ns));
        // reference time is expressed in PTP time
        assert_eq!(source_clock.to_reference_time(ntp_time), packet_sys_time);
    }

    #[ignore = "pool.ntp.org address can't be resolved on CI"]
    #[test]
    fn get_reference_time_ntp_clock() {
        const NTP_HOSTNAME: &str = "pool.ntp.org";

        init();

        let mut signalled_clocks = SignalledClocks::default();

        // build the object but do not attempt to sync for this test
        let ntp_clock = glib::Object::builder::<gst_net::NtpClock>()
            .property("address", NTP_HOSTNAME)
            .property("port", DEFAULT_NTP_PORT as i32)
            .build()
            .upcast::<gst::Clock>();
        // Add the NTP clock with hostname
        // Note: port not declared as part of the ReferenceClock
        signalled_clocks
            .add(
                glib::GString::from(
                    sdp_types::ReferenceClock::from_ntp_hostname(NTP_HOSTNAME).to_string(),
                )
                .as_gstr(),
                ntp_clock,
            )
            .unwrap();

        let media_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("payload", PT as i32)
            .field("clock-rate", CLOCK_RATE as i32)
            .field("encoding-name", "L24")
            .field("a-ts-refclk", format!("ntp={NTP_HOSTNAME}"))
            .build();
        signalled_clocks.add_or_update_from_caps(PT, &media_caps);
        signalled_clocks.maybe_init_reference_clock(SSRC, PT);

        let source_clock = signalled_clocks.get_reference_clock(SSRC).unwrap();
        assert_eq!(
            source_clock.ts_meta_ref(),
            &gst::Caps::builder("timestamp/x-ntp")
                .field("host", NTP_HOSTNAME)
                .build(),
        );

        let packet_ntp_time = 5.seconds() + FIRST_JANUARY_2026_NTP_TIME;
        let rtptime = (packet_ntp_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        let ntp_clock_now = 6.seconds() + FIRST_JANUARY_2026_NTP_TIME;
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(ntp_clock_now, rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_ntp_time);

        let ntp_time = NtpTime::from_duration(Duration::from_nanos(packet_ntp_time.nseconds()));
        // reference time is also expressed as NTP time
        assert_eq!(source_clock.to_reference_time(ntp_time), packet_ntp_time);
    }

    #[test]
    fn get_reference_time_system_clock_as_ntp() {
        assert_eq!(
            FIRST_JANUARY_2026_NTP_TIME,
            FIRST_JANUARY_2026_UNIX_TIME + gst::clock_time::UNIX_TO_NTP_TIME_OFFSET
        );
        init();

        let mut signalled_clocks = SignalledClocks::default();

        let system_clock = glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build()
            .upcast::<gst::Clock>();
        // Add the system clock and declare it as synced to an NTP clock
        signalled_clocks
            .add(
                glib::GString::from(sdp_types::ReferenceClock::new_ntp_traceable().to_string())
                    .as_gstr(),
                system_clock,
            )
            .unwrap();

        let media_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("payload", PT as i32)
            .field("clock-rate", CLOCK_RATE as i32)
            .field("encoding-name", "L24")
            .field("a-ts-refclk", "ntp=/traceable/")
            .build();
        signalled_clocks.add_or_update_from_caps(PT, &media_caps);
        signalled_clocks.maybe_init_reference_clock(SSRC, PT);

        let source_clock = signalled_clocks.get_reference_clock(SSRC).unwrap();
        assert_eq!(
            source_clock.ts_meta_ref(),
            &gst::Caps::new_empty_simple("timestamp/x-ntp"),
        );

        let packet_ntp_time = 5.seconds() + FIRST_JANUARY_2026_NTP_TIME;
        let rtptime = (packet_ntp_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        let system_clock_now = 6.seconds() + FIRST_JANUARY_2026_UNIX_TIME;
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(system_clock_now, rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time.nseconds(), packet_ntp_time.nseconds());

        let ntp_time = NtpTime::from_duration(Duration::from_nanos(packet_ntp_time.nseconds()));
        // reference time is also expressed as NTP time (as requested by the ReferenceClock)
        assert_eq!(source_clock.to_reference_time(ntp_time), packet_ntp_time);
    }

    #[test]
    fn get_reference_time_ptp_clock() {
        init();

        let mut signalled_clocks = SignalledClocks::default();

        // build the object but do not attempt to sync for this test
        let ptp_clock = glib::Object::builder::<gst_net::PtpClock>()
            .property("domain", PTP_DOMAIN_NUMBER as u32)
            .build()
            .upcast::<gst::Clock>();
        // Add the PTP clock gmid & domain number (version is inferred)
        signalled_clocks
            .add(
                glib::GString::from(
                    sdp_types::ReferenceClock::try_from_ptp_gmid_with_domain_number(
                        PTP_GMID,
                        PTP_DOMAIN_NUMBER,
                    )
                    .unwrap()
                    .to_string(),
                )
                .as_gstr(),
                ptp_clock,
            )
            .unwrap();

        let media_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("payload", PT as i32)
            .field("clock-rate", CLOCK_RATE as i32)
            .field("encoding-name", "L24")
            .field(
                "a-ts-refclk",
                format!("ptp={PTP_VERSION}:{PTP_GMID_STR}:{PTP_DOMAIN_NUMBER}"),
            )
            .build();
        signalled_clocks.add_or_update_from_caps(PT, &media_caps);
        signalled_clocks.maybe_init_reference_clock(SSRC, PT);

        let source_clock = signalled_clocks.get_reference_clock(SSRC).unwrap();
        assert_eq!(
            source_clock.ts_meta_ref(),
            &gst::Caps::builder("timestamp/x-ptp")
                .field("version", PTP_VERSION)
                .field("gmid", PTP_GMID)
                .field("domain", PTP_DOMAIN_NUMBER as u32)
                .build(),
        );

        let packet_ptp_time = 5.seconds() + *FIRST_JANUARY_2026_PTP_TIME;
        let rtptime = (packet_ptp_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        let ptp_clock_now = 6.seconds() + *FIRST_JANUARY_2026_PTP_TIME;
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(ptp_clock_now, rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_ptp_time);

        // packet clock in NTP time:
        let packet_ntp_time_ns = (packet_ptp_time + gst::clock_time::UNIX_TO_NTP_TIME_OFFSET
            - *gst::clock_time::UNIX_TO_PTP_TIME_OFFSET)
            .nseconds();
        let ntp_time = NtpTime::from_duration(Duration::from_nanos(packet_ntp_time_ns));
        // reference time is expressed in PTP time
        assert_eq!(source_clock.to_reference_time(ntp_time), packet_ptp_time);
    }

    #[test]
    fn get_reference_time_system_clock_as_ptp() {
        init();

        let mut signalled_clocks = SignalledClocks::default();

        let system_clock = glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build()
            .upcast::<gst::Clock>();
        // Add the system clock and declare it as synced to PTP clock gmid & domain number (version is inferred)
        signalled_clocks
            .add(
                glib::GString::from(
                    sdp_types::ReferenceClock::try_from_ptp_gmid_with_domain_number(
                        PTP_GMID,
                        PTP_DOMAIN_NUMBER,
                    )
                    .unwrap()
                    .to_string(),
                )
                .as_gstr(),
                system_clock,
            )
            .unwrap();

        let media_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("payload", PT as i32)
            .field("clock-rate", CLOCK_RATE as i32)
            .field("encoding-name", "L24")
            .field(
                "a-ts-refclk",
                format!("ptp={PTP_VERSION}:{PTP_GMID_STR}:{PTP_DOMAIN_NUMBER}"),
            )
            .build();
        signalled_clocks.add_or_update_from_caps(PT, &media_caps);
        signalled_clocks.maybe_init_reference_clock(SSRC, PT);

        let source_clock = signalled_clocks.get_reference_clock(SSRC).unwrap();
        assert_eq!(
            source_clock.ts_meta_ref(),
            &gst::Caps::builder("timestamp/x-ptp")
                .field("version", PTP_VERSION)
                .field("gmid", PTP_GMID)
                .field("domain", PTP_DOMAIN_NUMBER as u32)
                .build(),
        );

        let packet_ptp_time = 5.seconds() + *FIRST_JANUARY_2026_PTP_TIME;
        let rtptime = (packet_ptp_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        let system_clock_now = 6.seconds() + FIRST_JANUARY_2026_UNIX_TIME;
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(system_clock_now, rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_ptp_time);

        // packet clock in NTP time:
        let packet_ntp_time_ns = (packet_ptp_time + gst::clock_time::UNIX_TO_NTP_TIME_OFFSET
            - *gst::clock_time::UNIX_TO_PTP_TIME_OFFSET)
            .nseconds();
        let ntp_time = NtpTime::from_duration(Duration::from_nanos(packet_ntp_time_ns));
        // reference time is expressed in PTP time
        assert_eq!(source_clock.to_reference_time(ntp_time), packet_ptp_time);
    }

    #[test]
    fn get_reference_time_first_period_offset_0() {
        init();

        let media_clock = MediaLevelClock::new(
            &SignalledClock::try_new(
                glib::gstr!("local"),
                glib::Object::builder::<gst::SystemClock>()
                    .property("clock-type", gst::ClockType::Realtime)
                    .build()
                    .upcast(),
            )
            .unwrap(),
        );
        let source_clock = SourceLevelClock::from(SSRC, &media_clock);
        assert_eq!(source_clock.offset, 0);

        // # first period packet

        let packet_sys_time = 5.seconds();
        let packet_rtptime = (packet_sys_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        // 'now' first period, after packet sys time
        let sys_clock_now = packet_sys_time + 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, packet_rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // 'now' first period, before packet sys time
        let sys_clock_now = packet_sys_time - 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, packet_rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);
    }

    #[test]
    fn get_reference_time_first_period_offset_worth_7s() {
        init();

        let offset_time = 7.seconds();
        let offset = (offset_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        let mut media_clock = MediaLevelClock::new(
            &SignalledClock::try_new(
                glib::gstr!("local"),
                glib::Object::builder::<gst::SystemClock>()
                    .property("clock-type", gst::ClockType::Realtime)
                    .build()
                    .upcast(),
            )
            .unwrap(),
        );
        media_clock
            .add_mediaclk(&format!("direct={offset}"))
            .unwrap();
        let source_clock = SourceLevelClock::from(SSRC, &media_clock);
        assert_eq!(source_clock.offset, offset);

        // # first period, packet before offset

        let packet_sys_time = offset_time - 2.seconds();
        let rtptime = (packet_sys_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;
        let rtptime_with_offset = rtptime.wrapping_add(offset);

        // 'now' first period, after packet sys time
        let sys_clock_now = packet_sys_time + 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime_with_offset, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // 'now' first period, before packet sys time
        let sys_clock_now = packet_sys_time - 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime_with_offset, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // # first period, packet after offset

        let packet_sys_time = offset_time + 1.seconds();
        let packet_rtptime = (packet_sys_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;
        let rtptime_with_offset = packet_rtptime.wrapping_add(offset);

        // 'now' first period, after packet sys time
        let sys_clock_now = packet_sys_time + 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime_with_offset, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // 'now' first period, before packet sys time
        let sys_clock_now = packet_sys_time - 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime_with_offset, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);
    }

    #[test]
    fn get_reference_time_second_period_offset_0() {
        init();

        let media_clock = MediaLevelClock::new(
            &SignalledClock::try_new(
                glib::gstr!("local"),
                glib::Object::builder::<gst::SystemClock>()
                    .property("clock-type", gst::ClockType::Realtime)
                    .build()
                    .upcast(),
            )
            .unwrap(),
        );
        let source_clock = SourceLevelClock::from(SSRC, &media_clock);
        assert_eq!(source_clock.offset, 0);

        // # second period packet

        let second_period_rtime_ext = 1u64 << 32;
        let second_period_start_time = gst::ClockTime::from_nseconds(
            second_period_rtime_ext
                .mul_div_floor(SECOND, CLOCK_RATE as u64)
                .unwrap(),
        );

        // align on packet multiples to avoid rounding errors
        let packet_relative_time = 5.seconds();
        let packet_rtime_ext = second_period_rtime_ext
            + packet_relative_time
                .nseconds()
                .mul_div_floor(CLOCK_RATE as u64, SECOND)
                .unwrap();
        let packet_sys_time = gst::ClockTime::from_nseconds(
            packet_rtime_ext
                .mul_div_floor(SECOND, CLOCK_RATE as u64)
                .unwrap(),
        );
        let packet_rtptime = (packet_rtime_ext & (u32::MAX as u64)) as u32;

        // 'now' second period, after packet sys time
        let sys_clock_now = packet_sys_time + 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, packet_rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // 'now' second period, before packet sys time
        let sys_clock_now = packet_sys_time - 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, packet_rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // 'now' first period
        let sys_clock_now = second_period_start_time - 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, packet_rtptime, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);
    }

    #[test]
    fn get_reference_time_second_period_offset_worth_7s() {
        init();

        let offset_time = 7.seconds();
        let offset = (offset_time
            .nseconds()
            .mul_div_floor(CLOCK_RATE as u64, SECOND)
            .unwrap()
            & (u32::MAX as u64)) as u32;

        let mut media_clock = MediaLevelClock::new(
            &SignalledClock::try_new(
                glib::gstr!("local"),
                glib::Object::builder::<gst::SystemClock>()
                    .property("clock-type", gst::ClockType::Realtime)
                    .build()
                    .upcast(),
            )
            .unwrap(),
        );
        media_clock
            .add_mediaclk(&format!("direct={offset}"))
            .unwrap();
        let source_clock = SourceLevelClock::from(SSRC, &media_clock);
        assert_eq!(source_clock.offset, offset);

        // # second period packet

        let second_period_rtime_ext = 1u64 << 32;
        let second_period_start_time = gst::ClockTime::from_nseconds(
            second_period_rtime_ext
                .mul_div_floor(SECOND, CLOCK_RATE as u64)
                .unwrap(),
        );

        // align on packet multiples to avoid rounding errors
        let packet_relative_time = 5.seconds();
        let packet_rtime_ext = second_period_rtime_ext
            + packet_relative_time
                .nseconds()
                .mul_div_floor(CLOCK_RATE as u64, SECOND)
                .unwrap();
        let packet_sys_time = gst::ClockTime::from_nseconds(
            packet_rtime_ext
                .mul_div_floor(SECOND, CLOCK_RATE as u64)
                .unwrap(),
        );
        let packet_rtptime = (packet_rtime_ext & (u32::MAX as u64)) as u32;
        let rtptime_with_offset = packet_rtptime.wrapping_add(offset);

        // 'now' second period, after packet sys time
        let sys_clock_now = packet_sys_time + 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime_with_offset, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // 'now' second period, before packet sys time
        let sys_clock_now = packet_sys_time - 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime_with_offset, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);

        // 'now' first period
        let sys_clock_now = second_period_start_time - 1.seconds();
        let rtp_ref_clock_time =
            source_clock.get_reference_time_priv(sys_clock_now, rtptime_with_offset, CLOCK_RATE);
        assert_eq!(rtp_ref_clock_time, packet_sys_time);
    }
}
