// GStreamer RTP Raw Video Depayloader
//
// Copyright (C) 2026 François Laignel <francois centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//! Line numbering identification
//!
//! [RFC 4175] § 3 mentions specific line numbering schemes for the payloading
//! of some video formats such as SMPTE 274M (1920x1080p) & 296M (1280x720p).
//! Because these formats can include ancillary data at lines corresponding to
//! the vertical blanking, the numbers for the active lines don't start from 0.
//!
//! Other progressive formats which are known to possibly include ancillary data
//! at lines corresponding to vertical blanking are also supported:
//!
//! * DCI 2K:  2048x1080p
//! * UHD1 4K: 3840x2160p
//! * DCI 4K:  4096x2160p
//! * UHD2 8K: 7680x4320p
//! * DCI 8K:  8192x4320p
//!
//! In DEF STAN 00-082 part 1, § B.1 specifies that line numbering for VESA display monitor
//! standards must start from 1. § B.1.1 gives the example of a VGA resolution (640x480)
//! for which scan lines shall be numbered from 1 to 480.
//!
//! These numbering schemes are not part of the SDP attributes, so they have to be agreed
//! upon or inferred.
//!
//! This module defines structures & strategies for the receiver of raw video packets
//! to translate incoming line numbers to the 0-based scheme used by GStreamer.
//!
//! [rfc-4175]: https://www.rfc-editor.org/rfc/rfc4175.html

// FIXME fengalin how about ROI?
// TODO handle interlace

use std::sync::LazyLock;

use crate::raw_video::pixel_group::PixelGroup;

/// SMPTE 274M / HD 1080p: 1920x1080p (inclusive ranges)
/// 1125 total lines, active range: [42, 1121]
/// V sync [1, 5], ancillary [7, 41], V sync [1122, 1125]
pub const HD_1080P_ACTIVE_WIDTH: u32 = 1920;
pub const HD_1080P_ACTIVE_HEIGHT: u32 = 1080;
pub const HD_1080P_FIRST_ACTIVE: u32 = 42;
pub const HD_1080P_LAST_ACTIVE: u32 = 1121;

/// SMPTE 296M / HD 720p: 1280x720p (inclusive ranges)
/// 750 total lines, active range: [26, 745]
/// V sync [1, 5], ancillary [7, 25], V sync [746, 750]
pub const HD_720P_ACTIVE_WIDTH: u32 = 1280;
pub const HD_720P_ACTIVE_HEIGHT: u32 = 720;
pub const HD_720P_FIRST_ACTIVE: u32 = 26;
pub const HD_720P_LAST_ACTIVE: u32 = 745;

/// DCI 2K: 2048x1080p (inclusive ranges)
/// XXX total lines, active range: [42, 1121]
/// V sync [1, 5], ancillary [7, 41], V sync [1122, XXX]
pub const DCI_2K_ACTIVE_WIDTH: u32 = 2048;
pub const DCI_2K_ACTIVE_HEIGHT: u32 = 1080;
pub const DCI_2K_FIRST_ACTIVE: u32 = 42;
pub const DCI_2K_LAST_ACTIVE: u32 = 1121;

/// UHD1_4K: 3840x2160p (inclusive ranges)
/// XXX total lines, active range: [42, 2201]
/// V sync [1, 5], ancillary [7, 41], V sync [2202, XXX]
pub const UHD1_4K_ACTIVE_WIDTH: u32 = 3840;
pub const UHD1_4K_ACTIVE_HEIGHT: u32 = 2160;
pub const UHD1_4K_FIRST_ACTIVE: u32 = 42;
pub const UHD1_4K_LAST_ACTIVE: u32 = 2201;

/// DCI_4K: 4096x2160p (inclusive ranges)
/// XXX total lines, active range: [42, 2201]
/// V sync [1, 5], ancillary [7, 41], V sync [2202, XXX]
pub const DCI_4K_ACTIVE_WIDTH: u32 = 4096;
pub const DCI_4K_ACTIVE_HEIGHT: u32 = 2160;
pub const DCI_4K_FIRST_ACTIVE: u32 = 42;
pub const DCI_4K_LAST_ACTIVE: u32 = 2201;

