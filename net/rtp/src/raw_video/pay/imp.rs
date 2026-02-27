// GStreamer RTP Raw Video Payloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpvrawpay2
 * @see_also: rtpvrawdepay2, rtpvrawdepay, rtpvrawpay
 *
 * Payload raw video frames into RTP packets as per [RFC 4175][rfc-4175].
 *
 * [rfc-4175]: https://www.rfc-editor.org/rfc/rfc4175.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 videotestsrc ! video/x-raw,width=1280,height=720,framerate=25/1,format=RGB ! timeoverlay font-desc=Sans,22 ! queue ! rtpvrawpay2 chunks-per-frame=10 ! udpsink host=127.0.0.1 port=5555 buffer-size=4194304
 * ]| This will create and payload a raw video stream in RGB format with a test pattern and send it out via UDP.
 *
 * ## Performance and system tuning considerations
 *
 * Raw uncompressed video can easily take up a lot of memory and generate high data rates and
 * packet rates, and as such is more demanding on the system and network than lower-bitrate
 * compressed video.
 *
 * This means you may need to tune your system's network configuration and configure udpsink
 * for high datarate streams.
 *
 * In particular, you may want to increase the maximum allowed buffer size for the kernel-side
 * UDP send buffer, as the default is quite low, and a lot of that may be taken up by
 * kernel-internal data structure overhead already.
 *
 * On Linux systems you can change the maximum allowed value for the send buffer with e.g.
 * |[
 * sysctl -w net.core.wmem_max=67108864
 * ]|
 *
 * Alternatively this can also be configured in `/etc/sysctl.conf`.
 *
 * Once this is configured kernel-side, you can use `udpsink buffer-size=NNN` to increase the
 * value to something larger than the default. If the value is too low it's possible that a lot
 * of packets may never get sent out because they will be overwritten by new data before they can
 * all be sent out.
 *
 * On the payloader side you can set the `chunks-per-frame` property to make the payloader output
 * RTP packets in batches instead of only when the entire video frame has been payloaded. That way
 * the network stack can already start sending out data while the payloader is payloading the rest
 * of the video data.
 *
 * Since: plugins-rs-0.15.0
 */
use atomic_refcell::AtomicRefCell;

use gst::{glib, prelude::*, subclass::prelude::*};

use std::{num::Wrapping, sync::LazyLock};

use gst_video::{VideoColorimetry, VideoFormat, VideoFrame, VideoFrameExt, VideoInfo};

use crate::basepay::RtpBasePay2Ext;

use super::packing_template::{FramePackingTemplate, VRAW_CHUNK_HDR_LEN, VRAW_EXT_SEQNUM_LEN};

use std::str::FromStr;

#[derive(Default)]
pub struct RtpRawVideoPay {
    state: AtomicRefCell<State>,
}

