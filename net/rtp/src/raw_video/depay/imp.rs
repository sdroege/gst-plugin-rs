// GStreamer RTP Raw Video Depayloader
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpvrawdepay2
 * @see_also: rtpvrawpay2, rtpvrawpay, rtpvrawdepay
 *
 * Depayload raw video frames from RTP packets as per [RFC 4175][rfc-4175].
 *
 * [rfc-4175]: https://www.rfc-editor.org/rfc/rfc4175.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc address=127.0.0.1 port=5555 caps='application/x-rtp,media=video,clock-rate=90000,encoding-name=RAW,width=320,height=240,depth=8,sampling=BGRA' ! rtpjitterbuffer latency=100 ! rtpvrawdepay2 ! queue ! videoconvertscale ! autovideosink
 * ]| This will depayload an incoming Raw Video RTP stream. You can use the #rtpvrawpay2 or #rtpvrawpay
 * element to create such an RTP stream.
 *
 * ## Performance and system tuning considerations
 *
 * Raw uncompressed video can easily take up a lot of memory and generate high data rates and
 * packet rates, and as such is more demanding on the system and network than lower-bitrate
 * compressed video.
 *
 * This means you may need to tune your system's network configuration and configure #udpsrc
 * for high datarate streams.
 *
 * In particular, you may want to increase the maximum allowed buffer size for the kernel-side
 * UDP receive buffer, as the default is quite low, and a lot of that may be taken up by
 * kernel-internal data structure overhead already.
 *
 * On Linux systems you can change the maximum allowed value for the receive buffer with e.g.
 * |[
 * sysctl -w net.core.rmem_max=67108864
 * ]|
 *
 * Alternatively this can also be configured in `/etc/sysctl.conf`.
 *
 * Once this is configured kernel-side, you can use `udpsrc buffer-size=NNN` to increase the
 * value to something larger than the default. If the value is too low it's possible that a lot
 * of packets may never get read out by the pipeline after capture because they will be overwritten
 * by new data before they can all be read out.
 *
 * Since: plugins-rs-0.15.0
 */
use atomic_refcell::AtomicRefCell;

use gst::{BufferPool, glib, prelude::*, subclass::prelude::*};

use std::sync::LazyLock;

use gst_video::{VideoColorimetry, VideoFormat, VideoFrame, VideoInfo, video_frame::*};

use crate::basedepay::{PacketToBufferRelation, RtpBaseDepay2Ext};

use crate::raw_video::pixel_group::PixelGroup;
use crate::raw_video::vframe_utils;

use std::str::FromStr;

pub(crate) const VRAW_CHUNK_HDR_LEN: usize = 6;

#[derive(Default)]
pub struct RtpRawVideoDepay {
    state: AtomicRefCell<State>,
}

#[derive(Debug)]
struct OutputFrame {
    vframe: gst_video::VideoFrame<Writable>,
    ext_timestamp: u64,
    seq_start: u64,
    seq_end: u64,
}

