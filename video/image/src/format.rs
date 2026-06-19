use std::fmt::Display;

use gst::glib;
use image::ImageFormat;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRsImageFormat")]
pub enum Format {
    #[enum_value(name = "Animated PNG", nick = "apng")]
    Apng,
    #[enum_value(name = "AV1 image file format", nick = "avif")]
    Avif,
    #[enum_value(name = "Microsoft bitmap", nick = "bmp")]
    Bmp,
    #[enum_value(name = "DirectDraw Surface", nick = "dds")]
    Dds,
    #[enum_value(name = "OpenEXR", nick = "exr")]
    Exr,
    #[enum_value(name = "The Farbfeld simple image encoding format", nick = "farbfeld")]
    Farbfeld,
    #[enum_value(name = "GIF image file format", nick = "gif")]
    Gif,
    #[enum_value(name = "Radiance HDR image file format", nick = "hdr")]
    Hdr,
    #[enum_value(name = "Microsoft icon", nick = "ico")]
    Ico,
    #[enum_value(name = "JPEG image file format", nick = "jpeg")]
    Jpeg,
    #[enum_value(name = "OpenRaster image file format", nick = "openraster")]
    OpenRaster,
    #[enum_value(name = "Nokia Over The Air bitmap", nick = "nokia")]
    Nokia,
    #[enum_value(name = "PiCture eXchange file format", nick = "pcx")]
    Pcx,
    #[enum_value(name = "Portable Network Graphics", nick = "png")]
    Png,
    #[enum_value(name = "Netpbm image file format", nick = "pnm")]
    Pnm,
    #[enum_value(name = "The Quite OK Image Format", nick = "qoi")]
    Qoi,
    #[enum_value(name = "Silicon Graphics Image", nick = "sgi")]
    Sgi,
    #[enum_value(name = "Truevision Targa", nick = "tga")]
    Tga,
    #[enum_value(name = "Tagged Image File Format", nick = "tiff")]
    Tiff,
    #[enum_value(name = "Wireless Application Protocol Bitmap", nick = "wbmp")]
    Wbmp,
    #[enum_value(name = "WebP image file format", nick = "webp")]
    WebP,
    #[enum_value(name = "X Bitmap", nick = "xbm")]
    Xbm,
    #[enum_value(name = "X Pixmap", nick = "xpm")]
    Xpm,
}

#[derive(Debug, Copy, Clone)]
pub enum UnsupportedFormat<'a> {
    MimetypeNotFound(&'a str),
    NonNativeFormat(Format),
    UnhandledFormat(ImageFormat),
}

impl From<UnsupportedFormat<'_>> for gst::ErrorMessage {
    fn from(value: UnsupportedFormat) -> Self {
        gst::ErrorMessage::from(&value)
    }
}

impl From<&UnsupportedFormat<'_>> for gst::ErrorMessage {
    fn from(value: &UnsupportedFormat) -> Self {
        match value {
            UnsupportedFormat::MimetypeNotFound(v) => {
                gst::error_msg!(gst::StreamError::CodecNotFound, ["Unknown mimetype {v}"])
            }
            UnsupportedFormat::NonNativeFormat(v) => {
                gst::error_msg!(gst::StreamError::CodecNotFound, ["Unknown format {v:?}"])
            }
            UnsupportedFormat::UnhandledFormat(v) => {
                gst::error_msg!(
                    gst::StreamError::CodecNotFound,
                    ["FIXME: image-rs format {v:?} has no supported caps yet"]
                )
            }
        }
    }
}

impl Display for UnsupportedFormat<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", gst::ErrorMessage::from(self))?;
        Ok(())
    }
}

impl TryFrom<Format> for ImageFormat {
    type Error = UnsupportedFormat<'static>;

    fn try_from(value: Format) -> Result<Self, Self::Error> {
        match value {
            Format::Avif => Ok(ImageFormat::Avif),
            Format::Apng => Ok(ImageFormat::Png),
            Format::Bmp => Ok(ImageFormat::Bmp),
            Format::Exr => Ok(ImageFormat::OpenExr),
            Format::Farbfeld => Ok(ImageFormat::Farbfeld),
            Format::Gif => Ok(ImageFormat::Gif),
            Format::Hdr => Ok(ImageFormat::Hdr),
            Format::Ico => Ok(ImageFormat::Ico),
            Format::Jpeg => Ok(ImageFormat::Jpeg),
            Format::Pnm => Ok(ImageFormat::Pnm),
            Format::Png => Ok(ImageFormat::Png),
            Format::Qoi => Ok(ImageFormat::Qoi),
            Format::Tga => Ok(ImageFormat::Tga),
            Format::Tiff => Ok(ImageFormat::Tiff),
            Format::WebP => Ok(ImageFormat::WebP),
            v => Err(UnsupportedFormat::NonNativeFormat(v)),
        }
    }
}

impl TryFrom<ImageFormat> for Format {
    type Error = UnsupportedFormat<'static>;

    fn try_from(value: ImageFormat) -> Result<Self, Self::Error> {
        match value {
            ImageFormat::Avif => Ok(Format::Avif),
            ImageFormat::Bmp => Ok(Format::Bmp),
            ImageFormat::Dds => Ok(Format::Dds),
            ImageFormat::OpenExr => Ok(Format::Exr),
            ImageFormat::Farbfeld => Ok(Format::Farbfeld),
            ImageFormat::Gif => Ok(Format::Gif),
            ImageFormat::Hdr => Ok(Format::Hdr),
            ImageFormat::Ico => Ok(Format::Ico),
            ImageFormat::Jpeg => Ok(Format::Jpeg),
            ImageFormat::Png => Ok(Format::Png),
            ImageFormat::Pnm => Ok(Format::Pnm),
            ImageFormat::Qoi => Ok(Format::Qoi),
            ImageFormat::Tga => Ok(Format::Tga),
            ImageFormat::Tiff => Ok(Format::Tiff),
            ImageFormat::WebP => Ok(Format::WebP),
            v => Err(UnsupportedFormat::UnhandledFormat(v)),
        }
    }
}