/// UHD2_8K: 7680x4320p (inclusive ranges)
/// XXX total lines, active range: [42, 4361]
/// V sync [1, 5], ancillary [7, 41], V sync [4362, XXX]
pub const UHD2_8K_ACTIVE_WIDTH: u32 = 7680;
pub const UHD2_8K_ACTIVE_HEIGHT: u32 = 4320;
pub const UHD2_8K_FIRST_ACTIVE: u32 = 42;
pub const UHD2_8K_LAST_ACTIVE: u32 = 4361;

/// DCI_8K: 8192x4320p (inclusive ranges)
/// XXX total lines, active range: [42, 4361]
/// V sync [1, 5], ancillary [7, 41], V sync [4362, XXX]
pub const DCI_8K_ACTIVE_WIDTH: u32 = 8192;
pub const DCI_8K_ACTIVE_HEIGHT: u32 = 4320;
pub const DCI_8K_FIRST_ACTIVE: u32 = 42;
pub const DCI_8K_LAST_ACTIVE: u32 = 4361;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpvrawlinenb",
        gst::DebugColorFlags::empty(),
        Some("RTP Raw Video Line Numbering"),
    )
});

#[derive(Copy, Clone, Debug, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstRtpRawVideoLineNumberingIdentificationMethod")]
#[repr(i32)]
pub enum LineNumberingIdentificationMethod {
    /// Keep line number from RTP payload header unchanged
    #[enum_value(name = "Keep line number unchanged", nick = "passthrough")]
    Passthrough,

    /// Infer the scan line numbering scheme and translate it to what GStreamer expects.
    ///
    /// If the first line number of a frame is not 0, then the scan line numbering
    /// scheme is inferred on a best effort basis.
    #[default]
    #[enum_value(name = "Infer scan line numbering scheme", nick = "infer")]
    Infer,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstRtpRawVideoLineNumberingScheme")]
#[repr(i32)]
pub enum LineNumberingScheme {
    /// Use GStreamer 0-based line numbering in the RTP payload header
    #[default]
    #[enum_value(name = "0-based line numbering", nick = "passthrough")]
    Passthrough,

    /// Use VESA 1-based line numbering in the RTP payload header
    #[enum_value(name = "1-based line numbering", nick = "vesa")]
    Vesa,

    /// Use SMPTE ancillary-tolerant line numbering in the RTP payload header
    ///
    /// Use that scheme if the format resolution matches one of those known to use
    /// ancillary-tolerant numbering scheme; if not, error-out.
    #[enum_value(
        name = "Use SMPTE ancillary-tolerant line numbering (requires eligible resolution)",
        nick = "smpte"
    )]
    Smpte,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LineNumberingSchemeError {
    #[error("SMPTE line numbering scheme: unsupported resolution {}x{}", .width, .height)]
    SmpteSchemeUnsupportedResolution { width: u32, height: u32 },
}

impl LineNumberingScheme {
    pub fn to_line_nb_offset(
        self,
        vinfo: &gst_video::VideoInfo,
    ) -> Result<u16, LineNumberingSchemeError> {
        use LineNumberingScheme::*;
        let offset = match self {
            Passthrough => 0,
            Vesa => 1,
            Smpte => match (vinfo.width(), vinfo.height()) {
                (HD_1080P_ACTIVE_WIDTH, HD_1080P_ACTIVE_HEIGHT) => HD_1080P_FIRST_ACTIVE as u16,
                (HD_720P_ACTIVE_WIDTH, HD_720P_ACTIVE_HEIGHT) => HD_720P_FIRST_ACTIVE as u16,
                (DCI_2K_ACTIVE_WIDTH, DCI_2K_ACTIVE_HEIGHT) => DCI_2K_FIRST_ACTIVE as u16,
                (UHD1_4K_ACTIVE_WIDTH, UHD1_4K_ACTIVE_HEIGHT) => UHD1_4K_FIRST_ACTIVE as u16,
                (DCI_4K_ACTIVE_WIDTH, DCI_4K_ACTIVE_HEIGHT) => DCI_4K_FIRST_ACTIVE as u16,
                (UHD2_8K_ACTIVE_WIDTH, UHD2_8K_ACTIVE_HEIGHT) => UHD2_8K_FIRST_ACTIVE as u16,
                (DCI_8K_ACTIVE_WIDTH, DCI_8K_ACTIVE_HEIGHT) => DCI_8K_FIRST_ACTIVE as u16,
                (width, height) => {
                    return Err(LineNumberingSchemeError::SmpteSchemeUnsupportedResolution {
                        width,
                        height,
                    });
                }
            },
        };

        gst::debug!(
            CAT,
            "selected line number offset {offset} for {}x{}",
            vinfo.width(),
            vinfo.height(),
        );

        Ok(offset)
    }
}