#[derive(Default)]
struct State {
    pool: Option<BufferPool>,
    output_frame: Option<OutputFrame>,
    video_info: Option<VideoInfo>,
    pgroup: Option<PixelGroup>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpvrawdepay2",
        gst::DebugColorFlags::empty(),
        Some("RTP Raw Video Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpRawVideoDepay {
    const NAME: &'static str = "GstRtpRawVideoDepay2";
    type Type = super::RtpRawVideoDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpRawVideoDepay {}

impl GstObjectImpl for RtpRawVideoDepay {}

impl ElementImpl for RtpRawVideoDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP Raw Video Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload a Raw Uncompressed Video Stream from RTP packets (RFC 4175)",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    // Todo: more 10-bit / 12-bit / 16-bit formats
                    .structure(
                        gst::Structure::builder("application/x-rtp")
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
                            .field("depth", "8")
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("clock-rate", 90000i32)
                            .field("encoding-name", "RAW")
                            .field("sampling", gst::List::new(["YCbCr-4:2:2"]))
                            .field("depth", "10")
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst_video::VideoCapsBuilder::new()
                    .format_list([
                        VideoFormat::Rgb,
                        VideoFormat::Rgba,
                        VideoFormat::Bgr,
                        VideoFormat::Bgra,
                        VideoFormat::V308,
                        VideoFormat::Uyvy,
                        VideoFormat::I420,
                        VideoFormat::Y41b,
                        VideoFormat::Uyvp,
                    ])
                    .height_range(1..=32767)
                    .width_range(1..=32767)
                    .field("interlace-mode", "progressive")
                    .build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basedepay::RtpBaseDepay2Impl for RtpRawVideoDepay {
    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        gst::info!(CAT, imp = self, "Got caps {caps} ..");

        let mut state = self.state.borrow_mut();

        let sampling = s.get::<&str>("sampling").unwrap();

        let depth = s
            .get::<&str>("depth")
            .ok()
            .and_then(|depth| depth.parse::<i32>().ok())
            .filter(|&v| v > 0)
            .unwrap();

        let Some(width) = s
            .get::<&str>("width")
            .ok()
            .and_then(|width| width.parse::<u32>().ok())
            .filter(|&v| v > 0)
        else {
            gst::error!(CAT, imp = self, "RTP raw video caps without width!");
            return false;
        };

        let Some(height) = s
            .get::<&str>("height")
            .ok()
            .and_then(|height| height.parse::<u32>().ok())
            .filter(|&v| v > 0)
        else {
            gst::error!(CAT, imp = self, "RTP raw video caps without height!");
            return false;
        };

        // https://www.iana.org/assignments/media-types/video/raw
        //
        if s.get::<&str>("interlace").is_ok() {
            // Todo: handle interlaced video as well
            gst::error!(
                CAT,
                imp = self,
                "Interlaced RTP raw video is not supported yet, sorry!"
            );
            return false;
        }

        let colorimetry =
            s.get::<&str>("colorimetry")
                .ok()
                .and_then(|colorimetry| match colorimetry {
                    "BT601-5" => VideoColorimetry::from_str("bt601").ok(),
                    "BT709-2" => VideoColorimetry::from_str("bt709").ok(),
                    "SMPTE240M" => VideoColorimetry::from_str("smpte240m").ok(),
                    colorimetry => {
                        gst::warning!(CAT, imp = self, "Unexpected colorimetry {colorimetry}");
                        VideoColorimetry::from_str(&colorimetry.to_lowercase()).ok()
                    }
                });

        let fmt = match (sampling, depth) {
            // Todo: could also support some of the 5/6-bit depth RGB variations from RFC-4421
            // Todo: could probably support higher-depth RGB variations quite easily
            // (not that our payloader supports those yet)
            ("RGB", 8) => VideoFormat::Rgb,
            ("RGBA", 8) => VideoFormat::Rgba,
            ("BGR", 8) => VideoFormat::Bgr,
            ("BGRA", 8) => VideoFormat::Bgra,
            // Todo: for bonus points we could support different output formats for
            // the various YUV subsamplings (e.g. packed and planar variations)
            ("YCbCr-4:4:4", 8) => VideoFormat::V308,
            ("YCbCr-4:2:2", 8) => VideoFormat::Uyvy,
            ("YCbCr-4:2:0", 8) => VideoFormat::I420,
            ("YCbCr-4:1:1", 8) => VideoFormat::Y41b,
            ("YCbCr-4:2:2", 10) => VideoFormat::Uyvp,
            (sampling, depth) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Unsupported video format / depth combination: {sampling} with {depth} bpp"
                );
                return false;
            }
        };

        let mut video_info = VideoInfo::builder(fmt, width, height);

        if let Some(colorimetry) = colorimetry.as_ref() {
            video_info = video_info.colorimetry(colorimetry);
        }

        let video_info = video_info.build().unwrap();

        let output_caps = video_info.to_caps().unwrap();

        if self
            .negotiate_pool(&mut state, &output_caps, &video_info)
            .is_err()
        {
            return false;
        }

        let pgroup = PixelGroup::from_video_info(&video_info).unwrap();
        gst::info!(CAT, imp = self, "{pgroup:?} for {video_info:?}");
        state.pgroup = Some(pgroup);

        state.video_info = Some(video_info);

        self.obj().set_src_caps(&output_caps);

        true
    }

    // RTP Payload Format for Uncompressed Video:
    // https://www.rfc-editor.org/rfc/rfc4175.html#section-4
    //
    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        gst::trace!(CAT, imp = self, "Got packet {packet:?}");