impl<'a> TryFrom<&'a gst::StructureRef> for Format {
    type Error = UnsupportedFormat<'a>;

    fn try_from(value: &'a gst::StructureRef) -> Result<Self, Self::Error> {
        match value.name().as_str() {
            "image/x-MS-bmp" => Ok(Format::Bmp),
            "image/x-direct-draw-surface" => Ok(Format::Dds),
            "image/x-farbfeld" => Ok(Format::Farbfeld),
            "image/openraster" => Ok(Format::OpenRaster),
            "image/x-nokia-over-the-air-bitmap" => Ok(Format::Nokia),
            "image/vnd.zbrush.pcx" => Ok(Format::Pcx),
            "image/x-pcx" => Ok(Format::Pcx),
            "image/x-portable-bitmap" => Ok(Format::Pnm),
            "image/x-portable-graymap" => Ok(Format::Pnm),
            "image/x-portable-pixmap" => Ok(Format::Pnm),
            // https://github.com/phoboslab/qoi/issues/167
            "image/qoi" => Ok(Format::Qoi),
            "image/sgi" => Ok(Format::Sgi),
            "image/x-tga" => Ok(Format::Tga),
            "image/vnd.wap.wbmp" => Ok(Format::Wbmp),
            "image/x-xbitmap" | "image/x-xbm" => Ok(Format::Xbm),
            "image/x-xpixmap" => Ok(Format::Xpm),
            // https://learn.microsoft.com/es-es/windows/win32/wic/dds-format-overview
            "image/vnd.ms-dds" => Ok(Format::Dds),
            "image/png" => {
                if let Ok(v) = value.get::<bool>("animated")
                    && v
                {
                    return Ok(Format::Apng);
                }
                Ok(Format::Png)
            }
            v => match ImageFormat::from_mime_type(v) {
                Some(v) => Format::try_from(v),
                None => Err(UnsupportedFormat::MimetypeNotFound(v)),
            },
        }
    }
}

impl Format {
    /// Missing formats from gdkpixbufdec:
    /// - application/x-navi-animation
    /// - image/svg
    /// - image/svg+xml
    pub fn all_decoding_formats() -> impl IntoIterator<Item = gst::Caps> {
        [
            // FIXME upstream: AVIF also supports animations
            // https://github.com/image-rs/image/issues/2794
            #[cfg(feature = "avif")]
            make_caps!(ImageFormat::Avif),
            #[cfg(any(feature = "bmp", feature = "ico"))]
            make_caps_with_extra_mimetypes!(ImageFormat::Bmp, "image/x-MS-bmp"),
            // https://learn.microsoft.com/en-us/windows/win32/wic/dds-format-overview
            #[cfg(feature = "dds")]
            make_caps!("image/vnd.ms-dds"),
            #[cfg(feature = "exr")]
            make_caps!(ImageFormat::OpenExr),
            #[cfg(feature = "ff")]
            /// farbfeld's MIME type in image-rs is application/octet-stream, correct it here
            make_caps!("image/x-farbfeld"),
            #[cfg(feature = "hdr")]
            make_caps!(ImageFormat::Hdr),
            #[cfg(feature = "ico")]
            make_caps!(ImageFormat::Ico),
            #[cfg(feature = "jpeg")]
            // FIXME upstream: doesn't support MJPEG
            make_caps!(ImageFormat::Jpeg),
            #[cfg(feature = "ora")]
            make_caps!("image/openraster"),
            #[cfg(feature = "otb")]
            /// https://snisurset.net/code/abydos/supported.html
            make_caps!("image/x-nokia-over-the-air-bitmap"),
            #[cfg(feature = "pcx")]
            make_caps!("image/vnd.zbrush.pcx", "image/x-pcx"),
            #[cfg(any(feature = "png", feature = "ico"))]
            make_caps!(ImageFormat::Png),
            #[cfg(feature = "pnm")]
            make_caps_with_extra_mimetypes!(
                ImageFormat::Pnm,
                "image/x-portable-bitmap",
                "image/x-portable-graymap",
                "image/x-portable-pixmap"
            ),
            // https://github.com/phoboslab/qoi/issues/167
            #[cfg(feature = "qoi")]
            make_caps_with_extra_mimetypes!(ImageFormat::Qoi, "image/qoi"),
            #[cfg(feature = "sgi")]
            make_caps!("image/sgi"),
            #[cfg(feature = "tga")]
            make_caps_with_extra_mimetypes!(ImageFormat::Tga, "image/x-tga"),
            #[cfg(feature = "tiff")]
            make_caps!(ImageFormat::Tiff),
            #[cfg(feature = "webp")]
            make_caps!(ImageFormat::WebP),
            #[cfg(feature = "wbmp")]
            make_caps!("image/vnd.wap.wbmp"),
            #[cfg(feature = "xbm")]
            make_caps!("image/x-xbitmap", "image/x-xbm"),
            #[cfg(feature = "xpm")]
            make_caps!("image/x-xpixmap"),
        ]
    }
}