/// State of the [`LineNumberingSchemeIdentifier`]
///
/// The [`LineNumberingSchemeIdentifier`] transitions from the `Unidentified`
/// state to `Identified` if:
///
/// * A line with number 0 is received. This can only be sent by a 0-based
///   line numbering scheme.
/// * A line is flagged as the last line of the frame and:
///   * the line matches the frame's height - 1
///     => 0-based line numbering scheme
///   * the line matches the frame's height
///     => 1-based line numbering scheme
///   * the frame resolution matches an SMPTE resolution for which a payload
///     can contain ancillary data & the line matches the last active line
///     for this SMPTE resolution
///     => ancillary-tolerant line numbering scheme, where the offset is
///     the number of the first active line for this SMPTE resolution
///
/// In other words, for non-0-based line numbering scheme, the last frame line
/// is required to transition to `Identified`. Until such a line is received,
/// frames are offset and lines which could match ancillary data are filtered out.
///
/// See also [`LineNumberingSchemeIdentifier::convert_to_gst_line_nb`].
#[derive(Debug, Default)]
enum State {
    /// Line numbering scheme has been identified
    ///
    /// In this state, the line numbers are offset by `line_offset`, which should
    /// render them as expected with regard to GStreamer's 0-based line numbering scheme.
    ///
    /// Out of range lines are filtered out. This includes ancillary lines.
    Identified { line_offset: u32 },

    /// Line numbering scheme hasn't been identified yet
    ///
    /// In this state, line numbers are kept unchanged. Depending on the line
    /// numbering scheme, the image might be offset.
    ///
    /// Additionally:
    ///
    /// * Out of range lines are filtered out.
    /// * When the frame resolution matches an SMPTE resolution for which a payload
    ///   can contain ancillary data:
    ///   * lines which might contain ancillary data are filtered out.
    ///   * active lines which would be out of frame due to the offset are filtered out.
    #[default]
    Unidentified,
}

/// A line numbering scheme identifier
#[derive(Debug, Default)]
pub struct LineNumberingSchemeIdentifier {
    method: LineNumberingIdentificationMethod,
    state: State,
    width: u32,
    height: u32,
    y_inc: u32,
}

impl LineNumberingSchemeIdentifier {
    pub fn set_method(&mut self, method: LineNumberingIdentificationMethod) {
        gst::debug!(CAT, "using {method:?}");
        self.method = method;
        self.reset();
    }

    pub fn set_video_info(&mut self, vinfo: &gst_video::VideoInfo) {
        let pgroup = PixelGroup::from_video_info(vinfo).expect("supported format");
        self.width = vinfo.width();
        self.height = vinfo.height();
        self.y_inc = pgroup.y_inc() as u32;
        gst::debug!(
            CAT,
            "video resolution: {}x{}, y inc: {}",
            self.width,
            self.height,
            self.y_inc,
        );
        self.reset();
    }

    pub fn reset(&mut self) {
        gst::trace!(CAT, "reset");
        self.state = State::default();
    }