        // Push out frame if finished
        if let Some(output_frame) = state.output_frame.as_ref()
            && output_frame.ext_timestamp != packet.ext_timestamp()
        {
            self.finish_current_frame(&mut state)?;
        }

        let pgroup = state.pgroup.unwrap();

        let output_frame = if let Some(output_frame) = state.output_frame.as_mut() {
            output_frame
        } else {
            let pool = state.pool.as_ref().unwrap();

            gst::log!(CAT, imp = self, "Acquiring new buffer from pool..");

            let buf = pool.acquire_buffer(None)?;
            let mut vframe =
                VideoFrame::from_buffer_writable(buf, state.video_info.as_ref().unwrap()).map_err(
                    |_| {
                        gst::error!(CAT, imp = self, "Failed to map video buffer for writing");
                        gst::FlowError::Error
                    },
                )?;

            // Clear video frame to avoid data leakage in case of lost packets
            // Todo: this is quite heavy handed, we could probably do something
            // better if we tracked missing fragments or lines.
            vframe_utils::clear_frame(&mut vframe);

            let new_output_frame = OutputFrame {
                vframe,
                ext_timestamp: packet.ext_timestamp(),
                seq_start: packet.ext_seqnum(),
                seq_end: packet.ext_seqnum(),
            };

            gst::debug!(CAT, imp = self, "New output frame: {new_output_frame:?}");

            state.output_frame = Some(new_output_frame);
            state.output_frame.as_mut().unwrap()
        };

        output_frame.seq_end = packet.ext_seqnum();

        let payload = packet.payload();

        if payload.len() < 2 + VRAW_CHUNK_HDR_LEN {
            gst::warning!(
                CAT,
                imp = self,
                "Payload too small: {} bytes, but need at least 8 bytes",
                payload.len()
            );
            return Ok(gst::FlowSuccess::Ok);
        }

        // Skip the extended seqnum bytes

        let payload = &payload[2..];

        // Figure out number of chunks

        let n_chunks = payload
            .chunks_exact(VRAW_CHUNK_HDR_LEN)
            .enumerate()
            .find(|(_, c)| c[4] & 0x80 == 0x00) // Continuation flag
            .map(|(i, _)| i + 1);

        let Some(n_chunks) = n_chunks else {
            gst::warning!(
                CAT,
                imp = self,
                "Payload too small: last chunk header had continuation flag set, but no more bytes",
            );
            return Ok(gst::FlowSuccess::Ok);
        };

        gst::trace!(
            CAT,
            imp = self,
            "{n_chunks} chunks in packet for ext RTP time {}",
            packet.ext_timestamp(),
        );

        // Prepare
        let width = output_frame.vframe.width() as usize;
        let height = output_frame.vframe.height() as usize;

        let format = output_frame.vframe.format();

        let stride = output_frame.vframe.plane_stride()[0] as usize;
        let pstride = output_frame.vframe.comp_pstride(0) as usize;

        let vframe = &mut output_frame.vframe;

        // Iterate over chunks

        let preamble_length = n_chunks * VRAW_CHUNK_HDR_LEN;

        let (preamble, mut payload) = payload.split_at(preamble_length);

        for (i, (length, y, x)) in preamble
            .chunks_exact(6)
            .map(|c| {
                (
                    u16::from_be_bytes([c[0], c[1]]) as usize, // length
                    (u16::from_be_bytes([c[2], c[3]]) & 0x7fff) as usize, // line number
                    (u16::from_be_bytes([c[4], c[5]]) & 0x7fff) as usize, // pixel offset
                )
            })
            .enumerate()
        {
            gst::trace!(CAT, imp = self, "Chunk {i}: {length} bytes @ {x},{y}");

            let n_pixels = (length / pgroup.size()) * pgroup.x_inc();

            // Check lengths and line/pixel offsets

            if payload.len() < length || length % pgroup.size() != 0 {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Bad chunk header: specifies {} bytes. Available: {} bytes",
                    length,
                    payload.len(),
                );
                return Ok(gst::FlowSuccess::Ok);
            }

