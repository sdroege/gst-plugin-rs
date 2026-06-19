// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on Mathieu Duponchelle's WebP plugin -- see video/webp/src/dec/imp.rs

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::VideoColorimetry;

use image::{ImageReader, metadata::LoopCount};

use std::io::Cursor;
use std::sync::{LazyLock, Mutex};

use crate::anim::LoopStrategy;
use crate::{buffer::*, cicp::ImageCicp, format::Format};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "animatedimagersdec",
        gst::DebugColorFlags::empty(),
        Some("image-rs decoder for animated formats"),
    )
});

#[derive(Default)]
struct Settings {
    max_size: u64,
    max_alloc: u64,
    do_loop: LoopStrategy,
}

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    total_size: usize,
    format_from_caps: Option<Format>,
    in_par: Option<gst::Fraction>,
}

pub struct Decoder {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Decoder {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {buffer:?}");

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        if settings.max_size == 0
            || state.total_size as u64 + buffer.size() as u64 <= settings.max_size
        {
            gst::log!(CAT, imp = self, "Writing buffer size {}", buffer.size());
            state.total_size += buffer.size();
            state.buffers.push(buffer);

            Ok(gst::FlowSuccess::Ok)
        } else {
            gst::error!(
                CAT,
                obj = pad,
                "Exhausted memory limit of {} bytes",
                settings.max_size
            );
            Err(gst::FlowError::Error)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_pool(
        &self,
        caps: &gst::Caps,
        pool: &gst::BufferPool,
        size: u32,
        min: u32,
        max: u32,
        allocator: Option<&gst::Allocator>,
        params: &gst::AllocationParams,
        video_meta_supported: bool,
    ) -> bool {
        gst::debug!(
            CAT,
            imp = self,
            "Trying pool {} and allocator {}",
            pool.name(),
            allocator
                .as_ref()
                .map(|a| a.name())
                .as_deref()
                .unwrap_or("None"),
        );

        let mut config = pool.config();

        let video_meta_supported =
            video_meta_supported && pool.has_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
        if video_meta_supported {
            config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
        }

        config.set_allocator(allocator, Some(params));
        config.set_params(Some(caps), size, min, max);

        if pool.set_config(config).is_err() {
            let config = pool.config();
            if config.validate_params(Some(caps), size, min, max).is_ok()
                && pool.set_config(config).is_err()
            {
                return false;
            }
        }

        true
    }

    fn render_many_frames(
        &self,
        buf: Vec<u8>,
        format: Option<crate::format::Format>,
        wh: (u32, u32),
        pixel_aspect_ratio: Option<gst::Fraction>,
        loop_info: (LoopStrategy, u32),
    ) -> Result<(), gst::ErrorMessage> {
        let mut prev_timestamp = None;
        let mut prev_caps = gst::Caps::new_empty();
        let (do_loop, mut iterations) = loop_info;
        let mut video_meta_supported = false;
        let mut output_pool: Option<gst::BufferPool> = None;

        loop {
            if iterations > 0 && do_loop != LoopStrategy::Yes {
                iterations -= 1;
                gst::debug!(CAT, "repeat count: {iterations}");
            }
            let (_, _, frames) = self.create_decoder(&buf, format)?;
            for frame in frames {
                let frame = frame.map_err(|v| {
                    gst::error_msg!(gst::StreamError::Decode, ["Failed to get next frame: {v}"])
                })?;
                let color_info: Option<VideoColorimetry> = ImageCicp(frame.buffer().color_space())
                    .try_into()
                    .inspect_err(|e| {
                        gst::info!(
                            CAT,
                            imp = self,
                            "Failed converting to VideoColorimetry: {e}"
                        );
                    })
                    .ok();

                let video_info =
                    gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, wh.0, wh.1)
                        .par_if_some(pixel_aspect_ratio)
                        .colorimetry_if_some(color_info.as_ref())
                        .build()
                        .map_err(|e| gst::error_msg!(gst::StreamError::Format, ["{e}"]))?;

                if self.srcpad.check_reconfigure() {
                    let caps = video_info
                        .to_caps()
                        .map_err(|e| gst::error_msg!(gst::StreamError::Format, ["{e}"]))?;

                    if caps != prev_caps {
                        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));

                        gst::debug!(CAT, imp = self, "New caps, set up new buffer pool");

                        let mut query = gst::query::Allocation::new(Some(&caps), true);

                        let pool = if self.srcpad.peer_query(&mut query) {
                            gst::debug!(
                                CAT,
                                imp = self,
                                "Updated caps, querying zerocopy support: {query:?}"
                            );

                            // Downstream supports videometa so let's remember
                            // that in any case for the output side
                            video_meta_supported = query
                                .find_allocation_meta::<gst_video::VideoMeta>()
                                .is_some();

                            let (allocator, params) =
                                query.allocation_params().next().unwrap_or_default();

                            // We select the first pool below that allows for
                            // at least 1 buffer.
                            //
                            // If it supports video meta then it's used for a
                            // 1:1 copy, otherwise we must pad as specified by
                            // the format.
                            let mut selected_pool = None;
                            for (pool, size, min, max) in query.allocation_pools() {
                                let Some(pool) = pool else {
                                    continue;
                                };

                                if self.validate_pool(
                                    &caps,
                                    &pool,
                                    std::cmp::max(size as usize, video_info.size()) as u32,
                                    min,
                                    max,
                                    allocator.as_ref(),
                                    &params,
                                    video_meta_supported,
                                ) {
                                    selected_pool = Some(pool);
                                    gst::debug!(CAT, imp = self, "Using negotiated buffer pool");
                                    break;
                                }
                            }

                            selected_pool
                        } else {
                            gst::debug!(CAT, imp = self, "Could not query for buffer pool");
                            None
                        };

                        output_pool = pool;
                        prev_caps = caps;
                    }
                }

                if output_pool.is_none() {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "No compatible buffer pools found, creating fallback pool"
                    );

                    let pool = gst_video::VideoBufferPool::new();

                    // Set up pool configuration
                    let mut config = pool.config();

                    if video_meta_supported {
                        config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
                    }

                    config.set_params(Some(&prev_caps), video_info.size() as u32, 2, 0);

                    pool.set_config(config).unwrap();

                    output_pool = Some(pool.upcast())
                }

