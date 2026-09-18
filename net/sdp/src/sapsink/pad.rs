// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::{
    collections::BTreeSet,
    sync::{LazyLock, Mutex},
};

#[derive(Default)]
struct Settings {
    dest_ip: Option<String>,
    dest_port: u16,
}

#[derive(Default)]
struct State {
    caps: Option<gst::Caps>,
}

/// Result of checking caps against the caps a stream was started with.
#[derive(Debug, PartialEq, Eq)]
pub enum CapsStatus {
    /// The pad had no caps before, these are now stored.
    Stored,
    /// The caps are equivalent to the stored ones.
    Same,
    /// The caps differ from the stored ones.
    Conflict,
}

#[derive(Default)]
pub struct SapSinkPad {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for SapSinkPad {
    const NAME: &'static str = "GstSapSinkPad";
    type Type = super::SapSinkPad;
    type ParentType = gst::GhostPad;
}

impl ObjectImpl for SapSinkPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("dest-ip")
                    .nick("Destination IP")
                    .blurb(
                        "IP address to send this media stream to, defaults to a \
                             multicast group if unset",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("dest-port")
                    .nick("Destination Port")
                    .blurb("UDP port to send this media stream to (required)")
                    .minimum(0)
                    .maximum(u32::from(u16::MAX))
                    .default_value(0)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "dest-ip" => {
                let v = value.get::<Option<&str>>().expect("type checked upstream");
                self.settings.lock().unwrap().dest_ip =
                    v.filter(|s| !s.is_empty()).map(str::to_string);
            }
            "dest-port" => {
                self.settings.lock().unwrap().dest_port =
                    value.get::<u32>().expect("type checked upstream") as u16;
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "dest-ip" => settings.dest_ip.as_ref().to_value(),
            "dest-port" => u32::from(settings.dest_port).to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for SapSinkPad {}

impl PadImpl for SapSinkPad {}

impl ProxyPadImpl for SapSinkPad {}

impl GhostPadImpl for SapSinkPad {}

impl SapSinkPad {
    /// Configured destination of this stream, port 0 if unset.
    pub fn dest(&self) -> (Option<String>, u16) {
        let settings = self.settings.lock().unwrap();
        (settings.dest_ip.clone(), settings.dest_port)
    }

    pub fn caps(&self) -> Option<gst::Caps> {
        self.state.lock().unwrap().caps.clone()
    }

    /// Clears the per-stream state, e.g. when going back to READY so the
    /// next start waits for new caps.
    pub fn reset(&self) {
        *self.state.lock().unwrap() = State::default();
    }

    /// Checks `caps` against the caps stored on the pad. The first caps are
    /// stored, later ones have to describe the same set of payload types.
    pub fn check_caps(&self, caps: &gst::Caps) -> CapsStatus {
        let mut state = self.state.lock().unwrap();
        let Some(existing) = &state.caps else {
            state.caps = Some(caps.clone());
            return CapsStatus::Stored;
        };
        if caps_identity(existing) == caps_identity(caps) {
            CapsStatus::Same
        } else {
            CapsStatus::Conflict
        }
    }
}

/// Identity of RTP caps: everything that would result in a different set of
/// media lines / payload types in the SDP.
fn caps_identity(caps: &gst::CapsRef) -> BTreeSet<(String, String, i32, String, i32)> {
    caps.iter()
        .map(|s| {
            (
                s.name().to_string(),
                s.get::<&str>("media").unwrap_or_default().to_string(),
                s.get::<i32>("payload").unwrap_or(-1),
                s.get::<&str>("encoding-name")
                    .map(|n| n.to_ascii_lowercase())
                    .unwrap_or_default(),
                s.get::<i32>("clock-rate").unwrap_or(-1),
            )
        })
        .collect()
}
