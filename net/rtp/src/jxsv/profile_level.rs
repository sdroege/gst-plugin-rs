// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! RFC 9134 / ISO/IEC 21122-2 profile, level and sublevel string identifiers.
//!
//! Valid string values match [AMWA NMOS flow_video_jxsv_register], for example.
//! `sdp::video_jxsv` in nmos-cpp `video_jxsv.h`.
//!
//! [AMWA NMOS flow_video_jxsv_register]: https://specs.amwa.tv/nmos-parameter-registers/branches/main/flow-attributes/flow_video_jxsv_register.html

const PROFILE_FIELD: &str = "profile";
const LEVEL_FIELD: &str = "level";
const SUBLEVEL_FIELD: &str = "sublevel";

macro_rules! define_profile_table {
    ($($profile:ident, $name:literal, $ppih:literal;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Profile {
            $($profile,)*
        }

        struct ProfileEntry {
            profile: Profile,
            name: &'static str,
            ppih: u16,
        }

        const PROFILES: &[ProfileEntry] = &[
            $(ProfileEntry {
                profile: Profile::$profile,
                name: $name,
                ppih: $ppih,
            },)*
        ];

        impl TryFrom<&str> for Profile {
            type Error = ();

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                PROFILES
                    .iter()
                    .find(|entry| entry.name == value)
                    .map(|entry| entry.profile)
                    .ok_or(())
            }
        }
    };
}

define_profile_table! {
    HighBayer, "HighBayer", 0xC340;
    MainBayer, "MainBayer", 0xB340;
    LightBayer, "LightBayer", 0x9300;
    High4444_12, "High4444.12", 0x4E40;
    Main4444_12, "Main4444.12", 0x3E40;
    High444_12, "High444.12", 0x4A40;
    Main444_12, "Main444.12", 0x3A40;
    Light444_12, "Light444.12", 0x1A00;
    Main422_10, "Main422.10", 0x3540;
    Light422_10, "Light422.10", 0x1500;
    LightSubline422_10, "Light-Subline422.10", 0x2500;
    Mls12, "MLS.12", 0x6EC0;
    High420_12, "High420.12", 0x4240;
    Main420_12, "Main420.12", 0x3240;
}

macro_rules! define_level_table {
    ($($level:ident, $name:literal, $plev_high:literal;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[allow(clippy::enum_variant_names)]
        enum Level {
            $($level,)*
        }

        struct LevelEntry {
            level: Level,
            name: &'static str,
            plev_high: u8,
        }

        const LEVELS: &[LevelEntry] = &[
            $(LevelEntry {
                level: Level::$level,
                name: $name,
                plev_high: $plev_high,
            },)*
        ];

        impl TryFrom<&str> for Level {
            type Error = ();

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                LEVELS
                    .iter()
                    .find(|entry| entry.name == value)
                    .map(|entry| entry.level)
                    .ok_or(())
            }
        }
    };
}

define_level_table! {
    Level1k1, "1k-1", 0x04;
    Level2k1, "2k-1", 0x10;
    Level4k1, "4k-1", 0x20;
    Level4k2, "4k-2", 0x24;
    Level4k3, "4k-3", 0x28;
    Level8k1, "8k-1", 0x30;
    Level8k2, "8k-2", 0x34;
    Level8k3, "8k-3", 0x38;
    Level10k1, "10k-1", 0x40;
    Bayer2k1, "Bayer2k-1", 0x04;
    Bayer4k1, "Bayer4k-1", 0x10;
    Bayer8k1, "Bayer8k-1", 0x20;
    Bayer8k2, "Bayer8k-2", 0x24;
    Bayer8k3, "Bayer8k-3", 0x28;
    Bayer16k1, "Bayer16k-1", 0x30;
    Bayer16k2, "Bayer16k-2", 0x34;
    Bayer16k3, "Bayer16k-3", 0x38;
    Bayer20k1, "Bayer20k-1", 0x40;
}

macro_rules! define_sublevel_table {
    ($($sublevel:ident, $name:literal, $plev_low:literal;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Sublevel {
            $($sublevel,)*
        }

        struct SublevelEntry {
            sublevel: Sublevel,
            name: &'static str,
            plev_low: u8,
        }

        const SUBLEVELS: &[SublevelEntry] = &[
            $(SublevelEntry {
                sublevel: Sublevel::$sublevel,
                name: $name,
                plev_low: $plev_low,
            },)*
        ];

        impl TryFrom<&str> for Sublevel {
            type Error = ();

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                SUBLEVELS
                    .iter()
                    .find(|entry| entry.name == value)
                    .map(|entry| entry.sublevel)
                    .ok_or(())
            }
        }
    };
}

