// Copyright (C) 2024 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-mpegtslivesrc
 * @see_also: udpsrc, srtsrtc, tsdemux
 *
 * Clock provider from live MPEG-TS sources.
 *
 * This element allows wrapping an existing live "mpeg-ts source" (udpsrc,
 * srtsrc,...) and providing a clock based on the actual PCR of the stream.
 *
 * Combined with tsdemux ignore-pcr=True downstream of it, this allows playing
 * back the content at the same rate as the (remote) provider and not modify the
 * original timestamps.
 *
 * Since: plugins-rs-0.13.0
 */
use anyhow::Context;
use anyhow::{bail, Result};
use bitstream_io::{BigEndian, BitRead, BitReader};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::ops::Add;
use std::ops::ControlFlow;
use std::sync::Mutex;

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "mpegtslivesrc",
        gst::DebugColorFlags::empty(),
        Some("MPEG-TS Live Source"),
    )
});

#[derive(Clone, Copy, Debug)]
struct MpegTsPcr {
    // Raw PCR value
    value: u64,
    // Number of wraparounds to apply
    wraparound: u64,
}

impl MpegTsPcr {
    // Maximum PCR value
    const MAX: u64 = ((1 << 33) * 300) - 1;
    const RATE: u64 = 27_000_000;

    // Create a new PCR given the 27MHz unit.
    // Can be provided values exceed MAX_PCR and will automatically calculate
    // the number of wraparound involved
    fn new(value: u64) -> MpegTsPcr {
        MpegTsPcr {
            value: value % (Self::MAX + 1),
            wraparound: value / (Self::MAX + 1),
        }
    }

    // Create a new PCR given the 27MHz unit and the latest PCR observed.
    // The wraparound will be based on the provided reference PCR
    //
    // If a discontinuity greater than 15s is detected, no value will be
    // returned
    //
    // Note, this constructor will clamp value to be within MAX_PCR
    fn new_with_reference(
        imp: &MpegTsLiveSource,
        value: u64,
        reference: &MpegTsPcr,
    ) -> Option<MpegTsPcr> {
        // Clamp our value to maximum
        let value = value % (Self::MAX + 1);
        let ref_value = reference.value;

        // Fast path, within 15s
        if value.abs_diff(ref_value) <= (15 * Self::RATE) {
            return Some(MpegTsPcr {
                value,
                wraparound: reference.wraparound,
            });
        };

        // new value wrapped around
        if (value + Self::MAX + 1).abs_diff(ref_value) <= 15 * Self::RATE {
            gst::debug!(
                CAT,
                imp = imp,
                "Wraparound detected %{value} vs %{ref_value}"
            );
            return Some(MpegTsPcr {
                value,
                wraparound: reference.wraparound + 1,
            });
        };

        // new value went below 0
        if value.abs_diff(ref_value + Self::MAX + 1) <= 15 * Self::RATE {
            gst::debug!(
                CAT,
                imp = imp,
                "Backward PCR within tolerance detected %{value} vs %{ref_value}"
            );
            return Some(MpegTsPcr {
                value,
                wraparound: reference.wraparound - 1,
            });
        }

        gst::debug!(CAT, imp = imp, "Discont detected %{value} vs %{ref_value}");
        None
    }

    // Full value with wraparound in 27MHz units
    fn to_units(self) -> u64 {
        self.wraparound * (Self::MAX + 1) + self.value
    }

    fn saturating_sub(self, other: MpegTsPcr) -> MpegTsPcr {
        MpegTsPcr::new(self.to_units().saturating_sub(other.to_units()))
    }
}

impl Add for MpegTsPcr {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        MpegTsPcr::new(self.to_units() + other.to_units())
    }
}

impl From<MpegTsPcr> for gst::ClockTime {
    fn from(value: MpegTsPcr) -> gst::ClockTime {
        gst::ClockTime::from_nseconds(
            value
                .to_units()
                .mul_div_floor(1000, 27)
                .expect("failed to convert"),
        )
    }
}