    /// Converts the RTP payload header line number to GStreamer compliant 0-based
    /// line numbering scheme, improving our knowledge of the input scheme on the way.
    ///
    /// See also [`State`].
    ///
    /// # Arguments
    ///
    /// * `line_nb`: the line number from the RTP payload header.
    /// * `is_last`: must be `true` when the this `line_nb` corresponds to the
    ///   last line of a packet with the Marker (M) bit set.
    ///
    /// # Return
    ///
    /// * `Some(gst_line_nb)`: if the line can be pushed to a GStreamer video frame.
    /// * `None`: if the line would be out of range or could contain ancillary data.
    pub fn convert_to_gst_line_nb(&mut self, line_nb: u32, is_last: bool) -> Option<u32> {
        if matches!(self.method, LineNumberingIdentificationMethod::Passthrough) {
            return Some(line_nb);
        }

        use State::*;
        match self.state {
            Identified { line_offset } => {
                if let Some(gst_line_nb) = line_nb.checked_sub(line_offset)
                    && gst_line_nb < self.height
                {
                    if is_last && self.height != gst_line_nb + self.y_inc {
                        gst::warning!(
                            CAT,
                            "identified offset {line_offset}: incoming last line {line_nb} leads to \
                             unexpected 0-based line number {gst_line_nb}, {}x{}, y inc: {}",
                            self.width,
                            self.height,
                            self.y_inc,
                        );
                    }

                    return Some(gst_line_nb);
                }

                // line nb out of range
                match (self.width, self.height) {
                    (HD_1080P_ACTIVE_WIDTH, HD_1080P_ACTIVE_HEIGHT)
                        if line_offset == HD_1080P_FIRST_ACTIVE =>
                    {
                        self.log_vb_line_identified::<HD_1080P_LAST_ACTIVE>(line_offset, line_nb);
                    }
                    (HD_720P_ACTIVE_WIDTH, HD_720P_ACTIVE_HEIGHT)
                        if line_offset == HD_720P_FIRST_ACTIVE =>
                    {
                        self.log_vb_line_identified::<HD_720P_LAST_ACTIVE>(line_offset, line_nb);
                    }
                    (DCI_2K_ACTIVE_WIDTH, DCI_2K_ACTIVE_HEIGHT)
                        if line_offset == DCI_2K_FIRST_ACTIVE =>
                    {
                        self.log_vb_line_identified::<DCI_2K_LAST_ACTIVE>(line_offset, line_nb);
                    }
                    (UHD1_4K_ACTIVE_WIDTH, UHD1_4K_ACTIVE_HEIGHT)
                        if line_offset == UHD1_4K_FIRST_ACTIVE =>
                    {
                        self.log_vb_line_identified::<UHD1_4K_LAST_ACTIVE>(line_offset, line_nb);
                    }
                    (DCI_4K_ACTIVE_WIDTH, DCI_4K_ACTIVE_HEIGHT)
                        if line_offset == DCI_4K_FIRST_ACTIVE =>
                    {
                        self.log_vb_line_identified::<DCI_4K_LAST_ACTIVE>(line_offset, line_nb);
                    }
                    (UHD2_8K_ACTIVE_WIDTH, UHD2_8K_ACTIVE_HEIGHT)
                        if line_offset == UHD2_8K_FIRST_ACTIVE =>
                    {
                        self.log_vb_line_identified::<UHD2_8K_LAST_ACTIVE>(line_offset, line_nb);
                    }
                    (DCI_8K_ACTIVE_WIDTH, DCI_8K_ACTIVE_HEIGHT)
                        if line_offset == DCI_8K_FIRST_ACTIVE =>
                    {
                        self.log_vb_line_identified::<DCI_8K_LAST_ACTIVE>(line_offset, line_nb);
                    }
                    (width, height) => {
                        gst::warning!(
                            CAT,
                            "identified offset {line_offset}: line nb {line_nb} out of range, {width}x{height}, y inc {}",
                            self.y_inc,
                        );
                    }
                }

                None
            }
            Unidentified => {
                if line_nb == 0 {
                    gst::debug!(
                        CAT,
                        "unidentified: got line number 0, {}x{}, y inc: {}",
                        self.width,
                        self.height,
                        self.y_inc,
                    );
                    self.state = Identified { line_offset: 0 };
                    return Some(line_nb);
                }

                if is_last {
                    if self.height == line_nb + self.y_inc {
                        gst::debug!(
                            CAT,
                            "unidentified: got last line {line_nb} of 0-based numbering, {}x{}, y inc: {}",
                            self.width,
                            self.height,
                            self.y_inc,
                        );
                        self.state = Identified { line_offset: 0 };
                        return Some(line_nb);
                    }
                    if self.height + 1 == line_nb + self.y_inc {
                        gst::debug!(
                            CAT,
                            "unidentified: got last line {line_nb} of VESA 1-based numbering, {}x{}, y inc: {}",
                            self.width,
                            self.height,
                            self.y_inc,
                        );
                        self.state = Identified { line_offset: 1 };
                        // don't display for now as it is out of range for current frame
                        return None;
                    }
                }

                match (self.width, self.height) {
                    (HD_1080P_ACTIVE_WIDTH, HD_1080P_ACTIVE_HEIGHT) => {
                        return self.maybe_anc_tolerant_to_gst_unidentified::<
                            HD_1080P_ACTIVE_HEIGHT,
                            HD_1080P_FIRST_ACTIVE,
                            HD_1080P_LAST_ACTIVE,
                        >(is_last, line_nb);
                    }
                    (HD_720P_ACTIVE_WIDTH, HD_720P_ACTIVE_HEIGHT) => {
                        return self.maybe_anc_tolerant_to_gst_unidentified::<
                            HD_720P_ACTIVE_HEIGHT,
                            HD_720P_FIRST_ACTIVE,
                            HD_720P_LAST_ACTIVE,
                        >(is_last, line_nb);
                    }
                    (DCI_2K_ACTIVE_WIDTH, DCI_2K_ACTIVE_HEIGHT) => {
                        return self.maybe_anc_tolerant_to_gst_unidentified::<
                            DCI_2K_ACTIVE_HEIGHT,
                            DCI_2K_FIRST_ACTIVE,
                            DCI_2K_LAST_ACTIVE,
                        >(is_last, line_nb);
                    }
                    (UHD1_4K_ACTIVE_WIDTH, UHD1_4K_ACTIVE_HEIGHT) => {
                        return self.maybe_anc_tolerant_to_gst_unidentified::<
                            UHD1_4K_ACTIVE_HEIGHT,
                            UHD1_4K_FIRST_ACTIVE,
                            UHD1_4K_LAST_ACTIVE,
                        >(is_last, line_nb);
                    }
                    (DCI_4K_ACTIVE_WIDTH, DCI_4K_ACTIVE_HEIGHT) => {
                        return self.maybe_anc_tolerant_to_gst_unidentified::<
                            DCI_4K_ACTIVE_HEIGHT,
                            DCI_4K_FIRST_ACTIVE,
                            DCI_4K_LAST_ACTIVE,
                        >(is_last, line_nb);
                    }
                    (UHD2_8K_ACTIVE_WIDTH, UHD2_8K_ACTIVE_HEIGHT) => {
                        return self.maybe_anc_tolerant_to_gst_unidentified::<
                            UHD2_8K_ACTIVE_HEIGHT,
                            UHD2_8K_FIRST_ACTIVE,
                            UHD2_8K_LAST_ACTIVE,
                        >(is_last, line_nb);
                    }
                    (DCI_8K_ACTIVE_WIDTH, DCI_8K_ACTIVE_HEIGHT) => {
                        return self.maybe_anc_tolerant_to_gst_unidentified::<
                            DCI_8K_ACTIVE_HEIGHT,
                            DCI_8K_FIRST_ACTIVE,
                            DCI_8K_LAST_ACTIVE,
                        >(is_last, line_nb);
                    }
                    (_, height) if height <= line_nb => {
                        // out of range considering current knowledge
                        return None;
                    }
                    // else, need more clues, display at current line number for now
                    // this is ok because this is NOT:
                    // * out of range for current frame
                    // * a line which contains ancillary data
                    _ => (),
                }

                Some(line_nb)
            }
        }
    }