#[derive(Default)]
struct State {
    video_info: Option<VideoInfo>,
    packing_template: Option<FramePackingTemplate>,
    extended_seqnum: Wrapping<u32>,
    // Temporary buffer that can be used for swizzling/packing pixels before payloading
    scratch_space_vec: Vec<u8>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpvrawpay2",
        gst::DebugColorFlags::empty(),
        Some("RTP Raw Video Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpRawVideoPay {
    const NAME: &'static str = "GstRtpRawVideoPay";
    type Type = super::RtpRawVideoPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpRawVideoPay {}

impl GstObjectImpl for RtpRawVideoPay {}

impl ElementImpl for RtpRawVideoPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP Raw Video Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload a Raw Uncompressed Video Stream into RTP packets (RFC 4175)",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("clock-rate", 90000i32)
                    .field("encoding-name", "RAW")
                    .field(
                        "sampling",
                        gst::List::new([
                            "RGB",
                            "RGBA",
                            "BGR",
                            "BGRA",
                            "YCbCr-4:4:4",
                            "YCbCr-4:2:2",
                            "YCbCr-4:2:0",
                            "YCbCr-4:1:1",
                        ]),
                    )
                    .field("depth", gst::List::new(["8", "10", "12", "16"]))
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst_video::VideoCapsBuilder::new()
                    // Note: not advertising Ayuv here, which was a mistake and should be v308 really
                    .format_list([
                        VideoFormat::Rgb,
                        VideoFormat::Rgba,
                        VideoFormat::Bgr,
                        VideoFormat::Bgra,
                        VideoFormat::V308,
                        VideoFormat::Uyvy,
                        //VideoFormat::I420, // TODO: needs rtp packet payloading with swizzling
                        //VideoFormat::Y41b, // TODO: needs rtp packet payloading with swizzling
                        VideoFormat::Uyvp,
                    ])
                    .height_range(1..=32767)
                    .width_range(1..=32767)
                    .field("interlace-mode", "progressive") // TODO: handle interlaced too
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basepay::RtpBasePay2Impl for RtpRawVideoPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let Ok(info) = gst_video::VideoInfo::from_caps(caps) else {
            gst::error!(
                CAT,
                imp = self,
                "Can't parse input caps {caps} into video info"
            );
            return false;
        };

        gst::info!(CAT, imp = self, "Got caps, video info: {info:?}");

        let (sampling, pgroup, x_inc, y_inc) = match info.format() {
            VideoFormat::Rgb => ("RGB", 3, 1, 1),
            VideoFormat::Rgba => ("RGBA", 4, 1, 1),
            VideoFormat::Bgr => ("BGR", 3, 1, 1),
            VideoFormat::Bgra => ("BGRA", 4, 1, 1),
            // Not advertising AYUV since we just drop the alpha then, should've been v308 instead
            // VideoFormat::Ayuv => ("YCbCr-4:4:4", 3, 1, 1),
            VideoFormat::V308 => ("YCbCr-4:4:4", 3, 1, 1),
            VideoFormat::Uyvy => ("YCbCr-4:2:2", 4, 2, 1),
            VideoFormat::I420 => ("YCbCr-4:2:0", 6, 2, 2),
            VideoFormat::Y41b => ("YCbCr-4:1:1", 6, 4, 1),
            VideoFormat::Uyvp => ("YCbCr-4:2:2", 5, 2, 1),
            _ => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Unexpected video format {:?}",
                    info.format()
                );
                return false;
            }
        };

        // We always have the same depths for all components (we don't support 5:6:5 RGB yet)
        let depth = info.comp_depth(0);

        // Assume SDR transfer system characteristic unless overridden below
        let mut tsc = "SDR";

        // FIXME: Should there be constants/defines for these in gst_video, to match GST_VIDEO_COLORIMETRY_*?
        let colorimetry = if info.colorimetry() == VideoColorimetry::from_str("bt601").unwrap() {
            "BT601"
        } else if info.colorimetry() == VideoColorimetry::from_str("bt709").unwrap() {
            "BT709"
        } else if info.colorimetry() == VideoColorimetry::from_str("bt2020").unwrap()
            || info.colorimetry() == VideoColorimetry::from_str("bt2020-10").unwrap()
        {
            "BT2020"
        } else if info.colorimetry() == VideoColorimetry::from_str("bt2100-pq").unwrap() {
            tsc = "PQ";
            "BT2100"
        } else if info.colorimetry() == VideoColorimetry::from_str("bt2100-hlg").unwrap() {
            tsc = "HLG";
            "BT2100"
        } else if info.colorimetry() == VideoColorimetry::from_str("smpte240m").unwrap() {
            "SMPTE240M"
        } else {
            gst::info!(CAT, imp = self, "Mapping {:?} to BT709", info.colorimetry());
            "BT709"
        };

        let framerate = if info.fps().numer() == 0 {
            None
        } else if info.fps().denom() == 1 {
            Some(info.fps().numer().to_string())
        } else {
            Some(format!("{}/{}", info.fps().numer(), info.fps().denom()))
        };

        let chroma_position = if ["YCbCr-4:2:2", "YCbCr-4:2:0", "YCbCr-4:1:1"].contains(&sampling) {
            if info
                .chroma_site()
                .contains(gst_video::VideoChromaSite::COSITED)
            {
                Some("0")
            } else if info
                .chroma_site()
                .contains(gst_video::VideoChromaSite::H_COSITED)
            {
                Some("3")
            } else if info
                .chroma_site()
                .contains(gst_video::VideoChromaSite::V_COSITED)
            {
                Some("1")
            } else if info
                .chroma_site()
                .contains(gst_video::VideoChromaSite::NONE)
            {
                Some("4")
            } else {
                None
            }
        } else {
            // Chroma-siting only makes sense for sub-sampled YUV
            None
        };

        let mut src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "RAW")
            .field("clock-rate", 90000i32)
            .field("sampling", sampling)
            .field("width", format!("{}", info.width()))
            .field("height", format!("{}", info.height()))
            .field("depth", format!("{depth}"))
            .field("colorimetry", colorimetry)
            .field("tsc", tsc)
            .field_if("interlace", "true", info.is_interlaced())
            .field_if_some("exactframerate", framerate)
            .field_if_some("chroma-position", chroma_position)
            .build();

        // Special handling for BT601-5 vs. BT601 and BT709-2 vs. BT709. The dash-less versions
        // are used by ST2110-20. We prefer the ST2110-20 values.
        if ["BT601", "BT709"].contains(&colorimetry) {
            let obj = self.obj();
            let src_pad = obj.src_pad();

            let mut filter_caps = src_caps.copy();
            {
                let filter_caps = filter_caps.get_mut().unwrap();

                filter_caps.set(
                    "colorimetry",
                    if colorimetry == "BT601" {
                        gst::List::new(["BT601", "BT601-5"])
                    } else {
                        gst::List::new(["BT709", "BT709-2"])
                    },
                );
            }

            let mut downstream_caps = src_pad.peer_query_caps(Some(&filter_caps));
            downstream_caps.fixate();
            let negotiated_colorimetry = downstream_caps
                .structure(0)
                .and_then(|s| s.get::<&str>("colorimetry").ok());

            if colorimetry == "BT601" && negotiated_colorimetry == Some("BT601-5") {
                let src_caps = src_caps.get_mut().unwrap();
                src_caps.set("colorimetry", "BT601-5");
            } else if negotiated_colorimetry == Some("BT709-2") {
                let src_caps = src_caps.get_mut().unwrap();
                src_caps.set("colorimetry", "BT709-2");
            }
        }

        self.obj().set_src_caps(&src_caps);

        let y_inc = if info.is_interlaced() {
            y_inc * 2
        } else {
            y_inc
        };

        gst::info!(
            CAT,
            imp = self,
            "Format config: {sampling}, pgroup {pgroup}, \
             x_inc {x_inc}, y_inc {y_inc} depth {depth}, \
             interlaced {}",
            info.is_interlaced()
        );

        let mut state = self.state.borrow_mut();

        let max_payload_size = self.obj().max_payload_size() as usize;

        // Build a template for how to pack the frame data into packets
        let Ok(packing_template) =
            FramePackingTemplate::new(max_payload_size, &info, 0, pgroup, x_inc, y_inc)
        else {
            gst::error!(CAT, imp = self, "Failed to create frame packing template");
            return false;
        };
        state.packing_template = Some(packing_template);

        state.video_info = Some(info);

        true
    }

    // RTP Payload Format for Uncompressed Video:
    // https://www.rfc-editor.org/rfc/rfc4175.html#section-4
    //
    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        // Reset extended seqnum counter if the seqnum was reset
        if state.extended_seqnum.0 & 0x0000_ffff != self.obj().next_seqnum() as u32 {
            state.extended_seqnum.0 = self.obj().next_seqnum() as u32;
        }

        let video_info = state.video_info.as_ref().unwrap();

        let vframe =
            VideoFrame::from_buffer_readable(buffer.clone(), video_info).map_err(|_| {
                gst::error!(CAT, imp = self, "Can't map video buffer for reading");
                gst::FlowError::Error
            })?;

        gst::log!(CAT, imp = self, "video frame: {vframe:?}");

        let video_info = vframe.info();

        let State {
            packing_template,
            extended_seqnum,
            scratch_space_vec,
            ..
        } = &mut *state;

        let packing_template = packing_template.as_ref().unwrap();

        // Temporary buffer that can be used for swizzling/packing pixels before payloading
        let mut scratch_space_vec = scratch_space_vec;

        // Only allocate heap memory if needed
        let need_scratch_space =
            video_info.n_planes() > 1 || video_info.format() == VideoFormat::V308;

        if need_scratch_space {
            scratch_space_vec.resize(packing_template.mtu, 0u8);
        }

        let n_packets = packing_template.packets.len();

        let field = 0; // FIXME: support interlaced

        for (i, packet) in packing_template.packets.iter().enumerate() {
            let is_last = i == (n_packets - 1);

            let hdr = packet.make_headers(field, extended_seqnum.0);

            let mut rtp_packet_builder = rtp_types::RtpPacketBuilder::new()
                .marker_bit(is_last)
                .payload(hdr.as_slice());

            match video_info.format() {
                // Packed formats that can be payloaded directly as-is
                VideoFormat::Rgb
                | VideoFormat::Rgba
                | VideoFormat::Bgr
                | VideoFormat::Bgra
                | VideoFormat::Uyvy => {
                    let data = vframe.plane_data(0).unwrap();
                    let stride = vframe.plane_stride()[0] as usize;
                    let pstride = vframe.comp_pstride(0) as usize;

                    for chunks in &packet.chunks {
                        let line_number = chunks.y_off as usize;
                        let pixel_offset = chunks.x_off as usize;

                        let length = chunks.length as usize;

                        let line = data
                            .chunks_exact(stride)
                            .skip(field as usize)
                            .nth(line_number)
                            .unwrap();

                        let byte_offset = pixel_offset * pstride;
                        let pixels = &line[byte_offset..][..length];

                        rtp_packet_builder = rtp_packet_builder.payload(pixels);
                    }
                }

                // v308: Packed format, but components need to be swizzled for payloading
                VideoFormat::V308 => {
                    let data = vframe.plane_data(0).unwrap();
                    let stride = vframe.plane_stride()[0] as usize;
                    let pstride = vframe.comp_pstride(0) as usize;

                    let mut packed_len = 0;

                    let scratch_space = &mut scratch_space_vec;

                    let mut scratch_iter = scratch_space.chunks_exact_mut(3);

                    for chunks in &packet.chunks {
                        let line_number = chunks.y_off as usize;
                        let pixel_offset = chunks.x_off as usize;

                        let length = chunks.length as usize;

                        let line = data
                            .chunks_exact(stride)
                            .skip(field as usize)
                            .nth(line_number)
                            .unwrap();

                        let byte_offset = pixel_offset * pstride;
                        let pixels = &line[byte_offset..][..length];

                        for (src_pixel, dest_pixel) in
                            std::iter::zip(pixels.chunks_exact(3), &mut scratch_iter)
                        {
                            dest_pixel[0] = src_pixel[1]; // Cb
                            dest_pixel[1] = src_pixel[0]; // Y
                            dest_pixel[2] = src_pixel[2]; // Cr
                        }

                        packed_len += length;
                    }

                    rtp_packet_builder = rtp_packet_builder.payload(&scratch_space[0..packed_len]);
                }

                unexpected_fmt => todo!("Implement {unexpected_fmt:?}"),
            }

            self.obj().queue_packet(id.into(), rtp_packet_builder)?;
            *extended_seqnum += 1;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        // Make sure configured MTU is large enough. Need space for headers and at least some data.
        let max_payload_size = self.obj().max_payload_size() as usize;

        if max_payload_size <= VRAW_EXT_SEQNUM_LEN + VRAW_CHUNK_HDR_LEN + 64 {
            return Err(gst::error_msg!(
                gst::LibraryError::Settings,
                ("Configured MTU is too small")
            ));
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        Ok(())
    }
}

impl RtpRawVideoPay {}
