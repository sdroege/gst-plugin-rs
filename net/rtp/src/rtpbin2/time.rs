// SPDX-License-Identifier: MPL-2.0

use std::{
    fmt,
    ops::{Add, Sub},
    time::{Duration, SystemTime},
};

use gst::prelude::MulDiv as _;

// time between the NTP time at 1900-01-01 and the unix EPOCH (1970-01-01)
const NTP_OFFSET: Duration = Duration::from_secs((365 * 70 + 17) * 24 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NtpTime(u64);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NtpOutOfRangeError;

impl fmt::Display for NtpOutOfRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("value out of range for NTP timestamps")
    }
}

impl std::error::Error for NtpOutOfRangeError {}

impl NtpTime {
    pub fn from_duration(dur: Duration) -> Self {
        let seconds = dur.as_secs();
        let fractional = (dur.subsec_nanos() as u64)
            .mul_div_ceil(1 << 32, 1_000_000_000)
            .unwrap();

        let ntp = seconds << 32 | fractional;

        Self(ntp)
    }

    /// Converts to a duration relative to the UNIX epoch.
    pub fn as_duration(&self) -> Result<Duration, NtpOutOfRangeError> {
        let nanos = self
            .0
            .mul_div_ceil(1_000_000_000, 1 << 32)
            .ok_or(NtpOutOfRangeError)?;
        Ok(Duration::from_nanos(nanos))
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
    use chrono::prelude::*;

    #[test]
    fn ntp_rollover() {
        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(ntpt.as_u64(), (std::u32::MAX as u64) << 32);

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(ntpt.as_u64(), 0);
    }
}