define_sublevel_table! {
    Full, "Full", 0x80;
    Sublev12bpp, "Sublev12bpp", 0x10;
    Sublev9bpp, "Sublev9bpp", 0x0C;
    Sublev6bpp, "Sublev6bpp", 0x08;
    Sublev4bpp, "Sublev4bpp", 0x06;
    Sublev3bpp, "Sublev3bpp", 0x04;
    Sublev2bpp, "Sublev2bpp", 0x03;
}

#[derive(Clone, Debug, Default)]
pub struct ProfileLevel {
    profile: Option<String>,
    level: Option<String>,
    sublevel: Option<String>,
}

impl ProfileLevel {
    pub fn from_caps(s: &gst::StructureRef) -> Result<Self, String> {
        Self::new(
            s.get::<&str>(PROFILE_FIELD).ok(),
            s.get::<&str>(LEVEL_FIELD).ok(),
            s.get::<&str>(SUBLEVEL_FIELD).ok(),
        )
    }

    pub fn new(
        profile: Option<&str>,
        level: Option<&str>,
        sublevel: Option<&str>,
    ) -> Result<Self, String> {
        if let Some(profile) = profile
            && profile_to_ppih(profile).is_none()
        {
            return Err(format!("unknown JPEG XS profile '{profile}'"));
        }

        if let Some(level) = level
            && level_to_plev_high(level).is_none()
        {
            return Err(format!("unknown JPEG XS level '{level}'"));
        }

        if let Some(sublevel) = sublevel
            && sublevel_to_plev_low(sublevel).is_none()
        {
            return Err(format!("unknown JPEG XS sublevel '{sublevel}'"));
        }

        Ok(Self {
            profile: profile.map(str::to_owned),
            level: level.map(str::to_owned),
            sublevel: sublevel.map(str::to_owned),
        })
    }

    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    pub fn level(&self) -> Option<&str> {
        self.level.as_deref()
    }

    pub fn sublevel(&self) -> Option<&str> {
        self.sublevel.as_deref()
    }

    pub fn add_to_caps(
        &self,
        mut builder: gst::caps::Builder<gst::caps::NoFeature>,
    ) -> gst::caps::Builder<gst::caps::NoFeature> {
        if let Some(profile) = self.profile() {
            builder = builder.field(PROFILE_FIELD, profile);
        }
        if let Some(level) = self.level() {
            builder = builder.field(LEVEL_FIELD, level);
        }
        if let Some(sublevel) = self.sublevel() {
            builder = builder.field(SUBLEVEL_FIELD, sublevel);
        }

        builder
    }

    pub fn resolve_ppih_plev(
        &self,
        codestream_ppih: u16,
        codestream_plev: u16,
    ) -> Result<(u16, u16, Vec<String>), String> {
        resolve_ppih_plev(
            codestream_ppih,
            codestream_plev,
            self.profile(),
            self.level(),
            self.sublevel(),
        )
    }
}

/// Map an RFC 9134 profile name to the PIH `Ppih` field (ISO/IEC 21122-2 Table A.5).
pub fn profile_to_ppih(profile: &str) -> Option<u16> {
    PROFILES
        .iter()
        .find(|entry| entry.name == profile)
        .map(|entry| entry.ppih)
}

/// High byte of the PIH `Plev` field for an RFC 9134 level (ISO/IEC 21122-2 Table A.12).
pub fn level_to_plev_high(level: &str) -> Option<u8> {
    LEVELS
        .iter()
        .find(|entry| entry.name == level)
        .map(|entry| entry.plev_high)
}

/// Low byte of the PIH `Plev` field for an RFC 9134 sublevel (ISO/IEC 21122-2 Table A.13).
pub fn sublevel_to_plev_low(sublevel: &str) -> Option<u8> {
    SUBLEVELS
        .iter()
        .find(|entry| entry.name == sublevel)
        .map(|entry| entry.plev_low)
}

