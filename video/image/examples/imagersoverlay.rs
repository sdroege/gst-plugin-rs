// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Inspired by Tim-Philipp Müller's gdkpixbufoverlay test app
// https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.29.1/subprojects/gst-plugins-good/tests/interactive/gdkpixbufoverlay-test.c
// https://gitlab.freedesktop.org/gstreamer/gstreamer-rs/-/blob/0.25.1/examples/src/bin/glib-futures.rs

use std::sync::LazyLock;
use std::sync::atomic::AtomicI32;

use futures::prelude::*;
use gst::prelude::*;

use anyhow::bail;
use image::GenericImageView;

const VIDEO_WIDTH: i32 = 720;
const VIDEO_HEIGHT: i32 = 480;
const VIDEO_FPS: i32 = 50;
const SPEED_SCALE_FACTOR: f64 = (VIDEO_FPS * 4) as f64;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "imagersoverlay-examples",
        gst::DebugColorFlags::empty(),
        Some("imagersoverlay example"),
    )
});
static COUNT: AtomicI32 = AtomicI32::new(0);

fn main() -> anyhow::Result<()> {
    gst::init().unwrap();
    gstimagers::plugin_register_static().unwrap();

    let args: Vec<String> = std::env::args().collect();

    if args.len() != 2 {
        bail!("Usage: {} PATH_TO_IMAGE", args[0]);
    }

    let (logo_w, logo_h) = {
        let image: image::DynamicImage = image::open(&args[1])?;
        image.dimensions()
    };

    let ctx = gst::glib::MainContext::default();

    let pipeline = gst::Pipeline::with_name("pipeline");

    let src = gst::ElementFactory::make("videotestsrc")
        .property_from_str("pattern", "smpte-rp-219")
        .build()
        .unwrap();

    let overlay = gst::ElementFactory::make("imagersoverlay")
        .property("location", &args[1])
        .property_from_str("positioning-mode", "pixels-absolute")
        .build()
        .unwrap();

    {
        let sink_pad = overlay.static_pad("sink").unwrap();
        sink_pad.add_probe(
            gst::PadProbeType::BUFFER,
            move |pad, _info| -> gst::PadProbeReturn {
                let overlay = pad.parent().unwrap().downcast::<gst::Element>().unwrap();

                let (x, y) = {
                    COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let c = COUNT.load(std::sync::atomic::Ordering::SeqCst);

                    /* nicked from videotestsrc's ball pattern renderer */
                    let r_x = f64::from(logo_w) / 2.0;
                    let r_y = f64::from(logo_h) / 2.0;
                    let w = f64::from(VIDEO_WIDTH) + f64::from(logo_w);
                    let h = f64::from(VIDEO_HEIGHT) + f64::from(logo_h);

                    let x = r_x
                        + (0.5
                            + 0.5
                                * f64::sin(
                                    2.0 * std::f64::consts::PI * (c as f64) / SPEED_SCALE_FACTOR,
                                ))
                            * (w - 2.0 * r_x)
                        - f64::from(logo_w);
                    let y = r_y
                        + (0.5
                            + 0.5
                                * f64::sin(
                                    2.0 * std::f64::consts::PI * f64::sqrt(2.0) * (c as f64)
                                        / SPEED_SCALE_FACTOR,
                                ))
                            * (h - 2.0 * r_y)
                        - f64::from(logo_h);

                    (x, y)
                };
                gst::log!(CAT, "{x} {y}");

                overlay.set_property("offset-x", x as i32);
                overlay.set_property("offset-y", y as i32);

                gst::PadProbeReturn::Ok
            },
        );
    }

    let q = gst::ElementFactory::make("queue").build().unwrap();

    let capsfilter = gst::ElementFactory::make("capsfilter").build().unwrap();
    {
        let filter_caps = gst_video::VideoCapsBuilder::new()
            .width(VIDEO_WIDTH)
            .height(VIDEO_HEIGHT)
            .framerate(gst::Fraction::from_integer(VIDEO_FPS))
            .build();
        capsfilter.set_property("caps", filter_caps);
    }

    let sink = gst::ElementFactory::make("autovideosink").build().unwrap();

    pipeline
        .add_many([&src, &q, &overlay, &capsfilter, &sink])
        .unwrap();

    gst::Element::link_many([src, q, overlay, capsfilter, sink]).unwrap();

    let bus = pipeline.bus().unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    ctx.block_on(async {
        let mut messages = bus.stream();

        while let Some(msg) = messages.next().await {
            use gst::MessageView;

            // Determine whether we want to quit: on EOS or error message
            // we quit, otherwise simply continue.
            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    msg.src()
                        .unwrap()
                        .default_error(&err.error(), err.debug().as_deref());
                    break;
                }
                _ => (),
            }
        }
    });

    pipeline.set_state(gst::State::Null).unwrap();

    Ok(())
}
