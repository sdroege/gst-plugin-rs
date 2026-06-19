use byte_slice_cast::*;
use image::{DynamicImage, ImageBuffer, PixelWithColorType};

use std::ops::Deref;

/// This enum converts a DynamicImage based on whether its data
/// layout can be directly ingested as a gst::Buffer (Image)
/// or not (Vec).
pub(crate) enum Wrapper {
    /// The DynamicImage's sample layout is 4-byte aligned, or
    /// GstVideoMeta is supported. Can be directly converted to
    /// a gst::Buffer.
    Image(DynamicImage),
    /// The DynamicImage's sample layout is not 4-byte aligned,
    /// so it's been cloned and padded as required.
    Vec(Vec<u8>),
}

impl AsRef<[u8]> for Wrapper {
    fn as_ref(&self) -> &[u8] {
        match self {
            Wrapper::Image(v) => v.as_bytes(),
            Wrapper::Vec(v) => v.as_slice(),
        }
    }
}

impl Wrapper {
    pub fn into_gst_buffer(self) -> gst::Buffer {
        gst::Buffer::from_slice(self)
    }
}

/// If given a misaligned (for GStreamer's memory requirements) ImageBuffer,
/// this function clones the image into a Vec and pads the rows as
/// appropriate. Otherwise returns None.
#[track_caller]
#[inline(never)]
fn convert_strides<P, C>(image: &ImageBuffer<P, C>) -> Option<Vec<u8>>
where
    P: PixelWithColorType,
    C: Deref<Target = [P::Subpixel]> + AsByteSlice<P::Subpixel>,
{
    let layout = image.sample_layout();
    let row_stride = layout
        .height_stride
        .strict_mul(std::mem::size_of::<P::Subpixel>());

    if !row_stride.is_multiple_of(4) {
        let fixed_row_stride = row_stride.next_multiple_of(4);
        assert!(fixed_row_stride > row_stride);
        let padding = fixed_row_stride - row_stride;
        let new_len = fixed_row_stride.strict_mul(layout.height as usize);
        let mut buffer = Vec::<u8>::with_capacity(new_len);
        for row in image.as_raw().as_byte_slice().chunks_exact(row_stride) {
            buffer.extend(row);
            buffer.resize(buffer.len() + padding, 0);
        }
        assert_eq!(buffer.len(), new_len);
        Some(buffer)
    } else {
        None
    }
}

pub(crate) trait ImageStride {
    fn stride_in_bytes(&self) -> i32;
}

pub(crate) trait GStreamerImage {
    fn wrap_for_gstreamer(self) -> Wrapper;
}

impl<P, C> ImageStride for ImageBuffer<P, C>
where
    P: PixelWithColorType,
    C: Deref<Target = [P::Subpixel]>,
{
    fn stride_in_bytes(&self) -> i32 {
        // image-rs's height_stride is equivalent to VideoFrame.comp_stride
        self.sample_layout()
            .height_stride
            .checked_mul(std::mem::size_of::<P::Subpixel>())
            .map(i32::try_from)
            .unwrap()
            .unwrap()
    }
}

impl ImageStride for DynamicImage {
    fn stride_in_bytes(&self) -> i32 {
        use DynamicImage::*;
        match self {
            ImageRgb8(v) => v.stride_in_bytes(),
            ImageRgba8(v) => v.stride_in_bytes(),
            ImageLuma8(v) => v.stride_in_bytes(),
            ImageLuma16(v) => v.stride_in_bytes(),
            ImageRgba16(v) => v.stride_in_bytes(),
            _ => unreachable!(),
        }
    }
}

impl GStreamerImage for DynamicImage {
    fn wrap_for_gstreamer(self) -> Wrapper {
        use DynamicImage::*;
        use Wrapper::*;
        match self {
            ImageRgb8(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            ImageRgba8(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            ImageLuma8(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            ImageLuma16(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            ImageRgba16(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            _ => unreachable!(),
        }
    }
}