impl From<gst::ClockTime> for MpegTsPcr {
    fn from(value: gst::ClockTime) -> MpegTsPcr {
        MpegTsPcr::new(
            value
                .nseconds()
                .mul_div_floor(27, 1000)
                .expect("Failed to convert"),
        )
    }
}

struct MpegTSLiveSourceState {
    // Controlled source element
    source: Option<gst::Element>,

    // Clock we control and expose
    external_clock: gst::SystemClock,

    // Last observed PCR (for handling wraparound)
    last_seen_pcr: Option<MpegTsPcr>,

    // First observed PCR and associated timestamp
    base_pcr: Option<MpegTsPcr>,
    base_monotonic: Option<gst::ClockTime>,
}

impl MpegTSLiveSourceState {
    /// Grab time of our clock and controlled clock
    ///
    /// Returns `true` on PCR discontinuities.
    fn store_observation(
        &mut self,
        imp: &MpegTsLiveSource,
        pcr: u64,
        monotonic_time: gst::ClockTime,
    ) -> bool {
        // If this is the first PCR we observe:
        // * Remember the PCR *and* the associated monotonic clock value when capture
        // * `base_pcr` `base_monotonic`

        // If we have a PCR we need to store an observation
        // * Subtract the base PCR from that value and add the base monotonic value
        //   * observation_monotonic = pcr - base_pcr + base_monotonic
        // * Store (observation_monotonic, buffer_pts)

        let new_pcr: MpegTsPcr;
        let mut discont = false;

        if let (Some(base_pcr), Some(base_monotonic), Some(last_seen_pcr)) =
            (self.base_pcr, self.base_monotonic, self.last_seen_pcr)
        {
            gst::trace!(CAT, imp = imp, "pcr:{pcr}, monotonic_time:{monotonic_time}");

            let mut handled_pcr = MpegTsPcr::new_with_reference(imp, pcr, &last_seen_pcr);
            if let Some(new_pcr) = handled_pcr {
                // First check if this is more than 1s off from the current clock calibration and
                // if so consider it a discontinuity too.
                let (internal, external, num, denom) = self.external_clock.calibration();

                let expected_external = gst::Clock::adjust_with_calibration(
                    monotonic_time,
                    internal,
                    external,
                    num,
                    denom,
                );
                let new_external =
                    gst::ClockTime::from(new_pcr.saturating_sub(base_pcr)) + base_monotonic;
                if expected_external.absdiff(new_external) >= gst::ClockTime::SECOND {
                    gst::warning!(
                        CAT,
                        imp = imp,
                        "New PCR clock estimation {new_external} too far from old estimation {expected_external}: {}",
                        new_external.into_positive() - expected_external,
                    );
                    handled_pcr = None;
                }
            }

            if let Some(handled_pcr) = handled_pcr {
                new_pcr = handled_pcr;
                gst::trace!(
                    CAT,
                    imp = imp,
                    "Adding new observation internal: {} -> external: {}",
                    gst::ClockTime::from(new_pcr.saturating_sub(base_pcr)) + base_monotonic,
                    monotonic_time,
                );
                self.external_clock.add_observation(
                    monotonic_time,
                    gst::ClockTime::from(new_pcr.saturating_sub(base_pcr)) + base_monotonic,
                );
            } else {
                let (internal, external, num, denom) = self.external_clock.calibration();
                let scaled_monotonic = gst::Clock::adjust_with_calibration(
                    monotonic_time,
                    internal,
                    external,
                    num,
                    denom,
                );
                gst::warning!(CAT, imp = imp, "DISCONT detected, Picking new reference times (pcr:{pcr:#?}, monotonic:{monotonic_time}, scaled monotonic:{scaled_monotonic}");
                new_pcr = MpegTsPcr::new(pcr);
                self.base_pcr = Some(new_pcr);
                self.base_monotonic = Some(monotonic_time);
                discont = true;
            }
        } else {
            gst::debug!(
                CAT,
                imp = imp,
                "Picking initial reference times (pcr:{pcr:#?}, monotonic:{monotonic_time}"
            );
            new_pcr = MpegTsPcr::new(pcr);
            self.base_pcr = Some(new_pcr);
            self.base_monotonic = Some(monotonic_time);
        }
        self.last_seen_pcr = Some(new_pcr);

        discont
    }
}

