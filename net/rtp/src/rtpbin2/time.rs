// SPDX-License-Identifier: MPL-2.0

use std::{
    ops::{Add, Sub},
    time::{Duration, SystemTime},
};

use gst::prelude::MulDiv as _;

use std::sync::OnceLock;

// time between the NTP time at 1900-01-01 and the unix EPOCH (1970-01-01)
const NTP_OFFSET: Duration = Duration::from_secs((365 * 70 + 17) * 24 * 60 * 60);

static CURRENT_TIME: OnceLock<SystemTime> = OnceLock::new();

pub fn get_or_init_current_time<'a>() -> &'a SystemTime {
    CURRENT_TIME.get_or_init(SystemTime::now)
}

pub fn ntp_era_from_system_time(st: &SystemTime) -> u64 {
    st.duration_since(SystemTime::UNIX_EPOCH)
        .expect("NTP time is before unix epoch?!")
        .add(NTP_OFFSET)
        .as_secs()
        / (1 << 32)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NtpTime(u64);

impl NtpTime {
    pub fn from_duration(dur: Duration) -> Self {
        let seconds = dur.as_secs();
        let fractional = (dur.subsec_nanos() as u64)
            .mul_div_ceil(1 << 32, 1_000_000_000)
            .unwrap();

        let ntp = seconds << 32 | fractional;

        Self(ntp)
    }

    /// Converts to a duration relative to the prime epoch (1900-01-01 at 00:00).
    pub fn as_duration(&self) -> Duration {
        self.as_duration_with_current_time(get_or_init_current_time())
    }

    /// Converts to a duration relative to the prime epoch (1900-01-01 at 00:00).
    pub fn as_duration_with_current_time(&self, current_time: &SystemTime) -> Duration {
        let current_ntp_time = system_time_to_ntp_time_u64(*current_time);
        let mut timestamp_era = ntp_era_from_system_time(current_time);

        if current_ntp_time.0 > self.0 && current_ntp_time.0 - self.0 > 1 << 63 {
            timestamp_era += 1;
        } else if current_ntp_time.0 < self.0 && self.0 - current_ntp_time.0 > 1 << 63 {
            timestamp_era -= 1;
        }

        let nanos = self
            .0
            .mul_div_ceil(1_000_000_000, 1 << 32)
            .expect("result doesn't fit?!");

        Duration::from_nanos(nanos) + Duration::from_secs(timestamp_era << 32)
    }

    /// Middle 32 bit of the NTP timestamp (16.16 seconds).
    pub fn as_u32(self) -> u32 {
        ((self.0 >> 16) & 0xffffffff) as u32
    }

    /// Full 64 bit NTP timestamp (32.32 seconds).
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl Sub for NtpTime {
    type Output = NtpTime;
    fn sub(self, rhs: Self) -> Self::Output {
        NtpTime(self.0 - rhs.0)
    }
}

impl Add for NtpTime {
    type Output = NtpTime;
    fn add(self, rhs: Self) -> Self::Output {
        NtpTime(self.0 + rhs.0)
    }
}

pub fn system_time_to_ntp_time_u64(time: SystemTime) -> NtpTime {
    let dur = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("time is before unix epoch?!")
        + NTP_OFFSET;

    NtpTime::from_duration(dur)
}

impl From<u64> for NtpTime {
    fn from(value: u64) -> Self {
        NtpTime(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_rollover() {
        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(ntpt.as_u64(), (u32::MAX as u64) << 32);

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(ntpt.as_u64(), 0);
    }

    #[test]
    fn ntp_time_as_duration_before_rollover() {
        let current_time: SystemTime =
            chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
                .unwrap()
                .into();

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967295
        );

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967296
        );
    }

    #[test]
    fn ntp_time_as_duration_after_rollover() {
        let current_time: SystemTime =
            chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
                .unwrap()
                .into();

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967295
        );

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967296
        );
    }
}