            if x + n_pixels > width
                || y + pgroup.y_inc() > height
                || x % pgroup.x_inc() != 0
                || y % pgroup.y_inc() != 0
            {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Bad chunk header: {n_pixels} pixels @ {x},{y} \
                    with resolution {width}x{height} \
                    and pgroup size {}, x_inc {}, y_inc {}",
                    pgroup.size(),
                    pgroup.x_inc(),
                    pgroup.y_inc(),
                );
                return Ok(gst::FlowSuccess::Ok);
            }

            let (chunk_data, remainder) = payload.split_at(length);

            match format {
                // Formats where we can just memcpy pixels directly from source to dest
                VideoFormat::Rgb
                | VideoFormat::Rgba
                | VideoFormat::Bgr
                | VideoFormat::Bgra
                | VideoFormat::Uyvy => {
                    let data = vframe.plane_data_mut(0).unwrap();
                    let line = data.chunks_exact_mut(stride).nth(y).unwrap();

                    let byte_offset = x * pstride;
                    let pixels = &mut line[byte_offset..][..length];

                    pixels.copy_from_slice(chunk_data);
                }

                // Uyvp: packed 10-bit 4:2:2 YUV (U0-Y0-V0-Y1 U2-Y2-V2-Y3 U4 ...), 2 pixels in 5 bytes
                VideoFormat::Uyvp => {
                    let data = vframe.plane_data_mut(0).unwrap();
                    let line = data.chunks_exact_mut(stride).nth(y).unwrap();

                    let byte_offset = (x / 2) * 5;
                    let pixels = &mut line[byte_offset..][..length];

                    pixels.copy_from_slice(chunk_data);
                }

                // v308 is straight copy with some component reordering
                VideoFormat::V308 => {
                    let data = vframe.plane_data_mut(0).unwrap();
                    let line = data.chunks_exact_mut(stride).nth(y).unwrap();

                    let byte_offset = x * pstride;
                    let pixels = &mut line[byte_offset..][..length];

                    for (dest, src) in
                        std::iter::zip(pixels.chunks_exact_mut(3), chunk_data.chunks_exact(3))
                    {
                        dest[0] = src[1];
                        dest[1] = src[0];
                        dest[2] = src[2];
                    }
                }

                // Planar YUV 4:2:0
                VideoFormat::I420 => {
                    use itertools::izip;

                    const PGROUP_SIZE_I420: usize = 6;

                    let u_stride = vframe.plane_stride()[1] as usize;
                    let v_stride = vframe.plane_stride()[2] as usize;

                    let [y_data, u_data, v_data, _] = vframe.planes_data_mut();
                    let y_lines = y_data.chunks_exact_mut(2 * stride).nth(y / 2).unwrap();
                    let (y_line1, y_line2) = y_lines.split_at_mut(stride);
                    let y1_pixels = &mut y_line1[x..][..n_pixels];
                    let y2_pixels = &mut y_line2[x..][..n_pixels];

                    let u_line = u_data.chunks_exact_mut(u_stride).nth(y / 2).unwrap();
                    let u_pixels = &mut u_line[x / 2..][..n_pixels / 2];

                    let v_line = v_data.chunks_exact_mut(v_stride).nth(y / 2).unwrap();
                    let v_pixels = &mut v_line[x / 2..][..n_pixels / 2];

                    for (y1, y2, u, v, src) in izip!(
                        y1_pixels.chunks_exact_mut(2),
                        y2_pixels.chunks_exact_mut(2),
                        u_pixels,
                        v_pixels,
                        chunk_data.chunks_exact(PGROUP_SIZE_I420)
                    ) {
                        y1[0] = src[0];
                        y1[1] = src[1];
                        y2[0] = src[2];
                        y2[1] = src[3];
                        *u = src[4];
                        *v = src[5];
                    }
                }

                // Planar YUV 4:1:1
                // Samples are packed in order Cb0-Y0-Y1-Cr0-Y2-Y3
                VideoFormat::Y41b => {
                    use itertools::izip;

                    const PGROUP_SIZE_Y41B: usize = 6;

                    let u_stride = vframe.plane_stride()[1] as usize;
                    let v_stride = vframe.plane_stride()[2] as usize;
                    let [y_data, u_data, v_data, _] = vframe.planes_data_mut();

                    let y_line = y_data.chunks_exact_mut(stride).nth(y).unwrap();
                    let y_pixels = &mut y_line[x..][..n_pixels];

                    let u_line = u_data.chunks_exact_mut(u_stride).nth(y).unwrap();
                    let u_pixels = &mut u_line[x / 4..][..n_pixels / 4];

                    let v_line = v_data.chunks_exact_mut(v_stride).nth(y).unwrap();
                    let v_pixels = &mut v_line[x / 4..][..n_pixels / 4];

                    for (y, u, v, src) in izip!(
                        y_pixels.chunks_exact_mut(4),
                        u_pixels,
                        v_pixels,
                        chunk_data.chunks_exact(PGROUP_SIZE_Y41B)
                    ) {
                        *u = src[0];
                        y[0] = src[1];
                        y[1] = src[2];
                        *v = src[3];
                        y[2] = src[4];
                        y[3] = src[5];
                    }
                }

                fmt => unreachable!("Unexpected video format {fmt}"),
            }

            payload = remainder;
        }

        // Marker = end of frame
        if packet.marker_bit() {
            self.finish_current_frame(&mut state)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn flush(&self) {
        // Keep current frame around, or we'd clear it after every lost packet.
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.borrow_mut();

        if let Some(pool) = state.pool.as_ref() {
            let _ = pool.set_active(false);
        }

        *state = State::default();

        Ok(())
    }
}

