// GStreamer APE tag demuxer - utility functions
//
// Copyright (C) 2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

use std::num::NonZeroU32;
use std::str::FromStr;

#[derive(Debug, PartialEq)]
pub(crate) struct NumberWithCount {
    pub(crate) number: Option<NonZeroU32>,
    pub(crate) count: Option<NonZeroU32>,
}

impl FromStr for NumberWithCount {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (n, c) = match s.split_once('/') {
            None => (Some(s.parse::<NonZeroU32>()?), None),
            Some((n_str, c_str)) => {
                let n = n_str.parse::<NonZeroU32>().ok();
                let c = c_str.parse::<NonZeroU32>().ok();

                match (n, c) {
                    (Some(_), ..) | (.., Some(_)) => (n, c),
                    _ => return Err(n_str.parse::<NonZeroU32>().expect_err("parse")),
                }
            }
        };

        Ok(NumberWithCount {
            number: n,
            count: c,
        })
    }
}

// Floating point strings can be "1,234" or "1.234" depending on the locale
// of whoever wrote the tag. We need to be able to parse and read either
// version, so this is a little helper for that.
// Note that f64::from_str() works locale-independent.
#[derive(Debug, PartialEq)]
pub struct FloatFromString {
    pub value: f64,
}

impl FromStr for FloatFromString {
    type Err = std::num::ParseFloatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = if s.contains(',') {
            s.replace(',', ".").parse::<f64>()?
        } else {
            s.parse::<f64>()?
        };

        Ok(FloatFromString { value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apedemux_utils_number_with_count() {
        assert_eq!(
            "1/2".parse::<NumberWithCount>(),
            Ok(NumberWithCount {
                number: Some(NonZeroU32::new(1).unwrap()),
                count: Some(NonZeroU32::new(2).unwrap())
            })
        );
        // We just parse, we don't validate (but perhaps we should?)
        assert_eq!(
            "2/1".parse::<NumberWithCount>(),
            Ok(NumberWithCount {
                number: Some(NonZeroU32::new(2).unwrap()),
                count: Some(NonZeroU32::new(1).unwrap())
            })
        );
        assert_eq!(
            "0/2".parse::<NumberWithCount>(),
            Ok(NumberWithCount {
                number: None,
                count: Some(NonZeroU32::new(2).unwrap())
            })
        );
        assert_eq!(
            "-1/2".parse::<NumberWithCount>(),
            Ok(NumberWithCount {
                number: None,
                count: Some(NonZeroU32::new(2).unwrap())
            })
        );
        assert_eq!(
            "1".parse::<NumberWithCount>(),
            Ok(NumberWithCount {
                number: Some(NonZeroU32::new(1).unwrap()),
                count: None
            })
        );
        assert_eq!(
            "cd 1/2".parse::<NumberWithCount>(),
            Ok(NumberWithCount {
                number: None,
                count: Some(NonZeroU32::new(2).unwrap())
            })
        );
        assert!("-1".parse::<NumberWithCount>().is_err());
        assert!("abc".parse::<NumberWithCount>().is_err());
        assert!("".parse::<NumberWithCount>().is_err());
        assert!("0".parse::<NumberWithCount>().is_err());
    }

    #[test]
    fn test_apedemux_utils_float_from_string() {
        assert_eq!(
            "64".parse::<FloatFromString>(),
            Ok(FloatFromString { value: 64.0 })
        );
        assert_eq!(
            "64,8".parse::<FloatFromString>(),
            Ok(FloatFromString { value: 64.8 })
        );
        assert_eq!(
            "64.8".parse::<FloatFromString>(),
            Ok(FloatFromString { value: 64.8 })
        );
        assert!("abc".parse::<FloatFromString>().is_err());
        assert!("".parse::<FloatFromString>().is_err());
    }
}