// Struct containing all the element data
pub struct MpegTsLiveSource {
    srcpad: gst::GhostPad,

    // Clock set on source element
    internal_clock: gst::SystemClock,

    state: Mutex<MpegTSLiveSourceState>,
}

fn find_pcr(slice: &[u8], imp: &MpegTsLiveSource) -> Result<Option<u64>> {
    // Find sync byte
    let Some(pos) = slice.iter().position(|&b| b == 0x47) else {
        bail!("Couldn't find sync byte");
    };
    let mut buffer_pcr = None;

    for chunk in slice[pos..].chunks_exact(188) {
        if chunk[0] != 0x47 {
            gst::error!(CAT, imp = imp, "Lost sync");
            break;
        }
        let mut reader = BitReader::endian(chunk, BigEndian);
        // Sync Byte
        reader.skip(8)?;
        // Transport Error Indicator
        if reader.read_bit()? {
            continue;
        };
        // PUSI and transport priority
        reader.skip(2).context("PUSI and transport priority")?;
        // PID
        let pid = reader.read::<u16>(13).expect("PID");
        // transport scrambling control
        reader.skip(2)?;
        // Adaptation field present
        let af_present = reader.read_bit().context("Adaptation field present")?;
        reader.skip(5)?;
        if af_present {
            // adaptation_field_length
            if reader.read::<u8>(8).context("adaptation field length")? >= 7 {
                reader.skip(3)?;
                let pcr_present = reader.read_bit().context("pcr_present")?;
                reader.skip(4)?;
                if pcr_present {
                    let pcr_base = reader.read::<u64>(33).context("PCR_base")?;
                    reader.skip(6)?;
                    let pcr_ext = reader.read::<u64>(9).context("PCR_ext")?;
                    let pcr = pcr_base * 300 + pcr_ext;
                    gst::debug!(CAT, imp = imp, "PID {pid} PCR {pcr}");
                    buffer_pcr = Some(pcr);
                    break;
                }
            }
        }
    }

    Ok(buffer_pcr)
}

fn get_pcr_from_buffer(imp: &MpegTsLiveSource, buffer: &gst::Buffer) -> Option<u64> {
    let Ok(range) = buffer.map_readable() else {
        return None;
    };
    let buffer_pcr = match find_pcr(range.as_slice(), imp) {
        Ok(pcr) => pcr,
        Err(err) => {
            gst::error!(CAT, imp = imp, "Failed parsing MPEG-TS packets: {err}");
            return None;
        }
    };

    let Some(raw_pcr) = buffer_pcr else {
        gst::debug!(CAT, imp = imp, "No PCR observed in {:?}", buffer);
        return None;
    };
    Some(raw_pcr)
}

