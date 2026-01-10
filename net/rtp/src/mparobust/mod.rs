// SPDX-License-Identifier: MPL-2.0

pub mod depay;

pub(crate) mod mpeg_audio_utils;
pub(crate) use mpeg_audio_utils::{FrameHeader, peek_frame_header};

#[cfg(test)]
mod tests;