/// Combine RFC 9134 level and sublevel strings into a PIH `Plev` value.
pub fn plev_from_level_sublevel(level: Option<&str>, sublevel: Option<&str>) -> Option<u16> {
    let level_high = level.and_then(level_to_plev_high)?;
    let sublevel_low = match sublevel {
        Some(sublevel) => sublevel_to_plev_low(sublevel)?,
        None => 0,
    };
    Some(u16::from(level_high) << 8 | u16::from(sublevel_low))
}

/// Resolve `Ppih`/`Plev` for the `jxpl` box from codestream PIH and optional caps strings.
///
/// Codestream values take precedence when non-zero. Caps strings are used as a fallback
/// (e.g. when `svtjpegxsenc` writes `Ppih=0`/`Plev=0`). Returns warning messages when
/// both sources disagree.
pub fn resolve_ppih_plev(
    codestream_ppih: u16,
    codestream_plev: u16,
    profile: Option<&str>,
    level: Option<&str>,
    sublevel: Option<&str>,
) -> Result<(u16, u16, Vec<String>), String> {
    let mut warnings = Vec::new();

    let caps_ppih = profile
        .map(|name| {
            profile_to_ppih(name).ok_or_else(|| format!("unknown JPEG XS profile '{name}'"))
        })
        .transpose()?;

    let caps_plev = match (level, sublevel) {
        (None, None) => None,
        (level, sublevel) => {
            if level.is_some() {
                Some(
                    plev_from_level_sublevel(level, sublevel)
                        .ok_or_else(|| format!("unknown JPEG XS level '{}'", level.unwrap()))?,
                )
            } else {
                let sublevel = sublevel.unwrap();
                sublevel_to_plev_low(sublevel)
                    .map(u16::from)
                    .ok_or_else(|| format!("unknown JPEG XS sublevel '{sublevel}'"))?
                    .into()
            }
        }
    };

    let ppih = if codestream_ppih != 0 {
        if let Some(expected) = caps_ppih
            && expected != codestream_ppih
        {
            warnings.push(format!(
                "codestream Ppih 0x{codestream_ppih:04x} does not match caps profile (0x{expected:04x})"
            ));
        }
        codestream_ppih
    } else {
        caps_ppih.unwrap_or(0)
    };

    let plev = if codestream_plev != 0 {
        if let Some(expected) = caps_plev
            && expected != codestream_plev
        {
            warnings.push(format!(
                "codestream Plev 0x{codestream_plev:04x} does not match caps level/sublevel (0x{expected:04x})"
            ));
        }
        codestream_plev
    } else {
        caps_plev.unwrap_or(0)
    };

    Ok((ppih, plev, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_ppih_roundtrip_names() {
        assert_eq!(profile_to_ppih("Main422.10"), Some(0x3540));
        assert_eq!(profile_to_ppih("MLS.12"), Some(0x6EC0));
        assert!(profile_to_ppih("Unknown").is_none());
    }

    #[test]
    fn level_sublevel_to_plev() {
        assert_eq!(
            plev_from_level_sublevel(Some("2k-1"), Some("Full")),
            Some(0x1080)
        );
        assert_eq!(
            plev_from_level_sublevel(Some("4k-1"), Some("Sublev6bpp")),
            Some(0x2008)
        );
        assert_eq!(plev_from_level_sublevel(Some("2k-1"), None), Some(0x1000));
    }

    #[test]
    fn resolve_prefers_codestream() {
        let (ppih, plev, warnings) = resolve_ppih_plev(
            0x3540,
            0x1080,
            Some("Main422.10"),
            Some("2k-1"),
            Some("Full"),
        )
        .unwrap();
        assert_eq!(ppih, 0x3540);
        assert_eq!(plev, 0x1080);
        assert!(warnings.is_empty());
    }

    #[test]
    fn resolve_falls_back_to_caps() {
        let (ppih, plev, warnings) =
            resolve_ppih_plev(0, 0, Some("Main422.10"), Some("2k-1"), Some("Full")).unwrap();
        assert_eq!(ppih, 0x3540);
        assert_eq!(plev, 0x1080);
        assert!(warnings.is_empty());
    }

    #[test]
    fn resolve_warns_on_mismatch() {
        let (_, _, warnings) =
            resolve_ppih_plev(0x1500, 0x1000, Some("Main422.10"), Some("2k-1"), None).unwrap();
        assert_eq!(warnings.len(), 1);
    }
}