impl MpegTsLiveSource {
    // process a buffer to extract the PCR
    fn chain(
        &self,
        pad: &gst::ProxyPad,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let base_time = self.obj().base_time().expect("No base time on element");
        let mut monotonic_time = None;
        let buffer_timestamp = buffer.dts_or_pts();

        if let Some(pts) = buffer_timestamp {
            monotonic_time = Some(pts + base_time);
        };

        if let (Some(monotonic_time), Some(raw_pcr)) =
            (monotonic_time, get_pcr_from_buffer(self, &buffer))
        {
            if state.store_observation(self, raw_pcr, monotonic_time) {
                let buffer = buffer.make_mut();
                buffer.set_flags(gst::BufferFlags::DISCONT);
            }
        };

        // Update buffer timestamp if present
        if let Some(pts) = buffer_timestamp {
            let buffer = buffer.make_mut();
            let new_pts = state
                .external_clock
                .adjust_unlocked(pts + base_time)
                .expect("Couldn't adjust {pts}")
                .saturating_sub(base_time);
            gst::debug!(
                CAT,
                imp = self,
                "Updating buffer pts from {pts} to {:?}",
                new_pts
            );
            buffer.set_pts(new_pts);
            buffer.set_dts(new_pts);
        };

        gst::ProxyPad::chain_default(pad, Some(&*self.obj()), buffer)
    }

    fn chain_list(
        &self,
        pad: &gst::ProxyPad,
        mut bufferlist: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let base_time = self.obj().base_time().expect("No base time on element");

        // The last monotonic time
        let mut monotonic_time = None;

        bufferlist.make_mut().foreach_mut(|mut buffer, _idx| {
            let this_buffer_timestamp = buffer.dts_or_pts();

            // Grab latest buffer timestamp, we want to use the "latest" one for
            // our observations. Depending on the use-cases, this might only be
            // present on the first buffer of the list or on all
            if let Some(pts) = this_buffer_timestamp {
                monotonic_time = Some(pts + base_time);
            };

            // Store observation if pcr is present
            if let (Some(monotonic_time), Some(raw_pcr)) =
                (monotonic_time, get_pcr_from_buffer(self, &buffer))
            {
                if state.store_observation(self, raw_pcr, monotonic_time) {
                    let buffer = buffer.make_mut();
                    buffer.set_flags(gst::BufferFlags::DISCONT);
                }
            };

            // Update buffer timestamp if present
            if let Some(pts) = this_buffer_timestamp {
                let buffer = buffer.make_mut();
                let new_pts = state
                    .external_clock
                    .adjust_unlocked(pts + base_time)
                    .expect("Couldn't adjust {pts}")
                    .saturating_sub(base_time);
                gst::debug!(
                    CAT,
                    imp = self,
                    "Updating buffer pts from {pts} to {:?}",
                    new_pts
                );
                buffer.set_pts(new_pts);
                buffer.set_dts(new_pts);
            };
            ControlFlow::Continue(Some(buffer))
        });

        gst::ProxyPad::chain_list_default(pad, Some(&*self.obj()), bufferlist)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for MpegTsLiveSource {
    const NAME: &'static str = "GstMpegTsLiveSource";
    type Type = super::MpegTsLiveSource;
    type ParentType = gst::Bin;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::GhostPad::builder_from_template(&templ)
            .name(templ.name())
            .proxy_pad_chain_function(move |pad, parent, buffer| {
                let parent = parent.and_then(|p| p.parent());
                MpegTsLiveSource::catch_panic_pad_function(
                    parent.as_ref(),
                    || Err(gst::FlowError::Error),
                    |imp| imp.chain(pad, buffer),
                )
            })
            .proxy_pad_chain_list_function(move |pad, parent, bufferlist| {
                let parent = parent.and_then(|p| p.parent());
                MpegTsLiveSource::catch_panic_pad_function(
                    parent.as_ref(),
                    || Err(gst::FlowError::Error),
                    |imp| imp.chain_list(pad, bufferlist),
                )
            })
            .flags(
                gst::PadFlags::PROXY_CAPS
                    | gst::PadFlags::PROXY_ALLOCATION
                    | gst::PadFlags::PROXY_SCHEDULING,
            )
            .build();
        let internal_clock = glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Monotonic)
            .property("name", "mpegts-internal-clock")
            .build();
        let external_clock = glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Monotonic)
            .property("name", "mpegts-live-clock")
            .build();
        // Return an instance of our struct
        Self {
            srcpad,
            internal_clock,
            state: Mutex::new(MpegTSLiveSourceState {
                source: None,
                external_clock,
                last_seen_pcr: None,
                base_pcr: None,
                base_monotonic: None,
            }),
        }
    }
}

