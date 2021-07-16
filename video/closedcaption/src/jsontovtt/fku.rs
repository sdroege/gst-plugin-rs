// Copyright (C) 2021 Jan Schmidt <jan@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::cmp::Ordering;

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct ForceKeyUnitRequest {
    pub running_time: gst::ClockTime,
    pub all_headers: bool,
    pub count: u32,
}

impl Ord for ForceKeyUnitRequest {
    fn cmp(&self, other: &Self) -> Ordering {
        self.running_time.cmp(&other.running_time)
    }
}

impl PartialOrd for ForceKeyUnitRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl ForceKeyUnitRequest {
    pub fn new_from_event(fku: &gst_video::UpstreamForceKeyUnitEvent) -> ForceKeyUnitRequest {
        ForceKeyUnitRequest {
            running_time: fku.running_time.unwrap(),
            all_headers: fku.all_headers,
            count: fku.count,
        }
    }
}
