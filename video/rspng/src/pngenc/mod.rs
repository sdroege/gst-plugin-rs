// Copyright (C) 2020 Natanael Mojica <neithanmo@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib::GEnum;
use gst::glib;
use gst::prelude::*;

mod imp;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, GEnum)]
#[repr(u32)]
#[genum(type_name = "GstRsPngCompressionLevel")]
pub(crate) enum CompressionLevel {
    #[genum(name = "Default: Use the default compression level.", nick = "default")]
    Default,
    #[genum(name = "Fast: A fast compression algorithm.", nick = "fast")]
    Fast,
    #[genum(
        name = "Best: Uses the algorithm with the best results.",
        nick = "best"
    )]
    Best,
    #[genum(name = "Huffman: Huffman compression.", nick = "huffman")]
    Huffman,
    #[genum(name = "Rle: Rle compression.", nick = "rle")]
    Rle,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, GEnum)]
#[repr(u32)]
#[genum(type_name = "GstRsPngFilterType")]
pub(crate) enum FilterType {
    #[genum(
        name = "NoFilter: No filtering applied to the output.",
        nick = "nofilter"
    )]
    NoFilter,
    #[genum(name = "Sub: filter applied to each pixel.", nick = "sub")]
    Sub,
    #[genum(name = "Up: Up filter similar to Sub.", nick = "up")]
    Up,
    #[genum(
        name = "Avg: The Average filter uses the average of the two neighboring pixels.",
        nick = "avg"
    )]
    Avg,
    #[genum(
        name = "Paeth: The Paeth filter computes a simple linear function of the three neighboring pixels.",
        nick = "paeth"
    )]
    Paeth,
}

impl From<CompressionLevel> for png::Compression {
    fn from(value: CompressionLevel) -> Self {
        match value {
            CompressionLevel::Default => png::Compression::Default,
            CompressionLevel::Fast => png::Compression::Fast,
            CompressionLevel::Best => png::Compression::Best,
            CompressionLevel::Huffman => png::Compression::Huffman,
            CompressionLevel::Rle => png::Compression::Rle,
        }
    }
}

impl From<FilterType> for png::FilterType {
    fn from(value: FilterType) -> Self {
        match value {
            FilterType::NoFilter => png::FilterType::NoFilter,
            FilterType::Sub => png::FilterType::Sub,
            FilterType::Up => png::FilterType::Up,
            FilterType::Avg => png::FilterType::Avg,
            FilterType::Paeth => png::FilterType::Paeth,
        }
    }
}

glib::wrapper! {
    pub struct PngEncoder(ObjectSubclass<imp::PngEncoder>) @extends gst_video::VideoEncoder, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for PngEncoder {}
unsafe impl Sync for PngEncoder {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rspngenc",
        gst::Rank::Primary,
        PngEncoder::static_type(),
    )
}