    fn log_vb_line_identified<const LAST_ACTIVE: u32>(&mut self, line_offset: u32, line_nb: u32) {
        if line_nb == 0 || line_nb + self.y_inc >= LAST_ACTIVE {
            // unexpected
            // we might as well warn for line nb out of the ANC & active range
            gst::warning!(
                CAT,
                "identified offset {line_offset}: line nb {line_nb} out of range, {}x{}, y inc {}",
                self.width,
                self.height,
                self.y_inc,
            );
        } else {
            gst::log!(
                CAT,
                "identified offset {line_offset}: skipping vertical blanking at line nb {line_nb}, {}x{}, y inc {}",
                self.width,
                self.height,
                self.y_inc,
            );
        }
    }

    fn maybe_anc_tolerant_to_gst_unidentified<
        const ACTIVE_HEIGHT: u32,
        const FIRST_ACTIVE: u32,
        const LAST_ACTIVE: u32,
    >(
        &mut self,
        is_last: bool,
        line_nb: u32,
    ) -> Option<u32> {
        // Note: general use cases already handled by caller

        if is_last && LAST_ACTIVE + 1 == line_nb + self.y_inc {
            gst::debug!(
                CAT,
                "unidentified: ancillary-tolerant numbering identified, offset {FIRST_ACTIVE}, {}x{}, y inc {}",
                self.width,
                self.height,
                self.y_inc,
            );
            self.state = State::Identified {
                line_offset: FIRST_ACTIVE,
            };
            // don't display this line as it is out of range for current frame
            return None;
        }

        if line_nb < FIRST_ACTIVE {
            // possibly ancillary-tolerant vertical blanking
            // don't display until we know better
            return None;
        }
        if line_nb >= FIRST_ACTIVE && line_nb + self.y_inc <= ACTIVE_HEIGHT {
            // can't differentiate ancillary-tolerant, 0-based or 1-based
            // display at current line number for now
            return Some(line_nb);
        }
        // else out of range: can't guess at this point
        // don't display this line
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD_1080P_FIRST_ANC: u32 = 7;
    const HD_720P_FIRST_ANC: u32 = 7;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
            crate::plugin_register_static().expect("rtpvraw test");
        });
    }

    #[test]
    fn no_initial_packet_loss() {
        no_initial_packet_loss_for_format(gst_video::VideoFormat::Rgb);
        no_initial_packet_loss_for_format(gst_video::VideoFormat::I420);
    }

    fn no_initial_packet_loss_for_format(format: gst_video::VideoFormat) {
        const WIDTH: u32 = 320;
        const HEIGHT: u32 = 240;

        init();

        let vinfo = gst_video::VideoInfo::builder(format, WIDTH, HEIGHT)
            .build()
            .unwrap();
        let y_inc = PixelGroup::from_video_info(&vinfo).unwrap().y_inc() as u32;

        let mut ider = LineNumberingSchemeIdentifier::default();
        ider.set_method(LineNumberingIdentificationMethod::Infer);
        ider.set_video_info(&vinfo);

        // 0-based
        // => immediately identifiable
        assert_eq!(0, ider.convert_to_gst_line_nb(0, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, true).unwrap()
        );
        // next frame keeps the same scheme
        assert_eq!(0, ider.convert_to_gst_line_nb(0, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, true).unwrap()
        );
        // unexpected last line => only causes a warning at this point
        assert_eq!(
            HEIGHT - y_inc - 1,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc - 1, true)
                .unwrap()
        );

        ider.reset();

        // 1-based
        // => confirmation on last line
        assert_eq!(1, ider.convert_to_gst_line_nb(1, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, false).unwrap()
        );
        // confirmation, but out of range with current offset
        assert!(
            ider.convert_to_gst_line_nb(HEIGHT + 1 - y_inc, true)
                .is_none()
        );
        // next frame benefits from identification
        assert_eq!(0, ider.convert_to_gst_line_nb(1, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc - 1,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, false).unwrap()
        );
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc + 1, true)
                .unwrap()
        );
        // next frame keeps the same scheme
        assert_eq!(0, ider.convert_to_gst_line_nb(1, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc + 1, true)
                .unwrap()
        );

        no_packet_loss_maybe_ancillary_tolerant::<
            HD_1080P_ACTIVE_WIDTH,
            HD_1080P_ACTIVE_HEIGHT,
            HD_1080P_FIRST_ANC,
            HD_1080P_FIRST_ACTIVE,
            HD_1080P_LAST_ACTIVE,
        >(&mut ider, format);

        no_packet_loss_maybe_ancillary_tolerant::<
            HD_720P_ACTIVE_WIDTH,
            HD_720P_ACTIVE_HEIGHT,
            HD_720P_FIRST_ANC,
            HD_720P_FIRST_ACTIVE,
            HD_720P_LAST_ACTIVE,
        >(&mut ider, format);

        fn no_packet_loss_maybe_ancillary_tolerant<
            const ACTIVE_WIDTH: u32,
            const ACTIVE_HEIGHT: u32,
            const FIRST_ANC: u32,
            const FIRST_ACTIVE: u32,
            const LAST_ACTIVE: u32,
        >(
            ider: &mut LineNumberingSchemeIdentifier,
            format: gst_video::VideoFormat,
        ) {
            let vinfo = gst_video::VideoInfo::builder(format, ACTIVE_WIDTH, ACTIVE_HEIGHT)
                .build()
                .unwrap();
            let y_inc = PixelGroup::from_video_info(&vinfo).unwrap().y_inc() as u32;

            ider.set_video_info(&vinfo);

            // 0-based
            // => immediately identifiable
            assert_eq!(0, ider.convert_to_gst_line_nb(0, false).unwrap());
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, true)
                    .unwrap()
            );
            // next frame keeps the same scheme
            assert_eq!(0, ider.convert_to_gst_line_nb(0, false).unwrap());
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, true)
                    .unwrap()
            );

            ider.reset();

            // 1-based
            // => only potential active lines can be displayed initially
            // => identification on last line
            assert!(ider.convert_to_gst_line_nb(1, false).is_none());
            assert!(ider.convert_to_gst_line_nb(FIRST_ANC, false).is_none());
            assert_eq!(
                None,
                ider.convert_to_gst_line_nb(FIRST_ACTIVE - y_inc, false)
            );
            // potential SMPTE-xxx active lines displayed at their line nb
            assert_eq!(
                FIRST_ACTIVE,
                ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap()
            );
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, false)
                    .unwrap()
            );
            // confirmation, but out of range with current offset
            assert_eq!(
                None,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc + 1, true)
            );
            // next frame benefits from identification
            assert_eq!(0, ider.convert_to_gst_line_nb(1, false).unwrap());
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc + 1, true)
                    .unwrap()
            );
            // next frame keeps the same scheme
            assert_eq!(0, ider.convert_to_gst_line_nb(1, false).unwrap());
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc + 1, true)
                    .unwrap()
            );

            ider.reset();

            // SMPTE xxx
            // => only potential active lines can be displayed initially
            // => identification on last line
            assert_eq!(None, ider.convert_to_gst_line_nb(FIRST_ANC, false));
            assert_eq!(
                None,
                ider.convert_to_gst_line_nb(FIRST_ACTIVE - y_inc, false)
            );
            // potential SMPTE-xxx active lines displayed at their line nb
            assert_eq!(
                FIRST_ACTIVE,
                ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap()
            );
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, false)
                    .unwrap()
            );
            // valid but can't be displayed considering current knowledge & frame
            assert_eq!(None, ider.convert_to_gst_line_nb(ACTIVE_HEIGHT, false));
            // confirmation, but out of range with current offset
            assert_eq!(
                None,
                ider.convert_to_gst_line_nb(LAST_ACTIVE - y_inc + 1, true)
            );
            // next frame benefits from identification
            // but ancillary lines are not displayed
            assert!(ider.convert_to_gst_line_nb(FIRST_ANC, false).is_none());
            assert_eq!(None, ider.convert_to_gst_line_nb(FIRST_ACTIVE - 1, false));
            // first active line gets number 0
            assert_eq!(0, ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap());
            // last active line
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(LAST_ACTIVE - y_inc + 1, true)
                    .unwrap()
            );
            // next frame keeps the same scheme
            assert!(ider.convert_to_gst_line_nb(FIRST_ANC, false).is_none());
            assert_eq!(0, ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap());
            // out of range => not returned & only causes a warning at this point
            assert_eq!(None, ider.convert_to_gst_line_nb(LAST_ACTIVE + 1, false));
        }
    }

    #[test]
    fn initial_packet_loss() {
        initial_packet_loss_for_format(gst_video::VideoFormat::Rgb);
        initial_packet_loss_for_format(gst_video::VideoFormat::I420);
    }

    fn initial_packet_loss_for_format(format: gst_video::VideoFormat) {
        const WIDTH: u32 = 320;
        const HEIGHT: u32 = 240;

        init();

        let vinfo = gst_video::VideoInfo::builder(format, WIDTH, HEIGHT)
            .build()
            .unwrap();
        let y_inc = PixelGroup::from_video_info(&vinfo).unwrap().y_inc() as u32;

        let mut ider = LineNumberingSchemeIdentifier::default();
        ider.set_method(LineNumberingIdentificationMethod::Infer);
        ider.set_video_info(&vinfo);

        // 0-based
        // missing 1st line => not to be confused with 1-based
        assert_eq!(1, ider.convert_to_gst_line_nb(1, false).unwrap());
        // identification on last frame
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, true).unwrap()
        );
        // next frame gets the proper scheme
        assert_eq!(0, ider.convert_to_gst_line_nb(0, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, true).unwrap()
        );

        ider.reset();

        // 1-based
        // missing 1st line
        assert_eq!(2, ider.convert_to_gst_line_nb(2, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, false).unwrap()
        );
        // identification on last frame, but out of range with current offset
        assert_eq!(None, ider.convert_to_gst_line_nb(HEIGHT - y_inc + 1, true));
        // next frame benefits from identification
        assert_eq!(0, ider.convert_to_gst_line_nb(1, false).unwrap());
        assert_eq!(
            HEIGHT - y_inc - 1,
            ider.convert_to_gst_line_nb(HEIGHT - y_inc, false).unwrap()
        );

        no_packet_loss_maybe_ancillary_tolerant::<
            HD_1080P_ACTIVE_WIDTH,
            HD_1080P_ACTIVE_HEIGHT,
            HD_1080P_FIRST_ANC,
            HD_1080P_FIRST_ACTIVE,
            HD_1080P_LAST_ACTIVE,
        >(&mut ider, format);

        no_packet_loss_maybe_ancillary_tolerant::<
            HD_720P_ACTIVE_WIDTH,
            HD_720P_ACTIVE_HEIGHT,
            HD_720P_FIRST_ANC,
            HD_720P_FIRST_ACTIVE,
            HD_720P_LAST_ACTIVE,
        >(&mut ider, format);

        fn no_packet_loss_maybe_ancillary_tolerant<
            const ACTIVE_WIDTH: u32,
            const ACTIVE_HEIGHT: u32,
            const FIRST_ANC: u32,
            const FIRST_ACTIVE: u32,
            const LAST_ACTIVE: u32,
        >(
            ider: &mut LineNumberingSchemeIdentifier,
            format: gst_video::VideoFormat,
        ) {
            let vinfo = gst_video::VideoInfo::builder(format, ACTIVE_WIDTH, ACTIVE_HEIGHT)
                .build()
                .unwrap();
            let y_inc = PixelGroup::from_video_info(&vinfo).unwrap().y_inc() as u32;

            ider.set_video_info(&vinfo);

            // 0-based
            // missing 1st line
            // => only potential active lines can be displayed initially
            assert!(ider.convert_to_gst_line_nb(1, false).is_none());
            assert_eq!(
                FIRST_ACTIVE,
                ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap()
            );
            // identification on last frame
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, true)
                    .unwrap()
            );
            // next frame gets the proper scheme
            assert_eq!(0, ider.convert_to_gst_line_nb(0, false).unwrap());
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, true)
                    .unwrap()
            );

            ider.reset();

            // 1-based
            // missing 1st line
            // => only potential active lines can be displayed initially
            assert_eq!(None, ider.convert_to_gst_line_nb(2, false));
            // potential SMPTE-xxx active lines displayed at their line nb
            assert_eq!(
                FIRST_ACTIVE,
                ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap()
            );
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, false)
                    .unwrap()
            );
            // confirmation, but out of range with current offset
            assert_eq!(
                None,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc + 1, true)
            );
            // next frame benefits from identification
            assert_eq!(0, ider.convert_to_gst_line_nb(1, false).unwrap());
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc + 1, true)
                    .unwrap()
            );

            ider.reset();

            // SMPTE xxx
            // => only potential active lines can be displayed initially
            // missing 1st line
            assert_eq!(
                FIRST_ACTIVE + y_inc,
                ider.convert_to_gst_line_nb(FIRST_ACTIVE + y_inc, false)
                    .unwrap()
            );
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(ACTIVE_HEIGHT - y_inc, false)
                    .unwrap()
            );
            // confirmation, but out of range with current offset
            assert_eq!(
                None,
                ider.convert_to_gst_line_nb(LAST_ACTIVE - y_inc + 1, true)
            );
            // next frame benefits from identification
            // but ancillary lines are not displayed
            assert_eq!(None, ider.convert_to_gst_line_nb(FIRST_ANC, false));
            assert_eq!(None, ider.convert_to_gst_line_nb(FIRST_ACTIVE - 1, false));
            // first active line gets number 0
            assert_eq!(0, ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap());
            // last active line
            assert_eq!(
                ACTIVE_HEIGHT - y_inc,
                ider.convert_to_gst_line_nb(LAST_ACTIVE - y_inc + 1, true)
                    .unwrap()
            );
            // next frame keeps the same scheme
            assert!(ider.convert_to_gst_line_nb(FIRST_ANC, false).is_none());
            assert_eq!(0, ider.convert_to_gst_line_nb(FIRST_ACTIVE, false).unwrap());
        }
    }
}
