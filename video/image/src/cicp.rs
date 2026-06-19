use gst_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoTransferFunction,
};
use image::metadata::{
    Cicp, CicpColorPrimaries, CicpMatrixCoefficients, CicpTransferCharacteristics,
    CicpVideoFullRangeFlag,
};

#[derive(Debug, Copy, Clone)]
pub(crate) struct ImageCicp(pub Cicp);

impl From<ImageCicp> for Cicp {
    fn from(value: ImageCicp) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum UnsupportedCicp {
    #[error("Unknown color range {:?}", .0)]
    ColorRange(CicpVideoFullRangeFlag),
    #[error("Unknown color matrix {:?}", .0)]
    ColorMatrix(CicpMatrixCoefficients),
    #[error("Unknown transfer function {:?}", .0)]
    TransferFunction(CicpTransferCharacteristics),
    #[error("Unknown color primaries {:?}", .0)]
    Primaries(CicpColorPrimaries),
}

impl TryFrom<ImageCicp> for VideoColorimetry {
    type Error = UnsupportedCicp;

    fn try_from(value: ImageCicp) -> Result<Self, Self::Error> {
        use UnsupportedCicp::*;

        let rg = match value.0.full_range {
            CicpVideoFullRangeFlag::NarrowRange => VideoColorRange::Range16_235,
            CicpVideoFullRangeFlag::FullRange => VideoColorRange::Range0_255,
            v => return Err(ColorRange(v)),
        };

        let mx = match value.0.matrix {
            CicpMatrixCoefficients::Unspecified => VideoColorMatrix::Unknown,
            v => match VideoColorMatrix::from_iso(v as u32) {
                VideoColorMatrix::Unknown => return Err(ColorMatrix(v)),
                v => v,
            },
        };

        let tf = match value.0.transfer {
            CicpTransferCharacteristics::Unspecified => VideoTransferFunction::Unknown,
            v => match VideoTransferFunction::from_iso(v as u32) {
                VideoTransferFunction::Unknown => return Err(TransferFunction(v)),
                v => v,
            },
        };

        // See Rec. ITU-T H.273 (V4) (07/2024) table 2, p. 5
        // and the image-rs docs
        let pr = match value.0.primaries {
            CicpColorPrimaries::Unspecified => VideoColorPrimaries::Unknown,
            v => match VideoColorPrimaries::from_iso(v as u32) {
                VideoColorPrimaries::Unknown => return Err(Primaries(v)),
                v => v,
            },
        };

        Ok(VideoColorimetry::new(rg, mx, tf, pr))
    }
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum UnsupportedVideoColorimetry {
    #[error("Unknown color range {:?}", .0)]
    ColorRange(VideoColorRange),
    #[error("Unknown color matrix {:?}", .0)]
    ColorMatrix(VideoColorMatrix),
    #[error("Unknown transfer function {:?}", .0)]
    TransferFunction(VideoTransferFunction),
    #[error("Unknown color primaries {:?}", .0)]
    Primaries(VideoColorPrimaries),
}

impl TryFrom<VideoColorimetry> for ImageCicp {
    type Error = UnsupportedVideoColorimetry;

    fn try_from(value: VideoColorimetry) -> Result<Self, Self::Error> {
        use UnsupportedVideoColorimetry::*;

        // This can NOT be done with VideoColorPrimaries::to_iso because it
        // is unsafe to convert an integer to an enum value.
        let mx = match value.matrix() {
            VideoColorMatrix::Unknown => CicpMatrixCoefficients::Unspecified,
            VideoColorMatrix::Rgb => CicpMatrixCoefficients::Identity,
            VideoColorMatrix::Fcc => CicpMatrixCoefficients::UsFCC,
            VideoColorMatrix::Bt709 => CicpMatrixCoefficients::Bt709,
            VideoColorMatrix::Bt601 => CicpMatrixCoefficients::Smpte170m,
            VideoColorMatrix::Smpte240m => CicpMatrixCoefficients::Smpte240m,
            VideoColorMatrix::Bt2020 => CicpMatrixCoefficients::Bt2020NonConstant,
            v => return Err(ColorMatrix(v)),
        };

        let tf = match value.transfer() {
            VideoTransferFunction::Unknown => CicpTransferCharacteristics::Unspecified,
            VideoTransferFunction::Gamma10 => CicpTransferCharacteristics::Linear,
            VideoTransferFunction::Gamma22 => CicpTransferCharacteristics::Bt470M,
            VideoTransferFunction::Bt709 => CicpTransferCharacteristics::Bt709,
            VideoTransferFunction::Smpte240m => CicpTransferCharacteristics::Smpte240m,
            VideoTransferFunction::Srgb => CicpTransferCharacteristics::SRgb,
            VideoTransferFunction::Gamma28 => CicpTransferCharacteristics::Bt470BG,
            VideoTransferFunction::Log100 => CicpTransferCharacteristics::Log100,
            VideoTransferFunction::Log316 => CicpTransferCharacteristics::LogSqrt,
            VideoTransferFunction::Bt202012 => CicpTransferCharacteristics::Bt2020_12bit,
            VideoTransferFunction::Bt202010 => CicpTransferCharacteristics::Bt2020_10bit,
            VideoTransferFunction::Smpte2084 => CicpTransferCharacteristics::Smpte2084,
            VideoTransferFunction::AribStdB67 => CicpTransferCharacteristics::Bt2100Hlg,
            VideoTransferFunction::Bt601 => CicpTransferCharacteristics::Bt601,
            v => return Err(TransferFunction(v)),
        };

        let pr = match value.primaries() {
            VideoColorPrimaries::Bt709 => CicpColorPrimaries::SRgb,
            VideoColorPrimaries::Unknown => CicpColorPrimaries::Unspecified,
            VideoColorPrimaries::Bt470m => CicpColorPrimaries::RgbM,
            VideoColorPrimaries::Bt470bg => CicpColorPrimaries::RgbB,
            VideoColorPrimaries::Smpte170m => CicpColorPrimaries::Bt601,
            VideoColorPrimaries::Smpte240m => CicpColorPrimaries::Rgb240m,
            VideoColorPrimaries::Film => CicpColorPrimaries::GenericFilm,
            VideoColorPrimaries::Bt2020 => CicpColorPrimaries::Rgb2020,
            VideoColorPrimaries::Smptest428 => CicpColorPrimaries::Xyz,
            VideoColorPrimaries::Smpterp431 => CicpColorPrimaries::SmpteRp431,
            VideoColorPrimaries::Smpteeg432 => CicpColorPrimaries::SmpteRp432,
            VideoColorPrimaries::Ebu3213 => CicpColorPrimaries::Industry22,
            v => return Err(Primaries(v)),
        };

        let rg = match value.range() {
            gst_video::VideoColorRange::Range0_255 => {
                image::metadata::CicpVideoFullRangeFlag::FullRange
            }
            gst_video::VideoColorRange::Range16_235 => {
                image::metadata::CicpVideoFullRangeFlag::NarrowRange
            }
            v => return Err(ColorRange(v)),
        };

        Ok(ImageCicp(Cicp {
            full_range: rg,
            matrix: mx,
            primaries: pr,
            transfer: tf,
        }))
    }
}