impl ObjectImpl for MpegTsLiveSource {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecObject::builder::<gst::Element>("source")
                    .nick("Source")
                    .blurb("Source element")
                    .mutable_ready()
                    .readwrite()
                    .build(),
                glib::ParamSpecInt::builder("window-size")
                    .nick("Window Size")
                    .blurb("The size of the window used to calculate rate and offset")
                    .minimum(2)
                    .maximum(1024)
                    .default_value(32)
                    .readwrite()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "source" => {
                let mut state = self.state.lock().unwrap();
                if let Some(existing_source) = state.source.take() {
                    let _ = self.obj().remove(&existing_source);
                    let _ = self.srcpad.set_target(None::<&gst::Pad>);
                }
                if let Some(source) = value
                    .get::<Option<gst::Element>>()
                    .expect("type checked upstream")
                {
                    if self.obj().add(&source).is_err() {
                        gst::warning!(CAT, imp = self, "Failed to add source");
                        return;
                    };
                    if source.set_clock(Some(&self.internal_clock)).is_err() {
                        gst::warning!(CAT, imp = self, "Failed to set clock on source");
                        return;
                    };

                    let Some(target_pad) = source.static_pad("src") else {
                        gst::warning!(CAT, imp = self, "Source element has no 'src' pad");
                        return;
                    };
                    if self.srcpad.set_target(Some(&target_pad)).is_err() {
                        gst::warning!(CAT, imp = self, "Failed to set ghost pad target");
                        return;
                    }
                    state.source = Some(source);
                } else {
                    state.source = None;
                }
            }
            "window-size" => {
                let state = self.state.lock().unwrap();
                state.external_clock.set_window_size(value.get().unwrap());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "source" => self.state.lock().unwrap().source.to_value(),
            "window-size" => self
                .state
                .lock()
                .unwrap()
                .external_clock
                .window_size()
                .to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.set_element_flags(
            gst::ElementFlags::PROVIDE_CLOCK
                | gst::ElementFlags::REQUIRE_CLOCK
                | gst::ElementFlags::SOURCE,
        );
        obj.set_suppressed_flags(
            gst::ElementFlags::SOURCE
                | gst::ElementFlags::SINK
                | gst::ElementFlags::PROVIDE_CLOCK
                | gst::ElementFlags::REQUIRE_CLOCK,
        );
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for MpegTsLiveSource {}

impl ElementImpl for MpegTsLiveSource {
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused
            && self
                .state
                .lock()
                .expect("Couldn't get state")
                .source
                .is_none()
        {
            gst::error!(CAT, "No source to control");
            return Err(gst::StateChangeError);
        }
        let ret = self.parent_change_state(transition)?;
        if transition == gst::StateChange::ReadyToPaused
            && ret != gst::StateChangeSuccess::NoPreroll
        {
            gst::error!(CAT, "We can only control live sources");
            return Err(gst::StateChangeError);
        } else if transition == gst::StateChange::PausedToReady {
            let mut state = self.state.lock().expect("Could get state");
            state.external_clock.set_calibration(
                gst::ClockTime::from_nseconds(0),
                gst::ClockTime::from_nseconds(0),
                1,
                1,
            );
            // Hack to flush out observations, we set the window-size to the
            // same value
            state
                .external_clock
                .set_window_size(state.external_clock.window_size());
            state.last_seen_pcr = None;
            state.base_monotonic = None;
            state.base_pcr = None;
        }
        Ok(ret)
    }

    fn set_clock(&self, clock: Option<&gst::Clock>) -> bool {
        // We only accept our clock
        if let Some(proposed) = clock {
            if *proposed
                != self
                    .state
                    .lock()
                    .expect("Couldn't get state")
                    .external_clock
            {
                return false;
            }
        }
        true
    }

    fn provide_clock(&self) -> Option<gst::Clock> {
        let state = self.state.lock().expect("Couldn't get state");
        Some(state.external_clock.clone().upcast())
    }

    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "MpegTsLiveSource",
                "Network",
                "Wrap MPEG-TS sources and provide a live clock",
                "Edward Hervey <edward@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BinImpl for MpegTsLiveSource {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcr_basic_test() {
        // Smallest value
        let pcr = MpegTsPcr::new(0);
        assert_eq!(pcr.value, 0);
        assert_eq!(pcr.wraparound, 0);

        // Biggest (non-wrapped) value
        let mut pcr = MpegTsPcr::new(MpegTsPcr::MAX);
        assert_eq!(pcr.value, MpegTsPcr::MAX);
        assert_eq!(pcr.wraparound, 0);

        // a 33bit value overflows into 0
        pcr = MpegTsPcr::new((1u64 << 33) * 300);
        assert_eq!(pcr.value, 0);
        assert_eq!(pcr.wraparound, 1);

        // Adding one to biggest value overflows
        pcr = MpegTsPcr::new(MpegTsPcr::MAX + 1);
        assert_eq!(pcr.value, 0);
        assert_eq!(pcr.wraparound, 1);
    }