impl RtpRawVideoDepay {
    fn finish_current_frame(&self, state: &mut State) -> Result<gst::FlowSuccess, gst::FlowError> {
        let output_frame = state.output_frame.take().unwrap();

        gst::trace!(CAT, imp = self, "Outputting frame {output_frame:?} ..");

        self.obj().queue_buffer(
            PacketToBufferRelation::Seqnums(output_frame.seq_start..=output_frame.seq_end),
            output_frame.vframe.into_buffer(),
        )
    }

    fn negotiate_pool(
        &self,
        state: &mut State,
        caps: &gst::Caps,
        video_info: &VideoInfo,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pool = (|| {
            gst::info!(
                CAT,
                imp = self,
                "Running allocation query with caps {caps} .."
            );

            let mut query = gst::query::Allocation::new(Some(caps), true);

            // Ignore return value, either query has been filled or not
            let _ = self.obj().src_pad().peer_query(&mut query);

            // Buffer pool configuration
            let (pool, size, min, max) =
                query.allocation_pools().next().clone().unwrap_or_else(|| {
                    gst::info!(
                        CAT,
                        imp = self,
                        "No pool provided by downstream, will create our own"
                    );
                    (None, video_info.size() as u32, 0, 0)
                });

            let mut pool = pool.unwrap_or_else(|| gst_video::VideoBufferPool::new().upcast());

            let (allocator, params) = query
                .allocation_params()
                .next()
                .clone()
                .unwrap_or((None, gst::AllocationParams::default()));

            let mut config = pool.config();
            config.set_params(Some(caps), size, min, max);
            config.set_allocator(allocator.as_ref(), Some(&params));

            // Should be fine to add it unconditionally as long as it's with default strides/offsets
            config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);

            gst::info!(
                CAT,
                imp = self,
                "Configuring buffer pool {pool:?} with options {config:?}.."
            );

            if pool.set_config(config).is_ok() {
                return Ok(pool);
            }

            gst::info!(
                CAT,
                imp = self,
                "Failed to configure buffer pool {pool:?}, second try.."
            );

            let mut config = pool.config();
            if config.validate_params(Some(caps), size, min, max).is_err() {
                pool = gst_video::VideoBufferPool::new().upcast();
                config = pool.config();
                config.set_params(Some(caps), size, min, max);
                config.set_allocator(allocator.as_ref(), Some(&params));
            }

            if pool.set_config(config).is_ok() {
                return Ok(pool);
            }

            gst::error!(CAT, imp = self, "Failed to configure buffer pool");
            Err(gst::FlowError::Error)
        })()?;

        if pool.set_active(true).is_err() {
            gst::error!(CAT, imp = self, "Failed to activate buffer pool {pool:?}");
            return Err(gst::FlowError::Error);
        }

        gst::info!(CAT, imp = self, "Activated buffer pool {pool:?}");
        state.pool = Some(pool);

        Ok(gst::FlowSuccess::Ok)
    }
}
