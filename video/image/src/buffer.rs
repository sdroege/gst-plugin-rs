use byte_slice_cast::*;
use image::{DynamicImage, ImageBuffer, PixelWithColorType};

use std::ops::Deref;

/// Helper struct to implement AsRef<[u8]> for DynamicImage.
pub(crate) struct ImageWrapper(pub DynamicImage);

impl AsRef<[u8]> for ImageWrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

#[track_caller]
fn copy_to_framebuffer_with_padding(
    image: &[u8],
    buffer: &mut [u8],
    row_stride: usize,
    padding: usize,
) {
    for (src, dst) in image
        .chunks_exact(row_stride)
        .zip(buffer.chunks_exact_mut(row_stride + padding))
    {
        let (pixels, pad) = dst.split_at_mut(row_stride);
        pixels.copy_from_slice(src);
        pad.fill(0);
    }
}

/// Fills the given slice with the contents of the image, with padding as
/// appropriate.
#[track_caller]
#[inline(never)]
pub fn copy_from_image<P, C>(image: &ImageBuffer<P, C>, buffer: &mut [u8], format_stride: usize)
where
    P: PixelWithColorType,
    C: Deref<Target = [P::Subpixel]> + AsByteSlice<P::Subpixel>,
{
    let layout = image.sample_layout();
    let row_stride = layout
        .height_stride
        .strict_mul(std::mem::size_of::<P::Subpixel>());
    // Padding may be zero in case we're forcing a copy-to-buffer.
    assert!(format_stride >= row_stride);
    let padding = format_stride - row_stride;
    copy_to_framebuffer_with_padding(image.as_raw().as_byte_slice(), buffer, row_stride, padding);
}

/// If given a misaligned (for GStreamer's memory requirements) ImageBuffer,
/// this function clones the image into a Vec and pads the rows as
/// appropriate. Otherwise returns None.
#[track_caller]
#[inline(never)]
fn convert_strides<P, C>(image: &ImageBuffer<P, C>) -> Option<gst::Buffer>
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
        let mut mem = gst::Memory::with_size(new_len);
        {
            let mut mem_map = mem.get_mut().unwrap().map_writable().unwrap();
            copy_to_framebuffer_with_padding(
                image.as_raw().as_byte_slice(),
                mem_map.as_mut_slice(),
                row_stride,
                padding,
            );
        }
        Some(gst::Buffer::from(mem))
    } else {
        None
    }
}

pub(crate) trait ImageStride {
    fn stride_in_bytes(&self) -> i32;
}

/// This trait converts a DynamicImage based on whether its data
/// layout can be directly ingested as a gst::Buffer.
/// It is a zerocopy conversion into a Buffer if possible, otherwise
/// it will be cloned and padded as required.
///
/// Conversions under known zerocopy support must use
/// gst::Buffer::from_slice(ImageWrapper(img)) instead.
pub(crate) trait GStreamerImage {
    fn wrap_for_gstreamer(self) -> gst::Buffer;
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
    fn wrap_for_gstreamer(self) -> gst::Buffer {
        use DynamicImage::*;
        match self {
            ImageRgb8(ref v) => match convert_strides(v) {
                Some(v) => v,
                None => gst::Buffer::from_slice(ImageWrapper(self)),
            },
            ImageRgba8(ref v) => match convert_strides(v) {
                Some(v) => v,
                None => gst::Buffer::from_slice(ImageWrapper(self)),
            },
            ImageLuma8(ref v) => match convert_strides(v) {
                Some(v) => v,
                None => gst::Buffer::from_slice(ImageWrapper(self)),
            },
            ImageLuma16(ref v) => match convert_strides(v) {
                Some(v) => v,
                None => gst::Buffer::from_slice(ImageWrapper(self)),
            },
            ImageRgba16(ref v) => match convert_strides(v) {
                Some(v) => v,
                None => gst::Buffer::from_slice(ImageWrapper(self)),
            },
            _ => unreachable!(),
        }
    }
}