    #[test]
    fn pcr_wraparound_test() {
        gst::init().unwrap();
        crate::plugin_register_static().expect("mpegtslivesrc test");

        let element = gst::ElementFactory::make("mpegtslivesrc")
            .build()
            .unwrap()
            .downcast::<super::super::MpegTsLiveSource>()
            .unwrap();
        let imp = element.imp();

        // Basic test going forward within 15s
        let ref_pcr = MpegTsPcr {
            value: 360 * MpegTsPcr::RATE,
            wraparound: 100,
        };
        let pcr = MpegTsPcr::new_with_reference(imp, 370 * MpegTsPcr::RATE, &ref_pcr);
        assert!(pcr.is_some());
        if let Some(pcr) = pcr {
            assert_eq!(pcr.value, 370 * MpegTsPcr::RATE);
            assert_eq!(pcr.wraparound, ref_pcr.wraparound);
        };

        // Discont
        let pcr = MpegTsPcr::new_with_reference(imp, 344 * MpegTsPcr::RATE, &ref_pcr);
        assert!(pcr.is_none());

        let pcr = MpegTsPcr::new_with_reference(imp, 386 * MpegTsPcr::RATE, &ref_pcr);
        assert!(pcr.is_none());

        // Wraparound, ref is 10s before MAX
        let ref_pcr = MpegTsPcr {
            value: MpegTsPcr::MAX - 10 * MpegTsPcr::RATE,
            wraparound: 600,
        };
        let pcr = MpegTsPcr::new_with_reference(imp, 0, &ref_pcr);
        assert!(pcr.is_some());
        if let Some(pcr) = pcr {
            assert_eq!(pcr.value, 0);
            assert_eq!(pcr.wraparound, ref_pcr.wraparound + 1);
        };

        // Discont
        let pcr = MpegTsPcr::new_with_reference(imp, 10 * MpegTsPcr::RATE, &ref_pcr);
        assert!(pcr.is_none());

        // reference is 5s after wraparound
        let ref_pcr = MpegTsPcr {
            value: 5 * MpegTsPcr::RATE,
            wraparound: 600,
        };
        // value is 5s before wraparound
        let pcr =
            MpegTsPcr::new_with_reference(imp, MpegTsPcr::MAX + 1 - 5 * MpegTsPcr::RATE, &ref_pcr);
        assert!(pcr.is_some());
        if let Some(pcr) = pcr {
            assert_eq!(pcr.value, MpegTsPcr::MAX + 1 - 5 * MpegTsPcr::RATE);
            assert_eq!(pcr.wraparound, ref_pcr.wraparound - 1);
        }
    }
}
