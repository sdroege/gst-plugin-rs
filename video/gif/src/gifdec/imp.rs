// Copyright (C) 2025  Taruntej Kanakamalla <tarun@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// The design of this element is same as WebPDec element.
// Most parts of code in this file are taken from video/webp/src/dec/imp.rs

use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("gifdec", gst::DebugColorFlags::empty(), Some("GIF decoder"))
});

#[derive(Default)]
struct Settings {
    do_loop: bool,
}

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    total_size: usize,
}

pub struct GifDec {
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl GifDec {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();

        state.total_size += buffer.size();
        state.buffers.push(buffer);

        Ok(gst::FlowSuccess::Ok)
    }

    fn decode(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if state.buffers.is_empty() {
            return Err(gst::error_msg!(
                gst::StreamError::Decode,
                ["No buffers received before end of stream"]
            ));
        }

        let mut buf = Vec::with_capacity(state.total_size);

        for buffer in state.buffers.drain(..) {
            buf.extend_from_slice(&buffer.map_readable().expect("Failed to map buffer"));
        }

        drop(state);

        let mut dec = gif::DecodeOptions::new();
        dec.set_color_output(gif::ColorOutput::RGBA);
        let mut d = match dec.clone().read_info(buf.as_slice()) {
            Ok(d) => d,
            Err(e) => {
                return Err(gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed to read input buffers {e}"]
                ));
            }
        };

        let mut do_loop = self.settings.lock().unwrap().do_loop;

        let mut repeat = match d.repeat() {
            gif::Repeat::Finite(n) => {
                gst::info!(CAT, "repeat {n}");
                n
            }
            gif::Repeat::Infinite => {
                gst::info!(CAT, "repeat infinite");
                do_loop = true;
                0
            }
        };

        let width = d.width();
        let height = d.height();
        let size = (width as usize * height as usize) * 4;
        let bg_color = if let Some(palette) = d.global_palette() {
            if let Some(c) = d.bg_color() {
                // opaque if there is a bg color
                [palette[c], palette[c + 1], palette[c + 2], 255]
            } else {
                [0_u8; 4]
            }
        } else {
            [0_u8; 4]
        };

        gst::debug!(
            CAT,
            imp = self,
            "Image details: size {size} width {width} height {height} buffer size {}",
            d.buffer_size()
        );

        let video_info = gst_video::VideoInfo::builder(
            gst_video::VideoFormat::Rgba,
            width.into(),
            height.into(),
        )
        .fps((0, 1))
        .build()
        .unwrap();

        let caps = video_info.to_caps().unwrap();

        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        let mut prev_frame_buffer = vec![0_u8; size];
        let mut next_timestamp = gst::ClockTime::ZERO;

        while do_loop || repeat != 0 {
            let next_frame = match d.read_next_frame() {
                Ok(f) => f,
                Err(e) => {
                    return Err(gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed to read frame : {e}",]
                    ));
                }
            };

            let Some(frame) = next_frame else {
                gst::debug!(CAT, "end of frames, replaying..");

                d = dec.clone().read_info(buf.as_slice()).unwrap();
                if repeat > 0 && !do_loop {
                    repeat -= 1;
                    gst::debug!(CAT, "repeat count: {repeat} ");
                }
                continue;
            };

            let timestamp = next_timestamp;
            // Frame delay in units of 10 ms.
            let duration = (frame.delay as u64 * 10).mseconds();

            gst::trace!(
                CAT,
                imp = self,
                "frame details: width {} x height {}, top {}, left {}, dispose {:?}, transparent {:?} ",
                frame.width,
                frame.height,
                frame.top,
                frame.left,
                frame.dispose,
                frame.transparent,
            );

            let mut curr_frame_buffer = if (frame.top, frame.left) != (0, 0)
                || (frame.width, frame.height) != (width, height)
            {
                // fill the current frame buffer with previous frame
                prev_frame_buffer.clone()
            } else {
                vec![0_u8; size]
            };

            // since we use `read_next_frame`, we get the raw bytes from the `frame.buffer` and not the palette indices
            for ((prev_frame_row, curr_frame_row), this_frame_row) in Iterator::zip(
                Iterator::zip(
                    prev_frame_buffer.chunks_exact_mut(width as usize * 4),
                    curr_frame_buffer.chunks_exact_mut(width as usize * 4),
                )
                .skip(frame.top as usize),
                frame.buffer.chunks_exact(frame.width as usize * 4),
            ) {
                for ((prev_frame_pixel, curr_frame_pixel), this_frame_pixel) in Iterator::zip(
                    Iterator::zip(
                        prev_frame_row.chunks_exact_mut(4),
                        curr_frame_row.chunks_exact_mut(4),
                    )
                    .skip(frame.left as usize),
                    this_frame_row.chunks_exact(4),
                ) {
                    // the fill_buffer call internally is doing the job to check if this pixel is
                    // supposed to be transparent and indicates that by setting alpha channel value to
                    // 0, so copy the same pixel data from the previous frame
                    if this_frame_pixel[3] == 0 {
                        curr_frame_pixel.copy_from_slice(prev_frame_pixel);
                    } else {
                        curr_frame_pixel.copy_from_slice(this_frame_pixel);
                    }

                    match frame.dispose {
                        gif::DisposalMethod::Background => {
                            prev_frame_pixel.copy_from_slice(&bg_color);
                        }
                        gif::DisposalMethod::Any | gif::DisposalMethod::Keep => {
                            prev_frame_pixel.copy_from_slice(curr_frame_pixel);
                        }
                        gif::DisposalMethod::Previous => {}
                    }
                }
            }

            let mut out_buf = gst::Buffer::from_mut_slice(curr_frame_buffer);
            {
                let out_buf_mut = out_buf.get_mut().unwrap();
                out_buf_mut.set_pts(timestamp);
                out_buf_mut.set_duration(duration);
            }

            match self.srcpad.push(out_buf) {
                Ok(_) => (),
                Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => break,
                Err(flow) => {
                    return Err(gst::error_msg!(
                        gst::StreamError::Failed,
                        ["Failed to push buffers: {:?}", flow]
                    ));
                }
            }

            next_timestamp = timestamp + duration;
        }

        Ok(())
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::info!(CAT, obj = pad, "Handling event {:?}", event);
        match event.view() {
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Eos(..) => {
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

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(..) => false,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for GifDec {
    const NAME: &'static str = "GstGifDec";
    type Type = super::GifDec;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                GifDec::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |dec| dec.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                GifDec::catch_panic_pad_function(parent, || false, |dec| dec.sink_event(pad, event))
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                GifDec::catch_panic_pad_function(parent, || false, |dec| dec.src_event(pad, event))
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for GifDec {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![glib::ParamSpecBoolean::builder("loop")
                .nick("Loop")
                .blurb("Respects the internal 'repeat' setting by default and overrides it to run inifinitely if true")
                .default_value(false)
                .mutable_ready()
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "loop" => {
                let settings = self.settings.lock().unwrap();
                settings.do_loop.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "loop" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_loop = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for GifDec {}

impl ElementImpl for GifDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Gif decoder",
                "Codec/Decoder/Image",
                "Decodes GIF images",
                "Taruntej Kanakamalla <tarun@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("image/gif").build();

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
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        let res = self.parent_change_state(transition);

        if transition == gst::StateChange::PausedToReady {
            *self.state.lock().unwrap() = State::default();
        }

        res
    }
}