                let pool = output_pool.as_ref().unwrap();

                if !pool.is_active() {
                    pool.set_active(true).unwrap();
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Activated pool {} ({:?})",
                        pool.name(),
                        pool.config()
                    );
                }

                if prev_timestamp.is_none() {
                    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                    let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));
                    prev_timestamp = Some(gst::ClockTime::ZERO)
                }

                let delay: gst::ClockTime = std::time::Duration::from(frame.delay())
                    .try_into()
                    .map_err(|v| {
                        gst::error_msg!(gst::StreamError::Decode, ["Invalid frame duration: {v}"])
                    })?;

                // We can consume the frame here because AnimatedDecoder
                // supports only RGBA output, and image-rs's ImageBuffer
                // class only accepts tightly packed buffers.
                let mut out_buf = pool.acquire_buffer(None).map_err(|v| {
                    gst::error_msg!(gst::StreamError::Failed, ["Failed to acquire buffer: {v}"])
                })?;

                if video_meta_supported {
                    out_buf
                        .get_mut()
                        .unwrap()
                        .copy_from_slice(0, frame.buffer())
                        .map_err(|v| {
                            gst::error_msg!(
                                gst::StreamError::Decode,
                                [
                                    "Incomplete copy: {v} bytes copied, {} expected, buffer size {}",
                                    frame.buffer().len(),
                                    out_buf.size()
                                ]
                            )
                        })?;
                    gst_video::VideoMeta::add_full(
                        out_buf.get_mut().unwrap(),
                        gst_video::VideoFrameFlags::empty(),
                        gst_video::VideoFormat::Rgba,
                        wh.0,
                        wh.1,
                        &[0],
                        &[frame.buffer().stride_in_bytes()],
                    )
                    .map_err(|v| gst::error_msg!(gst::StreamError::Format, ["{v}"]))?;
                } else {
                    let mut out_map = out_buf.get_mut().unwrap().map_writable().unwrap();
                    copy_from_image(
                        frame.buffer(),
                        &mut out_map,
                        video_info.stride()[0].try_into().unwrap(),
                    );
                }

                {
                    let out_buf_mut = out_buf.get_mut().unwrap();
                    out_buf_mut.set_pts(prev_timestamp);
                    out_buf_mut.set_duration(delay);
                }

                *prev_timestamp.as_mut().unwrap() += delay;

                match self.srcpad.push(out_buf) {
                    Ok(_) => (),
                    Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => return Ok(()),
                    Err(error) => {
                        return Err(gst::error_msg!(
                            gst::StreamError::Failed,
                            ["Failed to push buffers: {error:?}"]
                        ));
                    }
                }
            }
            if iterations == 0 && do_loop == LoopStrategy::No {
                break;
            }
        }

        Ok(())
    }

    fn set_format_from_caps(&self, event_caps: &gst::event::Caps) -> Result<(), gst::ErrorMessage> {
        let s = event_caps.caps().structure(0).unwrap();
        let mut state = self.state.lock().unwrap();
        state.format_from_caps = Some(s.try_into()?);
        state.in_par = s.get::<gst::Fraction>("pixel-aspect-ratio").ok();

        Ok(())
    }

    fn create_decoder<'a>(
        &'a self,
        buf: &'a Vec<u8>,
        format: Option<crate::format::Format>,
    ) -> Result<((u32, u32), LoopCount, image::Frames<'a>), gst::ErrorMessage> {
        let cursor = Cursor::new(buf);

        let mut reader = ImageReader::new(cursor);

        if let Some(v) = format {
            reader.set_format(v.try_into().unwrap());
        } else {
            reader = reader.with_guessed_format().map_err(|v| {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed detecting container: {v}"]
                )
            })?;
        }

        match reader.format() {
            #[cfg(feature = "gif")]
            Some(image::ImageFormat::Gif) => {
                use image::{AnimationDecoder, ImageDecoder, Limits};

                let mut decoder = image::codecs::gif::GifDecoder::new(reader.into_inner())
                    .map_err(|v| {
                        gst::error_msg!(
                            gst::StreamError::Decode,
                            ["Failed decoding GIF container: {v}"]
                        )
                    })?;

                {
                    let settings = self.settings.lock().unwrap();
                    if settings.max_alloc != 0 {
                        let mut limits = Limits::default();
                        limits.max_alloc = Some(settings.max_alloc);
                        decoder.set_limits(limits).map_err(|v| {
                            gst::error_msg!(
                                gst::StreamError::Decode,
                                ["Failed setting limits: {v}"]
                            )
                        })?;
                    }
                }

                Ok((
                    decoder.dimensions(),
                    decoder.loop_count(),
                    decoder.into_frames(),
                ))
            }
            #[cfg(feature = "webp")]
            Some(image::ImageFormat::WebP) => {
                use image::{AnimationDecoder, ImageDecoder, Limits};

                let mut decoder = image::codecs::webp::WebPDecoder::new(reader.into_inner())
                    .map_err(|v| {
                        gst::error_msg!(
                            gst::StreamError::Decode,
                            ["Failed decoding WebP container: {v}"]
                        )
                    })?;

                {
                    let settings = self.settings.lock().unwrap();
                    if settings.max_alloc != 0 {
                        let mut limits = Limits::default();
                        limits.max_alloc = Some(settings.max_alloc);
                        decoder.set_limits(limits).map_err(|v| {
                            gst::error_msg!(
                                gst::StreamError::Decode,
                                ["Failed setting limits: {v}"]
                            )
                        })?;
                    }
                }

                Ok((
                    decoder.dimensions(),
                    decoder.loop_count(),
                    decoder.into_frames(),
                ))
            }
            #[cfg(any(feature = "png", feature = "ico"))]
            Some(image::ImageFormat::Png) => {
                use image::{AnimationDecoder, ImageDecoder, Limits};

                let limits = {
                    let settings = self.settings.lock().unwrap();
                    if settings.max_alloc != 0 {
                        let mut limits = Limits::default();
                        limits.max_alloc = Some(settings.max_alloc);
                        Some(limits)
                    } else {
                        None
                    }
                };

                let decoder = match limits {
                    Some(v) => image::codecs::png::PngDecoder::with_limits(reader.into_inner(), v)
                        .map_err(|v| {
                            gst::error_msg!(
                                gst::StreamError::Decode,
                                ["Failed decoding PNG container: {v}"]
                            )
                        })?,
                    None => {
                        image::codecs::png::PngDecoder::new(reader.into_inner()).map_err(|v| {
                            gst::error_msg!(
                                gst::StreamError::Decode,
                                ["Failed decoding PNG container: {v}"]
                            )
                        })?
                    }
                };

                let wh = decoder.dimensions();

                let apng_decoder = decoder.apng().map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding animated PNG container: {v}"]
                    )
                })?;

                Ok((wh, apng_decoder.loop_count(), apng_decoder.into_frames()))
            }
            v => Err(gst::error_msg!(
                gst::StreamError::CodecNotFound,
                ["Unhandled format {v:?}"]
            )),
        }
    }

    fn decode(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        let format = state.format_from_caps;

        if state.buffers.is_empty() {
            return Err(gst::error_msg!(
                gst::StreamError::Decode,
                ["No valid frames decoded before end of stream"]
            ));
        }

        let mut buf = Vec::with_capacity(state.total_size);

        for buffer in state.buffers.drain(..) {
            buf.extend_from_slice(&buffer.map_readable().expect("Failed to map buffer"));
        }

        state.total_size = 0;

        let par = state.in_par;

        drop(state);

        let mut do_loop = self.settings.lock().unwrap().do_loop;

        let (wh, iterations) = {
            let (wh, loop_count, _) = self.create_decoder(&buf, format)?;

            let repeat = if do_loop == LoopStrategy::No {
                0
            } else {
                match loop_count {
                    LoopCount::Finite(n) => {
                        gst::info!(CAT, imp = self, "repeat {n}");
                        do_loop = LoopStrategy::No;
                        n.get()
                    }
                    LoopCount::Infinite => {
                        gst::info!(CAT, imp = self, "repeat infinite");
                        do_loop = LoopStrategy::Yes;
                        0
                    }
                }
            };

            (wh, repeat)
        };

        self.render_many_frames(buf, format, wh, par, (do_loop, iterations))?;

        Ok(())
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {event:?}");
        match event.view() {
            EventView::Caps(v) => {
                if let Err(err) = self.set_format_from_caps(v) {
                    self.post_error_message(err);
                    return false;
                }
                true
            }
            EventView::FlushStop(..) => {
                *self.state.lock().unwrap() = State::default();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Eos(..) | EventView::SegmentDone(..) => {
                if let Err(err) = self.decode() {
                    self.post_error_message(err);
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Segment(..) => true,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {event:?}");
        match event.view() {
            EventView::Seek(..) => false,
            EventView::FlushStop(..) => {
                *self.state.lock().unwrap() = State::default();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Decoder {
    const NAME: &'static str = "GstImageRsAnimDecoder";
    type Type = super::Decoder;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Decoder::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |dec| dec.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Decoder::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec| dec.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                Decoder::catch_panic_pad_function(parent, || false, |dec| dec.src_event(pad, event))
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for Decoder {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("max-size-bytes")
                    .nick("Max. size")
                    .blurb("Max. amount of data to buffer (bytes, 0=disable)")
                    .default_value(10 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("max-alloc-bytes")
                    .nick("Memory allocation limits")
                    .blurb("Max. amount of data to allocate for decoding (bytes, 0=disable)")
                    .default_value(128 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<LoopStrategy>("loop")
                    .nick("Loop")
                    .blurb("Set the repeat behaviour of the image")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "max-alloc-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_alloc = value.get::<u64>().expect("type checked upstream");
            }
            "max-size-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size = value.get::<u64>().expect("type checked upstream");
            }
            "loop" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_loop = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "max-alloc-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.max_alloc.to_value()
            }
            "max-size-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.max_size.to_value()
            }

            "loop" => {
                let settings = self.settings.lock().unwrap();
                settings.do_loop.to_value()
            }
            name => panic!("No getter for {name}"),
        }
    }
}

impl GstObjectImpl for Decoder {}

impl ElementImpl for Decoder {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs decoder (animated formats)",
                "Codec/Decoder/Video",
                "Decodes animated image formats",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = Format::all_animated_formats()
                .into_iter()
                .collect::<gst::Caps>();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {transition:?}");

        if transition == gst::StateChange::PausedToReady {
            *self.state.lock().unwrap() = State::default();
        }

        self.parent_change_state(transition)
    }
}
